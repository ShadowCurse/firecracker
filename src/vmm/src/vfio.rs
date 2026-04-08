use std::ops::DerefMut;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::{Arc, Barrier, Mutex};

use arrayvec::ArrayVec;
use kvm_bindings::{
    kvm_create_device, kvm_device_type_KVM_DEV_TYPE_VFIO, kvm_userspace_memory_region,
};
use vfio_bindings::bindings::vfio::*;
pub use vfio_ioctls::{
    VfioContainer, VfioDevice, VfioDeviceFd, VfioRegionInfoCap, VfioRegionInfoCapSparseMmap,
    VfioRegionSparseMmapArea,
};
use vm_allocator::AllocPolicy;
use vm_memory::{GuestMemory, GuestMemoryRegion};
use vmm_sys_util::eventfd::EventFd;
use zerocopy::IntoBytes;

use crate::Vm;
use crate::arch::host_page_size;
use crate::logger::{debug, error, warn};
use crate::pci::configuration::{BAR0_REG_IDX, Bars, NUM_BAR_REGS};
use crate::pci::msix::{MsixCap, MsixConfig};
use crate::pci::{PciCapabilityId, PciDevice, PciExpressCapabilityId, PciSBDF};
use crate::utils::{
    align_down_host_page, align_up_host_page, offset_from_lower_host_page, u64_to_usize,
    usize_to_u64,
};
use crate::vmm_config::vfio::VfioConfig;
use crate::vstate::bus::BusDevice;
use crate::vstate::interrupts::InterruptError;
use crate::vstate::memory::{GuestMemoryMmap, GuestRegionType};
use crate::vstate::resources::ResourceAllocator;

// First BAR offset in the PCI config space.
const PCI_CONFIG_BAR_OFFSET: u32 = 0x10;
// Capability register offset in the PCI config space.
const PCI_CONFIG_CAPABILITY_OFFSET: u32 = 0x34;
// Extended capabilities register offset in the PCI config space.
const PCI_CONFIG_EXTENDED_CAPABILITY_OFFSET: u32 = 0x100;
// IO BAR when first BAR bit is 1.
const PCI_CONFIG_IO_BAR: u32 = 1 << 0;
// 64-bit memory bar flag.
const PCI_CONFIG_MEMORY_BAR_64BIT: u32 = 1 << 2;
// Prefetchable BAR bit
const PCI_CONFIG_BAR_PREFETCHABLE: u32 = 1 << 3;

/// VfioError
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VfioError {
    /// Failed to allocate guest address for BAR
    BarAllocation,
    /// mmap failed
    Mmap,
    /// Failed to allocate KVM slot
    KvmSlot,
    /// Failed to set KVM user memory region: {0}
    SetUserMemoryRegion(String),
    /// Cannot create Msix vector group: {0}
    MsixConfig(#[from] InterruptError),
    /// Device does not provide MSIx irq
    NoMsixIrq,
    /// KVM failed to create KVM_DEV_TYPE_VFIO device: {0}
    KVMCreateVfioDevice(kvm_ioctls::Error),
    /// Partial DMA unmap: requested {0:#x}, got {1:#x}
    PartialDmaUnmap(u64, u64),
    /// vfio-ioctls crate error: {0}
    VfioIoctls(#[from] vfio_ioctls::VfioError),
}

#[derive(Debug, Clone)]
struct VfioRegionInfo {
    pub flags: u32,
    pub size: u64,
    pub offset: u64,
    pub caps: Vec<VfioRegionInfoCap>,
}

/// Mask for specific register in the configuration space
#[derive(Debug)]
pub struct RegisterMask {
    /// register
    pub register: u16,
    /// applied as (R & mask) | value
    pub mask: u32,
    /// value
    pub value: u32,
}

/// Type of the hole in the bar
#[derive(Debug, Copy, Clone)]
pub enum BarHoleInfoUsage {
    /// The hole contains MSIx table
    Table,
    /// The hole contains MSIx pba
    Pba,
}

/// Information about the location of the hole in the bar
#[derive(Debug, Copy, Clone)]
pub struct BarHoleInfo {
    /// Guest location of the hole
    pub gpa: u64,
    /// Size of the hole
    pub size: u64,
    /// What does the hole contain
    pub usage: BarHoleInfoUsage,
}

/// Information about the bar mapping
#[derive(Debug, Copy, Clone)]
pub struct BarMapping {
    /// KVM slot assigned to the mapping
    pub slot: u32,
    /// Guest physical address
    pub iova: u64,
    /// Size
    pub size: u64,
    /// Host virtual address
    pub host_addr: u64,
}

/// Container for everything MSIx related
#[derive(Debug)]
pub struct MsixState {
    /// Register idx where the capability is in the configuration space
    pub register: u8,
    /// The actual capability (without first 2 bytes)
    pub cap: MsixCap,
    /// Info about Table and Pba holes
    pub bar_hole_infos: ArrayVec<BarHoleInfo, 2>,
    /// Config
    pub config: MsixConfig,
}

/// The VFIO device bundle
pub struct VfioDeviceBundle {
    /// Configuration with wich the device was created
    pub config: VfioConfig,
    /// SBDF of the device in the configuration space
    pub sbdf: PciSBDF,
    /// devices
    pub device: VfioDevice,
    /// container
    pub container: Arc<VfioContainer>,
    /// Information about BARs
    pub bars: Bars,
    /// There are 6 bars, but one of them can be split in 3 by MSI-X table/pba
    pub bar_mappings: ArrayVec<BarMapping, 8>,
    /// MSIx state
    pub msix_state: Option<MsixState>,
    /// Masks for configuration space registers
    pub masks: Vec<RegisterMask>,
    /// Vm
    pub vm: Arc<Vm>,
}

impl std::fmt::Debug for VfioDeviceBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VfioDeviceBundle")
            .field("config", &self.config)
            .field("sbdf", &self.sbdf)
            .finish()
    }
}

