// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.

//! Handles routing to devices in an address space.

use std::cell::UnsafeCell;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex, Weak};

/// Trait for devices that respond to reads or writes in an arbitrary address space.
///
/// The device does not care where it exists in address space as each method is only given an offset
/// into its allocated portion of address space.
#[allow(unused_variables)]
pub trait BusDevice: Send {
    /// Reads at `offset` from this device
    fn read(&mut self, base: u64, offset: u64, data: &mut [u8]) {}
    /// Writes at `offset` into this device
    fn write(
        &mut self,
        range: &BusRange,
        base: u64,
        offset: u64,
        data: &[u8],
    ) -> Option<Arc<Barrier>> {
        None
    }
}

/// Error type for [`Bus`]-related operations.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum BusError {
    /// The insertion failed because the new device overlapped with an old device.
    Overlap,
    /// Failed to operate on zero sized range.
    ZeroSizedRange,
    /// Failed to find address range.
    MissingAddressRange,
    /// The supplied range is invalid.
    InvalidRange,
}

/// Address range occupied by a device.
///
/// Encoded as `base` + `len` in two independent `AtomicU64`s so that:
///  * readers can observe the range without locking or blocking any writer;
///  * a writer can atomically hide the range from readers by storing `len = 0`,
///    change `base` while the range is invisible, and republish the new
///    `(base, len)` — without racing with concurrent lookups.
///
/// `len == 0` is the "empty / in transition" sentinel: searches skip such
/// slots. Because `len` is only ever stored as `0` or as the real (non-zero)
/// length of the range, the seqlock-style double check on `len` in `snapshot`
/// is immune to ABA — the same `len` value at both endpoints implies the
/// intervening `base` load returned a consistent view of the current range.
#[derive(Debug, Default)]
pub struct BusRange {
    /// Base address (inclusive).
    pub base: AtomicU64,
    /// Length of the range in bytes. Zero means the slot is empty / in transition.
    pub len: AtomicU64,
}

impl BusRange {
    /// Create a new range covering `[base, base + len)`.
    ///
    /// Fails if `len == 0` or `base + len - 1` overflows.
    pub fn new(base: u64, len: u64) -> Result<Self, BusError> {
        if len == 0 {
            return Err(BusError::ZeroSizedRange);
        }
        base.checked_add(len - 1).ok_or(BusError::InvalidRange)?;
        Ok(BusRange {
            base: AtomicU64::new(base),
            len: AtomicU64::new(len),
        })
    }

    /// Base address of the range.
    pub fn base(&self) -> u64 {
        self.base.load(Ordering::Acquire)
    }

    /// Length of the range. Zero means empty / in transition.
    pub fn len(&self) -> u64 {
        self.len.load(Ordering::Acquire)
    }

    /// True when the range is currently empty / in transition.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reads a consistent `(base, len)` snapshot. Returns `(0, 0)` if the slot
    /// is empty or currently being updated by a writer.
    fn snapshot(&self) -> (u64, u64) {
        loop {
            let len1 = self.len.load(Ordering::Acquire);
            if len1 == 0 {
                return (0, 0);
            }
            let base = self.base.load(Ordering::Acquire);
            let len2 = self.len.load(Ordering::Acquire);
            if len1 == len2 {
                return (base, len1);
            }
            std::hint::spin_loop();
        }
    }

    /// Returns whether `addr` currently falls within this range.
    pub fn contains(&self, addr: u64) -> bool {
        let (base, len) = self.snapshot();
        len != 0 && base <= addr && addr - base < len
    }

    /// Returns whether this range overlaps with `other`.
    pub fn overlaps(&self, other: &BusRange) -> bool {
        let (a_base, a_len) = self.snapshot();
        let (b_base, b_len) = other.snapshot();
        if a_len == 0 || b_len == 0 {
            return false;
        }
        let a_end = a_base + a_len - 1;
        let b_end = b_base + b_len - 1;
        a_base <= b_end && b_base <= a_end
    }

    /// Publish a new `(base, len)` for this range. Writer-only.
    ///
    /// Sequence:
    ///  1. Store `len = 0` — the range becomes invisible to lookups.
    ///  2. Store the new `base` while the range is still invisible.
    ///  3. Store the new (non-zero) `len` — republishes with the new base.
    pub fn set(&self, new_base: u64, new_len: u64) {
        self.len.store(0, Ordering::Release);
        self.base.store(new_base, Ordering::Release);
        self.len.store(new_len, Ordering::Release);
    }

