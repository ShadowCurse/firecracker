// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.

//! Handles routing to devices in an address space.

use std::sync::{Arc, Barrier, Mutex, RwLock, Weak};

use crate::utils::usize_to_u64;

/// Trait for devices that respond to reads or writes in an arbitrary address space.
///
/// The device does not care where it exists in address space as each method is only given the
/// tag of the accessed range and an offset into it.
#[allow(unused_variables)]
pub trait BusDevice: Send {
    /// Reads at `offset` from the range `tag` of this device
    fn read(&mut self, tag: u8, offset: u64, data: &mut [u8]) {}
    /// Writes at `offset` into the range `tag` of this device
    fn write(&mut self, tag: u8, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
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
    /// The bus has no room for another range.
    Full,
}

/// Ranges on the MMIO bus: 32 PCI slots, each holding a root port and a device
/// with up to 3 ranges, leave half of it for the MMIO devices.
pub const MMIO_BUS_ENTRIES_MAX: u16 = 256;
/// Ranges on the PIO bus: serial, i8042 and PCI config space, with spare room.
pub const PIO_BUS_ENTRIES_MAX: u16 = 8;

/// `size` takes the low bits of [`BusEntry::size_and_tag`] and `tag` the high byte.
const BUS_ENTRY_SIZE_BITS: u32 = 56;
const BUS_ENTRY_SIZE_MAX: u64 = (1 << BUS_ENTRY_SIZE_BITS) - 1;
const _: () = assert!(BUS_ENTRY_SIZE_BITS + u8::BITS == u64::BITS);

/// A device range [`base`, `base` + `size`) on a [`Bus`].
#[derive(Debug)]
struct BusEntry {
    device: Weak<Mutex<dyn BusDevice>>,
    base: u64,
    /// The tag identifies the range to the device. Unlike `base` it is fixed
    /// for the lifetime of the range, so an access racing a move is still
    /// routed to the range it was resolved to.
    size_and_tag: u64,
}
// Two entries per cache line.
const _: () = assert!(std::mem::size_of::<BusEntry>() == 32);

impl BusEntry {
    fn new(device: Weak<Mutex<dyn BusDevice>>, base: u64, size: u64, tag: u8) -> BusEntry {
        assert!(0 < size);
        assert!(size <= BUS_ENTRY_SIZE_MAX);
        let size_and_tag = (u64::from(tag) << BUS_ENTRY_SIZE_BITS) | size;
        BusEntry {
            device,
            base,
            size_and_tag,
        }
    }

    fn size(&self) -> u64 {
        self.size_and_tag & BUS_ENTRY_SIZE_MAX
    }

    fn tag(&self) -> u8 {
        let tag = self.size_and_tag >> BUS_ENTRY_SIZE_BITS;
        u8::try_from(tag).unwrap()
    }

