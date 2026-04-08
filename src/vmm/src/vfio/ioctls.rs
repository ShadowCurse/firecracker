// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

use std::ffi::CStr;
use std::fs::File;
use std::os::unix::io::{AsRawFd, FromRawFd};

use vfio_bindings::bindings::vfio::*;
use vmm_sys_util::errno::Error as SysError;
use vmm_sys_util::ioctl::{
    ioctl, ioctl_with_mut_ref, ioctl_with_ptr, ioctl_with_ref, ioctl_with_val,
};
use vmm_sys_util::ioctl_io_nr;

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VfioIoctlError {
    /// Failed to get Group Status
    GetGroupStatus,
    /// Group is not viable
    GroupViable,
    /// Vfio API version doesn't match with VFIO_API_VERSION defined in vfio-bindings
    VfioApiVersion,
    /// Failed to check VFIO extension
    VfioExtension,
    /// Invalid VFIO type
    VfioInvalidType,
    /// Container doesn't support VfioType1V2 IOMMU driver type
    VfioType1V2,
    /// Failed to add vfio group into vfio container
    GroupSetContainer,
    /// Failed to unset vfio container: {0}
    GroupUnsetContainer(SysError),
    /// Failed get VFIO IOMMU info: {0}
    IommuGetInfo(SysError),
    /// Failed to set container's IOMMU driver type as VfioType1V2: {0}
    ContainerSetIOMMU(SysError),
    /// Failed to get vfio device fd: {0}
    GroupGetDeviceFD(SysError),
    /// Failed reset the device
    DeviceReset,
    /// Failed to set vfio device's attribute: {0}
    SetDeviceAttr(SysError),
    /// Failed to get vfio device's info: {0}
    VfioDeviceGetInfo(SysError),
    /// Vfio PCI device info doesn't match
    VfioDeviceGetInfoPCI,
    /// Unsupported vfio device type
    VfioDeviceGetInfoOther,
    /// Failed to get vfio device's region info: {0}
    VfioDeviceGetRegionInfo(SysError),
    /// Failed to get vfio device's irq info: {0}
    VfioDeviceGetIrqInfo(SysError),
    /// Invalid file path
    InvalidPath,
    /// Failed to add guest memory map into iommu table: {0}
    IommuDmaMap(SysError),
    /// Failed to remove guest memory map from iommu table: {0}
    IommuDmaUnmap(SysError),
    /// Failed to set vfio device irq
    VfioDeviceSetIrq,
    /// Failed to enable vfio device irq
    VfioDeviceEnableIrq,
    /// Failed to disable vfio device irq
    VfioDeviceDisableIrq,
    /// Failed to unmask vfio device irq
    VfioDeviceUnmaskIrq,
    /// Failed to trigger vfio device irq
    VfioDeviceTriggerIrq,
    /// Failed to set vfio device irq resample fd
    VfioDeviceSetIrqResampleFd,
    /// Failed to duplicate fd
    VfioDeviceDupFd,
    /// Wrong device fd type
    VfioDeviceFdWrongType,
    /// Failed to get host address
    GetHostAddress,
    /// Invalid dma unmap size
    InvalidDmaUnmapSize,
    /// Failed to downcast VfioOps
    DowncastVfioOps,
    // #[cfg(feature = "vfio_cdev")]
    // /// failed to bind the vfio device to the specified iommufd: {0}
    // VfioDeviceBindIommufd(SysError),
    // #[cfg(feature = "vfio_cdev")]
    // /// failed to associate the vfio device with an IOAS within the bound iommufd: {0}
    // VfioDeviceAttachIommufdPt(SysError),
    // #[cfg(feature = "vfio_cdev")]
    // #[error(
    //     "failed to remove the association of the vfio device and its current associated IOAS:
    // {0}" )]
    // VfioDeviceDetachIommufdPt(SysError),
    // #[cfg(feature = "vfio_cdev")]
    // /// failed to new VfioIommufd
    // NewVfioIommufd(IommufdError),
    // #[cfg(feature = "vfio_cdev")]
    // /// invalid 'vfio_dev' folder
    // InvalidVfioDev,
    // #[cfg(feature = "vfio_cdev")]
    // /// failed to open device cdev
    // OpenDeviceCdev(io::Error),
    // #[cfg(feature = "vfio_cdev")]
    // /// failed iommufd ioctl
    // IommufdIoctlError(#[source] IommufdError),
}

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

