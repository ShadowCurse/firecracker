#![allow(missing_docs)]
/// ioctls
pub mod ioctls;

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::marker::PhantomData;
use std::ops::DerefMut;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::{Arc, Barrier, Mutex};

use arrayvec::ArrayVec;
use kvm_bindings::{
    KVM_DEV_VFIO_FILE, KVM_DEV_VFIO_FILE_ADD, KVM_DEV_VFIO_FILE_DEL, kvm_create_device,
    kvm_device_attr, kvm_device_type_KVM_DEV_TYPE_VFIO, kvm_userspace_memory_region,
};
use kvm_ioctls::DeviceFd;
use vfio_bindings::bindings::vfio::*;
use vm_allocator::AllocPolicy;
use vm_memory::{GuestMemory, GuestMemoryRegion};
use zerocopy::IntoBytes;

use crate::Vm;
use crate::arch::host_page_size;
use crate::logger::{debug, error, warn};
use crate::pci::configuration::{BAR0_REG_IDX, Bars, NUM_BAR_REGS};
use crate::pci::msix::{MsixCap, MsixConfig};
use crate::pci::{PciCapabilityId, PciDevice, PciExpressCapabilityId, PciSBDF};
use crate::utils::{
    align_down_host_page, align_up, align_up_host_page, offset_from_lower_host_page, u64_to_usize,
    usize_to_u64,
};
use crate::vfio::ioctls::VfioIoctlError;
use crate::vmm_config::vfio::VfioConfig;
use crate::vstate::bus::BusDevice;
use crate::vstate::interrupts::InterruptError;
use crate::vstate::memory::{GuestMemoryMmap, GuestRegionType};
use crate::vstate::resources::ResourceAllocator;

