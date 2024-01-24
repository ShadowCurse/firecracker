use std::process::Command;
use std::str::FromStr;

use utils::net::mac::MacAddr;
use vmm::resources::VmmConfig;
use vmm::vmm_config::boot_source::BootSourceConfig;
use vmm::vmm_config::drive::BlockDeviceConfig;
use vmm::vmm_config::machine_config::MachineConfig;
use vmm::vmm_config::net::NetworkInterfaceConfig;

use crate::{Fc, FcLaunchOptions, ResourceDir, ResultDir, SshConnection, TestConfig};

const FREE_HOST_CPU: usize = 10;

#[test]
fn net_perf() {
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
    let results_dir = ResultDir::new("net").unwrap();

    for mode in ["g2h", "h2g", "bd"] {
        for payload_length in ["128K", "1024K"] {
            for vcpus in [1, 2, 4] {
                let config = VmmConfig {
                    boot_source: BootSourceConfig {
                        kernel_image_path: kernel_path.to_str().unwrap().to_owned(),
                        boot_args: Some("console=ttyS0 reboot=k panic=1 pci=off".to_string()),
                        ..Default::default()
                    },
                    block_devices: vec![BlockDeviceConfig {
                        drive_id: "rootfs".to_string(),
                        is_root_device: true,
                        path_on_host: Some(rootfs_path.to_str().unwrap().to_owned()),
                        ..Default::default()
                    }],
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

                println!("fc logs: {}", _fc.stdout());
                println!("running: mode: {mode}, payload_length: {payload_length}, vcpus: {vcpus}");

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

                        SshConnection::ssh_no_block(
                            "172.16.0.2",
                            "root",
                            &test_config.rootfs_ssh_key_path,
                            &iperf_guest_cmd,
                        )
                        .unwrap()
                    })
                    .collect::<Vec<_>>();

                for (i, mut ssh_connection) in children.into_iter().enumerate() {
                    let stdout = ssh_connection.stdout();
                    let stderr = ssh_connection.stderr();

                    // println!("fio stdout: {stdout}");
                    println!("guest stderr: {stderr}");

                    let result_name = format!("{mode}_{payload_length}_{vcpus}_{i}.json");
                    results_dir
                        .write_result(&result_name, stdout.as_bytes())
                        .unwrap();
                }
            }
        }
    }
}

#[test]
fn remote_net_perf() {
    let test_config = TestConfig::new("../../tests/rust_test_config.json".into());

    if test_config.server_ip.is_none() {
        return;
    }

    let firecracker_path = test_config.firecracker_path.canonicalize().unwrap();
    let kernel_path = test_config.kernel_path.canonicalize().unwrap();
    let rootfs_path = test_config.rootfs_path.canonicalize().unwrap();
    let server_ip = test_config.server_ip.unwrap();
    let server_ssh_key_path = test_config.server_ssh_key_path.unwrap();

    let server_ports = (0..test_config.vms)
        .map(|i| {
            let server_port = format!("{}", 5000 + i);
            let iperf_server_cmd = ["iperf3", "-s", "-1", "-D", "-p", &server_port].join(" ");
            let (_stdout, stderr) = SshConnection::ssh(
                &server_ip,
                "ec2-user",
                &server_ssh_key_path,
                &iperf_server_cmd,
            )
            .unwrap();
            println!("creating iperf server stderr: {stderr:?}");
            server_port
        })
        .collect::<Vec<_>>();

    let results_dir = ResultDir::new("net_multi").unwrap();

    let resources_dir = ResourceDir::new().unwrap();
    let vm_ips = (0..test_config.vms)
        .map(|i| {
            let config = VmmConfig {
                boot_source: BootSourceConfig {
                    kernel_image_path: kernel_path.to_str().unwrap().to_owned(),
                    boot_args: Some("console=ttyS0 reboot=k panic=1 pci=off".to_string()),
                    ..Default::default()
                },
                block_devices: vec![BlockDeviceConfig {
                    drive_id: "rootfs".to_string(),
                    is_root_device: true,
                    path_on_host: Some(rootfs_path.to_str().unwrap().to_owned()),
                    ..Default::default()
                }],
                net_devices: vec![NetworkInterfaceConfig {
                    iface_id: "eth0".to_string(),
                    guest_mac: Some(MacAddr::from_str("06:00:AC:10:00:02").unwrap()),
                    host_dev_name: format!("tap{i}"),
                    ..Default::default()
                }],
                machine_config: Some(MachineConfig {
                    vcpu_count: 2,
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

            format!("172.16.0.{i}")
        })
        .collect::<Vec<_>>();

    let children = server_ports
        .iter()
        .zip(vm_ips.iter())
        .map(|(server_port, vm_ip)| {
            let iperf_guest_cmd = [
                "iperf3",
                "--time=20",
                "--json",
                "--omit=5",
                "-p",
                &server_port,
                "-c",
                &server_ip,
            ]
            .join(" ");
            println!("runnign guest command: {}", iperf_guest_cmd);

            SshConnection::ssh_no_block(
                vm_ip,
                "ec2-user",
                &test_config.rootfs_ssh_key_path,
                &iperf_guest_cmd,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();

    for (i, mut ssh_connection) in children.into_iter().enumerate() {
        let stdout = ssh_connection.stdout();
        let stderr = ssh_connection.stderr();

        // println!("fio stdout: {stdout}");
        println!("guest stderr: {stderr}");

        let result_name = format!("test_net_perf_multi_vm_{i}.json");
        results_dir
            .write_result(&result_name, stdout.as_bytes())
            .unwrap();
    }
}