/// Report the version of the VFIO API.  This allows us to bump the entire
/// API version should we later need to add or change features in incompatible
/// ways.
/// Availability: Always
pub fn check_api_version(container: &impl AsRawFd) -> i32 {
    // SAFETY: file is vfio container fd and ioctl is defined by kernel.
    unsafe { ioctl(container, VFIO_GET_API_VERSION()) }
}

/// Check whether an extension is supported.
/// Return: 0 if not supported, 1 (or some other positive integer) if supported.
/// Availability: Always
pub fn check_extension(container: &impl AsRawFd, val: u32) -> Result<u32, VfioIoctlError> {
    // SAFETY: file is vfio container and make sure val is valid.
    let ret = unsafe { ioctl_with_val(container, VFIO_CHECK_EXTENSION(), val.into()) };
    if ret < 0 {
        Err(VfioIoctlError::VfioExtension)
    } else {
        Ok(ret as u32)
    }
}

// Retrieve information about the IOMMU object. Fills in provided
// struct vfio_iommu_info. Caller sets argsz.
pub fn iommu_get_info(
    container: &impl AsRawFd,
    info: &vfio_iommu_type1_info,
) -> Result<(), VfioIoctlError> {
    // SAFETY: file is vfio container and make sure val is valid.
    let ret = unsafe { ioctl_with_ref(container, VFIO_IOMMU_GET_INFO(), info) };
    if ret < 0 {
        Err(VfioIoctlError::IommuGetInfo(SysError::last()))
    } else {
        Ok(())
    }
}

/// Set the iommu to the given type.  The type must be supported by an
/// iommu driver as verified by calling CHECK_EXTENSION using the same
/// type.  A group must be set to this file descriptor before this
/// ioctl is available.  The IOMMU interfaces enabled by this call are
/// specific to the value set.
/// Availability: When VFIO group attached
pub fn set_iommu(container: &impl AsRawFd, val: u32) -> Result<(), VfioIoctlError> {
    // SAFETY: file is vfio container and make sure val is valid.
    let ret = unsafe { ioctl_with_val(container, VFIO_SET_IOMMU(), val.into()) };
    if ret < 0 {
        Err(VfioIoctlError::ContainerSetIOMMU(SysError::last()))
    } else {
        Ok(())
    }
}

/// Map process virtual addresses to IO virtual addresses using the
/// provided struct vfio_dma_map. Caller sets argsz. READ &/ WRITE required.
///
/// If flags & VFIO_DMA_MAP_FLAG_VADDR, update the base vaddr for iova. The vaddr
/// must have previously been invalidated with VFIO_DMA_UNMAP_FLAG_VADDR.  To
/// maintain memory consistency within the user application, the updated vaddr
/// must address the same memory object as originally mapped.  Failure to do so
/// will result in user memory corruption and/or device misbehavior.  iova and
/// size must match those in the original MAP_DMA call.  Protection is not
/// changed, and the READ & WRITE flags must be 0.
pub fn iommu_map_dma(
    container: &impl AsRawFd,
    dma_map: &vfio_iommu_type1_dma_map,
) -> Result<(), VfioIoctlError> {
    // SAFETY: file is vfio container, dma_map is constructed by us, and
    // we check the return value
    let ret = unsafe { ioctl_with_ref(container, VFIO_IOMMU_MAP_DMA(), dma_map) };
    if ret != 0 {
        Err(VfioIoctlError::IommuDmaMap(SysError::last()))
    } else {
        Ok(())
    }
}

