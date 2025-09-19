// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use vm_memory::GuestAddress;

use crate::Vm;
use crate::devices::virtio::device::DeviceState;
use crate::devices::virtio::generated::virtio_ids::VIRTIO_ID_PMEM;
use crate::devices::virtio::persist::{PersistError as VirtioStateError, VirtioDeviceState};
use crate::devices::virtio::pmem::{PMEM_NUM_QUEUES, PMEM_QUEUE_SIZE};
use crate::snapshot::Persist;
use crate::vmm_config::pmem::PmemDeviceConfig;
use crate::vstate::memory::{GuestMemoryMmap, GuestRegionMmap};
use crate::vstate::vm::VmError;

use super::device::{Pmem, PmemError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PmemState {
    pub virtio_state: VirtioDeviceState,
    pub guest_address: u64,
    pub config: PmemDeviceConfig,
}

#[derive(Debug)]
pub struct PmemConstructorArgs<'a> {
    pub mem: &'a GuestMemoryMmap,
    pub vm: &'a Vm,
}

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum PmemPersistError {
    /// Error resetting VirtIO state: {0}
    VirtioState(#[from] VirtioStateError),
    /// Error creating Pmem devie: {0}
    Pmem(#[from] PmemError),
    /// Error registering memory region: {0}
    Vm(#[from] VmError),
}

impl<'a> Persist<'a> for Pmem {
    type State = PmemState;
    type ConstructorArgs = PmemConstructorArgs<'a>;
    type Error = PmemPersistError;

    fn save(&self) -> Self::State {
        PmemState {
            virtio_state: VirtioDeviceState::from_device(self),
            guest_address: self.config_space.start,
            config: self.config.clone(),
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

        let mut pmem = Pmem::new_with_queues(state.config.clone(), queues)?;
        pmem.set_mem_region(11, constructor_args.vm.fd());

        // let region = pmem.mmap_region();
        // let region = GuestRegionMmap::new(region, GuestAddress(state.guest_address)).unwrap();
        // #[allow(mutable_transmutes)]
        // let vm: &mut Vm = unsafe { std::mem::transmute(constructor_args.vm) };
        // vm.register_memory_region(region)?;

        pmem.avail_features = state.virtio_state.avail_features;
        pmem.acked_features = state.virtio_state.acked_features;

        Ok(pmem)
    }
}