// First BAR offset in the PCI config space.
pub const PCI_CONFIG_BAR_OFFSET: u32 = 0x10;
// Capability register offset in the PCI config space.
pub const PCI_CONFIG_CAPABILITY_OFFSET: u32 = 0x34;
// Extended capabilities register offset in the PCI config space.
pub const PCI_CONFIG_EXTENDED_CAPABILITY_OFFSET: u32 = 0x100;
// IO BAR when first BAR bit is 1.
pub const PCI_CONFIG_IO_BAR: u32 = 1 << 0; // 0x1;
// 64-bit memory bar flag.
pub const PCI_CONFIG_MEMORY_BAR_64BIT: u32 = 1 << 2; // 0x4;
// Prefetchable BAR bit
pub const PCI_CONFIG_BAR_PREFETCHABLE: u32 = 1 << 3; // 0x8;
// Number of BARs for a PCI device
pub const BAR_NUMS: u8 = 6;

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VfioError {
    /// Cannot open /dev/vfio/vfio: {0}
    Open(std::io::Error),
    /// Invalid vfio api version. Expected 0 got {0}
    CheckApiVersion(i32),
    /// VFIO does not support VFIO_TYPE1v2_IOMMU
    CheckExtension,
    /// Failed to read iommu_group symlink: {0}
    ReadIommuGroup(std::io::Error),
    /// Invalid iommu_group path
    InvalidGroupPath,
    /// Failed to parse group id: {0}
    ParseGroupId(std::num::ParseIntError),
    /// Cannot open /dev/vfio/{0}: {1}
    OpenGroup(u32, std::io::Error),
    /// Group {0} is not viable.
    GroupNotViable(u32),
    /// Invalid IOMMU type: {0}
    InvalidIommuType(u32),
    /// Invalid device path
    InvalidDevicePath,
    /// Failed to read region at index {0} offset {1:#x}: {2}
    RegionRead(u32, u64, std::io::Error),
    /// Failed to write region at index {0} offset {1:#x}: {2}
    RegionWrite(u32, u64, std::io::Error),
    /// Failed to allocate guest address for BAR
    BarAllocation,
    /// mmap failed
    Mmap,
    /// Failed to allocate KVM slot
    KvmSlot,
    /// Failed to set KVM user memory region: {0}
    SetUserMemoryRegion(String),
    /// Failed to copy ROM into guest memory: {0}
    CopyRom(String),
    /// Vfio ioctl failure: {0}
    Ioctl(#[from] VfioIoctlError),
    /// Cannot create Msix vector group: {0}
    MsixConfig(#[from] InterruptError),
    /// KVM failed to create KVM_DEV_TYPE_VFIO device: {0}
    KVMCreateVfioDevice(kvm_ioctls::Error),
    /// KVM failed to add vfio file: {0}
    KVMVfioDeviceFileAdd(kvm_ioctls::Error),
    /// Partial DMA unmap: requested {0:#x}, got {1:#x}
    PartialDmaUnmap(u64, u64),
    /// Device is not a PCI device
    NotPciDevice,
}

fn allocate_8byte_aligned_byte_array(n: u64) -> Vec<u8> {
    // Need 8 byte alignment, but Rust is making it hard
    // There can be some left overs after rounding up, but
    // this is not an issue.
    let total_u64_bytes = align_up(n, 8);
    let total_u64s = total_u64_bytes / 8;
    let u64s = vec![0_u64; u64_to_usize(total_u64s)];
    let ptr = u64s.as_ptr();
    let len = u64s.len() * 8;
    let cap = u64s.capacity() * 8;
    std::mem::forget(u64s);
    unsafe { Vec::from_raw_parts(ptr as *mut u8, len, cap) }
}

struct VfioRegionInfoWithCap {
    pub bytes: Vec<u8>,
}
impl VfioRegionInfoWithCap {
    pub fn new_with_argsz(n: u32) -> Self {
        assert!(std::mem::size_of::<vfio_region_info>() <= n as usize);
        let bytes = allocate_8byte_aligned_byte_array(u64::from(n));
        Self { bytes }
    }
    pub fn vfio_region_info_mut(&mut self) -> &mut vfio_region_info {
        unsafe { &mut *(self.bytes.as_mut_ptr() as *mut vfio_region_info) }
    }
    pub fn vfio_info_cap_header_at_offset(&mut self, offset: u32) -> Option<&vfio_info_cap_header> {
        let vfio_region_info_bytes = std::mem::size_of::<vfio_region_info>();
        if offset < vfio_region_info_bytes as u32 {
            None
        } else {
            let next_cap_offset = offset as usize;
            let next_cap_header_end = next_cap_offset + std::mem::size_of::<vfio_info_cap_header>();
            if self.bytes.len() < next_cap_offset || self.bytes.len() < next_cap_header_end {
                // This data comes from the kernel, so it should be valid and this path should
                // never be taken, but just in case do these checks.
                None
            } else {
                let cap_header = unsafe {
                    &*(self.bytes.as_ptr().add(next_cap_offset) as *const vfio_info_cap_header)
                };
                Some(cap_header)
            }
        }
    }
}

struct VfioIrqSet<T: Sized> {
    pub bytes: Vec<u8>,
    pub entries: u64,
    _pd: PhantomData<T>,
}

impl<T: Sized> VfioIrqSet<T> {
    pub fn new_with_entries(entries: u64) -> Self {
        let vfio_irq_set_bytes = std::mem::size_of::<vfio_irq_set>();
        let entries_bytes = std::mem::size_of::<T>() * u64_to_usize(entries);
        let total_bytes = vfio_irq_set_bytes + entries_bytes;
        let bytes = allocate_8byte_aligned_byte_array(usize_to_u64(total_bytes));
        Self {
            bytes,
            entries,
            _pd: PhantomData,
        }
    }
    pub fn irq_set_mut(&mut self) -> &mut vfio_irq_set {
        unsafe { &mut *(self.bytes.as_mut_ptr() as *mut vfio_irq_set) }
    }
    pub fn entries_mut(&mut self) -> &mut [T] {
        let vfio_irq_set_bytes = std::mem::size_of::<vfio_irq_set>();
        let entries_start = unsafe { self.bytes.as_mut_ptr().add(vfio_irq_set_bytes) };
        unsafe {
            std::slice::from_raw_parts_mut(entries_start as *mut T, u64_to_usize(self.entries))
        }
    }
}

/// Represent one area of the sparse mmap
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct VfioRegionSparseMmapArea {
    /// Offset of mmap'able area within region
    pub offset: u64,
    /// Size of mmap'able area
    pub size: u64,
}

/// List of sparse mmap areas
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VfioRegionCapSparseMmap {
    /// List of areas
    pub areas: Vec<VfioRegionSparseMmapArea>,
}

/// Represent a specific device by providing type and subtype
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct VfioRegionCapType {
    /// Device type
    pub type_: u32,
    /// Device subtype
    pub subtype: u32,
}

/// Carry NVLink SSA TGT information
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct VfioRegionCapNvlink2Ssatgt {
    /// TGT value
    pub tgt: u64,
}

/// Carry NVLink link speed information
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct VfioRegionCapNvlink2Lnkspd {
    /// Link speed value
    pub link_speed: u32,
}

/// List of capabilities that can be related to a region.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VfioRegionCap {
    /// Sparse memory mapping type
    SparseMmap(VfioRegionCapSparseMmap),
    /// Capability holding type and subtype
    Type(VfioRegionCapType),
    /// Indicate if the region is mmap'able with the presence of MSI-X region
    MsixMappable,
    /// NVLink SSA TGT
    Nvlink2Ssatgt(VfioRegionCapNvlink2Ssatgt),
    /// NVLink Link Speed
    Nvlink2Lnkspd(VfioRegionCapNvlink2Lnkspd),
}

#[derive(Debug)]
pub struct VfioRegionInfo {
    pub flags: u32,
    pub size: u64,
    pub offset: u64,
    pub caps: Vec<VfioRegionCap>,
}

#[derive(Debug)]
pub struct RegisterMask {
    register: u16,
    // applied as (R & mask) | value
    mask: u32,
    value: u32,
}

#[derive(Debug)]
pub struct BarInfo {
    pub index: u8,
    pub gpa: u64,
    pub size: u64,
    pub is_64_bits: bool,
    pub is_prefetchable: bool,

    // just for testing
    pub about_to_read_size: bool,
}

#[derive(Debug, Copy, Clone)]
pub enum BarHoleInfoUsage {
    Table,
    Pba,
}

#[derive(Debug, Copy, Clone)]
pub struct BarHoleInfo {
    pub gpa: u64,
    pub size: u64,
    pub usage: BarHoleInfoUsage,
}

#[derive(Debug, Copy, Clone)]
pub struct BarMapping {
    pub slot: u32,
    pub iova: u64,
    pub size: u64,
    pub host_addr: u64,
}

#[derive(Debug)]
pub struct VfioDevice {
    pub file: File,
    pub info: vfio_device_info,
    pub region_infos: Vec<VfioRegionInfo>,
    pub irq_infos: Vec<vfio_irq_info>,
}

#[derive(Debug)]
pub struct MsixState {
    pub register: u8,
    pub cap: MsixCap,
    pub bar_hole_infos: ArrayVec<BarHoleInfo, 2>,
    pub config: MsixConfig,
}

/// The VFIO device bundle
#[derive(Debug)]
pub struct VfioDeviceBundle {
    pub config: VfioConfig,
    pub sbdf: PciSBDF,
    pub group_id: u32,
    pub group: File,
    pub device: VfioDevice,
    pub bars: Bars,
    // There are 6 bars, but one of them can be split in 3 by MSI-X table/pba
    pub bar_mappings: ArrayVec<BarMapping, 8>,
    pub msix_state: Option<MsixState>,
    pub masks: Vec<RegisterMask>,
    pub vm: Arc<Vm>,
}

#[derive(Debug)]
pub struct VfioKvmAndContainer {
    pub container: File,
    pub kvm_device: DeviceFd,
}

macro_rules! handle_bar_access {
    ($state:expr, $device:expr, $base:expr, $offset:expr, $data:expr,
     $table_fn:ident, $pba_fn:ident, $region_fn:ident) => {{
        let mut name = "----";
        let mut handled = false;
        let data_start = $offset;
        let data_end = $offset + $data.len() as u64;
        for hole in $state.bar_hole_infos.iter() {
            if hole.gpa == $base {
                match hole.usage {
                    BarHoleInfoUsage::Table => {
                        let (table_offset, table_size) = $state.cap.table_range();
                        let table_start = offset_from_lower_host_page(table_offset);
                        let table_end = table_start + table_size;
                        if table_start <= data_start && data_end <= table_end {
                            name = "MsiTable";
                            $state.config.$table_fn($offset, $data);
                        } else {
                            name = "OutsideTable";
                            let region_index = $state.cap.table_bir();
                            let _ = $region_fn(
                                &$device.file,
                                &$device.region_infos,
                                region_index as u32,
                                $offset,
                                $data,
                            );
                        }
                    }
                    BarHoleInfoUsage::Pba => {
                        let (table_offset, table_size) = $state.cap.pba_range();
                        let table_start = offset_from_lower_host_page(table_offset);
                        let table_end = table_start + table_size;
                        if table_start <= data_start && data_end <= table_end {
                            name = "PbaTable";
                            $state.config.$pba_fn($offset, $data);
                        } else {
                            name = "OutsideTable";
                            let region_index = $state.cap.pba_bir();
                            let _ = $region_fn(
                                &$device.file,
                                &$device.region_infos,
                                region_index as u32,
                                $offset,
                                $data,
                            );
                        }
                    }
                }
                handled = true;
            }
        }
        (name, handled)
    }};
}

// This should only serve BARs
impl BusDevice for VfioDeviceBundle {
    fn read(&mut self, base: u64, offset: u64, data: &mut [u8]) {
        let Some(state) = self.msix_state.as_ref() else {
            warn!("BusDevice::read called on VFIO device without MSI-X state");
            data.fill(0);
            return;
        };
        let (name, handled) = handle_bar_access!(
            state,
            self.device,
            base,
            offset,
            data,
            read_table,
            read_pba,
            vfio_device_region_read
        );
        if !handled {
            data.fill(0);
        }
        debug!(
            "[{}] base: {base:<#10x} offset: {offset:<#5x} data: {data:<4?} name: {name} handled: \
             {handled}",
            self.config.id,
        );
    }

    fn write(&mut self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        let Some(state) = self.msix_state.as_mut() else {
            warn!("BusDevice::write called on VFIO device without MSI-X state");
            return None;
        };
        let (name, handled) = handle_bar_access!(
            state,
            self.device,
            base,
            offset,
            data,
            write_table,
            write_pba,
            vfio_device_region_write
        );
        if !handled {
            warn!(
                "[{}] BusDevice::write not handled: base: {base:#x} offset: {offset:#x}",
                self.config.id
            );
        }
        debug!(
            "[{}] base: {base:<#10x} offset: {offset:<#5x} data: {data:<4?} table_name: {name}, \
             handled: {handled}",
            self.config.id
        );
        None
    }
}

// This should only serve config space
impl PciDevice for VfioDeviceBundle {
    fn write_config_register(
        &mut self,
        reg_idx: u16,
        offset: u8,
        data: &[u8],
    ) -> Option<Arc<Barrier>> {
        let mut name = "----";
        let mut handled: bool = false;

        if BAR0_REG_IDX <= reg_idx && reg_idx < BAR0_REG_IDX + u16::from(NUM_BAR_REGS) {
            // reg_idx is in [BAR0_REG, BAR0_REG+NUM_BAR_REGS), so the difference is 0..5.
            #[allow(clippy::cast_possible_truncation)]
            let bar_idx = (reg_idx - u16::from(BAR0_REG_IDX)) as u8;
            // offset is within a 4-byte PCI config register (0..3).
            #[allow(clippy::cast_possible_truncation)]
            let offset = offset as u8;
            self.bars.write(bar_idx, offset, data);
            name = "BAR";
            handled = true;
        } else if let Some(state) = self.msix_state.as_mut() {
            if reg_idx == u16::from(state.register) {
                // offset is within a 4-byte PCI config register (0..3).
                #[allow(clippy::cast_possible_truncation)]
                let offset = offset as u8;
                state.config.write_msg_ctl_register(offset, data);
                name = "MSIX_CAP";
                // Don't set `handled` since we need to passthrough write
                // to the msg_ctl register to the device, so it will enable Msix
                // interrupts
            }
        }
        let config_offset = reg_idx * 4 + u16::from(offset);
        if !handled {
            let _ = vfio_device_region_write(
                &self.device.file,
                &self.device.region_infos,
                VFIO_PCI_CONFIG_REGION_INDEX,
                u64::from(config_offset),
                data,
            );
        }
        debug!(
            "[{}] reg: {reg_idx:>3}({config_offset:>#6x}) data: {data:<4?} name: {name}",
            self.config.id
        );
        None
    }
    fn read_config_register(&mut self, reg_idx: u16) -> u32 {
        let mut name = "----";
        let config_offset = reg_idx as u64 * 4;
        let mut result: u32 = 0;
        if BAR0_REG_IDX <= reg_idx && reg_idx < BAR0_REG_IDX + u16::from(NUM_BAR_REGS) {
            // reg_idx is in [BAR0_REG, BAR0_REG+NUM_BAR_REGS), so the difference is 0..5.
            #[allow(clippy::cast_possible_truncation)]
            let bar_idx = (reg_idx - u16::from(BAR0_REG_IDX)) as u8;
            self.bars.read(bar_idx, 0, result.as_mut_bytes());
            name = "BAR";
        } else {
            let _ = vfio_device_region_read(
                &self.device.file,
                &self.device.region_infos,
                VFIO_PCI_CONFIG_REGION_INDEX,
                config_offset,
                result.as_mut_bytes(),
            );
            if let Some(state) = self.msix_state.as_ref() {
                if reg_idx == u16::from(state.register) {
                    result = (result & !(1 << 31 | 1 << 30))
                        | ((state.config.enabled as u32) << 31)
                        | ((state.config.masked as u32) << 30);
                    name = "MSIX_CAP";
                }
            }
            for mask in self.masks.iter() {
                if mask.register == reg_idx as u16 {
                    result = (result & mask.mask) | mask.value;
                    name = "MASK";
                    break;
                }
            }
        }
        debug!(
            "[{}] reg: {reg_idx:>3}({config_offset:>#6x}) data: {:<4?} name: {name}",
            self.config.id,
            result.as_bytes()
        );
        result
    }
}

pub fn vfio_open() -> Result<File, VfioError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/vfio/vfio")
        .map_err(VfioError::Open)
}

pub fn vfio_check_api_version(container: &impl AsRawFd) -> Result<(), VfioError> {
    let version = ioctls::check_api_version(container);
    debug!("vfio api version: {}", version);
    if version != VFIO_API_VERSION as i32 {
        return Err(VfioError::CheckApiVersion(version));
    }
    Ok(())
}

pub fn vfio_check_extension(container: &impl AsRawFd) -> Result<(), VfioError> {
    let ret = ioctls::check_extension(container, VFIO_TYPE1v2_IOMMU)?;
    if ret < 1 {
        return Err(VfioError::CheckExtension);
    }
    Ok(())
}

