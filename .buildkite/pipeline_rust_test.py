#!/usr/bin/env python3
# Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

"""Generate Buildkite pipelines dynamically"""

from common import BKPipeline, get_changed_files, run_all_tests

pipeline = BKPipeline(
    with_build_step=False,
    timeout_in_minutes=45,
    priority=1,
    artifact_paths=["./rust_test_results/**/*"]
)

pipeline.build_group(
    "📦 Cargo test",
    "./tests/prepare_rust_tests.sh",
)
print(pipeline.to_json())
