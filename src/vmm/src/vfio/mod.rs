#![allow(missing_docs)]
/// bindings
pub mod bindings;
/// ioctls
pub mod ioctls;

// First BAR offset in the PCI config space.
pub const PCI_CONFIG_BAR_OFFSET: u32 = 0x10;
// Capability register offset in the PCI config space.
pub const PCI_CONFIG_CAPABILITY_OFFSET: u32 = 0x34;
// Extended capabilities register offset in the PCI config space.
pub const PCI_CONFIG_EXTENDED_CAPABILITY_OFFSET: u32 = 0x100;
// IO BAR when first BAR bit is 1.
pub const PCI_CONFIG_IO_BAR: u32 = 0x1;
// 64-bit memory bar flag.
pub const PCI_CONFIG_MEMORY_BAR_64BIT: u32 = 0x4;
// Prefetchable BAR bit
pub const PCI_CONFIG_BAR_PREFETCHABLE: u32 = 0x8;
// PCI config register size (4 bytes).
pub const PCI_CONFIG_REGISTER_SIZE: usize = 4;
// Number of BARs for a PCI device
pub const BAR_NUMS: usize = 6;
// PCI Header Type register index
pub const PCI_HEADER_TYPE_REG_INDEX: usize = 3;
// First BAR register index
pub const PCI_CONFIG_BAR0_INDEX: usize = 4;
// PCI ROM expansion BAR register index
pub const PCI_ROM_EXP_BAR_INDEX: usize = 12;

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::{Arc, Barrier};

pub use bindings::*;
pub use ioctls::*;
use kvm_bindings::kvm_userspace_memory_region;
use pci::{PciCapabilityId, PciExpressCapabilityId};
use vm_allocator::AllocPolicy;
use vm_memory::{GuestMemory, GuestMemoryRegion};
use zerocopy::IntoBytes;

use crate::Vm;
use crate::pci::msix::MsixConfig;
use crate::pci::{BarReprogrammingParams, DeviceRelocationError, PciDevice};
use crate::vstate::bus::BusDevice;
use crate::vstate::memory::{GuestMemoryMmap, GuestRegionType};
use crate::vstate::resources::ResourceAllocator;

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
    pub index: u32,
    pub size: u64,
    pub offset: u64,
    pub caps: Vec<VfioRegionCap>,
}

/// 7.7.1.2 Message Control Register for MSI
#[derive(Debug)]
pub struct MsiCap {
    pub msg_ctl: u16,
}

/// 7.7.2 MSI-X Capability and Table Structure
#[derive(Debug)]
pub struct MsixCap {
    pub msg_ctl: u16,
    pub table_offset: u32,
    pub pba_offset: u32,
}
impl MsixCap {
    const FUNCTION_MASK_BIT: u8 = 14;
    const MSIX_ENABLE_BIT: u8 = 15;

    pub fn masked(&self) -> bool {
        (self.msg_ctl >> Self::FUNCTION_MASK_BIT) & 0x1 == 0x1
    }

    pub fn enabled(&self) -> bool {
        (self.msg_ctl >> Self::MSIX_ENABLE_BIT) & 0x1 == 0x1
    }

    pub fn table_offset(&self) -> u32 {
        self.table_offset & 0xffff_fff8
    }

    pub fn pba_offset(&self) -> u32 {
        self.pba_offset & 0xffff_fff8
    }

    pub fn table_set_offset(&mut self, addr: u32) {
        self.table_offset &= 0x7;
        self.table_offset += addr;
    }

    pub fn pba_set_offset(&mut self, addr: u32) {
        self.pba_offset &= 0x7;
        self.pba_offset += addr;
    }

    pub fn table_bir(&self) -> u32 {
        self.table_offset & 0x7
    }

    pub fn pba_bir(&self) -> u32 {
        self.pba_offset & 0x7
    }

    pub fn table_size(&self) -> u16 {
        (self.msg_ctl & 0x7ff) + 1
    }

    pub fn table_range(&self) -> (u64, u64) {
        // The table takes 16 bytes per entry.
        let size = self.table_size() as u64 * 16;
        (self.table_offset() as u64, size)
    }

    pub fn pba_range(&self) -> (u64, u64) {
        // The table takes 1 bit per entry modulo 8 bytes.
        let size = ((self.table_size() as u64 / 64) + 1) * 8;
        (self.pba_offset() as u64, size)
    }
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
    pub idx: u32,
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
    pub offset_in_hole: u64,
    pub usage: BarHoleInfoUsage,
}

#[derive(Debug)]
pub struct VfioDevice {
    pub file: File,
    pub info: vfio_device_info,
    pub region_infos: Vec<VfioRegionInfo>,
    pub irq_infos: Vec<vfio_irq_info>,
}

pub struct VfioDeviceBundle {
    pub id: String,
    pub group_id: u32,
    pub group: File,
    pub device: VfioDevice,
    pub bar_infos: Vec<BarInfo>,
    pub expansion_rom_info: Option<ExpansionRomInfo>,

    pub msi_cap: Option<MsiCap>,

    // these 2 must exist togather
    pub msix_cap: Option<MsixCap>,
    pub bar_hole_infos: Vec<BarHoleInfo>,

    pub masks: Option<Vec<RegisterMask>>,

    pub msix_config: Option<MsixConfig>,
}

macro_rules! function_name {
    () => {{
        fn f() {}
        let name = std::any::type_name_of_val(&f);
        // Strip "::f" suffix
        &name[..name.len() - 3]
    }};
}
macro_rules! LOG {
    ($($arg:tt)*) => {
        println!("[{}:{:<4}:{:<80}] {}", file!(), line!(), function_name!(), format_args!($($arg)*))
    };
}
// This should only serve BARs
impl BusDevice for VfioDeviceBundle {
    fn read(&mut self, base: u64, offset: u64, data: &mut [u8]) {
        let mut table_name = "----";
        if let Some(msix_config) = self.msix_config.as_ref() {
            for info in self.bar_hole_infos.iter() {
                if info.gpa == base {
                    if info.offset_in_hole <= offset && offset < info.offset_in_hole + info.size {
                        match info.usage {
                            BarHoleInfoUsage::Table => {
                                table_name = "MsiTable";
                                msix_config.read_table(offset, data);
                            }
                            BarHoleInfoUsage::Pba => {
                                table_name = "PbaTable";
                                msix_config.read_pba(offset, data);
                            }
                        }
                    } else {
                        let msix_cap = self.msix_cap.as_ref().unwrap();
                        let region_index = match info.usage {
                            BarHoleInfoUsage::Table => msix_cap.table_bir(),
                            BarHoleInfoUsage::Pba => msix_cap.pba_bir(),
                        };
                        let region_info = &self.device.region_infos[region_index as usize];
                        vfio_device_region_read(
                            &self.device.file,
                            &self.device.region_infos,
                            region_index,
                            region_info.offset + offset,
                            data,
                        );
                    }
                }
            }
            LOG!(
                "base: {base:<#10x} offset: {offset:<#5x} data: {data:<4?} table_name: \
                 {table_name}"
            );
        } else {
            panic!("Should never happen");
        }
    }