    fn last(&self) -> u64 {
        let size = self.size();
        assert!(0 < size);
        self.base + (size - 1)
    }
}

// Return the last address of [`base`, `base` + `size`).
fn range_last(base: u64, size: u64) -> Result<u64, BusError> {
    if size == 0 {
        return Err(BusError::ZeroSizedRange);
    }
    base.checked_add(size - 1).ok_or(BusError::InvalidRange)
}

// Assert `entries` are sorted by base and do not overlap.
fn entries_assert_valid(entries: &[BusEntry]) {
    for pair in entries.windows(2) {
        assert!(pair[0].last() < pair[1].base);
    }
}

// Return the index at which [`base`, `last`] keeps `entries` sorted and not overlapping.
fn entries_free_index(entries: &[BusEntry], base: u64, last: u64) -> Result<usize, BusError> {
    assert!(base <= last);
    let index = entries.partition_point(|entry| entry.base < base);
    if 0 < index {
        let previous = &entries[index - 1];
        if base <= previous.last() {
            return Err(BusError::Overlap);
        }
    }
    if index < entries.len() {
        let next = &entries[index];
        if next.base <= last {
            return Err(BusError::Overlap);
        }
    }
    Ok(index)
}

// Return the index of the entry registered at [`base`, `base` + `size`).
fn entries_find_index(entries: &[BusEntry], base: u64, size: u64) -> Result<usize, BusError> {
    range_last(base, size)?;
    let Ok(index) = entries.binary_search_by_key(&base, |entry| entry.base) else {
        return Err(BusError::MissingAddressRange);
    };
    assert_eq!(
        entries[index].size(),
        size,
        "Bus mapping at {base:#x} is registered with size {}, not {size}",
        entries[index].size()
    );
    Ok(index)
}

/// A device container for routing reads and writes over some address space.
///
/// This doesn't have any restrictions on what kind of device or address space this applies to. The
/// only restriction is that no two devices can overlap in this address space.
///
/// `entries` is only held while resolving an address, never while calling
/// into a device, so a device can move its own range during an access.
///
/// Lock ordering: `gate` -> device -> `entries`.
#[derive(Debug)]
pub struct Bus {
    /// Held (read) for the duration of an access so that [`Bus::remove`]
    /// blocks until any in-flight access to the device has finished.
    gate: RwLock<()>,
    /// Device ranges sorted by base. Never grows past its initial capacity.
    entries: RwLock<Vec<BusEntry>>,
}

impl Bus {
    /// Constructs a bus with an empty address space for up to `entries_max` ranges.
    pub fn new(entries_max: u16) -> Bus {
        assert!(0 < entries_max);
        let entries = Vec::with_capacity(usize::from(entries_max));
        Bus {
            gate: RwLock::new(()),
            entries: RwLock::new(entries),
        }
    }

    /// Insert a device into the [`Bus`] in the range [`base`, `base` + `size`).
    /// Accesses to the range are passed to the device with `tag`.
    pub fn insert(
        &self,
        device: Arc<Mutex<dyn BusDevice>>,
        base: u64,
        size: u64,
        tag: u8,
    ) -> Result<(), BusError> {
        let last = range_last(base, size)?;
        if BUS_ENTRY_SIZE_MAX < size {
            return Err(BusError::InvalidRange);
        }

        let mut entries = self.entries.write().unwrap();
        if entries.len() == entries.capacity() {
            return Err(BusError::Full);
        }
        let index = entries_free_index(&entries, base, last)?;
        let device = Arc::downgrade(&device);
        let entry = BusEntry::new(device, base, size, tag);
        entries.insert(index, entry);
        entries_assert_valid(&entries);
        Ok(())
    }

    /// Removes the device at the given address space range.
    pub fn remove(&self, base: u64, size: u64) -> Result<(), BusError> {
        let _gate = self.gate.write().unwrap();
        let mut entries = self.entries.write().unwrap();
        let index = entries_find_index(&entries, base, size)?;
        entries.remove(index);
        Ok(())
    }

    // Perform `operation` on the device owning [`address`, `address` + `size`).
    fn with_device<T>(
        &self,
        address: u64,
        size: u64,
        operation: impl FnOnce(&mut dyn BusDevice, u8, u64) -> T,
    ) -> Result<T, BusError> {
        let last = range_last(address, size)?;

        // Declared before `device`, so it is released after it.
        let _gate = self.gate.read().unwrap();
        let entries = self.entries.read().unwrap();
        let index = entries.partition_point(|entry| entry.base <= address);
        if index == 0 {
            return Err(BusError::MissingAddressRange);
        }
        let entry = &entries[index - 1];
        assert!(entry.base <= address);
        if entry.last() < last {
            return Err(BusError::MissingAddressRange);
        }
        let tag = entry.tag();
        let offset = address - entry.base;
        let device = entry.device.upgrade();
        drop(entries);

        let Some(device) = device else {
            return Err(BusError::MissingAddressRange);
        };
        let mut device_locked = device.lock().unwrap();
        Ok(operation(&mut *device_locked, tag, offset))
    }

