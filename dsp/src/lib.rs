// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Read-only ADAU1701 parameter-RAM core.
//!
//! V1 contains no hardware write operation. This library owns the typed bus
//! seam, bounds, cadence and Binder-independent service semantics. The sole D18
//! process, `bengal-hud`, supplies the resolved Linux transport and Binder
//! adapter so no second process can race the guard's global ADAU arm token.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

pub mod adau1701;
pub mod service;