    fn write(&mut self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        let mut table_name = "----";
        if let Some(msix_config) = self.msix_config.as_mut() {
            for info in self.bar_hole_infos.iter() {
                if info.gpa == base {
                    if info.offset_in_hole <= offset && offset < info.offset_in_hole + info.size {
                        match info.usage {
                            BarHoleInfoUsage::Table => {
                                table_name = "MsiTable";
                                msix_config.write_table(offset, data);
                            }
                            BarHoleInfoUsage::Pba => {
                                table_name = "PbaTable";
                                msix_config.write_pba(offset, data);
                            }
                        }
                    } else {
                        let msix_cap = self.msix_cap.as_ref().unwrap();
                        let region_index = match info.usage {
                            BarHoleInfoUsage::Table => msix_cap.table_bir(),
                            BarHoleInfoUsage::Pba => msix_cap.pba_bir(),
                        };
                        let region_info = &self.device.region_infos[region_index as usize];
                        vfio_device_region_write(
                            &self.device.file,
                            &self.device.region_infos,
                            region_index,
                            region_info.offset + offset,
                            data,
                        );
                    }
                }
            }
            LOG!(
                "base: {base:<#10x} offset: {offset:<#5x} data: {data:<4?} table_name: \
                 {table_name}"
            );
        } else {
            panic!("Should never happen");
        }
        None
    }
}

// This should only serve config space
impl PciDevice for VfioDeviceBundle {
    fn write_config_register(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> Option<Arc<Barrier>> {
        let config_offset = reg_idx as u64 * 4 + offset;
        if 4 <= reg_idx && reg_idx < 10 {
            let bar_idx = (reg_idx - 4) as u32;

            let mut looks_like_request_to_read: bool = false;
            if data.len() == 4 {
                let d: u32 = u32::from_le_bytes(data.try_into().unwrap());
                if d == 0xFFFF_FFFF {
                    looks_like_request_to_read = true;
                }
            }

            for bar_info in self.bar_infos.iter_mut() {
                if bar_idx == bar_info.idx {
                    if looks_like_request_to_read {
                        bar_info.about_to_read_size = true;
                    }
                } else if bar_idx == bar_info.idx + 1 && bar_info.is_64_bits {
                    if looks_like_request_to_read {
                        bar_info.about_to_read_size = true;
                    }
                }
            }
        } else if reg_idx == 12 {
            if let Some(rom_info) = self.expansion_rom_info.as_mut() {
                // Expansino Rom
                let mut looks_like_request_to_read: bool = false;
                if data.len() == 4 {
                    let d: u32 = u32::from_le_bytes(data.try_into().unwrap());
                    if d == 0xFFFF_FFFF {
                        looks_like_request_to_read = true;
                    }
                }
                if looks_like_request_to_read {
                    rom_info.about_to_read_size = true;
                }
            }
        } else {
            vfio_device_region_write(
                &self.device.file,
                &self.device.region_infos,
                VFIO_PCI_CONFIG_REGION_INDEX,
                config_offset,
                data,
            );
        }
        LOG!("reg: {reg_idx:>3}({config_offset:>#6x}) data: {data:<4?}");
        None
    }
    fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        let config_offset = reg_idx as u64 * 4;
        let mut result: u32 = 0;
        let mut applied_mask: bool = false;
        if 4 <= reg_idx && reg_idx < 10 {
            let bar_idx = (reg_idx - 4) as u32;
            for bar_info in self.bar_infos.iter() {
                if bar_idx == bar_info.idx {
                    if bar_info.about_to_read_size {
                        let size = !(bar_info.size - 1);
                        result = (size & 0xFFFF_FFFF) as u32;
                    } else {
                        let is_64_bits = if bar_info.is_64_bits { 0b10 << 1 } else { 0 };
                        let is_prefetchable = if bar_info.is_prefetchable { 0b1000 } else { 0 };
                        result = (bar_info.gpa & 0xFFFF_FFFF) as u32 | is_64_bits | is_prefetchable;
                    }
                } else if bar_info.is_64_bits && bar_idx == bar_info.idx + 1 {
                    if bar_info.about_to_read_size {
                        let size = !(bar_info.size - 1);
                        result = (size >> 32) as u32;
                    } else {
                        result = (bar_info.gpa >> 32) as u32;
                    }
                }
            }
        } else if reg_idx == 12 {
            if let Some(rom_info) = self.expansion_rom_info.as_mut() {
                if rom_info.about_to_read_size {
                    result = rom_info.size;
                } else {
                    result = (rom_info.gpa << 11) as u32 | rom_info.extra as u32;
                }
            }
        } else {
            if let Some(masks) = self.masks.as_ref() {
                for mask in masks.iter() {
                    if mask.register == reg_idx as u16 {
                        vfio_device_region_read(
                            &self.device.file,
                            &self.device.region_infos,
                            VFIO_PCI_CONFIG_REGION_INDEX,
                            config_offset,
                            result.as_mut_bytes(),
                        );
                        applied_mask = true;
                        result = (result & mask.mask) | mask.value;
                        break;
                    }
                }
            }
            if !applied_mask {
                vfio_device_region_read(
                    &self.device.file,
                    &self.device.region_infos,
                    VFIO_PCI_CONFIG_REGION_INDEX,
                    config_offset,
                    result.as_mut_bytes(),
                );
            }
        }
        LOG!(
            "reg: {reg_idx:>3}({config_offset:>#6x}) data: {:<4?} applied mask: {applied_mask}",
            result.as_bytes()
        );
        result
    }
    fn detect_bar_reprogramming(
        &mut self,
        _reg_idx: usize,
        _data: &[u8],
    ) -> Option<BarReprogrammingParams> {
        None
    }
    fn read_bar(&mut self, _base: u64, _offset: u64, _data: &mut [u8]) {
        LOG!("base: {_base:#x} offset: {_offset:#x} data: {_data:?}");
    }
    fn write_bar(&mut self, _base: u64, _offset: u64, _data: &[u8]) -> Option<Arc<Barrier>> {
        LOG!("base: {_base:#x} offset: {_offset:#x} data: {_data:?}");
        None
    }
    fn move_bar(&mut self, _old_base: u64, _new_base: u64) -> Result<(), DeviceRelocationError> {
        Ok(())
    }
}

