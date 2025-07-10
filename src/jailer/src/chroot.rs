// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::ffi::{CStr, CString};
use std::path::Path;
use std::ptr::null;

use vmm_sys_util::syscall::SyscallReturnCode;

use crate::env::Env;

use super::{JailerError, to_cstring};

const OLD_ROOT_DIR: &CStr = c"old_root";
const ROOT_DIR: &CStr = c"/";
const CURRENT_DIR: &CStr = c".";

pub fn print_caps() {
    use caps::CapSet;

    let cur = caps::read(None, CapSet::Permitted).unwrap();
    eprintln!("Current permitted caps: {:?}.", cur);

    // Retrieve effective set.
    let cur = caps::read(None, CapSet::Effective).unwrap();
    eprintln!("Current effective caps: {:?}.", cur);
}

// This uses switching to a new mount namespace + pivot_root(), together with the regular chroot,
// to provide a hardened jail (at least compared to only relying on chroot).
pub fn chroot(env: &Env) -> Result<(), JailerError> {
    if true {
        let current_uid = unsafe { libc::getuid() };
        let current_gid = unsafe { libc::getgid() };

        let new_uid = env.uid;
        let new_gid = env.gid;

        // eprintln!("before uid: {}", uid);
        // eprintln!("before gid: {}", gid);
        //
        // eprintln!("before unshare");
        // print_caps();

        // We unshare into a new mount namespace.
        // SAFETY: The call is safe because we're invoking a C library
        // function with valid parameters.
        SyscallReturnCode(unsafe { libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWUSER) })
            .into_empty_result()
            .map_err(JailerError::UnshareNewNs)?;

        // eprintln!("after unshare");
        // print_caps();

        // In order to remain root we do this
        unsafe {
            let uid_map = libc::open(c"/proc/self/uid_map".as_ptr(), libc::O_WRONLY);
            assert!(0 < uid_map, "cannont open uid_map");
            let uid_info = format!("{} {} 1", new_uid, current_uid);
            let a = libc::write(uid_map, uid_info.as_ptr().cast(), uid_info.len());
            assert!(0 < a, "cannot write to uid_map");
            libc::close(uid_map);

            let setgroups = libc::open(c"/proc/self/setgroups".as_ptr(), libc::O_WRONLY);
            assert!(0 < setgroups, "cannont open setgroups");
            let setgroups_info = "deny";
            let a = libc::write(
                uid_map,
                setgroups_info.as_ptr().cast(),
                setgroups_info.len(),
            );
            assert!(0 < a, "cannot write to setgroups");
            libc::close(setgroups);

            let gid_map = libc::open(c"/proc/self/gid_map".as_ptr(), libc::O_WRONLY);
            assert!(0 < gid_map, "cannont open gid_map");
            let gid_info = format!("{} {} 1", new_gid, current_gid);
            let a = libc::write(uid_map, gid_info.as_ptr().cast(), gid_info.len());
            assert!(0 < a, "cannot write to gid_map");
            libc::close(gid_map);
        }

        // let uid = unsafe { libc::getuid() };
        // let gid = unsafe { libc::getgid() };

        // eprintln!("after uid: {}", uid);
        // eprintln!("after gid: {}", gid);
    } else {
        SyscallReturnCode(unsafe { libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWUSER) })
            .into_empty_result()
            .map_err(JailerError::UnshareNewNs)?;
    }

    // We need a CString for the following mount call.
    let chroot_path_c = CString::new(env.chroot_dir.to_str().unwrap()).unwrap();

    unsafe { libc::mount(null(), ROOT_DIR.as_ptr(), null(), libc::MS_PRIVATE, null()) };

    // Bind mount the jail root directory over itself, so we can go around a restriction
    // imposed by pivot_root, which states that the new root and the old root should not
    // be on the same filesystem.
    // SAFETY: Safe because we provide valid parameters.
    SyscallReturnCode(unsafe {
        libc::mount(
            chroot_path_c.as_ptr(),
            chroot_path_c.as_ptr(),
            null(),
            libc::MS_BIND,
            null(),
        )
    })
    .into_empty_result()
    .map_err(JailerError::MountBind)?;

    // Change current dir to the chroot dir, so we only need to handle relative paths from now on.
    SyscallReturnCode(unsafe { libc::chdir(chroot_path_c.as_ptr()) })
        .into_empty_result()
        .map_err(JailerError::SetCurrentDir)?;

    // Create the old_root folder we're going to use for pivot_root, using a relative path.
    // SAFETY: The call is safe because we provide valid arguments.
    SyscallReturnCode(unsafe { libc::mkdir(OLD_ROOT_DIR.as_ptr(), libc::S_IRUSR | libc::S_IWUSR) })
        .into_empty_result()
        .map_err(JailerError::MkdirOldRoot)?;

    // We are now ready to call pivot_root. We have to use sys_call because there is no libc
    // wrapper for pivot_root.
    // SAFETY: Safe because we provide valid parameters.
    SyscallReturnCode(unsafe {
        libc::syscall(
            libc::SYS_pivot_root,
            CURRENT_DIR.as_ptr(),
            OLD_ROOT_DIR.as_ptr(),
        )
    })
    .into_empty_result()
    .map_err(JailerError::PivotRoot)?;

    // pivot_root doesn't guarantee that we will be in "/" at this point, so switch to "/"
    // explicitly.
    // SAFETY: Safe because we provide valid parameters.
    SyscallReturnCode(unsafe { libc::chdir(ROOT_DIR.as_ptr()) })
        .into_empty_result()
        .map_err(JailerError::ChdirNewRoot)?;

    // Umount the old_root, thus isolating the process from everything outside the jail root folder.
    // SAFETY: Safe because we provide valid parameters.
    SyscallReturnCode(unsafe { libc::umount2(OLD_ROOT_DIR.as_ptr(), libc::MNT_DETACH) })
        .into_empty_result()
        .map_err(JailerError::UmountOldRoot)?;

    // Remove the no longer necessary old_root directory.
    // SAFETY: Safe because we provide valid parameters.
    SyscallReturnCode(unsafe { libc::rmdir(OLD_ROOT_DIR.as_ptr()) })
        .into_empty_result()
        .map_err(JailerError::RmOldRootDir)
}