macro_rules! handle_bar_access {
    ($state:expr, $device:expr, $base:expr, $offset:expr, $data:expr,
     $table_fn:ident, $pba_fn:ident, $region_method:ident) => {{
        let mut name = "----";
        let mut handled = false;
        let data_start = $offset;
        let data_end = $offset + $data.len() as u64;
        for hole in $state.bar_hole_infos.iter() {
            if hole.gpa == $base {
                match hole.usage {
                    BarHoleInfoUsage::Table => {
                        let (table_offset, table_size) = $state.cap.table_range();
                        let table_start = offset_from_lower_host_page(table_offset);
                        let table_end = table_start + table_size;
                        if table_start <= data_start && data_end <= table_end {
                            name = "MsiTable";
                            $state.config.$table_fn($offset, $data);
                        } else {
                            name = "OutsideTable";
                            let region_index = $state.cap.table_bir();
                            $device.$region_method(region_index as u32, $data, $offset);
                        }
                    }
                    BarHoleInfoUsage::Pba => {
                        let (table_offset, table_size) = $state.cap.pba_range();
                        let table_start = offset_from_lower_host_page(table_offset);
                        let table_end = table_start + table_size;
                        if table_start <= data_start && data_end <= table_end {
                            name = "PbaTable";
                            $state.config.$pba_fn($offset, $data);
                        } else {
                            name = "OutsideTable";
                            let region_index = $state.cap.pba_bir();
                            $device.$region_method(region_index as u32, $data, $offset);
                        }
                    }
                }
                handled = true;
            }
        }
        (name, handled)
    }};
}

// This should only serve BARs
impl BusDevice for VfioDeviceBundle {
    fn read(&mut self, base: u64, offset: u64, data: &mut [u8]) {
        let Some(state) = self.msix_state.as_ref() else {
            warn!("BusDevice::read called on VFIO device without MSI-X state");
            data.fill(0);
            return;
        };
        let (name, handled) = handle_bar_access!(
            state,
            self.device,
            base,
            offset,
            data,
            read_table,
            read_pba,
            region_read
        );
        if !handled {
            data.fill(0);
        }
        debug!(
            "[{}] base: {base:<#10x} offset: {offset:<#5x} data: {data:<4?} name: {name} handled: \
             {handled}",
            self.config.id,
        );
    }

    fn write(&mut self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        let Some(state) = self.msix_state.as_mut() else {
            warn!("BusDevice::write called on VFIO device without MSI-X state");
            return None;
        };
        let (name, handled) = handle_bar_access!(
            state,
            self.device,
            base,
            offset,
            data,
            write_table,
            write_pba,
            region_write
        );
        if !handled {
            warn!(
                "[{}] BusDevice::write not handled: base: {base:#x} offset: {offset:#x}",
                self.config.id
            );
        }
        debug!(
            "[{}] base: {base:<#10x} offset: {offset:<#5x} data: {data:<4?} table_name: {name}, \
             handled: {handled}",
            self.config.id
        );
        None
    }
}

// This should only serve config space
impl PciDevice for VfioDeviceBundle {
    fn write_config_register(
        &mut self,
        reg_idx: u16,
        offset: u8,
        data: &[u8],
    ) -> Option<Arc<Barrier>> {
        let mut name = "----";
        let mut handled: bool = false;

        if BAR0_REG_IDX <= reg_idx && reg_idx < BAR0_REG_IDX + u16::from(NUM_BAR_REGS) {
            // reg_idx is in [BAR0_REG, BAR0_REG+NUM_BAR_REGS), so the difference is 0..5.
            #[allow(clippy::cast_possible_truncation)]
            let bar_idx = (reg_idx - BAR0_REG_IDX) as u8;
            // offset is within a 4-byte PCI config register (0..3).
            #[allow(clippy::cast_possible_truncation)]
            let offset = offset as u8;
            self.bars.write(bar_idx, offset, data);
            name = "BAR";
            handled = true;
        } else if let Some(state) = self.msix_state.as_mut() {
            if reg_idx == u16::from(state.register) {
                // offset is within a 4-byte PCI config register (0..3).
                #[allow(clippy::cast_possible_truncation)]
                let offset = offset as u8;
                state.config.write_msg_ctl_register(offset, data);
                name = "MSIX_CAP";
                // Don't set `handled` since we need to passthrough write
                // to the msg_ctl register to the device, so it will enable Msix
                // interrupts
            }
        }
        let config_offset = reg_idx * 4 + u16::from(offset);
        if !handled {
            self.device
                .region_write(VFIO_PCI_CONFIG_REGION_INDEX, data, u64::from(config_offset));
        }
        debug!(
            "[{}] reg: {reg_idx:>3}({config_offset:>#6x}) data: {data:<4?} name: {name}",
            self.config.id
        );
        None
    }
    fn read_config_register(&mut self, reg_idx: u16) -> u32 {
        let mut name = "----";
        let config_offset = reg_idx as u64 * 4;
        let mut result: u32 = 0;
        if BAR0_REG_IDX <= reg_idx && reg_idx < BAR0_REG_IDX + u16::from(NUM_BAR_REGS) {
            // reg_idx is in [BAR0_REG, BAR0_REG+NUM_BAR_REGS), so the difference is 0..5.
            #[allow(clippy::cast_possible_truncation)]
            let bar_idx = (reg_idx - BAR0_REG_IDX) as u8;
            self.bars.read(bar_idx, 0, result.as_mut_bytes());
            name = "BAR";
        } else {
            self.device.region_read(
                VFIO_PCI_CONFIG_REGION_INDEX,
                result.as_mut_bytes(),
                config_offset,
            );
            if let Some(state) = self.msix_state.as_ref() {
                if reg_idx == u16::from(state.register) {
                    result = (result & !(1 << 31 | 1 << 30))
                        | ((state.config.enabled as u32) << 31)
                        | ((state.config.masked as u32) << 30);
                    name = "MSIX_CAP";
                }
            }
            for mask in self.masks.iter() {
                if mask.register == reg_idx as u16 {
                    result = (result & mask.mask) | mask.value;
                    name = "MASK";
                    break;
                }
            }
        }
        debug!(
            "[{}] reg: {reg_idx:>3}({config_offset:>#6x}) data: {:<4?} name: {name}",
            self.config.id,
            result.as_bytes()
        );
        result
    }
}

