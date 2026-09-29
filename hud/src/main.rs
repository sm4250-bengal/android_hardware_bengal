// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! `bengal-hud` — the device-3 vendor daemon entry point (D18).
//!
//! # Binder does not build at the desk, and that is the design
//!
//! The Android entry point needs `libbinder_rs` and
//! `vendor.xyauto.hud-V1-rust`; Soong generates the latter from the `.aidl` in
//! `platform/interfaces/vendor/xyauto/hud/`. `cargo build --offline` compiles
//! this file with an inert default-feature `main`, so it still tests everything
//! available without an Android tree and cannot reach Binder or board hardware.
//!
//! The `binder` feature is enabled only by Soong. It first attempts to register
//! the inert typed FM facade, then registers the typed HUD service and starts
//! MCU capture.  FM registration failure is logged but deliberately nonfatal;
//! default-feature Cargo builds retain an inert main and cannot open Binder or
//! board hardware.
//!
//! # Startup order
//!
//! init starts this `class hal`, `user system`, `oneshot` service (see
//! `bengal-hud.rc`). On start:
//!
//! 1. construct the sole USB owner without changing any output;
//! 2. attempt `vendor.xyauto.fm.IFmRadio/default` (nonfatal while phase 4c
//!    audio guard is unavailable);
//! 3. register `vendor.xyauto.hud.IHud/default`;
//! 4. start the sole MCU UART reader, whose path is an rc argument;
//! 5. join the Binder thread pool. Uinput remains property- and capture-gated.

fn main() -> std::process::ExitCode {
    bengal_hud::cli::execute(std::env::args_os().skip(1), run).into()
}

#[cfg(feature = "binder")]
fn run(mcu_device: Option<std::path::PathBuf>) -> Result<(), String> {
    // Keep the lock FD alive through all registration and hardware activity.
    // Never unlink it: another process could then lock a different inode.
    let _owner = bengal_hud::startup::acquire_daemon_owner(std::path::Path::new(
        "/data/vendor/bengal-hud/daemon.lock",
    ))
    .map_err(|error| format!("cannot acquire sole hardware ownership: {error}"))?;
    bengal_hud::binder::register_and_join(mcu_device).map_err(|error| error.to_string())
}

#[cfg(not(feature = "binder"))]
fn run(_mcu_device: Option<std::path::PathBuf>) -> Result<(), String> {
    Ok(()) // Host binary remains hardware-inert.
}
