// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
use std::os::fd::AsRawFd;
use utils::epoll::EventSet;

use super::io::FileEngine;
use crate::devices::virtio::block::virtio::device::VirtioBlock;
use crate::logger::info;

impl event_manager::RegisterEvents for VirtioBlock {
    fn register(&mut self, event_manager: &mut event_manager::BufferedEventManager) {
        info!("Registering {}", std::any::type_name::<Self>());
        let self_ptr = self as *const Self;
        let action = Box::new(
            move |_event_manager: &mut event_manager::EventManager, _event_set: EventSet| {
                let self_mut_ref: &mut Self = unsafe { std::mem::transmute(self_ptr) };
                self_mut_ref.process_queue_event();
            },
        );
        event_manager
            .add(self.queue_evts[0].as_raw_fd(), EventSet::IN, action)
            .expect("failed to register event");

        let action = Box::new(
            move |_event_manager: &mut event_manager::EventManager, _event_set: EventSet| {
                let self_mut_ref: &mut Self = unsafe { std::mem::transmute(self_ptr) };
                self_mut_ref.process_rate_limiter_event();
            },
        );
        event_manager
            .add(self.rate_limiter.as_raw_fd(), EventSet::IN, action)
            .expect("failed to register event");

        if let FileEngine::Async(ref engine) = self.disk.file_engine {
            let action = Box::new(
                move |_event_manager: &mut event_manager::EventManager, _event_set: EventSet| {
                    let self_mut_ref: &mut Self = unsafe { std::mem::transmute(self_ptr) };
                    self_mut_ref.process_async_completion_event()
                },
            );
            event_manager
                .add(engine.completion_evt().as_raw_fd(), EventSet::IN, action)
                .expect("failed to register event");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use event_manager::{EventManager, SubscriberOps};

    use super::*;
    use crate::devices::virtio::block::virtio::device::FileEngineType;
    use crate::devices::virtio::block::virtio::test_utils::{
        default_block, read_blk_req_descriptors, set_queue, simulate_async_completion_event,
    };
    use crate::devices::virtio::block::virtio::{VIRTIO_BLK_S_OK, VIRTIO_BLK_T_OUT};
    use crate::devices::virtio::queue::VIRTQ_DESC_F_NEXT;
    use crate::devices::virtio::test_utils::{default_mem, VirtQueue};
    use crate::vstate::memory::{Bytes, GuestAddress};

    #[test]
    fn test_event_handler() {
        let mut event_manager = EventManager::new().unwrap();
        let mut block = default_block(FileEngineType::default());
        let mem = default_mem();
        let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
        set_queue(&mut block, 0, vq.create_queue());
        read_blk_req_descriptors(&vq);

        let block = Arc::new(Mutex::new(block));
        let _id = event_manager.add_subscriber(block.clone());

        let request_type_addr = GuestAddress(vq.dtable[0].addr.get());
        let data_addr = GuestAddress(vq.dtable[1].addr.get());
        let status_addr = GuestAddress(vq.dtable[2].addr.get());

        // Push a 'Write' operation.
        {
            mem.write_obj::<u32>(VIRTIO_BLK_T_OUT, request_type_addr)
                .unwrap();
            // Make data read only, 512 bytes in len, and set the actual value to be written.
            vq.dtable[1].flags.set(VIRTQ_DESC_F_NEXT);
            vq.dtable[1].len.set(512);
            mem.write_obj::<u64>(123_456_789, data_addr).unwrap();

            // Trigger the queue event.
            block.lock().unwrap().queue_evts[0].write(1).unwrap();
        }

        // EventManager should report no events since block has only registered
        // its activation event so far (even though queue event is pending).
        let ev_count = event_manager.run_with_timeout(50).unwrap();
        assert_eq!(ev_count, 0);

        // Now activate the device.
        block.lock().unwrap().activate(mem.clone()).unwrap();
        // Process the activate event.
        let ev_count = event_manager.run_with_timeout(50).unwrap();
        assert_eq!(ev_count, 1);

        // Handle the pending queue event through EventManager.
        event_manager
            .run_with_timeout(100)
            .expect("Metrics event timeout or error.");
        // Complete async IO ops if needed
        simulate_async_completion_event(&mut block.lock().unwrap(), true);

        assert_eq!(vq.used.idx.get(), 1);
        assert_eq!(vq.used.ring[0].get().id, 0);
        assert_eq!(vq.used.ring[0].get().len, 1);
        assert_eq!(mem.read_obj::<u32>(status_addr).unwrap(), VIRTIO_BLK_S_OK);
    }
}
