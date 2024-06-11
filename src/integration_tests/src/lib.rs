use std::env::temp_dir;
use std::fs::{create_dir, create_dir_all, remove_dir_all, File, OpenOptions};
use std::io::{Read, Write};
use std::ops::Deref;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{sleep, JoinHandle};
use std::time::Duration;
use std::{path::PathBuf, process::Command};

use rand::random;
use serde::{Deserialize, Serialize};
use vmm::cpu_config::templates::CustomCpuTemplate;
use vmm::logger::LoggerConfig;
use vmm::resources::VmmConfig;
use vmm::vmm_config::balloon::{
    BalloonDeviceConfig, BalloonUpdateConfig, BalloonUpdateStatsConfig,
};
use vmm::vmm_config::boot_source::BootSourceConfig;
use vmm::vmm_config::drive::BlockDeviceConfig;
use vmm::vmm_config::entropy::EntropyDeviceConfig;
use vmm::vmm_config::machine_config::{MachineConfig, MachineConfigUpdate};
use vmm::vmm_config::metrics::MetricsConfig;
use vmm::vmm_config::mmds::MmdsConfig;
use vmm::vmm_config::net::NetworkInterfaceConfig;
use vmm::vmm_config::snapshot::{CreateSnapshotParams, LoadSnapshotConfig};
use vmm::vmm_config::vsock::VsockDeviceConfig;

// #[cfg(test)]
// pub mod block;
#[cfg(test)]
pub mod net;

pub fn binary_path(name: &str) -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    path.pop();
    path.pop();
    if cfg!(target_arch = "x86_64") {
        path.join("x86_64-unknown-linux-musl/release").join(name)
    } else if cfg!(target_arch = "aarch64") {
        path.join("aarch64-unknown-linux-musl/release").join(name)
    } else {
        unreachable!();
    }
}

pub fn artifacts_paths() -> (&'static str, &'static str, &'static str, &'static str) {
    if cfg!(target_arch = "x86_64") {
        (
            "../../build/img/x86_64/vmlinux-5.10.210",
            "../../build/img/x86_64/vmlinux-6.1.82",
            "../../build/img/x86_64/ubuntu-22.04.ext4",
            "../../build/img/x86_64/ubuntu-22.04.id_rsa",
        )
    } else if cfg!(target_arch = "aarch64") {
        (
            "../../build/img/aarch64/vmlinux-5.10.210",
            "../../build/img/aarch64/vmlinux-6.1.82",
            "../../build/img/aarch64/ubuntu-22.04.ext4",
            "../../build/img/aarch64/ubuntu-22.04.id_rsa",
        )
    } else {
        unreachable!();
    }
}

#[derive(Debug)]
pub struct ResourceDir {
    path: PathBuf,
}

impl ResourceDir {
    pub fn new() -> Result<Self, std::io::Error> {
        let random_id = format!("{}", random::<u64>());
        let path = temp_dir().join(random_id);
        create_dir(&path)?;
        Ok(Self { path })
    }
}

