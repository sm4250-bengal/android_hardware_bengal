// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! The one line this daemon writes to `/dev/kmsg`, and why it is one `write()`.
//!
//! # The bug this module exists to fix
//!
//! `bengal-hud.rc` sets `stdio_to_kmsg`, so the daemon's stderr *is*
//! `/dev/kmsg`. Two kernel facts turn that into silent data loss:
//!
//! 1. **Every `write()` to `/dev/kmsg` is one kernel record**, and Rust's
//!    `Stderr` is unbuffered, so `Write::write_fmt` issues **one syscall per
//!    format fragment**. `eprintln!("bengal-hud: {}", message)` therefore
//!    produced **three** records — the literal prefix, the message, the
//!    newline. Measured on D3-0109:
//!
//!    ```text
//!    [   49.022774] [  T598] bengal-hud:
//!    [   49.022850] [  T598] uinput disabled property=… value=0 mapping_confirmed=false
//!    [   49.026318] [  T598]
//!    ```
//!
//!    That split is also why `logcat` showed a blank `W bengal-hud:` followed
//!    by an untagged continuation: those lines come back through logd's
//!    *kernel* buffer, where `LogKlog` parses `tag: message` out of the first
//!    record (empty message) and finds no tag in the second.
//!
//! 2. **`devkmsg_write()` rate-limits at 10 records per 5 s per open fd**
//!    (`kernel/printk/printk.c:763,945-946`,
//!    `include/linux/ratelimit_types.h:9-10`), and `devkmsg_open()` sets
//!    `RATELIMIT_MSG_ON_RELEASE`, so the `"N output lines suppressed"` notice
//!    is only emitted when the fd **closes** — never, for a service. Three
//!    records per line meant a real budget of about **three log lines per five
//!    seconds**, and the losses were invisible. On D3-0109 exactly ten records
//!    landed and the stream was cut mid-line; the `mcu-rx` line for the only
//!    MCU frame of the entire boot was destroyed that way.
//!
//! One `write_all` of a preformatted line fixes both halves: one record per
//! line, three times the budget.
//!
//! # The `<N>` prefix
//!
//! `devkmsg_write()` parses a leading `<N>` and takes it as the syslog value:
//! `level = LOG_LEVEL(u)`, and `facility` stays `LOG_USER` because
//! `LOG_FACILITY(6)` and `LOG_FACILITY(3)` are both 0. Without a prefix the
//! record takes `default_message_loglevel`, which `/proc/sys/kernel/printk`
//! reports as `4` = `KERN_WARNING` on this unit — which is why an `I` line
//! from `log_both` showed up as `W` in the kernel buffer. `<6>` and `<3>` make
//! logd tag the record `I` and `E`, matching the liblog copy. The kernel
//! strips the prefix before storing, so `dmesg` shows the clean text.
//!
//! Full write-up: `docs/research/2026-09-03-mcu-button-capture-prep.md` §1.
//!
//! This module has zero dependencies and no I/O, which is the point: the
//! formatting is the part worth testing, and it tests at the desk with
//! `cargo test --offline` even though `src/binder.rs` needs Soong.

/// Tag used for both sinks: the liblog tag, and the `tag: ` prefix that
/// `LogKlog` parses back out of the kernel record.
pub const LOG_TAG: &str = "bengal-hud";

/// `<6>` — `KERN_INFO`, the level `log_both(false, …)` asks for.
pub const KMSG_LEVEL_INFO: &str = "<6>";

/// `<3>` — `KERN_ERR`, the level `log_both(true, …)` asks for.
pub const KMSG_LEVEL_ERROR: &str = "<3>";

/// The complete `/dev/kmsg` line for one log message: level prefix, tag,
/// message, trailing newline — and **nothing that can split it**.
///
/// Interior newlines and carriage returns are flattened to spaces. A newline
/// inside the text would put the remainder into a second kernel record with no
/// tag, which is exactly the untagged-continuation symptom this change removes;
/// a NUL would truncate the C-side copy. No message emitted today contains any
/// of them, and this keeps that true by construction rather than by convention.
///
/// The caller writes the returned string with a single `write_all`.
pub fn kmsg_line(error: bool, message: &str) -> String {
    let level = if error {
        KMSG_LEVEL_ERROR
    } else {
        KMSG_LEVEL_INFO
    };
    let mut line = String::with_capacity(level.len() + LOG_TAG.len() + message.len() + 4);
    line.push_str(level);
    line.push_str(LOG_TAG);
    line.push_str(": ");
    for ch in message.chars() {
        match ch {
            '\n' | '\r' => line.push(' '),
            '\0' => line.push('?'),
            other => line.push(other),
        }
    }
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_and_error_carry_the_right_kernel_level() {
        assert_eq!(kmsg_line(false, "hello"), "<6>bengal-hud: hello\n");
        assert_eq!(kmsg_line(true, "hello"), "<3>bengal-hud: hello\n");
    }

    #[test]
    fn a_frame_line_is_exactly_one_record() {
        // The regression under test: this used to be three write() calls and
        // therefore three kernel records.
        let line = kmsg_line(
            false,
            "mcu-rx elapsed_ms=1234 checksum=ok command=0x10 \
             decoded=device1-oracle:Button;device3-map=PROVISIONAL frame=5a a5 03 10 01 2a c2",
        );
        assert_eq!(
            line.matches('\n').count(),
            1,
            "one trailing newline = one /dev/kmsg record: {line:?}"
        );
        assert!(line.ends_with('\n'));
        assert!(line.starts_with("<6>bengal-hud: "));
        assert!(line.contains("frame=5a a5 03 10 01 2a c2"));
    }

    #[test]
    fn interior_newlines_cannot_split_the_record() {
        let line = kmsg_line(true, "first\nsecond\r\nthird");
        assert_eq!(line, "<3>bengal-hud: first second  third\n");
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn interior_nul_cannot_truncate() {
        // CString::new() would refuse this for the liblog copy; the kmsg copy
        // must not carry a raw NUL either.
        let line = kmsg_line(false, "a\0b");
        assert_eq!(line, "<6>bengal-hud: a?b\n");
        assert!(!line.contains('\0'));
    }

    #[test]
    fn an_empty_message_still_produces_a_tagged_record() {
        // The old code emitted a bare "bengal-hud: " record for EVERY line;
        // now a tagged empty line is only what an actually empty message gives.
        assert_eq!(kmsg_line(false, ""), "<6>bengal-hud: \n");
    }

    #[test]
    fn the_level_prefix_is_a_valid_syslog_value_with_facility_zero() {
        // devkmsg_write() takes LOG_FACILITY(u) only when it is non-zero;
        // 6 >> 3 == 0 and 3 >> 3 == 0, so both stay LOG_USER as intended.
        for (text, want_level) in [(KMSG_LEVEL_INFO, 6u32), (KMSG_LEVEL_ERROR, 3u32)] {
            let value: u32 = text
                .trim_start_matches('<')
                .trim_end_matches('>')
                .parse()
                .expect("decimal syslog value");
            assert_eq!(value & 0x07, want_level, "LOG_LEVEL");
            assert_eq!(value >> 3, 0, "LOG_FACILITY must be 0 so LOG_USER is kept");
        }
    }
}
