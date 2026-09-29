// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! `I2C_RDWR` combined transfers — and nothing else.
//!
//! The design goal here is that the part which can be *silently* wrong is a
//! pure function with tests, and the `unsafe` part is three lines with nothing
//! to get wrong. Encoding a `struct i2c_msg` array with a bad offset produces
//! a transfer to the wrong address, or a read into the wrong buffer, and the
//! kernel returns success — so [`encode`] is where the tests live and
//! [`Bus::transfer`] is a shim.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;

/// `I2C_RDWR` — combined R/W transfer, one STOP only.
/// `uapi/linux/i2c-dev.h:35`.
///
/// The only ioctl this crate issues, and the only one sepolicy grants
/// (`allowxperm ... ioctl { 0x0707 }`). `I2C_SLAVE` (0x0703) is deliberately
/// absent: it takes persistent per-fd address state and returns `EBUSY` when a
/// driver owns the address. Note for anyone extending this — `I2C_FUNCS` is
/// **0x0705** and **0x0706** is `I2C_SLAVE_FORCE`; those two are easy to swap
/// and the mistake is silent.
pub const I2C_RDWR: u64 = 0x0707;

/// `I2C_M_RD` — "guaranteed to be 0x0001", says `uapi/linux/i2c.h:77`.
const I2C_M_RD: u16 = 0x0001;

/// The kernel caps one `I2C_RDWR` at this many messages
/// (`I2C_RDWR_IOCTL_MAX_MSGS`, `i2c-dev.h:55`).
pub const MAX_MSGS: usize = 42;

/// One leg of a combined transfer.
#[derive(Debug)]
pub enum Message<'a> {
    /// Write these bytes to `address`.
    Write {
        /// 7-bit address.
        address: u16,
        /// Payload.
        data: &'a [u8],
    },
    /// Read `into.len()` bytes from `address`.
    Read {
        /// 7-bit address.
        address: u16,
        /// Destination.
        into: &'a mut [u8],
    },
}

/// `struct i2c_msg` (`uapi/linux/i2c.h:74-88`).
///
/// ```c
/// struct i2c_msg {
///     __u16 addr;
///     __u16 flags;
///     __u16 len;
///     __u8 *buf;
/// };
/// ```
///
/// On every LP64 target — aarch64 (the unit) and x86_64 (this desk) — that is
/// 2 + 2 + 2 + **2 bytes of padding** + 8 = 16 bytes. The padding is the trap:
/// the device-1 Go port needed an explicit `_ uint16` filler for exactly this
/// reason. `#[repr(C)]` gets it right for us, and
/// [`tests::i2c_msg_layout_matches_the_uapi_struct`] asserts it rather than
/// trusting that.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct I2cMsg {
    addr: u16,
    flags: u16,
    len: u16,
    _pad: u16,
    buf: u64,
}

/// `struct i2c_rdwr_ioctl_data` (`uapi/linux/i2c-dev.h:50-53`).
#[repr(C)]
#[derive(Debug)]
struct I2cRdwrIoctlData {
    msgs: u64,
    nmsgs: u32,
    _pad: u32,
}

/// Why a transfer could not be built or issued.
#[derive(Debug)]
pub enum TransferError {
    /// More than [`MAX_MSGS`] legs.
    TooManyMessages(usize),
    /// A leg longer than `u16::MAX`, which `i2c_msg.len` cannot express.
    MessageTooLong(usize),
    /// No legs at all.
    Empty,
    /// The ioctl failed.
    Io(io::Error),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransferError::TooManyMessages(n) => {
                write!(f, "{} messages, kernel maximum is {}", n, MAX_MSGS)
            }
            TransferError::MessageTooLong(n) => {
                write!(f, "message of {} bytes exceeds i2c_msg.len (u16)", n)
            }
            TransferError::Empty => write!(f, "no messages"),
            TransferError::Io(e) => write!(f, "I2C_RDWR: {}", e),
        }
    }
}