impl Deref for ResourceDir {
    type Target = PathBuf;
    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl Drop for ResourceDir {
    fn drop(&mut self) {
        remove_dir_all(&self.path).expect("Resource directory removal");
    }
}

#[derive(Debug)]
pub struct ResultDir {
    path: PathBuf,
}
impl ResultDir {
    pub fn new(test_name: &str) -> Result<Self, std::io::Error> {
        let cwd = std::env::current_dir()?;
        let path = cwd.join("../../rust_test_results/").join(test_name);
        create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub fn write_result(&self, name: &str, data: &[u8]) -> Result<(), std::io::Error> {
        let mut result_file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(self.join(name))
            .unwrap();
        result_file.write_all(data)?;
        Ok(())
    }
}

impl Deref for ResultDir {
    type Target = PathBuf;
    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

// Copied from firecracker/api_server
#[derive(Debug, Deserialize, Serialize)]
pub enum ActionType {
    FlushMetrics,
    InstanceStart,
    SendCtrlAltDel,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionBody {
    pub action_type: ActionType,
}

#[derive(Debug)]
pub enum FcLaunchOptions<'a> {
    NoApi(&'a VmmConfig),
    WithApi(&'a VmmConfig),
    WithApiNoConfig,
}

#[derive(Debug)]
pub struct Fc {
    socket_path: PathBuf,
    proccess_handle: Child,

    stdout_thread: JoinHandle<()>,
    stdout_data: Arc<Mutex<Vec<u8>>>,
}

impl Fc {
    pub fn new_from_config(
        artifacts_dir: PathBuf,
        options: FcLaunchOptions,
    ) -> Result<Self, std::io::Error> {
        let cwd = std::env::current_dir()?;

        // switch to artifacts_dir to start FC
        // because paths in the config are relative to
        // artifacts_dir
        std::env::set_current_dir(&artifacts_dir)?;
        let binary_path = binary_path("firecracker");

        let socket_path = artifacts_dir.join("socket.socket");
        if socket_path.exists() {
            std::fs::remove_file(&socket_path)?;
        }

        let config_path = artifacts_dir.join("vm_config.json");
        if config_path.exists() {
            std::fs::remove_file(&config_path)?;
        }

        let mut command = Command::new(binary_path);

        match options {
            FcLaunchOptions::NoApi(config) => {
                let config_json = serde_json::to_string(&config).unwrap();
                let mut config_file = File::options()
                    .create(true)
                    .write(true)
                    .open(&config_path)?;
                config_file.write_all(config_json.as_bytes())?;

                command.arg("--no-api");
                command.arg("--config-file");
                command.arg(&config_path);
            }
            FcLaunchOptions::WithApi(config) => {
                let config_json = serde_json::to_string(&config).unwrap();
                let mut config_file = File::options()
                    .create(true)
                    .write(true)
                    .open(&config_path)?;
                config_file.write_all(config_json.as_bytes())?;

                command.arg("--api-sock");
                command.arg(&socket_path);
                command.arg("--config-file");
                command.arg(&config_path);
            }
            FcLaunchOptions::WithApiNoConfig => {
                command.arg("--api-sock");
                command.arg(&socket_path);
            }
        }

        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut proccess_handle = command.spawn()?;

        let mut stdout = proccess_handle.stdout.take().unwrap();
        let stdout_data = Arc::new(Mutex::new(Vec::new()));
        let stdout_data_clone = stdout_data.clone();
        let stdout_thread = std::thread::spawn(move || loop {
            let mut buf = [0];
            match stdout.read(&mut buf) {
                Err(err) => {
                    println!("[{}] Error reading from stream: {}", line!(), err);
                    break;
                }
                Ok(got) => {
                    if got == 0 {
                        break;
                    } else if got == 1 {
                        stdout_data_clone.lock().expect("!lock").push(buf[0])
                    } else {
                        println!("[{}] Unexpected number of bytes: {}", line!(), got);
                        break;
                    }
                }
            }
        });

        // switch back to previous cwd in order
        // to keep everything as it was
        std::env::set_current_dir(cwd)?;

        // let fc to boot
        sleep(Duration::from_millis(2000));

        Ok(Self {
            socket_path,
            proccess_handle,

            stdout_thread,
            stdout_data,
        })
    }

    pub fn kill(&mut self) -> Result<(), std::io::Error> {
        self.proccess_handle.kill()?;
        // let fc to die
        sleep(Duration::from_millis(1000));
        Ok(())
    }

    pub fn stdout(&self) -> String {
        let stdout_data = self.stdout_data.lock().unwrap();
        String::from_utf8(stdout_data.clone()).unwrap()
    }

    /// Calls `curl` with parameters
    /// `request_type` can be `PUT`, `PATCH`
    fn send_curl_request(
        &self,
        request_type: &str,
        request_destination: &str,
        data: &str,
    ) -> Result<(), std::io::Error> {
        let mut command = Command::new("curl");
        command.args([
            "-X",
            request_type,
            "--unix-socket",
            self.socket_path.to_str().unwrap(),
            "--data",
            data,
            &format!("http://localhost/{request_destination}"),
        ]);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut proccess_handle = command.spawn()?;
        let exit_status = proccess_handle.wait()?;

        if !exit_status.success() {
            let mut stdout = proccess_handle.stdout.take().unwrap();
            let mut stderr = proccess_handle.stdout.take().unwrap();

            let mut stdout_str = String::new();
            stdout.read_to_string(&mut stdout_str)?;

            let mut stderr_str = String::new();
            stderr.read_to_string(&mut stderr_str)?;

            eprintln!("api_put_logger error stdout: {stdout_str}");
            eprintln!("api_put_logger error stderr: {stderr_str}");
        }
        Ok(())
    }

    pub fn api_put_action(&self, data: &ActionBody) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        self.send_curl_request("PUT", "actions", &json)
    }

    pub fn api_put_ballon(&self, data: &BalloonDeviceConfig) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        self.send_curl_request("PUT", "balloon", &json)
    }

    pub fn api_patch_ballon_update(
        &self,
        data: &BalloonUpdateConfig,
    ) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        self.send_curl_request("PATCH", "balloon", &json)
    }

    pub fn api_patch_ballon_update_stats(
        &self,
        data: &BalloonUpdateStatsConfig,
    ) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        self.send_curl_request("PATCH", "balloon", &json)
    }

    pub fn api_put_boot_source(&self, data: &BootSourceConfig) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        self.send_curl_request("PUT", "boot-source", &json)
    }

