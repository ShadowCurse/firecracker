#!/usr/bin/env python3
# Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Script used to generate snapshots of microVMs."""

import json
import os
import platform
import re
import shutil
import sys
from pathlib import Path

# Hack to be able to import testing framework functions.
sys.path.append(os.path.join(os.getcwd(), "tests"))  # noqa: E402

# pylint: disable=wrong-import-position
from framework.artifacts import disks, kernels
from framework.microvm import MicroVMFactory
from framework.utils import (
    configure_mmds,
    generate_mmds_get_request,
    generate_mmds_session_token,
)
from framework.utils_cpu_templates import get_supported_cpu_templates
from host_tools.cargo_build import get_firecracker_binaries

import host_tools.cargo_build as host
from framework import utils
from framework.properties import global_props

# pylint: enable=wrong-import-position

# Default IPv4 address to route MMDS requests.
IPV4_ADDRESS = "169.254.169.254"
NET_IFACE_FOR_MMDS = "eth3"
# Root directory for the snapshot artifacts.
SNAPSHOT_ARTIFACTS_ROOT_DIR = "snapshot_artifacts"


def populate_mmds(microvm, data_store):
    """Populate MMDS contents with json data provided."""
    # MMDS should be empty.
    response = microvm.api.mmds.get()
    assert response.json() == {}

    # Populate MMDS with data.
    microvm.api.mmds.put(**data_store)

    # Ensure data is persistent inside the data store.
    response = microvm.api.mmds.get()
    assert response.json() == data_store


def validate_mmds(ssh_connection, data_store):
    """Validate that MMDS contents fetched from the guest."""
    # Configure interface to route MMDS requests
    cmd = "ip route add {} dev {}".format(IPV4_ADDRESS, NET_IFACE_FOR_MMDS)
    _, stdout, stderr = ssh_connection.run(cmd)
    assert stdout == stderr == ""

    # Fetch metadata to ensure MMDS is accessible.
    token = generate_mmds_session_token(ssh_connection, IPV4_ADDRESS, token_ttl=60)

    cmd = generate_mmds_get_request(IPV4_ADDRESS, token=token)
    _, stdout, _ = ssh_connection.run(cmd)
    assert json.loads(stdout) == data_store