pub fn group_id_from_device_path(device_path: &impl AsRef<Path>) -> Result<u32, VfioError> {
    let uuid_path: std::path::PathBuf = device_path.as_ref().join("iommu_group");
    let group_path = uuid_path.read_link().map_err(VfioError::ReadIommuGroup)?;
    let group_osstr = group_path.file_name().ok_or(VfioError::InvalidGroupPath)?;
    let group_str = group_osstr.to_str().ok_or(VfioError::InvalidGroupPath)?;
    group_str.parse::<u32>().map_err(VfioError::ParseGroupId)
}

pub fn vfio_group_open(id: u32) -> Result<File, VfioError> {
    let group_path = Path::new("/dev/vfio").join(id.to_string());
    let group = OpenOptions::new()
        .read(true)
        .write(true)
        .open(group_path)
        .map_err(|e| VfioError::OpenGroup(id, e))?;

    let mut group_status = vfio_group_status {
        argsz: std::mem::size_of::<vfio_group_status>() as u32,
        flags: 0,
    };
    ioctls::group_get_status(&group, &mut group_status)?;
    if group_status.flags & VFIO_GROUP_FLAGS_VIABLE == 0 {
        return Err(VfioError::GroupNotViable(id));
    }
    Ok(group)
}

pub fn vfio_container_set_iommu(container: &impl AsRawFd, val: u32) -> Result<(), VfioError> {
    if val != VFIO_TYPE1_IOMMU && val != VFIO_TYPE1v2_IOMMU {
        return Err(VfioError::InvalidIommuType(val));
    }
    ioctls::set_iommu(container, val)?;
    Ok(())
}

pub fn vfio_group_get_device(
    group: &impl AsRawFd,
    path: &impl AsRef<Path>,
) -> Result<File, VfioError> {
    let uuid_osstr = path
        .as_ref()
        .file_name()
        .ok_or(VfioError::InvalidDevicePath)?;
    let uuid_str = uuid_osstr.to_str().ok_or(VfioError::InvalidDevicePath)?;
    let path = CString::new(uuid_str.as_bytes()).map_err(|_| VfioError::InvalidDevicePath)?;
    let device = ioctls::group_get_device_fd(group, &path)?;
    Ok(device)
}

pub fn vfio_device_get_info(device: &impl AsRawFd) -> Result<vfio_device_info, VfioError> {
    let mut dev_info = vfio_device_info {
        argsz: std::mem::size_of::<vfio_device_info>() as u32,
        flags: 0,
        num_regions: 0,
        num_irqs: 0,
        cap_offset: 0,
        pad: 0,
    };
    ioctls::device_get_info(device, &mut dev_info)?;
    Ok(dev_info)
}

pub fn vfio_device_reset(
    device: &impl AsRawFd,
    device_info: &vfio_device_info,
) -> Result<(), VfioError> {
    if device_info.flags & VFIO_DEVICE_FLAGS_RESET != 0 {
        ioctls::device_reset(device)?;
    }
    Ok(())
}

// TODO: we only care about BARs, so maybe we only need to query them.
// This means we can have `vfio_device_get_region_info` to get a single region instead of
// all at once.
pub fn vfio_device_get_region_infos(
    device: &impl AsRawFd,
    device_info: &vfio_device_info,
) -> Result<Vec<VfioRegionInfo>, VfioError> {
    let mut regions = Vec::with_capacity(device_info.num_regions as usize);
    for i in 0..device_info.num_regions {
        debug!("Getting bar region info: {}", i);
        let region_info_struct_size = std::mem::size_of::<vfio_region_info>() as u32;
        let mut region_info = vfio_region_info {
            argsz: region_info_struct_size,
            flags: 0,
            index: i,
            cap_offset: 0,
            size: 0,
            offset: 0,
        };
        if ioctls::device_get_region_info(device, &mut region_info).is_err()
            || region_info.size == 0
        {
            debug!("Region {i} is not available or not implemented. Setting to 0");
            let region_info = VfioRegionInfo {
                flags: 0,
                size: 0,
                offset: 0,
                caps: Vec::new(),
            };
            regions.push(region_info);
        } else {
            debug!("Flags: ");
            debug!(
                "VFIO_REGION_INFO_FLAG_READ: {}",
                region_info.flags & VFIO_REGION_INFO_FLAG_READ != 0
            );
            debug!(
                "VFIO_REGION_INFO_FLAG_WRITE: {}",
                region_info.flags & VFIO_REGION_INFO_FLAG_WRITE != 0
            );
            debug!(
                "VFIO_REGION_INFO_FLAG_MMAP: {}",
                region_info.flags & VFIO_REGION_INFO_FLAG_MMAP != 0
            );
            debug!(
                "VFIO_REGION_INFO_FLAG_CAPS: {}",
                region_info.flags & VFIO_REGION_INFO_FLAG_CAPS != 0
            );
            let mut caps = Vec::new();
            if region_info.flags & VFIO_REGION_INFO_FLAG_CAPS == 0
                || region_info.argsz <= region_info_struct_size
            {
                debug!("Region has no caps");
            } else {
                debug!("Region caps:");
                let mut vfio_region_info_with_caps =
                    VfioRegionInfoWithCap::new_with_argsz(region_info.argsz);
                let region_info_with_caps = vfio_region_info_with_caps.vfio_region_info_mut();
                region_info_with_caps.argsz = region_info.argsz;
                region_info_with_caps.flags = 0;
                region_info_with_caps.index = i;
                region_info_with_caps.cap_offset = 0;
                region_info_with_caps.size = 0;
                region_info_with_caps.offset = 0;
                ioctls::device_get_region_info(device, region_info_with_caps)?;

                let mut next_cap_offset = region_info_with_caps.cap_offset;
                while let Some(cap_header) =
                    vfio_region_info_with_caps.vfio_info_cap_header_at_offset(next_cap_offset)
                {
                    debug!("Cap id: {}", cap_header.id);
                    match u32::from(cap_header.id) {
                        VFIO_REGION_INFO_CAP_SPARSE_MMAP => {
                            // SAFETY: data structure returned by kernel is trusted.
                            let cap_sparse_mmap = unsafe {
                                &*(cap_header as *const vfio_info_cap_header
                                    as *const vfio_region_info_cap_sparse_mmap)
                            };
                            let areas = unsafe {
                                cap_sparse_mmap
                                    .areas
                                    .as_slice(cap_sparse_mmap.nr_areas as usize)
                            };
                            let areas = areas
                                .iter()
                                .map(|a| VfioRegionSparseMmapArea {
                                    offset: a.offset,
                                    size: a.size,
                                })
                                .collect();
                            let cap = VfioRegionCapSparseMmap { areas };
                            caps.push(VfioRegionCap::SparseMmap(cap));
                        }
                        VFIO_REGION_INFO_CAP_TYPE => {
                            // SAFETY: data structure returned by kernel is trusted.
                            let cap_type = unsafe {
                                &*(cap_header as *const vfio_info_cap_header
                                    as *const vfio_region_info_cap_type)
                            };
                            let cap = VfioRegionCapType {
                                type_: cap_type.type_,
                                subtype: cap_type.subtype,
                            };
                            caps.push(VfioRegionCap::Type(cap));
                        }
                        VFIO_REGION_INFO_CAP_MSIX_MAPPABLE => {
                            caps.push(VfioRegionCap::MsixMappable);
                        }
                        VFIO_REGION_INFO_CAP_NVLINK2_SSATGT => {
                            // SAFETY: data structure returned by kernel is trusted.
                            let cap_nvlink2_ssatgt = unsafe {
                                &*(cap_header as *const vfio_info_cap_header
                                    as *const vfio_region_info_cap_nvlink2_ssatgt)
                            };
                            let cap = VfioRegionCapNvlink2Ssatgt {
                                tgt: cap_nvlink2_ssatgt.tgt,
                            };
                            caps.push(VfioRegionCap::Nvlink2Ssatgt(cap));
                        }
                        VFIO_REGION_INFO_CAP_NVLINK2_LNKSPD => {
                            // SAFETY: data structure returned by kernel is trusted.
                            let cap_nvlink2_lnkspd = unsafe {
                                &*(cap_header as *const vfio_info_cap_header
                                    as *const vfio_region_info_cap_nvlink2_lnkspd)
                            };
                            let cap = VfioRegionCapNvlink2Lnkspd {
                                link_speed: cap_nvlink2_lnkspd.link_speed,
                            };
                            caps.push(VfioRegionCap::Nvlink2Lnkspd(cap));
                        }
                        _ => {
                            debug!("Got unknown region capability id: {}", cap_header.id);
                        }
                    }
                    next_cap_offset = cap_header.next;
                }
            }
            let region_info = VfioRegionInfo {
                flags: region_info.flags,
                size: region_info.size,
                offset: region_info.offset,
                caps,
            };
            debug!(
                "Region {i} info: flags: {:x} index: {} size: {} offset: {}",
                region_info.flags, i, region_info.size, region_info.offset
            );
            for cap in region_info.caps.iter() {
                debug!("Cap: {:?}", cap);
            }
            regions.push(region_info);
        }
    }
    Ok(regions)
}

