// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Differential agreement between the Rust model and the device-1 C oracle.
//!
//! (Crate doc added 2026-09-05 so `clippy -D missing_docs --all-targets`
//! passes; it was the one target in this crate without one.)

#[path = "fixtures/dsp_fm_tools.rs"]
mod oracle;

use xyauto_fm::qn8035::{self, programme_service_segment, RdsAssembler, RegisterWrite};

#[test]
fn address_identity_and_reset_match_the_c_oracle() {
    assert_eq!(qn8035::ADDRESS, oracle::ADDRESS);
    assert_eq!(qn8035::CHIP_ID, oracle::CHIP_ID);
    assert_eq!(
        RegisterWrite {
            register: qn8035::reg::SYSTEM1,
            value: qn8035::system1::SOFTWARE_RESET,
        },
        oracle::RESET
    );
}

#[test]
fn tune_sequence_matches_the_c_oracle_byte_for_byte() {
    assert_eq!(qn8035::frequency_word(97_500).unwrap(), 0x02ee);
    assert_eq!(qn8035::tune_sequence(97_500).unwrap(), oracle::TUNE_97_5);
}

#[test]
fn rds_group_zero_offsets_match_the_c_oracle() {
    let mut assembler = RdsAssembler::default();
    for segment in 0..4 {
        let blocks = oracle::rds_group(segment);
        let decoded = programme_service_segment(qn8035::status2::RDS_SYNC, blocks).unwrap();
        assert_eq!(decoded.pi, oracle::PI);
        assert_eq!(decoded.segment, segment);
        let publication = assembler.ingest(qn8035::status2::RDS_SYNC, blocks);
        if segment < 3 {
            assert!(publication.is_none());
        } else {
            assert_eq!(publication.unwrap().programme_service, oracle::PS);
        }
    }
}

#[test]
fn stricter_daemon_policy_rejects_the_errors_the_c_tool_only_prints() {
    let blocks = oracle::rds_group(0);
    assert!(programme_service_segment(qn8035::status2::RDS_SYNC | 1, blocks).is_none());
}