    /// Relocate an existing device range from `base_old` to `base_new`.
    pub fn move_range(&self, base_old: u64, base_new: u64, size: u64) -> Result<(), BusError> {
        let last_new = range_last(base_new, size)?;
        let mut entries = self.entries.write().unwrap();
        let index_old = entries_find_index(&entries, base_old, size)?;

        // Removed first because the new range might overlap the old range.
        let mut entry = entries.remove(index_old);
        let index_new = match entries_free_index(&entries, base_new, last_new) {
            Ok(index_new) => index_new,
            Err(error) => {
                entries.insert(index_old, entry);
                return Err(error);
            }
        };
        entry.base = base_new;
        entries.insert(index_new, entry);
        entries_assert_valid(&entries);
        Ok(())
    }

    /// Reads data from the device that owns the range containing `address` and puts it into `data`.
    pub fn read(&self, address: u64, data: &mut [u8]) -> Result<(), BusError> {
        let size = usize_to_u64(data.len());
        self.with_device(address, size, |device, tag, offset| {
            device.read(tag, offset, data)
        })
    }

    /// Writes `data` to the device that owns the range containing `address`.
    pub fn write(&self, address: u64, data: &[u8]) -> Result<Option<Arc<Barrier>>, BusError> {
        let size = usize_to_u64(data.len());
        self.with_device(address, size, |device, tag, offset| {
            device.write(tag, offset, data)
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
        fn read(&mut self, _tag: u8, offset: u64, data: &mut [u8]) {
            for (i, v) in data.iter_mut().enumerate() {
                *v = (offset as u8) + (i as u8);
            }
        }

        #[allow(clippy::cast_possible_truncation)]
        fn write(&mut self, _tag: u8, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
            for (i, v) in data.iter().enumerate() {
                assert_eq!(*v, (offset as u8) + (i as u8))
            }

            None
        }
    }

    #[test]
    fn bus_range_last() {
        assert!(matches!(range_last(0, 0), Err(BusError::ZeroSizedRange)));
        assert!(matches!(
            range_last(u64::MAX, 0),
            Err(BusError::ZeroSizedRange)
        ));
        assert!(matches!(
            range_last(u64::MAX, 2),
            Err(BusError::InvalidRange)
        ));
        assert!(matches!(
            range_last(2, u64::MAX),
            Err(BusError::InvalidRange)
        ));
        assert_eq!(range_last(u64::MAX, 1).unwrap(), u64::MAX);
        assert_eq!(range_last(1, u64::MAX).unwrap(), u64::MAX);
        assert_eq!(range_last(0, 1).unwrap(), 0);
        assert_eq!(range_last(0x1000, 0x400).unwrap(), 0x13ff);
    }

    /// A device that answers every read with its own id, so that a test can
    /// tell which device an access was routed to.
    struct IdDevice(u8);
    impl BusDevice for IdDevice {
        fn read(&mut self, _tag: u8, _offset: u64, data: &mut [u8]) {
            data.fill(self.0);
        }
    }

    #[test]
    fn bus_multiple_devices() {
        let bus = Bus::new(16);
        let dev_a = Arc::new(Mutex::new(IdDevice(0xa)));
        let dev_b = Arc::new(Mutex::new(IdDevice(0xb)));
        let dev_c = Arc::new(Mutex::new(IdDevice(0xc)));
        let dev_d = Arc::new(Mutex::new(IdDevice(0xd)));
        let dev_e = Arc::new(Mutex::new(IdDevice(0xe)));

        let read = |addr| {
            let mut data = [0u8; 1];
            bus.read(addr, &mut data).unwrap();
            data[0]
        };

        bus.insert(dev_a.clone(), 0x1000, 0x100, 0).unwrap();
        bus.insert(dev_b.clone(), 0x2000, 0x100, 0).unwrap();
        bus.insert(dev_c.clone(), 0x3000, 0x100, 0).unwrap();

        // Every range decodes to the device that owns it, at both ends.
        assert_eq!(read(0x1000), 0xa);
        assert_eq!(read(0x10ff), 0xa);
        assert_eq!(read(0x2000), 0xb);
        assert_eq!(read(0x20ff), 0xb);
        assert_eq!(read(0x3000), 0xc);
        assert_eq!(read(0x30ff), 0xc);

        // Removing the middle device leaves its range undecoded and does not
        // disturb the routing of the others.
        bus.remove(0x2000, 0x100).unwrap();
        bus.read(0x2000, &mut [0; 1]).unwrap_err();
        assert_eq!(read(0x1000), 0xa);
        assert_eq!(read(0x3000), 0xc);

        // The new device must only answer within its own range, and the
        // removed range must stay dead.
        bus.insert(dev_d.clone(), 0x4000, 0x100, 0).unwrap();
        assert_eq!(read(0x4000), 0xd);
        bus.read(0x2000, &mut [0; 1]).unwrap_err();
        assert_eq!(read(0x1000), 0xa);
        assert_eq!(read(0x3000), 0xc);

        // Reusing the freed range rebinds it to the new device
        bus.insert(dev_e.clone(), 0x2000, 0x100, 0).unwrap();
        assert_eq!(read(0x2000), 0xe);

        // Accesses must be fully contained in a single device's range. Add a
        // device right after `dev_e` and one at the very top of the address
        // space to exercise the boundaries.
        bus.insert(dev_a.clone(), 0x2100, 0x100, 0).unwrap();
        bus.insert(dev_a.clone(), u64::MAX, 1, 0).unwrap();

        // An access ending on the last byte of a range is served, while one
        // crossing into the adjacent device is served by neither.
        let mut data = [0; 4];
        bus.read(0x20fc, &mut data).unwrap();
        assert_eq!(data, [0xe; 4]);
        assert!(matches!(
            bus.read(0x20fe, &mut data),
            Err(BusError::MissingAddressRange)
        ));

        // An access wrapping around the end of the address space is rejected.
        bus.read(u64::MAX, &mut [0; 1]).unwrap();
        assert!(matches!(
            bus.read(u64::MAX, &mut data),
            Err(BusError::InvalidRange)
        ));

        // Zero sized accesses are rejected.
        assert!(matches!(
            bus.read(0x2000, &mut []),
            Err(BusError::ZeroSizedRange)
        ));
    }

    /// A device that answers every read with its id and the accessed tag
    /// and offset.
    struct TagDevice(u8);
    impl BusDevice for TagDevice {
        #[allow(clippy::cast_possible_truncation)]
        fn read(&mut self, tag: u8, offset: u64, data: &mut [u8]) {
            data.copy_from_slice(&[self.0, tag, offset as u8]);
        }
    }

    #[test]
    fn bus_access_racing_move_range() {
        let bus = Bus::new(16);
        let dev_a = Arc::new(Mutex::new(TagDevice(0xa)));
        let dev_b = Arc::new(Mutex::new(TagDevice(0xb)));
        bus.insert(dev_a.clone(), 0x1000, 0x100, 1).unwrap();
        bus.insert(dev_b.clone(), 0x2000, 0x100, 2).unwrap();

        // Hold the device lock like a config space write relocating a BAR
        // would, while another access to the old base waits for it.
        let locked = dev_a.lock().unwrap();
        std::thread::scope(|s| {
            let reader = s.spawn(|| {
                let mut data = [0u8; 3];
                bus.read(0x1004, &mut data).map(|()| data)
            });
            std::thread::sleep(std::time::Duration::from_millis(100));
            bus.move_range(0x1000, 0x3000, 0x100).unwrap();
            bus.move_range(0x2000, 0x1000, 0x100).unwrap();
            drop(locked);
            // The access finishes on the range it was resolved to, or on
            // the range moved in if it resolved late.
            let data = reader.join().unwrap().unwrap();
            assert!(data == [0xa, 1, 4] || data == [0xb, 2, 4], "{data:?}");
        });
    }

    #[test]
    fn bus_large_range() {
        let bus = Bus::new(16);
        let device = Arc::new(Mutex::new(TagDevice(0xa)));

        // A range larger than 4 GiB keeps both its size and its tag.
        bus.insert(device.clone(), 0x1_0000_0000, 0x4_0000_0000, 0xff)
            .unwrap();
        let mut data = [0u8; 3];
        bus.read(0x3_0000_0004, &mut data).unwrap();
        assert_eq!(data, [0xa, 0xff, 4]);
        bus.read(0x4_ffff_fffd, &mut data).unwrap();
        bus.read(0x4_ffff_fffe, &mut data).unwrap_err();

        // Sizes not fitting in the entry are rejected.
        assert!(matches!(
            bus.insert(device.clone(), 0, BUS_ENTRY_SIZE_MAX + 1, 0),
            Err(BusError::InvalidRange)
        ));
        bus.insert(device, 0x10_0000_0000, BUS_ENTRY_SIZE_MAX, 0)
            .unwrap();
    }

    #[test]
    fn bus_insert() {
        let bus = Bus::new(16);
        let dummy = Arc::new(Mutex::new(DummyDevice));
        bus.insert(dummy.clone(), 0x10, 0, 0).unwrap_err();
        bus.insert(dummy.clone(), 0x10, 0x10, 0).unwrap();

        let result = bus.insert(dummy.clone(), 0x0f, 0x10, 0);
        assert_eq!(format!("{result:?}"), "Err(Overlap)");

        bus.insert(dummy.clone(), 0x10, 0x10, 0).unwrap_err();
        bus.insert(dummy.clone(), 0x10, 0x15, 0).unwrap_err();
        bus.insert(dummy.clone(), 0x12, 0x15, 0).unwrap_err();
        bus.insert(dummy.clone(), 0x12, 0x01, 0).unwrap_err();
        bus.insert(dummy.clone(), 0x0, 0x20, 0).unwrap_err();
        bus.insert(dummy.clone(), 0x20, 0x05, 0).unwrap();
        bus.insert(dummy.clone(), 0x25, 0x05, 0).unwrap();
        bus.insert(dummy, 0x0, 0x10, 0).unwrap();
    }

    #[test]
    fn bus_full() {
        let bus = Bus::new(2);
        let dummy = Arc::new(Mutex::new(DummyDevice));
        let capacity = bus.entries.read().unwrap().capacity();
        for index in 0..capacity {
            let base = usize_to_u64(index) * 0x10;
            bus.insert(dummy.clone(), base, 0x10, 0).unwrap();
        }
        let base = usize_to_u64(capacity) * 0x10;
        assert!(matches!(
            bus.insert(dummy.clone(), base, 0x10, 0),
            Err(BusError::Full)
        ));
        assert_eq!(bus.entries.read().unwrap().capacity(), capacity);

        // Removing a range makes room for another one.
        bus.remove(0, 0x10).unwrap();
        bus.insert(dummy, base, 0x10, 0).unwrap();
    }

    #[test]
    fn bus_remove() {
        let bus = Bus::new(16);
        let dummy = Arc::new(Mutex::new(DummyDevice));

        bus.remove(0x42, 0x0).unwrap_err();

        bus.remove(0x13, 0x12).unwrap_err();

        bus.insert(dummy.clone(), 0x13, 0x12, 0).unwrap();
        bus.remove(0x42, 0x42).unwrap_err();
        bus.remove(0x13, 0x12).unwrap();
    }

    #[test]
    fn bus_move_range() {
        let bus = Bus::new(16);
        let dev = Arc::new(Mutex::new(ConstantDevice));
        bus.insert(dev.clone(), 0x10, 0x10, 0).unwrap();

        // A zero length is rejected before the mapping is touched.
        assert!(matches!(
            bus.move_range(0x10, 0x30, 0),
            Err(BusError::ZeroSizedRange)
        ));

        // Moving a range that is not registered fails and leaves the bus untouched.
        assert!(matches!(
            bus.move_range(0x2000, 0x3000, 0x10),
            Err(BusError::MissingAddressRange)
        ));

        // The device is reachable at its original base but not at the target.
        bus.read(0x10, &mut [0; 4]).unwrap();
        bus.read(0x30, &mut [0; 4]).unwrap_err();

        // Relocate the mapping; reads now resolve at the new base and the
        // ConstantDevice still answers relative to the (new) range base.
        bus.move_range(0x10, 0x30, 0x10).unwrap();
        bus.read(0x10, &mut [0; 4]).unwrap_err();
        let mut values = [0, 1, 2, 3];
        bus.read(0x35, &mut values).unwrap();
        assert_eq!(values, [5, 6, 7, 8]);

        // Moving a mapping onto its own base is a no-op.
        bus.move_range(0x30, 0x30, 0x10).unwrap();
        bus.read(0x35, &mut [0; 4]).unwrap();

        // A destination overlapping the source is accepted, because the old
        // range is removed before the overlap check runs.
        bus.move_range(0x30, 0x38, 0x10).unwrap();
        bus.read(0x30, &mut [0; 4]).unwrap_err();
        let mut values = [0, 1, 2, 3];
        bus.read(0x3d, &mut values).unwrap();
        assert_eq!(values, [5, 6, 7, 8]);

        // A move onto a range occupied by another device is rejected, and the
        // source mapping is left in place so nothing is lost.
        bus.insert(dev.clone(), 0x10, 0x10, 0).unwrap();
        assert!(matches!(
            bus.move_range(0x38, 0x18, 0x10),
            Err(BusError::Overlap)
        ));
        bus.read(0x3d, &mut [0; 4]).unwrap();
        bus.read(0x15, &mut [0; 4]).unwrap();
    }

    #[test]
    #[should_panic(expected = "is registered with size")]
    fn bus_move_range_wrong_len() {
        let bus = Bus::new(16);
        let dev = Arc::new(Mutex::new(DummyDevice));
        bus.insert(dev, 0x10, 0x10, 0).unwrap();
        let _ = bus.move_range(0x10, 0x30, 0x20);
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn bus_read_write() {
        let bus = Bus::new(16);
        let dummy = Arc::new(Mutex::new(DummyDevice));
        bus.insert(dummy.clone(), 0x10, 0x10, 0).unwrap();
        bus.read(0x10, &mut [0, 0, 0, 0]).unwrap();
        bus.write(0x10, &[0, 0, 0, 0]).unwrap();
        bus.read(0x11, &mut [0, 0, 0, 0]).unwrap();
        bus.write(0x11, &[0, 0, 0, 0]).unwrap();
        bus.read(0x16, &mut [0, 0, 0, 0]).unwrap();
        bus.write(0x16, &[0, 0, 0, 0]).unwrap();
        bus.read(0x1c, &mut [0, 0, 0, 0]).unwrap();
        bus.write(0x1c, &[0, 0, 0, 0]).unwrap();
        bus.read(0x1d, &mut [0, 0, 0, 0]).unwrap_err();
        bus.write(0x1d, &[0, 0, 0, 0]).unwrap_err();
        bus.read(0x20, &mut [0, 0, 0, 0]).unwrap_err();
        bus.write(0x20, &[0, 0, 0, 0]).unwrap_err();
        bus.read(0x06, &mut [0, 0, 0, 0]).unwrap_err();
        bus.write(0x06, &[0, 0, 0, 0]).unwrap_err();
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn bus_read_write_values() {
        let bus = Bus::new(16);
        let dummy = Arc::new(Mutex::new(ConstantDevice));
        bus.insert(dummy.clone(), 0x10, 0x10, 0).unwrap();

        let mut values = [0, 1, 2, 3];
        bus.read(0x10, &mut values).unwrap();
        assert_eq!(values, [0, 1, 2, 3]);
        bus.write(0x10, &values).unwrap();
        bus.read(0x15, &mut values).unwrap();
        assert_eq!(values, [5, 6, 7, 8]);
        bus.write(0x15, &values).unwrap();
    }

    #[test]
    fn bus_overlap() {
        let bus = Bus::new(16);
        let dummy = Arc::new(Mutex::new(DummyDevice));
        bus.insert(dummy.clone(), 0x1000, 0x400, 0).unwrap();
        for (base, len) in [
            (0x1000, 0x400),
            (0xf00, 0x400),
            (0x1000, 0x01),
            (0xfff, 0x02),
            (0x1100, 0x100),
            (0x13ff, 0x100),
        ] {
            assert!(matches!(
                bus.insert(dummy.clone(), base, len, 0),
                Err(BusError::Overlap)
            ));
        }
        bus.insert(dummy.clone(), 0x1400, 0x100, 0).unwrap();
        bus.insert(dummy, 0xf00, 0x100, 0).unwrap();
    }
}
