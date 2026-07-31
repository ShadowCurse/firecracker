// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;

use vm_memory::bitmap::Bitmap;
use vm_memory::{GuestMemoryBackend, GuestMemoryError};

use crate::vstate::memory::{GuestAddress, GuestMemoryMmap};

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum SyncIoError {
    /// Flush: {0}
    Flush(std::io::Error),
    /// Seek: {0}
    Seek(std::io::Error),
    /// SyncAll: {0}
    SyncAll(std::io::Error),
    /// Transfer: {0}
    Transfer(GuestMemoryError),
    /// Read/write: {0}
    Io(std::io::Error),
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
        &self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<u32, SyncIoError> {
        let slice = mem
            .get_slice(addr, count as usize)
            .map_err(SyncIoError::Transfer)?;
        let guard = slice.ptr_guard_mut();
        // SAFETY: `guard` holds the slice mapped and the pointer valid for `count` bytes.
        // `pread` writes into that region and does not modify any shared kernel state on
        // `self.file` (the offset lives on the stack).
        let ret = unsafe {
            libc::pread(
                self.file.as_raw_fd(),
                guard.as_ptr().cast::<libc::c_void>(),
                count as usize,
                offset.cast_signed(),
            )
        };
        if ret < 0 {
            return Err(SyncIoError::Io(io::Error::last_os_error()));
        }
        let n = ret.cast_unsigned() as usize;
        if n < count as usize {
            return Err(SyncIoError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short pread",
            )));
        }
        // Mark the guest-memory region we just wrote to as dirty for snapshot tracking.
        slice.bitmap().mark_dirty(0, n);
        Ok(count)
    }

    pub fn write(
        &self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<u32, SyncIoError> {
        let slice = mem
            .get_slice(addr, count as usize)
            .map_err(SyncIoError::Transfer)?;
        let guard = slice.ptr_guard();
        // SAFETY: see `read()`. `pwrite` reads from the guest slice; `guard` keeps it valid.
        let ret = unsafe {
            libc::pwrite(
                self.file.as_raw_fd(),
                guard.as_ptr().cast::<libc::c_void>(),
                count as usize,
                offset.cast_signed(),
            )
        };
        if ret < 0 {
            return Err(SyncIoError::Io(io::Error::last_os_error()));
        }
        let n = ret.cast_unsigned() as usize;
        if n < count as usize {
            return Err(SyncIoError::Io(io::Error::new(
                io::ErrorKind::WriteZero,
                "short pwrite",
            )));
        }
        Ok(count)
    }

    pub fn flush(&self) -> Result<(), SyncIoError> {
        // SAFETY: `fsync` on a valid, open fd; touches no user-mode state.
        let ret = unsafe { libc::fsync(self.file.as_raw_fd()) };
        if ret < 0 {
            return Err(SyncIoError::SyncAll(io::Error::last_os_error()));
        }
        Ok(())
    }
}
