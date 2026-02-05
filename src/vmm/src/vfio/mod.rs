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
use std::sync::{Arc, Barrier, Mutex};

pub use bindings::*;
pub use ioctls::*;
use kvm_bindings::{
    KVM_DEV_VFIO_FILE, kvm_create_device, kvm_device_attr, kvm_device_type_KVM_DEV_TYPE_VFIO,
    kvm_userspace_memory_region,
};
use kvm_ioctls::{DeviceFd, VmFd};
use pci::{PciBdf, PciCapabilityId, PciClassCode, PciSubclass};
use vm_allocator::AllocPolicy;
use zerocopy::IntoBytes;

use crate::Vm;
use crate::devices::virtio::transport::pci::device::VirtioInterruptMsix;
use crate::pci::configuration::PciConfiguration;
use crate::pci::msix::MsixConfig;
use crate::pci::{BarReprogrammingParams, DeviceRelocationError, PciDevice};
use crate::vstate::resources::ResourceAllocator;

pub fn vfio_open() -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/vfio/vfio")
        .unwrap()
}
pub fn vfio_check_api_version(container: &impl AsRawFd) {
    let version = crate::vfio::ioctls::ioctls::check_api_version(container);
    println!("vfio api version: {}", version);
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

pub fn vfio_device_get_region_infos(
    device: &impl AsRawFd,
    device_info: &vfio_device_info,
) -> Vec<VfioRegionInfo> {
    let num_regions = device_info.num_regions - VFIO_PCI_BAR0_REGION_INDEX;
    let mut regions = Vec::with_capacity(num_regions as usize);

    for i in VFIO_PCI_BAR0_REGION_INDEX..device_info.num_regions {
        println!("Getting bar region info: {}", i);
        let region_info_struct_size = std::mem::size_of::<vfio_region_info>() as u32;
        let mut region_info = vfio_region_info {
            argsz: region_info_struct_size,
            flags: 0,
            index: i,
            cap_offset: 0,
            size: 0,
            offset: 0,
        };
        crate::vfio::device_get_region_info(device, &mut region_info).unwrap();
        println!("Region info: {:?}", region_info);
        println!("Flags: ");
        println!(
            "VFIO_REGION_INFO_FLAG_READ: {}",
            region_info.flags & VFIO_REGION_INFO_FLAG_READ != 0
        );
        println!(
            "VFIO_REGION_INFO_FLAG_WRITE: {}",
            region_info.flags & VFIO_REGION_INFO_FLAG_WRITE != 0
        );
        println!(
            "VFIO_REGION_INFO_FLAG_MMAP: {}",
            region_info.flags & VFIO_REGION_INFO_FLAG_MMAP != 0
        );
        println!(
            "VFIO_REGION_INFO_FLAG_CAPS: {}",
            region_info.flags & VFIO_REGION_INFO_FLAG_CAPS != 0
        );
        let mut caps = Vec::new();
        if region_info.flags & VFIO_REGION_INFO_FLAG_CAPS == 0
            || region_info.argsz <= region_info_struct_size
        {
            println!("Region has no caps");
        } else {
            println!("Region caps:");
            let mut region_info_with_cap_bytes =
                Vec::<u8>::with_capacity(region_info.argsz as usize);
            let region_info_with_caps =
                unsafe { &mut *(region_info_with_cap_bytes.as_mut_ptr() as *mut vfio_region_info) };
            region_info_with_caps.argsz = region_info.argsz;
            region_info_with_caps.flags = 0;
            region_info_with_caps.index = region_info.index;
            region_info_with_caps.cap_offset = 0;
            region_info_with_caps.size = 0;
            region_info_with_caps.offset = 0;
            crate::vfio::device_get_region_info(device, region_info_with_caps).unwrap();
            if region_info_struct_size <= region_info_with_caps.cap_offset {
                let mut next_cap_offset = region_info_with_caps.cap_offset;
                while region_info_struct_size < next_cap_offset {
                    let cap_header = unsafe {
                        &*(region_info_with_cap_bytes[next_cap_offset as usize..].as_ptr()
                            as *const vfio_info_cap_header)
                    };
                    println!("Cap id: {}", cap_header.id);
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
                            println!("Got unknown region capability id: {}", cap_header.id);
                        }
                    }
                    next_cap_offset = cap_header.next;
                }
            }
        }
        let region_info = VfioRegionInfo {
            flags: region_info.flags,
            index: region_info.index,
            size: region_info.size,
            offset: region_info.offset,
            caps,
        };
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
        println!("Getting irq info: {}", i);
        let mut irq_info = vfio_irq_info {
            argsz: std::mem::size_of::<vfio_irq_info>() as u32,
            flags: 0,
            index: i,
            count: 0,
        };
        match crate::vfio::device_get_irq_info(device, &mut irq_info) {
            Ok(()) => {
                println!("Irq info: {:?}", irq_info);
                println!(
                    "VFIO_IRQ_INFO_EVENTFD: {}",
                    irq_info.flags & VFIO_IRQ_INFO_EVENTFD != 0
                );
                println!(
                    "VFIO_IRQ_INFO_MASKABLE  :{}",
                    irq_info.flags & VFIO_IRQ_INFO_MASKABLE != 0
                );
                println!(
                    "VFIO_IRQ_INFO_AUTOMASKED  :{}",
                    irq_info.flags & VFIO_IRQ_INFO_AUTOMASKED != 0
                );
                println!(
                    "VFIO_IRQ_INFO_NORESIZE  :{}",
                    irq_info.flags & VFIO_IRQ_INFO_NORESIZE != 0
                );
                irqs.push(irq_info);
            }
            Err(e) => println!("Irq info: got error: {:?}", e),
        }
    }
    irqs
}

