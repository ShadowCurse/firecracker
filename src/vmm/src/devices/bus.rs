// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

//! Handles routing to devices in an address space.

use std::cmp::{Ord, Ordering, PartialEq, PartialOrd};
use std::collections::btree_map::BTreeMap;
use std::sync::{Arc, Mutex};

/// Errors triggered during bus operations.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum BusError {
    /// New device overlaps with an old device.
    Overlap,
}

#[derive(Debug, Copy, Clone)]
struct BusRange(u64, u64);

impl Eq for BusRange {}

impl PartialEq for BusRange {
    fn eq(&self, other: &BusRange) -> bool {
        self.0 == other.0
    }
}

impl Ord for BusRange {
    fn cmp(&self, other: &BusRange) -> Ordering {
        self.0.cmp(&other.0)
    }
}

impl PartialOrd for BusRange {
    fn partial_cmp(&self, other: &BusRange) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone)]
struct BusDeviceVTable {
    ptr: *mut (),
    read_ptr: usize,
    write_ptr: usize,
}

impl BusDeviceVTable {
    pub fn read(&self, offset: u64, data: &mut [u8]) {
        unsafe {
            let read_fn: fn(*mut (), u64, &mut [u8]) = std::mem::transmute(self.read_ptr);
            read_fn(self.ptr, offset, data);
        }
    }
    pub fn write(&self, offset: u64, data: &[u8]) {
        unsafe {
            let write_fn: fn(*mut (), u64, &[u8]) = std::mem::transmute(self.write_ptr);
            write_fn(self.ptr, offset, data);
        }
    }
}

unsafe impl Send for BusDeviceVTable {}

/// A device container for routing reads and writes over some address space.
///
/// This doesn't have any restrictions on what kind of device or address space this applies to. The
/// only restriction is that no two devices can overlap in this address space.
#[derive(Debug, Clone, Default)]
pub struct Bus {
    pio_locations: Vec<(u64, u64)>,
    pio_devices: Vec<Arc<Mutex<BusDevice>>>,
    pio_devices_vtable: Vec<BusDeviceVTable>,
    mmio_devices: Vec<Arc<Mutex<BusDevice>>>,
    mmio_devices_vtable: Vec<BusDeviceVTable>,
}

use event_manager::{EventOps, Events, MutEventSubscriber};

use crate::arch::MMIO_MEM_START;
use crate::device_manager::mmio::MMIO_LEN;

#[cfg(target_arch = "aarch64")]
use super::legacy::RTCDevice;
use super::legacy::{I8042Device, SerialDevice};
use super::pseudo::BootTimer;
use super::virtio::mmio::MmioTransport;

#[derive(Debug)]
pub enum BusDevice {
    I8042Device(I8042Device),
    #[cfg(target_arch = "aarch64")]
    RTCDevice(RTCDevice),
    BootTimer(BootTimer),
    MmioTransport(MmioTransport),
    Serial(SerialDevice<std::io::Stdin>),
    #[cfg(test)]
    Dummy(DummyDevice),
    #[cfg(test)]
    Constant(ConstantDevice),
}

#[cfg(test)]
#[derive(Debug)]
pub struct DummyDevice;

#[cfg(test)]
impl DummyDevice {
    pub fn bus_write(&mut self, _offset: u64, _data: &[u8]) {}
    pub fn bus_read(&mut self, _offset: u64, _data: &[u8]) {}
}

#[cfg(test)]
#[derive(Debug)]
pub struct ConstantDevice;

#[cfg(test)]
impl ConstantDevice {
    pub fn bus_read(&mut self, offset: u64, data: &mut [u8]) {
        for (i, v) in data.iter_mut().enumerate() {
            *v = ((offset + i as u64) & 0xff) as u8;
        }
    }

    fn bus_write(&mut self, offset: u64, data: &[u8]) {
        for (i, v) in data.iter().enumerate() {
            assert_eq!(*v, ((offset + i as u64) & 0xff) as u8)
        }
    }
}