impl std::error::Error for TransferError {}

/// Build the `i2c_msg` array for `messages`, without issuing anything.
///
/// Split out of [`Bus::transfer`] so it can be tested: this is the part where a
/// wrong offset produces a transfer to the wrong address that the kernel
/// happily reports as success.
///
/// The returned `Vec` borrows the message buffers, so it must not outlive
/// them — [`Bus::transfer`] keeps both alive across the ioctl.
fn encode(messages: &mut [Message<'_>]) -> Result<Vec<I2cMsg>, TransferError> {
    if messages.is_empty() {
        return Err(TransferError::Empty);
    }
    if messages.len() > MAX_MSGS {
        return Err(TransferError::TooManyMessages(messages.len()));
    }
    let mut out = Vec::with_capacity(messages.len());
    for m in messages.iter_mut() {
        let (addr, flags, len, ptr) = match m {
            Message::Write { address, data } => {
                if data.len() > u16::MAX as usize {
                    return Err(TransferError::MessageTooLong(data.len()));
                }
                (*address, 0u16, data.len() as u16, data.as_ptr() as u64)
            }
            Message::Read { address, into } => {
                if into.len() > u16::MAX as usize {
                    return Err(TransferError::MessageTooLong(into.len()));
                }
                (
                    *address,
                    I2C_M_RD,
                    into.len() as u16,
                    into.as_mut_ptr() as u64,
                )
            }
        };
        out.push(I2cMsg {
            addr,
            flags,
            len,
            _pad: 0,
            buf: ptr,
        });
    }
    Ok(out)
}

// The libc ioctl. Declared here rather than pulled in as a crate so that this
// whole library builds with `cargo build --offline` and no dependencies at all;
// Soong links libc regardless.
extern "C" {
    fn ioctl(fd: RawFd, request: u64, ...) -> i32;
}

/// An open I2C adapter.
///
/// Owns the fd. Holds no address state — every message carries its own, which
/// is the property that lets this coexist with a kernel driver on the same bus
/// (`mem:qn8035-direct-i2c`).
#[derive(Debug)]
pub struct Bus {
    file: File,
    path: String,
}

impl Bus {
    /// Open `/dev/i2c-N`.
    ///
    /// Take the path from [`super::adapter::resolve`], never from a constant.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Bus {
            file,
            path: path.display().to_string(),
        })
    }

    /// The adapter this bus was opened on, for logs and `TunerState`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Issue one combined transfer.
    ///
    /// Reads land in the `into` slices of the `Read` legs.
    pub fn transfer(&mut self, messages: &mut [Message<'_>]) -> Result<(), TransferError> {
        let mut msgs = encode(messages)?;
        let data = I2cRdwrIoctlData {
            msgs: msgs.as_mut_ptr() as u64,
            nmsgs: msgs.len() as u32,
            _pad: 0,
        };
        // SAFETY: `data.msgs` points at `msgs`, which lives until the end of
        // this function; each I2cMsg.buf points into `messages`, which the
        // caller owns for the whole call; nmsgs is msgs.len(). The kernel
        // writes only into the Read legs' buffers, which are &mut.
        let rc = unsafe { ioctl(self.file.as_raw_fd(), I2C_RDWR, &data as *const _) };
        if rc < 0 {
            return Err(TransferError::Io(io::Error::last_os_error()));
        }
        // Keep the borrow alive until after the ioctl returns.
        drop(msgs);
        Ok(())
    }

    /// Read `len` bytes starting at `register`: the standard
    /// write-subaddress-then-read combined transaction, one STOP.
    pub fn read_register(
        &mut self,
        address: u16,
        register: u8,
        len: usize,
    ) -> Result<Vec<u8>, TransferError> {
        let reg = [register];
        let mut buf = vec![0u8; len];
        {
            let mut msgs = [
                Message::Write {
                    address,
                    data: &reg,
                },
                Message::Read {
                    address,
                    into: &mut buf,
                },
            ];
            self.transfer(&mut msgs)?;
        }
        Ok(buf)
    }

    /// Read one register.
    pub fn read_byte(&mut self, address: u16, register: u8) -> Result<u8, TransferError> {
        Ok(self.read_register(address, register, 1)?[0])
    }

    /// Write one register.
    pub fn write_byte(
        &mut self,
        address: u16,
        register: u8,
        value: u8,
    ) -> Result<(), TransferError> {
        let payload = [register, value];
        let mut msgs = [Message::Write {
            address,
            data: &payload,
        }];
        self.transfer(&mut msgs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn i2c_msg_layout_matches_the_uapi_struct() {
        // 2 + 2 + 2 + 2 pad + 8 on every LP64 target. If this ever fails, every
        // transfer this crate issues is addressing the wrong thing.
        assert_eq!(size_of::<I2cMsg>(), 16);
        assert_eq!(size_of::<I2cRdwrIoctlData>(), 16);
        assert_eq!(size_of::<u64>(), 8, "this code assumes an LP64 target");
    }

    #[test]
    fn a_register_read_is_write_then_read_with_the_same_address() {
        let reg = [0x06u8];
        let mut buf = [0u8; 1];
        let mut msgs = [
            Message::Write {
                address: 0x10,
                data: &reg,
            },
            Message::Read {
                address: 0x10,
                into: &mut buf,
            },
        ];
        let encoded = encode(&mut msgs).unwrap();
        assert_eq!(encoded.len(), 2);

        // leg 0: write one byte, no flags
        assert_eq!(encoded[0].addr, 0x10);
        assert_eq!(encoded[0].flags, 0);
        assert_eq!(encoded[0].len, 1);
        assert_ne!(encoded[0].buf, 0);

        // leg 1: read one byte, I2C_M_RD
        assert_eq!(encoded[1].addr, 0x10);
        assert_eq!(encoded[1].flags, I2C_M_RD);
        assert_eq!(encoded[1].len, 1);
        assert_ne!(encoded[1].buf, 0);

        // the two legs must NOT share a buffer
        assert_ne!(encoded[0].buf, encoded[1].buf);
    }

    #[test]
    fn every_leg_carries_its_own_address() {
        // This is why I2C_SLAVE is unnecessary, and the test states it: a
        // transfer can address two chips and the fd holds no address state.
        let a = [0x00u8];
        let b = [0x01u8];
        let mut msgs = [
            Message::Write {
                address: 0x10,
                data: &a,
            },
            Message::Write {
                address: 0x34,
                data: &b,
            },
        ];
        let encoded = encode(&mut msgs).unwrap();
        assert_eq!(encoded[0].addr, 0x10);
        assert_eq!(encoded[1].addr, 0x34);
    }

    #[test]
    fn read_flag_is_exactly_one() {
        // uapi/linux/i2c.h:77 -- "guaranteed to be 0x0001". Asserted because a
        // wrong flag turns a read into a write of whatever was in the buffer.
        assert_eq!(I2C_M_RD, 1);
    }

    #[test]
    fn rejects_empty_and_oversized_transfers() {
        let mut none: [Message<'_>; 0] = [];
        assert!(matches!(encode(&mut none), Err(TransferError::Empty)));

        let payload = [0u8; 1];
        let mut many: Vec<Message<'_>> = (0..MAX_MSGS + 1)
            .map(|_| Message::Write {
                address: 0x10,
                data: &payload,
            })
            .collect();
        assert!(matches!(
            encode(&mut many),
            Err(TransferError::TooManyMessages(43))
        ));
    }

    #[test]
    fn the_only_ioctl_is_the_one_sepolicy_grants() {
        // Belt and braces with phase4d_types.te's allowxperm.
        assert_eq!(I2C_RDWR, 0x0707);
    }
}