pub fn vfio_device_get_pci_capabilities(
    device: &impl FileExt,
    region_infos: &[VfioRegionInfo],
    irq_infos: &[vfio_irq_info],
) {
    let mut cap_offset: u32 = 0;
    vfio_device_region_read(
        device,
        region_infos,
        VFIO_PCI_CONFIG_REGION_INDEX,
        PCI_CONFIG_CAPABILITY_OFFSET as u64,
        cap_offset.as_mut_bytes(),
    );

    // let mut pci_express_cap_found = false;
    // let mut power_management_cap_found = false;

    println!("PCI CAPS offset: {}", cap_offset);
    // let mut caps = Vec::new();
    while cap_offset != 0 {
        let mut cap_id: u8 = 0;
        vfio_device_region_read(
            device,
            region_infos,
            VFIO_PCI_CONFIG_REGION_INDEX,
            cap_offset as u64,
            cap_id.as_mut_bytes(),
        );

        match PciCapabilityId::from(cap_id) {
            PciCapabilityId::MessageSignalledInterrupts => {
                if (VFIO_PCI_MSI_IRQ_INDEX as usize) < irq_infos.len() {
                    let irq_info = irq_infos[VFIO_PCI_MSI_IRQ_INDEX as usize];
                    if 0 < irq_info.count {
                        println!("Found MSI cap");
                    }
                }
                // if let Some(irq_info) = self.vfio_wrapper.get_irq_info(VFIO_PCI_MSI_IRQ_INDEX) {
                //     if irq_info.count > 0 {
                //         // Parse capability only if the VFIO device
                //         // supports MSI.
                //         let msg_ctl = self.parse_msi_capabilities(cap_iter);
                //         self.initialize_msi(msg_ctl, cap_iter as u32, None);
                //     }
                // }
            }
            PciCapabilityId::MsiX => {
                if (VFIO_PCI_MSIX_IRQ_INDEX as usize) < irq_infos.len() {
                    let irq_info = irq_infos[VFIO_PCI_MSIX_IRQ_INDEX as usize];
                    if 0 < irq_info.count {
                        println!("Found MSIX cap");
                    }
                }
                // if let Some(irq_info) = self.vfio_wrapper.get_irq_info(VFIO_PCI_MSIX_IRQ_INDEX) {
                //     if irq_info.count > 0 {
                //         // Parse capability only if the VFIO device
                //         // supports MSI-X.
                //         let msix_cap = self.parse_msix_capabilities(cap_iter);
                //         self.initialize_msix(msix_cap, cap_iter as u32, bdf, None);
                //     }
                // }
            }
            // PciCapabilityId::PciExpress => pci_express_cap_found = true,
            // PciCapabilityId::PowerManagement => power_management_cap_found = true,
            _ => {}
        };

        vfio_device_region_read(
            device,
            region_infos,
            VFIO_PCI_CONFIG_REGION_INDEX,
            (cap_offset as u64) + 1,
            cap_offset.as_mut_bytes(),
        );
    }

    // if let Some(clique_id) = self.x_nv_gpudirect_clique {
    //     self.add_nv_gpudirect_clique_cap(cap_iter, clique_id);
    // }
    //
    // if pci_express_cap_found && power_management_cap_found {
    //     self.parse_extended_capabilities();
    // }
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
                "Failed to read from region at index: {index}, offset: {offset:#x}, region size: \
                 {:#x} error: {e}",
                region_info.size
            );
        }
    } else {
        panic!(
            "Failed to read from region at index: {index}, offset: {offset:#x}, region size: \
             {:#x} error: read beyond region memory",
            region_info.size
        );
    }
    println!("Reading from device region {index} at offset: {offset}: {buf:?}");
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
                "Failed to write to region at index: {index}, offset: {offset:#x}, region size: \
                 {:#x} error: {e}",
                region_info.size
            );
        }
    } else {
        panic!(
            "Failed to write to region at index: {index}, offset: {offset:#x}, region size: {:#x} \
             error: write beyond region memory",
            region_info.size
        );
    }
    println!("Writing into device region {index} at offset: {offset}: {buf:?}");
}