fn vfio_device_get_pci_capabilities(
    device: &VfioDevice,
) -> Result<(Option<(MsixCap, u8)>, Vec<RegisterMask>), VfioError> {
    let mut next_cap_offset: u8 = 0;
    device.region_read(
        VFIO_PCI_CONFIG_REGION_INDEX,
        next_cap_offset.as_mut_bytes(),
        PCI_CONFIG_CAPABILITY_OFFSET as u64,
    );

    let mut has_pci_express_cap = false;

    let mut msix_cap_and_register = None;
    debug!("PCI CAPS offset: {}", next_cap_offset);
    // The legacy region with PCI capis is 256 bytes long and
    // split into 4 byte registers.
    const LOOP_UPPER_BOUND: u32 = 256 / 4;
    let mut loop_bound: u32 = 0;
    while next_cap_offset != 0 && loop_bound < LOOP_UPPER_BOUND {
        loop_bound += 1;

        let mut cap_id_and_next_ptr: u16 = 0;
        device.region_read(
            VFIO_PCI_CONFIG_REGION_INDEX,
            cap_id_and_next_ptr.as_mut_bytes(),
            next_cap_offset as u64,
        );

        let current_cap_offset = next_cap_offset;

        // 7.5.3.1 PCI Express Capability List Register
        // |      2 bytes    |     1 byte    |          1 byte         |
        // |   Cap register  | Capability ID | Next Capability Pointer |
        let cap_id: u8 = (cap_id_and_next_ptr & 0xff) as u8;
        next_cap_offset = ((cap_id_and_next_ptr & 0xff00) >> 8) as u8;
        debug!("PCI CAP id: {cap_id} next offset: {next_cap_offset:#x}");

        match PciCapabilityId::from(cap_id) {
            PciCapabilityId::MessageSignalledInterrupts => {
                if let Some(irq_info) = device.get_irq_info(VFIO_PCI_MSI_IRQ_INDEX) {
                    if irq_info.count != 0 {
                        let register = current_cap_offset / 4;
                        debug!("Found MSI cap at offset: {current_cap_offset:#x}({register})");
                    } else {
                        debug!("Found MSI cap, but the device does not support MSI interrupts.");
                    }
                }
            }
            PciCapabilityId::MsiX => {
                if let Some(irq_info) = device.get_irq_info(VFIO_PCI_MSIX_IRQ_INDEX) {
                    if irq_info.count != 0 {
                        let register = current_cap_offset / 4;
                        debug!("Found MSIX cap at offset: {current_cap_offset:#x}({register})");

                        // 7.7.2 MSI-X Capability and Table Structure
                        let mut msg_ctl: u16 = 0;
                        let mut table: u32 = 0;
                        let mut pba: u32 = 0;
                        device.region_read(
                            VFIO_PCI_CONFIG_REGION_INDEX,
                            msg_ctl.as_mut_bytes(),
                            (current_cap_offset as u64) + 2,
                        );
                        device.region_read(
                            VFIO_PCI_CONFIG_REGION_INDEX,
                            table.as_mut_bytes(),
                            (current_cap_offset as u64) + 4,
                        );
                        device.region_read(
                            VFIO_PCI_CONFIG_REGION_INDEX,
                            pba.as_mut_bytes(),
                            (current_cap_offset as u64) + 8,
                        );
                        msix_cap_and_register = Some((
                            MsixCap {
                                msg_ctl,
                                table,
                                pba,
                            },
                            register,
                        ));
                    } else {
                        debug!(
                            "Found MSI-X cap, but the device does not support MSI-X interrupts."
                        );
                    }
                }
            }
            PciCapabilityId::PciExpress => {
                let register = current_cap_offset / 4;
                debug!("Found PciExpress cap at offset: {current_cap_offset:#x}({register})");

                has_pci_express_cap = true;
            }
            // 7.5.2 PCI Power Management Capability Structure
            // This structure is required for all PCI Express Functions.
            // But I think just PciExpress shoudl be enough?
            _ => {}
        };
    }

    let mut masks = Vec::new();
    if has_pci_express_cap {
        let mut next_cap_offset: u16 = PCI_CONFIG_EXTENDED_CAPABILITY_OFFSET as u16;

        // The PCIe region is 4K in size and split into 4 byte registers
        const LOOP_UPPER_BOUND: u32 = 4096 / 4;
        let mut loop_bound: u32 = 0;
        while next_cap_offset != 0 && loop_bound < LOOP_UPPER_BOUND {
            loop_bound += 1;

            let mut cap_id_and_next_ptr: u32 = 0;
            device.region_read(
                VFIO_PCI_CONFIG_REGION_INDEX,
                cap_id_and_next_ptr.as_mut_bytes(),
                next_cap_offset as u64,
            );
            let current_cap_offset = next_cap_offset;

            // 7.7.3.1 Secondary PCI Express Extended Capability Header
            // |           31-20        |         19-16       |          15-0         |
            // | Next capability offset | Capability Version  |   PCIe Capability ID  |
            let cap_id: u16 = (cap_id_and_next_ptr & 0xffff) as u16;
            next_cap_offset = (cap_id_and_next_ptr >> 20) as u16;

            let pci_cap = PciExpressCapabilityId::from(cap_id);
            let register = current_cap_offset / 4;
            debug!("Found {pci_cap:?} cap at offset: {current_cap_offset:#x}({register})");

            // NOTE: the list of capabilities is hardcoded for now. In the future this
            // may be configurable from the user side.
            match pci_cap {
                PciExpressCapabilityId::AlternativeRoutingIdentificationInterpretation
                | PciExpressCapabilityId::ResizeableBar
                | PciExpressCapabilityId::SingleRootIoVirtualization => {
                    debug!(
                        "Found cap to be masked at register: {register}({current_cap_offset:#x})"
                    );
                    masks.push(RegisterMask {
                        register,
                        mask: 0xffff_0000,
                        value: 0x0000_0000,
                    })
                }
                _ => {}
            }
        }
    }
    Ok((msix_cap_and_register, masks))
}