    /// Hide the range from lookups. Writer-only.
    pub fn clear(&self) {
        self.len.store(0, Ordering::Release);
    }
}

/// One slot inside a [`Bus`]: an address range plus the device it points to.
///
/// The `Weak` lives in an `UnsafeCell` so the writer can update it through
/// `&self`. It is only mutated by the writer thread and only while
/// `range.len == 0`. Readers only touch it after having observed
/// `range.len != 0` — the release-store that published that `len` also
/// published the `device` write, so the read is data-race free.
#[derive(Default)]
struct Slot {
    range: BusRange,
    device: UnsafeCell<Option<Weak<Mutex<dyn BusDevice>>>>,
}

impl fmt::Debug for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Slot").field("range", &self.range).finish()
    }
}

// SAFETY: access to `device` is coordinated via `range.len`. See the type
// comment above. Writers only touch it while readers are guaranteed not to,
// and readers only touch it in a happens-after position relative to the
// writer's release-store of a non-zero `len`.
unsafe impl Sync for Slot {}

/// A device container for routing reads and writes over some address space.
///
/// Invariants:
///  * At most one writer thread performs `insert` / `remove` at a time.
///    Readers may run concurrently with the writer and with each other; none
///    of them block.
///  * Currently-visible ranges do not overlap.
#[derive(Debug)]
pub struct Bus {
    slots: [Slot; NUM_SLOTS],
}

const NUM_SLOTS: usize = 32;

impl Default for Bus {
    fn default() -> Self {
        Bus {
            slots: std::array::from_fn(|_| Slot::default()),
        }
    }
}

impl Bus {
    /// Constructs an empty bus.
    pub fn new() -> Bus {
        Bus::default()
    }

    /// Insert a device into the [`Bus`] in the range `[base, base + len)`.
    ///
    /// Must only be called from the (single) writer thread.
    pub fn insert(
        &self,
        device: Arc<Mutex<dyn BusDevice>>,
        base: u64,
        len: u64,
    ) -> Result<(), BusError> {
        if len == 0 {
            return Err(BusError::ZeroSizedRange);
        }
        base.checked_add(len - 1).ok_or(BusError::InvalidRange)?;

        // Reject overlap with any currently visible range. Slots with `len == 0`
        // (empty or transitioning) are ignored.
        let new_end = base + len - 1;
        for slot in self.slots.iter() {
            let (sbase, slen) = slot.range.snapshot();
            if slen == 0 {
                continue;
            }
            let send = sbase + slen - 1;
            if base <= send && sbase <= new_end {
                return Err(BusError::Overlap);
            }
        }

        // Find first empty slot and publish the device.
        for slot in self.slots.iter() {
            if slot.range.len.load(Ordering::Acquire) != 0 {
                continue;
            }
            // SAFETY: writer is single-threaded, and readers never dereference
            // `device` while `range.len == 0`. It is safe to overwrite the
            // `Weak` in place here.
            unsafe {
                *slot.device.get() = Some(Arc::downgrade(&device));
            }
            // Publish the base while the range is still invisible to readers.
            slot.range.base.store(base, Ordering::Release);
            // Publish `len` last; the release-store makes both the `base`
            // above and the `device` write visible to any reader that
            // acquire-loads this non-zero `len`.
            slot.range.len.store(len, Ordering::Release);
            return Ok(());
        }
        Err(BusError::Overlap)
    }

    /// Removes the device at the given address space range.
    ///
    /// Must only be called from the (single) writer thread.
    pub fn remove(&self, base: u64, len: u64) -> Result<(), BusError> {
        if len == 0 {
            return Err(BusError::ZeroSizedRange);
        }
        base.checked_add(len - 1).ok_or(BusError::InvalidRange)?;

        for slot in self.slots.iter() {
            let (sbase, slen) = slot.range.snapshot();
            if slen == len && sbase == base {
                slot.range.clear();
                return Ok(());
            }
        }
        Err(BusError::MissingAddressRange)
    }

