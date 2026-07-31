// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;

use vm_memory::bitmap::Bitmap;
use vm_memory::{GuestMemoryBackend, GuestMemoryError};

use crate::vstate::memory::{GuestAddress, GuestMemoryMmap};

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum SyncIoError {
    /// Discard: {0}
    Discard(std::io::Error),
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

const BLKDISCARD: libc::Ioctl = 0x1277;
const FALLOC_FL_KEEP_SIZE: libc::c_int = 0x01;
const FALLOC_FL_PUNCH_HOLE: libc::c_int = 0x02;

pub(super) fn file_discard_range(file: &File, range: (u64, u32)) -> Result<u32, std::io::Error> {
    let file_type = file.metadata()?.file_type();
    let (offset, len) = range;

    if len == 0 {
        return Ok(0);
    }
    let discarded = len;
    let len = u64::from(len);

    if file_type.is_block_device() {
        let mut range = [offset, len];
        // SAFETY: file is a valid fd, BLKDISCARD expects a pointer to two u64 values
        // representing byte offset and byte length.
        let ret = unsafe { libc::ioctl(file.as_raw_fd(), BLKDISCARD, range.as_mut_ptr()) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
    } else {
        let off = libc::off_t::try_from(offset).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "discard offset overflow")
        })?;
        let len = libc::off_t::try_from(len).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "discard length overflow")
        })?;
        // SAFETY: file is a valid fd and fallocate does not retain the passed values.
        let ret = unsafe {
            libc::fallocate(
                file.as_raw_fd(),
                FALLOC_FL_KEEP_SIZE | FALLOC_FL_PUNCH_HOLE,
                off,
                len,
            )
        };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    Ok(discarded)
}

impl SyncFileEngine {
    pub fn from_file(file: File) -> SyncFileEngine {
        SyncFileEngine { file }
    }

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

    pub fn discard(&mut self, range: (u64, u32)) -> Result<u32, SyncIoError> {
        file_discard_range(&self.file, range).map_err(SyncIoError::Discard)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};

    use vmm_sys_util::tempfile::TempFile;

    use super::file_discard_range;

    #[test]
    fn test_discard_regular_file() {
        let mut file = TempFile::new().unwrap().into_file();
        file.write_all(&vec![0x5a; 4096]).unwrap();

        match file_discard_range(&file, (1024, 2048)) {
            Ok(discarded) => assert_eq!(discarded, 2048),
            // Some filesystems do not support hole punching; that is a host/filesystem
            // capability limitation, not a test failure for the regular discard path.
            Err(err)
                if matches!(
                    err.raw_os_error(),
                    Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS)
                ) =>
            {
                return;
            }
            Err(err) => panic!("discard failed: {err}"),
        }

        file.seek(SeekFrom::Start(0)).unwrap();
        let mut data = vec![0u8; 4096];
        file.read_exact(&mut data).unwrap();

        assert_eq!(&data[..1024], vec![0x5a; 1024].as_slice());
        assert_eq!(&data[1024..3072], vec![0; 2048].as_slice());
        assert_eq!(&data[3072..], vec![0x5a; 1024].as_slice());
    }
}