impl BusDevice {
    pub fn i8042_device_ref(&self) -> Option<&I8042Device> {
        match self {
            Self::I8042Device(x) => Some(x),
            _ => None,
        }
    }
    #[cfg(target_arch = "aarch64")]
    pub fn rtc_device_ref(&self) -> Option<&RTCDevice> {
        match self {
            Self::RTCDevice(x) => Some(x),
            _ => None,
        }
    }
    pub fn boot_timer_ref(&self) -> Option<&BootTimer> {
        match self {
            Self::BootTimer(x) => Some(x),
            _ => None,
        }
    }
    pub fn mmio_transport_ref(&self) -> Option<&MmioTransport> {
        match self {
            Self::MmioTransport(x) => Some(x),
            _ => None,
        }
    }
    pub fn serial_ref(&self) -> Option<&SerialDevice<std::io::Stdin>> {
        match self {
            Self::Serial(x) => Some(x),
            _ => None,
        }
    }

    pub fn i8042_device_mut(&mut self) -> Option<&mut I8042Device> {
        match self {
            Self::I8042Device(x) => Some(x),
            _ => None,
        }
    }
    #[cfg(target_arch = "aarch64")]
    pub fn rtc_device_mut(&mut self) -> Option<&mut RTCDevice> {
        match self {
            Self::RTCDevice(x) => Some(x),
            _ => None,
        }
    }
    pub fn boot_timer_mut(&mut self) -> Option<&mut BootTimer> {
        match self {
            Self::BootTimer(x) => Some(x),
            _ => None,
        }
    }
    pub fn mmio_transport_mut(&mut self) -> Option<&mut MmioTransport> {
        match self {
            Self::MmioTransport(x) => Some(x),
            _ => None,
        }
    }
    pub fn serial_mut(&mut self) -> Option<&mut SerialDevice<std::io::Stdin>> {
        match self {
            Self::Serial(x) => Some(x),
            _ => None,
        }
    }

    pub fn as_vtable(&self) -> BusDeviceVTable {
        unsafe {
            match self {
                Self::I8042Device(x) => BusDeviceVTable {
                    ptr: std::mem::transmute(x),
                    read_ptr: I8042Device::bus_read as usize,
                    write_ptr: I8042Device::bus_write as usize,
                },
                #[cfg(target_arch = "aarch64")]
                Self::RTCDevice(x) => BusDeviceVTable {
                    ptr: std::mem::transmute(x),
                    read_ptr: RTCDevice::bus_read as usize,
                    write_ptr: RTCDevice::bus_write as usize,
                },
                Self::BootTimer(x) => BusDeviceVTable {
                    ptr: std::mem::transmute(x),
                    read_ptr: BootTimer::bus_read as usize,
                    write_ptr: BootTimer::bus_write as usize,
                },
                Self::MmioTransport(x) => BusDeviceVTable {
                    ptr: std::mem::transmute(x),
                    read_ptr: MmioTransport::bus_read as usize,
                    write_ptr: MmioTransport::bus_write as usize,
                },
                Self::Serial(x) => BusDeviceVTable {
                    ptr: std::mem::transmute(x),
                    read_ptr: SerialDevice::<std::io::Stdin>::bus_read as usize,
                    write_ptr: SerialDevice::<std::io::Stdin>::bus_write as usize,
                },
                _ => unreachable!(),
            }
        }
    }

    pub fn read(&mut self, offset: u64, data: &mut [u8]) {
        match self {
            Self::I8042Device(x) => x.bus_read(offset, data),
            #[cfg(target_arch = "aarch64")]
            Self::RTCDevice(x) => x.bus_read(offset, data),
            Self::BootTimer(x) => x.bus_read(offset, data),
            Self::MmioTransport(x) => x.bus_read(offset, data),
            Self::Serial(x) => x.bus_read(offset, data),
            #[cfg(test)]
            Self::Dummy(x) => x.bus_read(offset, data),
            #[cfg(test)]
            Self::Constant(x) => x.bus_read(offset, data),
        }
    }

    pub fn write(&mut self, offset: u64, data: &[u8]) {
        match self {
            Self::I8042Device(x) => x.bus_write(offset, data),
            #[cfg(target_arch = "aarch64")]
            Self::RTCDevice(x) => x.bus_write(offset, data),
            Self::BootTimer(x) => x.bus_write(offset, data),
            Self::MmioTransport(x) => x.bus_write(offset, data),
            Self::Serial(x) => x.bus_write(offset, data),
            #[cfg(test)]
            Self::Dummy(x) => x.bus_write(offset, data),
            #[cfg(test)]
            Self::Constant(x) => x.bus_write(offset, data),
        }
    }
}