fn vfio_device_get_single_bar_info(
    device: &VfioDevice,
    bar_idx: u8,
) -> Result<(u32, u32), VfioError> {
    // 7.5.1.2.1 Base Address Registers
    // IMPLEMENTATION NOTE: SIZING A 32-BIT BASE ADDRESS REGISTER
    let bar_offset = u64::from(PCI_CONFIG_BAR_OFFSET) + u64::from(bar_idx) * 4;
    let mut value: u32 = 0;
    let mut size: u32 = 0;
    device.region_read(
        VFIO_PCI_CONFIG_REGION_INDEX,
        value.as_mut_bytes(),
        bar_offset,
    );
    device.region_write(
        VFIO_PCI_CONFIG_REGION_INDEX,
        0xFFFF_FFFF_u32.as_bytes(),
        bar_offset,
    );
    device.region_read(
        VFIO_PCI_CONFIG_REGION_INDEX,
        size.as_mut_bytes(),
        bar_offset,
    );
    device.region_write(VFIO_PCI_CONFIG_REGION_INDEX, value.as_bytes(), bar_offset);
    Ok((value, size))
}

fn vfio_device_get_bars(
    device: &VfioDevice,
    resource_allocator: &mut ResourceAllocator,
) -> Result<Bars, VfioError> {
    let mut bars = Bars::default();
    let mut bar_idx = 0;
    while bar_idx < NUM_BAR_REGS {
        let (bar_info, mut lower_size) = vfio_device_get_single_bar_info(device, bar_idx)?;

        let is_io_bar = bar_info & PCI_CONFIG_IO_BAR != 0;
        let is_64_bits = bar_info & PCI_CONFIG_MEMORY_BAR_64BIT != 0;
        let is_prefetchable = bar_info & PCI_CONFIG_BAR_PREFETCHABLE != 0;

        let mut size = 0;
        if is_io_bar {
            if lower_size != 0 {
                lower_size &= !0b11;
                lower_size = !lower_size + 1;
                size = u64::from(lower_size);
            }
        } else if !is_64_bits {
            if lower_size != 0 {
                lower_size &= !0b1111;
                lower_size = !lower_size + 1;
                size = u64::from(lower_size);
            }
        } else {
            let (_, upper_size) = vfio_device_get_single_bar_info(device, bar_idx + 1)?;

            size = u64::from(upper_size) << 32 | u64::from(lower_size);
            size &= !0b1111;
            size = !size + 1;
        }
        if size != 0 {
            fn calculate_alignment(size: u64) -> u64 {
                // 7.5.1.2.1 Base Address Registers
                // This design implies that all address spaces used are a power of two
                // in size and are naturally aligned.
                let alignment = std::cmp::max(host_page_size(), 1 << size.trailing_zeros());
                usize_to_u64(alignment)
            }

            let idx = bar_idx;
            let gpa;
            if is_io_bar {
                debug!(
                    "BAR{bar_idx} size: {size:>#10x} io_bar: {is_io_bar} 64bits: {is_64_bits} \
                     prefetchable: {is_prefetchable} Skipping"
                );
                // TODO: Handle IO BARs somehow
                bar_idx += 1;
                continue;
            } else if is_64_bits {
                let alignment = calculate_alignment(size);
                gpa = resource_allocator
                    .mmio64_memory
                    .allocate(size, alignment, AllocPolicy::FirstMatch)
                    .map_err(|_| VfioError::BarAllocation)?
                    .start();
                bar_idx += 1;
                bars.set_bar_64(idx, gpa, size, is_prefetchable);
            } else {
                let alignment = calculate_alignment(size);
                gpa = resource_allocator
                    .mmio32_memory
                    .allocate(size, alignment, AllocPolicy::FirstMatch)
                    .map_err(|_| VfioError::BarAllocation)?
                    .start();
                assert!(gpa < u64::from(u32::MAX));
                assert!(size < u64::from(u32::MAX));
                #[allow(clippy::cast_possible_truncation)]
                let gpa = gpa as u32;
                #[allow(clippy::cast_possible_truncation)]
                let size = size as u32;
                bars.set_bar_32(idx, gpa, size, is_prefetchable);
            }
            debug!(
                "BAR{bar_idx} gpa: [{:#x}..{:#x}] size: {size:>#10x} io_bar: {is_io_bar} 64bits: \
                 {is_64_bits} prefetchable: {is_prefetchable}",
                gpa,
                gpa + size
            );
        } else {
            debug!(
                "BAR{bar_idx} size: {size:>#10x} io_bar: {is_io_bar} 64bits: {is_64_bits} \
                 prefetchable: {is_prefetchable}"
            );
        }
        bar_idx += 1;
    }
    Ok(bars)
}

/// Intermediate type to store areas needed to be mmaped for the device
#[derive(Debug, Clone, Copy)]
struct BarArea {
    /// offset
    bar_gpa: u64,
    /// offset
    region_offset: u64,
    /// offset
    offset: u64,
    /// size
    size: u64,
    /// prot
    prot: i32,
}

