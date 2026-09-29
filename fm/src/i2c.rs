// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! I2C port used by the tuner model.
//!
//! Every operation carries the 7-bit address. There is deliberately no
//! `set_slave` method: device 1 proved that `I2C_RDWR` can coexist with the
//! vendor driver while `I2C_SLAVE` returns `EBUSY` and leaves per-fd state.

#[cfg(feature = "linux-i2c")]
pub mod linux;

/// Minimal transaction surface needed by the QN8035.
pub trait I2cBus {
    /// Adapter-specific error.
    type Error;

    /// Write one message to `address`.
    fn write(&mut self, address: u16, bytes: &[u8]) -> Result<(), Self::Error>;

    /// Write a register pointer, then read without an intervening STOP.
    fn write_read(
        &mut self,
        address: u16,
        write: &[u8],
        read: &mut [u8],
    ) -> Result<(), Self::Error>;
}
