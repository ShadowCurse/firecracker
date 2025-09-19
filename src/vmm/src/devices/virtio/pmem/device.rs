// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::fs::OpenOptions;
use std::ops::Deref;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};

use kvm_ioctls::VmFd;
use log::debug;
use log::error;
use vm_memory::GuestAddress;
use vm_memory::GuestMemoryError;
use vm_memory::mmap::MmapRegionBuilder;
use vm_memory::mmap::MmapRegionError;
use vmm_sys_util::eventfd::EventFd;

use crate::devices::virtio::ActivateError;
use crate::devices::virtio::device::{ActiveState, DeviceState, VirtioDevice};
use crate::devices::virtio::generated::virtio_config::VIRTIO_F_VERSION_1;
use crate::devices::virtio::generated::virtio_ids::VIRTIO_ID_PMEM;
use crate::devices::virtio::pmem::PMEM_QUEUE_SIZE;
use crate::devices::virtio::pmem::metrics::{PmemDeviceMetrics, PmemMetricsPerDevice};
use crate::devices::virtio::queue::DescriptorChain;
use crate::devices::virtio::queue::InvalidAvailIdx;
use crate::devices::virtio::queue::Queue;
use crate::devices::virtio::queue::QueueError;
use crate::devices::virtio::transport::{VirtioInterrupt, VirtioInterruptType};
use crate::logger::IncMetric;
use crate::utils::u64_to_usize;
use crate::vmm_config::pmem::PmemDeviceConfig;
use crate::vstate::memory::GuestMmapRegion;
use crate::vstate::memory::{ByteValued, Bytes, GuestMemoryMmap};

