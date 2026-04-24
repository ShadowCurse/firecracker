// Copyright 2023 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
#![allow(
    clippy::similar_names,
    clippy::module_name_repetitions,
    clippy::unreadable_literal,
    clippy::unsafe_derive_deserialize
)]

/// CPUID normalize implementation.
mod normalize;

pub use normalize::{DeterministicCacheError, NormalizeCpuidError};

use super::{CpuidEntry, CpuidKey, CpuidTrait};
use crate::cpu_config::x86_64::cpuid::cpuid_insert;

/// A typed view over a `kvm_bindings::CpuId` that is known to contain Intel CPUID data.
///
/// Matches the Intel CPUID specification as described in
/// [Intel® 64 and IA-32 Architectures Software Developer's Manual Combined Volumes 2A, 2B, 2C, and 2D: Instruction Set Reference, A-Z](https://cdrdv2.intel.com/v1/dl/getContent/671110)
/// .
#[derive(Debug, Clone, PartialEq)]
#[repr(transparent)]
pub struct IntelCpuid(pub kvm_bindings::CpuId);

impl Eq for IntelCpuid {}

impl CpuidTrait for IntelCpuid {
    /// Gets a given sub-leaf.
    #[inline]
    fn get(&self, key: &CpuidKey) -> Option<&CpuidEntry> {
        self.0.get(key)
    }

    /// Gets a given sub-leaf.
    #[inline]
    fn get_mut(&mut self, key: &CpuidKey) -> Option<&mut CpuidEntry> {
        self.0.get_mut(key)
    }
}

impl IntelCpuid {
    /// Insert or update a CPUID entry.
    pub fn insert(&mut self, key: CpuidKey, entry: CpuidEntry) {
        cpuid_insert(&mut self.0, key, entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get() {
        let cpuid = IntelCpuid(kvm_bindings::CpuId::from_entries(&[]).unwrap());
        assert_eq!(
            cpuid.get(&CpuidKey {
                leaf: 0,
                subleaf: 0
            }),
            None
        );
    }

    #[test]
    fn get_mut() {
        let mut cpuid = IntelCpuid(kvm_bindings::CpuId::from_entries(&[]).unwrap());
        assert_eq!(
            cpuid.get_mut(&CpuidKey {
                leaf: 0,
                subleaf: 0
            }),
            None
        );
    }
}
