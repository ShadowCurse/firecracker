// Copyright 2023 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use self::virtio::NetError as VirtioNetError;
use self::virtio::persist::NetPersistError as VirtioPersistError;

pub mod gen;
pub mod device;
pub mod persist;
pub mod vhost;
pub mod virtio;

/// Errors the block device can trigger.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum NetError {
    /// Virtio backend error: {0}
    VirtioBackend(VirtioNetError),
    /// Persist error: {0}
    VirtioBackendPersist(VirtioPersistError),
    /// Vhost backend error: {0}
    VhostBackend(VirtioNetError),
}
