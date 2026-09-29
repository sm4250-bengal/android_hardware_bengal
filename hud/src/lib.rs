// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! `bengal_hud` — the hardware core of the device-3 vendor daemon (D18).
//!
//! # Why Binder is feature-gated
//!
//! Everything in this crate builds and tests on a plain Linux host with
//! `cargo build --offline` / `cargo test --offline` and **zero dependencies** —
//! no `libc`, no `binder`, no network. That is deliberate and it is the whole
//! reason for keeping the service core separate from its Android adapter:
//!
//! * the parts that touch hardware are the parts most worth testing, and they
//!   are exactly the parts a `m` build cannot iterate on quickly;
//! * the binder/AIDL half needs `libbinder_rs` and the Soong-generated
//!   `vendor.xyauto.hud-V1-rust` crate, neither of which exists outside an
//!   Android build tree. The `binder` feature keeps those imports out of the
//!   default Cargo graph; its binary entry point is inert at the desk.
//!
//! Soong builds both: `rust_library` over `src/lib.rs`, `rust_binary` over
//! `src/main.rs` with this crate in `rustlibs`. See `Android.bp` and
//! `docs/research/2026-08-30-hud-daemon-skeleton.md`.
//!
//! # Why one daemon
//!
//! D18: *"Vendor daemon in Rust (`/vendor`), owning MCU UART, ADAU1701 DSP,
//! QN8035 tuner, backlight/GPIO; exposed as AIDL HAL(s) (`vendor.xyauto.*`)."*
//! One process, because these subsystems are not independent: selecting FM
//! means an MCU frame, then the analogue mux, then a tuner un-mute, in that
//! order (`fm::SOURCE_SELECT_SEQUENCE`). Two processes doing halves of that is
//! the failure mode `mem:dsp-arm-token-is-global` already cost this project
//! once, when two writers stole each other's one-shot DSP arm token.
//!
//! # Rust version
//!
//! Edition 2021, no external crates, nothing newer than Rust 1.85 std. The
//! Android build uses `prebuilts/rust/linux-x86/1.88.0`; this desk uses 1.98.
//! Keeping inside 1.85 means the two toolchains cannot disagree — and the
//! project has been bitten by exactly that class of drift before
//! (`mem:rust-198-breaks-kernel-build`, where a host rustc bump silently
//! changed a build the project depended on reproducing).

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

#[cfg(any(test, feature = "acc-power-channel"))]
pub mod acc_power;
/// Android-only typed physical-audio guard Binder adapter.
#[cfg(feature = "binder")]
pub mod audio_binder;
#[cfg(feature = "binder")]
pub mod can_binder;
pub mod bezel;
pub mod buttons;
pub mod callback_registry;
pub mod can;
pub mod cli;
pub mod cpu_latency;
pub mod dsp;
pub mod fm;
pub mod gpio;
pub mod i2c;
pub mod logsink;
pub mod mcu;
pub mod service;
pub mod startup;
pub mod usb;

/// Android-only libbinder_rs adapter and service registration.
#[cfg(feature = "binder")]
pub mod binder;

/// Where the daemon reads sysfs from.
///
/// A parameter, not a constant, for one reason: it makes every sysfs-walking
/// path testable against a fake tree on the host. The USB hub watchdog shipped
/// in Phase 4g learned this the hard way — its first version was untestable,
/// and when a harness was finally written it immediately found a bug that
/// would have power-cycled the log stick's own USB port.
///
/// `init` never sets it; on the unit it is always `/sys`.
pub const SYSFS_ROOT_ENV: &str = "XYAUTO_HUD_SYSFS_ROOT";

/// Resolve the sysfs root: `$XYAUTO_HUD_SYSFS_ROOT`, else `/sys`.
pub fn sysfs_root() -> std::path::PathBuf {
    match std::env::var_os(SYSFS_ROOT_ENV) {
        Some(v) if !v.is_empty() => std::path::PathBuf::from(v),
        _ => std::path::PathBuf::from("/sys"),
    }
}