pub fn vfio_open() -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/vfio/vfio")
        .unwrap()
}
pub fn vfio_check_api_version(container: &impl AsRawFd) {
    let version = crate::vfio::ioctls::ioctls::check_api_version(container);
    LOG!("vfio api version: {}", version);
    if version as u32 != VFIO_API_VERSION {
        panic!("Vfio api version");
    }
}
pub fn vfio_check_extension(container: &impl AsRawFd, val: u32) {
    if val != VFIO_TYPE1_IOMMU && val != VFIO_TYPE1v2_IOMMU {
        panic!();
    }
    let ret = crate::vfio::check_extension(container, val).unwrap();
    if ret != 1 {
        panic!();
    }
}
pub fn group_id_from_device_path(device_path: &impl AsRef<Path>) -> u32 {
    let uuid_path: std::path::PathBuf = device_path.as_ref().join("iommu_group");
    let group_path = uuid_path.read_link().unwrap();
    let group_osstr = group_path.file_name().unwrap();
    let group_str = group_osstr.to_str().unwrap();
    group_str.parse::<u32>().unwrap()
}
pub fn vfio_group_open(id: u32) -> File {
    let group_path = Path::new("/dev/vfio").join(id.to_string());
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(group_path)
        .unwrap()
}
pub fn vfio_group_check_status(group: &impl AsRawFd) {
    let mut group_status = vfio_group_status {
        argsz: std::mem::size_of::<vfio_group_status>() as u32,
        flags: 0,
    };
    crate::vfio::group_get_status(group, &mut group_status).unwrap();
    if group_status.flags != VFIO_GROUP_FLAGS_VIABLE {
        panic!();
    }
}
pub fn vfio_container_set_iommu(container: &impl AsRawFd, val: u32) {
    assert!(val == VFIO_TYPE1_IOMMU || val == VFIO_TYPE1v2_IOMMU);
    crate::vfio::ioctls::set_iommu(container, val).unwrap();
}

