// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::os::fd::AsRawFd;

use log::info;
use utils::epoll::EventSet;

use crate::devices::virtio::net::device::Net;
use crate::devices::virtio::net::{RX_INDEX, TX_INDEX};

impl event_manager::RegisterEvents for Net {
    fn register(&mut self, event_manager: &mut event_manager::BufferedEventManager) {
        info!("Registering {}", std::any::type_name::<Self>());
        let self_ptr = self as *const Self;

        let action = Box::new(
            move |_event_manager: &mut event_manager::EventManager, _event_set: EventSet| {
                let self_mut_ref: &mut Self = unsafe { std::mem::transmute(self_ptr) };
                self_mut_ref.process_rx_queue_event();
            },
        );
        event_manager
            .add(self.queue_evts[RX_INDEX].as_raw_fd(), EventSet::IN, action)
            .expect("failed to register event");

        let action = Box::new(
            move |_event_manager: &mut event_manager::EventManager, _event_set: EventSet| {
                let self_mut_ref: &mut Self = unsafe { std::mem::transmute(self_ptr) };
                self_mut_ref.process_tx_queue_event();
            },
        );
        event_manager
            .add(self.queue_evts[TX_INDEX].as_raw_fd(), EventSet::IN, action)
            .expect("failed to register event");

        let action = Box::new(
            move |_event_manager: &mut event_manager::EventManager, _event_set: EventSet| {
                let self_mut_ref: &mut Self = unsafe { std::mem::transmute(self_ptr) };
                self_mut_ref.process_tap_rx_event();
            },
        );
        event_manager
            .add(self.tap.as_raw_fd(), EventSet::IN, action)
            .expect("failed to register event");

        let action = Box::new(
            move |_event_manager: &mut event_manager::EventManager, _event_set: EventSet| {
                let self_mut_ref: &mut Self = unsafe { std::mem::transmute(self_ptr) };
                self_mut_ref.process_rx_rate_limiter_event();
            },
        );
        event_manager
            .add(self.rx_rate_limiter.as_raw_fd(), EventSet::IN, action)
            .expect("failed to register event");

        let action = Box::new(
            move |_event_manager: &mut event_manager::EventManager, _event_set: EventSet| {
                let self_mut_ref: &mut Self = unsafe { std::mem::transmute(self_ptr) };
                self_mut_ref.process_tx_rate_limiter_event();
            },
        );
        event_manager
            .add(self.tx_rate_limiter.as_raw_fd(), EventSet::IN, action)
            .expect("failed to register event");
    }
}

#[cfg(test)]
pub mod tests {
    use crate::devices::virtio::net::test_utils::test::TestHelper;
    use crate::devices::virtio::net::test_utils::NetQueue;
    use crate::devices::virtio::net::TX_INDEX;

    #[test]
    fn test_event_handler() {
        let mut th = TestHelper::get_default();

        // Push a queue event, use the TX_QUEUE_EVENT in this test.
        th.add_desc_chain(NetQueue::Tx, 0, &[(0, 4096, 0)]);

        // EventManager should report no events since net has only registered
        // its activation event so far (even though there is also a queue event pending).
        let ev_count = th.event_manager.run_with_timeout(50).unwrap();
        assert_eq!(ev_count, 0);

        // Manually force a queue event and check it's ignored pre-activation.
        th.net().queue_evts[TX_INDEX].write(1).unwrap();
        let ev_count = th.event_manager.run_with_timeout(50).unwrap();
        assert_eq!(ev_count, 0);
        // Validate there was no queue operation.
        assert_eq!(th.txq.used.idx.get(), 0);

        // Now activate the device.
        th.activate_net();
        // Handle the previously pushed queue event through EventManager.
        th.event_manager
            .run_with_timeout(50)
            .expect("Metrics event timeout or error.");
        // Make sure the data queue advanced.
        assert_eq!(th.txq.used.idx.get(), 1);
    }
}
