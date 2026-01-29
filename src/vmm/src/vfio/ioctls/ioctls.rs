// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause
//
#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(missing_docs)]

use std::ffi::CStr;
use std::fs::File;
// use std::mem::size_of;
use std::os::unix::io::{AsRawFd, FromRawFd};

use vmm_sys_util::errno::Error as SysError;
use vmm_sys_util::ioctl::{
    ioctl, ioctl_with_mut_ref, ioctl_with_ptr, ioctl_with_ref, ioctl_with_val,
};
use vmm_sys_util::ioctl_io_nr;

use crate::vfio::bindings::vfio::*;
use crate::vfio::fam::vec_with_array_field;
use crate::vfio::ioctls::VfioError;
// use crate::vfio_device::{VfioDeviceInfo, vfio_region_info_with_cap};
// use crate::{Result, VfioContainer, VfioDevice, VfioError, VfioGroup};

ioctl_io_nr!(VFIO_GET_API_VERSION, VFIO_TYPE.into(), VFIO_BASE);
ioctl_io_nr!(VFIO_CHECK_EXTENSION, VFIO_TYPE.into(), VFIO_BASE + 1);
ioctl_io_nr!(VFIO_SET_IOMMU, VFIO_TYPE.into(), VFIO_BASE + 2);
ioctl_io_nr!(VFIO_GROUP_GET_STATUS, VFIO_TYPE.into(), VFIO_BASE + 3);
ioctl_io_nr!(VFIO_GROUP_SET_CONTAINER, VFIO_TYPE.into(), VFIO_BASE + 4);
ioctl_io_nr!(VFIO_GROUP_UNSET_CONTAINER, VFIO_TYPE.into(), VFIO_BASE + 5);
ioctl_io_nr!(VFIO_GROUP_GET_DEVICE_FD, VFIO_TYPE.into(), VFIO_BASE + 6);
ioctl_io_nr!(VFIO_DEVICE_GET_INFO, VFIO_TYPE.into(), VFIO_BASE + 7);
ioctl_io_nr!(VFIO_DEVICE_GET_REGION_INFO, VFIO_TYPE.into(), VFIO_BASE + 8);
ioctl_io_nr!(VFIO_DEVICE_GET_IRQ_INFO, VFIO_TYPE.into(), VFIO_BASE + 9);
ioctl_io_nr!(VFIO_DEVICE_SET_IRQS, VFIO_TYPE.into(), VFIO_BASE + 10);
ioctl_io_nr!(VFIO_DEVICE_RESET, VFIO_TYPE.into(), VFIO_BASE + 11);
ioctl_io_nr!(
    VFIO_DEVICE_GET_PCI_HOT_RESET_INFO,
    VFIO_TYPE.into(),
    VFIO_BASE + 12
);
ioctl_io_nr!(VFIO_DEVICE_PCI_HOT_RESET, VFIO_TYPE.into(), VFIO_BASE + 13);
ioctl_io_nr!(
    VFIO_DEVICE_QUERY_GFX_PLANE,
    VFIO_TYPE.into(),
    VFIO_BASE + 14
);
ioctl_io_nr!(VFIO_DEVICE_GET_GFX_DMABUF, VFIO_TYPE.into(), VFIO_BASE + 15);
ioctl_io_nr!(VFIO_DEVICE_IOEVENTFD, VFIO_TYPE.into(), VFIO_BASE + 16);
// #[cfg(feature = "vfio_cdev")]
// ioctl_io_nr!(VFIO_DEVICE_BIND_IOMMUFD, VFIO_TYPE.into(), VFIO_BASE + 18);
// #[cfg(feature = "vfio_cdev")]
// ioctl_io_nr!(
//     VFIO_DEVICE_ATTACH_IOMMUFD_PT,
//     VFIO_TYPE.into(),
//     VFIO_BASE + 19
// );
// #[cfg(feature = "vfio_cdev")]
// ioctl_io_nr!(
//     VFIO_DEVICE_DETACH_IOMMUFD_PT,
//     VFIO_TYPE.into(),
//     VFIO_BASE + 20
// );
ioctl_io_nr!(VFIO_IOMMU_GET_INFO, VFIO_TYPE.into(), VFIO_BASE + 12);
ioctl_io_nr!(VFIO_IOMMU_MAP_DMA, VFIO_TYPE.into(), VFIO_BASE + 13);
ioctl_io_nr!(VFIO_IOMMU_UNMAP_DMA, VFIO_TYPE.into(), VFIO_BASE + 14);
ioctl_io_nr!(VFIO_IOMMU_ENABLE, VFIO_TYPE.into(), VFIO_BASE + 15);
ioctl_io_nr!(VFIO_IOMMU_DISABLE, VFIO_TYPE.into(), VFIO_BASE + 16);