// TODO: we only care about MSI-X interrupt which has index of 2. Maybe
// instead of this generic function, it is better to have vfio_device_get_irq_info
// which queries one specific interrupt type.
pub fn vfio_device_get_irq_infos(
    device: &impl AsRawFd,
    device_info: &vfio_device_info,
) -> Vec<vfio_irq_info> {
    let mut irqs = Vec::with_capacity(device_info.num_irqs as usize);
    for i in 0..device_info.num_irqs {
        let mut irq_info = vfio_irq_info {
            argsz: std::mem::size_of::<vfio_irq_info>() as u32,
            flags: 0,
            index: i,
            count: 0,
        };
        match ioctls::device_get_irq_info(device, &mut irq_info) {
            Ok(()) => {
                debug!("Irq info: {:?}", irq_info);
                debug!(
                    "VFIO_IRQ_INFO_EVENTFD: {}",
                    irq_info.flags & VFIO_IRQ_INFO_EVENTFD != 0
                );
                debug!(
                    "VFIO_IRQ_INFO_MASKABLE  :{}",
                    irq_info.flags & VFIO_IRQ_INFO_MASKABLE != 0
                );
                debug!(
                    "VFIO_IRQ_INFO_AUTOMASKED  :{}",
                    irq_info.flags & VFIO_IRQ_INFO_AUTOMASKED != 0
                );
                debug!(
                    "VFIO_IRQ_INFO_NORESIZE  :{}",
                    irq_info.flags & VFIO_IRQ_INFO_NORESIZE != 0
                );
            }
            Err(e) => {
                // reset count to 0 just in case.
                // TODO: is this really needed?
                irq_info.count = 0;
                debug!("Irq info: got error: {:?}", e);
            }
        }
        irqs.push(irq_info);
    }
    irqs
}

pub fn vfio_device_region_read(
    device: &impl FileExt,
    region_infos: &[VfioRegionInfo],
    index: u32,
    offset: u64,
    buf: &mut [u8],
) -> Result<(), VfioError> {
    let region_info = &region_infos[index as usize];
    let end = offset + buf.len() as u64;
    assert!(end <= region_info.size);
    device
        .read_exact_at(buf, region_info.offset + offset)
        .map_err(|e| VfioError::RegionRead(index, offset, e))?;
    Ok(())
}

pub fn vfio_device_region_write(
    device: &impl FileExt,
    region_infos: &[VfioRegionInfo],
    index: u32,
    offset: u64,
    buf: &[u8],
) -> Result<(), VfioError> {
    let region_info = &region_infos[index as usize];
    let end = offset + buf.len() as u64;
    assert!(end <= region_info.size);
    device
        .write_all_at(buf, region_info.offset + offset)
        .map_err(|e| VfioError::RegionWrite(index, offset, e))?;
    Ok(())
}

pub fn vfio_device_get_pci_capabilities(
    device: &impl FileExt,
    region_infos: &[VfioRegionInfo],
    irq_infos: &[vfio_irq_info],
) -> Result<(Option<(MsixCap, u8)>, Vec<RegisterMask>), VfioError> {
    let mut next_cap_offset: u8 = 0;
    vfio_device_region_read(
        device,
        region_infos,
        VFIO_PCI_CONFIG_REGION_INDEX,
        PCI_CONFIG_CAPABILITY_OFFSET as u64,
        next_cap_offset.as_mut_bytes(),
    )?;

    let mut has_pci_express_cap = false;

    let mut msix_cap_and_register = None;
    debug!("PCI CAPS offset: {}", next_cap_offset);
    // The legacy region with PCI capis is 256 bytes long and
    // split into 4 byte registers.
    const LOOP_UPPER_BOUND: u32 = 256 / 4;
    let mut loop_bound: u32 = 0;
    while next_cap_offset != 0 && loop_bound < LOOP_UPPER_BOUND {
        loop_bound += 1;

        let mut cap_id_and_next_ptr: u16 = 0;
        vfio_device_region_read(
            device,
            region_infos,
            VFIO_PCI_CONFIG_REGION_INDEX,
            next_cap_offset as u64,
            cap_id_and_next_ptr.as_mut_bytes(),
        )?;

        let current_cap_offset = next_cap_offset;

        // 7.5.3.1 PCI Express Capability List Register
        // |      2 bytes    |     1 byte    |          1 byte         |
        // |   Cap register  | Capability ID | Next Capability Pointer |
        let cap_id: u8 = (cap_id_and_next_ptr & 0xff) as u8;
        next_cap_offset = ((cap_id_and_next_ptr & 0xff00) >> 8) as u8;
        debug!("PCI CAP id: {cap_id} next offset: {next_cap_offset:#x}");

        match PciCapabilityId::from(cap_id) {
            PciCapabilityId::MessageSignalledInterrupts => {
                if let Some(irq_info) = irq_infos.get(VFIO_PCI_MSI_IRQ_INDEX as usize) {
                    if irq_info.count != 0 {
                        let register = current_cap_offset / 4;
                        debug!("Found MSI cap at offset: {current_cap_offset:#x}({register})");
                    } else {
                        debug!("Found MSI cap, but the device does not support MSI interrupts.");
                    }
                }
            }
            PciCapabilityId::MsiX => {
                if let Some(irq_info) = irq_infos.get(VFIO_PCI_MSIX_IRQ_INDEX as usize) {
                    if irq_info.count != 0 {
                        let register = current_cap_offset / 4;
                        debug!("Found MSIX cap at offset: {current_cap_offset:#x}({register})");

                        // 7.7.2 MSI-X Capability and Table Structure
                        let mut msg_ctl: u16 = 0;
                        let mut table: u32 = 0;
                        let mut pba: u32 = 0;
                        vfio_device_region_read(
                            device,
                            region_infos,
                            VFIO_PCI_CONFIG_REGION_INDEX,
                            (current_cap_offset as u64) + 2,
                            msg_ctl.as_mut_bytes(),
                        )?;
                        vfio_device_region_read(
                            device,
                            region_infos,
                            VFIO_PCI_CONFIG_REGION_INDEX,
                            (current_cap_offset as u64) + 4,
                            table.as_mut_bytes(),
                        )?;
                        vfio_device_region_read(
                            device,
                            region_infos,
                            VFIO_PCI_CONFIG_REGION_INDEX,
                            (current_cap_offset as u64) + 8,
                            pba.as_mut_bytes(),
                        )?;
                        msix_cap_and_register = Some((
                            MsixCap {
                                msg_ctl,
                                table,
                                pba,
                            },
                            register,
                        ));
                    } else {
                        debug!(
                            "Found MSI-X cap, but the device does not support MSI-X interrupts."
                        );
                    }
                }
            }
            PciCapabilityId::PciExpress => {
                let register = current_cap_offset / 4;
                debug!("Found PciExpress cap at offset: {current_cap_offset:#x}({register})");

                has_pci_express_cap = true;
            }
            // 7.5.2 PCI Power Management Capability Structure
            // This structure is required for all PCI Express Functions.
            // But I think just PciExpress shoudl be enough?
            _ => {}
        };
    }

    let mut masks = Vec::new();
    if has_pci_express_cap {
        let mut next_cap_offset: u16 = PCI_CONFIG_EXTENDED_CAPABILITY_OFFSET as u16;

        // The PCIe region is 4K in size and split into 4 byte registers
        const LOOP_UPPER_BOUND: u32 = 4096 / 4;
        let mut loop_bound: u32 = 0;
        while next_cap_offset != 0 && loop_bound < LOOP_UPPER_BOUND {
            loop_bound += 1;

            let mut cap_id_and_next_ptr: u32 = 0;
            vfio_device_region_read(
                device,
                region_infos,
                VFIO_PCI_CONFIG_REGION_INDEX,
                next_cap_offset as u64,
                cap_id_and_next_ptr.as_mut_bytes(),
            )?;
            let current_cap_offset = next_cap_offset;

            // 7.7.3.1 Secondary PCI Express Extended Capability Header
            // |           31-20        |         19-16       |          15-0         |
            // | Next capability offset | Capability Version  |   PCIe Capability ID  |
            let cap_id: u16 = (cap_id_and_next_ptr & 0xffff) as u16;
            next_cap_offset = (cap_id_and_next_ptr >> 20) as u16;

            let pci_cap = PciExpressCapabilityId::from(cap_id);
            let register = current_cap_offset / 4;
            debug!("Found {pci_cap:?} cap at offset: {current_cap_offset:#x}({register})");

            // NOTE: the list of capabilities is hardcoded for now. In the future this
            // may be configurable from the user side.
            match pci_cap {
                PciExpressCapabilityId::AlternativeRoutingIdentificationInterpretation
                | PciExpressCapabilityId::ResizeableBar
                | PciExpressCapabilityId::SingleRootIoVirtualization => {
                    debug!(
                        "Found cap to be masked at register: {register}({current_cap_offset:#x})"
                    );
                    masks.push(RegisterMask {
                        register,
                        mask: 0xffff_0000,
                        value: 0x0000_0000,
                    })
                }
                _ => {}
            }
        }
    }
    Ok((msix_cap_and_register, masks))
}

fn vfio_device_get_single_bar_info(
    device: &impl FileExt,
    region_infos: &[VfioRegionInfo],
    bar_idx: u8,
) -> Result<(u32, u32), VfioError> {
    // 7.5.1.2.1 Base Address Registers
    // IMPLEMENTATION NOTE: SIZING A 32-BIT BASE ADDRESS REGISTER
    let bar_offset = u64::from(PCI_CONFIG_BAR_OFFSET) + u64::from(bar_idx) * 4;
    let mut value: u32 = 0;
    let mut size: u32 = 0;
    vfio_device_region_read(
        device,
        region_infos,
        VFIO_PCI_CONFIG_REGION_INDEX,
        bar_offset,
        value.as_mut_bytes(),
    )?;
    vfio_device_region_write(
        device,
        region_infos,
        VFIO_PCI_CONFIG_REGION_INDEX,
        bar_offset,
        0xFFFF_FFFF_u32.as_bytes(),
    )?;
    vfio_device_region_read(
        device,
        region_infos,
        VFIO_PCI_CONFIG_REGION_INDEX,
        bar_offset,
        size.as_mut_bytes(),
    )?;
    vfio_device_region_write(
        device,
        region_infos,
        VFIO_PCI_CONFIG_REGION_INDEX,
        bar_offset,
        value.as_bytes(),
    )?;
    Ok((value, size))
}