/// Unmap IO virtual addresses using the provided struct vfio_dma_unmap.
/// Caller sets argsz.  The actual unmapped size is returned in the size
/// field.  No guarantee is made to the user that arbitrary unmaps of iova
/// or size different from those used in the original mapping call will
/// succeed.
///
/// VFIO_DMA_UNMAP_FLAG_GET_DIRTY_BITMAP should be set to get the dirty bitmap
/// before unmapping IO virtual addresses. When this flag is set, the user must
/// provide a struct vfio_bitmap in data[]. User must provide zero-allocated
/// memory via vfio_bitmap.data and its size in the vfio_bitmap.size field.
/// A bit in the bitmap represents one page, of user provided page size in
/// vfio_bitmap.pgsize field, consecutively starting from iova offset. Bit set
/// indicates that the page at that offset from iova is dirty. A Bitmap of the
/// pages in the range of unmapped size is returned in the user-provided
/// vfio_bitmap.data.
///
/// If flags & VFIO_DMA_UNMAP_FLAG_ALL, unmap all addresses.  iova and size
/// must be 0.  This cannot be combined with the get-dirty-bitmap flag.
///
/// If flags & VFIO_DMA_UNMAP_FLAG_VADDR, do not unmap, but invalidate host
/// virtual addresses in the iova range.  DMA to already-mapped pages continues.
/// Groups may not be added to the container while any addresses are invalid.
/// This cannot be combined with the get-dirty-bitmap flag.
pub fn iommu_unmap_dma(
    vfio: &impl AsRawFd,
    dma_map: &mut vfio_iommu_type1_dma_unmap,
) -> Result<(), VfioIoctlError> {
    // SAFETY: file is vfio container, dma_unmap is constructed by us, and
    // we check the return value
    let ret = unsafe { ioctl_with_ref(vfio, VFIO_IOMMU_UNMAP_DMA(), dma_map) };
    if ret != 0 {
        Err(VfioIoctlError::IommuDmaUnmap(SysError::last()))
    } else {
        Ok(())
    }
}

/// Retrieve information about the group.  Fills in provided
/// struct vfio_group_info.  Caller sets argsz.
/// Availability: Always
pub fn group_get_status(
    group: &impl AsRawFd,
    group_status: &mut vfio_group_status,
) -> Result<(), VfioIoctlError> {
    // SAFETY: we are the owner of group and group_status which are valid value.
    let ret = unsafe { ioctl_with_mut_ref(group, VFIO_GROUP_GET_STATUS(), group_status) };
    if ret < 0 {
        Err(VfioIoctlError::GetGroupStatus)
    } else {
        Ok(())
    }
}

