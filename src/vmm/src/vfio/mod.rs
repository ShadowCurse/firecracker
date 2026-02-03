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

pub use bindings::*;
pub use ioctls::*;
use kvm_bindings::{
    KVM_DEV_VFIO_FILE, kvm_create_device, kvm_device_attr, kvm_device_type_KVM_DEV_TYPE_VFIO,
};
use kvm_ioctls::{DeviceFd, VmFd};
use pci::PciCapabilityId;

fn vfio_open() -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/vfio/vfio")
        .unwrap()
}
fn vfio_check_api_version(container: &impl AsRawFd) {
    let version = crate::vfio::ioctls::ioctls::check_api_version(container);
    println!("vfio api version: {}", version);
    if version as u32 != VFIO_API_VERSION {
        panic!("Vfio api version");
    }
}
fn vfio_check_extension(container: &impl AsRawFd, val: u32) {
    if val != VFIO_TYPE1_IOMMU && val != VFIO_TYPE1v2_IOMMU {
        panic!();
    }
    let ret = crate::vfio::check_extension(container, val).unwrap();
    if ret != 1 {
        panic!();
    }
}
fn group_id_from_device_path(device_path: &impl AsRef<Path>) -> u32 {
    let uuid_path: std::path::PathBuf = device_path.as_ref().join("iommu_group");
    let group_path = uuid_path.read_link().unwrap();
    let group_osstr = group_path.file_name().unwrap();
    let group_str = group_osstr.to_str().unwrap();
    group_str.parse::<u32>().unwrap()
}
fn vfio_group_open(id: u32) -> File {
    let group_path = Path::new("/dev/vfio").join(id.to_string());
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(group_path)
        .unwrap()
}
fn vfio_group_check_status(group: &impl AsRawFd) {
    let mut group_status = vfio_group_status {
        argsz: std::mem::size_of::<vfio_group_status>() as u32,
        flags: 0,
    };
    crate::vfio::group_get_status(group, &mut group_status).unwrap();
    if group_status.flags != VFIO_GROUP_FLAGS_VIABLE {
        panic!();
    }
}
fn vfio_container_set_iommu(container: &impl AsRawFd, val: u32) {
    assert!(val == VFIO_TYPE1_IOMMU || val == VFIO_TYPE1v2_IOMMU);
    crate::vfio::ioctls::set_iommu(container, val).unwrap();
}

fn vfio_group_get_device(group: &impl AsRawFd, path: &impl AsRef<Path>) -> File {
    let uuid_osstr = path.as_ref().file_name().unwrap();
    let uuid_str = uuid_osstr.to_str().unwrap();
    let path = CString::new(uuid_str.as_bytes()).unwrap();
    let device = crate::vfio::group_get_device_fd(group, &path).unwrap();
    device
}
fn vfio_device_get_info(device: &impl AsRawFd) -> vfio_device_info {
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
fn vfio_device_reset(device: &impl AsRawFd, device_info: &vfio_device_info) {
    if device_info.flags & VFIO_DEVICE_FLAGS_RESET != 0 {
        crate::vfio::device_reset(device);
    }
}
fn vfio_device_get_region_infos(
    device: &impl AsRawFd,
    device_info: &vfio_device_info,
) -> Vec<vfio_region_info> {
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
        println!("Region info: {:#?}", region_info);
        regions.push(region_info);
        if region_info.flags & VFIO_REGION_INFO_FLAG_CAPS == 0
            || region_info.argsz <= region_info_struct_size
        {
            println!("Region has no caps");
            continue;
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
                while (region_info_struct_size < next_cap_offset) {
                    let cap_header = unsafe {
                        &*(region_info_with_cap_bytes[next_cap_offset as usize..].as_ptr()
                            as *const vfio_info_cap_header)
                    };
                    println!("Cap id: {}", cap_header.id);
                    next_cap_offset = cap_header.next;
                }
            }
        }
    }
    regions
}
fn vfio_device_get_irq_infos(
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
                println!("Irq info: {:#?}", irq_info);
                irqs.push(irq_info);
            }
            Err(e) => println!("Irq info: got error: {:#?}", e),
        }
    }
    irqs
}
fn vfio_device_region_read(
    device: &impl FileExt,
    region_infos: &[vfio_region_info],
    index: u32,
    offset: u64,
    buf: &mut [u8],
) {
    let region_info = region_infos[index as usize];
    println!(
        "Reading device region {index} at offset: {offset} with region info: {region_info:#?}"
    );
    let buf_size = buf.len() as u64;
    if offset + buf_size <= region_info.size {
        if let Err(e) = device.read_exact_at(buf, region_info.offset + offset) {
            println!("Failed to read region in index: {index}, offset: {offset}, error: {e}");
        }
    } else {
        println!(
            "Failed to read region in index: {index}, offset: {offset}, error: read beyond region \
             memory"
        );
    }
}

