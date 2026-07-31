// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::cmp;
use std::convert::From;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::ops::Deref;
use std::os::linux::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use block_io::FileEngine;
use serde::{Deserialize, Serialize};
use vm_memory::ByteValued;
use vmm_sys_util::eventfd::EventFd;

use super::io::async_io;
use super::request::*;
use super::{SECTOR_SHIFT, SECTOR_SIZE, VirtioBlockError, io as block_io};
use crate::devices::virtio::ActivateError;
use crate::devices::virtio::block::CacheType;
use crate::devices::virtio::block::virtio::metrics::{BlockDeviceMetrics, BlockMetricsPerDevice};
use crate::devices::virtio::device::{ActiveState, DeviceState, VirtioDevice, VirtioDeviceType};
use crate::devices::virtio::generated::virtio_blk::{
    VIRTIO_BLK_F_FLUSH, VIRTIO_BLK_F_MQ, VIRTIO_BLK_F_RO, VIRTIO_BLK_ID_BYTES,
};
use crate::devices::virtio::generated::virtio_config::VIRTIO_F_VERSION_1;
use crate::devices::virtio::generated::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use crate::devices::virtio::queue::{FIRECRACKER_MAX_QUEUE_SIZE, InvalidAvailIdx, Queue};
use crate::devices::virtio::transport::{VirtioInterrupt, VirtioInterruptType};
use crate::impl_device_type;
use crate::logger::{IncMetric, error, warn};
use crate::rate_limiter::{BucketUpdate, RateLimiter};
use crate::utils::u64_to_usize;
use crate::vmm_config::RateLimiterConfig;
use crate::vmm_config::drive::BlockDeviceConfig;
use crate::vstate::memory::GuestMemoryMmap;

/// The engine file type, either Sync or Async (through io_uring).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum FileEngineType {
    /// Use an Async engine, based on io_uring.
    Async,
    /// Use a Sync engine, based on blocking system calls.
    #[default]
    Sync,
}

/// Helper object for setting up all `Block` fields derived from its backing file.
#[derive(Debug)]
pub struct DiskProperties {
    pub file_path: String,
    pub file_engine: FileEngine,
    pub nsectors: u64,
    pub image_id: [u8; VIRTIO_BLK_ID_BYTES as usize],
}

impl DiskProperties {
    // Helper function that opens the file with the proper access permissions
    fn open_file(disk_image_path: &str, is_disk_read_only: bool) -> Result<File, VirtioBlockError> {
        OpenOptions::new()
            .read(true)
            .write(!is_disk_read_only)
            .open(PathBuf::from(&disk_image_path))
            .map_err(|x| VirtioBlockError::BackingFile(x, disk_image_path.to_string()))
    }

    // Helper function that gets the size of the file
    fn file_size(disk_image_path: &str, disk_image: &mut File) -> Result<u64, VirtioBlockError> {
        let disk_size = disk_image
            .seek(SeekFrom::End(0))
            .map_err(|x| VirtioBlockError::BackingFile(x, disk_image_path.to_string()))?;

        // We only support disk size, which uses the first two words of the configuration space.
        // If the image is not a multiple of the sector size, the tail bits are not exposed.
        if disk_size % u64::from(SECTOR_SIZE) != 0 {
            warn!(
                "Disk size {} is not a multiple of sector size {}; the remainder will not be \
                 visible to the guest.",
                disk_size, SECTOR_SIZE
            );
        }

        Ok(disk_size)
    }

    /// Create a new file for the block device using a FileEngine
    pub fn new(
        disk_image_path: String,
        is_disk_read_only: bool,
        file_engine_type: FileEngineType,
    ) -> Result<Self, VirtioBlockError> {
        let mut disk_image = Self::open_file(&disk_image_path, is_disk_read_only)?;
        let disk_size = Self::file_size(&disk_image_path, &mut disk_image)?;
        let image_id = Self::build_disk_image_id(&disk_image);

        Ok(Self {
            file_path: disk_image_path,
            file_engine: FileEngine::from_file(disk_image, file_engine_type)
                .map_err(VirtioBlockError::FileEngine)?,
            nsectors: disk_size >> SECTOR_SHIFT,
            image_id,
        })
    }

    /// Update the path to the file backing the block device
    pub fn update(
        &mut self,
        disk_image_path: String,
        is_disk_read_only: bool,
    ) -> Result<(), VirtioBlockError> {
        let mut disk_image = Self::open_file(&disk_image_path, is_disk_read_only)?;
        let disk_size = Self::file_size(&disk_image_path, &mut disk_image)?;

        self.image_id = Self::build_disk_image_id(&disk_image);
        self.file_engine
            .update_file_path(disk_image)
            .map_err(VirtioBlockError::FileEngine)?;
        self.nsectors = disk_size >> SECTOR_SHIFT;
        self.file_path = disk_image_path;

        Ok(())
    }

    fn build_device_id(disk_file: &File) -> Result<String, VirtioBlockError> {
        let blk_metadata = disk_file
            .metadata()
            .map_err(VirtioBlockError::GetFileMetadata)?;
        // This is how kvmtool does it.
        let device_id = format!(
            "{}{}{}",
            blk_metadata.st_dev(),
            blk_metadata.st_rdev(),
            blk_metadata.st_ino()
        );
        Ok(device_id)
    }

    fn build_disk_image_id(disk_file: &File) -> [u8; VIRTIO_BLK_ID_BYTES as usize] {
        let mut default_id = [0; VIRTIO_BLK_ID_BYTES as usize];
        match Self::build_device_id(disk_file) {
            Err(_) => {
                warn!("Could not generate device id. We'll use a default.");
            }
            Ok(disk_id_string) => {
                // The kernel only knows to read a maximum of VIRTIO_BLK_ID_BYTES.
                // This will also zero out any leftover bytes.
                let disk_id = disk_id_string.as_bytes();
                let bytes_to_copy = cmp::min(disk_id.len(), VIRTIO_BLK_ID_BYTES as usize);
                default_id[..bytes_to_copy].copy_from_slice(&disk_id[..bytes_to_copy]);
            }
        }
        default_id
    }
}

// `num_queues` sits at offset 34 in `virtio_blk_config` (`VIRTIO_BLK_F_MQ`). Unused
// fields in between (size_max, seg_max, geometry, blk_size, topology, writeback, unused)
// stay zero, which the virtio spec treats as "not supported" for each of them.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
#[repr(C, packed)]
pub struct ConfigSpace {
    pub capacity: u64,
    pub _reserved: [u8; 26],
    pub num_queues: u16,
}

// SAFETY: `ConfigSpace` contains only PODs in `repr(C, packed)`; every byte is
// initialized and there are no invalid bit patterns.
unsafe impl ByteValued for ConfigSpace {}

/// Use this structure to set up the Block Device before booting the kernel.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VirtioBlockConfig {
    /// Unique identifier of the drive.
    pub drive_id: String,
    /// Part-UUID. Represents the unique id of the boot partition of this device. It is
    /// optional and it will be used only if the `is_root_device` field is true.
    pub partuuid: Option<String>,
    /// If set to true, it makes the current device the root block device.
    /// Setting this flag to true will mount the block device in the
    /// guest under /dev/vda unless the partuuid is present.
    pub is_root_device: bool,
    /// If set to true, the drive will ignore flush requests coming from
    /// the guest driver.
    #[serde(default)]
    pub cache_type: CacheType,

    /// If set to true, the drive is opened in read-only mode. Otherwise, the
    /// drive is opened as read-write.
    pub is_read_only: bool,
    /// Path of the backing file on the host
    pub path_on_host: String,
    /// Rate Limiter for I/O operations.
    pub rate_limiter: Option<RateLimiterConfig>,
    /// The type of IO engine used by the device.
    #[serde(default)]
    #[serde(rename = "io_engine")]
    pub file_engine_type: FileEngineType,
    /// Number of virtio queues to expose to the guest. Values greater than 1 enable
    /// thread-per-queue mode with one worker thread driving each queue.
    #[serde(default = "default_num_queues")]
    pub num_queues: u16,
}

fn default_num_queues() -> u16 {
    1
}

impl TryFrom<&BlockDeviceConfig> for VirtioBlockConfig {
    type Error = VirtioBlockError;

    fn try_from(value: &BlockDeviceConfig) -> Result<Self, Self::Error> {
        if let (Some(path_on_host), None) = (&value.path_on_host, &value.socket) {
            Ok(Self {
                drive_id: value.drive_id.clone(),
                partuuid: value.partuuid.clone(),
                is_root_device: value.is_root_device,
                cache_type: value.cache_type,

                is_read_only: value.is_read_only.unwrap_or(false),
                path_on_host: path_on_host.clone(),
                rate_limiter: value.rate_limiter,
                file_engine_type: value.file_engine_type.unwrap_or_default(),
                num_queues: value.num_queues.unwrap_or(1),
            })
        } else {
            Err(VirtioBlockError::Config)
        }
    }
}

impl From<VirtioBlockConfig> for BlockDeviceConfig {
    fn from(value: VirtioBlockConfig) -> Self {
        Self {
            drive_id: value.drive_id,
            partuuid: value.partuuid,
            is_root_device: value.is_root_device,
            cache_type: value.cache_type,

            is_read_only: Some(value.is_read_only),
            path_on_host: Some(value.path_on_host),
            rate_limiter: value.rate_limiter,
            file_engine_type: Some(value.file_engine_type),
            num_queues: Some(value.num_queues),

            socket: None,
        }
    }
}