def main():
    """
    Run the main logic.

    Create snapshot artifacts from complex microVMs with all Firecracker's
    functionality enabled. The kernels are parametrized to include all guest
    supported versions.

    Artifacts are saved in the following format:
    snapshot_artifacts
        |
        -> <guest_kernel_supported_0>_<cpu_template>_guest_snapshot
            |
            -> vm.mem
            -> vm.vmstate
            -> ubuntu-22.04.id_rsa
            -> ubuntu-22.04.ext4
        -> <guest_kernel_supported_1>_<cpu_template>_guest_snapshot
            |
            ...
    """
    # Create directory dedicated to store snapshot artifacts for
    # each guest kernel version.
    print("Cleanup")
    shutil.rmtree(SNAPSHOT_ARTIFACTS_ROOT_DIR, ignore_errors=True)
    vm_factory = MicroVMFactory(*get_firecracker_binaries())

    cpu_templates = []
    if platform.machine() == "x86_64":
        cpu_templates = ["None"]
    cpu_templates += get_supported_cpu_templates()

    snap_editor = host.get_binary("snapshot-editor")
    remove_regs_5_10_to_6_1 = [str(r) for r in  [6931039826524487690, 6931039826524487720, 6931039826524488945, 6931039826524488946, 6931039826524495072, 6931039826524495073, 6931039826524495074, 6931039826524495075, 6931039826524495076, 6931039826524495077, 6931039826524495080, 6931039826524495088, 6931039826524495091, 6931039826524495680, 6931039826524495681, 6931039826524495682, 6931039826524495683, 6931039826524495684, 6931039826524495685, 6931039826524495686, 6931039826524495687, 6931039826524495688, 6931039826524495689, 6931039826524495690, 6931039826524495691, 6931039826524495692, 6931039826524495693, 6931039826524495694, 6931039826524495695, 6931039826524495696, 6931039826524495697, 6931039826524495698, 6931039826524495699, 6931039826524495700, 6931039826524495701, 6931039826524495702, 6931039826524495703, 6931039826524495704, 6931039826524495705, 6931039826524495706, 6931039826524495707, 6931039826524495708, 6931039826524495709, 6931039826524495710, 6931039826524495712, 6931039826524495713, 6931039826524495714, 6931039826524495715, 6931039826524495716, 6931039826524495717, 6931039826524495718, 6931039826524495719, 6931039826524495720, 6931039826524495721, 6931039826524495722, 6931039826524495723, 6931039826524495724, 6931039826524495725, 6931039826524495726, 6931039826524495727, 6931039826524495728, 6931039826524495729, 6931039826524495730, 6931039826524495731, 6931039826524495732, 6931039826524495733, 6931039826524495734, 6931039826524495735, 6931039826524495736, 6931039826524495737, 6931039826524495738, 6931039826524495739, 6931039826524495740, 6931039826524495741, 6931039826524495742, 6931039826524495743]]
    remove_regs_6_1_to_5_10 = [str(r) for r in [6931039826524504064,, 6931039826524635136,, 6931039826524635137,, 6931039826524635138,, 6931039826524471436,, 6931039826524487690,, 6931039826524487720]], 

    host_kernel = global_props.host_linux_version

    for cpu_template in cpu_templates:
        for kernel in kernels(glob="vmlinux-*"):
            for rootfs in disks(glob="ubuntu-*.squashfs"):
                print(kernel, rootfs, cpu_template)
                vm = vm_factory.build(kernel, rootfs)
                vm.spawn(log_level="Info")
                vm.basic_config(
                    vcpu_count=2,
                    mem_size_mib=1024,
                    cpu_template=cpu_template,
                    track_dirty_pages=True,
                )
                # Add 4 network devices
                for i in range(4):
                    vm.add_net_iface()
                # Add a vsock device
                vm.api.vsock.put(vsock_id="vsock0", guest_cid=3, uds_path="/v.sock")
                # Add MMDS
                configure_mmds(vm, ["eth3"], version="V2")
                # Add a memory balloon.
                vm.api.balloon.put(
                    amount_mib=0, deflate_on_oom=True, stats_polling_interval_s=1
                )

                vm.start()
                # Ensure the microVM has started.
                assert vm.state == "Running"

                # Populate MMDS.
                data_store = {
                    "latest": {
                        "meta-data": {
                            "ami-id": "ami-12345678",
                            "reservation-id": "r-fea54097",
                            "local-hostname": "ip-10-251-50-12.ec2.internal",
                            "public-hostname": "ec2-203-0-113-25.compute-1.amazonaws.com",
                        }
                    }
                }
                populate_mmds(vm, data_store)

                # Iterate and validate connectivity on all ifaces after boot.
                for i in range(4):
                    exit_code, _, _ = vm.ssh_iface(i).run("sync")
                    assert exit_code == 0

                # Validate MMDS.
                validate_mmds(vm.ssh, data_store)

                # Snapshot the microVM.
                snapshot = vm.snapshot_diff()

                # Create snapshot artifacts directory specific for the kernel version used.
                guest_kernel_version = re.search("vmlinux-(.*)", kernel.name)

                snapshot_artifacts_dir = (
                    Path(SNAPSHOT_ARTIFACTS_ROOT_DIR)
                    / f"{guest_kernel_version.group(1)}_{cpu_template}_guest_snapshot"
                )
                snapshot_artifacts_dir.mkdir(parents=True)
                snapshot.save_to(snapshot_artifacts_dir)
                print(f"Copied snapshot to: {snapshot_artifacts_dir}.")

                vm_state_path = f"{snapshot_artifacts_dir}/vmstate";
                if host_kernel == "5.10":
                    print("Removing regs for 5.10 to 6.1")
                    cmd = [
                        str(snap_editor),
                        "edit-vmstate",
                        "remove-regs",
                        "--vmstate-path",
                        vm_state_path,
                        "--output-path",
                        vm_state_path,
                        remove_regs_5_10_to_6_1,
                    ]
                    utils.run_cmd(cmd)
                if host_kernel == "6.1":
                    print("Removing regs for 6.1 to 5.10")
                    cmd = [
                        str(snap_editor),
                        "edit-vmstate",
                        "remove-regs",
                        "--vmstate-path",
                        vm_state_path,
                        "--output-path",
                        vm_state_path,
                        remove_regs_5_10_to_6_1,
                    ]
                    utils.run_cmd(cmd)


                vm.kill()


if __name__ == "__main__":
    main()
