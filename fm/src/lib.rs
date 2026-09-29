// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! QN8035 hardware core and Binder-independent FM service facade.
//!
//! This is a library for the D18 `bengal-hud` process, not a second daemon.
//! Keeping Binder out of the core makes register encoding, seek policy and RDS
//! assembly runnable on the host with no Android tree and no network.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

pub mod i2c;
pub mod qn8035;
pub mod service;

/// Android-only generated-AIDL adapter and service-manager registration.
#[cfg(feature = "binder")]
pub mod binder;