/// Calculate areas needed to be mmaped for the device BARs including any BAR holes caused
/// by MSI-X table/pba
fn calculate_bar_areas(
    bars: &Bars,
    region_infos: &[VfioRegionInfo],
    msix_cap: Option<&MsixCap>,
) -> (ArrayVec<BarArea, 8>, ArrayVec<BarHoleInfo, 2>) {
    let mut areas = ArrayVec::<BarArea, 8>::new();
    let mut bar_hole_infos = ArrayVec::<BarHoleInfo, 2>::new();
    let mut bar_idx: u8 = 0;
    while bar_idx < NUM_BAR_REGS {
        let bar_gpa = bars.get_bar_addr(bar_idx);
        if bar_gpa != 0 {
            let region_info = &region_infos[bar_idx as usize];
            let mut has_msix_mappable = false;
            let mut sparse_mmap_cap = None;
            for cap in region_info.caps.iter() {
                match cap {
                    VfioRegionInfoCap::SparseMmap(cap) => sparse_mmap_cap = Some(cap),
                    VfioRegionInfoCap::MsixMappable => has_msix_mappable = true,
                    _ => {}
                }
            }
            let mut contain_msix_table: bool = false;
            let mut msix_table_offset = 0;
            let mut msix_table_size = 0;

            let mut contain_msix_pba: bool = false;
            let mut msix_pba_offset = 0;
            let mut msix_pba_size = 0;

            if let Some(msix_cap) = msix_cap {
                contain_msix_table = bar_idx == msix_cap.table_bir();
                if contain_msix_table {
                    let (offset, size) = msix_cap.table_range();
                    let offset_in_hole = offset_from_lower_host_page(offset);

                    msix_table_offset = align_down_host_page(offset);
                    msix_table_size = align_up_host_page(offset_in_hole + size);

                    debug!(
                        "BAR{} msix_table hole: [{:#x}..{:#x}] actual table: [{:#x} ..{:#x}]",
                        bar_idx,
                        bar_gpa + msix_table_offset,
                        bar_gpa + msix_table_offset + msix_table_size,
                        bar_gpa + offset_in_hole,
                        bar_gpa + offset_in_hole + size,
                    );

                    let info = BarHoleInfo {
                        gpa: bar_gpa + msix_table_offset,
                        size: msix_table_size,
                        usage: BarHoleInfoUsage::Table,
                    };
                    bar_hole_infos.push(info);
                }

                contain_msix_pba = bar_idx == msix_cap.pba_bir();
                if contain_msix_pba {
                    let (offset, size) = msix_cap.pba_range();
                    let offset_in_hole = offset_from_lower_host_page(offset);

                    msix_pba_offset = align_down_host_page(offset);
                    msix_pba_size = align_up_host_page(offset_in_hole + size);

                    debug!(
                        "BAR{} pba_table hole: [{:#x} ..{:#x}] actual table: [{:#x} ..{:#x}]",
                        bar_idx,
                        bar_gpa + msix_pba_offset,
                        bar_gpa + msix_pba_offset + msix_pba_size,
                        bar_gpa + offset_in_hole,
                        bar_gpa + offset_in_hole + size,
                    );

                    let info = BarHoleInfo {
                        gpa: bar_gpa + msix_pba_offset,
                        size: msix_pba_size,
                        usage: BarHoleInfoUsage::Pba,
                    };
                    bar_hole_infos.push(info);
                }
            }

            if (contain_msix_table || contain_msix_pba)
                && !has_msix_mappable
                && sparse_mmap_cap.is_none()
            {
                debug!(
                    "BAR{} contains msix_table: {} msix_pba: {}, but mappable is {} and \
                     sparse_mmap_cap is {}. Skipping",
                    bar_idx,
                    contain_msix_table,
                    contain_msix_pba,
                    has_msix_mappable,
                    sparse_mmap_cap.is_some()
                );
            } else {
                let can_mmap = region_info.flags & VFIO_REGION_INFO_FLAG_MMAP != 0;
                if can_mmap || sparse_mmap_cap.is_some() {
                    let mut prot = 0;
                    if region_info.flags & VFIO_REGION_INFO_FLAG_READ != 0 {
                        prot |= libc::PROT_READ;
                    }
                    if region_info.flags & VFIO_REGION_INFO_FLAG_WRITE != 0 {
                        prot |= libc::PROT_WRITE;
                    }
                    let region_size = region_info.size;

                    if let Some(cap) = sparse_mmap_cap {
                        for area in cap.areas.iter() {
                            areas.push(BarArea {
                                bar_gpa,
                                region_offset: region_info.offset,
                                offset: area.offset,
                                size: area.size,
                                prot,
                            });
                        }
                    } else if has_msix_mappable {
                        let mut first_gap_offset = msix_table_offset;
                        let mut first_gap_size = msix_table_size;
                        let mut second_gap_offset = msix_pba_offset;
                        let mut second_gap_size = msix_pba_size;
                        if second_gap_offset < first_gap_offset {
                            second_gap_offset = msix_table_offset;
                            second_gap_size = msix_table_size;
                            first_gap_offset = msix_pba_offset;
                            first_gap_size = msix_pba_size;
                        }
                        let mut offset = 0;
                        if first_gap_size != 0 {
                            let area_size = first_gap_offset - offset;
                            if area_size != 0 {
                                areas.push(BarArea {
                                    bar_gpa,
                                    region_offset: region_info.offset,
                                    offset: offset,
                                    size: area_size,
                                    prot,
                                });
                            }
                            offset = first_gap_offset + first_gap_size;
                        }
                        if second_gap_size != 0 {
                            let area_size = second_gap_offset - offset;
                            if area_size != 0 {
                                areas.push(BarArea {
                                    bar_gpa,
                                    region_offset: region_info.offset,
                                    offset: offset,
                                    size: area_size,
                                    prot,
                                });
                            }
                            offset = second_gap_offset + second_gap_size;
                        }
                        let area_size = region_size - offset;
                        if area_size != 0 {
                            areas.push(BarArea {
                                bar_gpa,
                                region_offset: region_info.offset,
                                offset: offset,
                                size: area_size,
                                prot,
                            });
                        }
                    } else {
                        areas.push(BarArea {
                            bar_gpa,
                            region_offset: region_info.offset,
                            offset: 0,
                            size: region_size,
                            prot,
                        });
                    }
                }
            }
        }
        if bars.bars[bar_idx as usize].is_64bit() {
            bar_idx += 1;
        }
        bar_idx += 1;
    }
    (areas, bar_hole_infos)
}