pub fn vfio_group_get_device(group: &impl AsRawFd, path: &impl AsRef<Path>) -> File {
    let uuid_osstr = path.as_ref().file_name().unwrap();
    let uuid_str = uuid_osstr.to_str().unwrap();
    let path = CString::new(uuid_str.as_bytes()).unwrap();
    let device = crate::vfio::group_get_device_fd(group, &path).unwrap();
    device
}
pub fn vfio_device_get_info(device: &impl AsRawFd) -> vfio_device_info {
    let mut dev_info = vfio_device_info {
        argsz: std::mem::size_of::<vfio_device_info>() as u32,
        flags: 0,
        num_regions: 0,
        num_irqs: 0,
        cap_offset: 0,
        pad: 0,
    };
    crate::vfio::ioctls::device_get_info(device, &mut dev_info).unwrap();
    dev_info
}
pub fn vfio_device_reset(device: &impl AsRawFd, device_info: &vfio_device_info) {
    if device_info.flags & VFIO_DEVICE_FLAGS_RESET != 0 {
        crate::vfio::device_reset(device);
    }
}
pub fn vfio_device_get_region_infos(
    device: &impl AsRawFd,
    device_info: &vfio_device_info,
) -> Vec<VfioRegionInfo> {
    let num_regions = device_info.num_regions - VFIO_PCI_BAR0_REGION_INDEX;
    let mut regions = Vec::with_capacity(num_regions as usize);

    for i in VFIO_PCI_BAR0_REGION_INDEX..device_info.num_regions {
        LOG!("Getting bar region info: {}", i);
        let region_info_struct_size = std::mem::size_of::<vfio_region_info>() as u32;
        let mut region_info = vfio_region_info {
            argsz: region_info_struct_size,
            flags: 0,
            index: i,
            cap_offset: 0,
            size: 0,
            offset: 0,
        };
        if crate::vfio::device_get_region_info(device, &mut region_info).is_err() {
            LOG!("Canno get regino {i} info. Skipping");
            continue;
        }
        LOG!("Flags: ");
        LOG!(
            "VFIO_REGION_INFO_FLAG_READ: {}",
            region_info.flags & VFIO_REGION_INFO_FLAG_READ != 0
        );
        LOG!(
            "VFIO_REGION_INFO_FLAG_WRITE: {}",
            region_info.flags & VFIO_REGION_INFO_FLAG_WRITE != 0
        );
        LOG!(
            "VFIO_REGION_INFO_FLAG_MMAP: {}",
            region_info.flags & VFIO_REGION_INFO_FLAG_MMAP != 0
        );
        LOG!(
            "VFIO_REGION_INFO_FLAG_CAPS: {}",
            region_info.flags & VFIO_REGION_INFO_FLAG_CAPS != 0
        );
        let mut caps = Vec::new();
        if region_info.flags & VFIO_REGION_INFO_FLAG_CAPS == 0
            || region_info.argsz <= region_info_struct_size
        {
            LOG!("Region has no caps");
        } else {
            LOG!("Region caps:");
            let mut region_info_with_cap_bytes = vec![0_u8; region_info.argsz as usize];
            let region_info_with_caps =
                unsafe { &mut *(region_info_with_cap_bytes.as_mut_ptr() as *mut vfio_region_info) };
            region_info_with_caps.argsz = region_info.argsz;
            region_info_with_caps.flags = 0;
            region_info_with_caps.index = region_info.index;
            region_info_with_caps.cap_offset = 0;
            region_info_with_caps.size = 0;
            region_info_with_caps.offset = 0;
            crate::vfio::device_get_region_info(device, region_info_with_caps).unwrap();
            LOG!("Region info with caps: {:?}", region_info_with_caps);

            let mut next_cap_offset = region_info_with_caps.cap_offset;
            while region_info_struct_size <= next_cap_offset {
                let cap_header = unsafe {
                    &*(region_info_with_cap_bytes[next_cap_offset as usize..].as_ptr()
                        as *const vfio_info_cap_header)
                };
                LOG!("Cap id: {}", cap_header.id);
                match u32::from(cap_header.id) {
                    VFIO_REGION_INFO_CAP_SPARSE_MMAP => {
                        let cap_sparse_mmap = unsafe {
                            &*(cap_header as *const vfio_info_cap_header
                                as *const vfio_region_info_cap_sparse_mmap)
                        };
                        let areas = cap_sparse_mmap
                            .areas
                            .as_slice(cap_sparse_mmap.nr_areas as usize);
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
                        LOG!("Got unknown region capability id: {}", cap_header.id);
                    }
                }
                next_cap_offset = cap_header.next;
            }
        }
        let region_info = VfioRegionInfo {
            flags: region_info.flags,
            index: region_info.index,
            size: region_info.size,
            offset: region_info.offset,
            caps,
        };
        LOG!("Region {i} info: {region_info:?}");
        regions.push(region_info);
    }
    regions
}
pub fn vfio_device_get_irq_infos(
    device: &impl AsRawFd,
    device_info: &vfio_device_info,
) -> Vec<vfio_irq_info> {
    let mut irqs = Vec::with_capacity(device_info.num_irqs as usize);
    for i in 0..device_info.num_irqs {
        LOG!("Getting irq info: {}", i);
        let mut irq_info = vfio_irq_info {
            argsz: std::mem::size_of::<vfio_irq_info>() as u32,
            flags: 0,
            index: i,
            count: 0,
        };
        match crate::vfio::device_get_irq_info(device, &mut irq_info) {
            Ok(()) => {
                LOG!("Irq info: {:?}", irq_info);
                LOG!(
                    "VFIO_IRQ_INFO_EVENTFD: {}",
                    irq_info.flags & VFIO_IRQ_INFO_EVENTFD != 0
                );
                LOG!(
                    "VFIO_IRQ_INFO_MASKABLE  :{}",
                    irq_info.flags & VFIO_IRQ_INFO_MASKABLE != 0
                );
                LOG!(
                    "VFIO_IRQ_INFO_AUTOMASKED  :{}",
                    irq_info.flags & VFIO_IRQ_INFO_AUTOMASKED != 0
                );
                LOG!(
                    "VFIO_IRQ_INFO_NORESIZE  :{}",
                    irq_info.flags & VFIO_IRQ_INFO_NORESIZE != 0
                );
                irqs.push(irq_info);
            }
            Err(e) => LOG!("Irq info: got error: {:?}", e),
        }
    }
    irqs
}
pub fn vfio_device_get_pci_capabilities(
    device: &impl FileExt,
    region_infos: &[VfioRegionInfo],
    irq_infos: &[vfio_irq_info],
) -> (Option<MsiCap>, Option<MsixCap>, Option<Vec<RegisterMask>>) {
    let mut next_cap_offset: u8 = 0;
    vfio_device_region_read(
        device,
        region_infos,
        VFIO_PCI_CONFIG_REGION_INDEX,
        PCI_CONFIG_CAPABILITY_OFFSET as u64,
        next_cap_offset.as_mut_bytes(),
    );

    let mut has_pci_express_cap = false;
    let mut has_power_management_cap = false;

    let mut msi_cap = None;
    let mut msix_cap = None;
    LOG!("PCI CAPS offset: {}", next_cap_offset);
    // let mut caps = Vec::new();
    while next_cap_offset != 0 {
        let mut cap_id_and_next_ptr: u16 = 0;
        vfio_device_region_read(
            device,
            region_infos,
            VFIO_PCI_CONFIG_REGION_INDEX,
            next_cap_offset as u64,
            cap_id_and_next_ptr.as_mut_bytes(),
        );

        let cap_id: u8 = (cap_id_and_next_ptr & 0xff) as u8;
        let current_cap_offset = next_cap_offset;
        next_cap_offset = ((cap_id_and_next_ptr & 0xff00) >> 8) as u8;
        LOG!("PCI CAP id: {cap_id} next offset: {next_cap_offset:#x}");

        match PciCapabilityId::from(cap_id) {
            PciCapabilityId::MessageSignalledInterrupts => {
                if (VFIO_PCI_MSI_IRQ_INDEX as usize) < irq_infos.len() {
                    let irq_info = irq_infos[VFIO_PCI_MSI_IRQ_INDEX as usize];
                    if 0 < irq_info.count {
                        LOG!("Found MSI cap");
                        let mut msg_ctl: u16 = 0;
                        vfio_device_region_read(
                            device,
                            region_infos,
                            VFIO_PCI_CONFIG_REGION_INDEX,
                            (current_cap_offset as u64) + 2,
                            msg_ctl.as_mut_bytes(),
                        );
                        msi_cap = Some(MsiCap { msg_ctl });
                    }
                }
            }
            PciCapabilityId::MsiX => {
                if (VFIO_PCI_MSIX_IRQ_INDEX as usize) < irq_infos.len() {
                    let irq_info = irq_infos[VFIO_PCI_MSIX_IRQ_INDEX as usize];
                    if 0 < irq_info.count {
                        LOG!("Found MSIX cap");

                        // 7.7.2 MSI-X Capability and Table Structure
                        let mut msg_ctl: u16 = 0;
                        let mut table_offset: u32 = 0;
                        let mut pba_offset: u32 = 0;
                        vfio_device_region_read(
                            device,
                            region_infos,
                            VFIO_PCI_CONFIG_REGION_INDEX,
                            (current_cap_offset as u64) + 2,
                            msg_ctl.as_mut_bytes(),
                        );
                        vfio_device_region_read(
                            device,
                            region_infos,
                            VFIO_PCI_CONFIG_REGION_INDEX,
                            (current_cap_offset as u64) + 4,
                            table_offset.as_mut_bytes(),
                        );
                        vfio_device_region_read(
                            device,
                            region_infos,
                            VFIO_PCI_CONFIG_REGION_INDEX,
                            (current_cap_offset as u64) + 8,
                            pba_offset.as_mut_bytes(),
                        );
                        msix_cap = Some(MsixCap {
                            msg_ctl,
                            table_offset,
                            pba_offset,
                        });
                    }
                }
            }
            PciCapabilityId::PciExpress => has_pci_express_cap = true,
            PciCapabilityId::PowerManagement => has_power_management_cap = true,
            _ => {}
        };
    }

    // if let Some(clique_id) = self.x_nv_gpudirect_clique {
    //     self.add_nv_gpudirect_clique_cap(cap_iter, clique_id);
    // }
    //
    let mut masks = None;
    if has_pci_express_cap && has_power_management_cap {
        let mut tmp_masks = Vec::new();
        LOG!("Parsing extended caps");
        let mut next_cap_offset: u16 = PCI_CONFIG_EXTENDED_CAPABILITY_OFFSET as u16;
        while next_cap_offset != 0 {
            let mut cap_id_and_next_ptr: u32 = 0;
            vfio_device_region_read(
                device,
                region_infos,
                VFIO_PCI_CONFIG_REGION_INDEX,
                next_cap_offset as u64,
                cap_id_and_next_ptr.as_mut_bytes(),
            );
            let cap_id: u16 = (cap_id_and_next_ptr & 0xffff) as u16;
            let current_cap_offset = next_cap_offset;
            next_cap_offset = (cap_id_and_next_ptr >> 20) as u16;

            let pci_cap = PciExpressCapabilityId::from(cap_id);
            LOG!("Found extended cap: {pci_cap:#?}");
            if pci_cap == PciExpressCapabilityId::AlternativeRoutingIdentificationInterpretation
                || pci_cap == PciExpressCapabilityId::ResizeableBar
                || pci_cap == PciExpressCapabilityId::SingleRootIoVirtualization
            {
                let register = current_cap_offset / 4;
                LOG!("Found cap to be masked at register: {register}({current_cap_offset:#x})");
                tmp_masks.push(RegisterMask {
                    register,
                    mask: 0xffff_0000,
                    value: 0x0000_0000,
                })
            }
        }
        masks = Some(tmp_masks);
    }
    (msi_cap, msix_cap, masks)
}

