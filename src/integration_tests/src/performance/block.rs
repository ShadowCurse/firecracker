use std::fs::OpenOptions;
use std::str::FromStr;

use utils::net::mac::MacAddr;
use vmm::resources::VmmConfig;
use vmm::vmm_config::boot_source::BootSourceConfig;
use vmm::vmm_config::drive::BlockDeviceConfig;
use vmm::vmm_config::machine_config::MachineConfig;
use vmm::vmm_config::net::NetworkInterfaceConfig;

use crate::{Fc, FcLaunchOptions, ResourceDir, ResultDir, SshConnection, TestConfig};

#[test]
fn block_perf() {
    let test_config = TestConfig::new("../../tests/rust_test_config.json".into());
    let firecracker_path = test_config.firecracker_path.canonicalize().expect(&format!(
        "cannot find firecracker at {:?}",
        test_config.firecracker_path
    ));
    let kernel_path = test_config.kernel_path.canonicalize().expect(&format!(
        "cannot find kernel at {:?}",
        test_config.kernel_path
    ));
    let rootfs_path = test_config.rootfs_path.canonicalize().expect(&format!(
        "cannot find rootfs at {:?}",
        test_config.rootfs_path
    ));

    let resources_dir = ResourceDir::new().unwrap();
    let dummy = OpenOptions::new()
        .create(true)
        .write(true)
        .open(resources_dir.join("dummy"))
        .unwrap();
    _ = dummy.set_len(0x10000);

    let results_dir = ResultDir::new("block").unwrap();

    for mode in ["read", "randread"] {
        for vcpus in [1, 2, 4] {
            let config = VmmConfig {
                boot_source: BootSourceConfig {
                    kernel_image_path: kernel_path.to_str().unwrap().to_owned(),
                    boot_args: Some("console=ttyS0 reboot=k panic=1 pci=off".to_string()),
                    ..Default::default()
                },
                block_devices: vec![
                    BlockDeviceConfig {
                        drive_id: "rootfs".to_string(),
                        is_root_device: true,
                        path_on_host: Some(rootfs_path.to_str().unwrap().to_owned()),
                        ..Default::default()
                    },
                    BlockDeviceConfig {
                        drive_id: "dummy".to_string(),
                        is_root_device: false,
                        path_on_host: Some("dummy".to_string()),
                        ..Default::default()
                    },
                ],
                net_devices: vec![NetworkInterfaceConfig {
                    iface_id: "eth0".to_string(),
                    guest_mac: Some(MacAddr::from_str("06:00:AC:10:00:02").unwrap()),
                    host_dev_name: "tap0".to_string(),
                    ..Default::default()
                }],
                machine_config: Some(MachineConfig {
                    vcpu_count: vcpus,
                    mem_size_mib: 1024,
                    ..Default::default()
                }),
                ..Default::default()
            };

            let _fc = Fc::new_from_config(
                &firecracker_path,
                &resources_dir,
                FcLaunchOptions::NoApi(&config),
            )
            .unwrap();

            for run in 0..2 {
                println!("running: mode: {mode}, vcpus: {vcpus}, run: {run}");
                let num_jobs = format!("--numjobs={vcpus}");
                let name = format!("--name={mode}-4096");
                let size = format!("--size={}", 0x10000);
                let rw = format!("--rw={mode}");
                let cpus_allowed = format!(
                    "--cpus_allowed={}",
                    (0..vcpus)
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                );

                let fio_cmd = [
                    "fio",
                    &name,
                    &rw,
                    "--bs=4096",
                    "--filename=/dev/vdb",
                    &size,
                    "--ioengine=libaio",
                    "--iodepth=32",
                    &num_jobs,
                    &cpus_allowed,
                    "--randrepeat=0",
                    "--output-format=json+",
                    "--direct=1",
                    "--time_base=1",
                    "--ramp_time=10",
                    "--runtime=30",
                ]
                .join(" ");

                let (stdout, _stderr) = SshConnection::ssh(
                    "172.16.0.2",
                    "root",
                    &test_config.rootfs_ssh_key_path,
                    &fio_cmd,
                )
                .unwrap();

                // println!("fio stdout: {stdout}");
                // println!("fio stderr: {stderr}");

                let result_name = format!("{mode}_vcpus_{vcpus}_run_{run}.json");
                results_dir
                    .write_result(&result_name, stdout.as_bytes())
                    .unwrap();
            }
        }
    }
}
