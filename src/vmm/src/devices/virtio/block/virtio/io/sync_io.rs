// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;

use vm_memory::GuestMemoryError;

use crate::vstate::memory::{GuestAddress, GuestMemory, GuestMemoryMmap};

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
}

#[derive(Debug)]
pub struct SyncFileEngine {
    // file: File,
    file_mem: &'static mut [u8],
}

// SAFETY: `File` is send and ultimately a POD.
unsafe impl Send for SyncFileEngine {}

impl SyncFileEngine {
    pub fn from_file(file: File) -> SyncFileEngine {
        let prot = libc::PROT_READ | libc::PROT_WRITE;
        let flags = libc::MAP_PRIVATE;
        let size = file.metadata().unwrap().size();

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                prot,
                flags,
                file.as_raw_fd(),
                0,
            )
        };
        let file_mem = unsafe { std::slice::from_raw_parts_mut(ptr.cast(), size as usize) };
        SyncFileEngine { file_mem }
    }

    // #[cfg(test)]
    // pub fn file(&self) -> &File {
    //     &self.file
    // }

    /// Update the backing file of the engine
    pub fn update_file(&mut self, file: File) {
        unsafe {
            libc::munmap(self.file_mem.as_mut_ptr().cast(), self.file_mem.len());
        };
        let prot = libc::PROT_READ | libc::PROT_WRITE;
        let flags = libc::MAP_PRIVATE;
        let size = file.metadata().unwrap().size();

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                prot,
                flags,
                file.as_raw_fd(),
                0,
            )
        };
        let file_mem = unsafe { std::slice::from_raw_parts_mut(ptr.cast(), size as usize) };
        self.file_mem = file_mem;
    }

    pub fn read(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<u32, SyncIoError> {
        let mem_slice = unsafe {
            std::slice::from_raw_parts_mut(
                mem.get_slice(addr, count as usize)
                    .unwrap()
                    .ptr_guard_mut()
                    .as_ptr(),
                count as usize,
            )
        };
        mem_slice
            .copy_from_slice(&self.file_mem[offset as usize..offset as usize + count as usize]);
        Ok(count)
    }

    pub fn write(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<u32, SyncIoError> {
        let mem_slice = unsafe {
            std::slice::from_raw_parts_mut(
                mem.get_slice(addr, count as usize)
                    .unwrap()
                    .ptr_guard_mut()
                    .as_ptr(),
                count as usize,
            )
        };
        self.file_mem[offset as usize..offset as usize + count as usize]
            .copy_from_slice(&mem_slice);
        Ok(count)
    }

    pub fn flush(&mut self) -> Result<(), SyncIoError> {
        unsafe {
            libc::msync(
                self.file_mem.as_mut_ptr().cast(),
                self.file_mem.len(),
                libc::MS_ASYNC,
            )
        };
        return Ok(());
    }
}
