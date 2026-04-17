# VFIO Device Passthrough

## What is VFIO

VFIO (Virtual Function I/O) is a Linux kernel framework that allows userspace
programs to directly access physical devices in a secure, IOMMU-protected
environment. Firecracker uses VFIO to pass through PCI devices from the host
into the guest, giving the guest near-native performance access to physical
hardware such as GPUs, network adapters, and NVMe drives.

## How it works

Firecracker acts as an intermediary between the physical device and the guest.
The key mechanisms are:

- **PCI configuration space**: Firecracker partially emulates the device's
  config space. Most reads/writes are proxied to the physical device via the
  VFIO device fd. BAR registers, MSI-X capability bits, and certain extended
  capabilities are emulated or masked by Firecracker.
- **BAR regions**: Device BAR memory regions are `mmap`'d from the VFIO device
  fd and mapped directly into guest address space as KVM memory slots. Guest
  accesses hit the device hardware directly with no VM-exit, providing native
  performance.
- **MSI-X interrupts**: Firecracker emulates the MSI-X table and PBA. Physical
  device interrupts are delivered to the guest via eventfds wired through KVM's
  irqfd mechanism, requiring no VMM involvement in the hot path.
- **DMA**: Guest RAM is mapped into the device's IOMMU page tables so the
  device can DMA directly to/from guest memory. The IOMMU enforces that the
  device can only access explicitly mapped regions.

## Prerequisites

VFIO passthrough requires:

- An IOMMU (Intel VT-d, AMD-Vi, or ARM SMMU) enabled on the host.
- The target PCI device unbound from its native kernel driver and bound to the
  `vfio-pci` driver.
- All devices in the same IOMMU group must be bound to `vfio-pci`.

To bind a device (e.g. `0000:41:00.0`) to `vfio-pci`:

```bash
# Unbind from current driver
echo "0000:41:00.0" > /sys/bus/pci/devices/0000:41:00.0/driver/unbind
# Bind to vfio-pci
echo "vfio-pci" > /sys/bus/pci/devices/0000:41:00.0/driver_override
echo "0000:41:00.0" > /sys/bus/pci/drivers/vfio-pci/bind
```

## Configuration

Firecracker exposes the following configuration options for VFIO devices:

- `id` - unique identifier for the device
- `path_on_host` - sysfs path to the PCI device (e.g. `/sys/bus/pci/devices/0000:41:00.0`)

### Config file

```json
"vfio": [
    {
      "id": "devices0",
      "path_on_host": "/sys/bus/pci/devices/0000:11:22.3"
    }
]
```

### API

```console
curl --unix-socket $socket_location -i \
    -X PUT 'http://localhost/vfio/gpu0' \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d "{
         \"id\": \"device0\",
         \"path_on_host\": \"/sys/bus/pci/devices/0000:11:22.3\"
    }"
```

## Security

- **IOMMU is mandatory.** Without an IOMMU, a passthrough device could DMA to
  arbitrary host memory. Firecracker relies on VFIO's Type1v2 IOMMU backend to
  enforce DMA isolation.
- **IOMMU groups.** All devices in the same IOMMU group must be assigned to the
  same VM. Splitting a group across VMs would break DMA isolation.

## Snapshot support

VFIO devices do not support snapshots. Device state is opaque to the VMM and
cannot be serialized or restored. VMs with VFIO devices attached cannot be
snapshotted.

## Limitations

| Limitation              | Details                                                                                                  |
| :---------------------- | :------------------------------------------------------------------------------------------------------- |
| No snapshots            | Device state is opaque and cannot be saved/restored.                                                     |
| No hot-plug/unplug      | All VFIO devices must be configured before VM boot.                                                      |
| No BAR relocation       | BAR addresses are assigned at init and cannot be moved.                                                  |
| No BAR resizing         | Resizable BAR capability is masked from the guest.                                                       |
| No IO BARs              | I/O-type BARs are skipped. Devices relying solely on IO BARs will not work.                              |
| No ROM BAR              | Expansion ROM BAR is not handled.                                                                        |
| No MSI (non-X)          | Only MSI-X interrupts are supported. Devices with only MSI will not receive interrupts.                  |
| No INTx                 | Legacy pin-based interrupts are not supported.                                                           |
| No SR-IOV               | SR-IOV capability is masked. Virtual Functions cannot be created.                                        |
| No virtio-iommu         | The guest has no IOMMU. DMA isolation relies entirely on the host IOMMU.                                 |
