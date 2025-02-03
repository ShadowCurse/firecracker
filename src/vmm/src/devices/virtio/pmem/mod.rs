// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod device;
pub mod event_handler;
pub mod persist;

pub(crate) const PMEM_QUEUE_SIZE: u16 = 256;
pub(crate) const PMEM_NUM_QUEUES: usize = 1;
