# Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Performance benchmark for the jailer."""

import os
import shutil
from concurrent.futures import ProcessPoolExecutor

import pytest

from framework import utils
from framework.jailer import DEFAULT_CHROOT_PATH, JailerContext
from framework.properties import global_props


def setup_bind_mounts(tmp_path, n):
    """
    Create bind mount points. The exact location of them
    does not matter, they just need to exist.
    """
    mounts_paths = tmp_path / "mounts"
    os.makedirs(mounts_paths)
    for m in range(n):
        mount_path = f"{mounts_paths}/mount{m}"
        os.makedirs(mount_path)
        utils.check_output(f"mount --bind {mount_path} {mount_path}")


def clean_up_mounts(tmp_path):
    """Cleanup mounts and jailer dirs"""
    mounts_paths = tmp_path / "mounts"
    for d in os.listdir(mounts_paths):
        utils.check_output(f"umount {mounts_paths}/{d}")


import subprocess
import signal
import shlex
import time


@pytest.mark.nonci
@pytest.mark.parametrize("parallel", [1, 5, 10])
@pytest.mark.parametrize("mounts", [0, 100, 300, 500])
def test_jailer_startup(
    jailer_time_bin, tmp_path, microvm_factory, parallel, mounts, metrics, results_dir
):
    """
    Test the overhead of jailer startup without and with bind mounts
    with different parallelism options.
    """

    jailer_binary = microvm_factory.jailer_binary_path

    setup_bind_mounts(tmp_path, mounts)

    metrics.set_dimensions(
        {
            "instance": global_props.instance,
            "cpu_model": global_props.cpu_model,
            "performance_test": "test_jailer_startup_new",
            "parallel": str(parallel),
            "mounts": str(mounts),
        }
    )

    cmds = []
    for i in range(500):
        jailer = JailerContext(
            jailer_id=f"fakefc{i}",
            exec_file=jailer_time_bin,
            # Don't deamonize to get the stdout
            daemonize=False,
            new_pid_ns = True,
        )
        jailer.setup()

        cmd = [str(jailer_binary), *jailer.construct_param_list()]
        cmds.append(cmd)

    bpf_result_file = f"{results_dir}/bpf.txt"
    bpf_cmd = f"bpftrace host_tools/jailer_bpftrace.txt -o {bpf_result_file}"

    bpftrace = subprocess.Popen(
        shlex.split(bpf_cmd),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    time.sleep(1)

    with ProcessPoolExecutor(max_workers=parallel) as executor:
        # Submit all commands and get results
        results = executor.map(utils.check_output, cmds)

        # Get results as they complete
        for result in results:
            end_time, start_time = result.stdout.split()
            metrics.put_metric(
                "startup",
                int(end_time) - int(start_time),
                unit="Microseconds",
            )

    time.sleep(1)
    os.kill(bpftrace.pid, signal.SIGINT)
    bpftrace.communicate()

    clean_up_mounts(tmp_path)
    shutil.rmtree(DEFAULT_CHROOT_PATH)
