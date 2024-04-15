use std::os::unix::fs::symlink;
use std::str::FromStr;

use utils::net::mac::MacAddr;
use vmm::resources::VmmConfig;
use vmm::vmm_config::boot_source::BootSourceConfig;
use vmm::vmm_config::drive::BlockDeviceConfig;
use vmm::vmm_config::machine_config::MachineConfig;
use vmm::vmm_config::net::NetworkInterfaceConfig;

use crate::{artifacts_paths, Fc, FcLaunchOptions, ResourceDir, SshConnection};

#[test]
fn test_api_start() {
    let cwd = std::env::current_dir().unwrap();

    let (kernel_path, rootfs_path, key_path) = artifacts_paths();

    let resources_dir = ResourceDir::new().unwrap();
    symlink(cwd.join(kernel_path), resources_dir.join("kernel")).unwrap();
    symlink(cwd.join(rootfs_path), resources_dir.join("rootfs")).unwrap();

    let config = VmmConfig {
        boot_source: BootSourceConfig {
            kernel_image_path: "kernel".to_string(),
            boot_args: Some("console=ttyS0 reboot=k panic=1 pci=off".to_string()),
            ..Default::default()
        },
        block_devices: vec![BlockDeviceConfig {
            drive_id: "rootfs".to_string(),
            is_root_device: true,
            path_on_host: Some("rootfs".to_string()),
            ..Default::default()
        }],
        net_devices: vec![NetworkInterfaceConfig {
            iface_id: "eth0".to_string(),
            guest_mac: Some(MacAddr::from_str("06:00:AC:10:00:02").unwrap()),
            host_dev_name: "tap0".to_string(),
            ..Default::default()
        }],
        machine_config: Some(MachineConfig {
            vcpu_count: 1,
            mem_size_mib: 128,
            ..Default::default()
        }),
        ..Default::default()
    };

    let _fc =
        Fc::new_from_config(resources_dir.clone(), FcLaunchOptions::WithApi(&config)).unwrap();

    let (stdout, stderr) = SshConnection::ssh(key_path, "true").unwrap();
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
}

#[test]
fn test_api_configure_boot() {
    let cwd = std::env::current_dir().unwrap();

    let (kernel_path, rootfs_path, key_path) = artifacts_paths();

    let resources_dir = ResourceDir::new().unwrap();
    symlink(cwd.join(kernel_path), resources_dir.join("kernel")).unwrap();
    symlink(cwd.join(rootfs_path), resources_dir.join("rootfs")).unwrap();

    let fc = Fc::new_from_config(resources_dir.clone(), FcLaunchOptions::WithApiNoConfig).unwrap();

    let (stdout, stderr) = SshConnection::ssh(key_path, "true").unwrap();
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());

    let boot_source = BootSourceConfig {
        kernel_image_path: "lol".to_string(),
        boot_args: Some("lol".to_string()),
        initrd_path: None,
    };
    let _err = fc.api_put_boot_source(&boot_source).unwrap_err();
    // TODO assert error

    let boot_source = BootSourceConfig {
        kernel_image_path: "kernel".to_string(),
        boot_args: Some("console=ttyS0 reboot=k panic=1 pci=off".to_string()),
        initrd_path: None,
    };
    fc.api_put_boot_source(&boot_source).unwrap();
}

#[test]
fn test_api_configure_drive() {
    let cwd = std::env::current_dir().unwrap();

    let (kernel_path, rootfs_path, key_path) = artifacts_paths();

    let resources_dir = ResourceDir::new().unwrap();
    symlink(cwd.join(kernel_path), resources_dir.join("kernel")).unwrap();
    symlink(cwd.join(rootfs_path), resources_dir.join("rootfs")).unwrap();

    let fc = Fc::new_from_config(resources_dir.clone(), FcLaunchOptions::WithApiNoConfig).unwrap();

    let (stdout, stderr) = SshConnection::ssh(key_path, "true").unwrap();
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());

    let boot_source = BootSourceConfig {
        kernel_image_path: "kernel".to_string(),
        boot_args: Some("console=ttyS0 reboot=k panic=1 pci=off".to_string()),
        initrd_path: None,
    };
    fc.api_put_boot_source(&boot_source).unwrap();
}