fn dma_map_guest_memory(
    container: &VfioContainer,
    guest_memory: &GuestMemoryMmap,
) -> Result<(), VfioError> {
    for (i, region) in guest_memory.iter().enumerate() {
        if region.region_type == GuestRegionType::Dram {
            let region = &region.inner;
            let host_addr = region.as_ptr();
            let iova = region.start_addr().0;
            let size = region.size();
            debug!(
                "DMA map guest memory: [{:#x}..{:#x}]",
                iova,
                iova + size as u64
            );
            // SAFETY: guest memory region is valid and pinned for the lifetime of the VM.
            if let Err(e) = unsafe { container.vfio_dma_map(iova, size, host_addr) } {
                // Try to remove DMA mapping if anything fails. If unmap also fails, just log it
                // since there is nothing we can do about it.
                for (j, region) in guest_memory.iter().enumerate() {
                    if region.region_type == GuestRegionType::Dram && j < i {
                        let iova = region.start_addr().0;
                        let size = region.size();
                        if let Err(ee) = container.vfio_dma_unmap(iova, size) {
                            error!("Failed to unmap DAM from guest memory: {ee}");
                        }
                    }
                }
                return Err(VfioError::VfioIoctls(e));
            }
        }
    }
    Ok(())
}

fn map_bar_mapping(
    container: &VfioContainer,
    device: &VfioDevice,
    vm: &Vm,
    area: &BarArea,
    slot: u32,
) -> Result<BarMapping, VfioError> {
    // SAFETY: FFI call with correct arguments
    let host_addr_ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            area.size as usize,
            area.prot,
            libc::MAP_SHARED,
            device.as_raw_fd(),
            (area.region_offset + area.offset) as i64,
        )
    };

    if host_addr_ptr == libc::MAP_FAILED {
        return Err(VfioError::Mmap);
    }

    let slot = slot;
    let iova = area.bar_gpa + area.offset;
    let size = area.size;
    let host_addr = host_addr_ptr as u64;

    let kvm_memory_region = kvm_userspace_memory_region {
        slot,
        flags: 0,
        guest_phys_addr: iova,
        memory_size: size,
        userspace_addr: host_addr,
    };
    if let Err(e) = vm.set_user_memory_region(kvm_memory_region) {
        let r = unsafe { libc::munmap(host_addr as *mut libc::c_void, u64_to_usize(size)) };
        if r < 0 {
            error!(
                "Error on unmapping host memory on VFIO device creation failure: {r:?}. \
                 Continuing with other regions removal."
            );
        }
        return Err(VfioError::SetUserMemoryRegion(e.to_string()));
    }

    // TODO the `vfio_dma_map` always maps with `VFIO_DMA_MAP_FLAG_READ | VFIO_DMA_MAP_FLAG_WRITE`
    // which does not respect the `region_info.flags`.
    if let Err(e) =
        unsafe { container.vfio_dma_map(iova, u64_to_usize(size), host_addr_ptr as *mut u8) }
    {
        let kvm_memory_region = kvm_userspace_memory_region {
            slot,
            flags: 0,
            guest_phys_addr: iova,
            memory_size: 0,
            userspace_addr: host_addr,
        };
        if let Err(ee) = vm.set_user_memory_region(kvm_memory_region) {
            error!(
                "Error on removing KVM region on VFIO device creation failure: {ee:?}. Continuing \
                 with other regions removal."
            );
        }
        let r = unsafe { libc::munmap(host_addr as *mut libc::c_void, u64_to_usize(size)) };
        if r < 0 {
            error!(
                "Error on unmapping host memory on VFIO device creation failure: {r:?}. \
                 Continuing with other regions removal."
            );
        }
        return Err(e.into());
    }
    Ok(BarMapping {
        slot,
        iova,
        size,
        host_addr,
    })
}

fn unmap_bar_mapping(container: &VfioContainer, vm: &Vm, mapping: &BarMapping) {
    let kvm_memory_region = kvm_userspace_memory_region {
        slot: mapping.slot,
        flags: 0,
        guest_phys_addr: mapping.iova,
        memory_size: 0,
        userspace_addr: mapping.host_addr,
    };
    if let Err(ee) = vm.set_user_memory_region(kvm_memory_region) {
        error!(
            "Error on removing KVM region on VFIO device creation failure: {ee:?}. Continuing \
             with other regions removal."
        );
    }

    if let Err(ee) = container.vfio_dma_unmap(mapping.iova, u64_to_usize(mapping.size)) {
        error!(
            "Error on unmapping DMA region on VFIO device creation failure: {ee:?}. Continuing \
             with other regions removal."
        );
    }

    let r = unsafe {
        libc::munmap(
            mapping.host_addr as *mut libc::c_void,
            u64_to_usize(mapping.size),
        )
    };
    if r < 0 {
        error!(
            "Error on unmapping host memory on VFIO device creation failure: {r:?}. Continuing \
             with other regions removal."
        );
    }
}

/// Cleanup BAR mappings and DMA regions for a VFIO device bundle.
pub fn vfio_device_unmap_all_bars(bundle: &mut VfioDeviceBundle) {
    for mapping in bundle.bar_mappings.iter() {
        unmap_bar_mapping(&bundle.container, &bundle.vm, mapping);
    }
    bundle.bar_mappings.clear();
}