/// Virtio device for exposing block level read/write operations on a host file.
#[derive(Debug)]
pub struct VirtioBlock {
    // Virtio fields.
    pub avail_features: u64,
    pub acked_features: u64,
    pub config_space: ConfigSpace,
    pub activate_evt: EventFd,

    // Transport related fields.
    pub queues: Vec<Queue>,
    pub queue_evts: Vec<EventFd>,
    pub device_state: DeviceState,

    // Implementation specific fields.
    pub id: String,
    pub partuuid: Option<String>,
    pub cache_type: CacheType,
    pub root_device: bool,
    pub read_only: bool,
    pub thread_per_queue: bool,
    pub num_queues: u16,

    // Host file and properties.
    pub disk: DiskProperties,
    pub metrics: Arc<BlockDeviceMetrics>,

    pub runtime: Runtime,
}

/// Runtime layout: one thread drives all queues (single-thread) or one thread per queue.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Runtime {
    SingleThread(BlockRuntimeState),
    WorkerThreads(Vec<WorkerHandle>),
}

/// VMM-side handle for one worker thread.
#[derive(Debug)]
pub struct WorkerHandle {
    pub thread: std::thread::JoinHandle<()>,
    pub to_worker: Sender<ControlMsg>,
    pub from_worker: Receiver<ControlResponse>,
    /// EventFd the worker's EventManager subscribes to; VMM writes to it after posting a
    /// message on `to_worker` to wake the worker up.
    pub control_evt: EventFd,
}

/// Control messages sent VMM -> worker.
#[derive(Debug, Clone)]
pub enum ControlMsg {
    /// Stop processing queue events until [`ControlMsg::Resume`] is received.
    Pause,
    /// Resume processing queue events.
    Resume,
    /// Terminate the worker; the thread's event loop breaks.
    Terminate,
    /// Update the local rate-limiter's bucket configuration.
    UpdateRateLimiter(BucketUpdate, BucketUpdate),
}

/// Worker -> VMM responses.
#[derive(Debug)]
pub enum ControlResponse {
    Ok,
    Err(String),
}

/// Raw pointer to the owning [`VirtioBlock`]. Worker `i` only mutates slot `i` of
/// `queues`/`queue_evts` after activation and only reads the remaining (immutable-post-
/// activation) fields, so no locking is needed on this pointer.
#[derive(Default)]
pub struct BlockPtr(*mut VirtioBlock);

// SAFETY: See BlockPtr doc.
unsafe impl Send for BlockPtr {}

impl fmt::Debug for BlockPtr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlockPtr({:p})", self.0)
    }
}

/// Per-queue runtime state driven by one thread.
#[derive(Debug)]
pub struct BlockRuntimeState {
    /// Guest-visible index of the queue this state drives.
    pub queue_index: u16,
    pub rate_limiter: RateLimiter,
    pub is_io_engine_throttled: bool,
    /// Pointer back to the owning device. See [`BlockPtr`].
    pub(crate) block: BlockPtr,
    /// Present in worker-thread mode: control channel + eventfd + pause flag.
    /// In single-thread mode this is `None`.
    pub(crate) control: Option<WorkerControl>,
}

/// Worker-side end of the control channel.
#[derive(Debug)]
pub struct WorkerControl {
    pub from_vmm: Receiver<ControlMsg>,
    pub to_vmm: Sender<ControlResponse>,
    pub control_evt: EventFd,
    /// While paused the worker skips queue-event processing but still services control
    /// messages, so it can be resumed or terminated.
    pub paused: bool,
    /// Set by the worker after processing a [`ControlMsg::Terminate`]. `run_worker_loop`
    /// checks it after each `EventManager::run()` batch and exits.
    pub should_stop: bool,
}

macro_rules! unwrap_async_file_engine_or_return {
    ($file_engine: expr) => {
        match $file_engine {
            FileEngine::Async(engine) => engine,
            FileEngine::Sync(_) => {
                error!("The block device doesn't use an async IO engine");
                return;
            }
        }
    };
}

impl VirtioBlock {
    /// Create a new virtio block device that operates on the given file.
    ///
    /// The given file must be seekable and sizable.
    pub fn new(config: VirtioBlockConfig) -> Result<VirtioBlock, VirtioBlockError> {
        let mut avail_features = (1u64 << VIRTIO_F_VERSION_1) | (1u64 << VIRTIO_RING_F_EVENT_IDX);

        if config.cache_type == CacheType::Writeback {
            avail_features |= 1u64 << VIRTIO_BLK_F_FLUSH;
        }

        if config.is_read_only {
            avail_features |= 1u64 << VIRTIO_BLK_F_RO;
        };

        assert!(config.num_queues >= 1, "num_queues must be >= 1");
        if config.num_queues > 1 {
            avail_features |= 1u64 << VIRTIO_BLK_F_MQ;
        }
        let num_queues = config.num_queues as usize;

        // Single DiskProperties shared across all workers. Sync engine uses pread/pwrite
        // and is thread-safe (`unsafe impl Sync`); async (io_uring) is not — see
        // note below.
        let disk = DiskProperties::new(
            config.path_on_host.clone(),
            config.is_read_only,
            config.file_engine_type,
        )?;

        let queue_evts: Vec<EventFd> = (0..num_queues)
            .map(|_| EventFd::new(libc::EFD_NONBLOCK).map_err(VirtioBlockError::EventFd))
            .collect::<Result<_, _>>()?;

        let queues: Vec<Queue> = (0..num_queues)
            .map(|_| Queue::new(FIRECRACKER_MAX_QUEUE_SIZE))
            .collect();

        let config_space = ConfigSpace {
            capacity: disk.nsectors.to_le(),
            num_queues: config.num_queues.to_le(),
            ..Default::default()
        };

        // Any config with more than one queue implicitly runs in thread-per-queue mode.
        let thread_per_queue = num_queues > 1;

        let rate_limiter = config
            .rate_limiter
            .map(RateLimiter::from)
            .unwrap_or_default();

        // Pointer back to VirtioBlock is filled in by `refresh_block_ptr()` once its
        // address is stable (i.e. once wrapped in `Arc<Mutex<Block>>`).
        let runtime_state = BlockRuntimeState {
            queue_index: 0,
            rate_limiter,
            is_io_engine_throttled: false,
            block: BlockPtr::default(),
            control: None,
        };

        Ok(VirtioBlock {
            avail_features,
            acked_features: 0u64,
            config_space,
            activate_evt: EventFd::new(libc::EFD_NONBLOCK).map_err(VirtioBlockError::EventFd)?,

            queues,
            queue_evts,
            disk,
            device_state: DeviceState::Inactive,
            metrics: BlockMetricsPerDevice::alloc(config.drive_id.clone()),

            id: config.drive_id,
            partuuid: config.partuuid,
            cache_type: config.cache_type,
            root_device: config.is_root_device,
            read_only: config.is_read_only,
            thread_per_queue,
            num_queues: config.num_queues,

            runtime: Runtime::SingleThread(runtime_state),
        })
    }

    /// Wire the raw pointer inside the single-thread `BlockRuntimeState` once
    /// VirtioBlock's address is stable (post-`Arc<Mutex<...>>`).
    pub(crate) fn refresh_block_ptr(&mut self) {
        let ptr = self as *mut VirtioBlock;
        if let Runtime::SingleThread(state) = &mut self.runtime {
            state.block = BlockPtr(ptr);
        }
    }

    /// Returns the single-thread runtime state. Panics in worker-thread mode.
    pub fn runtime_state(&self) -> &BlockRuntimeState {
        match &self.runtime {
            Runtime::SingleThread(s) => s,
            Runtime::WorkerThreads(_) => unreachable!("only supported in single-thread mode"),
        }
    }

    /// Returns the single-thread runtime state. Panics in worker-thread mode.
    pub fn runtime_state_mut(&mut self) -> &mut BlockRuntimeState {
        match &mut self.runtime {
            Runtime::SingleThread(s) => s,
            Runtime::WorkerThreads(_) => unreachable!("only supported in single-thread mode"),
        }
    }

    /// Returns a copy of a device config.
    pub fn config(&self) -> VirtioBlockConfig {
        let rl: RateLimiterConfig = (&self.runtime_state().rate_limiter).into();
        VirtioBlockConfig {
            drive_id: self.id.clone(),
            path_on_host: self.disk.file_path.clone(),
            is_root_device: self.root_device,
            partuuid: self.partuuid.clone(),
            is_read_only: self.read_only,
            cache_type: self.cache_type,
            rate_limiter: rl.into_option(),
            file_engine_type: self.file_engine_type(),
            num_queues: self.num_queues,
        }
    }

    /// Convenience proxies to the single-thread runtime state.
    pub fn process_virtio_queues(&mut self) -> Result<(), InvalidAvailIdx> {
        self.runtime_state_mut().process_virtio_queues()
    }

    pub fn update_disk_image(&mut self, disk_image_path: String) -> Result<(), VirtioBlockError> {
        // In worker-thread mode we must quiesce the workers before touching `self.disk`;
        // they hold a raw pointer into it and would race with the reopen otherwise.
        self.pause();
        let update_result = self.disk.update(disk_image_path, self.read_only);
        // Always resume, whether the update succeeded or not.
        self.resume();
        update_result?;
        self.config_space.capacity = self.disk.nsectors.to_le();
        self.metrics.update_count.inc();
        if self.is_activated() {
            self.interrupt_trigger()
                .trigger(VirtioInterruptType::Config)
                .unwrap();
        }
        Ok(())
    }