impl MutEventSubscriber for BusDevice {
    fn process(&mut self, event: Events, ops: &mut EventOps) {
        match self {
            Self::Serial(serial) => serial.process(event, ops),
            _ => panic!(),
        }
    }
    fn init(&mut self, ops: &mut EventOps) {
        match self {
            Self::Serial(serial) => serial.init(ops),
            _ => panic!(),
        }
    }
}

impl Bus {
    /// Constructs an a bus with an empty address space.
    pub fn new() -> Bus {
        Bus {
            pio_locations: vec![],
            pio_devices: vec![],
            pio_devices_vtable: vec![],
            mmio_devices: vec![],
            mmio_devices_vtable: vec![],
        }
    }

    pub fn len(&self) -> usize {
        // self.devices_not_mmio.len() + self.devices_opt.len() + self.devices_no_opt.len()
        self.mmio_devices.len()
    }

    /// Returns the device found at some address.
    pub fn get_device(&self, addr: u64) -> Option<(u64, &Mutex<BusDevice>)> {
        // None
        if addr < MMIO_MEM_START {
            for (i, (base, len)) in self.pio_locations.iter().enumerate() {
                if *base <= addr && addr <= *base + *len {
                    let offset = addr - base;
                    return Some((offset, &self.pio_devices[i]));
                }
            }
            None
        } else {
            let index = (addr - MMIO_MEM_START) / MMIO_LEN;
            let offset = (addr - MMIO_MEM_START) - MMIO_LEN * index;
            Some((offset, &self.mmio_devices[index as usize]))
        }
    }

    /// Puts the given device at the given address space.
    pub fn insert(
        &mut self,
        device: Arc<Mutex<BusDevice>>,
        base: u64,
        len: u64,
    ) -> Result<(), BusError> {
        log::info!(
            "adding device {} at base: {base}, len: {len}",
            self.mmio_devices.len()
        );
        if base < MMIO_MEM_START {
            self.pio_devices_vtable
                .push(device.lock().unwrap().as_vtable());
            self.pio_devices.push(device);
            self.pio_locations.push((base, len));
        } else {
            self.mmio_devices_vtable
                .push(device.lock().unwrap().as_vtable());
            self.mmio_devices.push(device);
        }
        Ok(())
    }

    /// Reads data from the device that owns the range containing `addr` and puts it into `data`.
    ///
    /// Returns true on success, otherwise `data` is untouched.
    pub fn read(&self, addr: u64, data: &mut [u8]) -> bool {
        if addr < MMIO_MEM_START {
            for (i, (base, len)) in self.pio_locations.iter().enumerate() {
                if *base <= addr && addr <= *base + *len {
                    let offset = addr - base;
                    let vtable = &self.pio_devices_vtable[i];
                    // log::info!(
                    //     "read from device: {} at addr: {} offset: {}",
                    //     i,
                    //     addr,
                    //     offset
                    // );
                    vtable.read(addr - base, data);
                    return true;
                }
            }
            log::error!("WFT: {addr}, {data:?}");
            false
        } else {
            let index = (addr - MMIO_MEM_START) / MMIO_LEN;
            let offset = (addr - MMIO_MEM_START) - MMIO_LEN * index;
            let vtable = &self.mmio_devices_vtable[index as usize];
            // log::info!(
            //     "read from device: {} at addr: {} offset: {}",
            //     index,
            //     addr,
            //     offset
            // );
            vtable.read(offset, data);
            true
        }
    }

