#![allow(missing_docs)]
/// bindings
pub mod bindings;
/// ioctls
pub mod ioctls;

pub use bindings::*;
use kvm_ioctls::DeviceFd;

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
pub const BAR_NUMS: u8 = 6;
// PCI Header Type register index
pub const PCI_HEADER_TYPE_REG_INDEX: usize = 3;
// First BAR register index
pub const PCI_CONFIG_BAR0_INDEX: usize = 4;
// PCI ROM expansion BAR register index
pub const PCI_ROM_EXP_BAR_INDEX: usize = 12;

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::marker::PhantomData;
use std::ops::DerefMut;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::{Arc, Barrier, Mutex};

use kvm_bindings::{
    KVM_DEV_VFIO_FILE, KVM_DEV_VFIO_FILE_ADD, kvm_create_device, kvm_device_attr,
    kvm_device_type_KVM_DEV_TYPE_VFIO, kvm_userspace_memory_region,
};
use pci::{PciBdf, PciCapabilityId, PciExpressCapabilityId};
use vm_allocator::AllocPolicy;
use vm_memory::{GuestMemory, GuestMemoryRegion};
use zerocopy::IntoBytes;

use crate::Vm;
use crate::arch::host_page_size;
use crate::pci::msix::MsixConfig;
use crate::pci::{BarReprogrammingParams, DeviceRelocationError, PciDevice};
use crate::utils::usize_to_u64;
use crate::vfio::ioctls::VfioError as VfioIoctlError;
use crate::vstate::bus::BusDevice;
use crate::vstate::memory::{GuestMemoryMmap, GuestRegionType};
use crate::vstate::resources::ResourceAllocator;

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
    /// KVM failed to create KVM_DEV_TYPE_VFIO device: {0}
    KVMCreateVfioDevice(kvm_ioctls::Error),
}

struct VfioRegionInfoWithCap {
    pub bytes: Vec<u8>,
}
impl VfioRegionInfoWithCap {
    pub fn new_with_argsz(n: u32) -> Self {
        assert!(std::mem::size_of::<vfio_region_info>() <= n as usize);

        // Need 8 byte alignment, but Rust is making it hard
        // There can be some left overs after rounding up, but
        // this is not an issue.
        let total_bytes_u64 = (n + 7) / 8;
        let bytes = vec![0_u64; total_bytes_u64 as usize];
        let ptr = bytes.as_ptr();
        let len = bytes.len();
        let cap = bytes.capacity();
        std::mem::forget(bytes);
        let bytes: Vec<u8> = unsafe { Vec::from_raw_parts(ptr as *mut u8, len * 8, cap * 8) };
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
            assert!(next_cap_offset < self.bytes.len());
            assert!(next_cap_header_end <= self.bytes.len());
            let cap_header = unsafe {
                &*(self.bytes.as_ptr().add(next_cap_offset) as *const vfio_info_cap_header)
            };
            Some(cap_header)
        }
    }
}

struct VfioIrqSet<T: Sized> {
    pub bytes: Vec<u8>,
    _pd: PhantomData<T>,
}