#[derive(Debug)]
pub struct BarInfo {
    pub idx: u32,
    pub gpa: u64,
    pub size: u64,
    pub is_64_bits: bool,
    pub is_prefetchable: bool,
}
pub fn device_get_bar_infos(
    device: &impl FileExt,
    region_infos: &[VfioRegionInfo],
    resource_allocator: &mut ResourceAllocator,
) -> Vec<BarInfo> {
    let mut bar_infos = Vec::new();
    let mut bar_idx = VFIO_PCI_BAR0_REGION_INDEX;
    while bar_idx < VFIO_PCI_CONFIG_REGION_INDEX {
        println!("Gettig BAR{bar_idx} info");
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

        // // Is this an IO BAR?
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
        println!("BAR: {bar_idx} lower_size: {lower_size:#x}");

        let mut size = 0;
        if is_io_bar {
            size = u64::from(lower_size);
            lower_size &= !0b11;
        } else if is_64_bits {
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
            println!("BAR: {bar_idx} upper_size: {upper_size:#x}");

            size = u64::from(upper_size) << 32 | u64::from(lower_size);
            size &= !0b1111;
        } else {
            size = u64::from(lower_size);
            size &= !0b1111;
        }
        size = !size + 1;
        println!("BAR size: {size:#x}");
        if size != 0 {
            let idx = bar_idx;
            let mut gpa = 0;
            if is_io_bar {
                println!("Skipping IO bar with size: {size:#x}");
                bar_idx += 1;
                continue;
            } else if is_64_bits {
                // allocate 32bit guest address
                gpa = resource_allocator
                    .mmio32_memory
                    .allocate(size, 64, AllocPolicy::FirstMatch)
                    .unwrap()
                    .start();
                bar_idx += 2;
            } else {
                // allocate 64bit guest address
                gpa = resource_allocator
                    .mmio64_memory
                    .allocate(size, 64, AllocPolicy::FirstMatch)
                    .unwrap()
                    .start();
                bar_idx += 1;
            }
            println!(
                "Placing device BAR into guest with guest addr: {gpa:#x} size: {size:#x} 64bits: \
                 {is_64_bits} prefetchable: {is_prefetchable}",
            );
            bar_infos.push(BarInfo {
                idx,
                gpa,
                size,
                is_64_bits,
                is_prefetchable,
            });
        } else {
            println!(
                "Zero device BAR: size:{size:#x} 64bits: {is_64_bits} prefetchable: \
                 {is_prefetchable}",
            );
        }
    }
    bar_infos
}

