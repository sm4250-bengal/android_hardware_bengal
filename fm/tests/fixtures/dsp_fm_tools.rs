//! Fixtures transcribed from the device-1 blob-free oracle.
//!
//! Source: `~/android/apps/dsp-fm-tools/src/hu-qn8035.c`:
//! registers/address lines 40-60; frequency word lines 95-103; tune sequence
//! lines 106-123; RDS group-0 decode lines 125-153; reset lines 342-350.

use xyauto_fm::qn8035::RegisterWrite;

pub const ADDRESS: u16 = 0x10;
pub const CHIP_ID: u8 = 0x84;
pub const RESET: RegisterWrite = RegisterWrite {
    register: 0x00,
    value: 0x80,
};

/// 97.5 MHz => round((97.5 - 60.0) / 0.05) = 750 = 0x02ee.
pub const TUNE_97_5: [RegisterWrite; 3] = [
    RegisterWrite {
        register: 0x07,
        value: 0xee,
    },
    RegisterWrite {
        register: 0x0a,
        value: 0x02,
    },
    RegisterWrite {
        register: 0x00,
        value: 0x19,
    },
];

pub const PI: u16 = 0x8412;
pub const PS: &str = "TESTFM  ";

pub fn rds_group(segment: u8) -> [u8; 8] {
    let offset = segment as usize * 2;
    let pair = &PS.as_bytes()[offset..offset + 2];
    [
        (PI >> 8) as u8,
        PI as u8,
        0x00,
        segment,
        0,
        0,
        pair[0],
        pair[1],
    ]
}