pub fn vfio_device_region_read(
    device: &impl FileExt,
    region_infos: &[VfioRegionInfo],
    index: u32,
    offset: u64,
    buf: &mut [u8],
) {
    let region_info = &region_infos[index as usize];
    let buf_size = buf.len() as u64;
    if offset + buf_size <= region_info.size {
        if let Err(e) = device.read_exact_at(buf, region_info.offset + offset) {
            panic!(
                "Failed to read from region at index: {index} offset: {offset:#x} region size: \
                 {:#x} error: {e}",
                region_info.size
            );
        }
    } else {
        panic!(
            "Failed to read from region at index: {index} offset: {offset:#x} region size: {:#x} \
             error: read beyond region memory",
            region_info.size
        );
    }
    // LOG!("region: {index:>2} offset: {offset:#x}: data: {buf:?}");
}

pub fn vfio_device_region_write(
    device: &impl FileExt,
    region_infos: &[VfioRegionInfo],
    index: u32,
    offset: u64,
    buf: &[u8],
) {
    let region_info = &region_infos[index as usize];
    let buf_size = buf.len() as u64;
    if offset + buf_size <= region_info.size {
        if let Err(e) = device.write_all_at(buf, region_info.offset + offset) {
            panic!(
                "Failed to write to region at index: {index} offset: {offset:#x} region size: \
                 {:#x} error: {e}",
                region_info.size
            );
        }
    } else {
        panic!(
            "Failed to write to region at index: {index} offset: {offset:#x} region size: {:#x} \
             error: write beyond region memory",
            region_info.size
        );
    }
    // LOG!("region: {index:>2} offset: {offset:#x}: data: {buf:?}");
}
pub fn device_get_bar_infos(
    device: &impl FileExt,
    region_infos: &[VfioRegionInfo],
    resource_allocator: &mut ResourceAllocator,
) -> Vec<BarInfo> {
    let mut bar_infos = Vec::new();
    let mut bar_idx = VFIO_PCI_BAR0_REGION_INDEX;
    while bar_idx <= VFIO_PCI_BAR5_REGION_INDEX {
        let bar_offset = if bar_idx == VFIO_PCI_ROM_REGION_INDEX {
            (PCI_ROM_EXP_BAR_INDEX * 4) as u32
        } else {
            PCI_CONFIG_BAR_OFFSET + bar_idx * 4
        };

        let mut bar_info: u32 = 0;
        vfio_device_region_read(
            device,
            region_infos,
            VFIO_PCI_CONFIG_REGION_INDEX,
            bar_offset as u64,
            bar_info.as_mut_bytes(),
        );

        // Is this an IO BAR?
        let mut is_io_bar = bar_info & PCI_CONFIG_IO_BAR != 0;
        if bar_idx == VFIO_PCI_ROM_REGION_INDEX {
            is_io_bar = false;
        }

        let mut is_64_bits = bar_info & PCI_CONFIG_MEMORY_BAR_64BIT != 0;
        if bar_idx == VFIO_PCI_ROM_REGION_INDEX {
            is_64_bits = false;
        }
        let is_prefetchable = bar_info & PCI_CONFIG_BAR_PREFETCHABLE != 0;

        vfio_device_region_write(
            device,
            region_infos,
            VFIO_PCI_CONFIG_REGION_INDEX,
            bar_offset as u64,
            0xffff_ffff_u32.as_bytes(),
        );
        let mut lower_size: u32 = 0;
        vfio_device_region_read(
            device,
            region_infos,
            VFIO_PCI_CONFIG_REGION_INDEX,
            bar_offset as u64,
            lower_size.as_mut_bytes(),
        );

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
            vfio_device_region_write(
                device,
                region_infos,
                VFIO_PCI_CONFIG_REGION_INDEX,
                (bar_offset as u64) + 4,
                0xffff_ffff_u32.as_bytes(),
            );
            let mut upper_size: u32 = 0;
            vfio_device_region_read(
                device,
                region_infos,
                VFIO_PCI_CONFIG_REGION_INDEX,
                (bar_offset as u64) + 4,
                upper_size.as_mut_bytes(),
            );

            size = u64::from(upper_size) << 32 | u64::from(lower_size);
            size &= !0b1111;
            size = !size + 1;
        }
        if size != 0 {
            let idx = bar_idx;
            let mut gpa = 0;
            if is_io_bar {
                LOG!(
                    "BAR{bar_idx} size: {size:>#10x} io_bar: {is_io_bar} 64bits: {is_64_bits} \
                     prefetchable: {is_prefetchable} Skipping"
                );
                // TODO
                bar_idx += 1;
                continue;
            } else if is_64_bits {
                // allocate 64bit guest address
                gpa = resource_allocator
                    .mmio64_memory
                    .allocate(size, 64, AllocPolicy::FirstMatch)
                    .unwrap()
                    .start();
                bar_idx += 1;
            } else {
                // allocate 32bit guest address
                gpa = resource_allocator
                    .mmio32_memory
                    .allocate(size, 64, AllocPolicy::FirstMatch)
                    .unwrap()
                    .start();
            }
            LOG!(
                "BAR{bar_idx} gpa: [{:#x}..{:#x}] size: {size:>#10x} io_bar: {is_io_bar} 64bits: \
                 {is_64_bits} prefetchable: {is_prefetchable}",
                gpa,
                gpa + size
            );
            bar_infos.push(BarInfo {
                idx,
                gpa,
                size,
                is_64_bits,
                is_prefetchable,
                // just for testing
                about_to_read_size: false,
            });
        } else {
            LOG!(
                "BAR{bar_idx} size: {size:>#10x} io_bar: {is_io_bar} 64bits: {is_64_bits} \
                 prefetchable: {is_prefetchable}"
            );
        }
        bar_idx += 1;
    }
    bar_infos
}

