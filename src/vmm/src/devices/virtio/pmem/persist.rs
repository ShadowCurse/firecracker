// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::AtomicU32;
use std::sync::Arc;

use kvm_ioctls::VmFd;
use serde::{Deserialize, Serialize};
use vm_memory::GuestAddress;

use crate::devices::virtio::device::DeviceState;
use crate::devices::virtio::persist::{PersistError as VirtioStateError, VirtioDeviceState};
use crate::devices::virtio::pmem::{PMEM_NUM_QUEUES, PMEM_QUEUE_SIZE};
use crate::devices::virtio::generated::virtio_ids::VIRTIO_ID_PMEM;
use crate::snapshot::Persist;
use crate::vstate::memory::GuestMemoryMmap;

use super::device::{Pmem, PmemError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PmemState {
    pub virtio_state: VirtioDeviceState,
    pub drive_id: String,
    pub root_device: bool,
    pub backing_file_path: String,
    pub guest_address: u64,
    pub mem_slot: u32,
    pub shared: bool,
}

#[derive(Debug)]
pub struct PmemConstructorArgs<'a>{
    pub mem: &'a GuestMemoryMmap,
    pub vm_fd: &'a VmFd, 
}

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum PmemPersistError {
    /// Error resetting VirtIO state: {0}
    VirtioState(#[from] VirtioStateError),
    /// Error creating Pmem devie: {0}
    Pmem(#[from] PmemError),
}

impl<'a> Persist<'a> for Pmem {
    type State = PmemState;
    type ConstructorArgs = PmemConstructorArgs<'a>;
    type Error = PmemPersistError;

    fn save(&self) -> Self::State {
        PmemState {
            virtio_state: VirtioDeviceState::from_device(self),
            drive_id: self.drive_id.clone(),
            root_device: self.root_device,
            backing_file_path: self.backing_file_path.clone(),
            guest_address: self.config_space.start,
            mem_slot: self.mem_slot,
            shared: self.shared,
        }
    }

    fn restore(
        constructor_args: Self::ConstructorArgs,
        state: &Self::State,
    ) -> std::result::Result<Self, Self::Error> {
        let queues = state.virtio_state.build_queues_checked(
            &constructor_args.mem,
            VIRTIO_ID_PMEM,
            PMEM_NUM_QUEUES,
            PMEM_QUEUE_SIZE,
        )?;

        let mut pmem = Pmem::new_with_queues(
            queues,
            state.drive_id.clone(),
            state.backing_file_path.clone(),
            state.root_device,
            state.mem_slot,
            state.guest_address,
            state.shared,
        )?;
        pmem.set_mem_region(constructor_args.vm_fd);

        pmem.avail_features = state.virtio_state.avail_features;
        pmem.acked_features = state.virtio_state.acked_features;

        Ok(pmem)
    }
}
