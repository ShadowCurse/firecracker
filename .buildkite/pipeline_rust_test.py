#!/usr/bin/env python3
# Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

"""Generate Buildkite pipelines dynamically"""

from common import (
    COMMON_PARSER,
    devtool_test,
    get_changed_files,
    group,
    overlay_dict,
    pipeline_to_json,
    run_all_tests,
)

# Buildkite default job priority is 0. Setting this to 1 prioritizes PRs over
# scheduled jobs and other batch jobs.
DEFAULT_PRIORITY = 1


args = COMMON_PARSER.parse_args()

defaults = {
    "instances": args.instances,
    "platforms": args.platforms,
    # buildkite step parameters
    "priority": DEFAULT_PRIORITY,
    "timeout_in_minutes": 120,
    "artifacts": ["./rust_test_results/**/*"],
}
defaults = overlay_dict(defaults, args.step_param)

cargo_test = group(
    "📦 Cargo test",
    "./tests/prepare_rust_tests.sh",
    **defaults,
)

steps = [cargo_test]

pipeline = {"steps": steps}
print(pipeline_to_json(pipeline))