    pub fn update_rate_limiter(&mut self, bytes: BucketUpdate, ops: BucketUpdate) {
        match &self.runtime {
            Runtime::SingleThread(_) => {
                self.runtime_state_mut()
                    .rate_limiter
                    .update_buckets(bytes, ops);
            }
            Runtime::WorkerThreads(_) => {
                // Each worker owns its own rate limiter; ask them to update it.
                self.broadcast(ControlMsg::UpdateRateLimiter(bytes, ops));
            }
        }
    }

    /// Worker lifecycle. In single-thread mode these are no-ops (there are no workers).
    pub fn start(&mut self) {
        // Workers are spawned by `activate()`. This entry point exists so callers can
        // treat the lifecycle uniformly without checking mode.
    }

    /// Pause every worker thread and wait for them to acknowledge.
    pub fn pause(&mut self) {
        self.broadcast(ControlMsg::Pause);
    }

    /// Resume every worker thread and wait for them to acknowledge.
    pub fn resume(&mut self) {
        self.broadcast(ControlMsg::Resume);
    }

    /// Terminate every worker thread and join it. After this call the runtime holds an
    /// empty `WorkerThreads` vector; single-thread mode is not restored.
    pub fn terminate(&mut self) {
        self.broadcast(ControlMsg::Terminate);
        if let Runtime::WorkerThreads(workers) =
            std::mem::replace(&mut self.runtime, Runtime::WorkerThreads(Vec::new()))
        {
            for w in workers {
                if let Err(err) = w.thread.join() {
                    error!("block worker join failed: {:?}", err);
                }
            }
        }
    }

    /// Send `msg` to every worker (if any), waking each up via its control eventfd, and
    /// wait for each `ControlResponse`. In single-thread mode this is a no-op.
    fn broadcast(&mut self, msg: ControlMsg) {
        let workers = match &self.runtime {
            Runtime::WorkerThreads(w) => w,
            Runtime::SingleThread(_) => return,
        };
        // Send + kick each worker.
        for w in workers {
            if let Err(err) = w.to_worker.send(msg.clone()) {
                error!("block: failed to send control msg: {:?}", err);
                continue;
            }
            if let Err(err) = w.control_evt.write(1) {
                error!("block: failed to kick worker control evt: {:?}", err);
            }
        }
        // Await ack from each worker in turn.
        for w in workers {
            match w.from_worker.recv() {
                Ok(ControlResponse::Ok) => {}
                Ok(ControlResponse::Err(e)) => {
                    error!("block worker returned error for {:?}: {}", msg, e);
                }
                Err(err) => {
                    error!("block worker response channel closed: {:?}", err);
                }
            }
        }
    }

    /// Retrieve the file engine type.
    pub fn file_engine_type(&self) -> FileEngineType {
        match self.disk.file_engine {
            FileEngine::Sync(_) => FileEngineType::Sync,
            FileEngine::Async(_) => FileEngineType::Async,
        }
    }

    fn drain_and_flush(&mut self, discard: bool) {
        if let Err(err) = self.disk.file_engine.drain_and_flush(discard) {
            error!("Failed to drain ops and flush block data: {:?}", err);
        }
    }

    pub fn prepare_save(&mut self) {
        if !self.is_activated() {
            return;
        }
        // Only meaningful in single-thread mode; worker-thread mode would need an RPC.
        if let Runtime::SingleThread(_) = &self.runtime {
            self.drain_and_flush(false);
            let state_ptr: *mut BlockRuntimeState = self.runtime_state_mut() as *mut _;
            let is_async = matches!(self.disk.file_engine, FileEngine::Async(_));
            if is_async {
                // SAFETY: state_ptr points into self.runtime; unique reborrow.
                unsafe { (*state_ptr).process_async_completion_queue() };
            }
        }
    }
}

impl BlockRuntimeState {
    /// Access to the owning [`VirtioBlock`]. See [`BlockPtr`] for the safety invariant.
    /// The returned reference has an unbound lifetime so the borrow checker doesn't tie
    /// it to `&self` (the pointer isn't derived from `self`).
    #[allow(clippy::mut_from_ref)]
    pub(crate) fn block<'a>(&self) -> &'a mut VirtioBlock {
        // SAFETY: see BlockPtr. Worker `i` only mutates slot `i` of the per-queue arrays
        // after activation; everything else it touches is read-only or atomic.
        unsafe { &mut *self.block.0 }
    }

    pub(crate) fn block_ref(&self) -> &VirtioBlock {
        // SAFETY: see `block()`.
        unsafe { &*self.block.0 }
    }

    /// Process a single event in the Virtio queue.
    pub(crate) fn process_queue_event(&mut self) {
        self.block_ref().metrics.queue_event_count.inc();
        if let Err(err) = self.block_ref().queue_evts[self.queue_index as usize].read() {
            error!("Failed to get queue event: {:?}", err);
            self.block_ref().metrics.event_fails.inc();
        } else if self.rate_limiter.is_blocked() {
            self.block_ref().metrics.rate_limiter_throttled_events.inc();
        } else if self.is_io_engine_throttled {
            self.block_ref().metrics.io_engine_throttled_events.inc();
        } else {
            self.process_virtio_queues().unwrap()
        }
    }

    /// Process device virtio queue(s).
    pub fn process_virtio_queues(&mut self) -> Result<(), InvalidAvailIdx> {
        self.process_queue()
    }

    pub(crate) fn process_rate_limiter_event(&mut self) {
        self.block_ref().metrics.rate_limiter_event_count.inc();
        if self.rate_limiter.event_handler().is_ok() {
            self.process_queue().unwrap()
        }
    }

    /// Peek at this state's queue and process any pending descriptors.
    pub fn process_queue(&mut self) -> Result<(), InvalidAvailIdx> {
        let block = self.block();
        let active_state = block.device_state.active_state().unwrap();
        let mem = &active_state.mem;
        let interrupt = &*active_state.interrupt;
        let metrics: &BlockDeviceMetrics = &block.metrics;
        let disk = &mut block.disk;
        let queue = &mut block.queues[self.queue_index as usize];
        let guest_queue_index = self.queue_index;
        let mut used_any = false;

        while let Some(head) = queue.pop_or_enable_notification()? {
            metrics.remaining_reqs_count.add(queue.len().into());
            let processing_result = match Request::parse(&head, mem, disk.nsectors) {
                Ok(request) => {
                    if request.rate_limit(&mut self.rate_limiter) {
                        queue.undo_pop();
                        metrics.rate_limiter_throttled_events.inc();
                        break;
                    }
                    request.process(disk, head.index, mem, metrics)
                }
                Err(err) => {
                    error!("Failed to parse available descriptor chain: {:?}", err);
                    metrics.execute_fails.inc();
                    ProcessingResult::Executed(FinishedRequest {
                        num_bytes_to_mem: 0,
                        desc_idx: head.index,
                    })
                }
            };

            match processing_result {
                ProcessingResult::Submitted => {}
                ProcessingResult::Throttled => {
                    queue.undo_pop();
                    self.is_io_engine_throttled = true;
                    break;
                }
                ProcessingResult::Executed(finished) => {
                    used_any = true;
                    queue
                        .add_used(head.index, finished.num_bytes_to_mem)
                        .unwrap_or_else(|err| {
                            error!(
                                "Failed to add available descriptor head {}: {}",
                                head.index, err
                            )
                        });
                }
            }
        }
        queue.advance_used_ring_idx();

        if used_any && queue.prepare_kick() {
            interrupt
                .trigger(VirtioInterruptType::Queue(guest_queue_index))
                .unwrap_or_else(|_| {
                    metrics.event_fails.inc();
                });
        }

        if let FileEngine::Async(ref mut engine) = disk.file_engine
            && let Err(err) = engine.kick_submission_queue()
        {
            error!("BlockError submitting pending block requests: {:?}", err);
        }

        if !used_any {
            metrics.no_avail_buffer.inc();
        }

        Ok(())
    }

    fn process_async_completion_queue(&mut self) {
        let block = self.block();
        let active_state = block.device_state.active_state().unwrap();
        let mem = &active_state.mem;
        let interrupt = &*active_state.interrupt;
        let metrics: &BlockDeviceMetrics = &block.metrics;
        let disk = &mut block.disk;
        let queue = &mut block.queues[self.queue_index as usize];
        let guest_queue_index = self.queue_index;

        let engine = unwrap_async_file_engine_or_return!(&mut disk.file_engine);

        loop {
            match engine.pop(mem) {
                Err(error) => {
                    error!("Failed to read completed io_uring entry: {:?}", error);
                    break;
                }
                Ok(None) => break,
                Ok(Some(cqe)) => {
                    let res = cqe.result();
                    let user_data = cqe.user_data();
                    let (pending, res) = match res {
                        Ok(count) => (user_data, Ok(count)),
                        Err(error) => (
                            user_data,
                            Err(IoErr::FileEngine(block_io::BlockIoError::Async(
                                async_io::AsyncIoError::IO(error),
                            ))),
                        ),
                    };
                    let finished = pending.finish(mem, res, metrics);
                    queue
                        .add_used(finished.desc_idx, finished.num_bytes_to_mem)
                        .unwrap_or_else(|err| {
                            error!(
                                "Failed to add available descriptor head {}: {}",
                                finished.desc_idx, err
                            )
                        });
                }
            }
        }
        queue.advance_used_ring_idx();

        if queue.prepare_kick() {
            interrupt
                .trigger(VirtioInterruptType::Queue(guest_queue_index))
                .unwrap_or_else(|_| {
                    metrics.event_fails.inc();
                });
        }
    }