// Safety:
// - absolutely trust the underlying kernel
// - absolutely trust data returned by the underlying kernel
// - assume kernel will return error if caller passes in invalid file handle, parameter or buffer.
pub fn check_api_version(vfio: &impl AsRawFd) -> i32 {
    // SAFETY: file is vfio container fd and ioctl is defined by kernel.
    unsafe { ioctl(vfio, VFIO_GET_API_VERSION()) }
}

pub fn check_extension(container: &impl AsRawFd, val: u32) -> Result<u32, VfioError> {
    // SAFETY: file is vfio container and make sure val is valid.
    let ret = unsafe { ioctl_with_val(container, VFIO_CHECK_EXTENSION(), val.into()) };
    if ret < 0 {
        Err(VfioError::VfioExtension)
    } else {
        Ok(ret as u32)
    }
}

pub fn set_iommu(container: &impl AsRawFd, val: u32) -> Result<(), VfioError> {
    // SAFETY: file is vfio container and make sure val is valid.
    let ret = unsafe { ioctl_with_val(container, VFIO_SET_IOMMU(), val.into()) };
    if ret < 0 {
        Err(VfioError::ContainerSetIOMMU(SysError::last()))
    } else {
        Ok(())
    }
}

pub fn map_dma(
    container: &impl AsRawFd,
    dma_map: &vfio_iommu_type1_dma_map,
) -> Result<(), VfioError> {
    // SAFETY: file is vfio container, dma_map is constructed by us, and
    // we check the return value
    let ret = unsafe { ioctl_with_ref(container, VFIO_IOMMU_MAP_DMA(), dma_map) };
    if ret != 0 {
        Err(VfioError::IommuDmaMap(SysError::last()))
    } else {
        Ok(())
    }
}

pub fn unmap_dma(
    container: &impl AsRawFd,
    dma_map: &mut vfio_iommu_type1_dma_unmap,
) -> Result<(), VfioError> {
    // SAFETY: file is vfio container, dma_unmap is constructed by us, and
    // we check the return value
    let ret = unsafe { ioctl_with_ref(container, VFIO_IOMMU_UNMAP_DMA(), dma_map) };
    if ret != 0 {
        Err(VfioError::IommuDmaUnmap(SysError::last()))
    } else {
        Ok(())
    }
}

pub fn group_get_status(
    group: &impl AsRawFd,
    group_status: &mut vfio_group_status,
) -> Result<(), VfioError> {
    // SAFETY: we are the owner of group and group_status which are valid value.
    let ret = unsafe { ioctl_with_mut_ref(group, VFIO_GROUP_GET_STATUS(), group_status) };
    if ret < 0 {
        Err(VfioError::GetGroupStatus)
    } else {
        Ok(())
    }
}