    // Locate the device whose range currently contains `addr`, lock it, and
    // invoke `f`. Never blocks on the bus itself — only on the device mutex.
    fn with_device<T>(
        &self,
        addr: u64,
        f: impl FnOnce(&mut dyn BusDevice, &BusRange, u64, u64) -> T,
    ) -> Result<T, BusError> {
        for slot in self.slots.iter() {
            if !slot.range.contains(addr) {
                continue;
            }
            // SAFETY: we observed `range.len != 0` via `contains`, which used
            // an acquire load. That synchronizes-with the writer's release-store
            // of `len`, which was preceded by the write to `device`, so it is
            // safe to read the `Weak` here.
            let weak = unsafe { (*slot.device.get()).as_ref() };
            if let Some(device) = weak.and_then(|w| w.upgrade()) {
                let mut device = device.lock().unwrap();
                let base = slot.range.base();
                let offset = addr.wrapping_sub(base);
                return Ok(f(&mut *device, &slot.range, base, offset));
            }
        }
        Err(BusError::MissingAddressRange)
    }

    /// Reads data from the device that owns the range containing `addr` and puts it into `data`.
    ///
    /// Returns true on success, otherwise `data` is untouched.
    pub fn read(&self, addr: u64, data: &mut [u8]) -> Result<(), BusError> {
        self.with_device(addr, |dev, _range, base, offset| {
            dev.read(base, offset, data)
        })
    }

