// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::devices::virtio::pmem::device::{Pmem, PmemError};

/// Errors associated wit the operations allowed on a pmem device
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum PmemConfigError {
    /// Attempt to add pmem as a root device while the root device defined as a block device
    AddingSecondRootDevice,
    /// A root pmem device already exists
    RootPmemDeviceAlreadyAdded,
    /// Attempt to set pmem to be read only without setting it as a root device
    ReadOnlyNonRootDevice,
    /// Unable to create the virtio-pmem device
    CreatePmemDevice(#[from] PmemError),
    /// Error accessing underlying file
    File(std::io::Error),
}

/// Use this structure to setup a Pmem device before boothing the kernel.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PmemDeviceConfig {
    /// Unique identifier of the device.
    pub id: String,
    /// Path of the drive.
    pub path_on_host: String,
    /// Use this pmem device for rootfs
    pub root_device: bool,
    /// Map the file as read only
    pub read_only: bool,
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

    /// Specifies whether there is a root block device already present in the list.
    pub fn has_root_device(&self) -> bool {
        for device in self.devices.iter() {
            if device.lock().unwrap().config.root_device {
                return true;
            }
        }
        return false;
    }

    /// Build a device from the config
    pub fn build(
        &mut self,
        config: PmemDeviceConfig,
        has_block_root: bool,
    ) -> Result<(), PmemConfigError> {
        if config.root_device && has_block_root {
            return Err(PmemConfigError::AddingSecondRootDevice);
        }
        if config.root_device && self.has_root_device() {
            return Err(PmemConfigError::RootPmemDeviceAlreadyAdded);
        }
        if config.read_only && !config.root_device {
            return Err(PmemConfigError::ReadOnlyNonRootDevice);
        }
        let pmem = Pmem::new(config)?;
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
            .map(|b| b.lock().unwrap().config.clone())
            .collect()
    }
}
