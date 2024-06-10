// Copyright 2023 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::net::Ipv4Addr;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};

use event_manager::{EventOps, Events, MutEventSubscriber};
use utils::eventfd::EventFd;
use utils::net::mac::MacAddr;

use super::persist::{NetConstructorArgs, NetState};
use super::virtio::device::Net as VirtioNet;
use super::NetError;
use crate::devices::virtio::device::VirtioDevice;
use crate::devices::virtio::queue::Queue;
use crate::devices::virtio::{ActivateError, TYPE_NET};
use crate::mmds::data_store::Mmds;
use crate::mmds::ns::MmdsNetworkStack;
use crate::rate_limiter::{BucketUpdate, RateLimiter};
use crate::snapshot::Persist;
use crate::vstate::memory::GuestMemoryMmap;

// Clippy thinks that values of the enum are too different in size.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Net {
    Virtio(VirtioNet),
}

impl Net {
    pub fn new(
        id: String,
        tap_if_name: &str,
        guest_mac: Option<MacAddr>,
        rx_rate_limiter: RateLimiter,
        tx_rate_limiter: RateLimiter,
    ) -> Result<Self, NetError> {
        let net = VirtioNet::new(id, tap_if_name, guest_mac, rx_rate_limiter, tx_rate_limiter)
            .map_err(NetError::VirtioBackend)?;
        Ok(Self::Virtio(net))
    }

    /// Provides the ID of this net device.
    pub fn id(&self) -> &String {
        match self {
            Self::Virtio(b) => b.id(),
        }
    }

    /// Provides the MAC of this net device.
    pub fn guest_mac(&self) -> Option<&MacAddr> {
        match self {
            Self::Virtio(b) => b.guest_mac(),
        }
    }

    /// Provides the host IFACE name of this net device.
    pub fn iface_name(&self) -> String {
        match self {
            Self::Virtio(b) => b.iface_name(),
        }
    }

    /// Provides the MmdsNetworkStack of this net device.
    pub fn mmds_ns(&self) -> Option<&MmdsNetworkStack> {
        match self {
            Self::Virtio(b) => b.mmds_ns(),
        }
    }

    /// Configures the `MmdsNetworkStack` to allow device to forward MMDS requests.
    /// If the device already supports MMDS, updates the IPv4 address.
    pub fn configure_mmds_network_stack(&mut self, ipv4_addr: Ipv4Addr, mmds: Arc<Mutex<Mmds>>) {
        match self {
            Self::Virtio(b) => b.configure_mmds_network_stack(ipv4_addr, mmds),
        }
    }

    /// Disables the `MmdsNetworkStack` to prevent device to forward MMDS requests.
    pub fn disable_mmds_network_stack(&mut self) {
        match self {
            Self::Virtio(b) => b.disable_mmds_network_stack(),
        }
    }

    /// Provides a reference to the configured RX rate limiter.
    pub fn rx_rate_limiter(&self) -> &RateLimiter {
        match self {
            Self::Virtio(b) => b.rx_rate_limiter(),
        }
    }

    /// Provides a reference to the configured TX rate limiter.
    pub fn tx_rate_limiter(&self) -> &RateLimiter {
        match self {
            Self::Virtio(b) => b.tx_rate_limiter(),
        }
    }

    /// Updates the parameters for the rate limiters
    pub fn patch_rate_limiters(
        &mut self,
        rx_bytes: BucketUpdate,
        rx_ops: BucketUpdate,
        tx_bytes: BucketUpdate,
        tx_ops: BucketUpdate,
    ) {
        match self {
            Self::Virtio(b) => b.patch_rate_limiters(rx_bytes, rx_ops, tx_bytes, tx_ops),
        }
    }

    pub fn process_virtio_queues(&mut self) {
        match self {
            Self::Virtio(b) => b.process_virtio_queues(),
        }
    }
}

impl VirtioDevice for Net {
    fn avail_features(&self) -> u64 {
        match self {
            Self::Virtio(b) => b.avail_features,
        }
    }

    fn acked_features(&self) -> u64 {
        match self {
            Self::Virtio(b) => b.acked_features,
        }
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        match self {
            Self::Virtio(b) => b.acked_features = acked_features,
        }
    }

    fn device_type(&self) -> u32 {
        TYPE_NET
    }

    fn queues(&self) -> &[Queue] {
        match self {
            Self::Virtio(b) => &b.queues,
        }
    }

    fn queues_mut(&mut self) -> &mut [Queue] {
        match self {
            Self::Virtio(b) => &mut b.queues,
        }
    }

    fn queue_events(&self) -> &[EventFd] {
        match self {
            Self::Virtio(b) => &b.queue_evts,
        }
    }

    fn interrupt_evt(&self) -> &EventFd {
        match self {
            Self::Virtio(b) => &b.irq_trigger.irq_evt,
        }
    }

    fn interrupt_status(&self) -> Arc<AtomicU32> {
        match self {
            Self::Virtio(b) => b.irq_trigger.irq_status.clone(),
        }
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        match self {
            Self::Virtio(b) => b.read_config(offset, data),
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        match self {
            Self::Virtio(b) => b.write_config(offset, data),
        }
    }

    fn activate(&mut self, mem: GuestMemoryMmap) -> Result<(), ActivateError> {
        match self {
            Self::Virtio(b) => b.activate(mem),
        }
    }

    fn is_activated(&self) -> bool {
        match self {
            Self::Virtio(b) => b.device_state.is_activated(),
        }
    }
}

impl MutEventSubscriber for Net {
    fn process(&mut self, event: Events, ops: &mut EventOps) {
        match self {
            Self::Virtio(b) => b.process(event, ops),
        }
    }

    fn init(&mut self, ops: &mut EventOps) {
        match self {
            Self::Virtio(b) => b.init(ops),
        }
    }
}

impl Persist<'_> for Net {
    type State = NetState;
    type ConstructorArgs = NetConstructorArgs;
    type Error = NetError;

    fn save(&self) -> Self::State {
        match self {
            Self::Virtio(b) => Self::State::Virtio(b.save()),
        }
    }

    fn restore(
        constructor_args: Self::ConstructorArgs,
        state: &Self::State,
    ) -> Result<Self, Self::Error> {
        match state {
            NetState::Virtio(s) => Ok(Self::Virtio(
                VirtioNet::restore(constructor_args, s).map_err(NetError::VirtioBackendPersist)?,
            )),
        }
    }
}