pub fn vfio_device_get_bars(
    device: &impl FileExt,
    region_infos: &[VfioRegionInfo],
    resource_allocator: &mut ResourceAllocator,
) -> Result<Bars, VfioError> {
    let mut bars = Bars::default();
    let mut bar_idx = 0;
    while bar_idx < BAR_NUMS {
        let (bar_info, mut lower_size) =
            vfio_device_get_single_bar_info(device, region_infos, bar_idx)?;

        let is_io_bar = bar_info & PCI_CONFIG_IO_BAR != 0;
        let is_64_bits = bar_info & PCI_CONFIG_MEMORY_BAR_64BIT != 0;
        let is_prefetchable = bar_info & PCI_CONFIG_BAR_PREFETCHABLE != 0;

        let mut size = 0;
        if is_io_bar {
            if lower_size != 0 {
                lower_size &= !0b11;
                lower_size = !lower_size + 1;
                size = u64::from(lower_size);
            }
        } else if !is_64_bits {
            if lower_size != 0 {
                lower_size &= !0b1111;
                lower_size = !lower_size + 1;
                size = u64::from(lower_size);
            }
        } else {
            let (_, upper_size) =
                vfio_device_get_single_bar_info(device, region_infos, bar_idx + 1)?;

            size = u64::from(upper_size) << 32 | u64::from(lower_size);
            size &= !0b1111;
            size = !size + 1;
        }
        if size != 0 {
            fn calculate_alignment(size: u64) -> u64 {
                // 7.5.1.2.1 Base Address Registers
                // This design implies that all address spaces used are a power of two
                // in size and are naturally aligned.
                let alignment = std::cmp::max(host_page_size(), 1 << size.trailing_zeros());
                usize_to_u64(alignment)
            }

            let idx = bar_idx;
            let gpa;
            if is_io_bar {
                debug!(
                    "BAR{bar_idx} size: {size:>#10x} io_bar: {is_io_bar} 64bits: {is_64_bits} \
                     prefetchable: {is_prefetchable} Skipping"
                );
                // TODO: Handle IO BARs somehow
                bar_idx += 1;
                continue;
            } else if is_64_bits {
                let alignment = calculate_alignment(size);
                gpa = resource_allocator
                    .mmio64_memory
                    .allocate(size, alignment, AllocPolicy::FirstMatch)
                    .map_err(|_| VfioError::BarAllocation)?
                    .start();
                bar_idx += 1;
                bars.set_bar_64(idx, gpa, size, is_prefetchable);
            } else {
                let alignment = calculate_alignment(size);
                gpa = resource_allocator
                    .mmio32_memory
                    .allocate(size, alignment, AllocPolicy::FirstMatch)
                    .map_err(|_| VfioError::BarAllocation)?
                    .start();
                assert!(gpa < u64::from(u32::MAX));
                assert!(size < u64::from(u32::MAX));
                #[allow(clippy::cast_possible_truncation)]
                let gpa = gpa as u32;
                #[allow(clippy::cast_possible_truncation)]
                let size = size as u32;
                bars.set_bar_32(idx, gpa, size, is_prefetchable);
            }
            debug!(
                "BAR{bar_idx} gpa: [{:#x}..{:#x}] size: {size:>#10x} io_bar: {is_io_bar} 64bits: \
                 {is_64_bits} prefetchable: {is_prefetchable}",
                gpa,
                gpa + size
            );
        } else {
            debug!(
                "BAR{bar_idx} size: {size:>#10x} io_bar: {is_io_bar} 64bits: {is_64_bits} \
                 prefetchable: {is_prefetchable}"
            );
        }
        bar_idx += 1;
    }
    Ok(bars)
}

pub fn get_device(group: &impl AsRawFd, path: &str) -> Result<VfioDevice, VfioError> {
    let device_file = vfio_group_get_device(group, &path)?;
    let device_info = vfio_device_get_info(&device_file)?;
    debug!("Device info: {device_info:#?}");

    if device_info.flags & VFIO_DEVICE_FLAGS_PCI == 0 {
        return Err(VfioError::NotPciDevice);
    }

    vfio_device_reset(&device_file, &device_info)?;

    let device_region_infos = vfio_device_get_region_infos(&device_file, &device_info)?;
    let device_irq_infos = vfio_device_get_irq_infos(&device_file, &device_info);

    Ok(VfioDevice {
        file: device_file,
        info: device_info,
        region_infos: device_region_infos,
        irq_infos: device_irq_infos,
    })
}

/// Intermediate type to store areas needed to be mmaped for the device
#[derive(Debug, Clone, Copy)]
struct BarArea {
    /// offset
    bar_gpa: u64,
    /// offset
    region_offset: u64,
    /// offset
    offset: u64,
    /// size
    size: u64,
    /// prot
    prot: i32,
}

/// Calculate areas needed to be mmaped for the devic BARs including any BAR holes caused
/// by MSI-X table/pba
fn calculate_bar_areas(
    bars: &Bars,
    region_infos: &[VfioRegionInfo],
    msix_cap: Option<&MsixCap>,
) -> (ArrayVec<BarArea, 8>, ArrayVec<BarHoleInfo, 2>) {
    let mut areas = ArrayVec::<BarArea, 8>::new();
    let mut bar_hole_infos = ArrayVec::<BarHoleInfo, 2>::new();
    let mut bar_idx: u8 = 0;
    while bar_idx < NUM_BAR_REGS {
        let bar_gpa = bars.get_bar_addr(bar_idx);
        if bar_gpa != 0 {
            let region_info = &region_infos[bar_idx as usize];
            let mut has_msix_mappable = false;
            let mut sparse_mmap_cap = None;
            for cap in region_info.caps.iter() {
                match cap {
                    VfioRegionCap::SparseMmap(cap) => sparse_mmap_cap = Some(cap),
                    VfioRegionCap::MsixMappable => has_msix_mappable = true,
                    _ => {}
                }
            }
            let mut contain_msix_table: bool = false;
            let mut msix_table_offset = 0;
            let mut msix_table_size = 0;

            let mut contain_msix_pba: bool = false;
            let mut msix_pba_offset = 0;
            let mut msix_pba_size = 0;

            if let Some(msix_cap) = msix_cap {
                contain_msix_table = bar_idx == msix_cap.table_bir();
                if contain_msix_table {
                    let (offset, size) = msix_cap.table_range();
                    let offset_in_hole = offset_from_lower_host_page(offset);

                    msix_table_offset = align_down_host_page(offset);
                    msix_table_size = align_up_host_page(offset_in_hole + size);

                    debug!(
                        "BAR{} msix_table hole: [{:#x}..{:#x}] actual table: [{:#x} ..{:#x}]",
                        bar_idx,
                        bar_gpa + msix_table_offset,
                        bar_gpa + msix_table_offset + msix_table_size,
                        bar_gpa + offset_in_hole,
                        bar_gpa + offset_in_hole + size,
                    );

                    let info = BarHoleInfo {
                        gpa: bar_gpa + msix_table_offset,
                        size: msix_table_size,
                        usage: BarHoleInfoUsage::Table,
                    };
                    bar_hole_infos.push(info);
                }

                contain_msix_pba = bar_idx == msix_cap.pba_bir();
                if contain_msix_pba {
                    let (offset, size) = msix_cap.pba_range();
                    let offset_in_hole = offset_from_lower_host_page(offset);

                    msix_pba_offset = align_down_host_page(offset);
                    msix_pba_size = align_up_host_page(offset_in_hole + size);

                    debug!(
                        "BAR{} pba_table hole: [{:#x} ..{:#x}] actual table: [{:#x} ..{:#x}]",
                        bar_idx,
                        bar_gpa + msix_pba_offset,
                        bar_gpa + msix_pba_offset + msix_pba_size,
                        bar_gpa + offset_in_hole,
                        bar_gpa + offset_in_hole + size,
                    );

                    let info = BarHoleInfo {
                        gpa: bar_gpa + msix_pba_offset,
                        size: msix_pba_size,
                        usage: BarHoleInfoUsage::Pba,
                    };
                    bar_hole_infos.push(info);
                }
            }

            if (contain_msix_table || contain_msix_pba)
                && !has_msix_mappable
                && sparse_mmap_cap.is_none()
            {
                debug!(
                    "BAR{} contains msix_table: {} msix_pba: {}, but mappable is {} and \
                     sparse_mmap_cap is {}. Skipping",
                    bar_idx,
                    contain_msix_table,
                    contain_msix_pba,
                    has_msix_mappable,
                    sparse_mmap_cap.is_some()
                );
            } else {
                let can_mmap = region_info.flags & VFIO_REGION_INFO_FLAG_MMAP != 0;
                if can_mmap || sparse_mmap_cap.is_some() {
                    let mut prot = 0;
                    if region_info.flags & VFIO_REGION_INFO_FLAG_READ != 0 {
                        prot |= libc::PROT_READ;
                    }
                    if region_info.flags & VFIO_REGION_INFO_FLAG_WRITE != 0 {
                        prot |= libc::PROT_WRITE;
                    }
                    let region_size = region_info.size;

                    if let Some(cap) = sparse_mmap_cap {
                        for area in cap.areas.iter() {
                            areas.push(BarArea {
                                bar_gpa,
                                region_offset: region_info.offset,
                                offset: area.offset,
                                size: area.size,
                                prot,
                            });
                        }
                    } else if has_msix_mappable {
                        let mut first_gap_offset = msix_table_offset;
                        let mut first_gap_size = msix_table_size;
                        let mut second_gap_offset = msix_pba_offset;
                        let mut second_gap_size = msix_pba_size;
                        if second_gap_offset < first_gap_offset {
                            second_gap_offset = msix_table_offset;
                            second_gap_size = msix_table_size;
                            first_gap_offset = msix_pba_offset;
                            first_gap_size = msix_pba_size;
                        }
                        let mut offset = 0;
                        if first_gap_size != 0 {
                            let area_size = first_gap_offset - offset;
                            if area_size != 0 {
                                areas.push(BarArea {
                                    bar_gpa,
                                    region_offset: region_info.offset,
                                    offset: offset,
                                    size: area_size,
                                    prot,
                                });
                            }
                            offset = first_gap_offset + first_gap_size;
                        }
                        if second_gap_size != 0 {
                            let area_size = second_gap_offset - offset;
                            if area_size != 0 {
                                areas.push(BarArea {
                                    bar_gpa,
                                    region_offset: region_info.offset,
                                    offset: offset,
                                    size: area_size,
                                    prot,
                                });
                            }
                            offset = second_gap_offset + second_gap_size;
                        }
                        let area_size = region_size - offset;
                        if area_size != 0 {
                            areas.push(BarArea {
                                bar_gpa,
                                region_offset: region_info.offset,
                                offset: offset,
                                size: area_size,
                                prot,
                            });
                        }
                    } else {
                        areas.push(BarArea {
                            bar_gpa,
                            region_offset: region_info.offset,
                            offset: 0,
                            size: region_size,
                            prot,
                        });
                    }
                }
            }
        }
        if bars.bars[bar_idx as usize].is_64bit() {
            bar_idx += 1;
        }
        bar_idx += 1;
    }
    (areas, bar_hole_infos)
}

