# Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

"""Integration tests for VFIO passthrough API."""

import re

import pytest


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

    # Missing id in path
    with pytest.raises(RuntimeError):
        vm.api.vfio.put(path="/sys/bus/pci/devices/0003:16:00.0")

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
    expected_msg = re.escape("Empty device id")
    with pytest.raises(RuntimeError, match=expected_msg):
        vm.api.vfio.put(id="", path="/sys/bus/pci/devices/0003:16:00.0")

    # Empty path should fail
    expected_msg = re.escape("Empty device path")
    with pytest.raises(RuntimeError, match=expected_msg):
        vm.api.vfio.put(id="dev1", path="")
