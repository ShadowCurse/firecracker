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

# Skip tests if no VFIO device was passed throug env variables Also use
# `xdist_group` to make sure all tests here run sequentially since we only have
# 1 device to work with
pytestmark = [
    pytest.mark.skipif(
        VFIO_SBDF is None, reason="No VFIO device configured (set FC_VFIO_PCI_SBDF)"
    ),
    pytest.mark.xdist_group("vfio"),
]


def _create_sysfs_in_chroot(vm):
    """Create a minimal sysfs entry for the VFIO device inside the jailer chroot."""
    chroot = Path(vm.jailer.chroot_path())
    dev_sysfs = chroot / VFIO_SYSFS.lstrip("/")
    dev_sysfs.mkdir(parents=True, exist_ok=True)


def test_api_vfio(uvm_plain):
    """
    Test VFIO passthrough API commands.
    """
    vm = uvm_plain
    vm.jailer.setup()
    _create_sysfs_in_chroot(vm)
    vm.spawn()
    vm.basic_config()

    # Missing required field 'path'
    expected_msg = re.escape("missing field `path_on_host`")
    with pytest.raises(RuntimeError, match=expected_msg):
        vm.api.vfio.put(id="dev0")

    # Valid VFIO device config
    vm.api.vfio.put(id="nvme0", path_on_host=VFIO_SYSFS)

    # Overwriting an existing device should be OK
    vm.api.vfio.put(id="nvme0", path_on_host=VFIO_SYSFS)

    # Adding a second device should be OK
    vm.api.vfio.put(id="nvme1", path_on_host=VFIO_SYSFS)

    # Empty id should fail
    expected_msg = re.escape("The ID cannot be empty.")
    with pytest.raises(RuntimeError, match=expected_msg):
        vm.api.vfio.put(id="", path_on_host=VFIO_SYSFS)

    # Empty path should fail
    expected_msg = re.escape("Cannot verify path to the VFIO device")
    with pytest.raises(RuntimeError, match=expected_msg):
        vm.api.vfio.put(id="dev1", path_on_host="")

    # Invalid VFIO device config
    invalid_device_path = f"/sys/bus/pci/devices/69:69:69"
    expected_msg = re.escape("Cannot verify path to the VFIO device")
    with pytest.raises(RuntimeError, match=expected_msg):
        vm.api.vfio.put(id="nvme0", path_on_host=invalid_device_path)


@pytest.fixture
def uvm_with_vfio(microvm_factory, guest_kernel_linux_6_1, rootfs):
    """Boot a microVM with the VFIO NVMe device attached."""
    vm = microvm_factory.build(guest_kernel_linux_6_1, rootfs, pci=True)

    # Set up the jailer chroot directory before spawning
    vm.jailer.setup()
    _create_sysfs_in_chroot(vm)
    chroot = Path(vm.jailer.chroot_path())

    # Create VFIO device nodes inside the jailer chroot
    group_id = os.readlink(f"{VFIO_SYSFS}/iommu_group").split("/")[-1]
    vfio_dir = chroot / "dev" / "vfio"
    vfio_dir.mkdir(parents=True, exist_ok=True)
    for name in ["vfio", group_id]:
        src = Path(f"/dev/vfio/{name}")
        dst = vfio_dir / name
        st = src.stat()
        os.mknod(dst, stat.S_IFCHR | 0o600, st.st_rdev)
        os.chown(dst, vm.jailer.uid, vm.jailer.gid)

    # Create iommu_group symlink for the VFIO device.
    # The VFIO code readlink()s this to get the group ID from the basename.
    dev_sysfs = chroot / VFIO_SYSFS.lstrip("/")
    (dev_sysfs / "iommu_group").symlink_to(f"../iommu_groups/{group_id}")
    os.lchown(dev_sysfs / "iommu_group", vm.jailer.uid, vm.jailer.gid)

    vm.spawn()
    vm.basic_config(mem_size_mib=512)
    vm.add_net_iface()
    vm.api.vfio.put(id="nvme0", path_on_host=VFIO_SYSFS)
    vm.start()
    return vm


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
    assert "Non-Volatile memory controller" not in stdout


def test_vfio_nvme_visible(uvm_with_vfio):
    """The passthrough device appears on the guest PCI bus."""
    vm = uvm_with_vfio
    _, stdout, _ = vm.ssh.check_output("lspci -nn")
    assert "Non-Volatile memory controller" in stdout

    vm.ssh.check_output("test -d /sys/class/nvme/nvme0")
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
    vm.ssh.check_output("dd if=/tmp/pattern of=/dev/nvme0n1 bs=4k count=1 oflag=direct")
    vm.ssh.check_output(
        "dd if=/dev/nvme0n1 of=/tmp/readback bs=4k count=1 iflag=direct"
    )
    vm.ssh.check_output("cmp /tmp/pattern /tmp/readback")
