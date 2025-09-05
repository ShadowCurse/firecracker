// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::devices::virtio::pmem::device::{Pmem, PmemError};

/// Errors associated wit the operations allowed on a pmem device
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum PmemConfigError {
    /// Unable to create the virtio-pmem device
    CreatePmemDevice(#[from] PmemError),
    /// Error accessing underlying file
    File(std::io::Error),
}

/// Use this structure to setup a Pmem device before boothing the kernel.
#[derive(Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PmemDeviceConfig {
    /// Unique identifier of the device.
    pub id: String,
    /// Path of the drive.
    pub path_on_host: String,
    /// Use this pmem device for rootfs
    pub is_root_device: bool,
    /// Is this a shared memory
    pub shared: bool,
}

/// Wrapper for the collection that holds all the Pmem devices.
#[derive(Debug, Default)]
pub struct PmemBuilder {
    /// The list of pmem devices
    pub devices: Vec<Arc<Mutex<Pmem>>>,
}

impl PmemBuilder {
    /// Constructor for Pmem devices collection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a device from the config
    pub fn build(&mut self, config: PmemDeviceConfig) -> Result<(), PmemConfigError> {
        // TODO do we need to reorder these like we do with block devices.
        let pmem = Pmem::new(
            config.id,
            config.path_on_host,
            config.is_root_device,
            config.shared,
        )?;
        self.devices.push(Arc::new(Mutex::new(pmem)));
        Ok(())
    }

    /// Adds an existing network device in the builder.
    pub fn add_device(&mut self, device: Arc<Mutex<Pmem>>) {
        self.devices.push(device);
    }

    /// Returns a vec with the structures used to configure the devices.
    pub fn configs(&self) -> Vec<PmemDeviceConfig> {
        self.devices
            .iter()
            .map(|b| b.lock().unwrap().config())
            .collect()
    }
}