pub fn dma_map_guest_memory(
    container: &impl AsRawFd,
    guest_memory: &GuestMemoryMmap,
) -> Result<(), VfioError> {
    // NOTE: if viortio-iommu is attached no dma setup is
    // needed at this stage
    for region in guest_memory.iter() {
        if region.region_type == GuestRegionType::Dram {
            let region = &region.inner;

            let mapping_prot = region.prot();
            let mut flags: u32 = 0;
            if mapping_prot & libc::PROT_READ != 0 {
                flags |= VFIO_DMA_MAP_FLAG_READ;
            }
            if mapping_prot & libc::PROT_WRITE != 0 {
                flags |= VFIO_DMA_MAP_FLAG_WRITE;
            }
            let vaddr = region.as_ptr() as u64;
            let iova = region.start_addr().0 as u64;
            let size = region.size() as u64;

            let dma_map = vfio_iommu_type1_dma_map {
                argsz: std::mem::size_of::<vfio_iommu_type1_dma_map>() as u32,
                flags,
                vaddr,
                iova,
                size,
            };
            debug!("DMA map guest memory: [{:#x}..{:#x}]", iova, iova + size);
            ioctls::iommu_map_dma(container, &dma_map)?;
        }
    }
    Ok(())
}

pub fn dma_unmap_guest_memory(
    container: &impl AsRawFd,
    guest_memory: &GuestMemoryMmap,
) -> Result<(), VfioError> {
    // NOTE: if viortio-iommu is attached no dma setup is
    // needed at this stage
    for region in guest_memory.iter() {
        if region.region_type == GuestRegionType::Dram {
            let region = &region.inner;

            let iova = region.start_addr().0 as u64;
            let size = region.size() as u64;

            let mut dma_unmap = vfio_iommu_type1_dma_unmap {
                argsz: std::mem::size_of::<vfio_iommu_type1_dma_unmap>() as u32,
                flags: 0,
                iova,
                size,
                data: Default::default(),
            };
            debug!("DMA unmap guest memory: [{:#x}..{:#x}]", iova, iova + size);
            ioctls::iommu_unmap_dma(container, &mut dma_unmap)?;
            if dma_unmap.size != size {
                return Err(VfioError::PartialDmaUnmap(size, dma_unmap.size));
            }
        }
    }
    Ok(())
}

pub fn set_msix_irqs(
    device: &impl AsRawFd,
    irq_infos: &[vfio_irq_info],
    msix_config: &MsixConfig,
) -> Result<(), VfioError> {
    let msix_irq_info = &irq_infos[VFIO_PCI_MSIX_IRQ_INDEX as usize];
    if msix_irq_info.count == 0 || msix_config.vectors.vectors.len() != msix_irq_info.count as usize
    {
        debug!("Skipping MSI setup of vfio");
        return Ok(());
    }

    let mut vfio_irq_set = VfioIrqSet::new_with_entries(u64::from(msix_irq_info.count));
    let vfio_irq_set_bytes = vfio_irq_set.bytes.len();
    {
        let irq_set = vfio_irq_set.irq_set_mut();
        irq_set.argsz = vfio_irq_set_bytes as u32;
        irq_set.flags = VFIO_IRQ_SET_DATA_EVENTFD | VFIO_IRQ_SET_ACTION_TRIGGER;
        irq_set.index = VFIO_PCI_MSIX_IRQ_INDEX;
        irq_set.start = 0;
        irq_set.count = msix_irq_info.count;
    }
    {
        let irq_fds = vfio_irq_set.entries_mut();
        for (fd, v) in irq_fds.iter_mut().zip(&msix_config.vectors.vectors) {
            *fd = v.event_fd.as_raw_fd();
        }
    }
    ioctls::device_set_irqs(device, vfio_irq_set.irq_set_mut())?;
    Ok(())
}

pub fn unset_msix_irqs(
    device: &impl AsRawFd,
    irq_infos: &[vfio_irq_info],
) -> Result<(), VfioError> {
    let msix_irq_info = &irq_infos[VFIO_PCI_MSIX_IRQ_INDEX as usize];
    if msix_irq_info.count == 0 {
        return Ok(());
    }

    let mut vfio_irq_set = VfioIrqSet::<i32>::new_with_entries(0);
    let vfio_irq_set_bytes = vfio_irq_set.bytes.len();
    {
        let irq_set = vfio_irq_set.irq_set_mut();
        irq_set.argsz = vfio_irq_set_bytes as u32;
        irq_set.flags = VFIO_IRQ_SET_DATA_NONE | VFIO_IRQ_SET_ACTION_TRIGGER;
        irq_set.index = VFIO_PCI_MSIX_IRQ_INDEX;
        irq_set.start = 0;
        irq_set.count = 0;
    }
    ioctls::device_set_irqs(device, vfio_irq_set.irq_set_mut())?;
    Ok(())
}

/// Init KVM_DEV_TYPE_VFIO device
pub fn init_kvm_device_vfio(vm: &Vm) -> Result<DeviceFd, VfioError> {
    let mut vfio_dev = kvm_create_device {
        type_: kvm_device_type_KVM_DEV_TYPE_VFIO,
        fd: 0,
        flags: 0,
    };
    vm.fd()
        .create_device(&mut vfio_dev)
        .map_err(VfioError::KVMCreateVfioDevice)
}
// The `file` in this case shoud be a group `File` descriptor.
// flag must be either KVM_DEV_VFIO_FILE_ADD or KVM_DEV_VFIO_FILE_DEL
fn kvm_device_vfio_file_add_del(
    device: &DeviceFd,
    vfio_group: &impl AsRawFd,
    flag: u32,
) -> Result<(), VfioError> {
    assert!(flag == KVM_DEV_VFIO_FILE_ADD || flag == KVM_DEV_VFIO_FILE_DEL);
    let file_fd = vfio_group.as_raw_fd();
    let dev_attr = kvm_device_attr {
        flags: 0,
        group: KVM_DEV_VFIO_FILE,
        attr: flag as u64,
        addr: (&file_fd as *const i32) as u64,
    };
    device
        .set_device_attr(&dev_attr)
        .map_err(VfioError::KVMVfioDeviceFileAdd)
}
// The `file` in this case shoud be a group `File` descriptor.
pub fn kvm_device_vfio_file_add(
    device: &DeviceFd,
    vfio_group: &impl AsRawFd,
) -> Result<(), VfioError> {
    kvm_device_vfio_file_add_del(device, vfio_group, KVM_DEV_VFIO_FILE_ADD)
}
// The `file` in this case shoud be a group `File` descriptor.
pub fn kvm_device_vfio_file_del(
    device: &DeviceFd,
    vfio_group: &impl AsRawFd,
) -> Result<(), VfioError> {
    kvm_device_vfio_file_add_del(device, vfio_group, KVM_DEV_VFIO_FILE_DEL)
}

/// Init VFIO container
pub fn init_vfio_container() -> Result<File, VfioError> {
    let container = vfio_open()?;
    vfio_check_api_version(&container)?;
    vfio_check_extension(&container)?;
    Ok(container)
}