use crate::impl_device_type;

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum PmemError {
    /// Error accessing backing file: {0}
    BackingFileIo(std::io::Error),
    /// Error with EventFd: {0}
    EventFd(std::io::Error),
    /// Unexpected read-only descriptor
    ReadOnlyDescriptor,
    /// Unexpected write-only descriptor
    WriteOnlyDescriptor,
    /// UnknownRequestType: {0}
    UnknownRequestType(u32),
    /// Descriptor chain too short
    DescriptorChainTooShort,
    /// Guest memory error: {0}
    GuestMemory(#[from] GuestMemoryError),
    /// Error handling the VirtIO queue: {0}
    Queue(#[from] QueueError),
    /// Error during obtaining the descriptor from the queue: {0}
    QueuePop(#[from] InvalidAvailIdx),
}

const VIRTIO_PMEM_REQ_TYPE_FLUSH: u32 = 0;
const SUCCESS: i32 = 0;
const FAILURE: i32 = -1;

#[derive(Debug)]
pub struct Pmem {
    // VirtIO fields
    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,
    pub(crate) activate_event: EventFd,

    // Transport fields
    pub(crate) device_state: DeviceState,
    pub queues: Vec<Queue>,
    queue_events: Vec<EventFd>,

    // Pmem specific fields
    pub config_space: ConfigSpace,
    pub file: File,
    pub mmap_ptr: u64,

    pub(crate) metrics: Arc<PmemDeviceMetrics>,

    pub config: PmemDeviceConfig,
}

impl Pmem {
    // Pmem devices need to have address and size to be
    // a multiple of 2MB
    pub const ALIGNMENT: u64 = 2 * 1024 * 1024;

    /// Create a new Pmem device with a backing file at `disk_image_path` path.
    pub fn new(config: PmemDeviceConfig) -> Result<Self, PmemError> {
        Self::new_with_queues(config, vec![Queue::new(PMEM_QUEUE_SIZE)])
    }

    /// Create a new Pmem device with a backing file at `disk_image_path` path using a pre-created
    /// set of queues.
    pub fn new_with_queues(
        config: PmemDeviceConfig,
        queues: Vec<Queue>,
    ) -> Result<Self, PmemError> {
        let (file, mmap_ptr, mmap_len) =
            Self::mmap_backing_file(&config.path_on_host, config.read_only, config.shared)?;

        Ok(Self {
            avail_features: 1u64 << VIRTIO_F_VERSION_1,
            acked_features: 0u64,
            activate_event: EventFd::new(libc::EFD_NONBLOCK).map_err(PmemError::EventFd)?,
            device_state: DeviceState::Inactive,
            queues,
            queue_events: vec![EventFd::new(libc::EFD_NONBLOCK).map_err(PmemError::EventFd)?],
            config_space: ConfigSpace {
                start: 0,
                size: mmap_len,
            },
            file,
            mmap_ptr,
            metrics: PmemMetricsPerDevice::alloc(config.id.clone()),
            config,
        })
    }

    pub fn mmap_backing_file(
        path: &str,
        read_only: bool,
        shared: bool,
    ) -> Result<(File, u64, u64), PmemError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(PmemError::BackingFileIo)?;
        let file_size = file.metadata().unwrap().len();
        let mmap_len = crate::utils::align_up(file_size, Self::ALIGNMENT);

        let mut flags_1 = libc::MAP_ANONYMOUS | libc::MAP_NORESERVE;
        let mut flags_2 = libc::MAP_NORESERVE | libc::MAP_FIXED;
        if shared {
            flags_1 |= libc::MAP_SHARED;
            flags_2 |= libc::MAP_SHARED;
        } else {
            flags_1 |= libc::MAP_PRIVATE;
            flags_2 |= libc::MAP_PRIVATE;
        }
        let mut prot = libc::PROT_READ;
        if !read_only {
            prot |= libc::PROT_WRITE;
        }
        unsafe {
            let mmap_ptr = libc::mmap(
                std::ptr::null_mut(),
                u64_to_usize(mmap_len),
                prot,
                flags_1,
                -1,
                0,
            );
            _ = libc::mmap(
                mmap_ptr,
                u64_to_usize(file_size),
                prot,
                flags_2,
                file.as_raw_fd(),
                0,
            );
            Ok((file, mmap_ptr as u64, mmap_len))
        }
    }

    pub fn mmap_region(&self) -> GuestMmapRegion {
        let mut prot = libc::PROT_READ;
        if !self.config.read_only {
            prot |= libc::PROT_WRITE;
        }
        unsafe {
            MmapRegionBuilder::new(u64_to_usize(self.config_space.size))
                .with_mmap_prot(prot)
                .with_raw_mmap_pointer(self.mmap_ptr as *mut u8)
                .build()
                .unwrap()
        }
    }

    pub fn set_mem_region(&self, slot: u32, vm_fd: &VmFd) {
        use kvm_bindings::kvm_userspace_memory_region;
        let memory_region = kvm_userspace_memory_region {
            slot: slot,
            guest_phys_addr: self.config_space.start,
            memory_size: self.config_space.size,
            userspace_addr: self.mmap_ptr,
            flags: 0,
        };
        unsafe {
            vm_fd.set_user_memory_region(memory_region).unwrap();
        }
    }

    /// Return the drive id
    pub fn id(&self) -> &str {
        &self.config.id
    }

    fn handle_queue(&mut self) -> Result<(), PmemError> {
        // This is safe since we checked in the event handler that the device is activated.
        let active_state = self.device_state.active_state().unwrap();

        while let Some(head) = self.queues[0].pop()? {
            let add_result = match self.process_chain(head) {
                Ok(()) => self.queues[0].add_used(head.index, 4),
                Err(err) => {
                    error!("pmem: {err}");
                    self.metrics.event_fails.inc();
                    self.queues[0].add_used(head.index, 0)
                }
            };
            match add_result {
                Ok(()) => {}
                Err(err) => {
                    error!("pmem: {err}");
                    self.metrics.event_fails.inc();
                    break;
                }
            }
        }
        self.queues[0].advance_used_ring_idx();

        if self.queues[0].prepare_kick() {
            active_state
                .interrupt
                .trigger(VirtioInterruptType::Queue(0))
                .unwrap_or_else(|err| {
                    error!("pmem: {err}");
                    self.metrics.event_fails.inc();
                });
        }
        Ok(())
    }

    fn process_chain(&self, head: DescriptorChain) -> Result<(), PmemError> {
        // This is safe since we checked in the event handler that the device is activated.
        let active_state = self.device_state.active_state().unwrap();

        if head.is_write_only() {
            return Err(PmemError::WriteOnlyDescriptor);
        }
        let request: u32 = active_state.mem.read_obj(head.addr)?;
        if request != VIRTIO_PMEM_REQ_TYPE_FLUSH {
            return Err(PmemError::UnknownRequestType(request));
        }
        let Some(status_descriptor) = head.next_descriptor() else {
            return Err(PmemError::DescriptorChainTooShort);
        };
        if !status_descriptor.is_write_only() {
            return Err(PmemError::ReadOnlyDescriptor);
        }
        let mut result = SUCCESS;
        unsafe {
            if libc::msync(
                self.mmap_ptr as *mut libc::c_void,
                u64_to_usize(self.config_space.size),
                libc::MS_SYNC,
            ) < 0
            {
                result = FAILURE;
            }
        }
        active_state.mem.write_obj(result, status_descriptor.addr)?;
        return Ok(());
    }

    pub fn process_queue(&mut self) {
        self.metrics.queue_event_count.inc();
        if let Err(err) = self.queue_events[0].read() {
            error!("pmem: Failed to get queue event: {err:?}");
            self.metrics.event_fails.inc();
            return;
        }

        self.handle_queue().unwrap_or_else(|err| {
            error!("pmem: {err:?}");
            self.metrics.event_fails.inc();
        });
    }
}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct ConfigSpace {
    // Physical address of the first byte of the persistent memory region.
    pub start: u64,
    // Length of the address range
    pub size: u64,
}

// SAFETY: `ConfigSpace` contains only PODs in `repr(c)`, without padding.
unsafe impl ByteValued for ConfigSpace {}

impl VirtioDevice for Pmem {
    impl_device_type!(VIRTIO_ID_PMEM);

    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features;
    }

    fn queues(&self) -> &[Queue] {
        &self.queues
    }

    fn queues_mut(&mut self) -> &mut [Queue] {
        &mut self.queues
    }

    fn queue_events(&self) -> &[EventFd] {
        &self.queue_events
    }

    fn interrupt_trigger(&self) -> &dyn VirtioInterrupt {
        self.device_state
            .active_state()
            .expect("Device is not implemented")
            .interrupt
            .deref()
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        debug!(
            "pmem: reading {} bytes of PMEM config at offset: {offset}",
            data.len()
        );
        if let Some(config_space_bytes) = self.config_space.as_slice().get(u64_to_usize(offset)..) {
            let len = config_space_bytes.len().min(data.len());
            data[..len].copy_from_slice(&config_space_bytes[..len]);
        } else {
            error!("Failed to read config space");
            self.metrics.cfg_fails.inc();
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: Arc<dyn VirtioInterrupt>,
    ) -> Result<(), ActivateError> {
        for q in self.queues.iter_mut() {
            q.initialize(&mem)
                .map_err(ActivateError::QueueMemoryError)?;
        }

        if self.activate_event.write(1).is_err() {
            self.metrics.activate_fails.inc();
            return Err(ActivateError::EventFd);
        }
        self.device_state = DeviceState::Activated(ActiveState { mem, interrupt });
        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }
}