/// Return a new file descriptor for the device object described by
/// the provided string.  The string should match a device listed in
/// the devices subdirectory of the IOMMU group sysfs entry.  The
/// group containing the device must already be added to this context.
/// Return: new file descriptor on success, -errno on failure.
/// Availability: When attached to container
pub fn group_get_device_fd(group: &impl AsRawFd, path: &CStr) -> Result<File, VfioIoctlError> {
    // SAFETY: we are the owner of self and path_ptr which are valid value.
    let fd = unsafe { ioctl_with_ptr(group, VFIO_GROUP_GET_DEVICE_FD(), path.as_ptr()) };
    if fd < 0 {
        Err(VfioIoctlError::GroupGetDeviceFD(SysError::last()))
    } else {
        // SAFETY: fd is valid FD
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

/// Set the container for the VFIO group to the open VFIO file
/// descriptor provided.  Groups may only belong to a single
/// container.  Containers may, at their discretion, support multiple
/// groups.  Only when a container is set are all of the interfaces
/// of the VFIO file descriptor and the VFIO group file descriptor
/// available to the user.
/// Availability: Always
pub fn group_set_container(
    group: &impl AsRawFd,
    container: &impl AsRawFd,
) -> Result<(), VfioIoctlError> {
    // SAFETY: we are the owner of group and container_raw_fd which are valid value,
    // and we verify the ret value
    let ret = unsafe { ioctl_with_ref(group, VFIO_GROUP_SET_CONTAINER(), container) };
    if ret < 0 {
        Err(VfioIoctlError::GroupSetContainer)
    } else {
        Ok(())
    }
}

/// Remove the group from the attached container.  This is the
/// opposite of the SET_CONTAINER call and returns the group to
/// an initial state.  All device file descriptors must be released
/// prior to calling this interface.  When removing the last group
/// from a container, the IOMMU will be disabled and all state lost,
/// effectively also returning the VFIO file descriptor to an initial
/// state.
/// Availability: When attached to container
pub fn group_unset_container(
    group: &impl AsRawFd,
    container: &impl AsRawFd,
) -> Result<(), VfioIoctlError> {
    // SAFETY: we are the owner of self and container_raw_fd which are valid value.
    let ret = unsafe { ioctl_with_ref(group, VFIO_GROUP_UNSET_CONTAINER(), container) };
    if ret < 0 {
        Err(VfioIoctlError::GroupUnsetContainer(SysError::last()))
    } else {
        Ok(())
    }
}

/// Retrieve information about the device.  Fills in provided
/// struct vfio_device_info.  Caller sets argsz.
pub fn device_get_info(
    device: &impl AsRawFd,
    dev_info: &mut vfio_device_info,
) -> Result<(), VfioIoctlError> {
    // SAFETY: we are the owner of dev and dev_info which are valid value,
    // and we verify the return value.
    let ret = unsafe { ioctl_with_mut_ref(device, VFIO_DEVICE_GET_INFO(), dev_info) };
    if ret < 0 {
        Err(VfioIoctlError::VfioDeviceGetInfo(SysError::last()))
    } else {
        Ok(())
    }
}

/// Set signaling, masking, and unmasking of interrupts.  Caller provides
/// struct vfio_irq_set with all fields set.  'start' and 'count' indicate
/// the range of subindexes being specified.
///
/// The DATA flags specify the type of data provided.  If DATA_NONE, the
/// operation performs the specified action immediately on the specified
/// interrupt(s).  For example, to unmask AUTOMASKED interrupt [0,0]:
/// flags = (DATA_NONE|ACTION_UNMASK), index = 0, start = 0, count = 1.
///
/// DATA_BOOL allows sparse support for the same on arrays of interrupts.
/// For example, to mask interrupts [0,1] and [0,3] (but not [0,2]):
/// flags = (DATA_BOOL|ACTION_MASK), index = 0, start = 1, count = 3,
/// data = {1,0,1}
///
/// DATA_EVENTFD binds the specified ACTION to the provided __s32 eventfd.
/// A value of -1 can be used to either de-assign interrupts if already
/// assigned or skip un-assigned interrupts.  For example, to set an eventfd
/// to be trigger for interrupts [0,0] and [0,2]:
/// flags = (DATA_EVENTFD|ACTION_TRIGGER), index = 0, start = 0, count = 3,
/// data = {fd1, -1, fd2}
/// If index [0,1] is previously set, two count = 1 ioctls calls would be
/// required to set [0,0] and [0,2] without changing [0,1].
///
/// Once a signaling mechanism is set, DATA_BOOL or DATA_NONE can be used
/// with ACTION_TRIGGER to perform kernel level interrupt loopback testing
/// from userspace (ie. simulate hardware triggering).
///
/// Setting of an event triggering mechanism to userspace for ACTION_TRIGGER
/// enables the interrupt index for the device.  Individual subindex interrupts
/// can be disabled using the -1 value for DATA_EVENTFD or the index can be
/// disabled as a whole with: flags = (DATA_NONE|ACTION_TRIGGER), count = 0.
///
/// Note that ACTION_[UN]MASK specify user->kernel signaling (irqfds) while
/// ACTION_TRIGGER specifies kernel->user signaling.
pub fn device_set_irqs(
    device: &impl AsRawFd,
    // TODO: maybe move the Fam.. types here and pass those into ioclts to guarantee correctnes?
    irq_set: &vfio_irq_set,
) -> Result<(), VfioIoctlError> {
    // SAFETY: we are the owner of self and irq_set which are valid value
    let ret = unsafe { ioctl_with_ref(device, VFIO_DEVICE_SET_IRQS(), irq_set) };
    if ret < 0 {
        Err(VfioIoctlError::VfioDeviceSetIrq)
    } else {
        Ok(())
    }
}

/// Reset a device.
pub fn device_reset(device: &impl AsRawFd) -> Result<(), VfioIoctlError> {
    // SAFETY: file is vfio device
    let ret = unsafe { ioctl(device, VFIO_DEVICE_RESET()) };
    if ret < 0 {
        Err(VfioIoctlError::DeviceReset)
    } else {
        Ok(())
    }
}

/// Retrieve information about a device IRQ.  Caller provides
/// struct vfio_irq_info with index value set.  Caller sets argsz.
/// Implementation of IRQ mapping is bus driver specific.  Indexes
/// using multiple IRQs are primarily intended to support MSI-like
/// interrupt blocks.  Zero count irq blocks may be used to describe
/// unimplemented interrupt types.
///
/// The EVENTFD flag indicates the interrupt index supports eventfd based
/// signaling.
///
/// The MASKABLE flags indicates the index supports MASK and UNMASK
/// actions described below.
///
/// AUTOMASKED indicates that after signaling, the interrupt line is
/// automatically masked by VFIO and the user needs to unmask the line
/// to receive new interrupts.  This is primarily intended to distinguish
/// level triggered interrupts.
///
/// The NORESIZE flag indicates that the interrupt lines within the index
/// are setup as a set and new subindexes cannot be enabled without first
/// disabling the entire index.  This is used for interrupts like PCI MSI
/// and MSI-X where the driver may only use a subset of the available
/// indexes, but VFIO needs to enable a specific number of vectors
/// upfront.  In the case of MSI-X, where the user can enable MSI-X and
/// then add and unmask vectors, it's up to userspace to make the decision
/// whether to allocate the maximum supported number of vectors or tear
/// down setup and incrementally increase the vectors as each is enabled.
/// Absence of the NORESIZE flag indicates that vectors can be enabled
/// and disabled dynamically without impacting other vectors within the
/// index.
pub fn device_get_irq_info(
    device: &impl AsRawFd,
    irq_info: &mut vfio_irq_info,
) -> Result<(), VfioIoctlError> {
    // SAFETY: we are the owner of dev and irq_info which are valid value
    let ret = unsafe { ioctl_with_mut_ref(device, VFIO_DEVICE_GET_IRQ_INFO(), irq_info) };
    if ret < 0 {
        Err(VfioIoctlError::VfioDeviceGetIrqInfo(SysError::last()))
    } else {
        Ok(())
    }
}

/// Retrieve information about a device region.  Caller provides
/// struct vfio_region_info with index value set.  Caller sets argsz.
/// Implementation of region mapping is bus driver specific.  This is
/// intended to describe MMIO, I/O port, as well as bus specific
/// regions (ex. PCI config space).  Zero sized regions may be used
/// to describe unimplemented regions (ex. unimplemented PCI BARs).
/// Return: 0 on success, -errno on failure.
pub fn device_get_region_info(
    device: &impl AsRawFd,
    reg_info: &mut vfio_region_info,
) -> Result<(), VfioIoctlError> {
    // SAFETY: we are the owner of dev and region_info which are valid value
    // and we verify the return value.
    let ret = unsafe { ioctl_with_mut_ref(device, VFIO_DEVICE_GET_REGION_INFO(), reg_info) };
    if ret < 0 {
        Err(VfioIoctlError::VfioDeviceGetRegionInfo(SysError::last()))
    } else {
        Ok(())
    }
}

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
