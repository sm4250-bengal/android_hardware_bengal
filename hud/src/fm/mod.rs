// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! FM — QN8035 constants and the typed audio-guard handover contract.
//!
//! The register map and the sequence below are transcribed from the device-1
//! bridge (`apps/mcu-toolbox/bridge/{tuner_i2c,radio}.go`), which D15/D18 make
//! a **protocol oracle, not an architecture**: the numbers are evidence, the
//! shape of the code is not inherited.

use crate::i2c::QN8035_ADDRESS;

/// Android-only production QN8035 transport.  The typed IAudioGuard handover
/// remains app-owned; this module has no raw DSP or GPIO surface.
#[cfg(feature = "binder")]
pub mod backend;

/// The tuner's address on the `i2c-fm` bus.
pub const ADDRESS: u16 = QN8035_ADDRESS;

/// QN8035 registers that this project uses.
pub mod reg {
    /// Reset, standby, RX and RDS mode.
    pub const SYSTEM1: u8 = 0x00;
    /// Estimated RF input carrier-to-noise ratio.
    pub const SNR: u8 = 0x02;
    /// In-band signal strength.
    pub const RSSI: u8 = 0x03;
    /// Chip ID. Reads [`super::CHIP_ID`].
    pub const CID: u8 = 0x06;
    /// Low eight bits of the channel word.
    pub const CHANNEL: u8 = 0x07;
    /// High two channel bits, plus scan fields.
    pub const CH_STEP: u8 = 0x0a;
    /// RDS blocks A..D occupy 0x0b..0x12.
    pub const RDS_DATA: u8 = 0x0b;
    /// Block errors [3:0], RDSSYNC [4], RDS_RXUPD [7].
    pub const STATUS2: u8 = 0x13;
    /// Documented analogue/digital gain and audio mute.
    pub const VOLUME: u8 = 0x14;
}

/// What [`reg::CID`] must read. The Phase-4d proof point.
pub const CHIP_ID: u8 = 0x84;

/// `SYSTEM1` values.
pub mod system1 {
    /// Software reset.
    pub const SOFTWARE_RESET: u8 = 0x80;
    /// Standby.
    pub const STANDBY: u8 = 0x20;
    /// Channel from registers | RDS | receive.
    pub const RECEIVE_RDS: u8 = 0x19;
}

/// `VOLUME` values: 50 us de-emphasis, 0 dB digital/analogue.
pub mod volume {
    /// Un-muted.
    pub const UNITY_50US: u8 = 0x07;
    /// The same gain with the documented `MUTE_EN` bit.
    pub const MUTED: u8 = 0x87;
}

/// Band and grid. Off-grid frequencies have no defined channel word.
pub const MIN_KHZ: u32 = 87_500;
/// Top of band.
pub const MAX_KHZ: u32 = 108_000;
/// Channel spacing.
pub const STEP_KHZ: u32 = 100;

/// A station is any channel whose SNR clears this.
///
/// SNR, not RSSI, and the margin is the reason: measured on device 1, 97.5 MHz
/// read SNR 32 with its neighbours at 10 and 15 and empty air at a flat 6,
/// while RSSI managed only 89 against 83. The RSSI seek this replaced landed
/// +/-0.1 MHz off and missed one time in eight.
///
/// ★ THIS IS A MIRROR, NOT THE VALUE IN FORCE. The walk judges on
/// `xyauto_fm::qn8035::SEEK_SNR_FLOOR`; this copy exists because `xyauto_fm`
/// is a Soong `rustlib`, not a Cargo dependency, so this module cannot see it
/// at the desk. Changing one and not the other would leave a retuned floor
/// documented here and unused everywhere — `platform/scripts/fm-backend-desk-test.sh`
/// T4 asserts the two literals agree, and a drift control proves it fires.
pub const SEEK_SNR_FLOOR: u8 = 20;

