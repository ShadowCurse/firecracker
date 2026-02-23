// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;

use vm_memory::GuestMemoryError;
use vm_memory::bitmap::Bitmap;

use crate::vstate::memory::{GuestAddress, GuestMemory, GuestMemoryMmap};

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum SyncIoError {
    /// Flush: {0}
    Flush(std::io::Error),
    /// SyncAll: {0}
    SyncAll(std::io::Error),
    /// Transfer: {0}
    Transfer(GuestMemoryError),
    /// Pread/Pwrite: {0}
    PreadPwrite(std::io::Error),
}

#[derive(Debug)]
pub struct SyncFileEngine {
    file: File,
}

// SAFETY: `File` is send and ultimately a POD.
unsafe impl Send for SyncFileEngine {}

impl SyncFileEngine {
    pub fn from_file(file: File) -> SyncFileEngine {
        SyncFileEngine { file }
    }

    #[cfg(test)]
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Update the backing file of the engine
    pub fn update_file(&mut self, file: File) {
        self.file = file
    }

    pub fn read(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<u32, SyncIoError> {
        let slice = mem
            .get_slice(addr, count as usize)
            .map_err(SyncIoError::Transfer)?;
        let guard = slice.ptr_guard_mut();
        // SAFETY: The VolatileSlice guarantees the memory at `guard.as_ptr()` of length
        // `guard.len()` is valid, mapped, and not aliased by any Rust references.
        let buf = unsafe { std::slice::from_raw_parts_mut(guard.as_ptr(), guard.len()) };

        let mut bytes_done = 0usize;
        while bytes_done < buf.len() {
            match self.file.read_at(&mut buf[bytes_done..], offset + bytes_done as u64) {
                Ok(0) => {
                    return Err(SyncIoError::PreadPwrite(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    )));
                }
                Ok(n) => bytes_done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(SyncIoError::PreadPwrite(e)),
            }
        }

        slice.bitmap().mark_dirty(0, count as usize);
        Ok(count)
    }

    pub fn write(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<u32, SyncIoError> {
        let slice = mem
            .get_slice(addr, count as usize)
            .map_err(SyncIoError::Transfer)?;
        let guard = slice.ptr_guard();
        // SAFETY: The VolatileSlice guarantees the memory at `guard.as_ptr()` of length
        // `guard.len()` is valid and mapped.
        let buf = unsafe { std::slice::from_raw_parts(guard.as_ptr(), guard.len()) };

        let mut bytes_done = 0usize;
        while bytes_done < buf.len() {
            match self.file.write_at(&buf[bytes_done..], offset + bytes_done as u64) {
                Ok(0) => {
                    return Err(SyncIoError::PreadPwrite(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "failed to write whole buffer",
                    )));
                }
                Ok(n) => bytes_done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(SyncIoError::PreadPwrite(e)),
            }
        }

        Ok(count)
    }

    pub fn flush(&mut self) -> Result<(), SyncIoError> {
        // flush() first to force any cached data out of rust buffers.
        self.file.flush().map_err(SyncIoError::Flush)?;
        // Sync data out to physical media on host.
        self.file.sync_all().map_err(SyncIoError::SyncAll)
    }
}