pub fn get_group_and_device_with_info(group: &impl AsRawFd, path: &str) -> VfioPciDevice {
    let device_file = vfio_group_get_device(group, &path);
    let device_info = vfio_device_get_info(&device_file);
    vfio_device_reset(&device_file, &device_info);

    let device_region_infos = vfio_device_get_region_infos(&device_file, &device_info);

    println!("Getting PCI caps");
    let mut pci_cap_offset: u8 = 0;
    vfio_device_region_read(
        &device_file,
        &device_region_infos,
        VFIO_PCI_CONFIG_REGION_INDEX,
        PCI_CONFIG_CAPABILITY_OFFSET as u64,
        pci_cap_offset.as_mut_bytes(),
    );
    println!("PCI cap offset: {}", pci_cap_offset);
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
        println!("Pci cap found: {:?}", pci_cap);
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
        println!(
            "INTX IRQ info: {:?}",
            device_irq_infos[VFIO_PCI_INTX_IRQ_INDEX as usize]
        );
    }
    if VFIO_PCI_MSI_IRQ_INDEX < device_irq_infos.len() as u32 {
        println!(
            "MSI IRQ info: {:?}",
            device_irq_infos[VFIO_PCI_MSI_IRQ_INDEX as usize]
        );
    }
    if VFIO_PCI_MSIX_IRQ_INDEX < device_irq_infos.len() as u32 {
        println!(
            "MSIX IRQ info: {:?}",
            device_irq_infos[VFIO_PCI_MSIX_IRQ_INDEX as usize]
        );
    }

    VfioPciDevice {
        device_file,
        device_info,
        device_region_infos,
        device_irq_infos,
    }
}

#[derive(Debug)]
pub struct VfioPciDevice {
    pub device_file: File,
    pub device_info: vfio_device_info,
    pub device_region_infos: Vec<VfioRegionInfo>,
    pub device_irq_infos: Vec<vfio_irq_info>,
    // id: String,
    // pci_device_bdf: PciBdf,
    // configuration: PciConfiguration,
    // virtio_interrupt: Option<Arc<VirtioInterruptMsix>>,
    // // Allocated address for the BAR
    // pub bar_address: u64,
}

