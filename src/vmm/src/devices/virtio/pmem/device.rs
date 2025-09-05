// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex};
use std::ops::Deref;

use kvm_ioctls::VmFd;
use log::debug;
use log::error;
use vm_memory::GuestAddress;
use vm_memory::GuestMemoryError;
use vmm_sys_util::eventfd::EventFd;

use crate::logger::{IncMetric};
use crate::devices::virtio::pmem::metrics::{PmemDeviceMetrics, PmemMetricsPerDevice};
use crate::devices::virtio::device::{ActiveState, DeviceState, VirtioDevice};
use crate::devices::virtio::generated::virtio_ids::VIRTIO_ID_PMEM;
use crate::devices::virtio::generated::virtio_config::VIRTIO_F_VERSION_1;
use crate::devices::virtio::transport::{VirtioInterrupt, VirtioInterruptType};
use crate::devices::virtio::pmem::PMEM_QUEUE_SIZE;
use crate::devices::virtio::queue::Queue;
use crate::devices::virtio::queue::QueueError;
use crate::devices::virtio::ActivateError;
use crate::utils::u64_to_usize;
use crate::vmm_config::pmem::PmemDeviceConfig;
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
    /// Malformed guest request
    MalformedRequest,
    /// UnknownRequestType: {0}
    UnknownRequestType(u32),
    /// Descriptor chain too short
    DescriptorChainTooShort,
    /// Guest memory error: {0}
    GuestMemory(#[from] GuestMemoryError),
    /// Error handling the VirtIO queue: {0}
    Queue(#[from] QueueError),
}

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
    pub drive_id: String,
    pub root_device: bool,
    pub config_space: ConfigSpace,
    pub backing_file: File,
    pub backing_file_path: String,
    pub backing_file_size: u64,

    pub(crate) metrics: Arc<PmemDeviceMetrics>,

    pub mmaped_file: u64,
    pub mem_slot: u32,
    pub shared: bool,
}

impl Pmem {
    // Pmem devices need to have address and size to be
    // a multiple of 2MB
    pub const ALIGNMENT: u64 = 2 * 1024 * 1024;
    pub const MEM_SLOTS_START: u32 = 10;

    /// Create a new Pmem device with a backing file at `disk_image_path` path.
    pub fn new(
        drive_id: String,
        backing_file_path: String,
        root_device: bool,
        shared: bool,
    ) -> Result<Self, PmemError> {
        let backing_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&backing_file_path)
            .map_err(PmemError::BackingFileIo)?;
        let backing_file_size = backing_file.metadata().unwrap().len();
        let mapping_size = crate::utils::align_up(backing_file_size, Self::ALIGNMENT);

        let mmaped_file = Self::mmap_backing_file(
            backing_file.as_raw_fd(),
            mapping_size as usize,
            backing_file_size as usize,
            shared,
        );

