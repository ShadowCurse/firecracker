use std::os::unix::fs::symlink;
use std::process::Command;
use std::str::FromStr;

use utils::net::mac::MacAddr;
use vmm::resources::VmmConfig;
use vmm::vmm_config::boot_source::BootSourceConfig;
use vmm::vmm_config::drive::BlockDeviceConfig;
use vmm::vmm_config::machine_config::MachineConfig;
use vmm::vmm_config::net::NetworkInterfaceConfig;

use crate::{artifacts_paths, Fc, FcLaunchOptions, ResourceDir, ResultDir, SshConnection};

const FREE_HOST_CPU: usize = 10;

#[test]
fn test_net_perf() {
    let cwd = std::env::current_dir().unwrap();

    let (kernel_5_path, kernel_6_path, rootfs_path, key_path) = artifacts_paths();

    let resources_dir = ResourceDir::new().unwrap();
    symlink(cwd.join(kernel_5_path), resources_dir.join("kernel_5")).unwrap();
    symlink(cwd.join(kernel_6_path), resources_dir.join("kernel_6")).unwrap();
    symlink(cwd.join(rootfs_path), resources_dir.join("rootfs")).unwrap();
    // std::fs::soft_link(cwd.join(key_path), resources_dir.join("ssh_key.id_rsa"));

    let results_dir = ResultDir::new("net").unwrap();

    for kernel in ["kernel_5", "kernel_6"] {
        for is_vhost in [false, true] {
            for mode in ["g2h", "h2g", "bd"] {
                for payload_length in ["128K", "1024K"] {
                    for vcpus in [1, 2, 4] {
                        let config = VmmConfig {
                            boot_source: BootSourceConfig {
                                kernel_image_path: kernel.to_string(),
                                boot_args: Some(
                                    "console=ttyS0 reboot=k panic=1 pci=off".to_string(),
                                ),
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
                                vhost: is_vhost,
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
                            resources_dir.clone(),
                            FcLaunchOptions::NoApi(&config),
                        )
                        .unwrap();

                        println!("fc logs: {}", _fc.stdout());

                        // println!(
                        //     "running: mode: {mode}, payload_length: {payload_length}, vcpus: {vcpus}"
                        // );

                        let ports = (0..vcpus).map(|i| 5201 + i as u32).collect::<Vec<_>>();

                        for (i, port) in ports.iter().enumerate() {
                            let server_port = format!("{}", port);
                            let affinity = format!("--affinity={}", FREE_HOST_CPU + i);
                            let mut command = Command::new("iperf3");
                            command.args(["-sD", "-1", "-p", &server_port, &affinity]);
                            command.spawn().unwrap();
                        }

                        let children = ports
                            .iter()
                            .enumerate()
                            .map(|(i, port)| {
                                let server_port = format!("{}", port);
                                let len = format!("--len={}", payload_length);
                                let reverse = match mode {
                                    "g2h" => "",
                                    "h2g" => "-R",
                                    "bd" => {
                                        if i % 2 == 0 {
                                            ""
                                        } else {
                                            "-R"
                                        }
                                    }
                                    _ => unreachable!(),
                                };
                                let affinity = format!("--affinity={i}");

                                let iperf_guest_cmd = [
                                    "iperf3",
                                    "--time=20",
                                    "--json",
                                    "--omit=5",
                                    "-p",
                                    &server_port,
                                    "-c",
                                    "172.16.0.1",
                                    &len,
                                    &affinity,
                                    reverse,
                                ]
                                .join(" ");
                                println!("runnign guest command: {}", iperf_guest_cmd);

                                SshConnection::ssh_no_block(key_path, &iperf_guest_cmd).unwrap()
                            })
                            .collect::<Vec<_>>();

                        for (i, mut ssh_connection) in children.into_iter().enumerate() {
                            let stdout = ssh_connection.stdout();
                            let stderr = ssh_connection.stderr();

                            // println!("fio stdout: {stdout}");
                            println!("guest stderr: {stderr}");

                            let result_name = format!("{kernel}_vhost_{is_vhost}_{mode}_{payload_length}_{vcpus}_{i}.json");
                            results_dir
                                .write_result(&result_name, stdout.as_bytes())
                                .unwrap();
                        }
                    }
                }
            }
        }
    }
}