fn map_bar_mapping(
    container: &impl AsRawFd,
    device: &impl AsRawFd,
    vm: &Vm,
    area: &BarArea,
    slot: u32,
) -> Result<BarMapping, VfioError> {
    // SAFETY: FFI call with correct arguments
    let host_addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            area.size as usize,
            area.prot,
            libc::MAP_SHARED,
            device.as_raw_fd(),
            (area.region_offset + area.offset) as i64,
        )
    };

    if host_addr == libc::MAP_FAILED {
        return Err(VfioError::Mmap);
    }

    let slot = slot;
    let iova = area.bar_gpa + area.offset;
    let size = area.size;
    let host_addr = host_addr as u64;

    let kvm_memory_region = kvm_userspace_memory_region {
        slot,
        flags: 0,
        guest_phys_addr: iova,
        memory_size: size,
        userspace_addr: host_addr,
    };
    if let Err(e) = vm.set_user_memory_region(kvm_memory_region) {
        let r = unsafe { libc::munmap(host_addr as *mut libc::c_void, u64_to_usize(size)) };
        if r < 0 {
            error!(
                "Error on unmapping host memory on VFIO device creation failure: {r:?}. \
                 Continuing with other regions removal."
            );
        }
        return Err(VfioError::SetUserMemoryRegion(e.to_string()));
    }

    // NOTE: if viortio-iommu is attached no dma setup is
    // needed at this stage
    let dma_map = vfio_iommu_type1_dma_map {
        argsz: std::mem::size_of::<vfio_iommu_type1_dma_map>() as u32,
        // NOTE: VFIO_DMA_MAP_FLAG_READ and VFIO_DMA_MAP_FLAG_WRITE flags are
        // same as PROT_READ and PROT_WRITE
        flags: area.prot as u32,
        vaddr: host_addr,
        iova: iova,
        size: size,
    };
    if let Err(e) = ioctls::iommu_map_dma(container, &dma_map) {
        let kvm_memory_region = kvm_userspace_memory_region {
            slot: slot,
            flags: 0,
            guest_phys_addr: iova,
            memory_size: 0,
            userspace_addr: host_addr,
        };
        if let Err(ee) = vm.set_user_memory_region(kvm_memory_region) {
            error!(
                "Error on removing KVM region on VFIO device creation failure: {ee:?}. Continuing \
                 with other regions removal."
            );
        }
        let r = unsafe { libc::munmap(host_addr as *mut libc::c_void, u64_to_usize(size)) };
        if r < 0 {
            error!(
                "Error on unmapping host memory on VFIO device creation failure: {r:?}. \
                 Continuing with other regions removal."
            );
        }
        return Err(e.into());
    }
    let mapping = BarMapping {
        slot,
        iova,
        size,
        host_addr,
    };
    Ok(mapping)
}

fn unmap_bar_mapping(container: &impl AsRawFd, vm: &Vm, mapping: &BarMapping) {
    let kvm_memory_region = kvm_userspace_memory_region {
        slot: mapping.slot,
        flags: 0,
        guest_phys_addr: mapping.iova,
        memory_size: 0,
        userspace_addr: mapping.host_addr,
    };
    // TODO: what should happen if this fails as well?
    if let Err(ee) = vm.set_user_memory_region(kvm_memory_region) {
        error!(
            "Error on removing KVM region on VFIO device creation failure: {ee:?}. Continuing \
             with other regions removal."
        );
    }

    let mut dma_unmap = vfio_iommu_type1_dma_unmap {
        argsz: std::mem::size_of::<vfio_iommu_type1_dma_unmap>() as u32,
        flags: 0,
        iova: mapping.iova,
        size: mapping.size,
        data: Default::default(),
    };
    if let Err(ee) = ioctls::iommu_unmap_dma(container, &mut dma_unmap) {
        error!(
            "Error on unmapping DMA region on VFIO device creation failure: {ee:?}. Continuing \
             with other regions removal."
        );
    }
    if dma_unmap.size != mapping.size {
        error!(
            "Partial DMA unmap: requested {:#x}, got {:#x}",
            mapping.size, dma_unmap.size
        );
    }

    let r = unsafe {
        libc::munmap(
            mapping.host_addr as *mut libc::c_void,
            u64_to_usize(mapping.size),
        )
    };
    if r < 0 {
        error!(
            "Error on unmapping host memory on VFIO device creation failure: {r:?}. Continuing \
             with other regions removal."
        );
    }
}

/// Cleanup BAR mappings and DMA regions for a VFIO device bundle.
pub fn vfio_device_unmap_all_bars(bundle: &mut VfioDeviceBundle, container: &impl AsRawFd) {
    for mapping in bundle.bar_mappings.iter() {
        unmap_bar_mapping(container, &bundle.vm, mapping);
    }
    bundle.bar_mappings.clear();
}

/// The reason this is a separte functions is to ease the error handling of VFIO device creation.
/// This function does all the preparation steps with the VFIO device like gathering
/// information about regions, irqs and setting up the DMA/KVM regions and MSI-X vectors.
/// But if any error occurs inside this function, all DMA/KVM regions are destroyed.
/// Outer caller will need to deal with the rest of the init process.
pub fn prepare_vfio_device(
    container: &impl AsRawFd,
    group: &impl AsRawFd,
    vm: &Arc<Vm>,
    device_path: &str,
    sbdf: PciSBDF,
    first_vfio_device: bool,
) -> Result<
    (
        VfioDevice,
        Bars,
        ArrayVec<BarMapping, 8>,
        Option<MsixState>,
        Vec<RegisterMask>,
    ),
    VfioError,
> {
    if first_vfio_device {
        // If anything after this call, we don't need to do anything here.
        // The error handling in the caller should remove the device group from
        // the container which will trigger reset of iommu if that group was the
        // last one.
        vfio_container_set_iommu(container, VFIO_TYPE1v2_IOMMU)?;

        // Query IOMMU info for page sizes and capabilities.
        // TODO maybe worth taking `iova_pgsize` into account
        let iommu_info = vfio_iommu_type1_info {
            argsz: std::mem::size_of::<vfio_iommu_type1_info>() as u32,
            flags: 0,
            iova_pgsizes: 0,
            cap_offset: 0,
            pad: 0,
        };
        match ioctls::iommu_get_info(container, &iommu_info) {
            Ok(()) => {
                debug!(
                    "IOMMU info: flags={:#x} iova_pgsizes={:#x}",
                    iommu_info.flags, iommu_info.iova_pgsizes
                );
            }
            Err(e) => {
                warn!("Failed to query IOMMU info: {e:?}");
            }
        }
    }

    let device = get_device(group, device_path)?;
    let bars = {
        let mut resource_allocator_lock = vm.resource_allocator();
        let resource_allocator = resource_allocator_lock.deref_mut();
        vfio_device_get_bars(&device.file, &device.region_infos, resource_allocator)?
    };
    let (msix_cap_and_register, masks) =
        vfio_device_get_pci_capabilities(&device.file, &device.region_infos, &device.irq_infos)?;

    let mut msix_state = None;
    if let Some((msix_cap, msix_register)) = msix_cap_and_register {
        assert!(
            VFIO_PCI_MSIX_IRQ_INDEX < device.irq_infos.len() as u32,
            "Found MSI-X capability, but VFIO does not have irq_info at VFIO_PCI_MSIX_IRQ_INDEX"
        );
        let msix_irq_info = &device.irq_infos[VFIO_PCI_MSIX_IRQ_INDEX as usize];
        let msix_num = msix_irq_info.count as u16;
        let msix_vectors =
            Vm::create_msix_group(vm.clone(), msix_num).map_err(VfioError::MsixConfig)?;
        let msix_config = crate::pci::msix::MsixConfig::new(Arc::new(msix_vectors), sbdf);
        // We set VFIO irqs here on device setup. There is no reason to add additional tracking
        // for driver MSIx configuration since those are handled by the MsixState.
        // If anything after this call fails, we don't need to do anything since the kernel will
        // clean up these irqs when `device` file will be closed.
        set_msix_irqs(&device.file, &device.irq_infos, &msix_config)?;
        msix_state = Some(MsixState {
            register: msix_register,
            cap: msix_cap,
            bar_hole_infos: ArrayVec::new(),
            config: msix_config,
        });
    }

    let (areas, bar_hole_infos) = calculate_bar_areas(
        &bars,
        &device.region_infos,
        msix_cap_and_register.as_ref().map(|(v, _)| v),
    );
    let first_area_slot = vm
        .next_kvm_slot(areas.len() as u32)
        .ok_or(VfioError::KvmSlot)?;

    let mut bar_mappings = ArrayVec::<BarMapping, 8>::new();
    for (i, area) in areas.iter().enumerate() {
        match map_bar_mapping(
            container,
            &device.file,
            vm.as_ref(),
            area,
            first_area_slot + i as u32,
        ) {
            Ok(mapping) => {
                debug!(
                    "BAR area{} kvm gpa: [{:#x} ..{:#x}]",
                    i,
                    mapping.iova,
                    mapping.iova + mapping.size
                );
                bar_mappings.push(mapping);
            }
            Err(e) => {
                for mapping in bar_mappings.iter() {
                    unmap_bar_mapping(container, vm.as_ref(), mapping);
                }
                return Err(e);
            }
        }
    }

    if first_vfio_device {
        if let Err(e) = dma_map_guest_memory(container, vm.guest_memory()) {
            for mapping in bar_mappings.iter() {
                unmap_bar_mapping(container, vm.as_ref(), mapping);
            }
            return Err(e);
        }
    }

    if let Some(msix_state) = msix_state.as_mut() {
        msix_state.bar_hole_infos = bar_hole_infos;
    }
    return Ok((device, bars, bar_mappings, msix_state, masks));
}