// There is no direct access to `regions` in `VfioDevice`, so need to work around this
fn extract_bar_region_infos(device: &VfioDevice) -> Vec<VfioRegionInfo> {
    (0..NUM_BAR_REGS as u32)
        .map(|i| VfioRegionInfo {
            flags: device.get_region_flags(i),
            size: device.get_region_size(i),
            offset: device.get_region_offset(i),
            caps: device.get_region_caps(i),
        })
        .collect()
}

/// Create KVM_DEV_TYPE_VFIO device
fn create_kvm_vfio_device(vm: &Vm) -> Result<kvm_ioctls::DeviceFd, VfioError> {
    let mut vfio_dev = kvm_create_device {
        type_: kvm_device_type_KVM_DEV_TYPE_VFIO,
        fd: 0,
        flags: 0,
    };
    vm.fd()
        .create_device(&mut vfio_dev)
        .map_err(VfioError::KVMCreateVfioDevice)
}

/// Create a VfioContainer wraper around both KVM vfio device and VFIO container
pub fn init_kvm_vfio_device_and_vfio_container(vm: &Vm) -> Result<Arc<VfioContainer>, VfioError> {
    let kvm_device_fd = create_kvm_vfio_device(vm)?;
    let device_fd = VfioDeviceFd::new_from_kvm(kvm_device_fd);
    let container = VfioContainer::new(Some(Arc::new(device_fd)))?;
    Ok(Arc::new(container))
}

fn prepare_vfio_device(
    container: &Arc<VfioContainer>,
    vm: &Arc<Vm>,
    path_on_host: &str,
    sbdf: PciSBDF,
    first_vfio_device: bool,
) -> Result<
    (
        VfioDevice,
        Bars,
        ArrayVec<BarMapping, 8>,
        Option<MsixState>,
        Vec<RegisterMask>,
    ),
    VfioError,
> {
    let device = VfioDevice::new(
        Path::new(path_on_host),
        container.clone() as Arc<dyn vfio_ioctls::VfioOps>,
    )?;
    device.reset();

    let bars = {
        let mut resource_allocator_lock = vm.resource_allocator();
        let resource_allocator = resource_allocator_lock.deref_mut();
        vfio_device_get_bars(&device, resource_allocator)?
    };
    let (msix_cap_and_register, masks) = vfio_device_get_pci_capabilities(&device)?;

    let mut msix_state = None;
    if let Some((msix_cap, msix_register)) = msix_cap_and_register {
        if let Some(msix_irq_info) = device.get_irq_info(VFIO_PCI_MSIX_IRQ_INDEX) {
            let msix_num = msix_irq_info.count as u16;
            let msix_vectors =
                Vm::create_msix_group(vm.clone(), msix_num).map_err(VfioError::MsixConfig)?;
            let msix_config = MsixConfig::new(Arc::new(msix_vectors), sbdf);

            // We set VFIO irqs here on device setup. There is no reason to add additional tracking
            // for driver MSIx configuration since those are handled by the MsixState.
            // If anything after this call fails, we don't need to do anything since the kernel will
            // clean up these irqs when `device` file will be closed.
            let fds: Vec<&EventFd> = msix_config
                .vectors
                .vectors
                .iter()
                .map(|v| &v.event_fd)
                .collect();
            device.enable_msix(fds)?;
            msix_state = Some(MsixState {
                register: msix_register,
                cap: msix_cap,
                bar_hole_infos: ArrayVec::new(),
                config: msix_config,
            });
        } else {
            return Err(VfioError::NoMsixIrq);
        }
    }

    let bar_region_infos = extract_bar_region_infos(&device);
    let (areas, bar_hole_infos) = calculate_bar_areas(
        &bars,
        &bar_region_infos,
        msix_cap_and_register.as_ref().map(|(v, _)| v),
    );
    let first_area_slot = vm
        .next_kvm_slot(areas.len() as u32)
        .ok_or(VfioError::KvmSlot)?;

    let mut bar_mappings = ArrayVec::<BarMapping, 8>::new();
    for (i, area) in areas.iter().enumerate() {
        match map_bar_mapping(
            container,
            &device,
            vm.as_ref(),
            area,
            first_area_slot + i as u32,
        ) {
            Ok(mapping) => {
                debug!(
                    "BAR area{} kvm gpa: [{:#x} ..{:#x}]",
                    i,
                    mapping.iova,
                    mapping.iova + mapping.size
                );
                bar_mappings.push(mapping);
            }
            Err(e) => {
                for mapping in bar_mappings.iter() {
                    unmap_bar_mapping(container, vm.as_ref(), mapping);
                }
                return Err(e);
            }
        }
    }

    if first_vfio_device {
        if let Err(e) = dma_map_guest_memory(container, vm.guest_memory()) {
            for mapping in bar_mappings.iter() {
                unmap_bar_mapping(container, vm.as_ref(), mapping);
            }
            return Err(e);
        }
    }

    if let Some(msix_state) = msix_state.as_mut() {
        msix_state.bar_hole_infos = bar_hole_infos;
    }
    Ok((device, bars, bar_mappings, msix_state, masks))
}

