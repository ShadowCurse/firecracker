// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

use std::io;
use vmm_sys_util::errno::Error as SysError;

/// fam
pub mod fam;
/// device
pub mod device;
/// ioctls
pub mod ioctls;

pub use ioctls::*;

/// Error codes for VFIO operations.
#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum VfioError {
    #[error("failed to open /dev/vfio/vfio container: {0}")]
    OpenContainer(#[source] io::Error),
    #[error("failed to open /dev/vfio/{1} group: {0}")]
    OpenGroup(#[source] io::Error, String),
    #[error("failed to get Group Status")]
    GetGroupStatus,
    #[error("group is not viable")]
    GroupViable,
    #[error("vfio API version doesn't match with VFIO_API_VERSION defined in vfio-bindings")]
    VfioApiVersion,
    #[error("failed to check VFIO extension")]
    VfioExtension,
    #[error("invalid VFIO type")]
    VfioInvalidType,
    #[error("container doesn't support VfioType1V2 IOMMU driver type")]
    VfioType1V2,
    #[error("failed to add vfio group into vfio container")]
    GroupSetContainer,
    #[error("failed to unset vfio container")]
    UnsetContainer,
    #[error("failed to set container's IOMMU driver type as VfioType1V2: {0}")]
    ContainerSetIOMMU(#[source] SysError),
    #[error("failed to get vfio device fd: {0}")]
    GroupGetDeviceFD(#[source] SysError),
    #[error("failed to set vfio device's attribute: {0}")]
    SetDeviceAttr(#[source] SysError),
    #[error("failed to get vfio device's info: {0}")]
    VfioDeviceGetInfo(#[source] SysError),
    #[error("vfio PCI device info doesn't match")]
    VfioDeviceGetInfoPCI,
    #[error("unsupported vfio device type")]
    VfioDeviceGetInfoOther,
    #[error("failed to get vfio device's region info: {0}")]
    VfioDeviceGetRegionInfo(#[source] SysError),
    #[error("failed to get vfio device's irq info: {0}")]
    VfioDeviceGetIrqInfo(#[source] SysError),
    #[error("invalid file path")]
    InvalidPath,
    #[error("failed to add guest memory map into iommu table: {0}")]
    IommuDmaMap(#[source] SysError),
    #[error("failed to remove guest memory map from iommu table: {0}")]
    IommuDmaUnmap(#[source] SysError),
    #[error("failed to set vfio device irq")]
    VfioDeviceSetIrq,
    #[error("failed to enable vfio device irq")]
    VfioDeviceEnableIrq,
    #[error("failed to disable vfio device irq")]
    VfioDeviceDisableIrq,
    #[error("failed to unmask vfio device irq")]
    VfioDeviceUnmaskIrq,
    #[error("failed to trigger vfio device irq")]
    VfioDeviceTriggerIrq,
    #[error("failed to set vfio device irq resample fd")]
    VfioDeviceSetIrqResampleFd,
    #[error("failed to duplicate fd")]
    VfioDeviceDupFd,
    #[error("wrong device fd type")]
    VfioDeviceFdWrongType,
    #[error("failed to get host address")]
    GetHostAddress,
    #[error("invalid dma unmap size")]
    InvalidDmaUnmapSize,
    #[error("failed to downcast VfioOps")]
    DowncastVfioOps,
    // #[cfg(feature = "vfio_cdev")]
    // #[error("failed to bind the vfio device to the specified iommufd: {0}")]
    // VfioDeviceBindIommufd(#[source] SysError),
    // #[cfg(feature = "vfio_cdev")]
    // #[error("failed to associate the vfio device with an IOAS within the bound iommufd: {0}")]
    // VfioDeviceAttachIommufdPt(#[source] SysError),
    // #[cfg(feature = "vfio_cdev")]
    // #[error(
    //     "failed to remove the association of the vfio device and its current associated IOAS: {0}"
    // )]
    // VfioDeviceDetachIommufdPt(#[source] SysError),
    // #[cfg(feature = "vfio_cdev")]
    // #[error("failed to new VfioIommufd")]
    // NewVfioIommufd(#[source] IommufdError),
    // #[cfg(feature = "vfio_cdev")]
    // #[error("invalid 'vfio_dev' folder")]
    // InvalidVfioDev,
    // #[cfg(feature = "vfio_cdev")]
    // #[error("failed to open device cdev")]
    // OpenDeviceCdev(#[source] io::Error),
    // #[cfg(feature = "vfio_cdev")]
    // #[error("failed iommufd ioctl")]
    // IommufdIoctlError(#[source] IommufdError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn test_vfio_error_fmt() {
        let e = VfioError::GetGroupStatus;
        let e2 = VfioError::OpenContainer(std::io::Error::from(std::io::ErrorKind::Other));
        let str = format!("{e}");

        assert_eq!(&str, "failed to get Group Status");
        assert!(e2.source().is_some());
        assert!(e.source().is_none());
    }
}