        Ok(Self {
            avail_features: 1u64 << VIRTIO_F_VERSION_1,
            acked_features: 0u64,
            activate_event: EventFd::new(libc::EFD_NONBLOCK).map_err(PmemError::EventFd)?,
            device_state: DeviceState::Inactive,
            queues: vec![Queue::new(PMEM_QUEUE_SIZE)],
            queue_events: vec![EventFd::new(libc::EFD_NONBLOCK).map_err(PmemError::EventFd)?],
            drive_id: drive_id.clone(),
            root_device,
            config_space: ConfigSpace {
                start: 0,
                size: mapping_size,
            },
            backing_file,
            backing_file_path,
            backing_file_size,

            metrics: PmemMetricsPerDevice::alloc(drive_id),

            mmaped_file: mmaped_file as u64,
            mem_slot: 0,
            shared,
        })
    }

    /// Create a new Pmem device with a backing file at `disk_image_path` path using a pre-created
    /// set of queues.
    pub fn new_with_queues(
        queues: Vec<Queue>,
        drive_id: String,
        backing_file_path: String,
        root_device: bool,
        mem_slot: u32,
        guest_address: u64,
        shared: bool
    ) -> Result<Self, PmemError> {
        let backing_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&backing_file_path)
            .map_err(PmemError::BackingFileIo)?;
        let backing_file_size = backing_file.metadata().unwrap().len();
        let mapping_size = (backing_file_size + Self::ALIGNMENT) & !(Self::ALIGNMENT - 1);

        let mmaped_file = Self::mmap_backing_file(
            backing_file.as_raw_fd(),
            mapping_size as usize,
            backing_file_size as usize,
            shared,
        );

        Ok(Self {
            avail_features: 1u64 << VIRTIO_F_VERSION_1,
            acked_features: 0u64,
            activate_event: EventFd::new(libc::EFD_NONBLOCK).map_err(PmemError::EventFd)?,
            device_state: DeviceState::Inactive,
            queues,
            queue_events: vec![EventFd::new(libc::EFD_NONBLOCK).map_err(PmemError::EventFd)?],
            drive_id: drive_id.clone(),
            root_device,
            config_space: ConfigSpace {
                start: guest_address,
                size: mapping_size,
            },
            backing_file,
            backing_file_path,
            backing_file_size,

            metrics: PmemMetricsPerDevice::alloc(drive_id),

            mmaped_file: mmaped_file as u64,
            mem_slot,
            shared,
        })
    }

    pub fn mmap_backing_file(
        raw_fd: RawFd,
        mapping_size: usize,
        backing_file_size: usize,
        shared: bool,
    ) -> *mut libc::c_void {
        let mut flags_1 = libc::MAP_ANONYMOUS | libc::MAP_NORESERVE;
        let mut flags_2 = libc::MAP_NORESERVE | libc::MAP_FIXED;
        if shared {
            flags_1 |= libc::MAP_SHARED;
            flags_2 |= libc::MAP_SHARED;
        } else {
            flags_1 |= libc::MAP_PRIVATE;
            flags_2 |= libc::MAP_PRIVATE;
        }
        unsafe {
            let m = libc::mmap(
                std::ptr::null_mut(),
                mapping_size,
                libc::PROT_READ | libc::PROT_WRITE,
                flags_1,
                -1,
                0,
            );
            _ = libc::mmap(
                m,
                backing_file_size,
                libc::PROT_READ | libc::PROT_WRITE,
                flags_2,
                raw_fd,
                0,
            );
            m
        }
    }

    pub fn set_mem_region(&self, vm_fd: &VmFd) {
        use kvm_bindings::kvm_userspace_memory_region;
        let memory_region = kvm_userspace_memory_region {
            slot: self.mem_slot,
            guest_phys_addr: self.config_space.start,
            memory_size: self.config_space.size,
            userspace_addr: self.mmaped_file,
            flags: 0,
        };
        unsafe {
            vm_fd.set_user_memory_region(memory_region).unwrap();
        }
    }

    /// Return the drive id
    pub fn id(&self) -> &str {
        &self.drive_id
    }

    fn handle_queue(&mut self) -> Result<(), PmemError> {
        // This is safe since we checked in the event handler that the device is activated.
        let active_state = self.device_state.active_state().unwrap();

        let queue = &mut self.queues[0];
        while let Some(head) = queue.pop_or_enable_notification().unwrap() {
            let status_descriptor = head.next_descriptor().unwrap();
            active_state.mem.write_obj(0u32, status_descriptor.addr)?;
            queue.add_used(head.index, 4)?;
        }
        queue.advance_used_ring_idx();

        if queue.prepare_kick() {
            active_state
                .interrupt
                .trigger(VirtioInterruptType::Queue(0))
                .unwrap_or_else(|_| {
                    self.metrics.event_fails.inc();
                });
        }

        Ok(())
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

    pub fn config(&self) -> PmemDeviceConfig {
        PmemDeviceConfig {
            id: self.drive_id.clone(),
            path_on_host: self.backing_file_path.clone(),
            is_root_device: false,
            shared: self.shared,
        }
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

    fn activate(&mut self, mem: GuestMemoryMmap,
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