/// This will open a VFIO device, attach it's group both to the KVM VFIO device and to the VFIO
/// container. It will setup MSIx irqs and BAR DMAs.
pub fn init_vfio_device(
    container: &Arc<VfioContainer>,
    vm: &Arc<Vm>,
    config: VfioConfig,
    sbdf: PciSBDF,
    first_vfio_device: bool,
) -> Result<Arc<Mutex<VfioDeviceBundle>>, VfioError> {
    debug!("Opening device at path: {}", config.path_on_host);

    let (device, bars, bar_mappings, msix_state, masks) =
        prepare_vfio_device(container, vm, &config.path_on_host, sbdf, first_vfio_device)?;

    let vfio_device_bundle = Arc::new(Mutex::new(VfioDeviceBundle {
        config,
        sbdf,
        device,
        container: container.clone(),
        bars,
        bar_mappings,
        msix_state,
        masks,
        vm: vm.clone(),
    }));

    if let Some(msix_state) = vfio_device_bundle.lock().unwrap().msix_state.as_ref() {
        for hole in msix_state.bar_hole_infos.iter() {
            vm.common
                .mmio_bus
                .insert(vfio_device_bundle.clone(), hole.gpa, hole.size)
                .expect("Failed to register VFIO device mmio region");
        }
    }
    Ok(vfio_device_bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a BarRegionInfo with given size and flags for mmap.
    fn make_region(size: u64, caps: Vec<VfioRegionInfoCap>) -> VfioRegionInfo {
        let flags = if size != 0 {
            VFIO_REGION_INFO_FLAG_READ | VFIO_REGION_INFO_FLAG_WRITE | VFIO_REGION_INFO_FLAG_MMAP
        } else {
            0
        };
        VfioRegionInfo {
            flags,
            size,
            offset: 0,
            caps,
        }
    }

    /// Helper: create 6 empty region infos, then set specific ones.
    fn make_region_infos(overrides: &[(usize, VfioRegionInfo)]) -> Vec<VfioRegionInfo> {
        let mut infos: Vec<VfioRegionInfo> = (0..6)
            .map(|_| VfioRegionInfo {
                flags: 0,
                size: 0,
                offset: 0,
                caps: Vec::new(),
            })
            .collect();
        for (idx, info) in overrides {
            infos[*idx] = info.clone();
        }
        infos
    }

    #[test]
    fn test_calculate_bar_areas_no_msix() {
        let mut bars = Bars::default();
        bars.set_bar_64(0, 0x4000_0000_0000, 0x10_0000, false);
        let region_infos = make_region_infos(&[(0, make_region(0x10_0000, vec![]))]);

        let (areas, holes) = calculate_bar_areas(&bars, &region_infos, None);
        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0].bar_gpa, 0x4000_0000_0000);
        assert_eq!(areas[0].size, 0x10_0000);
        assert_eq!(areas[0].offset, 0);
        assert!(holes.is_empty());
    }

    #[test]
    fn test_calculate_bar_areas_msix_table_and_pba_different_bars() {
        let mut bars = Bars::default();
        bars.set_bar_64(0, 0x4000_0000_0000, 0x10_0000, false);
        bars.set_bar_64(2, 0x4000_0010_0000, 0x1_0000, false);

        let region_infos = make_region_infos(&[
            (
                0,
                make_region(0x10_0000, vec![VfioRegionInfoCap::MsixMappable]),
            ),
            (
                2,
                make_region(0x1_0000, vec![VfioRegionInfoCap::MsixMappable]),
            ),
        ]);

        let msix_cap = MsixCap::new(0, 32, 0, 2, 0);

        let (areas, holes) = calculate_bar_areas(&bars, &region_infos, Some(&msix_cap));
        assert_eq!(holes.len(), 2);
        assert!(!areas.is_empty());
    }

    #[test]
    fn test_calculate_bar_areas_msix_table_and_pba_same_bar() {
        let mut bars = Bars::default();
        bars.set_bar_64(0, 0x4000_0000_0000, 0x10_0000, false);

        let region_infos = make_region_infos(&[(
            0,
            make_region(0x10_0000, vec![VfioRegionInfoCap::MsixMappable]),
        )]);

        let msix_cap = MsixCap::new(0, 32, 0, 0, 0x1000);

        let (areas, holes) = calculate_bar_areas(&bars, &region_infos, Some(&msix_cap));
        assert_eq!(holes.len(), 2);
        assert!(!areas.is_empty());
        for area in areas.iter() {
            let area_start = area.bar_gpa + area.offset;
            let area_end = area_start + area.size;
            for hole in holes.iter() {
                let hole_end = hole.gpa + hole.size;
                assert!(
                    area_end <= hole.gpa || hole_end <= area_start,
                    "Area [{:#x}..{:#x}] overlaps with hole [{:#x}..{:#x}]",
                    area_start,
                    area_end,
                    hole.gpa,
                    hole_end
                );
            }
        }
    }

    #[test]
    fn test_calculate_bar_areas_sparse_mmap() {
        let mut bars = Bars::default();
        bars.set_bar_64(0, 0x4000_0000_0000, 0x10_0000, false);

        let sparse_areas = vec![
            VfioRegionSparseMmapArea {
                offset: 0,
                size: 0x8_0000,
            },
            VfioRegionSparseMmapArea {
                offset: 0xC_0000,
                size: 0x4_0000,
            },
        ];
        let region_infos = make_region_infos(&[(
            0,
            make_region(
                0x10_0000,
                vec![VfioRegionInfoCap::SparseMmap(VfioRegionInfoCapSparseMmap {
                    areas: sparse_areas,
                })],
            ),
        )]);

        let msix_cap = MsixCap::new(0, 32, 0x8_0000, 0, 0xB_0000);

        let (areas, _holes) = calculate_bar_areas(&bars, &region_infos, Some(&msix_cap));
        assert_eq!(areas.len(), 2);
        assert_eq!(areas[0].offset, 0);
        assert_eq!(areas[0].size, 0x8_0000);
        assert_eq!(areas[1].offset, 0xC_0000);
        assert_eq!(areas[1].size, 0x4_0000);
    }

    #[test]
    fn test_calculate_bar_areas_zero_size_bar() {
        let bars = Bars::default();
        let region_infos = make_region_infos(&[]);

        let (areas, holes) = calculate_bar_areas(&bars, &region_infos, None);
        assert!(areas.is_empty());
        assert!(holes.is_empty());
    }
}
