// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::AtomicU32;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use vm_memory::GuestAddress;

use crate::devices::virtio::device::DeviceState;
use crate::devices::virtio::persist::{PersistError as VirtioStateError, VirtioDeviceState};
use crate::devices::virtio::pmem::{PMEM_NUM_QUEUES, PMEM_QUEUE_SIZE};
use crate::devices::virtio::TYPE_PMEM;
use crate::snapshot::Persist;
use crate::vstate::memory::GuestMemoryMmap;

use super::device::{Pmem, PmemError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PmemState {
    virtio_state: VirtioDeviceState,
    drive_id: String,
    root_device: bool,
    backing_file_path: String,
    guest_address: u64,
}

#[derive(Debug)]
pub struct PmemConstructorArgs(GuestMemoryMmap);

impl PmemConstructorArgs {
    pub fn new(mem: GuestMemoryMmap) -> Self {
        Self(mem)
    }
}

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum PmemPersistError {
    /// Error resetting VirtIO state: {0}
    VirtioState(#[from] VirtioStateError),
    /// Error creating Pmem devie: {0}
    Pmem(#[from] PmemError),
}

impl Persist<'_> for Pmem {
    type State = PmemState;
    type ConstructorArgs = PmemConstructorArgs;
    type Error = PmemPersistError;

    fn save(&self) -> Self::State {
        PmemState {
            virtio_state: VirtioDeviceState::from_device(self),
            drive_id: self.drive_id.clone(),
            root_device: self.root_device,
            backing_file_path: self.backing_file_path.clone(),
            guest_address: self.guest_address.0,
        }
    }

    fn restore(
        constructor_args: Self::ConstructorArgs,
        state: &Self::State,
    ) -> std::result::Result<Self, Self::Error> {
        let queues = state.virtio_state.build_queues_checked(
            &constructor_args.0,
            TYPE_PMEM,
            PMEM_NUM_QUEUES,
            PMEM_QUEUE_SIZE,
        )?;

        let mut pmem = Pmem::new_with_queues(
            queues,
            GuestAddress(state.guest_address),
            state.drive_id.clone(),
            state.backing_file_path.clone(),
            state.root_device,
        )?;

        pmem.avail_features = state.virtio_state.avail_features;
        pmem.acked_features = state.virtio_state.acked_features;
        pmem.irq_trigger.irq_status = Arc::new(AtomicU32::new(state.virtio_state.interrupt_status));
        if state.virtio_state.activated {
            pmem.device_state = DeviceState::Activated(constructor_args.0);
        }

        Ok(pmem)
    }
}
