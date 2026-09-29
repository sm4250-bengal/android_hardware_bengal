// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Binder-startup ordering that remains host-testable without Binder.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Held for the complete hardware-owner lifetime, automatically released on exit.
pub struct DaemonOwner {
    _file: File,
}

/// Acquire a nonblocking process lock before registering Binder or opening hardware.
/// The caller supplies an init-created private directory. Do not unlink the file.
pub fn acquire_daemon_owner(path: &Path) -> io::Result<DaemonOwner> {
    const O_NOFOLLOW: i32 = 0x20000;
    const O_NONBLOCK: i32 = 0x800;
    const LOCK_EX_NB: i32 = 2 | 4;
    extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "owner lock is not a regular file",
        ));
    }
    // SAFETY: flock only consumes this live file descriptor and fixed flags;
    // it receives no pointers. The FD stays owned by DaemonOwner on success.
    if unsafe { flock(file.as_raw_fd(), LOCK_EX_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(DaemonOwner { _file: file })
}

/// Attempt FM registration, then bring up HUD and MCU capture.
///
/// FM is advertised before Phase 4c can safely operate its hardware.  A
/// service-manager failure registering that inert facade is reported, but must
/// not make the independent HUD service or its MCU reader unavailable.  HUD
/// registration remains fatal because this is the daemon that owns it.
pub fn register_services_and_start_mcu<E>(
    register_fm: impl FnOnce() -> Result<(), E>,
    register_hud: impl FnOnce() -> Result<(), E>,
    start_mcu_reader: impl FnOnce(),
    log_fm_registration_failure: impl FnOnce(&E),
) -> Result<(), E> {
    if let Err(error) = register_fm() {
        log_fm_registration_failure(&error);
    }
    register_hud()?;
    start_mcu_reader();
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::{acquire_daemon_owner, register_services_and_start_mcu};

    #[test]
    fn owner_lock_excludes_duplicates_releases_on_exit_and_refuses_symlinks() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("hud-owner-{}-{unique}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("daemon.lock");
        let owner = acquire_daemon_owner(&path).unwrap();
        assert!(matches!(acquire_daemon_owner(&path),
                         Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));
        drop(owner);
        assert!(path.is_file(), "unlock must preserve the inode");
        drop(acquire_daemon_owner(&path).unwrap());
        let link = dir.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(acquire_daemon_owner(&link).is_err());
        assert!(acquire_daemon_owner(&dir).is_err());
        std::fs::remove_file(link).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn forced_fm_registration_failure_still_registers_hud_and_starts_mcu() {
        let fm_failure_logged = Cell::new(false);
        let hud_registered = Cell::new(false);
        let mcu_started = Cell::new(false);

        register_services_and_start_mcu(
            || Err::<(), _>("forced FM registration failure"),
            || {
                hud_registered.set(true);
                Ok(())
            },
            || mcu_started.set(true),
            |error| {
                assert_eq!(error, &"forced FM registration failure");
                fm_failure_logged.set(true);
            },
        )
        .unwrap();

        assert!(fm_failure_logged.get());
        assert!(hud_registered.get());
        assert!(mcu_started.get());
    }

    #[test]
    fn hud_registration_failure_does_not_start_mcu() {
        let mcu_started = Cell::new(false);

        let error = register_services_and_start_mcu(
            || Ok::<(), &str>(()),
            || Err("forced HUD registration failure"),
            || mcu_started.set(true),
            |_| unreachable!("FM registration succeeded"),
        )
        .unwrap_err();

        assert_eq!(error, "forced HUD registration failure");
        assert!(!mcu_started.get());
    }
}
