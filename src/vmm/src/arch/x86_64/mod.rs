// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

/// Logic for handling x86_64 CPU models.
pub mod cpu_model;
mod gdt;
/// Contains logic for setting up Advanced Programmable Interrupt Controller (local version).
pub mod interrupts;
/// Layout for the x86_64 system.
pub mod layout;
mod mptable;
/// Logic for configuring x86_64 model specific registers (MSRs).
pub mod msr;
/// Logic for configuring x86_64 registers.
pub mod regs;

#[allow(missing_docs)]
pub mod gen;

use linux_loader::configurator::linux::LinuxBootConfigurator;
use linux_loader::configurator::{BootConfigurator, BootParams};
use linux_loader::loader::bootparam::boot_params;

use crate::arch::InitrdConfig;
use crate::device_manager::resources::ResourceAllocator;
use crate::utils::u64_to_usize;
use crate::vstate::memory::{
    Address, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion,
};

// Value taken from https://elixir.bootlin.com/linux/v5.10.68/source/arch/x86/include/uapi/asm/e820.h#L31
// Usable normal RAM
const E820_RAM: u32 = 1;
// Reserved area that should be avoided during memory allocations
const E820_RESERVED: u32 = 2;

/// Errors thrown while configuring x86_64 system.
#[derive(Debug, PartialEq, Eq, thiserror::Error, displaydoc::Display)]
pub enum ConfigurationError {
    /// Invalid e820 setup params.
    E820Configuration,
    /// Error writing MP table to memory: {0}
    MpTableSetup(#[from] mptable::MptableError),
    /// Error writing the zero page of guest memory.
    ZeroPageSetup,
    /// Failed to compute initrd address.
    InitrdAddress,
}

const FIRST_ADDR_PAST_32BITS: u64 = 1 << 32;

/// Size of MMIO gap at top of 32-bit address space.
pub const MEM_32BIT_GAP_SIZE: u64 = 768 << 20;
/// The start of the memory area reserved for MMIO devices.
pub const MMIO_MEM_START: u64 = FIRST_ADDR_PAST_32BITS - MEM_32BIT_GAP_SIZE;
/// The size of the memory area reserved for MMIO devices.
pub const MMIO_MEM_SIZE: u64 = MEM_32BIT_GAP_SIZE;

/// Returns a Vec of the valid memory addresses.
/// These should be used to configure the GuestMemoryMmap structure for the platform.
/// For x86_64 all addresses are valid from the start of the kernel except a
/// carve out at the end of 32bit address space.
pub fn arch_memory_regions(size: usize) -> Vec<(GuestAddress, usize)> {
    // It's safe to cast MMIO_MEM_START to usize because it fits in a u32 variable
    // (It points to an address in the 32 bit space).
    match size.checked_sub(usize::try_from(MMIO_MEM_START).unwrap()) {
        // case1: guest memory fits before the gap
        None | Some(0) => vec![(GuestAddress(0), size)],
        // case2: guest memory extends beyond the gap
        Some(remaining) => vec![
            (GuestAddress(0), usize::try_from(MMIO_MEM_START).unwrap()),
            (GuestAddress(FIRST_ADDR_PAST_32BITS), remaining),
        ],
    }
}

/// Returns the memory address where the kernel could be loaded.
pub fn get_kernel_start() -> u64 {
    layout::HIMEM_START
}

/// Returns the memory address where the initrd could be loaded.
pub fn initrd_load_addr(
    guest_mem: &GuestMemoryMmap,
    initrd_size: usize,
) -> Result<u64, ConfigurationError> {
    let first_region = guest_mem
        .find_region(GuestAddress::new(0))
        .ok_or(ConfigurationError::InitrdAddress)?;
    let lowmem_size = u64_to_usize(first_region.len());

    if lowmem_size < initrd_size {
        return Err(ConfigurationError::InitrdAddress);
    }

    let align_to_pagesize = |address| address & !(super::GUEST_PAGE_SIZE - 1);
    Ok(align_to_pagesize(lowmem_size - initrd_size) as u64)
}

/// Configures the system and should be called once per vm before starting vcpu threads.
///
/// # Arguments
///
/// * `guest_mem` - The memory to be used by the guest.
/// * `cmdline_addr` - Address in `guest_mem` where the kernel command line was loaded.
/// * `cmdline_size` - Size of the kernel command line in bytes including the null terminator.
/// * `initrd` - Information about where the ramdisk image was loaded in the `guest_mem`.
/// * `num_cpus` - Number of virtual CPUs the guest will have.
pub fn configure_system(
    guest_mem: &GuestMemoryMmap,
    resource_allocator: &mut ResourceAllocator,
    cmdline_addr: GuestAddress,
    cmdline_size: usize,
    initrd: &Option<InitrdConfig>,
    num_cpus: u8,
) -> Result<(), ConfigurationError> {
    const KERNEL_BOOT_FLAG_MAGIC: u16 = 0xaa55;
    const KERNEL_HDR_MAGIC: u32 = 0x5372_6448;
    const KERNEL_LOADER_OTHER: u8 = 0xff;
    const KERNEL_MIN_ALIGNMENT_BYTES: u32 = 0x0100_0000; // Must be non-zero.
    let first_addr_past_32bits = GuestAddress(FIRST_ADDR_PAST_32BITS);
    let end_32bit_gap_start = GuestAddress(MMIO_MEM_START);

    let himem_start = GuestAddress(layout::HIMEM_START);

    // Note that this puts the mptable at the last 1k of Linux's 640k base RAM
    mptable::setup_mptable(guest_mem, resource_allocator, num_cpus)?;

    // Set the location of RSDP in Boot Parameters to help the guest kernel find it faster.
    let mut params = boot_params {
        acpi_rsdp_addr: layout::RSDP_ADDR,
        ..Default::default()
    };

    params.hdr.type_of_loader = KERNEL_LOADER_OTHER;
    params.hdr.boot_flag = KERNEL_BOOT_FLAG_MAGIC;
    params.hdr.header = KERNEL_HDR_MAGIC;
    params.hdr.cmd_line_ptr = u32::try_from(cmdline_addr.raw_value()).unwrap();
    params.hdr.cmdline_size = u32::try_from(cmdline_size).unwrap();
    params.hdr.kernel_alignment = KERNEL_MIN_ALIGNMENT_BYTES;
    if let Some(initrd_config) = initrd {
        params.hdr.ramdisk_image = u32::try_from(initrd_config.address.raw_value()).unwrap();
        params.hdr.ramdisk_size = u32::try_from(initrd_config.size).unwrap();
    }

    // We mark first [0x0, SYSTEM_MEM_START) region as usable RAM and the subsequent
    // [SYSTEM_MEM_START, (SYSTEM_MEM_START + SYSTEM_MEM_SIZE)) as reserved (note
    // SYSTEM_MEM_SIZE + SYSTEM_MEM_SIZE == HIMEM_START).
    add_e820_entry(&mut params, 0, layout::SYSTEM_MEM_START, E820_RAM)?;
    add_e820_entry(
        &mut params,
        layout::SYSTEM_MEM_START,
        layout::SYSTEM_MEM_SIZE,
        E820_RESERVED,
    )?;

    let last_addr = guest_mem.last_addr();
    if last_addr < end_32bit_gap_start {
        add_e820_entry(
            &mut params,
            himem_start.raw_value(),
            // it's safe to use unchecked_offset_from because
            // mem_end > himem_start
            last_addr.unchecked_offset_from(himem_start) + 1,
            E820_RAM,
        )?;
    } else {
        add_e820_entry(
            &mut params,
            himem_start.raw_value(),
            // it's safe to use unchecked_offset_from because
            // end_32bit_gap_start > himem_start
            end_32bit_gap_start.unchecked_offset_from(himem_start),
            E820_RAM,
        )?;

        if last_addr > first_addr_past_32bits {
            add_e820_entry(
                &mut params,
                first_addr_past_32bits.raw_value(),
                // it's safe to use unchecked_offset_from because
                // mem_end > first_addr_past_32bits
                last_addr.unchecked_offset_from(first_addr_past_32bits) + 1,
                E820_RAM,
            )?;
        }
    }

    LinuxBootConfigurator::write_bootparams(
        &BootParams::new(&params, GuestAddress(layout::ZERO_PAGE_START)),
        guest_mem,
    )
    .map_err(|_| ConfigurationError::ZeroPageSetup)
}

/// Add an e820 region to the e820 map.
/// Returns Ok(()) if successful, or an error if there is no space left in the map.
fn add_e820_entry(
    params: &mut boot_params,
    addr: u64,
    size: u64,
    mem_type: u32,
) -> Result<(), ConfigurationError> {
    if params.e820_entries as usize >= params.e820_table.len() {
        return Err(ConfigurationError::E820Configuration);
    }

    params.e820_table[params.e820_entries as usize].addr = addr;
    params.e820_table[params.e820_entries as usize].size = size;
    params.e820_table[params.e820_entries as usize].type_ = mem_type;
    params.e820_entries += 1;

    Ok(())
}

#[cfg(test)]
mod tests {
    use linux_loader::loader::bootparam::boot_e820_entry;