impl<T: Sized> VfioIrqSet<T> {
    pub fn new_with_entries(n: usize) -> Self {
        let vfio_irq_set_bytes = std::mem::size_of::<vfio_irq_set>();
        let entries_bytes = std::mem::size_of::<T>() * n;
        let total_bytes = vfio_irq_set_bytes + entries_bytes;

        // Need 8 byte alignment, but Rust is making it hard
        // There can be some left overs after rounding up, but
        // this is not an issue.
        let total_bytes_u64 = (total_bytes + 7) / 8;
        let bytes = vec![0_u64; total_bytes_u64];
        let ptr = bytes.as_ptr();
        let len = bytes.len();
        let cap = bytes.capacity();
        std::mem::forget(bytes);
        let bytes: Vec<u8> = unsafe { Vec::from_raw_parts(ptr as *mut u8, len * 8, cap * 8) };
        Self {
            bytes,
            _pd: PhantomData,
        }
    }
    pub fn irq_set_mut(&mut self) -> &mut vfio_irq_set {
        unsafe { &mut *(self.bytes.as_mut_ptr() as *mut vfio_irq_set) }
    }
    pub fn entries_mut(&mut self) -> &mut [T] {
        let vfio_irq_set_bytes = std::mem::size_of::<vfio_irq_set>();
        let entries_start = unsafe { self.bytes.as_mut_ptr().add(vfio_irq_set_bytes) };

        let entry_bytes = std::mem::size_of::<T>();
        let entries_size = (self.bytes.len() - vfio_irq_set_bytes) / entry_bytes;
        unsafe { std::slice::from_raw_parts_mut(entries_start as *mut T, entries_size) }
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
    // TODO: this index is redundant. These infos are stored in the array where index into
    // array is same as the index of the region in VFIO.
    pub index: u32,
    pub size: u64,
    pub offset: u64,
    pub caps: Vec<VfioRegionCap>,
}

/// 7.7.1.2 Message Control Register for MSI
// #[derive(Debug)]
// pub struct MsiCap {
//     pub register: u8,
//     pub msg_ctl: u16,
//     pub low: u32,
//     pub high: u32,
//     pub data: u32,
// }

/// 7.7.2 MSI-X Capability and Table Structure
#[derive(Debug)]
pub struct MsixCap {
    pub register: u8,
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
        let size = ((self.table_size() as u64 + 63) / 64) * 8;
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
    pub idx: u8,
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
pub struct ConfigSpaceInfo {
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u32,
    pub revision_id: u8,
}

// #[derive(Debug)]
// pub struct ExpansionRomInfo {
//     pub kvm_region: kvm_userspace_memory_region,
//     // The actual size of the mapping is aligned up to page boudary
//     pub rom_bytes: Vec<u8>,
//     // Validation status and Validation Details
//     pub extra: u16,
//     // just for testing
//     pub about_to_read_size: bool,
// }

#[derive(Debug)]
pub struct VfioDevice {
    pub file: File,
    pub info: vfio_device_info,
    pub region_infos: Vec<VfioRegionInfo>,
    pub irq_infos: Vec<vfio_irq_info>,
}

#[derive(Debug)]
pub struct MsixState {
    pub cap: MsixCap,
    pub bar_hole_infos: Vec<BarHoleInfo>,
    pub config: MsixConfig,
}

/// The VFIO device bundle
#[derive(Debug)]
pub struct VfioDeviceBundle {
    pub id: String,
    pub group_id: u32,
    pub group: File,
    pub device: VfioDevice,
    pub bar_infos: Vec<BarInfo>,
    // pub expansion_rom_info: Option<ExpansionRomInfo>,
    pub msix_state: Option<MsixState>,
    // pub msi_cap: Option<MsiCap>,
    pub masks: Vec<RegisterMask>,
    pub vm: Arc<Vm>,
}

#[derive(Debug)]
pub struct VfioKvmAndContainer {
    pub container: File,
    pub kvm_device: DeviceFd,
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
        if let Some(state) = self.msix_state.as_ref() {
            let mut name = "----";
            let mut handled: bool = false;
            for info in state.bar_hole_infos.iter() {
                if info.gpa == base {
                    let hole_start = info.offset_in_hole;
                    let hole_end = info.offset_in_hole + info.size;
                    let data_start = offset;
                    let data_end = offset + data.len() as u64;
                    if hole_start <= data_start && data_end <= hole_end {
                        match info.usage {
                            BarHoleInfoUsage::Table => {
                                name = "MsiTable";
                                state.config.read_table(offset, data);
                            }
                            BarHoleInfoUsage::Pba => {
                                name = "PbaTable";
                                state.config.read_pba(offset, data);
                            }
                        }
                    } else {
                        let region_index = match info.usage {
                            BarHoleInfoUsage::Table => state.cap.table_bir(),
                            BarHoleInfoUsage::Pba => state.cap.pba_bir(),
                        };
                        let _ = vfio_device_region_read(
                            &self.device.file,
                            &self.device.region_infos,
                            region_index,
                            offset,
                            data,
                        );
                        name = "OutsideTable";
                    }
                    handled = true;
                }
            }
            if !handled {
                for d in data.iter_mut() {
                    *d = 0;
                }
            }
            LOG!(
                "[{}] base: {base:<#10x} offset: {offset:<#5x} data: {data:<4?} name: {name} \
                 handled: {handled}",
                self.id,
            );
        } else {
            panic!("Should never happen");
        }
    }