    pub fn process_async_completion_event(&mut self) {
        let disk = &mut self.block().disk;
        let engine = unwrap_async_file_engine_or_return!(&mut disk.file_engine);

        if let Err(err) = engine.completion_evt().read() {
            error!("Failed to get async completion event: {:?}", err);
        } else {
            self.process_async_completion_queue();

            if self.is_io_engine_throttled {
                self.is_io_engine_throttled = false;
                self.process_queue().unwrap()
            }
        }
    }

    pub fn is_activated(&self) -> bool {
        self.block_ref().device_state.is_activated()
    }
}

/// Body of a per-queue worker thread. Registers the given [`BlockRuntimeState`] with an
/// EventManager and runs the epoll loop. Exits when a [`ControlMsg::Terminate`] has been
/// serviced (which sets `should_stop`).
fn run_worker_loop(state: BlockRuntimeState) {
    use std::sync::{Arc, Mutex};

    use event_manager::{EventManager, SubscriberOps};

    let subscriber: Arc<Mutex<BlockRuntimeState>> = Arc::new(Mutex::new(state));
    let mut event_manager: EventManager<Arc<Mutex<dyn event_manager::MutEventSubscriber>>> =
        match EventManager::new() {
            Ok(em) => em,
            Err(err) => {
                error!("block worker: failed to create EventManager: {:?}", err);
                return;
            }
        };
    event_manager.add_subscriber(subscriber.clone());

    loop {
        if let Err(err) = event_manager.run() {
            error!("block worker: EventManager run failed: {:?}", err);
            return;
        }
        // Check for termination request.
        if subscriber
            .lock()
            .expect("Poisoned lock")
            .control
            .as_ref()
            .is_some_and(|c| c.should_stop)
        {
            return;
        }
    }
}

impl VirtioDevice for VirtioBlock {
    impl_device_type!(VirtioDeviceType::Block);

    fn id(&self) -> &str {
        &self.id
    }

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
        &self.queue_evts
    }

    fn interrupt_trigger(&self) -> &dyn VirtioInterrupt {
        self.device_state
            .active_state()
            .expect("Device is not initialized")
            .interrupt
            .deref()
    }

    fn config_as_bytes(&self) -> &[u8] {
        self.config_space.as_slice()
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        self.metrics.cfg_fails.inc();
        warn!(
            "virtio-block: guest driver attempted to write device config (offset={:#x}, len={:#x})",
            offset,
            data.len()
        );
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: Arc<dyn VirtioInterrupt>,
    ) -> Result<(), ActivateError> {
        assert!(!self.is_activated());

        let event_idx = self.has_feature(u64::from(VIRTIO_RING_F_EVENT_IDX));

        // Initialize only queues the guest actually configured. In single-thread mode
        // there's exactly one, always ready. In worker-thread mode blk-mq may configure
        // fewer than we advertised.
        for q in self.queues.iter_mut() {
            if q.ready {
                q.initialize(&mem)
                    .map_err(ActivateError::QueueMemoryError)?;
                if event_idx {
                    q.enable_notif_suppression();
                }
            }
        }

        self.device_state = DeviceState::Activated(ActiveState { mem, interrupt });

        // Now that VirtioBlock's address is stable (it's already inside its Arc<Mutex>),
        // fill in the raw pointer on the runtime state and (for worker mode) spawn one
        // worker per ready queue.
        self.refresh_block_ptr();

        if !self.thread_per_queue {
            if self.activate_evt.write(1).is_err() {
                self.metrics.activate_fails.inc();
                return Err(ActivateError::EventFd);
            }
            return Ok(());
        }

        // Worker-thread mode. Each worker gets its own RateLimiter built from the same
        // config, a control channel + eventfd pair for VMM-side coordination, and a
        // pointer back to VirtioBlock.
        let ptr = self as *mut VirtioBlock;
        let rl_config: RateLimiterConfig = (&self.runtime_state().rate_limiter).into();
        let mut workers: Vec<WorkerHandle> = Vec::new();
        for (idx, q) in self.queues.iter().enumerate() {
            if !q.ready {
                continue;
            }
            let (to_worker, from_vmm) = channel::<ControlMsg>();
            let (to_vmm, from_worker) = channel::<ControlResponse>();
            let control_evt =
                EventFd::new(libc::EFD_NONBLOCK).map_err(|_| ActivateError::EventFd)?;
            let worker_evt = control_evt.try_clone().map_err(|_| ActivateError::EventFd)?;

            let per_queue = BlockRuntimeState {
                queue_index: u16::try_from(idx).expect("queue index fits in u16"),
                rate_limiter: rl_config
                    .into_option()
                    .map(RateLimiter::from)
                    .unwrap_or_default(),
                is_io_engine_throttled: false,
                block: BlockPtr(ptr),
                control: Some(WorkerControl {
                    from_vmm,
                    to_vmm,
                    control_evt: worker_evt,
                    paused: false,
                    should_stop: false,
                }),
            };
            let name = format!("fc_virtio_blk_{}_{}", self.id, idx);
            let thread = std::thread::Builder::new()
                .name(name)
                .spawn(move || run_worker_loop(per_queue))
                .map_err(|_| ActivateError::EventFd)?;
            workers.push(WorkerHandle {
                thread,
                to_worker,
                from_worker,
                control_evt,
            });
        }
        assert!(
            !workers.is_empty(),
            "guest activated block device with zero ready queues"
        );
        self.runtime = Runtime::WorkerThreads(workers);
        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn deactivate(&mut self) {
        self.device_state = DeviceState::Inactive;
    }

    fn _reset(&mut self) -> bool {
        // Only meaningful before workers are spawned. In worker-thread mode we would need
        // an RPC to each worker to drain the io engine; punt for now.
        if matches!(self.runtime, Runtime::WorkerThreads(_)) {
            return false;
        }
        if let Err(err) = self.disk.file_engine.drain(true) {
            error!("Failed to reset block IO engine: {:?}", err);
            return false;
        }
        if let Runtime::SingleThread(state) = &mut self.runtime {
            state.is_io_engine_throttled = false;
        }
        true
    }
}

