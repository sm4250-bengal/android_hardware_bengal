// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Linux `/dev/i2c-N` adapter using `I2C_RDWR` only.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;

use super::I2cBus;

/// `I2C_RDWR` from `include/uapi/linux/i2c-dev.h`.
pub const I2C_RDWR: u64 = 0x0707;
const I2C_M_RD: u16 = 0x0001;

#[repr(C)]
#[derive(Debug)]
struct I2cMsg {
    addr: u16,
    flags: u16,
    len: u16,
    _padding: u16,
    buf: *mut u8,
}

#[repr(C)]
#[derive(Debug)]
struct I2cRdwrIoctlData {
    msgs: *mut I2cMsg,
    nmsgs: u32,
    _padding: u32,
}

extern "C" {
    fn ioctl(fd: RawFd, request: u64, ...) -> i32;
}

/// Open Linux I2C character device.
#[derive(Debug)]
pub struct LinuxI2c {
    file: File,
}

impl LinuxI2c {
    /// Open a path returned by adapter-name resolution, never a hard-coded bus.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self { file })
    }

    fn transfer(&mut self, messages: &mut [I2cMsg]) -> io::Result<()> {
        let mut data = I2cRdwrIoctlData {
            msgs: messages.as_mut_ptr(),
            nmsgs: messages.len() as u32,
            _padding: 0,
        };
        // SAFETY: `data` and `messages` remain alive for the ioctl. Each
        // message buffer is owned by the caller and remains alive for this
        // synchronous call. The structures are `repr(C)` UAPI layouts.
        let rc = unsafe { ioctl(self.file.as_raw_fd(), I2C_RDWR, &mut data) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else if rc as usize != messages.len() {
            Err(io::Error::other(format!(
                "I2C_RDWR completed {rc} of {} messages",
                messages.len()
            )))
        } else {
            Ok(())
        }
    }
}

impl I2cBus for LinuxI2c {
    type Error = io::Error;

    fn write(&mut self, address: u16, bytes: &[u8]) -> Result<(), Self::Error> {
        let mut messages = [I2cMsg {
            addr: address,
            flags: 0,
            len: checked_len(bytes.len())?,
            _padding: 0,
            buf: bytes.as_ptr().cast_mut(),
        }];
        self.transfer(&mut messages)
    }

    fn write_read(
        &mut self,
        address: u16,
        write: &[u8],
        read: &mut [u8],
    ) -> Result<(), Self::Error> {
        let mut messages = [
            I2cMsg {
                addr: address,
                flags: 0,
                len: checked_len(write.len())?,
                _padding: 0,
                buf: write.as_ptr().cast_mut(),
            },
            I2cMsg {
                addr: address,
                flags: I2C_M_RD,
                len: checked_len(read.len())?,
                _padding: 0,
                buf: read.as_mut_ptr(),
            },
        ];
        self.transfer(&mut messages)
    }
}

fn checked_len(len: usize) -> io::Result<u16> {
    u16::try_from(len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("I2C message is {len} bytes; u16 length overflow"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uapi_layout_matches_lp64_linux() {
        assert_eq!(std::mem::size_of::<I2cMsg>(), 16);
        assert_eq!(std::mem::size_of::<I2cRdwrIoctlData>(), 16);
    }

    #[test]
    fn rejects_a_message_larger_than_the_uapi_field() {
        assert!(checked_len(u16::MAX as usize + 1).is_err());
    }
}