/// `STATUS2` bit masks.
pub mod status2 {
    /// Block errors.
    pub const BLKERR: u8 = 0x0f;
    /// RDS synchronised.
    pub const RDSSYNC: u8 = 0x10;
    /// RDS receive updated.
    pub const RDS_RXUPD: u8 = 0x80;
}

/// Is this a frequency the tuner can express?
pub fn is_tunable(khz: u32) -> bool {
    (MIN_KHZ..=MAX_KHZ).contains(&khz) && khz % STEP_KHZ == 0
}

/// The ordered FM handover, as documentation that the compiler keeps honest.
///
/// It is a constant, not a comment, because the ORDER is the thing that must
/// not drift. `IAudioGuard` owns the FM rails, ADAU gates and analogue mux;
/// `IFmRadio::setEnabled` owns only the QN8035 part after that commit. Settles
/// are device 1's measured values, carried as a starting point for device 3
/// and **not** as fact.
pub const SOURCE_SELECT_SEQUENCE: &[Step] = &[
    Step {
        what: "IFmRadio.tune while disabled caches a valid channel",
        settle_ms: 0,
        why: "the Binder service records it without touching an unpowered QN8035",
    },
    Step {
        what: "IAudioGuard prepare/commit FM: muted ADAU route and A=0/B=1 mux",
        settle_ms: 0,
        why: "the typed guard, not the tuner or MCU path, owns source selection",
    },
    Step {
        what: "QN8035 soft reset, verify CID == 0x84, enter RX+RDS",
        settle_ms: 250,
        why: "covers the PLL lock",
    },
    Step {
        what: "prepared QN8035 channel is applied",
        settle_ms: 0,
        why: "tune occurs while the QN8035 output is still muted",
    },
    Step {
        what: "clear MUTE_EN (VOLUME 0x87 -> 0x07)",
        settle_ms: 250,
        why: "both the QN8035 and guard boundaries remained muted until now",
    },
];

/// One step of [`SOURCE_SELECT_SEQUENCE`].
#[derive(Debug)]
pub struct Step {
    /// What happens.
    pub what: &'static str,
    /// How long to wait afterwards.
    pub settle_ms: u32,
    /// Why the step and the settle exist.
    pub why: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_edges_and_grid() {
        assert!(is_tunable(87_500));
        assert!(is_tunable(97_500));
        assert!(is_tunable(108_000));
        // The three the device-1 channel-word encoder rejects in its own tests.
        assert!(!is_tunable(87_400));
        assert!(!is_tunable(87_525));
        assert!(!is_tunable(108_100));
    }

    #[test]
    fn the_guard_owns_the_source_mux_not_the_mcu_path() {
        // A comment can be deleted without anyone noticing. This cannot.
        let guard = SOURCE_SELECT_SEQUENCE
            .iter()
            .find(|s| s.what.contains("IAudioGuard"))
            .expect("the sequence must name the guard handover");
        assert!(guard.what.contains("A=0/B=1"));
        assert!(guard.why.contains("not the tuner or MCU"));
    }

    #[test]
    fn disabled_tune_guard_commit_then_tuner_enable() {
        let prepared = SOURCE_SELECT_SEQUENCE
            .iter()
            .position(|s| s.what.contains("tune while disabled"))
            .unwrap();
        let guard = SOURCE_SELECT_SEQUENCE
            .iter()
            .position(|s| s.what.contains("IAudioGuard"))
            .unwrap();
        let reset = SOURCE_SELECT_SEQUENCE
            .iter()
            .position(|s| s.what.contains("QN8035"))
            .unwrap();
        let unmute = SOURCE_SELECT_SEQUENCE
            .iter()
            .position(|s| s.what.contains("MUTE_EN"))
            .unwrap();
        assert!(prepared < guard, "the channel is cached while disabled");
        assert!(
            guard < reset,
            "the guard commits the source before QN8035 I/O"
        );
        assert!(reset < unmute, "un-mute is last");
    }
}