impl Drop for VirtioBlock {
    fn drop(&mut self) {
        // Terminate and join worker threads before touching `self.disk`; they hold a raw
        // pointer into it.
        if matches!(self.runtime, Runtime::WorkerThreads(_)) {
            self.terminate();
        }
        match self.cache_type {
            CacheType::Unsafe => {
                if let Err(err) = self.disk.file_engine.drain(true) {
                    error!("Failed to drain ops on drop: {:?}", err);
                }
            }
            CacheType::Writeback => {
                self.drain_and_flush(true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::metadata;
    use std::io::{Read, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::thread;
    use std::time::Duration;

    use vmm_sys_util::tempfile::TempFile;

    use super::*;
    use crate::check_metric_after_block;
    use crate::devices::virtio::block::virtio::IO_URING_NUM_ENTRIES;
    use crate::devices::virtio::block::virtio::test_utils::{
        default_block, read_blk_req_descriptors, set_queue, set_rate_limiter,
        simulate_async_completion_event, simulate_queue_and_async_completion_events,
        simulate_queue_event,
    };
    use crate::devices::virtio::queue::{VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
    use crate::devices::virtio::test_utils::{VirtQueue, default_interrupt, default_mem};
    use crate::rate_limiter::TokenType;
    use crate::vstate::memory::{Address, Bytes, GuestAddress};

    #[test]
    fn test_from_config() {
        let block_config = BlockDeviceConfig {
            drive_id: "".to_string(),
            partuuid: None,
            is_root_device: false,
            cache_type: CacheType::Unsafe,

            is_read_only: Some(true),
            path_on_host: Some("path".to_string()),
            rate_limiter: None,
            file_engine_type: Default::default(),
            num_queues: None,

            socket: None,
        };
        VirtioBlockConfig::try_from(&block_config).unwrap();

        let block_config = BlockDeviceConfig {
            drive_id: "".to_string(),
            partuuid: None,
            is_root_device: false,
            cache_type: CacheType::Unsafe,

            is_read_only: None,
            path_on_host: None,
            rate_limiter: None,
            file_engine_type: Default::default(),
            num_queues: None,

            socket: Some("sock".to_string()),
        };
        VirtioBlockConfig::try_from(&block_config).unwrap_err();

        let block_config = BlockDeviceConfig {
            drive_id: "".to_string(),
            partuuid: None,
            is_root_device: false,
            cache_type: CacheType::Unsafe,

            is_read_only: Some(true),
            path_on_host: Some("path".to_string()),
            rate_limiter: None,
            file_engine_type: Default::default(),
            num_queues: None,

            socket: Some("sock".to_string()),
        };
        VirtioBlockConfig::try_from(&block_config).unwrap_err();
    }

    #[test]
    fn test_disk_backing_file_helper() {
        let num_sectors = 2;
        let f = TempFile::new().unwrap();
        let size = u64::from(SECTOR_SIZE) * num_sectors;
        f.as_file().set_len(size).unwrap();

        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let disk_properties =
                DiskProperties::new(String::from(f.as_path().to_str().unwrap()), true, engine)
                    .unwrap();

            assert_eq!(size, u64::from(SECTOR_SIZE) * num_sectors);
            assert_eq!(disk_properties.nsectors, num_sectors);
            // Testing `backing_file.virtio_block_disk_image_id()` implies
            // duplicating that logic in tests, so skipping it.

            let res = DiskProperties::new("invalid-disk-path".to_string(), true, engine);
            assert!(
                matches!(res, Err(VirtioBlockError::BackingFile(_, _))),
                "{:?}",
                res
            );
        }
    }

    #[test]
    fn test_virtio_features() {
        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let mut block = default_block(engine);

            assert_eq!(block.device_type(), VirtioDeviceType::Block);

            let features: u64 = (1u64 << VIRTIO_F_VERSION_1) | (1u64 << VIRTIO_RING_F_EVENT_IDX);

            assert_eq!(
                block.avail_features_by_page(0),
                (features & 0xffffffff) as u32,
            );
            assert_eq!(block.avail_features_by_page(1), (features >> 32) as u32);

            for i in 2..10 {
                assert_eq!(block.avail_features_by_page(i), 0u32);
            }

            for i in 0..10 {
                block.ack_features_by_page(i, u32::MAX);
            }
            assert_eq!(block.acked_features, features);
        }
    }

    #[test]
    fn test_config_as_bytes() {
        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let block = default_block(engine);

            let config = block.config_as_bytes();
            // The block's backing file size is 0x1000, so there are 8 (4096/512) sectors.
            let expected_config_space = ConfigSpace {
                capacity: 8,
                num_queues: 1,
                ..Default::default()
            };
            assert_eq!(config, expected_config_space.as_slice());
        }
    }

    #[test]
    fn test_virtio_device_config_space_is_read_only() {
        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let mut block = default_block(engine);

            // Snapshot the config space before any write attempt.
            let initial_config = block.config_as_bytes().to_vec();

            // A guest write must be rejected: the config space is left unchanged
            // and the attempt is counted under cfg_fails.
            let cfg_fails_before = block.metrics.cfg_fails.count();
            block.write_config(
                0,
                ConfigSpace {
                    capacity: 0x1122334455667788,
                    ..Default::default()
                }
                .as_slice(),
            );
            assert_eq!(block.config_as_bytes(), initial_config);
            assert_eq!(block.metrics.cfg_fails.count(), cfg_fails_before + 1);
        }
    }

    #[test]
    fn test_invalid_request() {
        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let mut block = default_block(engine);
            let mem = default_mem();
            let interrupt = default_interrupt();
            let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
            set_queue(&mut block, 0, vq.create_queue());
            block.activate(mem.clone(), interrupt).unwrap();
            read_blk_req_descriptors(&vq);

            let request_type_addr = GuestAddress(vq.dtable[0].addr.get());

            // Request is invalid because the first descriptor is write-only.
            vq.dtable[0]
                .flags
                .set(VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE);
            mem.write_obj::<u32>(VIRTIO_BLK_T_IN, request_type_addr)
                .unwrap();

            simulate_queue_event(&mut block, Some(true));

            assert_eq!(vq.used.idx.get(), 1);
            assert_eq!(vq.used.ring[0].get().id, 0);
            assert_eq!(vq.used.ring[0].get().len, 0);
        }
    }

    #[test]
    fn test_addr_out_of_bounds() {
        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let mut block = default_block(engine);
            // Default mem size is 0x10000
            let mem = default_mem();
            let interrupt = default_interrupt();
            let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
            set_queue(&mut block, 0, vq.create_queue());
            block.activate(mem.clone(), interrupt).unwrap();
            read_blk_req_descriptors(&vq);
            let request_type_addr = GuestAddress(vq.dtable[0].addr.get());

            // Read at out of bounds address.
            {
                vq.used.idx.set(0);
                set_queue(&mut block, 0, vq.create_queue());

                // Mark the next available descriptor.
                vq.avail.idx.set(1);

                vq.dtable[1].set(0x20000, 0x1000, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE, 2);
                mem.write_obj::<u32>(VIRTIO_BLK_T_IN, request_type_addr)
                    .unwrap();

                simulate_queue_and_async_completion_events(&mut block, true);

                assert_eq!(vq.used.idx.get(), 1);

                let used = vq.used.ring[0].get();
                let status_addr = GuestAddress(vq.dtable[2].addr.get());
                assert_eq!(used.len, 1);
                assert_eq!(
                    u32::from(mem.read_obj::<u8>(status_addr).unwrap()),
                    VIRTIO_BLK_S_IOERR
                );
            }

            // Write at out of bounds address.
            {
                vq.used.idx.set(0);
                set_queue(&mut block, 0, vq.create_queue());

                // Mark the next available descriptor.
                vq.avail.idx.set(1);

                vq.dtable[1].set(0x20000, 0x1000, VIRTQ_DESC_F_NEXT, 2);
                mem.write_obj::<u32>(VIRTIO_BLK_T_OUT, request_type_addr)
                    .unwrap();

                simulate_queue_and_async_completion_events(&mut block, true);

                assert_eq!(vq.used.idx.get(), 1);

                let used = vq.used.ring[0].get();
                let status_addr = GuestAddress(vq.dtable[2].addr.get());
                assert_eq!(used.len, 1);
                assert_eq!(
                    u32::from(mem.read_obj::<u8>(status_addr).unwrap()),
                    VIRTIO_BLK_S_IOERR
                );
            }
        }
    }

