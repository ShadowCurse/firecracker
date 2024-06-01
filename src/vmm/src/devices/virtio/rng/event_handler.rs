// Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::os::fd::AsRawFd;

use log::info;
use utils::epoll::EventSet;

use super::{Entropy, RNG_QUEUE};
use crate::devices::virtio::device::VirtioDevice;

impl event_manager::RegisterEvents for Entropy {
    fn register(&mut self, event_manager: &mut event_manager::BufferedEventManager) {
        info!("Registering {}", std::any::type_name::<Self>());
        let self_ptr = self as *const Self;
        let action = Box::new(
            move |_event_manager: &mut event_manager::EventManager, _event_set: EventSet| {
                let self_mut_ref: &mut Self = unsafe { std::mem::transmute(self_ptr) };
                self_mut_ref.process_entropy_queue_event();
            },
        );
        event_manager
            .add(
                self.queue_events()[RNG_QUEUE].as_raw_fd(),
                EventSet::IN,
                action,
            )
            .expect("failed to register event");

        let action = Box::new(
            move |_event_manager: &mut event_manager::EventManager, _event_set: EventSet| {
                let self_mut_ref: &mut Self = unsafe { std::mem::transmute(self_ptr) };
                self_mut_ref.process_rate_limiter_event();
            },
        );
        event_manager
            .add(self.rate_limiter().as_raw_fd(), EventSet::IN, action)
            .expect("failed to register event");
    }
}