    pub fn api_put_cpu_config(&self, data: &CustomCpuTemplate) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        self.send_curl_request("PUT", "cpu-config", &json)
    }

    pub fn api_put_drive(&self, data: &BlockDeviceConfig) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        let destination = format!("drives/{}", data.drive_id);
        self.send_curl_request("PUT", &destination, &json)
    }

    pub fn api_put_entropy(&self, data: &EntropyDeviceConfig) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        self.send_curl_request("PUT", "entropy", &json)
    }

    pub fn api_get_instance_info(&self) -> Result<(), std::io::Error> {
        // let json = serde_json::to_string(&data).unwrap();
        // self.send_curl_request("PUT", "entropy", &json)
        Ok(())
    }

    pub fn api_put_logger(&self, data: &LoggerConfig) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        self.send_curl_request("PUT", "logger", &json)
    }

    pub fn api_put_machine_config(&self, data: &MachineConfig) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        self.send_curl_request("PUT", "machine-config", &json)
    }

    pub fn api_patch_machine_config(
        &self,
        data: &MachineConfigUpdate,
    ) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        self.send_curl_request("PATCH", "machine-config", &json)
    }

    pub fn api_put_metrics(&self, data: &MetricsConfig) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        self.send_curl_request("PUT", "metrics", &json)
    }

    pub fn api_get_mmds(&self) -> Result<(), std::io::Error> {
        // let json = serde_json::to_string(&data).unwrap();
        // self.send_curl_request("PUT", "metrics", &json)
        Ok(())
    }

    pub fn api_put_mmds(&self, _data: &MmdsConfig) -> Result<(), std::io::Error> {
        // let json = serde_json::to_string(&data).unwrap();
        // self.send_curl_request("PUT", "metrics", &json)
        Ok(())
    }

    pub fn api_put_mmds_config(&self, data: &MmdsConfig) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        self.send_curl_request("PUT", "", &json)
    }

    pub fn api_put_network(&self, data: &NetworkInterfaceConfig) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        let destination = format!("network-interfaces/{}", data.iface_id);
        self.send_curl_request("PUT", &destination, &json)
    }

    pub fn api_patch_network(&self, data: &NetworkInterfaceConfig) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        let destination = format!("network-interfaces/{}", data.iface_id);
        self.send_curl_request("PATCH", &destination, &json)
    }

    pub fn api_put_snapshot_create(
        &self,
        data: &CreateSnapshotParams,
    ) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        self.send_curl_request("PUT", "snapshot", &json)
    }

    pub fn api_put_snapshot_load(&self, data: &LoadSnapshotConfig) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        self.send_curl_request("PUT", "snapshot", &json)
    }

    pub fn api_get_version(&self) -> Result<(), std::io::Error> {
        // let json = serde_json::to_string(&data).unwrap();
        // self.send_curl_request("PUT", "", &json)
        Ok(())
    }

    pub fn api_put_vsock(&self, data: &VsockDeviceConfig) -> Result<(), std::io::Error> {
        let json = serde_json::to_string(&data).unwrap();
        self.send_curl_request("PUT", "vsock", &json)
    }
}

impl Drop for Fc {
    fn drop(&mut self) {
        self.kill().expect("Fc should stop");
    }
}

#[derive(Debug)]
pub struct SshConnection {
    child: Child,
}

impl SshConnection {
    pub fn ssh(
        key_path: impl AsRef<Path>,
        command: &str,
    ) -> Result<(String, String), std::io::Error> {
        let key_path: &Path = key_path.as_ref();
        let key_path = key_path.to_str().unwrap();
        let mut c = Command::new("ssh");
        c.args([
            "-o",
            "ConnectTimeout=1",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "PreferredAuthentications=publickey",
            "-i",
            key_path,
            "root@172.16.0.2",
            command,
        ]);
        c.stdout(Stdio::piped());
        c.stderr(Stdio::piped());
        let mut command_handle = c.spawn()?;
        let mut stdout = String::new();
        _ = command_handle
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout);
        let mut stderr = String::new();
        _ = command_handle
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr);
        Ok((stdout, stderr))
    }

    pub fn ssh_no_block(key_path: impl AsRef<Path>, command: &str) -> Result<Self, std::io::Error> {
        let key_path: &Path = key_path.as_ref();
        let key_path = key_path.to_str().unwrap();
        let mut c = Command::new("ssh");
        c.args([
            "-o",
            "ConnectTimeout=1",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "PreferredAuthentications=publickey",
            "-i",
            key_path,
            "root@172.16.0.2",
            command,
        ]);
        c.stdout(Stdio::piped());
        c.stderr(Stdio::piped());
        let child = c.spawn()?;
        Ok(Self { child })
    }

    pub fn stdout(&mut self) -> String {
        let mut stdout = String::new();
        _ = self
            .child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout);
        stdout
    }

    pub fn stderr(&mut self) -> String {
        let mut stderr = String::new();
        _ = self
            .child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr);
        stderr
    }
}
