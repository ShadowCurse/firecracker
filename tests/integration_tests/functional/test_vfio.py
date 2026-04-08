# Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

"""Integration tests for VFIO passthrough."""

import json
import os
import re
import stat
from pathlib import Path

import pytest

VFIO_SBDF = os.environ.get("FC_VFIO_PCI_SBDF")
VFIO_SYSFS = os.environ.get("FC_VFIO_PCI_SYSFS_PATH")

pytestmark = pytest.mark.skipif(
    VFIO_SBDF is None, reason="No VFIO device configured (set FC_VFIO_PCI_SBDF)"
)


def test_api_vfio(uvm_plain):
    """
    Test VFIO passthrough API commands.
    """
    vm = uvm_plain
    vm.spawn()
    vm.basic_config()

    # Missing required field 'path'
    expected_msg = re.escape("missing field `path`")
    with pytest.raises(RuntimeError, match=expected_msg):
        vm.api.vfio.put(id="dev0")

    # Valid VFIO device config
    vm.api.vfio.put(
        id="nvme0",
        path="/sys/bus/pci/devices/0003:16:00.0",
    )

    # Overwriting an existing device should be OK
    vm.api.vfio.put(
        id="nvme0",
        path="/sys/bus/pci/devices/0003:16:00.0",
    )

    # Adding a second device should be OK
    vm.api.vfio.put(
        id="nvme1",
        path="/sys/bus/pci/devices/0003:16:00.0",
    )

    # Empty id should fail
    expected_msg = re.escape("The ID cannot be empty.")
    with pytest.raises(RuntimeError, match=expected_msg):
        vm.api.vfio.put(id="", path="/sys/bus/pci/devices/0003:16:00.0")

    # Empty path should fail
    expected_msg = re.escape("Empty device path")
    with pytest.raises(RuntimeError, match=expected_msg):
        vm.api.vfio.put(id="dev1", path="")


@pytest.fixture
def uvm_with_vfio(microvm_factory, guest_kernel_linux_6_1, rootfs):
    """Boot a microVM with the VFIO NVMe device attached."""
    vm = microvm_factory.build(guest_kernel_linux_6_1, rootfs, pci=True)

    # Set up the jailer chroot directory before spawning
    vm.jailer.setup()
    chroot = Path(vm.jailer.chroot_path())

    # Create VFIO device nodes inside the jailer chroot
    vfio_dir = chroot / "dev" / "vfio"
    vfio_dir.mkdir(parents=True, exist_ok=True)
    for src in Path("/dev/vfio").iterdir():
        dst = vfio_dir / src.name
        st = src.stat()
        os.mknod(dst, stat.S_IFCHR | 0o600, st.st_rdev)
        os.chown(dst, vm.jailer.uid, vm.jailer.gid)

    # Create minimal sysfs structure for the VFIO device.
    # The VFIO code only needs to readlink() the iommu_group symlink to get
    # the group ID from the filename. Create a fake target directory so the
    # symlink basename resolves correctly.
    group_id = os.readlink(f"{VFIO_SYSFS}/iommu_group").split("/")[-1]
    dev_sysfs = chroot / VFIO_SYSFS.lstrip("/")
    dev_sysfs.mkdir(parents=True, exist_ok=True)
    # Create a symlink that points to a path whose basename is the group ID
    (dev_sysfs / "iommu_group").symlink_to(f"../iommu_groups/{group_id}")
    os.lchown(dev_sysfs / "iommu_group", vm.jailer.uid, vm.jailer.gid)

    vm.spawn()
    vm.basic_config(mem_size_mib=512)
    vm.add_net_iface()
    vm.api.vfio.put(id="nvme0", path=VFIO_SYSFS)
    vm.start()
    return vm


def test_vfio_nvme_visible(uvm_with_vfio):
    """The passthrough device appears on the guest PCI bus."""
    vm = uvm_with_vfio
    _, stdout, _ = vm.ssh.check_output("lspci -nn")
    # Amazon EBS NVMe: vendor 1d0f, device 0065
    assert "1d0f:0065" in stdout

    vm.ssh.check_output("test -d /sys/class/nvme/nvme0")


def test_vfio_nvme_block_device(uvm_with_vfio):
    """The NVMe driver creates a block device node."""
    vm = uvm_with_vfio
    vm.ssh.check_output("test -b /dev/nvme0n1")

    _, stdout, _ = vm.ssh.check_output("lsblk -Jb")
    blocks = json.loads(stdout)["blockdevices"]
    nvme = [b for b in blocks if b["name"] == "nvme0n1"]
    assert len(nvme) == 1
    assert int(nvme[0]["size"]) > 0


def test_vfio_nvme_read(uvm_with_vfio):
    """The guest can read data from the passthrough NVMe device."""
    vm = uvm_with_vfio
    _, stdout, _ = vm.ssh.check_output(
        "dd if=/dev/nvme0n1 of=/dev/null bs=4k count=256 2>&1"
    )
    assert "256+0 records in" in stdout


def test_vfio_nvme_write_readback(uvm_with_vfio):
    """Write data and read it back to confirm DMA in both directions."""
    vm = uvm_with_vfio
    vm.ssh.check_output("dd if=/dev/urandom of=/tmp/pattern bs=4k count=1")
    vm.ssh.check_output(
        "dd if=/tmp/pattern of=/dev/nvme0n1 bs=4k count=1 oflag=direct"
    )
    vm.ssh.check_output(
        "dd if=/dev/nvme0n1 of=/tmp/readback bs=4k count=1 iflag=direct"
    )
    vm.ssh.check_output("cmp /tmp/pattern /tmp/readback")


def test_vfio_nvme_interrupts(uvm_with_vfio):
    """MSI-X interrupts are delivered correctly."""
    vm = uvm_with_vfio

    # Verify NVMe interrupt lines exist
    _, stdout, _ = vm.ssh.check_output("grep nvme /proc/interrupts")
    assert "nvme" in stdout

    # Capture interrupt counts before I/O
    _, before, _ = vm.ssh.check_output("grep nvme /proc/interrupts")

    # Generate I/O
    vm.ssh.check_output("dd if=/dev/nvme0n1 of=/dev/null bs=4k count=100")

    # Verify interrupt counts increased
    _, after, _ = vm.ssh.check_output("grep nvme /proc/interrupts")

    def sum_irq_counts(lines):
        total = 0
        for line in lines.strip().splitlines():
            parts = line.split()
            for p in parts[1:]:
                if p.isdigit():
                    total += int(p)
                else:
                    break
        return total

    assert sum_irq_counts(after) > sum_irq_counts(before)


def test_vfio_nvme_not_present_without_config(
    microvm_factory, guest_kernel_linux_6_1, rootfs
):
    """NVMe device does NOT appear when no VFIO device is configured."""
    vm = microvm_factory.build(guest_kernel_linux_6_1, rootfs, pci=True)
    vm.spawn()
    vm.basic_config(mem_size_mib=512)
    vm.add_net_iface()
    # Do NOT add any VFIO device
    vm.start()

    # Check no NVMe block devices exist
    rc, _, _ = vm.ssh.run("test -e /dev/nvme0n1")
    assert rc != 0

    _, stdout, _ = vm.ssh.check_output("lspci -nn")
    assert "1d0f" not in stdout