pub fn mmap_bars(
    container: &File,
    device: &File,
    bar_infos: &[BarInfo],
    region_infos: &[VfioRegionInfo],
    vm: &VmFd,
) {
    for bar_info in bar_infos.iter() {
        let region_info = &region_infos[bar_info.idx as usize];
        if region_info.flags & VFIO_REGION_INFO_FLAG_CAPS != 0 {
            let mut has_msix_mappable = false;
            let mut sparce_mmap_cap = None;
            for cap in region_info.caps.iter() {
                match cap {
                    VfioRegionCap::SparseMmap(cap) => sparce_mmap_cap = Some(cap),
                    VfioRegionCap::MsixMappable => has_msix_mappable = true,
                    _ => {}
                }
            }
            let contain_msix_table = region_info.index == 0;
            let contain_msix_pba = region_info.index == 0;
            if (contain_msix_table || contain_msix_pba)
                && !has_msix_mappable
                && sparce_mmap_cap.is_none()
            {
                // continue;
            } else {
                let can_mmap = region_info.flags & VFIO_REGION_INFO_FLAG_MMAP != 0;
                if can_mmap || sparce_mmap_cap.is_some() {
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

                    let areas: &[VfioRegionSparseMmapArea] = if let Some(cap) = sparce_mmap_cap {
                        &cap.areas
                    } else if has_msix_mappable {
                        let msix_table_offset = 0;
                        let msix_table_size = 0;
                        if contain_msix_table {
                            // align this down to page boundary
                            let msix_table_offset = 0;
                            // align this up to page boundary
                            let msix_table_size = 4096;
                        }
                        let msix_pba_offset = 0;
                        let msix_pba_size = 0;
                        if contain_msix_pba {
                            let msix_pba_offset = 4096;
                            let msix_pba_size = 4096;
                        }
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
                            // error!(
                            //     "Could not mmap sparse area (offset = 0x{:x}, size = 0x{:x}):
                            // {}",     area.offset,
                            //     area.size,
                            //     std::io::Error::last_os_error()
                            // );
                            // return Err(VfioPciError::MmapArea);
                        }

                        // if !is_page_size_aligned(area.size) || !is_page_size_aligned(area.offset)
                        // {     warn!(
                        //         "Could not mmap sparse area that is not page size aligned (offset
                        // \          = 0x{:x}, size = 0x{:x})",
                        //         area.offset, area.size,
                        //     );
                        //     return Ok(());
                        // }

                        // let user_memory_region = UserMemoryRegion {
                        //     slot: (self.memory_slot)(),
                        //     start: region.start.0 + area.offset,
                        //     size: area.size,
                        //     host_addr: host_addr as u64,
                        // };
                        //
                        // region.user_memory_regions.push(user_memory_region);
                        //
                        // let mem_region = VfioCommon::make_user_memory_region(
                        //     user_memory_region.slot,
                        //     user_memory_region.start,
                        //     user_memory_region.size,
                        //     user_memory_region.host_addr,
                        //     false,
                        //     false,
                        // );

                        let iova = bar_info.gpa + area.offset;
                        let size = area.size;
                        let host_addr = host_addr as u64;

                        let kvm_memory_region = kvm_userspace_memory_region {
                            slot: 1,
                            flags: 1,
                            guest_phys_addr: iova,
                            memory_size: size,
                            userspace_addr: host_addr,
                        };
                        unsafe {
                            vm.set_user_memory_region(kvm_memory_region).unwrap();
                        }

                        // TODO: if virtual_iommu is attached this is not needed
                        if true {
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
        }
    }
}

// #[derive(Debug)]
// pub struct VfioPciDevice {
// }
// #[derive(Copy, Clone)]
// enum PciVfioSubclass {
//     VfioSubclass = 0xff,
// }
//
// impl PciSubclass for PciVfioSubclass {
//     fn get_register_value(&self) -> u8 {
//         *self as u8
//     }
// }
//
// impl VfioPciDevice {
//     pub fn new(id: String, bdf: PciBdf, vm: &Arc<Vm>) -> Self {
//         let msix_num = 1;
//         let msix_vectors = Vm::create_msix_group(vm.clone(), msix_num).unwrap();
//         let msix_config = Arc::new(Mutex::new(MsixConfig::new(
//             msix_vectors.clone(),
//             bdf.into(),
//         )));
//         let c = PciConfiguration::new_type0(
//             0,
//             0,
//             0,
//             PciClassCode::Other,
//             &PciVfioSubclass::VfioSubclass,
//             0,
//             0,
//             Some(msix_config.clone()),
//         );
//     }
// }
//
impl PciDevice for VfioPciDevice {
    fn write_config_register(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> Option<Arc<Barrier>> {
        None
    }
    fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        0
    }
    fn detect_bar_reprogramming(
        &mut self,
        _reg_idx: usize,
        _data: &[u8],
    ) -> Option<BarReprogrammingParams> {
        None
    }
    fn read_bar(&mut self, _base: u64, _offset: u64, _data: &mut [u8]) {}
    fn write_bar(&mut self, _base: u64, _offset: u64, _data: &[u8]) -> Option<Arc<Barrier>> {
        None
    }
    fn move_bar(&mut self, _old_base: u64, _new_base: u64) -> Result<(), DeviceRelocationError> {
        Ok(())
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
// pub fn do_vfio_magic(vm_fd: &VmFd, paths: &Vec<String>) {
pub fn do_vfio_magic(path: &str) {
    // vfio part
    let container = vfio_open();
    vfio_check_api_version(&container);
    vfio_check_extension(&container, VFIO_TYPE1v2_IOMMU);

    // open device and vfio group
    // let path = "/sys/bus/mdev/devices/c9abdcb5-5279-413a-9057-c81d2605ce9c/".to_string();
    println!("Openning device at path: {}", path);
    let group_id = group_id_from_device_path(&(path.to_string()));
    println!("Group id: {}", group_id);
    let group = vfio_group_open(group_id);
    vfio_group_check_status(&group);
    crate::vfio::group_set_container(&group, &container).unwrap();

    // only set after getting the first group
    vfio_container_set_iommu(&container, VFIO_TYPE1v2_IOMMU);

    println!("Getting device with info");
    let device = crate::vfio::get_group_and_device_with_info(&group, path);
    let mut resource_allocator = ResourceAllocator::new();
    println!("Getting BAR infos");
    let bar_infos = crate::vfio::device_get_bar_infos(
        &device.device_file,
        &device.device_region_infos,
        &mut resource_allocator,
    );
    println!("Getting PCI caps");
    vfio_device_get_pci_capabilities(
        &device.device_file,
        &device.device_region_infos,
        &device.device_irq_infos,
    );

    // KVM part
    // let kvm_vfio_fd = create_kvm_vfio_device(vm_fd);
    // kvm_vfio_device_file_add(&kvm_vfio_fd, &group, KVM_DEV_VFIO_FILE_ADD);
    // panic!("THE END");

    // for path in paths.iter() {
    //     println!("vfio path: {}", path);
    // }
}