pub struct ExpansionRomInfo {
    gpa: u64,
    size: u32,
    // Validation status and Validation Details
    extra: u16,

    // just for testing
    pub about_to_read_size: bool,
}
pub fn device_get_expansion_rom_info(
    device: &impl FileExt,
    region_infos: &[VfioRegionInfo],
    resource_allocator: &mut ResourceAllocator,
) -> Option<ExpansionRomInfo> {
    let mut rom_raw: u32 = 0;
    vfio_device_region_read(
        device,
        region_infos,
        VFIO_PCI_CONFIG_REGION_INDEX,
        0x30,
        rom_raw.as_mut_bytes(),
    );
    let mut result = None;
    if rom_raw & 0x1 != 0 {
        vfio_device_region_write(
            device,
            region_infos,
            VFIO_PCI_CONFIG_REGION_INDEX,
            0x30,
            0xffff_ffff_u32.as_bytes(),
        );
        let mut rom_size: u32 = 0;
        vfio_device_region_read(
            device,
            region_infos,
            VFIO_PCI_CONFIG_REGION_INDEX,
            0x30,
            rom_size.as_mut_bytes(),
        );
        let size = (rom_size & !((1 << 12) - 1)) as u32;
        let gpa = resource_allocator
            .mmio32_memory
            .allocate(size as u64, 64, AllocPolicy::FirstMatch)
            .unwrap()
            .start();
        LOG!(
            "Expansion ROM gpa: [{:#x}..{:#x}] size: {size:>#10x}",
            gpa,
            gpa + size as u64
        );
        result = Some(ExpansionRomInfo {
            gpa,
            size,
            extra: (rom_raw & ((1 << 12) - 1)) as u16,
            about_to_read_size: false,
        });
    }
    return result;
}

pub struct ConfigSpaceInfo {
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u32,
    pub revision_id: u8,
}
pub fn device_get_config_space_info(
    device: &impl FileExt,
    region_infos: &[VfioRegionInfo],
) -> ConfigSpaceInfo {
    let mut device_id_vendor_id: u32 = 0;
    vfio_device_region_read(
        device,
        region_infos,
        VFIO_PCI_CONFIG_REGION_INDEX,
        0x0,
        device_id_vendor_id.as_mut_bytes(),
    );
    let vendor_id = (device_id_vendor_id & 0xFF) as u16;
    let device_id = (device_id_vendor_id >> 16) as u16;
    LOG!("Vendor id: {vendor_id:#x} Device id: {device_id:#x}");

    let mut class_code_and_revision_id: u32 = 0;
    vfio_device_region_read(
        device,
        region_infos,
        VFIO_PCI_CONFIG_REGION_INDEX,
        0x8,
        class_code_and_revision_id.as_mut_bytes(),
    );
    let revision_id = (class_code_and_revision_id & 0xF) as u8;
    let class_code = (class_code_and_revision_id >> 8) as u32;
    LOG!("Revision id: {revision_id:#x} Class code: {class_code:#x}");
    let result = ConfigSpaceInfo {
        vendor_id,
        device_id,
        class_code,
        revision_id,
    };
    result
}

pub fn get_device(group: &impl AsRawFd, path: &str) -> VfioDevice {
    let device_file = vfio_group_get_device(group, &path);
    let device_info = vfio_device_get_info(&device_file);
    LOG!("Device info: {device_info:#?}");
    vfio_device_reset(&device_file, &device_info);

    let device_region_infos = vfio_device_get_region_infos(&device_file, &device_info);

    LOG!("Getting PCI caps");
    let mut pci_cap_offset: u8 = 0;
    vfio_device_region_read(
        &device_file,
        &device_region_infos,
        VFIO_PCI_CONFIG_REGION_INDEX,
        PCI_CONFIG_CAPABILITY_OFFSET as u64,
        pci_cap_offset.as_mut_bytes(),
    );
    LOG!("PCI cap offset: {}", pci_cap_offset);
    while pci_cap_offset != 0 {
        let mut pci_cap_id = 0;
        vfio_device_region_read(
            &device_file,
            &device_region_infos,
            VFIO_PCI_CONFIG_REGION_INDEX,
            pci_cap_offset as u64,
            pci_cap_id.as_mut_bytes(),
        );
        let pci_cap = PciCapabilityId::from(pci_cap_id);
        LOG!("Pci cap found: {:?}", pci_cap);
        vfio_device_region_read(
            &device_file,
            &device_region_infos,
            VFIO_PCI_CONFIG_REGION_INDEX,
            (pci_cap_offset + 1) as u64,
            pci_cap_offset.as_mut_bytes(),
        );
    }

    let device_irq_infos = vfio_device_get_irq_infos(&device_file, &device_info);
    if VFIO_PCI_INTX_IRQ_INDEX < device_irq_infos.len() as u32 {
        LOG!(
            "INTX IRQ info: {:?}",
            device_irq_infos[VFIO_PCI_INTX_IRQ_INDEX as usize]
        );
    }
    if VFIO_PCI_MSI_IRQ_INDEX < device_irq_infos.len() as u32 {
        LOG!(
            "MSI IRQ info: {:?}",
            device_irq_infos[VFIO_PCI_MSI_IRQ_INDEX as usize]
        );
    }
    if VFIO_PCI_MSIX_IRQ_INDEX < device_irq_infos.len() as u32 {
        LOG!(
            "MSIX IRQ info: {:?}",
            device_irq_infos[VFIO_PCI_MSIX_IRQ_INDEX as usize]
        );
    }

    VfioDevice {
        file: device_file,
        info: device_info,
        region_infos: device_region_infos,
        irq_infos: device_irq_infos,
    }
}

