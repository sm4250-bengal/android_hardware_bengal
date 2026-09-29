// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Copied device-1 protocol-oracle fixtures.
//!
//! These byte sequences are copied into this crate from
//! `~/android/apps/mcu-toolbox/bridge/main_test.go` (`TestInputMappingsWithoutExecution`,
//! `TestRotaryMagnitudeIsPreservedAndCapped`, and
//! `TestKnobPushPublishesTypedPressAndRelease`). They are deliberately not read
//! from that external tree at test time: this repository is the self-contained
//! device-3 port, while mcu-toolbox remains the measured protocol oracle.

/// One named, checksum-valid device-1 frame.
pub struct Device1Frame {
    /// Fixture name from the Go test's semantic assertion.
    pub name: &'static str,
    /// Complete MCU frame, including preamble and checksum.
    pub bytes: &'static [u8],
}

/// Recorded/canonical button and rotary frames used by the original bridge.
pub const DEVICE1_BUTTON_FRAMES: &[Device1Frame] = &[
    Device1Frame {
        name: "setup-press",
        bytes: &[0x5a, 0xa5, 0x04, 0x10, 0x01, 0x2d, 0xc2],
    },
    Device1Frame {
        name: "right-knob-push-press",
        bytes: &[0x5a, 0xa5, 0x04, 0x10, 0x01, 0x2a, 0xc5],
    },
    Device1Frame {
        name: "right-knob-push-release",
        bytes: &[0x5a, 0xa5, 0x04, 0x10, 0x02, 0x2a, 0xc4],
    },
    Device1Frame {
        name: "left-knob-volume-down",
        bytes: &[0x5a, 0xa5, 0x05, 0x10, 0x01, 0x0d, 0x01, 0xe1],
    },
    Device1Frame {
        name: "right-knob-dpad-right-fast",
        bytes: &[0x5a, 0xa5, 0x05, 0x10, 0x01, 0x02, 0x09, 0xe4],
    },
    Device1Frame {
        name: "left-knob-volume-up-capped",
        bytes: &[0x5a, 0xa5, 0x05, 0x10, 0x01, 0x0c, 0xff, 0xe4],
    },
];