    /// Writes `data` to the device that owns the range containing `addr`.
    ///
    /// Returns true on success, otherwise `data` is untouched.
    pub fn write(&self, addr: u64, data: &[u8]) -> Result<Option<Arc<Barrier>>, BusError> {
        self.with_device(addr, |dev, range, base, offset| {
            dev.write(range, base, offset, data)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyDevice;
    impl BusDevice for DummyDevice {}

    struct ConstantDevice;
    impl BusDevice for ConstantDevice {
        #[allow(clippy::cast_possible_truncation)]
        fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
            for (i, v) in data.iter_mut().enumerate() {
                *v = (offset as u8) + (i as u8);
            }
        }

        #[allow(clippy::cast_possible_truncation)]
        fn write(&mut self, _base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
            for (i, v) in data.iter().enumerate() {
                assert_eq!(*v, (offset as u8) + (i as u8))
            }

            None
        }
    }

    #[test]
    fn bus_range_new() {
        // Zero length is invalid.
        assert!(matches!(BusRange::new(0, 0), Err(BusError::ZeroSizedRange)));
        assert!(matches!(
            BusRange::new(u64::MAX, 0),
            Err(BusError::ZeroSizedRange)
        ));

        // Overflow is invalid.
        assert!(matches!(
            BusRange::new(u64::MAX, 2),
            Err(BusError::InvalidRange)
        ));
        assert!(matches!(
            BusRange::new(2, u64::MAX),
            Err(BusError::InvalidRange)
        ));

        // Ranges that exactly reach u64::MAX are valid.
        let r = BusRange::new(u64::MAX, 1).unwrap();
        assert_eq!(r.base(), u64::MAX);
        assert_eq!(r.end(), u64::MAX);

        let r = BusRange::new(1, u64::MAX).unwrap();
        assert_eq!(r.base(), 1);
        assert_eq!(r.end(), u64::MAX);

        let r = BusRange::new(u64::MAX - 4095, 4096).unwrap();
        assert_eq!(r.base(), u64::MAX - 4095);
        assert_eq!(r.end(), u64::MAX);

        // One sized valid range.
        let r = BusRange::new(0, 1).unwrap();
        assert_eq!(r.base(), 0);
        assert_eq!(r.end(), 0);

        // Normal valid range.
        let r = BusRange::new(0x1000, 0x400).unwrap();
        assert_eq!(r.base(), 0x1000);
        assert_eq!(r.end(), 0x13ff);
    }

    #[test]
    fn bus_insert() {
        let bus = Bus::new();
        let dummy = Arc::new(Mutex::new(DummyDevice));
        bus.insert(dummy.clone(), 0x10, 0).unwrap_err();
        bus.insert(dummy.clone(), 0x10, 0x10).unwrap();

        let result = bus.insert(dummy.clone(), 0x0f, 0x10);
        assert_eq!(format!("{result:?}"), "Err(Overlap)");

        bus.insert(dummy.clone(), 0x10, 0x10).unwrap_err();
        bus.insert(dummy.clone(), 0x10, 0x15).unwrap_err();
        bus.insert(dummy.clone(), 0x12, 0x15).unwrap_err();
        bus.insert(dummy.clone(), 0x12, 0x01).unwrap_err();
        bus.insert(dummy.clone(), 0x0, 0x20).unwrap_err();
        bus.insert(dummy.clone(), 0x20, 0x05).unwrap();
        bus.insert(dummy.clone(), 0x25, 0x05).unwrap();
        bus.insert(dummy, 0x0, 0x10).unwrap();
    }

    #[test]
    fn bus_remove() {
        let bus = Bus::new();
        let dummy = Arc::new(Mutex::new(DummyDevice));

        bus.remove(0x42, 0x0).unwrap_err();

        bus.remove(0x13, 0x12).unwrap_err();

        bus.insert(dummy.clone(), 0x13, 0x12).unwrap();
        bus.remove(0x42, 0x42).unwrap_err();
        bus.remove(0x13, 0x12).unwrap();
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn bus_read_write() {
        let bus = Bus::new();
        let dummy = Arc::new(Mutex::new(DummyDevice));
        bus.insert(dummy.clone(), 0x10, 0x10).unwrap();
        bus.read(0x10, &mut [0, 0, 0, 0]).unwrap();
        bus.write(0x10, &[0, 0, 0, 0]).unwrap();
        bus.read(0x11, &mut [0, 0, 0, 0]).unwrap();
        bus.write(0x11, &[0, 0, 0, 0]).unwrap();
        bus.read(0x16, &mut [0, 0, 0, 0]).unwrap();
        bus.write(0x16, &[0, 0, 0, 0]).unwrap();
        bus.read(0x20, &mut [0, 0, 0, 0]).unwrap_err();
        bus.write(0x20, &[0, 0, 0, 0]).unwrap_err();
        bus.read(0x06, &mut [0, 0, 0, 0]).unwrap_err();
        bus.write(0x06, &[0, 0, 0, 0]).unwrap_err();
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn bus_read_write_values() {
        let bus = Bus::new();
        let dummy = Arc::new(Mutex::new(ConstantDevice));
        bus.insert(dummy.clone(), 0x10, 0x10).unwrap();

        let mut values = [0, 1, 2, 3];
        bus.read(0x10, &mut values).unwrap();
        assert_eq!(values, [0, 1, 2, 3]);
        bus.write(0x10, &values).unwrap();
        bus.read(0x15, &mut values).unwrap();
        assert_eq!(values, [5, 6, 7, 8]);
        bus.write(0x15, &values).unwrap();
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn busrange_cmp() {
        let range = BusRange::new(0x10, 2).unwrap();
        assert_eq!(range, BusRange::new(0x10, 3).unwrap());
        assert_eq!(range, BusRange::new(0x10, 2).unwrap());

        assert!(range < BusRange::new(0x12, 1).unwrap());
        assert!(range < BusRange::new(0x12, 3).unwrap());

        assert_eq!(range, range.clone());

        let bus = Bus::new();
        let mut data = [1, 2, 3, 4];
        let device = Arc::new(Mutex::new(DummyDevice));
        bus.insert(device.clone(), 0x10, 0x10).unwrap();
        bus.write(0x10, &data).unwrap();
        bus.read(0x10, &mut data).unwrap();
        assert_eq!(data, [1, 2, 3, 4]);
    }

    #[test]
    fn bus_range_overlap() {
        let a = BusRange::new(0x1000, 0x400).unwrap();
        assert!(a.overlaps(&BusRange::new(0x1000, 0x400).unwrap()));
        assert!(a.overlaps(&BusRange::new(0xf00, 0x400).unwrap()));
        assert!(a.overlaps(&BusRange::new(0x1000, 0x01).unwrap()));
        assert!(a.overlaps(&BusRange::new(0xfff, 0x02).unwrap()));
        assert!(a.overlaps(&BusRange::new(0x1100, 0x100).unwrap()));
        assert!(a.overlaps(&BusRange::new(0x13ff, 0x100).unwrap()));
        assert!(!a.overlaps(&BusRange::new(0x1400, 0x100).unwrap()));
        assert!(!a.overlaps(&BusRange::new(0xf00, 0x100).unwrap()));
    }
}
