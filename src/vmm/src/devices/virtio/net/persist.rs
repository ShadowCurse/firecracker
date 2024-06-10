// Copyright 2023 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::virtio::persist::NetState as VirtioNetState;
use crate::{mmds::data_store::Mmds, vstate::memory::GuestMemoryMmap};

/// Block device state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetState {
    Virtio(VirtioNetState),
}

/// Auxiliary structure for creating a device when resuming from a snapshot.
#[derive(Debug)]
pub struct NetConstructorArgs {
    /// Pointer to guest memory.
    pub mem: GuestMemoryMmap,
    /// Pointer to the MMDS data store.
    pub mmds: Option<Arc<Mutex<Mmds>>>,
}