    fn write(&mut self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        let mut name = "----";
        if let Some(state) = self.msix_state.as_mut() {
            let mut handled: bool = false;
            for info in state.bar_hole_infos.iter() {
                if info.gpa == base {
                    let hole_start = info.offset_in_hole;
                    let hole_end = info.offset_in_hole + info.size;
                    let data_start = offset;
                    let data_end = offset + data.len() as u64;
                    if hole_start <= data_start && data_end <= hole_end {
                        match info.usage {
                            BarHoleInfoUsage::Table => {
                                name = "MsiTable";
                                state.config.write_table(offset, data);
                            }
                            BarHoleInfoUsage::Pba => {
                                name = "PbaTable";
                                state.config.write_pba(offset, data);
                            }
                        }
                        handled = true;
                    } else {
                        let region_index = match info.usage {
                            BarHoleInfoUsage::Table => state.cap.table_bir(),
                            BarHoleInfoUsage::Pba => state.cap.pba_bir(),
                        };
                        let _ = vfio_device_region_write(
                            &self.device.file,
                            &self.device.region_infos,
                            region_index,
                            offset,
                            data,
                        );
                        name = "OutsideTable";
                        handled = true;
                    }
                }
            }
            assert!(handled);
            LOG!(
                "[{}] base: {base:<#10x} offset: {offset:<#5x} data: {data:<4?} table_name: {name}",
                self.id
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
        let mut name = "----";
        let mut handled: bool = false;
        let config_offset = reg_idx as u64 * 4 + offset;
        if 4 <= reg_idx && reg_idx < 10 {
            let bar_idx = (reg_idx - 4) as u8;

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
                    name = "BAR";
                    handled = true;
                } else if bar_idx == bar_info.idx + 1 && bar_info.is_64_bits {
                    if looks_like_request_to_read {
                        bar_info.about_to_read_size = true;
                    }
                    name = "BAR";
                    handled = true;
                }
            }
        // } else if reg_idx == 12 {
        //     if let Some(rom_info) = self.expansion_rom_info.as_mut() {
        //         if data.len() == 4 {
        //             let data: u32 = u32::from_le_bytes(data.try_into().unwrap());
        //             if data & 0xFFFFF800 == 0xFFFFF800 {
        //                 rom_info.about_to_read_size = true;
        //             } else {
        //                 rom_info.extra = (data & ((1 << 11) - 1)) as u16;
        //                 // TODO handle ROM relocation, just as any other BAR relocation
        //             }
        //         }
        //         name = "ROM";
        //         handled = true;
        //     }
        // } else if let Some(msi_cap) = self.msi_cap.as_mut() {
        //     let data: u32 = u32::from_le_bytes(data.try_into().unwrap());
        //     if reg_idx == msi_cap.register as usize {
        //         if data & 0x1 != 0 {
        //             // controll
        //             let vector = MsixVector::new(0, true).unwrap();
        //             let config = MsixVectorConfig {
        //                 high_addr: msi_cap.high,
        //                 low_addr: msi_cap.low,
        //                 data: msi_cap.data,
        //                 devid: 0,
        //             };
        //             self.vm.register_msi(&vector, false, config).unwrap();
        //
        //             let mut vfio_irq_set = VfioIrqSet::new_with_entries(1);
        //             let vfio_irq_set_bytes = vfio_irq_set.bytes.len();
        //             {
        //                 let irq_set = vfio_irq_set.irq_set_mut();
        //                 irq_set.argsz = vfio_irq_set_bytes as u32;
        //                 irq_set.flags = VFIO_IRQ_SET_DATA_EVENTFD | VFIO_IRQ_SET_ACTION_TRIGGER;
        //                 irq_set.index = VFIO_PCI_MSI_IRQ_INDEX;
        //                 irq_set.start = 0;
        //                 irq_set.count = 1;
        //             }
        //             {
        //                 let irq_fds = vfio_irq_set.entries_mut();
        //                 irq_fds[0] = vector.event_fd.as_raw_fd();
        //             }
        //             ioctls::device_set_irqs(&self.device.file,
        // vfio_irq_set.irq_set_mut()).unwrap();             LOG!("MSI irq is set-up");
        //         }
        //     } else if reg_idx == msi_cap.register as usize + 1 {
        //         msi_cap.low = data;
        //     } else if reg_idx == msi_cap.register as usize + 2 {
        //         msi_cap.high = data;
        //     } else if reg_idx == msi_cap.register as usize + 3 {
        //         msi_cap.data = data;
        //     }
        //     name = "MSI_CAP";
        //     handled = true;
        } else if let Some(state) = self.msix_state.as_mut() {
            if reg_idx == state.cap.register as usize {
                if offset == 2 && data.len() == 2 {
                    let data = u16::from_le_bytes(data.try_into().unwrap());
                    state.config.set_msg_ctl(data);
                    name = "MSIX_CAP";
                } else if offset == 0 && data.len() == 4 {
                    let data = u16::from_le_bytes(data[2..].try_into().unwrap());
                    state.config.set_msg_ctl(data);
                    name = "MSIX_CAP";
                }
            }
        }
        if !handled {
            let _ = vfio_device_region_write(
                &self.device.file,
                &self.device.region_infos,
                VFIO_PCI_CONFIG_REGION_INDEX,
                config_offset,
                data,
            );
        }
        LOG!(
            "[{}] reg: {reg_idx:>3}({config_offset:>#6x}) data: {data:<4?} name: {name}",
            self.id
        );
        None
    }
    fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        let mut name = "----";
        let config_offset = reg_idx as u64 * 4;
        let mut result: u32 = 0;
        if 4 <= reg_idx && reg_idx < 10 {
            let bar_idx = (reg_idx - 4) as u8;
            for bar_info in self.bar_infos.iter_mut() {
                if bar_idx == bar_info.idx {
                    if bar_info.about_to_read_size {
                        let size = !(bar_info.size - 1);
                        result = (size & 0xFFFF_FFFF) as u32;
                        bar_info.about_to_read_size = false;
                    } else {
                        let is_64_bits = if bar_info.is_64_bits { 0b10 << 1 } else { 0 };
                        let is_prefetchable = if bar_info.is_prefetchable { 0b1000 } else { 0 };
                        result = (bar_info.gpa & 0xFFFF_FFFF) as u32 | is_64_bits | is_prefetchable;
                    }
                    name = "BAR";
                } else if bar_info.is_64_bits && bar_idx == bar_info.idx + 1 {
                    if bar_info.about_to_read_size {
                        let size = !(bar_info.size - 1);
                        result = (size >> 32) as u32;
                        bar_info.about_to_read_size = false;
                    } else {
                        result = (bar_info.gpa >> 32) as u32;
                    }
                    name = "BAR";
                }
            }
        // } else if reg_idx == 12 {
        //     if let Some(rom_info) = self.expansion_rom_info.as_mut() {
        //         if rom_info.about_to_read_size {
        //             result = !(rom_info.rom_bytes.len() as u32 - 1);
        //             rom_info.about_to_read_size = false;
        //         } else {
        //             result = (rom_info.kvm_region.guest_phys_addr & 0xFFFF_F800) as u32
        //                 | rom_info.extra as u32;
        //         }
        //         name = "ROM";
        //     }
        } else {
            let _ = vfio_device_region_read(
                &self.device.file,
                &self.device.region_infos,
                VFIO_PCI_CONFIG_REGION_INDEX,
                config_offset,
                result.as_mut_bytes(),
            );
            if let Some(state) = self.msix_state.as_ref() {
                if reg_idx == state.cap.register as usize {
                    result = ((state.config.enabled as u32) << 31)
                        | ((state.config.masked as u32) << 30)
                        | result;
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
        LOG!(
            "[{}] reg: {reg_idx:>3}({config_offset:>#6x}) data: {:<4?} name: {name}",
            self.id,
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

pub fn vfio_open() -> Result<File, VfioError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/vfio/vfio")
        .map_err(VfioError::Open)
}

pub fn vfio_check_api_version(container: &impl AsRawFd) -> Result<(), VfioError> {
    let version = ioctls::check_api_version(container);
    LOG!("vfio api version: {}", version);
    if version != VFIO_API_VERSION as i32 {
        return Err(VfioError::CheckApiVersion(version));
    }
    Ok(())
}

pub fn vfio_check_extension(container: &impl AsRawFd) -> Result<(), VfioError> {
    let ret = ioctls::check_extension(container, VFIO_TYPE1v2_IOMMU)?;
    if ret != 1 {
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
    if group_status.flags != VFIO_GROUP_FLAGS_VIABLE {
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

pub fn vfio_device_reset(device: &impl AsRawFd, device_info: &vfio_device_info) {
    if device_info.flags & VFIO_DEVICE_FLAGS_RESET != 0 {
        ioctls::device_reset(device);
    }
}

pub fn vfio_device_get_region_infos(
    device: &impl AsRawFd,
    device_info: &vfio_device_info,
) -> Result<Vec<VfioRegionInfo>, VfioError> {
    let mut regions = Vec::with_capacity(device_info.num_regions as usize);
    for i in 0..device_info.num_regions {
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
        if ioctls::device_get_region_info(device, &mut region_info).is_err()
            || region_info.size == 0
        {
            LOG!("Region {i} is not available or not implemented. Setting to 0");
            let region_info = VfioRegionInfo {
                flags: 0,
                index: region_info.index,
                size: 0,
                offset: 0,
                caps: Vec::new(),
            };
            regions.push(region_info);
        } else {
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
                let mut vfio_region_info_with_caps =
                    VfioRegionInfoWithCap::new_with_argsz(region_info.argsz);
                let region_info_with_caps = vfio_region_info_with_caps.vfio_region_info_mut();
                region_info_with_caps.argsz = region_info.argsz;
                region_info_with_caps.flags = 0;
                region_info_with_caps.index = region_info.index;
                region_info_with_caps.cap_offset = 0;
                region_info_with_caps.size = 0;
                region_info_with_caps.offset = 0;
                ioctls::device_get_region_info(device, region_info_with_caps)?;

                let mut next_cap_offset = region_info_with_caps.cap_offset;
                while let Some(cap_header) =
                    vfio_region_info_with_caps.vfio_info_cap_header_at_offset(next_cap_offset)
                {
                    LOG!("Cap id: {}", cap_header.id);
                    match u32::from(cap_header.id) {
                        VFIO_REGION_INFO_CAP_SPARSE_MMAP => {
                            // SAFETY: data structure returned by kernel is trusted.
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
            LOG!(
                "Region {i} info: flags: {:x} index: {} size: {} offset: {}",
                region_info.flags,
                region_info.index,
                region_info.size,
                region_info.offset
            );
            for cap in region_info.caps.iter() {
                LOG!("Cap: {:?}", cap);
            }
            regions.push(region_info);
        }
    }
    Ok(regions)
}

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

pub fn vfio_device_region_read(
    device: &impl FileExt,
    region_infos: &[VfioRegionInfo],
    index: u32,
    offset: u64,
    buf: &mut [u8],
) -> Result<(), VfioError> {
    let region_info = &region_infos[index as usize];
    let end = offset + buf.len() as u64;
    assert!(
        end <= region_info.size + 1,
        "Invalid device region read of [{:x}..{:x}], but region is [0..{:x}]",
        offset,
        end,
        region_info.size
    );
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
    assert!(
        end <= region_info.size + 1,
        "Invalid device region write of [{:x}..{:x}], but region is [0..{:x}]",
        offset,
        end,
        region_info.size
    );
    device
        .write_all_at(buf, region_info.offset + offset)
        .map_err(|e| VfioError::RegionWrite(index, offset, e))?;
    Ok(())
}

pub fn vfio_device_get_pci_capabilities(
    device: &impl FileExt,
    region_infos: &[VfioRegionInfo],
    irq_infos: &[vfio_irq_info],
    // ) -> Result<(Option<MsiCap>, Option<MsixCap>, Vec<RegisterMask>), VfioError> {
) -> Result<(Option<MsixCap>, Vec<RegisterMask>), VfioError> {
    let mut next_cap_offset: u8 = 0;
    vfio_device_region_read(
        device,
        region_infos,
        VFIO_PCI_CONFIG_REGION_INDEX,
        PCI_CONFIG_CAPABILITY_OFFSET as u64,
        next_cap_offset.as_mut_bytes(),
    )?;

    let mut has_pci_express_cap = false;
    // let mut has_power_management_cap = false;

    // let mut msi_cap = None;
    let mut msix_cap = None;
    LOG!("PCI CAPS offset: {}", next_cap_offset);
    while next_cap_offset != 0 {
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
        LOG!("PCI CAP id: {cap_id} next offset: {next_cap_offset:#x}");

        match PciCapabilityId::from(cap_id) {
            PciCapabilityId::MessageSignalledInterrupts => {
                if let Some(irq_info) = irq_infos.get(VFIO_PCI_MSI_IRQ_INDEX as usize) {
                    if irq_info.count != 0 {
                        let register = current_cap_offset / 4;
                        LOG!("Found MSI cap at offset: {current_cap_offset:#x}({register})");

                        // let mut msg_ctl: u16 = 0;
                        // vfio_device_region_read(
                        //     device,
                        //     region_infos,
                        //     VFIO_PCI_CONFIG_REGION_INDEX,
                        //     // 7.7.1 MSI Capability Structures
                        //     // |      2 bytes    |     1 byte    |          1 byte         |
                        //     // | Message Control | Capability ID | Next Capability Pointer |
                        //     (current_cap_offset as u64) + 2,
                        //     msg_ctl.as_mut_bytes(),
                        // )?;
                        // msi_cap = Some(MsiCap {
                        //     register,
                        //     msg_ctl,
                        //     low: 0,
                        //     high: 0,
                        //     data: 0,
                        // });
                    } else {
                        LOG!("Found MSI cap, but the device does not support MSI interrupts.");
                    }
                }
            }
            PciCapabilityId::MsiX => {
                if let Some(irq_info) = irq_infos.get(VFIO_PCI_MSIX_IRQ_INDEX as usize) {
                    if irq_info.count != 0 {
                        let register = current_cap_offset / 4;
                        LOG!("Found MSIX cap at offset: {current_cap_offset:#x}({register})");

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
                        )?;
                        vfio_device_region_read(
                            device,
                            region_infos,
                            VFIO_PCI_CONFIG_REGION_INDEX,
                            (current_cap_offset as u64) + 4,
                            table_offset.as_mut_bytes(),
                        )?;
                        vfio_device_region_read(
                            device,
                            region_infos,
                            VFIO_PCI_CONFIG_REGION_INDEX,
                            (current_cap_offset as u64) + 8,
                            pba_offset.as_mut_bytes(),
                        )?;
                        msix_cap = Some(MsixCap {
                            register,
                            msg_ctl,
                            table_offset,
                            pba_offset,
                        });
                    } else {
                        LOG!("Found MSI-X cap, but the device does not support MSI-X interrupts.");
                    }
                }
            }
            PciCapabilityId::PciExpress => {
                let register = current_cap_offset / 4;
                LOG!("Found PciExpress cap at offset: {current_cap_offset:#x}({register})");

                has_pci_express_cap = true;
            }
            // 7.5.2 PCI Power Management Capability Structure
            // This structure is required for all PCI Express Functions.
            // But I think just PciExpress shoudl be enough?
            // PciCapabilityId::PowerManagement => has_power_management_cap = true,
            _ => {}
        };
    }

    let mut masks = Vec::new();
    if has_pci_express_cap {
        let mut next_cap_offset: u16 = PCI_CONFIG_EXTENDED_CAPABILITY_OFFSET as u16;
        while next_cap_offset != 0 {
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
            LOG!("Found {pci_cap:?} cap at offset: {current_cap_offset:#x}({register})");

            // TODO: the list of capabilities is hardcoded for now. In the future this
            // may be configurable from the user side.
            match pci_cap {
                PciExpressCapabilityId::AlternativeRoutingIdentificationInterpretation
                | PciExpressCapabilityId::ResizeableBar
                | PciExpressCapabilityId::SingleRootIoVirtualization => {
                    LOG!("Found cap to be masked at register: {register}({current_cap_offset:#x})");
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
    // Ok((msi_cap, msix_cap, masks))
    Ok((msix_cap, masks))
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

pub fn vfio_device_get_bar_infos(
    device: &impl FileExt,
    region_infos: &[VfioRegionInfo],
    resource_allocator: &mut ResourceAllocator,
) -> Result<Vec<BarInfo>, VfioError> {
    let mut bar_infos = Vec::new();
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
            let mut gpa = 0;
            if is_io_bar {
                LOG!(
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
            } else {
                let alignment = calculate_alignment(size);
                gpa = resource_allocator
                    .mmio32_memory
                    .allocate(size, alignment, AllocPolicy::FirstMatch)
                    .map_err(|_| VfioError::BarAllocation)?
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
    Ok(bar_infos)
}

// pub fn vfio_device_get_expansion_rom_info(
//     device: &impl FileExt,
//     region_infos: &[VfioRegionInfo],
//     resource_allocator: &mut ResourceAllocator,
//     vm: &Vm,
// ) -> Result<Option<ExpansionRomInfo>, VfioError> {
//     let region_info = &region_infos[VFIO_PCI_ROM_REGION_INDEX as usize];
//     let rom_size = region_info.size as u32;
//     let mut result = None;
//     if rom_size != 0 {
//         // This is needed to enable ROM bar on the device
//         let mut rom_raw: u32 = 0;
//         vfio_device_region_read(
//             device,
//             region_infos,
//             VFIO_PCI_CONFIG_REGION_INDEX,
//             0x30,
//             rom_raw.as_mut_bytes(),
//         )?;
//         rom_raw |= 0x1;
//         vfio_device_region_write(
//             device,
//             region_infos,
//             VFIO_PCI_CONFIG_REGION_INDEX,
//             0x30,
//             rom_raw.as_mut_bytes(),
//         )?;
//
//         let mut rom_bytes = vec![0; rom_size as usize];
//         vfio_device_region_read(
//             device,
//             region_infos,
//             VFIO_PCI_ROM_REGION_INDEX,
//             0x0,
//             &mut rom_bytes,
//         )?;
//
//         rom_raw &= !0x1;
//         vfio_device_region_write(
//             device,
//             region_infos,
//             VFIO_PCI_CONFIG_REGION_INDEX,
//             0x30,
//             rom_raw.as_mut_bytes(),
//         )?;
//
//         let size = (rom_size + 4095) & !(4095);
//
//         let gpa = resource_allocator
//             .mmio32_memory
//             .allocate(size as u64, 64, AllocPolicy::FirstMatch)
//             .map_err(|_| VfioError::BarAllocation)?
//             .start();
//         LOG!(
//             "Expansion ROM gpa: [{:#x}..{:#x}] rom_size: {rom_size:>#10x} Configured from VFIO \
//              region",
//             gpa,
//             gpa + size as u64
//         );
//
//         // SAFETY: FFI call with correct arguments
//         let host_addr = unsafe {
//             libc::mmap(
//                 std::ptr::null_mut(),
//                 size as usize,
//                 libc::PROT_READ | libc::PROT_WRITE,
//                 libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
//                 -1,
//                 0,
//             )
//         };
//         if host_addr == libc::MAP_FAILED {
//             return Err(VfioError::Mmap);
//         }
//
//         // Copy ROM content
//         unsafe {
//             std::ptr::copy_nonoverlapping(
//                 rom_bytes.as_ptr(),
//                 host_addr as *mut u8,
//                 rom_bytes.len(),
//             );
//         }
//
//         let slot = vm.next_kvm_slot(1).ok_or(VfioError::KvmSlot)?;
//         let kvm_region = kvm_userspace_memory_region {
//             slot,
//             flags: 0,
//             guest_phys_addr: gpa,
//             memory_size: size as u64,
//             userspace_addr: host_addr as u64,
//         };
//         LOG!("ROM kvm gpa: [{:#x} ..{:#x}]", gpa, gpa + size as u64);
//         vm.set_user_memory_region(kvm_region)
//             .map_err(|e| VfioError::SetUserMemoryRegion(e.to_string()))?;
//
//         let (rom_raw, _) = vfio_device_get_single_bar_info(device, region_infos, 8)?;
//         result = Some(ExpansionRomInfo {
//             kvm_region,
//             rom_bytes,
//             // get extra data + set the enable bit
//             extra: (rom_raw & ((1 << 12) - 1)) as u16 | 0x1,
//             about_to_read_size: false,
//         });
//     }
//
//     Ok(result)
// }

pub fn vfio_device_get_config_space_info(
    device: &impl FileExt,
    region_infos: &[VfioRegionInfo],
) -> Result<ConfigSpaceInfo, VfioError> {
    let mut device_id_vendor_id: u32 = 0;
    vfio_device_region_read(
        device,
        region_infos,
        VFIO_PCI_CONFIG_REGION_INDEX,
        0x0,
        device_id_vendor_id.as_mut_bytes(),
    )?;
    let vendor_id = (device_id_vendor_id & 0xFFFF) as u16;
    let device_id = (device_id_vendor_id >> 16) as u16;
    LOG!("Vendor id: {vendor_id:#x} Device id: {device_id:#x}");

    let mut class_code_and_revision_id: u32 = 0;
    vfio_device_region_read(
        device,
        region_infos,
        VFIO_PCI_CONFIG_REGION_INDEX,
        0x8,
        class_code_and_revision_id.as_mut_bytes(),
    )?;
    let revision_id = (class_code_and_revision_id & 0xFF) as u8;
    let class_code = (class_code_and_revision_id >> 8) as u32;
    LOG!("Revision id: {revision_id:#x} Class code: {class_code:#x}");
    Ok(ConfigSpaceInfo {
        vendor_id,
        device_id,
        class_code,
        revision_id,
    })
}

pub fn get_device(group: &impl AsRawFd, path: &str) -> Result<VfioDevice, VfioError> {
    let device_file = vfio_group_get_device(group, &path)?;
    let device_info = vfio_device_get_info(&device_file)?;
    LOG!("Device info: {device_info:#?}");
    vfio_device_reset(&device_file, &device_info);

    let device_region_infos = vfio_device_get_region_infos(&device_file, &device_info)?;

    // LOG!("Getting PCI caps");
    // let mut pci_cap_offset: u8 = 0;
    // vfio_device_region_read(
    //     &device_file,
    //     &device_region_infos,
    //     VFIO_PCI_CONFIG_REGION_INDEX,
    //     PCI_CONFIG_CAPABILITY_OFFSET as u64,
    //     pci_cap_offset.as_mut_bytes(),
    // )?;
    // LOG!("PCI cap offset: {}", pci_cap_offset);
    // while pci_cap_offset != 0 {
    //     let mut pci_cap_id = 0;
    //     vfio_device_region_read(
    //         &device_file,
    //         &device_region_infos,
    //         VFIO_PCI_CONFIG_REGION_INDEX,
    //         pci_cap_offset as u64,
    //         pci_cap_id.as_mut_bytes(),
    //     )?;
    //     let pci_cap = PciCapabilityId::from(pci_cap_id);
    //     LOG!("Pci cap found: {:?}", pci_cap);
    //     vfio_device_region_read(
    //         &device_file,
    //         &device_region_infos,
    //         VFIO_PCI_CONFIG_REGION_INDEX,
    //         (pci_cap_offset + 1) as u64,
    //         pci_cap_offset.as_mut_bytes(),
    //     )?;
    // }

    let device_irq_infos = vfio_device_get_irq_infos(&device_file, &device_info);
    // if VFIO_PCI_INTX_IRQ_INDEX < device_irq_infos.len() as u32 {
    //     LOG!(
    //         "INTX IRQ info: {:?}",
    //         device_irq_infos[VFIO_PCI_INTX_IRQ_INDEX as usize]
    //     );
    // }
    // if VFIO_PCI_MSI_IRQ_INDEX < device_irq_infos.len() as u32 {
    //     LOG!(
    //         "MSI IRQ info: {:?}",
    //         device_irq_infos[VFIO_PCI_MSI_IRQ_INDEX as usize]
    //     );
    // }
    // if VFIO_PCI_MSIX_IRQ_INDEX < device_irq_infos.len() as u32 {
    //     LOG!(
    //         "MSIX IRQ info: {:?}",
    //         device_irq_infos[VFIO_PCI_MSIX_IRQ_INDEX as usize]
    //     );
    // }

    Ok(VfioDevice {
        file: device_file,
        info: device_info,
        region_infos: device_region_infos,
        irq_infos: device_irq_infos,
    })
}

pub fn mmap_bars(
    container: &impl AsRawFd,
    device: &impl AsRawFd,
    bar_infos: &[BarInfo],
    region_infos: &[VfioRegionInfo],
    msix_cap: Option<&MsixCap>,
    vm: &Vm,
) -> Result<Vec<BarHoleInfo>, VfioError> {
    let mut bar_hole_infos = Vec::new();
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
            align_page_size_down(v + 4096 - 1)
        }
        if let Some(msix_cap) = msix_cap {
            contain_msix_table = region_info.index == msix_cap.table_bir();
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
                bar_hole_infos.push(info);
            }

            contain_msix_pba = region_info.index == msix_cap.pba_bir();
            if contain_msix_pba {
                let (offset, size) = msix_cap.pba_range();
                msix_pba_offset = align_page_size_down(offset);
                msix_pba_size = align_page_size_up(size);
                let offset_in_hole = offset - msix_pba_offset;

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
                bar_hole_infos.push(info);
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
                        return Err(VfioError::Mmap);
                    }

                    let iova = bar_info.gpa + area.offset;
                    let size = area.size;
                    let host_addr = host_addr as u64;

                    let kvm_memory_region = kvm_userspace_memory_region {
                        slot: vm.next_kvm_slot(1).ok_or(VfioError::KvmSlot)?,
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
                    vm.set_user_memory_region(kvm_memory_region)
                        .map_err(|e| VfioError::SetUserMemoryRegion(e.to_string()))?;

                    // TODO: if viortio-iommu is attached no dma setup is
                    // needed at this stage
                    let dma_map = vfio_iommu_type1_dma_map {
                        argsz: std::mem::size_of::<vfio_iommu_type1_dma_map>() as u32,
                        // NOTE: VFIO_DMA_MAP_FLAG_READ and VFIO_DMA_MAP_FLAG_WRITE flags are
                        // same as PROT_READ and PROT_WRITE
                        flags: prot as u32,
                        vaddr: host_addr,
                        iova: iova,
                        size: size,
                    };
                    ioctls::iommu_map_dma(container, &dma_map)?;
                }
            }
        }
    }
    Ok(bar_hole_infos)
}

pub fn dma_map_guest_memory(
    container: &impl AsRawFd,
    guest_memory: &GuestMemoryMmap,
) -> Result<(), VfioError> {
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
            LOG!("DMA guest memory: [{:#x}..{:#x}]", iova, iova + size);
            ioctls::iommu_map_dma(container, &dma_map)?;
        }
    }
    Ok(())
}

// pub fn copy_rom_into_legacy_location(
//     guest_memory: &GuestMemoryMmap,
//     rom_info: &ExpansionRomInfo,
// ) -> Result<(), VfioError> {
//     guest_memory
//         .write(&rom_info.rom_bytes, GuestAddress(0xC0000))
//         .map_err(|e| VfioError::CopyRom(e.to_string()))?;
//     Ok(())
// }

pub fn set_msix_irqs(
    device: &impl AsRawFd,
    irq_infos: &[vfio_irq_info],
    msix_config: &MsixConfig,
) -> Result<(), VfioError> {
    let msix_irq_info = &irq_infos[VFIO_PCI_MSIX_IRQ_INDEX as usize];
    if msix_irq_info.count == 0 || msix_config.vectors.vectors.len() != msix_irq_info.count as usize
    {
        LOG!("Skipping MSI setup of vfio");
        return Ok(());
    }

    let mut vfio_irq_set = VfioIrqSet::new_with_entries(msix_irq_info.count as usize);
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

pub fn kvm_create_vfio_device(vm: &Vm) -> Result<DeviceFd, VfioError> {
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
// flags: KVM_DEV_VFIO_FILE_ADD or KVM_DEV_VFIO_FILE_DEL;
pub fn kvm_vfio_device_file_add(device: &DeviceFd, file: &impl AsRawFd) {
    let file_fd = file.as_raw_fd();
    let dev_attr = kvm_device_attr {
        flags: 0,
        group: KVM_DEV_VFIO_FILE,
        attr: KVM_DEV_VFIO_FILE_ADD as u64,
        addr: (&file_fd as *const i32) as u64,
    };
    device.set_device_attr(&dev_attr).unwrap();
}

pub fn init_vfio_device(
    vfio_kvm_and_container: &VfioKvmAndContainer,
    vm: &Arc<Vm>,
    id: String,
    path: &str,
    bdf: PciBdf,
    need_to_set_container_iommu: bool,
) -> Result<Arc<Mutex<VfioDeviceBundle>>, VfioError> {
    let container = &vfio_kvm_and_container.container;
    let kvm_device = &vfio_kvm_and_container.kvm_device;

    LOG!("Openning device at path: {}", path);
    let group_id = group_id_from_device_path(&path)?;
    LOG!("Group id: {}", group_id);
    let group = vfio_group_open(group_id)?;
    ioctls::group_set_container(&group, container).map_err(VfioError::from)?;
    kvm_vfio_device_file_add(kvm_device, &group);

    // only set after getting the first group
    if need_to_set_container_iommu {
        vfio_container_set_iommu(container, VFIO_TYPE1v2_IOMMU)?;
    }

    let device = get_device(&group, path)?;

    // let (bar_infos, expansion_rom_info) = {
    let bar_infos = {
        let mut resource_allocator_lock = vm.resource_allocator();
        let resource_allocator = resource_allocator_lock.deref_mut();
        let bar_infos =
            vfio_device_get_bar_infos(&device.file, &device.region_infos, resource_allocator)?;
        // let rom_info = device_get_expansion_rom_info(
        //     &device.file,
        //     &device.region_infos,
        //     resource_allocator,
        //     vm.as_ref(),
        // )?;
        // (bar_infos, rom_info)
        bar_infos
    };
    // let (msi_cap, msix_cap, masks) =
    let (msix_cap, masks) =
        vfio_device_get_pci_capabilities(&device.file, &device.region_infos, &device.irq_infos)?;
    let bar_hole_infos = mmap_bars(
        container,
        &device.file,
        &bar_infos,
        &device.region_infos,
        msix_cap.as_ref(),
        vm.as_ref(),
    )?;
    dma_map_guest_memory(container, vm.guest_memory())?;

    // #[cfg(x86_64)]
    // if let Some(rom_info) = expansion_rom_info.as_ref() {
    //     copy_rom_into_legacy_location(vm.guest_memory(), rom_info)?;
    // }

    let _config_space_info = vfio_device_get_config_space_info(&device.file, &device.region_infos)?;

    let mut msix_state = None;
    if let Some(msix_cap) = msix_cap {
        assert!(
            VFIO_PCI_MSIX_IRQ_INDEX < device.irq_infos.len() as u32,
            "Found MSI-X capability, but VFIO does not have irq_info at VFIO_PCI_MSIX_IRQ_INDEX"
        );
        let msix_irq_info = &device.irq_infos[VFIO_PCI_MSIX_IRQ_INDEX as usize];
        let msix_num = msix_irq_info.count as u16;
        println!("VFIO msix_num: {msix_num}");
        let msix_vectors = Vm::create_msix_group(vm.clone(), msix_num).unwrap();
        let msix_config = crate::pci::msix::MsixConfig::new(Arc::new(msix_vectors), bdf.into());
        set_msix_irqs(&device.file, &device.irq_infos, &msix_config)?;
        msix_state = Some(MsixState {
            cap: msix_cap,
            bar_hole_infos: bar_hole_infos,
            config: msix_config,
        });
    }

    // add to the segment since we will need to configure MSIs
    let vfio_device_bundle = Arc::new(Mutex::new(VfioDeviceBundle {
        id,
        group_id,
        group,
        device,
        bar_infos,
        // expansion_rom_info,
        msix_state,
        // msi_cap,
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

pub fn do_vfio_magic(path: &str) -> Result<(), VfioError> {
    // vfio part
    let container = vfio_open()?;
    vfio_check_api_version(&container)?;
    vfio_check_extension(&container)?;

    // open device and vfio group
    // let path = "/sys/bus/mdev/devices/c9abdcb5-5279-413a-9057-c81d2605ce9c/".to_string();
    LOG!("Openning device at path: {}", path);
    let group_id = group_id_from_device_path(&(path.to_string()))?;
    LOG!("Group id: {}", group_id);
    let group = vfio_group_open(group_id)?;
    ioctls::group_set_container(&group, &container)?;

    // only set after getting the first group
    vfio_container_set_iommu(&container, VFIO_TYPE1v2_IOMMU)?;

    LOG!("Getting device with info");
    let device = get_device(&group, path)?;
    let mut resource_allocator = ResourceAllocator::new();
    LOG!("Getting BAR infos");
    let bar_infos =
        vfio_device_get_bar_infos(&device.file, &device.region_infos, &mut resource_allocator)?;
    LOG!("Getting PCI caps");
    // let (msi_cap, msix_cap, masks) =
    let (msix_cap, masks) =
        vfio_device_get_pci_capabilities(&device.file, &device.region_infos, &device.irq_infos)?;
    // if let Some(msi_cap) = &msi_cap {
    //     LOG!("MSI cap: {msi_cap:#?}");
    // }
    if let Some(msix_cap) = &msix_cap {
        LOG!("MSIX cap: {msix_cap:#?}");
    }
    // if let Some(masks) = &masks {
    LOG!("MASKS: {masks:#?}");
    // }
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
    Ok(())
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
    #[should_panic]
    fn test_vfio_region_info_with_caps_panic_caps_at_offset() {
        let vfio_region_info_bytes = std::mem::size_of::<vfio_region_info>();
        let mut vriwc = VfioRegionInfoWithCap::new_with_argsz(vfio_region_info_bytes as u32);
        vriwc.vfio_info_cap_header_at_offset((vfio_region_info_bytes + 1) as u32);
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
}