pub fn mmap_bars(
    container: &File,
    device: &File,
    bar_infos: &[BarInfo],
    region_infos: &[VfioRegionInfo],
    msix_cap: Option<&MsixCap>,
    vm: &Vm,
) -> Vec<BarHoleInfo> {
    let mut infos = Vec::new();
    for bar_info in bar_infos.iter() {
        let region_info = &region_infos[bar_info.idx as usize];
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

        fn align_page_size_down(v: u64) -> u64 {
            v & !(4096 - 1)
        }
        fn align_page_size_up(v: u64) -> u64 {
            align_page_size_down(v + 4096)
        }
        if let Some(msix_cap) = msix_cap {
            contain_msix_table = region_info.index == msix_cap.pba_bir();
            if contain_msix_table {
                let (offset, size) = msix_cap.table_range();
                msix_table_offset = align_page_size_down(offset);
                msix_table_size = align_page_size_up(size);
                let offset_in_hole = offset - msix_table_offset;

                LOG!(
                    "BAR{} msix_table hole: [{:#x}..{:#x}] actual table: [{:#x} ..{:#x}]",
                    bar_info.idx,
                    bar_info.gpa + msix_table_offset,
                    bar_info.gpa + msix_table_offset + msix_table_size,
                    bar_info.gpa + offset,
                    bar_info.gpa + offset + size,
                );
                let info = BarHoleInfo {
                    gpa: bar_info.gpa + msix_table_offset,
                    size: msix_table_size,
                    offset_in_hole,
                    usage: BarHoleInfoUsage::Table,
                };
                infos.push(info);
            }

            contain_msix_pba = region_info.index == msix_cap.table_bir();
            if contain_msix_pba {
                let (offset, size) = msix_cap.pba_range();
                msix_pba_offset = align_page_size_down(offset);
                msix_pba_size = align_page_size_up(size);
                let offset_in_hole = offset - msix_table_offset;

                LOG!(
                    "BAR{} pba_table hole: [{:#x} ..{:#x}] actual table: [{:#x} ..{:#x}]",
                    bar_info.idx,
                    bar_info.gpa + msix_pba_offset,
                    bar_info.gpa + msix_pba_offset + msix_pba_size,
                    bar_info.gpa + offset,
                    bar_info.gpa + offset + size,
                );
                let info = BarHoleInfo {
                    gpa: bar_info.gpa + msix_pba_offset,
                    size: msix_pba_size,
                    offset_in_hole,
                    usage: BarHoleInfoUsage::Pba,
                };
                infos.push(info);
            }
        }

        if (contain_msix_table || contain_msix_pba)
            && !has_msix_mappable
            && sparse_mmap_cap.is_none()
        {
            LOG!(
                "BAR{} contains msix_table: {} msix_pba: {}, but mappable is {} and \
                 sparse_mmap_cap is {}",
                bar_info.idx,
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

                let mut tmp_areas = [VfioRegionSparseMmapArea::default(); 3];
                let mut tmp_areas_count = 0;

                let areas: &[VfioRegionSparseMmapArea] = if let Some(cap) = sparse_mmap_cap {
                    &cap.areas
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
                            tmp_areas[tmp_areas_count].offset = offset;
                            tmp_areas[tmp_areas_count].size = area_size;
                            tmp_areas_count += 1;
                        }
                        offset = first_gap_offset + first_gap_size;
                    }
                    if second_gap_size != 0 {
                        let area_size = second_gap_offset - offset;
                        if area_size != 0 {
                            tmp_areas[tmp_areas_count].offset = offset;
                            tmp_areas[tmp_areas_count].size = area_size;
                            tmp_areas_count += 1;
                        }
                        offset = second_gap_offset + second_gap_size;
                    }
                    let area_size = region_size - offset;
                    if area_size != 0 {
                        tmp_areas[tmp_areas_count].offset = offset;
                        tmp_areas[tmp_areas_count].size = area_size;
                        tmp_areas_count += 1;
                    }
                    &tmp_areas[0..tmp_areas_count]
                } else {
                    &[VfioRegionSparseMmapArea {
                        offset: 0,
                        size: region_size,
                    }]
                };

                for area in areas.iter() {
                    assert!(
                        (area.size & (4096 - 1)) == 0,
                        "Aresa size is not page aligned"
                    );
                    assert!(
                        (area.offset & (4096 - 1)) == 0,
                        "Aresa offset is not page aligned"
                    );
                    let region_offset = region_info.offset;
                    // SAFETY: FFI call with correct arguments
                    let host_addr = unsafe {
                        libc::mmap(
                            std::ptr::null_mut(),
                            area.size as usize,
                            prot,
                            libc::MAP_SHARED,
                            device.as_raw_fd(),
                            (region_offset + area.offset) as i64,
                        )
                    };

                    if host_addr == libc::MAP_FAILED {
                        panic!("mmap failed");
                    }

                    let iova = bar_info.gpa + area.offset;
                    let size = area.size;
                    let host_addr = host_addr as u64;

                    let kvm_memory_region = kvm_userspace_memory_region {
                        slot: vm.next_kvm_slot(1).unwrap(),
                        flags: 0,
                        guest_phys_addr: iova,
                        memory_size: size,
                        userspace_addr: host_addr,
                    };
                    LOG!(
                        "BAR{} kvm gpa: [{:#x} ..{:#x}]",
                        bar_info.idx,
                        iova,
                        iova + size
                    );
                    vm.set_user_memory_region(kvm_memory_region).unwrap();

                    // TODO: if viortio-iommu is attached no dma setup is
                    // needed at this stage
                    let dma_map = vfio_iommu_type1_dma_map {
                        argsz: std::mem::size_of::<vfio_iommu_type1_dma_map>() as u32,
                        flags: VFIO_DMA_MAP_FLAG_READ | VFIO_DMA_MAP_FLAG_WRITE,
                        vaddr: host_addr,
                        iova: iova,
                        size: size,
                    };
                    iommu_map_dma(container, &dma_map).unwrap();
                }
            }
        }
    }
    infos
}

