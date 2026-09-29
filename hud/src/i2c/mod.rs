// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! I2C: resolve an adapter by what it *is*, then talk to it with `I2C_RDWR`.
//!
//! Two rules, both measured, both cheap to get wrong:
//!
//! 1. **Never address a bus by its number.** `/dev/i2c-N` numbering is
//!    allocation order. On this board it has already moved between two of our
//!    own boots: on D3-0015 the FM bus was `i2c-0` (Goodix landed at `3-005d`),
//!    on D3-0043 it was `i2c-1` (Goodix at `0-005d`). A wrong adapter does not
//!    fail loudly — it addresses 0x10 on some other bus and reports the NAK as
//!    "the tuner is missing", or finds a different chip and talks to it.
//!    [`adapter`] resolves by adapter name and device-tree node instead, both
//!    of which are board facts. See
//!    `docs/research/2026-08-30-fm-4d-prep.md` §1.2.
//!
//! 2. **Only `I2C_RDWR`, never `I2C_SLAVE`.** `I2C_SLAVE` sets persistent
//!    per-fd address state and returns `EBUSY` when a driver owns the address;
//!    `I2C_RDWR` carries the address in every message and needs neither
//!    (`mem:qn8035-direct-i2c`). The sepolicy `allowxperm` in
//!    `platform/device/xyauto/bengal3/sepolicy/vendor/phase4d_types.te` grants
//!    ioctl `0x0707` and nothing else, so this is enforced from two sides.
//!
//! Neither of these needs `/dev/i2c-*` to exist to be *tested*: [`adapter`]
//! walks a sysfs root that the tests fake, and [`rdwr`] splits the ioctl
//! payload encoding — the part that can be silently wrong — out of the
//! `unsafe` call.

pub mod adapter;
pub mod rdwr;

pub use adapter::{resolve, Adapter, MatchedBy};
pub use rdwr::{Bus, Message, I2C_RDWR};

/// The QN8035 FM tuner's 7-bit address (`mem:qn8035-direct-i2c`).
pub const QN8035_ADDRESS: u16 = 0x10;

/// The ADAU1701 DSP's 7-bit address. Its bus (`i2c-dsp`, TLMM 29/30) is
/// `status = "disabled"` in our board DT until Phase 4c.
pub const ADAU1701_ADDRESS: u16 = 0x34;