pub fn group_get_device_fd(group: &impl AsRawFd, path: &CStr) -> Result<File, VfioError> {
    // SAFETY: we are the owner of self and path_ptr which are valid value.
    let fd = unsafe { ioctl_with_ptr(group, VFIO_GROUP_GET_DEVICE_FD(), path.as_ptr()) };
    if fd < 0 {
        Err(VfioError::GroupGetDeviceFD(SysError::last()))
    } else {
        // SAFETY: fd is valid FD
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

pub fn group_set_container(
    group: &impl AsRawFd,
    container: &impl AsRawFd,
) -> Result<(), VfioError> {
    // SAFETY: we are the owner of group and container_raw_fd which are valid value,
    // and we verify the ret value
    let ret = unsafe { ioctl_with_ref(group, VFIO_GROUP_SET_CONTAINER(), container) };
    if ret < 0 {
        Err(VfioError::GroupSetContainer)
    } else {
        Ok(())
    }
}

pub fn group_unset_container(
    group: &impl AsRawFd,
    container: &impl AsRawFd,
) -> Result<(), VfioError> {
    // SAFETY: we are the owner of self and container_raw_fd which are valid value.
    let ret = unsafe { ioctl_with_ref(group, VFIO_GROUP_UNSET_CONTAINER(), container) };
    if ret < 0 {
        Err(VfioError::GroupSetContainer)
    } else {
        Ok(())
    }
}

pub fn device_get_info(
    device: &impl AsRawFd,
    dev_info: &mut vfio_device_info,
) -> Result<(), VfioError> {
    // SAFETY: we are the owner of dev and dev_info which are valid value,
    // and we verify the return value.
    let ret = unsafe { ioctl_with_mut_ref(device, VFIO_DEVICE_GET_INFO(), dev_info) };
    if ret < 0 {
        Err(VfioError::VfioDeviceGetInfo(SysError::last()))
    } else {
        Ok(())
    }
}

pub fn device_set_irqs(device: &impl AsRawFd, irq_set: &[vfio_irq_set]) -> Result<(), VfioError> {
    if irq_set.is_empty() || irq_set[0].argsz as usize > std::mem::size_of_val(irq_set) {
        Err(VfioError::VfioDeviceSetIrq)
    } else {
        // SAFETY: we are the owner of self and irq_set which are valid value
        let ret = unsafe { ioctl_with_ref(device, VFIO_DEVICE_SET_IRQS(), &irq_set[0]) };
        if ret < 0 {
            Err(VfioError::VfioDeviceSetIrq)
        } else {
            Ok(())
        }
    }
}

pub fn device_reset(device: &impl AsRawFd) -> i32 {
    // SAFETY: file is vfio device
    unsafe { ioctl(device, VFIO_DEVICE_RESET()) }
}

pub fn device_get_irq_info(
    device: &impl AsRawFd,
    irq_info: &mut vfio_irq_info,
) -> Result<(), VfioError> {
    // SAFETY: we are the owner of dev and irq_info which are valid value
    let ret = unsafe { ioctl_with_mut_ref(device, VFIO_DEVICE_GET_IRQ_INFO(), irq_info) };
    if ret < 0 {
        Err(VfioError::VfioDeviceGetRegionInfo(SysError::last()))
    } else {
        Ok(())
    }
}

pub fn device_get_region_info(
    device: &impl AsRawFd,
    reg_info: &mut vfio_region_info,
) -> Result<(), VfioError> {
    // SAFETY: we are the owner of dev and region_info which are valid value
    // and we verify the return value.
    let ret = unsafe { ioctl_with_mut_ref(device, VFIO_DEVICE_GET_REGION_INFO(), reg_info) };
    if ret < 0 {
        Err(VfioError::VfioDeviceGetRegionInfo(SysError::last()))
    } else {
        Ok(())
    }
}

// #[repr(C)]
// #[derive(Debug, Default)]
// // A VFIO region structure with an incomplete array for region
// // capabilities information.
// //
// // When the VFIO_DEVICE_GET_REGION_INFO ioctl returns with
// // VFIO_REGION_INFO_FLAG_CAPS flag set, it also provides the size of the region
// // capabilities information. This is a kernel hint for us to fetch this
// // information by calling the same ioctl, but with the argument size set to
// // the region plus the capabilities information array length. The kernel will
// // then fill our vfio_region_info_with_cap structure with both the region info
// // and its capabilities.
// pub struct vfio_region_info_with_cap {
//     pub region_info: vfio_region_info,
//     cap_info: __IncompleteArrayField<u8>,
// }
// impl vfio_region_info_with_cap {
//     fn from_region_info(region_info: &vfio_region_info) -> Vec<Self> {
//         let region_info_size: u32 = std::mem::size_of::<vfio_region_info>() as u32;
//         let cap_len: usize = (region_info.argsz - region_info_size) as usize;
//
//         let mut region_with_cap = vec_with_array_field::<Self, u8>(cap_len);
//         region_with_cap[0].region_info.argsz = region_info.argsz;
//         region_with_cap[0].region_info.flags = 0;
//         region_with_cap[0].region_info.index = region_info.index;
//         region_with_cap[0].region_info.cap_offset = 0;
//         region_with_cap[0].region_info.size = 0;
//         region_with_cap[0].region_info.offset = 0;
//
//         region_with_cap
//     }
// }
//
// pub fn get_device_region_info_cap(
//     device: &impl AsRawFd,
//     reg_infos: &mut [vfio_region_info_with_cap],
// ) -> Result<(), VfioError> {
//     if reg_infos.is_empty()
//         || reg_infos[0].region_info.argsz as usize > reg_infos.len() * size_of::<vfio_region_info>()
//     {
//         Err(VfioError::VfioDeviceGetRegionInfo(SysError::new(
//             libc::EINVAL,
//         )))
//     } else {
//         // SAFETY: we are the owner of dev and region_info which are valid value,
//         // and we verify the return value.
//         let ret =
//             unsafe { ioctl_with_mut_ref(device, VFIO_DEVICE_GET_REGION_INFO(), &mut reg_infos[0]) };
//         if ret < 0 {
//             Err(VfioError::VfioDeviceGetRegionInfo(SysError::last()))
//         } else {
//             Ok(())
//         }
//     }
// }
// pub fn bind_device_iommufd(
//     vfio_cdev: &File,
//     bind: &mut vfio_device_bind_iommufd,
// ) -> Result<(), VfioError> {
//     // SAFETY:
//     // 1. The file descriptor provided by 'vfio_cdev' is valid and open.
//     // 2. The 'bind' points to initialized memory with expected data structure,
//     // and remains valid for the duration of syscall.
//     // 3. The return value is checked.
//     let ret = unsafe { ioctl_with_mut_ref(vfio_cdev, VFIO_DEVICE_BIND_IOMMUFD(), bind) };
//     if ret < 0 {
//         Err(VfioError::VfioDeviceBindIommufd(SysError::last()))
//     } else {
//         Ok(())
//     }
// }
//
// pub fn attach_device_iommufd_pt(
//     vfio_cdev: &File,
//     attach_data: &mut vfio_device_attach_iommufd_pt,
// ) -> Result<(), VfioError> {
//     // SAFETY:
//     // 1. The file descriptor provided by 'vfio_cdev' is valid and open.
//     // 2. The 'attach_data' points to initialized memory with expected data structure,
//     // and remains valid for the duration of syscall.
//     // 3. The return value is checked.
//     let ret =
//         unsafe { ioctl_with_mut_ref(vfio_cdev, VFIO_DEVICE_ATTACH_IOMMUFD_PT(), attach_data) };
//     if ret < 0 {
//         Err(VfioError::VfioDeviceAttachIommufdPt(SysError::last()))
//     } else {
//         Ok(())
//     }
// }
//
// pub fn detach_device_iommufd_pt(
//     vfio_cdev: &File,
//     detach_data: &vfio_device_detach_iommufd_pt,
// ) -> Result<(), VfioError> {
//     // SAFETY:
//     // 1. The file descriptor provided by 'vfio_cdev' is valid and open.
//     // 2. The 'detach_data' points to initialized memory with expected data structure,
//     // and remains valid for the duration of syscall.
//     // 3. The return value is checked.
//     let ret = unsafe { ioctl_with_ref(vfio_cdev, VFIO_DEVICE_DETACH_IOMMUFD_PT(), detach_data) };
//     if ret < 0 {
//         Err(VfioError::VfioDeviceDetachIommufdPt(SysError::last()))
//     } else {
//         Ok(())
//     }
// }

// #[cfg(test)]
// pub mod vfio_syscall {
//     use vfio_bindings::bindings::vfio::{VFIO_IRQ_INFO_EVENTFD, vfio_device_info};
//     use vmm_sys_util::tempfile::TempFile;
//
//     use super::*;
//
//     pub fn check_api_version(_fd: &impl AsRawFd) -> i32 {
//         VFIO_API_VERSION as i32
//     }
//
//     pub fn check_extension(_fd: &impl AsRawFd, val: u32) -> Result<u32, VfioError> {
//         if val == VFIO_TYPE1v2_IOMMU {
//             Ok(1)
//         } else {
//             Err(VfioError::VfioExtension)
//         }
//     }
//
//     pub fn set_iommu(_fd: &impl AsRawFd, _val: u32) -> Result<(), VfioError> {
//         Ok(())
//     }
//
//     pub fn map_dma(
//         _fd: &impl AsRawFd,
//         dma_map: &vfio_iommu_type1_dma_map,
//     ) -> Result<(), VfioError> {
//         if dma_map.iova == 0x1000 {
//             Ok(())
//         } else {
//             Err(VfioError::IommuDmaMap(SysError::last()))
//         }
//     }
//
//     pub fn unmap_dma(
//         _fd: &impl AsRawFd,
//         dma_map: &mut vfio_iommu_type1_dma_unmap,
//     ) -> Result<(), VfioError> {
//         if dma_map.iova == 0x1000 {
//             if dma_map.size == 0x2000 {
//                 dma_map.size = 0x1000;
//             }
//             Ok(())
//         } else {
//             Err(VfioError::IommuDmaUnmap(SysError::last()))
//         }
//     }
//
//     pub fn get_group_status(
//         _file: &File,
//         group_status: &mut vfio_group_status,
//     ) -> Result<(), VfioError> {
//         group_status.flags = VFIO_GROUP_FLAGS_VIABLE;
//         Ok(())
//     }
//
//     pub fn get_group_device_fd(_group: &impl AsRawFd, _path: &CStr) -> Result<File, VfioError> {
//         let tmp_file = TempFile::new().unwrap();
//         let device = File::open(tmp_file.as_path()).unwrap();
//
//         Ok(device)
//     }
//
//     pub fn set_group_container(group: &impl AsRawFd, fd: &impl AsRawFd) -> Result<(), VfioError>
// {         if group.as_raw_fd() >= 0 && fd.as_raw_fd() >= 0 {
//             Ok(())
//         } else {
//             Err(VfioError::GroupSetContainer)
//         }
//     }
//
//     pub fn unset_group_container(group: &impl AsRawFd, fd: &impl AsRawFd) -> Result<(),
// VfioError> {         if group.as_raw_fd() >= 0 && fd.as_raw_fd() >= 0 {
//             Ok(())
//         } else {
//             Err(VfioError::GroupSetContainer)
//         }
//     }
//
//     pub fn get_device_info(_file: &File, dev_info: &mut vfio_device_info) -> Result<(),
// VfioError> {         dev_info.flags = VFIO_DEVICE_FLAGS_PCI;
//         dev_info.num_regions = VFIO_PCI_NUM_REGIONS;
//         dev_info.num_irqs = VFIO_PCI_MSIX_IRQ_INDEX + 1;
//         Ok(())
//     }
//
//     #[allow(clippy::if_same_then_else)]
//     pub fn set_device_irqs(
//         _device: &impl AsRawFd,
//         irq_sets: &[vfio_irq_set],
//     ) -> Result<(), VfioError> {
//         if irq_sets.is_empty() || irq_sets[0].argsz as usize > std::mem::size_of_val(irq_sets) {
//             Err(VfioError::VfioDeviceSetIrq)
//         } else {
//             let irq_set = &irq_sets[0];
//             if irq_set.flags == VFIO_IRQ_SET_DATA_EVENTFD | VFIO_IRQ_SET_ACTION_TRIGGER
//                 && irq_set.index == 0
//                 && irq_set.count == 0
//             {
//                 Err(VfioError::VfioDeviceSetIrq)
//             } else if irq_set.flags == VFIO_IRQ_SET_DATA_NONE | VFIO_IRQ_SET_ACTION_TRIGGER
//                 && irq_set.index == 0
//                 && irq_set.count == 0
//             {
//                 Err(VfioError::VfioDeviceSetIrq)
//             } else if irq_set.flags == VFIO_IRQ_SET_DATA_NONE | VFIO_IRQ_SET_ACTION_UNMASK
//                 && irq_set.index == 1
//                 && irq_set.count == 1
//             {
//                 Err(VfioError::VfioDeviceSetIrq)
//             } else {
//                 Ok(())
//             }
//         }
//     }
//
//     pub fn reset(_device: &impl AsRawFd) -> i32 {
//         0
//     }
//
//     pub fn get_device_region_info(
//         _dev_info: &impl AsRawFd,
//         reg_info: &mut vfio_region_info,
//     ) -> Result<(), VfioError> {
//         match reg_info.index {
//             0 => {
//                 reg_info.flags = 0;
//                 reg_info.size = 0x1000;
//                 reg_info.offset = 0x10000;
//             }
//             1 => {
//                 reg_info.argsz = 88;
//                 reg_info.flags = VFIO_REGION_INFO_FLAG_CAPS;
//                 reg_info.size = 0x2000;
//                 reg_info.offset = 0x20000;
//             }
//             idx if idx == VFIO_PCI_VGA_REGION_INDEX => {
//                 return Err(VfioError::VfioDeviceGetRegionInfo(SysError::new(
//                     libc::EINVAL,
//                 )));
//             }
//             idx if (2..VFIO_PCI_NUM_REGIONS).contains(&idx) => {
//                 reg_info.flags = 0;
//                 reg_info.size = (idx as u64 + 1) * 0x1000;
//                 reg_info.offset = (idx as u64 + 1) * 0x10000;
//             }
//             idx if idx == VFIO_PCI_NUM_REGIONS => {
//                 return Err(VfioError::VfioDeviceGetRegionInfo(SysError::new(
//                     libc::EINVAL,
//                 )));
//             }
//             _ => panic!("invalid device region index"),
//         }
//
//         Ok(())
//     }
//
//     pub fn get_device_region_info_cap(
//         _dev_info: &impl AsRawFd,
//         reg_infos: &mut [vfio_region_info_with_cap],
//     ) -> Result<(), VfioError> {
//         if reg_infos.is_empty()
//             || reg_infos[0].region_info.argsz as usize
//                 > reg_infos.len() * size_of::<vfio_region_info>()
//         {
//             return Err(VfioError::VfioDeviceGetRegionInfo(SysError::new(
//                 libc::EINVAL,
//             )));
//         }
//
//         let reg_info = &mut reg_infos[0];
//         match reg_info.region_info.index {
//             1 => {
//                 reg_info.region_info.cap_offset = 32;
//                 // SAFETY: data structure returned by kernel is trusted.
//                 let header = unsafe {
//                     &mut *((reg_info as *mut vfio_region_info_with_cap as *mut u8).add(32)
//                         as *mut vfio_info_cap_header)
//                 };
//                 header.id = VFIO_REGION_INFO_CAP_MSIX_MAPPABLE as u16;
//                 header.next = 40;
//
//                 // SAFETY: data structure returned by kernel is trusted.
//                 let header = unsafe {
//                     &mut *((header as *mut vfio_info_cap_header as *mut u8).add(8)
//                         as *mut vfio_region_info_cap_type)
//                 };
//                 header.header.id = VFIO_REGION_INFO_CAP_TYPE as u16;
//                 header.header.next = 56;
//                 header.type_ = 0x5;
//                 header.subtype = 0x6;
//
//                 // SAFETY: data structure returned by kernel is trusted.
//                 let header = unsafe {
//                     &mut *((header as *mut vfio_region_info_cap_type as *mut u8).add(16)
//                         as *mut vfio_region_info_cap_sparse_mmap)
//                 };
//                 header.header.id = VFIO_REGION_INFO_CAP_SPARSE_MMAP as u16;
//                 header.header.next = 4;
//                 header.nr_areas = 1;
//
//                 // SAFETY: data structure returned by kernel is trusted.
//                 let mmap = unsafe {
//                     &mut *((header as *mut vfio_region_info_cap_sparse_mmap as *mut u8).add(16)
//                         as *mut vfio_region_sparse_mmap_area)
//                 };
//                 mmap.size = 0x3;
//                 mmap.offset = 0x4;
//             }
//             _ => panic!("invalid device region index"),
//         }
//
//         Ok(())
//     }
//
//     pub fn get_device_irq_info(
//         _dev_info: &impl AsRawFd,
//         irq_info: &mut vfio_irq_info,
//     ) -> Result<(), VfioError> {
//         match irq_info.index {
//             0 => {
//                 irq_info.flags = VFIO_IRQ_INFO_MASKABLE;
//                 irq_info.count = 1;
//             }
//             1 => {
//                 irq_info.flags = VFIO_IRQ_INFO_EVENTFD;
//                 irq_info.count = 32;
//             }
//             2 => {
//                 irq_info.flags = VFIO_IRQ_INFO_EVENTFD;
//                 irq_info.count = 2048;
//             }
//             3 => {
//                 return Err(VfioError::VfioDeviceGetRegionInfo(SysError::new(
//                     libc::EINVAL,
//                 )));
//             }
//             _ => panic!("invalid device irq index"),
//         }
//
//         Ok(())
//     }
//
//     pub fn create_dev_info_for_test() -> vfio_device_info {
//         vfio_device_info {
//             argsz: 0,
//             flags: 0,
//             num_regions: 2,
//             num_irqs: 4,
//             cap_offset: 0,
//             pad: 0,
//         }
//     }
//
//     #[cfg(feature = "vfio_cdev")]
//     pub fn bind_device_iommufd(
//         _vfio_cdev: &File,
//         _bind: &mut vfio_device_bind_iommufd,
//     ) -> Result<(), VfioError> {
//         Ok(())
//     }
//
//     #[cfg(feature = "vfio_cdev")]
//     pub fn attach_device_iommufd_pt(
//         _vfio_cdev: &File,
//         _attach_data: &mut vfio_device_attach_iommufd_pt,
//     ) -> Result<(), VfioError> {
//         Ok(())
//     }
//
//     #[cfg(feature = "vfio_cdev")]
//     pub fn detach_device_iommufd_pt(
//         _vfio_cdev: &File,
//         _detach_data: &vfio_device_detach_iommufd_pt,
//     ) -> Result<(), VfioError> {
//         Ok(())
//     }
// }
//
// #[cfg(test)]
// mod tests {
//     use super::*;
//
//     #[test]
//     fn test_vfio_ioctl_code() {
//         assert_eq!(VFIO_GET_API_VERSION(), 15204);
//         assert_eq!(VFIO_CHECK_EXTENSION(), 15205);
//         assert_eq!(VFIO_SET_IOMMU(), 15206);
//         assert_eq!(VFIO_GROUP_GET_STATUS(), 15207);
//         assert_eq!(VFIO_GROUP_SET_CONTAINER(), 15208);
//         assert_eq!(VFIO_GROUP_UNSET_CONTAINER(), 15209);
//         assert_eq!(VFIO_GROUP_GET_DEVICE_FD(), 15210);
//         assert_eq!(VFIO_DEVICE_GET_INFO(), 15211);
//         assert_eq!(VFIO_DEVICE_GET_REGION_INFO(), 15212);
//         assert_eq!(VFIO_DEVICE_GET_IRQ_INFO(), 15213);
//         assert_eq!(VFIO_DEVICE_SET_IRQS(), 15214);
//         assert_eq!(VFIO_DEVICE_RESET(), 15215);
//         assert_eq!(VFIO_DEVICE_IOEVENTFD(), 15220);
//         assert_eq!(VFIO_IOMMU_DISABLE(), 15220);
//         #[cfg(feature = "vfio_cdev")]
//         assert_eq!(VFIO_DEVICE_BIND_IOMMUFD(), 15222);
//         #[cfg(feature = "vfio_cdev")]
//         assert_eq!(VFIO_DEVICE_ATTACH_IOMMUFD_PT(), 15223);
//         #[cfg(feature = "vfio_cdev")]
//         assert_eq!(VFIO_DEVICE_DETACH_IOMMUFD_PT(), 15224);
//     }
// }