    #[test]
    fn test_request_parse_failures() {
        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let mut block = default_block(engine);
            let mem = default_mem();
            let interrupt = default_interrupt();
            let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
            set_queue(&mut block, 0, vq.create_queue());
            block.activate(mem.clone(), interrupt).unwrap();
            read_blk_req_descriptors(&vq);

            let request_type_addr = GuestAddress(vq.dtable[0].addr.get());

            {
                // First descriptor no longer writable.
                vq.dtable[0].flags.set(VIRTQ_DESC_F_NEXT);
                vq.dtable[1].flags.set(VIRTQ_DESC_F_NEXT);

                // Generate a seek execute error caused by a very large sector number.
                let request_header = RequestHeader::new(VIRTIO_BLK_T_OUT, 0x000f_ffff_ffff);
                mem.write_obj::<RequestHeader>(request_header, request_type_addr)
                    .unwrap();

                simulate_queue_event(&mut block, Some(true));

                assert_eq!(vq.used.idx.get(), 1);
                assert_eq!(vq.used.ring[0].get().id, 0);
                assert_eq!(vq.used.ring[0].get().len, 0);
            }

            {
                // Reset the queue to reuse descriptors and memory.
                vq.used.idx.set(0);
                set_queue(&mut block, 0, vq.create_queue());

                vq.dtable[1]
                    .flags
                    .set(VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE);
                // Set sector to a valid number large enough that the full 0x1000 read will fail.
                let request_header = RequestHeader::new(VIRTIO_BLK_T_IN, 10);
                mem.write_obj::<RequestHeader>(request_header, request_type_addr)
                    .unwrap();

                simulate_queue_event(&mut block, Some(true));

                assert_eq!(vq.used.idx.get(), 1);
                assert_eq!(vq.used.ring[0].get().id, 0);
                assert_eq!(vq.used.ring[0].get().len, 0);
            }
        }
    }

    #[test]
    fn test_unsupported_request_type() {
        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let mut block = default_block(engine);
            let mem = default_mem();
            let interrupt = default_interrupt();
            let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
            set_queue(&mut block, 0, vq.create_queue());
            block.activate(mem.clone(), interrupt).unwrap();
            read_blk_req_descriptors(&vq);

            let request_type_addr = GuestAddress(vq.dtable[0].addr.get());
            let status_addr = GuestAddress(vq.dtable[2].addr.get());

            // Currently only VIRTIO_BLK_T_IN, VIRTIO_BLK_T_OUT,
            // VIRTIO_BLK_T_FLUSH and VIRTIO_BLK_T_GET_ID  are supported.
            // Generate an unsupported request.
            let request_header = RequestHeader::new(42, 0);
            mem.write_obj::<RequestHeader>(request_header, request_type_addr)
                .unwrap();

            simulate_queue_event(&mut block, Some(true));

            assert_eq!(vq.used.idx.get(), 1);
            assert_eq!(vq.used.ring[0].get().id, 0);
            assert_eq!(vq.used.ring[0].get().len, 1);
            assert_eq!(
                mem.read_obj::<u32>(status_addr).unwrap(),
                VIRTIO_BLK_S_UNSUPP
            );
        }
    }

    #[test]
    fn test_end_of_region() {
        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let mut block = default_block(engine);
            let mem = default_mem();
            let interrupt = default_interrupt();
            let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
            set_queue(&mut block, 0, vq.create_queue());
            block.activate(mem.clone(), interrupt).unwrap();
            read_blk_req_descriptors(&vq);
            vq.dtable[1].set(0xf000, 0x1000, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE, 2);

            let request_type_addr = GuestAddress(vq.dtable[0].addr.get());
            let status_addr = GuestAddress(vq.dtable[2].addr.get());

            vq.used.idx.set(0);

            mem.write_obj::<u32>(VIRTIO_BLK_T_IN, request_type_addr)
                .unwrap();
            vq.dtable[1]
                .flags
                .set(VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE);

            check_metric_after_block!(
                &block.metrics.read_count,
                1,
                simulate_queue_and_async_completion_events(&mut block, true)
            );

            assert_eq!(vq.used.idx.get(), 1);
            assert_eq!(vq.used.ring[0].get().id, 0);
            // Added status byte length.
            assert_eq!(vq.used.ring[0].get().len, vq.dtable[1].len.get() + 1);
            assert_eq!(mem.read_obj::<u32>(status_addr).unwrap(), VIRTIO_BLK_S_OK);
        }
    }

    #[test]
    fn test_read_write() {
        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let mut block = default_block(engine);
            let mem = default_mem();
            let interrupt = default_interrupt();
            let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
            set_queue(&mut block, 0, vq.create_queue());
            block.activate(mem.clone(), interrupt).unwrap();
            read_blk_req_descriptors(&vq);

            let request_type_addr = GuestAddress(vq.dtable[0].addr.get());
            let data_addr = GuestAddress(vq.dtable[1].addr.get());
            let status_addr = GuestAddress(vq.dtable[2].addr.get());

            let empty_data = vec![0; 512];
            let rand_data = vmm_sys_util::rand::rand_alphanumerics(1024)
                .as_bytes()
                .to_vec();

            // Write with invalid data len (not a multiple of 512).
            {
                mem.write_obj::<u32>(VIRTIO_BLK_T_OUT, request_type_addr)
                    .unwrap();
                // Make data read only, 512 bytes in len, and set the actual value to be written.
                vq.dtable[1].flags.set(VIRTQ_DESC_F_NEXT);
                vq.dtable[1].len.set(511);
                mem.write_slice(&rand_data[..511], data_addr).unwrap();

                simulate_queue_and_async_completion_events(&mut block, true);

                assert_eq!(vq.used.idx.get(), 1);
                assert_eq!(vq.used.ring[0].get().id, 0);
                assert_eq!(vq.used.ring[0].get().len, 0);

                // Check that the data wasn't written to the file
                let mut buf = [0u8; 512];
                block
                    .disk
                    .file_engine
                    .file()
                    .seek(SeekFrom::Start(0))
                    .unwrap();
                block.disk.file_engine.file().read_exact(&mut buf).unwrap();
                assert_eq!(buf, empty_data.as_slice());
            }

            // Write from valid address, with an overflowing length.
            {
                let mut block = default_block(engine);

                // Default mem size is 0x10000
                let mem = default_mem();
                let interrupt = default_interrupt();
                let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
                set_queue(&mut block, 0, vq.create_queue());
                block.activate(mem.clone(), interrupt).unwrap();
                read_blk_req_descriptors(&vq);
                let request_type_addr = GuestAddress(vq.dtable[0].addr.get());

                vq.dtable[1].set(0xff00, 0x1000, VIRTQ_DESC_F_NEXT, 2);
                mem.write_obj::<u32>(VIRTIO_BLK_T_OUT, request_type_addr)
                    .unwrap();

                // Mark the next available descriptor.
                vq.avail.idx.set(1);
                vq.used.idx.set(0);

                check_metric_after_block!(
                    &block.metrics.invalid_reqs_count,
                    1,
                    simulate_queue_and_async_completion_events(&mut block, true)
                );

                let used_idx = vq.used.idx.get();
                assert_eq!(used_idx, 1);

                let status_addr = GuestAddress(vq.dtable[2].addr.get());
                assert_eq!(
                    u32::from(mem.read_obj::<u8>(status_addr).unwrap()),
                    VIRTIO_BLK_S_IOERR
                );
            }

            // Write.
            {
                vq.used.idx.set(0);
                set_queue(&mut block, 0, vq.create_queue());

                mem.write_obj::<u32>(VIRTIO_BLK_T_OUT, request_type_addr)
                    .unwrap();
                // Make data read only, 512 bytes in len, and set the actual value to be written.
                vq.dtable[1].flags.set(VIRTQ_DESC_F_NEXT);
                vq.dtable[1].len.set(512);
                mem.write_slice(&rand_data[..512], data_addr).unwrap();

                check_metric_after_block!(
                    &block.metrics.write_count,
                    1,
                    simulate_queue_and_async_completion_events(&mut block, true)
                );

                assert_eq!(vq.used.idx.get(), 1);
                assert_eq!(vq.used.ring[0].get().id, 0);
                assert_eq!(vq.used.ring[0].get().len, 1);
                assert_eq!(mem.read_obj::<u32>(status_addr).unwrap(), VIRTIO_BLK_S_OK);
            }

            // Read with invalid data len (not a multiple of 512).
            {
                vq.used.idx.set(0);
                set_queue(&mut block, 0, vq.create_queue());

                mem.write_obj::<u32>(VIRTIO_BLK_T_IN, request_type_addr)
                    .unwrap();
                vq.dtable[1]
                    .flags
                    .set(VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE);
                vq.dtable[1].len.set(511);
                mem.write_slice(empty_data.as_slice(), data_addr).unwrap();

                simulate_queue_and_async_completion_events(&mut block, true);

                assert_eq!(vq.used.idx.get(), 1);
                assert_eq!(vq.used.ring[0].get().id, 0);
                // The descriptor should have been discarded.
                assert_eq!(vq.used.ring[0].get().len, 0);

                // Check that no data was read.
                let mut buf = [0u8; 512];
                mem.read_slice(&mut buf, data_addr).unwrap();
                assert_eq!(buf, empty_data.as_slice());
            }

            // Read.
            {
                vq.used.idx.set(0);
                set_queue(&mut block, 0, vq.create_queue());

                mem.write_obj::<u32>(VIRTIO_BLK_T_IN, request_type_addr)
                    .unwrap();
                vq.dtable[1]
                    .flags
                    .set(VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE);
                vq.dtable[1].len.set(512);
                mem.write_slice(empty_data.as_slice(), data_addr).unwrap();

                check_metric_after_block!(
                    &block.metrics.read_count,
                    1,
                    simulate_queue_and_async_completion_events(&mut block, true)
                );

                assert_eq!(vq.used.idx.get(), 1);
                assert_eq!(vq.used.ring[0].get().id, 0);
                // Added status byte length.
                assert_eq!(vq.used.ring[0].get().len, vq.dtable[1].len.get() + 1);
                assert_eq!(mem.read_obj::<u32>(status_addr).unwrap(), VIRTIO_BLK_S_OK);

                // Check that the data is the same that we wrote before
                let mut buf = [0u8; 512];
                mem.read_slice(&mut buf, data_addr).unwrap();
                assert_eq!(buf, &rand_data[..512]);
            }

            // Read with error.
            {
                vq.used.idx.set(0);
                set_queue(&mut block, 0, vq.create_queue());

                mem.write_obj::<u32>(VIRTIO_BLK_T_IN, request_type_addr)
                    .unwrap();
                vq.dtable[1]
                    .flags
                    .set(VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE);
                mem.write_slice(empty_data.as_slice(), data_addr).unwrap();

                let size = block
                    .disk
                    .file_engine
                    .file()
                    .seek(SeekFrom::End(0))
                    .unwrap();
                block.disk.file_engine.file().set_len(size / 2).unwrap();
                mem.write_obj(10, GuestAddress(request_type_addr.0 + 8))
                    .unwrap();

                simulate_queue_and_async_completion_events(&mut block, true);

                assert_eq!(vq.used.idx.get(), 1);
                assert_eq!(vq.used.ring[0].get().id, 0);
                // The descriptor should have been discarded.
                assert_eq!(vq.used.ring[0].get().len, 0);

                // Check that no data was read.
                let mut buf = [0u8; 512];
                mem.read_slice(&mut buf, data_addr).unwrap();
                assert_eq!(buf, empty_data.as_slice());
            }

            // Partial buffer error on read.
            {
                vq.used.idx.set(0);
                set_queue(&mut block, 0, vq.create_queue());

                mem.write_obj::<u32>(VIRTIO_BLK_T_IN, request_type_addr)
                    .unwrap();
                vq.dtable[1]
                    .flags
                    .set(VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE);

                let size = block
                    .disk
                    .file_engine
                    .file()
                    .seek(SeekFrom::End(0))
                    .unwrap();
                block.disk.file_engine.file().set_len(size / 2).unwrap();
                // Update sector number: stored at `request_type_addr.0 + 8`
                mem.write_obj(5, GuestAddress(request_type_addr.0 + 8))
                    .unwrap();

                // This will attempt to read past end of file.
                simulate_queue_and_async_completion_events(&mut block, true);

                assert_eq!(vq.used.idx.get(), 1);
                assert_eq!(vq.used.ring[0].get().id, 0);

                // No data since can't read past end of file, only status byte length.
                assert_eq!(vq.used.ring[0].get().len, 1);
                assert_eq!(
                    mem.read_obj::<u32>(status_addr).unwrap(),
                    VIRTIO_BLK_S_IOERR
                );

                // Check that no data was read since we can't read past the end of the file.
                let mut buf = [0u8; 512];
                mem.read_slice(&mut buf, data_addr).unwrap();
                assert_eq!(buf, empty_data.as_slice());
            }

            {
                // Note: this test case only works because when we truncated the file above (with
                // set_len), we did not update the sector count stored in the block device
                // itself (is still 8, even though the file length is 1024 now, e.g. has 2 sectors).
                // Normally, requests that reach past the final sector are rejected by
                // Request::parse.
                vq.used.idx.set(0);
                set_queue(&mut block, 0, vq.create_queue());

                mem.write_obj::<u32>(VIRTIO_BLK_T_IN, request_type_addr)
                    .unwrap();
                vq.dtable[1]
                    .flags
                    .set(VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE);
                vq.dtable[1].len.set(1024);

                mem.write_obj(1, GuestAddress(request_type_addr.0 + 8))
                    .unwrap();

                block
                    .disk
                    .file_engine
                    .file()
                    .seek(SeekFrom::Start(512))
                    .unwrap();
                block
                    .disk
                    .file_engine
                    .file()
                    .write_all(&rand_data[512..])
                    .unwrap();

                simulate_queue_and_async_completion_events(&mut block, true);

                assert_eq!(vq.used.idx.get(), 1);
                assert_eq!(vq.used.ring[0].get().id, 0);

                assert_eq!(
                    mem.read_obj::<u32>(status_addr).unwrap(),
                    VIRTIO_BLK_S_IOERR
                );

                // Check that we correctly read the second file sector.
                let mut buf = [0u8; 512];
                mem.read_slice(&mut buf, data_addr).unwrap();
                assert_eq!(buf, rand_data[512..]);
            }

            // Read at valid address, with an overflowing length.
            {
                let mut block = default_block(engine);

                // Default mem size is 0x10000
                let mem = default_mem();
                let interrupt = default_interrupt();
                let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
                set_queue(&mut block, 0, vq.create_queue());
                block.activate(mem.clone(), interrupt).unwrap();
                read_blk_req_descriptors(&vq);
                vq.dtable[1].set(0xff00, 0x1000, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE, 2);

                let request_type_addr = GuestAddress(vq.dtable[0].addr.get());

                // Mark the next available descriptor.
                vq.avail.idx.set(1);
                vq.used.idx.set(0);

                mem.write_obj::<u32>(VIRTIO_BLK_T_IN, request_type_addr)
                    .unwrap();
                vq.dtable[1]
                    .flags
                    .set(VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE);

                check_metric_after_block!(
                    &block.metrics.invalid_reqs_count,
                    1,
                    simulate_queue_and_async_completion_events(&mut block, true)
                );

                let used_idx = vq.used.idx.get();
                assert_eq!(used_idx, 1);

                let status_addr = GuestAddress(vq.dtable[2].addr.get());
                assert_eq!(
                    u32::from(mem.read_obj::<u8>(status_addr).unwrap()),
                    VIRTIO_BLK_S_IOERR
                );
            }
        }
    }

    #[test]
    fn test_flush() {
        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let mut block = default_block(engine);
            let mem = default_mem();
            let interrupt = default_interrupt();
            let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
            set_queue(&mut block, 0, vq.create_queue());
            block.activate(mem.clone(), interrupt).unwrap();
            read_blk_req_descriptors(&vq);

            let request_type_addr = GuestAddress(vq.dtable[0].addr.get());
            let status_addr = GuestAddress(vq.dtable[2].addr.get());

            // Flush completes successfully without a data descriptor.
            {
                vq.dtable[0].next.set(2);

                mem.write_obj::<u32>(VIRTIO_BLK_T_FLUSH, request_type_addr)
                    .unwrap();

                simulate_queue_and_async_completion_events(&mut block, true);
                assert_eq!(vq.used.idx.get(), 1);
                assert_eq!(vq.used.ring[0].get().id, 0);
                assert_eq!(vq.used.ring[0].get().len, 1);
                assert_eq!(mem.read_obj::<u32>(status_addr).unwrap(), VIRTIO_BLK_S_OK);
            }

            // Flush completes successfully even with a data descriptor.
            {
                vq.used.idx.set(0);
                set_queue(&mut block, 0, vq.create_queue());
                vq.dtable[0].next.set(1);

                mem.write_obj::<u32>(VIRTIO_BLK_T_FLUSH, request_type_addr)
                    .unwrap();

                simulate_queue_and_async_completion_events(&mut block, true);
                assert_eq!(vq.used.idx.get(), 1);
                assert_eq!(vq.used.ring[0].get().id, 0);
                // status byte length.
                assert_eq!(vq.used.ring[0].get().len, 1);
                assert_eq!(mem.read_obj::<u32>(status_addr).unwrap(), VIRTIO_BLK_S_OK);
            }
        }
    }

    #[test]
    fn test_get_device_id() {
        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let mut block = default_block(engine);
            let mem = default_mem();
            let interrupt = default_interrupt();
            let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
            set_queue(&mut block, 0, vq.create_queue());
            block.activate(mem.clone(), interrupt).unwrap();
            read_blk_req_descriptors(&vq);

            let request_type_addr = GuestAddress(vq.dtable[0].addr.get());
            let data_addr = GuestAddress(vq.dtable[1].addr.get());
            let status_addr = GuestAddress(vq.dtable[2].addr.get());
            let blk_metadata = block.disk.file_engine.file().metadata();

            // Test that the driver receives the correct device id.
            {
                vq.dtable[1].len.set(VIRTIO_BLK_ID_BYTES);

                mem.write_obj::<u32>(VIRTIO_BLK_T_GET_ID, request_type_addr)
                    .unwrap();

                simulate_queue_event(&mut block, Some(true));
                assert_eq!(vq.used.idx.get(), 1);
                assert_eq!(vq.used.ring[0].get().id, 0);
                assert_eq!(vq.used.ring[0].get().len, 21);
                assert_eq!(mem.read_obj::<u32>(status_addr).unwrap(), VIRTIO_BLK_S_OK);

                let blk_meta = blk_metadata.unwrap();
                let expected_device_id = format!(
                    "{}{}{}",
                    blk_meta.st_dev(),
                    blk_meta.st_rdev(),
                    blk_meta.st_ino()
                );

                let mut buf = [0; VIRTIO_BLK_ID_BYTES as usize];
                mem.read_slice(&mut buf, data_addr).unwrap();
                let chars_to_trim: &[char] = &['\u{0}'];
                let received_device_id = String::from_utf8(buf.to_ascii_lowercase())
                    .unwrap()
                    .trim_matches(chars_to_trim)
                    .to_string();
                assert_eq!(received_device_id, expected_device_id);
            }

            // Test that a device ID request will be discarded, if it fails to provide enough buffer
            // space.
            {
                vq.used.idx.set(0);
                set_queue(&mut block, 0, vq.create_queue());
                vq.dtable[1].len.set(VIRTIO_BLK_ID_BYTES - 1);

                mem.write_obj::<u32>(VIRTIO_BLK_T_GET_ID, request_type_addr)
                    .unwrap();

                simulate_queue_event(&mut block, Some(true));
                assert_eq!(vq.used.idx.get(), 1);
                assert_eq!(vq.used.ring[0].get().id, 0);
                assert_eq!(vq.used.ring[0].get().len, 0);
            }
        }
    }

    fn add_flush_requests_batch(block: &mut VirtioBlock, vq: &VirtQueue, count: u16) {
        let mem = vq.memory();
        vq.avail.idx.set(0);
        vq.used.idx.set(0);
        set_queue(block, 0, vq.create_queue());

        let hdr_addr = vq
            .end()
            .checked_align_up(std::mem::align_of::<RequestHeader>() as u64)
            .unwrap();
        // Write request header. All requests will use the same header.
        mem.write_obj(RequestHeader::new(VIRTIO_BLK_T_FLUSH, 0), hdr_addr)
            .unwrap();

        let mut status_addr = hdr_addr
            .checked_add(std::mem::size_of::<RequestHeader>() as u64)
            .unwrap()
            .checked_align_up(4)
            .unwrap();

        for i in 0..count {
            let idx = i * 2;

            let hdr_desc = &vq.dtable[idx as usize];
            hdr_desc.addr.set(hdr_addr.0);
            hdr_desc.flags.set(VIRTQ_DESC_F_NEXT);
            hdr_desc.next.set(idx + 1);

            let status_desc = &vq.dtable[idx as usize + 1];
            status_desc.addr.set(status_addr.0);
            status_desc.flags.set(VIRTQ_DESC_F_WRITE);
            status_desc.len.set(4);
            status_addr = status_addr.checked_add(4).unwrap();

            vq.avail.ring[i as usize].set(idx);
            vq.avail.idx.set(i + 1);
        }
    }

    fn check_flush_requests_batch(count: u16, vq: &VirtQueue) {
        let used_idx = vq.used.idx.get();
        assert_eq!(used_idx, count);

        for i in 0..count {
            let used = vq.used.ring[i as usize].get();
            let status_addr = vq.dtable[used.id as usize + 1].addr.get();
            assert_eq!(used.len, 1);
            assert_eq!(
                u32::from(
                    vq.memory()
                        .read_obj::<u8>(GuestAddress(status_addr))
                        .unwrap(),
                ),
                VIRTIO_BLK_S_OK
            );
        }
    }

    #[test]
    fn test_io_engine_throttling() {
        // FullSQueue BlockError
        {
            let mut block = default_block(FileEngineType::Async);

            let mem = default_mem();
            let interrupt = default_interrupt();
            let vq = VirtQueue::new(GuestAddress(0), &mem, IO_URING_NUM_ENTRIES * 4);
            block.queues[0] = vq.create_queue();
            block.activate(mem.clone(), interrupt).unwrap();

            // Run scenario that doesn't trigger FullSq BlockError: Add sq_size flush requests.
            add_flush_requests_batch(&mut block, &vq, IO_URING_NUM_ENTRIES);
            simulate_queue_event(&mut block, Some(false));
            assert!(!block.runtime_state().is_io_engine_throttled);
            simulate_async_completion_event(&mut block, true);
            check_flush_requests_batch(IO_URING_NUM_ENTRIES, &vq);

            // Run scenario that triggers FullSqError : Add sq_size + 10 flush requests.
            add_flush_requests_batch(&mut block, &vq, IO_URING_NUM_ENTRIES + 10);
            simulate_queue_event(&mut block, Some(false));
            assert!(block.runtime_state().is_io_engine_throttled);
            // When the async_completion_event is triggered:
            // 1. sq_size requests should be processed processed.
            // 2. is_io_engine_throttled should be set back to false.
            // 3. process_queue() should be called again.
            simulate_async_completion_event(&mut block, true);
            assert!(!block.runtime_state().is_io_engine_throttled);
            check_flush_requests_batch(IO_URING_NUM_ENTRIES, &vq);
            // check that process_queue() was called again resulting in the processing of the
            // remaining 10 ops.
            simulate_async_completion_event(&mut block, true);
            assert!(!block.runtime_state().is_io_engine_throttled);
            check_flush_requests_batch(IO_URING_NUM_ENTRIES + 10, &vq);
        }

        // FullCQueue BlockError
        {
            let mut block = default_block(FileEngineType::Async);

            let mem = default_mem();
            let interrupt = default_interrupt();
            let vq = VirtQueue::new(GuestAddress(0), &mem, IO_URING_NUM_ENTRIES * 4);
            block.queues[0] = vq.create_queue();
            block.activate(mem.clone(), interrupt).unwrap();

            // Run scenario that triggers FullCqError. Push 2 * IO_URING_NUM_ENTRIES and wait for
            // completion. Then try to push another entry.
            add_flush_requests_batch(&mut block, &vq, IO_URING_NUM_ENTRIES);
            simulate_queue_event(&mut block, Some(false));
            assert!(!block.runtime_state().is_io_engine_throttled);
            thread::sleep(Duration::from_millis(150));
            add_flush_requests_batch(&mut block, &vq, IO_URING_NUM_ENTRIES);
            simulate_queue_event(&mut block, Some(false));
            assert!(!block.runtime_state().is_io_engine_throttled);
            thread::sleep(Duration::from_millis(150));

            add_flush_requests_batch(&mut block, &vq, 1);
            simulate_queue_event(&mut block, Some(false));
            assert!(block.runtime_state().is_io_engine_throttled);
            simulate_async_completion_event(&mut block, true);
            assert!(!block.runtime_state().is_io_engine_throttled);
            check_flush_requests_batch(IO_URING_NUM_ENTRIES * 2, &vq);
        }
    }

    #[test]
    fn test_prepare_save() {
        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let mut block = default_block(engine);

            let mem = default_mem();
            let interrupt = default_interrupt();
            let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
            block.queues[0] = vq.create_queue();
            block.activate(mem.clone(), interrupt).unwrap();

            // Add a batch of flush requests.
            add_flush_requests_batch(&mut block, &vq, 5);
            simulate_queue_event(&mut block, None);
            block.prepare_save();

            // Check that all the pending flush requests were processed during `prepare_save()`.
            check_flush_requests_batch(5, &vq);
        }
    }

    #[test]
    fn test_bandwidth_rate_limiter() {
        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let mut block = default_block(engine);
            let mem = default_mem();
            let interrupt = default_interrupt();
            let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
            set_queue(&mut block, 0, vq.create_queue());
            block.activate(mem.clone(), interrupt).unwrap();
            read_blk_req_descriptors(&vq);

            let request_type_addr = GuestAddress(vq.dtable[0].addr.get());
            let data_addr = GuestAddress(vq.dtable[1].addr.get());
            let status_addr = GuestAddress(vq.dtable[2].addr.get());

            // Create bandwidth rate limiter that allows only 5120 bytes/s with bucket size of 8
            // bytes.
            let mut rl = RateLimiter::new(512, 0, 100, 0, 0, 0);
            // Use up the budget.
            assert!(rl.consume(512, TokenType::Bytes));

            set_rate_limiter(&mut block, rl);

            mem.write_obj::<u32>(VIRTIO_BLK_T_OUT, request_type_addr)
                .unwrap();
            // Make data read only, 512 bytes in len, and set the actual value to be written
            vq.dtable[1].flags.set(VIRTQ_DESC_F_NEXT);
            vq.dtable[1].len.set(512);
            mem.write_obj::<u64>(123_456_789, data_addr).unwrap();

            // Following write procedure should fail because of bandwidth rate limiting.
            {
                // Trigger the attempt to write.
                check_metric_after_block!(
                    &block.metrics.rate_limiter_throttled_events,
                    1,
                    simulate_queue_event(&mut block, Some(false))
                );

                // Assert that limiter is blocked.
                assert!(block.runtime_state().rate_limiter.is_blocked());
                // Make sure the data is still queued for processing.
                assert_eq!(vq.used.idx.get(), 0);
            }

            // Wait for 100ms to give the rate-limiter timer a chance to replenish.
            // Wait for an extra 50ms to make sure the timerfd event makes its way from the kernel.
            thread::sleep(Duration::from_millis(150));

            // Following write procedure should succeed because bandwidth should now be available.
            {
                check_metric_after_block!(
                    &block.metrics.rate_limiter_throttled_events,
                    0,
                    block.runtime_state_mut().process_rate_limiter_event()
                );
                // Validate the rate_limiter is no longer blocked.
                assert!(!block.runtime_state().rate_limiter.is_blocked());
                // Complete async IO ops if needed
                simulate_async_completion_event(&mut block, true);

                // Make sure the data queue advanced.
                assert_eq!(vq.used.idx.get(), 1);
                assert_eq!(vq.used.ring[0].get().id, 0);
                assert_eq!(vq.used.ring[0].get().len, 1);
                assert_eq!(mem.read_obj::<u32>(status_addr).unwrap(), VIRTIO_BLK_S_OK);
            }
        }
    }

    #[test]
    fn test_ops_rate_limiter() {
        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let mut block = default_block(engine);
            let mem = default_mem();
            let interrupt = default_interrupt();
            let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
            set_queue(&mut block, 0, vq.create_queue());
            block.activate(mem.clone(), interrupt).unwrap();
            read_blk_req_descriptors(&vq);

            let request_type_addr = GuestAddress(vq.dtable[0].addr.get());
            let data_addr = GuestAddress(vq.dtable[1].addr.get());
            let status_addr = GuestAddress(vq.dtable[2].addr.get());

            // Create ops rate limiter that allows only 10 ops/s with bucket size of 1 ops.
            let mut rl = RateLimiter::new(0, 0, 0, 1, 0, 100);
            // Use up the budget.
            assert!(rl.consume(1, TokenType::Ops));

            set_rate_limiter(&mut block, rl);

            mem.write_obj::<u32>(VIRTIO_BLK_T_OUT, request_type_addr)
                .unwrap();
            // Make data read only, 512 bytes in len, and set the actual value to be written.
            vq.dtable[1].flags.set(VIRTQ_DESC_F_NEXT);
            vq.dtable[1].len.set(512);
            mem.write_obj::<u64>(123_456_789, data_addr).unwrap();

            // Following write procedure should fail because of ops rate limiting.
            {
                // Trigger the attempt to write.
                check_metric_after_block!(
                    &block.metrics.rate_limiter_throttled_events,
                    1,
                    simulate_queue_event(&mut block, Some(false))
                );

                // Assert that limiter is blocked.
                assert!(block.runtime_state().rate_limiter.is_blocked());
                // Make sure the data is still queued for processing.
                assert_eq!(vq.used.idx.get(), 0);
            }

            // Do a second write that still fails but this time on the fast path.
            {
                // Trigger the attempt to write.
                check_metric_after_block!(
                    &block.metrics.rate_limiter_throttled_events,
                    1,
                    simulate_queue_event(&mut block, Some(false))
                );

                // Assert that limiter is blocked.
                assert!(block.runtime_state().rate_limiter.is_blocked());
                // Make sure the data is still queued for processing.
                assert_eq!(vq.used.idx.get(), 0);
            }

            // Wait for 100ms to give the rate-limiter timer a chance to replenish.
            // Wait for an extra 50ms to make sure the timerfd event makes its way from the kernel.
            thread::sleep(Duration::from_millis(150));

            // Following write procedure should succeed because ops budget should now be available.
            {
                check_metric_after_block!(
                    &block.metrics.rate_limiter_throttled_events,
                    0,
                    block.runtime_state_mut().process_rate_limiter_event()
                );
                // Validate the rate_limiter is no longer blocked.
                assert!(!block.runtime_state().rate_limiter.is_blocked());
                // Complete async IO ops if needed
                simulate_async_completion_event(&mut block, true);

                // Make sure the data queue advanced.
                assert_eq!(vq.used.idx.get(), 1);
                assert_eq!(vq.used.ring[0].get().id, 0);
                assert_eq!(vq.used.ring[0].get().len, 1);
                assert_eq!(mem.read_obj::<u32>(status_addr).unwrap(), VIRTIO_BLK_S_OK);
            }
        }
    }

    #[test]
    fn test_update_disk_image() {
        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let mut block = default_block(engine);
            let mem = default_mem();
            let interrupt = default_interrupt();
            let vq = VirtQueue::new(GuestAddress(0), &mem, 16);
            set_queue(&mut block, 0, vq.create_queue());
            block.activate(mem, interrupt).unwrap();
            let f = TempFile::new().unwrap();
            let path = f.as_path();
            let mdata = metadata(path).unwrap();
            let mut id = vec![0; VIRTIO_BLK_ID_BYTES as usize];
            let str_id = format!("{}{}{}", mdata.st_dev(), mdata.st_rdev(), mdata.st_ino());
            let part_id = str_id.as_bytes();
            id[..cmp::min(part_id.len(), VIRTIO_BLK_ID_BYTES as usize)].clone_from_slice(
                &part_id[..cmp::min(part_id.len(), VIRTIO_BLK_ID_BYTES as usize)],
            );

            block
                .update_disk_image(String::from(path.to_str().unwrap()))
                .unwrap();

            assert_eq!(
                block.disk.file_engine.file().metadata().unwrap().st_ino(),
                mdata.st_ino()
            );
            assert_eq!(block.disk.image_id, id.as_slice());
        }
    }
}