    use super::*;
    use crate::test_utils::{arch_mem, single_region_mem};

    #[test]
    fn regions_lt_4gb() {
        let regions = arch_memory_regions(1usize << 29);
        assert_eq!(1, regions.len());
        assert_eq!(GuestAddress(0), regions[0].0);
        assert_eq!(1usize << 29, regions[0].1);
    }

    #[test]
    fn regions_gt_4gb() {
        let regions = arch_memory_regions((1usize << 32) + 0x8000);
        assert_eq!(2, regions.len());
        assert_eq!(GuestAddress(0), regions[0].0);
        assert_eq!(GuestAddress(1u64 << 32), regions[1].0);
    }

    #[test]
    fn test_system_configuration() {
        let no_vcpus = 4;
        let gm = single_region_mem(0x10000);
        let mut resource_allocator = ResourceAllocator::new().unwrap();
        let config_err =
            configure_system(&gm, &mut resource_allocator, GuestAddress(0), 0, &None, 1);
        assert_eq!(
            config_err.unwrap_err(),
            super::ConfigurationError::MpTableSetup(mptable::MptableError::NotEnoughMemory)
        );

        // Now assigning some memory that falls before the 32bit memory hole.
        let mem_size = 128 << 20;
        let gm = arch_mem(mem_size);
        let mut resource_allocator = ResourceAllocator::new().unwrap();
        configure_system(
            &gm,
            &mut resource_allocator,
            GuestAddress(0),
            0,
            &None,
            no_vcpus,
        )
        .unwrap();

        // Now assigning some memory that is equal to the start of the 32bit memory hole.
        let mem_size = 3328 << 20;
        let gm = arch_mem(mem_size);
        let mut resource_allocator = ResourceAllocator::new().unwrap();
        configure_system(
            &gm,
            &mut resource_allocator,
            GuestAddress(0),
            0,
            &None,
            no_vcpus,
        )
        .unwrap();

        // Now assigning some memory that falls after the 32bit memory hole.
        let mem_size = 3330 << 20;
        let gm = arch_mem(mem_size);
        let mut resource_allocator = ResourceAllocator::new().unwrap();
        configure_system(
            &gm,
            &mut resource_allocator,
            GuestAddress(0),
            0,
            &None,
            no_vcpus,
        )
        .unwrap();
    }

    #[test]
    fn test_add_e820_entry() {
        let e820_map = [(boot_e820_entry {
            addr: 0x1,
            size: 4,
            type_: 1,
        }); 128];

        let expected_params = boot_params {
            e820_table: e820_map,
            e820_entries: 1,
            ..Default::default()
        };

        let mut params: boot_params = Default::default();
        add_e820_entry(
            &mut params,
            e820_map[0].addr,
            e820_map[0].size,
            e820_map[0].type_,
        )
        .unwrap();
        assert_eq!(
            format!("{:?}", params.e820_table[0]),
            format!("{:?}", expected_params.e820_table[0])
        );
        assert_eq!(params.e820_entries, expected_params.e820_entries);

        // Exercise the scenario where the field storing the length of the e820 entry table is
        // is bigger than the allocated memory.
        params.e820_entries = u8::try_from(params.e820_table.len()).unwrap() + 1;
        assert!(add_e820_entry(
            &mut params,
            e820_map[0].addr,
            e820_map[0].size,
            e820_map[0].type_
        )
        .is_err());
    }
}