pub fn mmap_expansion_rom(
    container: &File,
    device: &File,
    expansion_rom_info: &ExpansionRomInfo,
    region_infos: &[VfioRegionInfo],
    vm: &Vm,
) {
    let region_info = &region_infos[VFIO_PCI_ROM_REGION_INDEX as usize];
    let region_offset = region_info.offset;
    let mut prot = 0;
    if region_info.flags & VFIO_REGION_INFO_FLAG_READ != 0 {
        prot |= libc::PROT_READ;
    }
    if region_info.flags & VFIO_REGION_INFO_FLAG_WRITE != 0 {
        prot |= libc::PROT_WRITE;
    }
    // SAFETY: FFI call with correct arguments
    let host_addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            expansion_rom_info.size as usize,
            prot,
            libc::MAP_SHARED,
            device.as_raw_fd(),
            region_offset as i64,
        )
    };

    if host_addr == libc::MAP_FAILED {
        panic!("mmap failed");
    }

    let iova = expansion_rom_info.gpa;
    let size = expansion_rom_info.size as u64;
    let host_addr = host_addr as u64;

    let kvm_memory_region = kvm_userspace_memory_region {
        slot: vm.next_kvm_slot(1).unwrap(),
        flags: 0,
        guest_phys_addr: iova,
        memory_size: size,
        userspace_addr: host_addr,
    };
    LOG!("Expansion ROM kvm gpa: [{:#x} ..{:#x}]", iova, iova + size);
    vm.set_user_memory_region(kvm_memory_region).unwrap();

    // TODO: if viortio-iommu is attached no dma setup is
    // needed at this stage
    let dma_map = vfio_iommu_type1_dma_map {
        argsz: std::mem::size_of::<vfio_iommu_type1_dma_map>() as u32,
        flags: VFIO_DMA_MAP_FLAG_READ | VFIO_DMA_MAP_FLAG_WRITE,
        vaddr: host_addr,
        iova: iova,
        size: size,
    };
    iommu_map_dma(container, &dma_map).unwrap();
}

pub fn dma_map_guest_memory(container: &impl AsRawFd, guest_memory: &GuestMemoryMmap) {
    // TODO: if viortio-iommu is attached no dma setup is
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
            iommu_map_dma(container, &dma_map).unwrap();
        }
    }
}

// fn create_kvm_vfio_device(vm: &VmFd) -> DeviceFd {
//     let mut vfio_dev = kvm_create_device {
//         type_: kvm_device_type_KVM_DEV_TYPE_VFIO,
//         fd: 0,
//         flags: 0,
//     };
//     vm.create_device(&mut vfio_dev).unwrap()
// }
// // flags: KVM_DEV_VFIO_FILE_ADD or KVM_DEV_VFIO_FILE_DEL;
// fn kvm_vfio_device_file_add(device: &DeviceFd, file: &impl AsRawFd, flags: u32) {
//     let file_fd = file.as_raw_fd();
//     let dev_attr = kvm_device_attr {
//         flags: 0,
//         group: KVM_DEV_VFIO_FILE,
//         attr: flags as u64,
//         addr: (&file_fd as *const i32) as u64,
//     };
//     device.set_device_attr(&dev_attr).unwrap();
// }

pub fn do_vfio_magic(path: &str) {
    // vfio part
    let container = vfio_open();
    vfio_check_api_version(&container);
    vfio_check_extension(&container, VFIO_TYPE1v2_IOMMU);

    // open device and vfio group
    // let path = "/sys/bus/mdev/devices/c9abdcb5-5279-413a-9057-c81d2605ce9c/".to_string();
    LOG!("Openning device at path: {}", path);
    let group_id = group_id_from_device_path(&(path.to_string()));
    LOG!("Group id: {}", group_id);
    let group = vfio_group_open(group_id);
    vfio_group_check_status(&group);
    crate::vfio::group_set_container(&group, &container).unwrap();

    // only set after getting the first group
    vfio_container_set_iommu(&container, VFIO_TYPE1v2_IOMMU);

    LOG!("Getting device with info");
    let device = crate::vfio::get_device(&group, path);
    let mut resource_allocator = ResourceAllocator::new();
    LOG!("Getting BAR infos");
    let bar_infos = crate::vfio::device_get_bar_infos(
        &device.file,
        &device.region_infos,
        &mut resource_allocator,
    );
    LOG!("Getting PCI caps");
    let (msi_cap, msix_cap, masks) =
        vfio_device_get_pci_capabilities(&device.file, &device.region_infos, &device.irq_infos);
    if let Some(msi_cap) = &msi_cap {
        LOG!("MSI cap: {msi_cap:#?}");
    }
    if let Some(msix_cap) = &msix_cap {
        LOG!("MSIX cap: {msix_cap:#?}");
    }
    if let Some(masks) = &masks {
        LOG!("MASKS: {masks:#?}");
    }
    // mmap_bars(
    //     &container,
    //     &device.file,
    //     &bar_infos,
    //     &device.region_infos,
    //     msix_cap.as_ref().unwrap(),
    //     _,
    // );
    // dma_map_guest_memory(&container, _);

    // KVM part
    // let kvm_vfio_fd = create_kvm_vfio_device(vm_fd);
    // kvm_vfio_device_file_add(&kvm_vfio_fd, &group, KVM_DEV_VFIO_FILE_ADD);
    // panic!("THE END");

    // for path in paths.iter() {
    //     LOG!("vfio path: {}", path);
    // }
}
