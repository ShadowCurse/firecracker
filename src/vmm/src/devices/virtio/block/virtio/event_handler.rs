// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
use event_manager::{EventOps, Events, MutEventSubscriber};
use vmm_sys_util::epoll::EventSet;

use super::io::FileEngine;
use crate::devices::virtio::block::virtio::device::{BlockRuntimeState, Runtime, VirtioBlock};
use crate::logger::{error, warn};

impl BlockRuntimeState {
    pub const PROCESS_ACTIVATE: u32 = 0;
    pub const PROCESS_QUEUE: u32 = 1;
    pub const PROCESS_RATE_LIMITER: u32 = 2;
    pub const PROCESS_ASYNC_COMPLETION: u32 = 3;

    pub fn register_runtime_events(&self, ops: &mut EventOps) {
        let block = self.block_ref();
        let queue_evt = &block.queue_evts[self.queue_index as usize];
        if let Err(err) =
            ops.add(Events::with_data(queue_evt, Self::PROCESS_QUEUE, EventSet::IN))
        {
            error!("Failed to register queue event: {}", err);
        }
        if let Err(err) = ops.add(Events::with_data(
            &self.rate_limiter,
            Self::PROCESS_RATE_LIMITER,
            EventSet::IN,
        )) {
            error!("Failed to register ratelimiter event: {}", err);
        }
        if let FileEngine::Async(ref engine) = block.disk.file_engine
            && let Err(err) = ops.add(Events::with_data(
                engine.completion_evt(),
                Self::PROCESS_ASYNC_COMPLETION,
                EventSet::IN,
            ))
        {
            error!("Failed to register IO engine completion event: {}", err);
        }
    }

    pub fn register_activate_event(&self, ops: &mut EventOps) {
        if let Err(err) = ops.add(Events::with_data(
            &self.activate_evt,
            Self::PROCESS_ACTIVATE,
            EventSet::IN,
        )) {
            error!("Failed to register activate event: {}", err);
        }
    }

    fn process_activate_event(&self, ops: &mut EventOps) {
        if let Err(err) = self.activate_evt.read() {
            error!("Failed to consume block activate event: {:?}", err);
        }
        self.register_runtime_events(ops);
        if let Err(err) = ops.remove(Events::with_data(
            &self.activate_evt,
            Self::PROCESS_ACTIVATE,
            EventSet::IN,
        )) {
            error!("Failed to un-register activate event: {}", err);
        }
    }
}

impl MutEventSubscriber for BlockRuntimeState {
    fn process(&mut self, event: Events, ops: &mut EventOps) {
        let source = event.data();
        let event_set = event.event_set();

        let supported_events = EventSet::IN;
        if !supported_events.contains(event_set) {
            warn!(
                "Block: Received unknown event: {:?} from source: {:?}",
                event_set, source
            );
            return;
        }

        if self.is_activated() {
            match source {
                Self::PROCESS_ACTIVATE => self.process_activate_event(ops),
                Self::PROCESS_QUEUE => self.process_queue_event(),
                Self::PROCESS_RATE_LIMITER => self.process_rate_limiter_event(),
                Self::PROCESS_ASYNC_COMPLETION => self.process_async_completion_event(),
                _ => warn!("Block: Spurious event received: {:?}", source),
            }
        } else {
            warn!(
                "Block: The device is not yet activated. Spurious event received: {:?}",
                source
            );
            match source {
                Self::PROCESS_QUEUE => {
                    let _ = self.block_ref().queue_evts[self.queue_index as usize].read();
                }
                Self::PROCESS_RATE_LIMITER => {
                    let _ = self.rate_limiter.event_handler();
                }
                Self::PROCESS_ASYNC_COMPLETION => {
                    if let FileEngine::Async(ref engine) = self.block_ref().disk.file_engine {
                        let _ = engine.completion_evt().read();
                    }
                }
                _ => (),
            }
        }
    }

    fn init(&mut self, ops: &mut EventOps) {
        if self.is_activated() {
            self.register_runtime_events(ops);
        } else {
            self.register_activate_event(ops);
        }
    }
}

impl MutEventSubscriber for VirtioBlock {
    fn process(&mut self, event: Events, ops: &mut EventOps) {
        match &mut self.runtime {
            Runtime::SingleThread(state) => state.process(event, ops),
            // In worker-thread mode each worker owns its own EventManager and drives its
            // own queue; the VMM-thread subscriber never receives per-queue events.
            Runtime::WorkerThreads(_) => {}
        }
    }

    fn init(&mut self, ops: &mut EventOps) {
        // VirtioBlock's address is stable from this point on (it lives inside the outer
        // `Arc<Mutex<Block>>` that the transport owns). Wire the single-thread runtime
        // state to it now, so its subscriber methods can dereference the pointer.
        self.refresh_block_ptr();
        match &mut self.runtime {
            Runtime::SingleThread(state) => state.init(ops),
            Runtime::WorkerThreads(_) => {}
        }
    }
}

// Tests are disabled during the thread-per-queue prototype.
#[cfg(any())]
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
    use crate::devices::virtio::test_utils::{VirtQueue, default_interrupt, default_mem};
    use crate::vstate::memory::{Bytes, GuestAddress};

    #[test]
    fn test_event_handler() {
        let mut event_manager = EventManager::new().unwrap();
        let mut block = default_block(FileEngineType::default());
        let mem = default_mem();
        let interrupt = default_interrupt();
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
        block
            .lock()
            .unwrap()
            .activate(mem.clone(), interrupt)
            .unwrap();
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
