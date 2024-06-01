// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::os::fd::AsRawFd;

use utils::epoll::EventSet;

use super::{report_balloon_event_fail, DEFLATE_INDEX, INFLATE_INDEX, STATS_INDEX};
use crate::devices::virtio::balloon::device::Balloon;

impl event_manager::RegisterEvents for Balloon {
    fn register(&mut self, event_manager: &mut event_manager::BufferedEventManager) {
        let self_ptr = self as *const Self;
        let action = Box::new(
            move |_event_manager: &mut event_manager::EventManager, _event_set: EventSet| {
                let self_mut_ref: &mut Self = unsafe { std::mem::transmute(self_ptr) };
                self_mut_ref
                    .process_inflate_queue_event()
                    .unwrap_or_else(report_balloon_event_fail);
            },
        );
        event_manager
            .add(
                self.queue_evts[INFLATE_INDEX].as_raw_fd(),
                EventSet::IN,
                action,
            )
            .expect("failed to register event");

        let action = Box::new(
            move |_event_manager: &mut event_manager::EventManager, _event_set: EventSet| {
                let self_mut_ref: &mut Self = unsafe { std::mem::transmute(self_ptr) };
                self_mut_ref
                    .process_deflate_queue_event()
                    .unwrap_or_else(report_balloon_event_fail);
            },
        );
        event_manager
            .add(
                self.queue_evts[DEFLATE_INDEX].as_raw_fd(),
                EventSet::IN,
                action,
            )
            .expect("failed to register event");

        if self.stats_enabled() {
            let action = Box::new(
                move |_event_manager: &mut event_manager::EventManager, _event_set: EventSet| {
                    let self_mut_ref: &mut Self = unsafe { std::mem::transmute(self_ptr) };
                    self_mut_ref
                        .process_stats_queue_event()
                        .unwrap_or_else(report_balloon_event_fail);
                },
            );
            event_manager
                .add(
                    self.queue_evts[STATS_INDEX].as_raw_fd(),
                    EventSet::IN,
                    action,
                )
                .expect("failed to register event");

            let action = Box::new(
                move |_event_manager: &mut event_manager::EventManager, _event_set: EventSet| {
                    let self_mut_ref: &mut Self = unsafe { std::mem::transmute(self_ptr) };
                    self_mut_ref
                        .process_stats_timer_event()
                        .unwrap_or_else(report_balloon_event_fail);
                },
            );
            event_manager
                .add(self.stats_timer.as_raw_fd(), EventSet::IN, action)
                .expect("failed to register event");
        }
    }
}

#[cfg(test)]
pub mod tests {
    use std::sync::{Arc, Mutex};

    use event_manager::{EventManager, SubscriberOps};

    use super::*;
    use crate::devices::virtio::balloon::test_utils::set_request;
    use crate::devices::virtio::test_utils::{default_mem, VirtQueue};
    use crate::vstate::memory::GuestAddress;

    #[test]
    fn test_event_handler() {
        let mut event_manager = EventManager::new().unwrap();
        let mut balloon = Balloon::new(0, true, 10, false).unwrap();
        let mem = default_mem();
        let infq = VirtQueue::new(GuestAddress(0), &mem, 16);
        balloon.set_queue(INFLATE_INDEX, infq.create_queue());

        let balloon = Arc::new(Mutex::new(balloon));
        let _id = event_manager.add_subscriber(balloon.clone());

        // Push a queue event, use the inflate queue in this test.
        {
            let addr = 0x100;
            set_request(&infq, 0, addr, 4, 0);
            balloon.lock().unwrap().queue_evts[INFLATE_INDEX]
                .write(1)
                .unwrap();
        }

        // EventManager should report no events since balloon has only registered
        // its activation event so far (even though there is also a queue event pending).
        let ev_count = event_manager.run_with_timeout(50).unwrap();
        assert_eq!(ev_count, 0);

        // Manually force a queue event and check it's ignored pre-activation.
        {
            let b = balloon.lock().unwrap();
            // Artificially push event.
            b.queue_evts[INFLATE_INDEX].write(1).unwrap();
            // Process the pushed event.
            let ev_count = event_manager.run_with_timeout(50).unwrap();
            // Validate there was no queue operation.
            assert_eq!(ev_count, 0);
            assert_eq!(infq.used.idx.get(), 0);
        }

        // Now activate the device.
        balloon.lock().unwrap().activate(mem.clone()).unwrap();
        // Process the activate event.
        let ev_count = event_manager.run_with_timeout(50).unwrap();
        assert_eq!(ev_count, 1);

        // Handle the previously pushed queue event through EventManager.
        event_manager
            .run_with_timeout(100)
            .expect("Metrics event timeout or error.");
        // Make sure the data queue advanced.
        assert_eq!(infq.used.idx.get(), 1);
    }
}