/// Init VFIO device
pub fn init_vfio_device(
    vfio_kvm_and_container: &VfioKvmAndContainer,
    vm: &Arc<Vm>,
    config: VfioConfig,
    sbdf: PciSBDF,
    first_vfio_device: bool,
) -> Result<Arc<Mutex<VfioDeviceBundle>>, VfioError> {
    let container = &vfio_kvm_and_container.container;
    let kvm_device = &vfio_kvm_and_container.kvm_device;

    debug!("Openning device at path: {}", config.path);
    let group_id = group_id_from_device_path(&config.path)?;
    debug!("Group id: {}", group_id);
    let group = vfio_group_open(group_id)?;

    // If anything after this call, we need to call kvm_device_vfio_file_del
    // if this device is the last one in the group
    kvm_device_vfio_file_add(kvm_device, &group)?;

    // If anything after this call, we need to call group_unset_container
    // The return value of group_unset_container can be used to detirmine if the device
    // is the last one in the group
    if let Err(e) = ioctls::group_set_container(&group, container) {
        // Best-effort cleanup: undo the file_add we just did.
        // This is safe because group_set_container failed, so no device
        // from this group was attached via this path.
        kvm_device_vfio_file_del(kvm_device, &group)?;
        return Err(e).map_err(VfioError::from);
    }

    match prepare_vfio_device(container, &group, vm, &config.path, sbdf, first_vfio_device) {
        Ok((device, bars, bar_mappings, msix_state, masks)) => {
            let vfio_device_bundle = Arc::new(Mutex::new(VfioDeviceBundle {
                config,
                sbdf,
                group_id,
                group,
                device,
                bars,
                bar_mappings,
                msix_state,
                masks,
                vm: vm.clone(),
            }));

            if let Some(msix_state) = vfio_device_bundle.lock().unwrap().msix_state.as_ref() {
                // This is for bars (or the poked holes in them where MSIx and PBA tables live)
                for hole in msix_state.bar_hole_infos.iter() {
                    vm.common
                        .mmio_bus
                        .insert(vfio_device_bundle.clone(), hole.gpa, hole.size)
                        .expect("Failed to register VFIO device mmio region");
                }
            }
            Ok(vfio_device_bundle)
        }
        Err(e) => {
            match ioctls::group_unset_container(&group, container) {
                Ok(()) => {
                    // If group_unset_container return successfully, it means this device was the
                    // last one in the group and so we can remove it from the KVM device as well.
                    // In other case group_unset_container will return EBUSY error and we don't
                    // need to do anything with KVM device
                    if let Err(ee) = kvm_device_vfio_file_del(kvm_device, &group) {
                        error!(
                            "Failed to remove VFIO group from KVM device during cleanup: {ee:?}"
                        );
                    }
                }
                Err(VfioIoctlError::GroupUnsetContainer(ee)) => {
                    if ee.errno() == libc::EBUSY {
                        // Other devices still using this group — nothing to clean up
                    } else {
                        error!("Failed to unset VFIO container during cleanup: {ee:?}");
                    }
                }
                _ => unreachable!(),
            }
            Err(e).map_err(VfioError::from)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic]
    fn test_vfio_region_info_with_caps_panic_new() {
        _ = VfioRegionInfoWithCap::new_with_argsz(
            std::mem::size_of::<vfio_region_info>() as u32 - 1,
        );
    }

    #[test]
    fn test_vfio_region_info_with_caps_panic_caps_at_offset() {
        let vfio_region_info_bytes = std::mem::size_of::<vfio_region_info>();
        let mut vriwc = VfioRegionInfoWithCap::new_with_argsz(vfio_region_info_bytes as u32);
        assert!(
            vriwc
                .vfio_info_cap_header_at_offset((vfio_region_info_bytes + 1) as u32)
                .is_none()
        );
    }

    #[test]
    fn test_vfio_region_info_with_caps() {
        let vfio_region_info_bytes = std::mem::size_of::<vfio_region_info>();
        let argsz = vfio_region_info_bytes + std::mem::size_of::<vfio_info_cap_header>();
        let mut vriwc = VfioRegionInfoWithCap::new_with_argsz(argsz as u32);

        let ri = vriwc.vfio_region_info_mut();
        assert!(ri.argsz == 0);
        assert!(ri.flags == 0);
        assert!(ri.index == 0);
        assert!(ri.cap_offset == 0);
        assert!(ri.size == 0);
        assert!(ri.offset == 0);

        let header = vriwc
            .vfio_info_cap_header_at_offset(vfio_region_info_bytes as u32)
            .unwrap();
        assert!(header.id == 0);
        assert!(header.version == 0);
        assert!(header.next == 0);

        assert!(
            vriwc
                .vfio_info_cap_header_at_offset((vfio_region_info_bytes - 1) as u32)
                .is_none()
        );
    }

    /// Helper: create a VfioRegionInfo with given size and flags for mmap.
    fn make_region(size: u64, caps: Vec<VfioRegionCap>) -> VfioRegionInfo {
        let flags = if size != 0 {
            VFIO_REGION_INFO_FLAG_READ | VFIO_REGION_INFO_FLAG_WRITE | VFIO_REGION_INFO_FLAG_MMAP
        } else {
            0
        };
        VfioRegionInfo {
            flags,
            size,
            offset: 0,
            caps,
        }
    }

    /// Helper: create 6 empty region infos, then set specific ones.
    fn make_region_infos(overrides: &[(usize, VfioRegionInfo)]) -> Vec<VfioRegionInfo> {
        let mut infos: Vec<VfioRegionInfo> = (0..6)
            .map(|_| VfioRegionInfo {
                flags: 0,
                size: 0,
                offset: 0,
                caps: Vec::new(),
            })
            .collect();
        for (idx, info) in overrides {
            infos[*idx] = VfioRegionInfo {
                flags: info.flags,
                size: info.size,
                offset: info.offset,
                caps: info.caps.clone(),
            };
        }
        infos
    }

    #[test]
    fn test_calculate_bar_areas_no_msix() {
        // Single BAR0 64-bit, 1MB, no MSI-X
        let mut bars = Bars::default();
        bars.set_bar_64(0, 0x4000_0000_0000, 0x10_0000, false);
        let region_infos = make_region_infos(&[(0, make_region(0x10_0000, vec![]))]);

        let (areas, holes) = calculate_bar_areas(&bars, &region_infos, None);
        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0].bar_gpa, 0x4000_0000_0000);
        assert_eq!(areas[0].size, 0x10_0000);
        assert_eq!(areas[0].offset, 0);
        assert!(holes.is_empty());
    }

    #[test]
    fn test_calculate_bar_areas_msix_table_and_pba_different_bars() {
        // BAR0: 64-bit 1MB, BAR2: 64-bit 64KB
        // MSI-X table in BAR0, PBA in BAR2
        let mut bars = Bars::default();
        bars.set_bar_64(0, 0x4000_0000_0000, 0x10_0000, false);
        bars.set_bar_64(2, 0x4000_0010_0000, 0x1_0000, false);

        let region_infos = make_region_infos(&[
            (0, make_region(0x10_0000, vec![VfioRegionCap::MsixMappable])),
            (2, make_region(0x1_0000, vec![VfioRegionCap::MsixMappable])),
        ]);

        // table_bir=0, table_offset=0, 32 vectors (32*16=512 bytes)
        // pba_bir=2, pba_offset=0
        let msix_cap = MsixCap::new(0, 32, 0, 2, 0);

        let (areas, holes) = calculate_bar_areas(&bars, &region_infos, Some(&msix_cap));
        // BAR0 has table hole at start, BAR2 has PBA hole at start
        assert_eq!(holes.len(), 2);
        // Areas should exist for the non-hole parts
        assert!(!areas.is_empty());
    }

    #[test]
    fn test_calculate_bar_areas_msix_table_and_pba_same_bar() {
        // BAR0: 64-bit 1MB
        // MSI-X table at offset 0, PBA at offset 0x1000, both in BAR0
        let mut bars = Bars::default();
        bars.set_bar_64(0, 0x4000_0000_0000, 0x10_0000, false);

        let region_infos =
            make_region_infos(&[(0, make_region(0x10_0000, vec![VfioRegionCap::MsixMappable]))]);

        // table_bir=0, 32 vectors, table_offset=0
        // pba_bir=0, pba_offset=0x1000
        let msix_cap = MsixCap::new(0, 32, 0, 0, 0x1000);

        let (areas, holes) = calculate_bar_areas(&bars, &region_infos, Some(&msix_cap));
        assert_eq!(holes.len(), 2);
        // Should have areas for the non-hole regions
        assert!(!areas.is_empty());
        // Verify no area overlaps with holes
        for area in areas.iter() {
            let area_start = area.bar_gpa + area.offset;
            let area_end = area_start + area.size;
            for hole in holes.iter() {
                let hole_end = hole.gpa + hole.size;
                // No overlap: area ends before hole starts or area starts after hole ends
                assert!(
                    area_end <= hole.gpa || hole_end <= area_start,
                    "Area [{:#x}..{:#x}] overlaps with hole [{:#x}..{:#x}]",
                    area_start,
                    area_end,
                    hole.gpa,
                    hole_end
                );
            }
        }
    }

    #[test]
    fn test_calculate_bar_areas_sparse_mmap() {
        // BAR0: 64-bit 1MB with sparse mmap capability
        let mut bars = Bars::default();
        bars.set_bar_64(0, 0x4000_0000_0000, 0x10_0000, false);

        let sparse_areas = vec![
            VfioRegionSparseMmapArea {
                offset: 0,
                size: 0x8_0000,
            },
            VfioRegionSparseMmapArea {
                offset: 0xC_0000,
                size: 0x4_0000,
            },
        ];
        let region_infos = make_region_infos(&[(
            0,
            make_region(
                0x10_0000,
                vec![VfioRegionCap::SparseMmap(VfioRegionCapSparseMmap {
                    areas: sparse_areas,
                })],
            ),
        )]);

        let msix_cap = MsixCap::new(0, 32, 0x8_0000, 0, 0xB_0000);

        let (areas, _holes) = calculate_bar_areas(&bars, &region_infos, Some(&msix_cap));
        // Sparse mmap takes precedence — should use the sparse areas directly
        assert_eq!(areas.len(), 2);
        assert_eq!(areas[0].offset, 0);
        assert_eq!(areas[0].size, 0x8_0000);
        assert_eq!(areas[1].offset, 0xC_0000);
        assert_eq!(areas[1].size, 0x4_0000);
    }

    #[test]
    fn test_calculate_bar_areas_zero_size_bar() {
        // All BARs zero — nothing to map
        let bars = Bars::default();
        let region_infos = make_region_infos(&[]);

        let (areas, holes) = calculate_bar_areas(&bars, &region_infos, None);
        assert!(areas.is_empty());
        assert!(holes.is_empty());
    }
}
