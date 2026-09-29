// SPDX-License-Identifier: Apache-2.0

//! Frame/timing fixtures extracted from hu-mcu-bridge's lifecycle tests.

use super::{TX_ACC_OFF_COMPLETE, TX_ACC_ON_ANSWER};

pub const ACC_OFF: &[u8] = &[0x5a, 0xa5, 0x03, 0x12, 0x03, 0xeb];
pub const FINAL_STAGE: &[u8] = &[0x5a, 0xa5, 0x03, 0x05, 0x10, 0xeb];
pub const ACC_ON: &[u8] = &[0x5a, 0xa5, 0x03, 0x12, 0x04, 0xea];

pub enum FixtureStep {
    Frame { at_ms: u64, bytes: &'static [u8] },
    Advance { at_ms: u64 },
    UartReopened,
}

pub struct GoFixture {
    pub name: &'static str,
    pub steps: &'static [FixtureStep],
    pub expected_tx: &'static [&'static [u8]],
}

const COMPLETE: &[FixtureStep] = &[
    FixtureStep::Frame {
        at_ms: 0,
        bytes: ACC_OFF,
    },
    FixtureStep::Frame {
        at_ms: 25_000,
        bytes: FINAL_STAGE,
    },
    FixtureStep::Advance { at_ms: 25_499 },
    FixtureStep::Advance { at_ms: 25_500 },
    FixtureStep::Frame {
        at_ms: 30_000,
        bytes: ACC_ON,
    },
    FixtureStep::UartReopened,
];

const CANCELLED: &[FixtureStep] = &[
    FixtureStep::Frame {
        at_ms: 0,
        bytes: ACC_OFF,
    },
    FixtureStep::Frame {
        at_ms: 25_000,
        bytes: FINAL_STAGE,
    },
    FixtureStep::Frame {
        at_ms: 25_499,
        bytes: ACC_ON,
    },
    FixtureStep::Advance { at_ms: 25_500 },
    FixtureStep::UartReopened,
];

const DUPLICATE_FINAL: &[FixtureStep] = &[
    FixtureStep::Frame {
        at_ms: 0,
        bytes: ACC_OFF,
    },
    FixtureStep::Frame {
        at_ms: 25_000,
        bytes: FINAL_STAGE,
    },
    FixtureStep::Frame {
        at_ms: 25_100,
        bytes: FINAL_STAGE,
    },
    FixtureStep::Advance { at_ms: 25_500 },
];

const INITIAL_ON_FILTER: &[FixtureStep] = &[
    FixtureStep::Frame {
        at_ms: 0,
        bytes: ACC_ON,
    },
    FixtureStep::Frame {
        at_ms: 1,
        bytes: ACC_ON,
    },
];

pub const GO_FIXTURES: &[GoFixture] = &[
    GoFixture {
        name: "complete cycle and reopen repeat",
        steps: COMPLETE,
        expected_tx: &[TX_ACC_OFF_COMPLETE, TX_ACC_ON_ANSWER, TX_ACC_ON_ANSWER],
    },
    GoFixture {
        name: "ACC-on cancels the 500 ms callback",
        steps: CANCELLED,
        expected_tx: &[TX_ACC_ON_ANSWER, TX_ACC_ON_ANSWER],
    },
    GoFixture {
        name: "duplicate final-stage frame completes once",
        steps: DUPLICATE_FINAL,
        expected_tx: &[TX_ACC_OFF_COMPLETE],
    },
    GoFixture {
        name: "initial ACC-on requires two frames and no answer",
        steps: INITIAL_ON_FILTER,
        expected_tx: &[],
    },
];
