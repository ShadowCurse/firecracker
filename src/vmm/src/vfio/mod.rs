/// bindings
pub mod bindings;
/// ioctls
pub mod ioctls;

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;

pub use bindings::*;
pub use ioctls::*;
use kvm_bindings::{
    KVM_DEV_VFIO_FILE, kvm_create_device, kvm_device_attr, kvm_device_type_KVM_DEV_TYPE_VFIO,
};
use kvm_ioctls::{DeviceFd, VmFd};
use log::info;

fn vfio_container_open() -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/vfio/vfio")
        .unwrap()
}
fn vfio_container_check_api_version(container: &impl AsRawFd) {
    let version = crate::vfio::ioctls::ioctls::check_api_version(container);
    info!("container version: {}", version);
    if version as u32 != VFIO_API_VERSION {
        panic!("Vfio api version");
    }
}
fn vfio_container_check_extension(container: &impl AsRawFd, val: u32) {
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

// fn vfio_device_region_get_mappings(
//     &self,
//     region: &mut VfioRegion,
//     region_info: &vfio_region_info,
// ) -> Result<()> {
//     let region_info_size: u32 = std::mem::size_of::<vfio_region_info>() as u32;
//     if region_info.flags & VFIO_REGION_INFO_FLAG_CAPS == 0 || region_info.argsz <=
// region_info_size     {
//         // There is not capabilities information for that region, we can just return.
//         return Ok(());
//     }
//
//     // There is a capability information for that region, we have to call
//     // VFIO_DEVICE_GET_REGION_INFO with a vfio_region_with_cap structure and the hinted size.
//     let mut region_with_cap = vfio_region_info_with_cap::from_region_info(region_info);
//     crate::vfio::get_device_region_info_cap(self, &mut region_with_cap).unwrap();
//
//     // region_with_cap[0] may contain different types of structure depending on the capability
//     // type, but all of them begin with vfio_info_cap_header in order to identify the capability
//     // type, version and if there's another capability after this one.
//     // It is safe to convert region_with_cap[0] with an offset of cap_offset into
//     // vfio_info_cap_header pointer and access its elements, as long as cap_offset is greater
//     // than region_info_size.
//     //
//     // Safety: following code is safe because we trust data returned by the kernel.
//     if region_with_cap[0].region_info.cap_offset >= region_info_size {
//         let mut next_cap_offset = region_with_cap[0].region_info.cap_offset;
//         let info_ptr = &region_with_cap[0] as *const vfio_region_info_with_cap as *const u8;
//
//         while next_cap_offset >= region_info_size {
//             // SAFETY: data structure returned by kernel is trusted.
//             let cap_header = unsafe {
//                 *(info_ptr.offset(next_cap_offset as isize) as *const vfio_info_cap_header)
//             };
//
//             match u32::from(cap_header.id) {
//                 VFIO_REGION_INFO_CAP_SPARSE_MMAP => {
//                     // SAFETY: data structure returned by kernel is trusted.
//                     let sparse_mmap = unsafe {
//                         info_ptr.offset(next_cap_offset as isize)
//                             as *const vfio_region_info_cap_sparse_mmap
//                     };
//                     // SAFETY: data structure returned by kernel is trusted.
//                     let nr_areas = unsafe { (*sparse_mmap).nr_areas };
//                     // SAFETY: data structure returned by kernel is trusted.
//                     let areas = unsafe { (*sparse_mmap).areas.as_slice(nr_areas as usize) };
//
//                     let cap = VfioRegionInfoCapSparseMmap {
//                         areas: areas
//                             .iter()
//                             .map(|a| VfioRegionSparseMmapArea {
//                                 offset: a.offset,
//                                 size: a.size,
//                             })
//                             .collect(),
//                     };
//                     region.caps.push(VfioRegionInfoCap::SparseMmap(cap));
//                 }
//                 VFIO_REGION_INFO_CAP_TYPE => {
//                     // SAFETY: data structure returned by kernel is trusted.
//                     let type_ = unsafe {
//                         *(info_ptr.offset(next_cap_offset as isize)
//                             as *const vfio_region_info_cap_type)
//                     };
//                     let cap = VfioRegionInfoCapType {
//                         type_: type_.type_,
//                         subtype: type_.subtype,
//                     };
//                     region.caps.push(VfioRegionInfoCap::Type(cap));
//                 }
//                 VFIO_REGION_INFO_CAP_MSIX_MAPPABLE => {
//                     region.caps.push(VfioRegionInfoCap::MsixMappable);
//                 }
//                 VFIO_REGION_INFO_CAP_NVLINK2_SSATGT => {
//                     // SAFETY: data structure returned by kernel is trusted.
//                     let nvlink2_ssatgt = unsafe {
//                         *(info_ptr.offset(next_cap_offset as isize)
//                             as *const vfio_region_info_cap_nvlink2_ssatgt)
//                     };
//                     let cap = VfioRegionInfoCapNvlink2Ssatgt {
//                         tgt: nvlink2_ssatgt.tgt,
//                     };
//                     region.caps.push(VfioRegionInfoCap::Nvlink2Ssatgt(cap));
//                 }
//                 VFIO_REGION_INFO_CAP_NVLINK2_LNKSPD => {
//                     // SAFETY: data structure returned by kernel is trusted.
//                     let nvlink2_lnkspd = unsafe {
//                         *(info_ptr.offset(next_cap_offset as isize)
//                             as *const vfio_region_info_cap_nvlink2_lnkspd)
//                     };
//                     let cap = VfioRegionInfoCapNvlink2Lnkspd {
//                         link_speed: nvlink2_lnkspd.link_speed,
//                     };
//                     region.caps.push(VfioRegionInfoCap::Nvlink2Lnkspd(cap));
//                 }
//                 _ => {}
//             }
//
//             next_cap_offset = cap_header.next;
//         }
//     }
//
//     Ok(())
// }
//
// fn vfio_device_get_regions(
//     device: &impl AsRawFd,
//     device_info: &vfio_device_info,
// ) -> Vec<vfio_region_info> {
//     let mut regions: Vec<vfio_region_info> = Vec::new();
//
//     for i in VFIO_PCI_BAR0_REGION_INDEX..device_info.num_regions {
//         let argsz: u32 = std::mem::size_of::<vfio_region_info>() as u32;
//         let mut reg_info = vfio_region_info {
//             argsz,
//             flags: 0,
//             index: i,
//             cap_offset: 0,
//             size: 0,
//             offset: 0,
//         };
//
//         if let Err(e) = crate::vfio::get_device_region_info(self, &mut reg_info) {
//             match e {
//                 // Non-VGA devices do not have the VGA region,
//                 // the kernel indicates this by returning -EINVAL,
//                 // and it's not an error.
//                 VfioError::VfioDeviceGetRegionInfo(e)
//                     if e.errno() == libc::EINVAL && i == VFIO_PCI_VGA_REGION_INDEX =>
//                 {
//                     continue;
//                 }
//                 _ => {
//                     error!("Could not get region #{i} info {e}");
//                     continue;
//                 }
//             }
//         }
//
//         let mut region = VfioRegion {
//             flags: reg_info.flags,
//             size: reg_info.size,
//             offset: reg_info.offset,
//             caps: Vec::new(),
//         };
//         if let Err(e) = self.get_region_map(&mut region, &reg_info) {
//             error!("Could not get region #{i} map {e}");
//             continue;
//         }
//
//         debug!("Region #{i}");
//         debug!("\tflag 0x{:x}", region.flags);
//         debug!("\tsize 0x{:x}", region.size);
//         debug!("\toffset 0x{:x}", region.offset);
//         regions.push(region);
//     }
//
//     Ok(regions)
// }

// pub fn vfio_device_region_read(&self, index: u32, buf: &mut [u8], addr: u64) {
//     let region: &VfioRegion = match self.regions.get(index as usize) {
//         Some(v) => v,
//         None => {
//             warn!("region read with invalid index: {index}");
//             return;
//         }
//     };
//
//     let size = buf.len() as u64;
//     if size > region.size || addr + size > region.size {
//         warn!("region read with invalid parameter, add: {addr}, size: {size}");
//         return;
//     }
//
//     if let Err(e) = self.device.read_exact_at(buf, region.offset + addr) {
//         warn!("Failed to read region in index: {index}, addr: {addr}, error: {e}");
//     }
// }

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
    let container = vfio_container_open();
    vfio_container_check_api_version(&container);
    vfio_container_check_extension(&container, VFIO_TYPE1v2_IOMMU);

    // open device and vfio group
    let path = "/sys/bus/mdev/devices/c9abdcb5-5279-413a-9057-c81d2605ce9c/".to_string();
    let group_id = group_id_from_device_path(&path);
    let group = vfio_group_open(group_id);
    vfio_group_check_status(&group);
    crate::vfio::group_set_container(&group, &container).unwrap();

    // only set after getting the first group
    vfio_container_set_iommu(&container, VFIO_TYPE1v2_IOMMU);

    let device_file = vfio_group_get_device(&group, &path);
    let device_info = vfio_device_get_info(&device_file);
    for i in VFIO_PCI_BAR0_REGION_INDEX..device_info.num_regions {
        info!("getting bar region info: {}", i);
        let region_info_struct_size = std::mem::size_of::<vfio_region_info>() as u32;
        let mut region_info = vfio_region_info {
            argsz: region_info_struct_size,
            flags: 0,
            index: i,
            cap_offset: 0,
            size: 0,
            offset: 0,
        };
        crate::vfio::device_get_region_info(&device_file, &mut region_info).unwrap();
        info!("Region info: {:#?}", region_info);
        if region_info.flags & VFIO_REGION_INFO_FLAG_CAPS == 0
            || region_info.argsz <= region_info_struct_size
        {
            info!("Region has no caps");
            continue;
        } else {
            info!("Region caps:");
            let mut region_info_with_cap_bytes =
                Vec::<u8>::with_capacity(region_info.argsz as usize);
            let region_info =
                unsafe { &mut *(region_info_with_cap_bytes.as_mut_ptr() as *mut vfio_region_info) };
            region_info.argsz = region_info.argsz;
            region_info.flags = 0;
            region_info.index = region_info.index;
            region_info.cap_offset = 0;
            region_info.size = 0;
            region_info.offset = 0;
            crate::vfio::device_get_region_info(&device_file, region_info);
            if region_info_struct_size <= region_info.cap_offset {
                let mut next_cap_offset = region_info.cap_offset;
                while (region_info_struct_size < next_cap_offset) {
                    let cap_header = unsafe {
                        &*(region_info_with_cap_bytes[next_cap_offset as usize..].as_ptr()
                            as *const vfio_info_cap_header)
                    };
                    info!("Cap id: {}", cap_header.id);
                    // match u32::from(cap_header.id) {
                    //     VFIO_REGION_INFO_CAP_SPARSE_MMAP => {
                    //     }
                    // }

                    next_cap_offset = cap_header.next;
                }
            }
        }
    }
    vfio_device_reset(&device_file, &device_info);

    // KVM part
    // let kvm_vfio_fd = create_kvm_vfio_device(vm_fd);
    // kvm_vfio_device_file_add(&kvm_vfio_fd, &group, KVM_DEV_VFIO_FILE_ADD);
    panic!("THE END");

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
