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
    mem_slot: u32,
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
        }
    }

    fn restore(
        constructor_args: Self::ConstructorArgs,
        state: &Self::State,
    ) -> std::result::Result<Self, Self::Error> {
        let queues = state.virtio_state.build_queues_checked(
            &constructor_args.mem,
            TYPE_PMEM,
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
        )?;
        pmem.set_mem_region(constructor_args.vm_fd);

        pmem.avail_features = state.virtio_state.avail_features;
        pmem.acked_features = state.virtio_state.acked_features;
        pmem.irq_trigger.irq_status = Arc::new(AtomicU32::new(state.virtio_state.interrupt_status));
        if state.virtio_state.activated {
            pmem.device_state = DeviceState::Activated(constructor_args.mem.clone());
        }

        Ok(pmem)
    }
}