fn create_kvm_vfio_device(vm: &VmFd) -> DeviceFd {
    let mut vfio_dev = kvm_create_device {
        type_: kvm_device_type_KVM_DEV_TYPE_VFIO,
        fd: 0,
        flags: 0,
    };
    vm.create_device(&mut vfio_dev).unwrap()
}
// flags: KVM_DEV_VFIO_FILE_ADD or KVM_DEV_VFIO_FILE_DEL;
fn kvm_vfio_device_file_add(device: &DeviceFd, file: &impl AsRawFd, flags: u32) {
    let file_fd = file.as_raw_fd();
    let dev_attr = kvm_device_attr {
        flags: 0,
        group: KVM_DEV_VFIO_FILE,
        attr: flags as u64,
        addr: (&file_fd as *const i32) as u64,
    };
    device.set_device_attr(&dev_attr).unwrap();
}

// pub fn do_vfio_magic(vm_fd: &VmFd, paths: &Vec<String>) {
pub fn do_vfio_magic() {
    // vfio part
    let container = vfio_open();
    vfio_check_api_version(&container);
    vfio_check_extension(&container, VFIO_TYPE1v2_IOMMU);

    // open device and vfio group
    let path = "/sys/bus/mdev/devices/c9abdcb5-5279-413a-9057-c81d2605ce9c/".to_string();
    println!("Openning device at path: {}", path);
    let group_id = group_id_from_device_path(&path);
    println!("Group id: {}", group_id);
    let group = vfio_group_open(group_id);
    vfio_group_check_status(&group);
    crate::vfio::group_set_container(&group, &container).unwrap();

    // only set after getting the first group
    vfio_container_set_iommu(&container, VFIO_TYPE1v2_IOMMU);

    let device_file = vfio_group_get_device(&group, &path);
    let device_info = vfio_device_get_info(&device_file);
    vfio_device_reset(&device_file, &device_info);

    let device_region_infos = vfio_device_get_region_infos(&device_file, &device_info);
    let mut buffer: [u8; 1] = [0];
    vfio_device_region_read(
        &device_file,
        &device_region_infos,
        VFIO_PCI_CONFIG_REGION_INDEX,
        PCI_CONFIG_CAPABILITY_OFFSET as u64,
        &mut buffer,
    );
    let mut pci_cap_offset = buffer[0];
    println!("PCI cap offset: {}", pci_cap_offset);
    while pci_cap_offset != 0 {
        vfio_device_region_read(
            &device_file,
            &device_region_infos,
            VFIO_PCI_CONFIG_REGION_INDEX,
            pci_cap_offset as u64,
            &mut buffer,
        );
        let mut pci_cap_id = buffer[0];
        let pci_cap = PciCapabilityId::from(pci_cap_id);
        println!("Pci cap found: {:#?}", pci_cap);
        vfio_device_region_read(
            &device_file,
            &device_region_infos,
            VFIO_PCI_CONFIG_REGION_INDEX,
            (pci_cap_offset + 1) as u64,
            &mut buffer,
        );
        pci_cap_offset = buffer[0];
    }

    let device_irq_infos = vfio_device_get_irq_infos(&device_file, &device_info);
    if VFIO_PCI_MSI_IRQ_INDEX < device_irq_infos.len() as u32 {
        println!(
            "MSI IRQ info: {:#?}",
            device_irq_infos[VFIO_PCI_MSI_IRQ_INDEX as usize]
        );
    }
    if VFIO_PCI_MSIX_IRQ_INDEX < device_irq_infos.len() as u32 {
        println!(
            "MSIX IRQ info: {:#?}",
            device_irq_infos[VFIO_PCI_MSIX_IRQ_INDEX as usize]
        );
    }
    if VFIO_PCI_INTX_IRQ_INDEX < device_irq_infos.len() as u32 {
        println!(
            "INTX IRQ info: {:#?}",
            device_irq_infos[VFIO_PCI_INTX_IRQ_INDEX as usize]
        );
    }

    // KVM part
    // let kvm_vfio_fd = create_kvm_vfio_device(vm_fd);
    // kvm_vfio_device_file_add(&kvm_vfio_fd, &group, KVM_DEV_VFIO_FILE_ADD);
    // panic!("THE END");

    // let configuration = PciConfiguration::new(
    //     0,
    //     0,
    //     0,
    //     PciClassCode::Other,
    //     subclass,
    //     None,
    //     PciHeaderType::Device,
    //     0,
    //     0,
    //     None,
    //     pci_configuration_state,
    // );
    // vfio_common.parse_capabilities(bdf);
    // vfio_common.initialize_legacy_interrupt()?;

    // for path in paths.iter() {
    //     println!("vfio path: {}", path);
    // }
}