    /// Writes `data` to the device that owns the range containing `addr`.
    ///
    /// Returns true on success, otherwise `data` is untouched.
    pub fn write(&self, addr: u64, data: &[u8]) -> bool {
        if addr < MMIO_MEM_START {
            for (i, (base, len)) in self.pio_locations.iter().enumerate() {
                if *base <= addr && addr <= *base + *len {
                    let offset = addr - base;
                    let vtable = &self.pio_devices_vtable[i];
                    // log::info!(
                    //     "write from device: {} at addr: {} offset: {}",
                    //     i,
                    //     addr,
                    //     offset
                    // );
                    vtable.write(offset, data);
                    return true;
                }
            }
            log::error!("WFT: {addr}, {data:?}");
            false
        } else {
            let index = (addr - MMIO_MEM_START) / MMIO_LEN;
            let offset = (addr - MMIO_MEM_START) - MMIO_LEN * index;
            let vtable = &self.mmio_devices_vtable[index as usize];
            // log::info!(
            //     "write from device: {} at addr: {} offset: {}",
            //     index,
            //     addr,
            //     offset
            // );
            vtable.write(offset, data);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_insert() {
        let mut bus = Bus::new();
        let dummy = Arc::new(Mutex::new(BusDevice::Dummy(DummyDevice)));
        // Insert len should not be 0.
        bus.insert(dummy.clone(), 0x10, 0).unwrap_err();
        bus.insert(dummy.clone(), 0x10, 0x10).unwrap();

        let result = bus.insert(dummy.clone(), 0x0f, 0x10);
        // This overlaps the address space of the existing bus device at 0x10.
        assert!(matches!(result, Err(BusError::Overlap)), "{:?}", result);

        // This overlaps the address space of the existing bus device at 0x10.
        bus.insert(dummy.clone(), 0x10, 0x10).unwrap_err();
        // This overlaps the address space of the existing bus device at 0x10.
        bus.insert(dummy.clone(), 0x10, 0x15).unwrap_err();
        // This overlaps the address space of the existing bus device at 0x10.
        bus.insert(dummy.clone(), 0x12, 0x15).unwrap_err();
        // This overlaps the address space of the existing bus device at 0x10.
        bus.insert(dummy.clone(), 0x12, 0x01).unwrap_err();
        // This overlaps the address space of the existing bus device at 0x10.
        bus.insert(dummy.clone(), 0x0, 0x20).unwrap_err();
        bus.insert(dummy.clone(), 0x20, 0x05).unwrap();
        bus.insert(dummy.clone(), 0x25, 0x05).unwrap();
        bus.insert(dummy, 0x0, 0x10).unwrap();
    }

    #[test]
    fn bus_read_write() {
        let mut bus = Bus::new();
        let dummy = Arc::new(Mutex::new(BusDevice::Dummy(DummyDevice)));
        bus.insert(dummy, 0x10, 0x10).unwrap();
        assert!(bus.read(0x10, &mut [0, 0, 0, 0]));
        assert!(bus.write(0x10, &[0, 0, 0, 0]));
        assert!(bus.read(0x11, &mut [0, 0, 0, 0]));
        assert!(bus.write(0x11, &[0, 0, 0, 0]));
        assert!(bus.read(0x16, &mut [0, 0, 0, 0]));
        assert!(bus.write(0x16, &[0, 0, 0, 0]));
        assert!(!bus.read(0x20, &mut [0, 0, 0, 0]));
        assert!(!bus.write(0x20, &[0, 0, 0, 0]));
        assert!(!bus.read(0x06, &mut [0, 0, 0, 0]));
        assert!(!bus.write(0x06, &[0, 0, 0, 0]));
    }

    #[test]
    fn bus_read_write_values() {
        let mut bus = Bus::new();
        let dummy = Arc::new(Mutex::new(BusDevice::Constant(ConstantDevice)));
        bus.insert(dummy, 0x10, 0x10).unwrap();

        let mut values = [0, 1, 2, 3];
        assert!(bus.read(0x10, &mut values));
        assert_eq!(values, [0, 1, 2, 3]);
        assert!(bus.write(0x10, &values));
        assert!(bus.read(0x15, &mut values));
        assert_eq!(values, [5, 6, 7, 8]);
        assert!(bus.write(0x15, &values));
    }

    #[test]
    fn busrange_cmp_and_clone() {
        assert_eq!(BusRange(0x10, 2), BusRange(0x10, 3));
        assert_eq!(BusRange(0x10, 2), BusRange(0x10, 2));

        assert!(BusRange(0x10, 2) < BusRange(0x12, 1));
        assert!(BusRange(0x10, 2) < BusRange(0x12, 3));

        let mut bus = Bus::new();
        let mut data = [1, 2, 3, 4];
        bus.insert(
            Arc::new(Mutex::new(BusDevice::Dummy(DummyDevice))),
            0x10,
            0x10,
        )
        .unwrap();
        assert!(bus.write(0x10, &data));
        let bus_clone = bus.clone();
        assert!(bus.read(0x10, &mut data));
        assert_eq!(data, [1, 2, 3, 4]);
        assert!(bus_clone.read(0x10, &mut data));
        assert_eq!(data, [1, 2, 3, 4]);
    }

    #[test]
    fn test_display_error() {
        assert_eq!(
            format!("{}", BusError::Overlap),
            "New device overlaps with an old device."
        );
    }
}
