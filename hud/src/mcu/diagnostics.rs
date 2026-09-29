// SPDX-License-Identifier: Apache-2.0
//! Read-only, in-memory MCU evidence. No wire payloads or key codes are retained.
//! RX times are daemon-relative pre-read samples; GPS TX times are sampled
//! after the host write returns. Neither is a wire/MCU receipt timestamp.
//! Root/shell: `dumpsys vendor.xyauto.hud.IHud/default --mcu` (or no arguments).
//! RX counters restart on each successful UART open; GPS TX evidence lasts
//! for the reader thread lifetime and retains only its last two returns. Read errors exclude
//! open/TX failures and poll timeouts; last_errno describes the latest read error.
//! A skipped publication leaves the previous snapshot until the owner publishes
//! again. This cannot recover frames received before this daemon started.

use std::ffi::CStr;
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use super::{Frame, Stats};

pub(crate) const CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Record {
    pub(crate) sequence: u64,
    pub(crate) sample_ms: u64,
    pub(crate) length: usize,
    pub(crate) command: u8,
    pub(crate) valid: bool,
    pub(crate) kind: &'static str,
    pub(crate) state: Option<u8>,
    pub(crate) version: Option<(u16, u32)>,
}

#[derive(Clone, Copy, Debug)]
struct GpsReadyReturn {
    notification: super::gps_ready::Notification,
    session: u64,
    generation: u64,
    sample_ms: u64,
    complete: bool,
}

/// The owner keeps this outside the reopen loop, independent of log delivery.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GpsReadyTxEvidence {
    complete: u64,
    errors: u64,
    recent: [Option<GpsReadyReturn>; 2],
}

impl GpsReadyTxEvidence {
    /// Observe only the actual fixed sender's return, never a scheduled intent.
    pub(crate) fn record_return(&mut self, notification: super::gps_ready::Notification,
                               session: u64, generation: u64, sample_ms: u64, complete: bool) {
        if complete { self.complete = self.complete.saturating_add(1); }
        else { self.errors = self.errors.saturating_add(1); }
        self.recent = [self.recent[1], Some(GpsReadyReturn {
            notification, session, generation, sample_ms, complete,
        })];
    }

    fn write(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "gps-ready-tx complete={} errors={} retained={} timebase=daemon-monotonic-post-write-ms boundary=write_all-return mcu_ack=unobserved",
                 self.complete, self.errors, self.recent.iter().flatten().count())?;
        for record in self.recent.iter().flatten() {
            writeln!(writer, "gps-ready-return reason={} sequence={} session={} generation={} sample_ms={} phase={}",
                     record.notification.reason.label(), record.notification.sequence,
                     record.session, record.generation, record.sample_ms,
                     if record.complete { "write-complete" } else { "write-error" })?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Snapshot {
    pub(crate) session: u64,
    gps_ready: GpsReadyTxEvidence,
    pub(crate) read_calls: u64,
    pub(crate) rx_bytes: u64,
    pub(crate) zero_reads: u64,
    pub(crate) read_errors: u64,
    pub(crate) last_errno: Option<i32>,
    pub(crate) last_read_ms: u64,
    pub(crate) frames: u64,
    pub(crate) valid: u64,
    pub(crate) bad: u64,
    pub(crate) discarded: u64,
    pub(crate) pending: usize,
    pub(crate) records: [Record; CAPACITY],
    pub(crate) retained: usize,
    next: usize,
}

impl Snapshot {
    /// Attach cumulative TX evidence to each fresh per-session RX publication.
    pub(crate) fn with_gps_ready(mut self, evidence: GpsReadyTxEvidence) -> Self {
        self.gps_ready = evidence;
        self
    }

    pub(crate) fn read(&mut self, sample_ms: u64, result: &io::Result<usize>) {
        self.read_calls = self.read_calls.saturating_add(1);
        self.last_read_ms = sample_ms;
        match result {
            Ok(count) => {
                self.rx_bytes = self.rx_bytes.saturating_add(*count as u64);
                self.zero_reads = self.zero_reads.saturating_add(u64::from(*count == 0));
            }
            Err(error) => {
                self.read_errors = self.read_errors.saturating_add(1);
                self.last_errno = error.raw_os_error();
            }
        }
    }

    pub(crate) fn frame(&mut self, frame: &Frame, valid: bool, sample_ms: u64) {
        self.frames = self.frames.saturating_add(1);
        let mut record = Record {
            sequence: self.frames,
            sample_ms,
            length: frame.raw().len(),
            command: frame.command(),
            valid,
            kind: "redacted",
            ..Record::default()
        };
        // Only these exact public status/version shapes may retain decoded fields.
        // Bad checksum, keys, unknown commands and trailing bytes never retain payload.
        if valid {
            match frame.payload() {
                [0x33, a, b, c, d, e, f] => {
                    record.kind = "version";
                    record.version = Some((
                        u16::from_le_bytes([*a, *b]),
                        u32::from_le_bytes([*c, *d, *e, *f]),
                    ));
                }
                [0x12, state] => {
                    record.kind = "device-state";
                    record.state = Some(*state);
                }
                [0x05, state] => {
                    record.kind = "power-state";
                    record.state = Some(*state);
                }
                [0x10, ..] => record.kind = "key-redacted",
                _ => {}
            }
        }
        self.records[self.next] = record;
        self.next = (self.next + 1) % CAPACITY;
        self.retained = (self.retained + 1).min(CAPACITY);
    }

    pub(crate) fn parser(&mut self, stats: Stats, pending: usize) {
        self.valid = stats.valid;
        self.bad = stats.bad;
        self.discarded = stats.discarded;
        self.pending = pending;
    }

    fn write(&self, writer: &mut dyn Write, skipped: u64) -> io::Result<()> {
        writeln!(writer, "mcu-diagnostics v1 timebase=daemon-monotonic-pre-read-ms payloads=redacted session={} publish_skipped={skipped}", self.session)?;
        writeln!(
            writer,
            "reads={} bytes={} zero={} errors={} last_errno={:?} last_read_ms={}",
            self.read_calls,
            self.rx_bytes,
            self.zero_reads,
            self.read_errors,
            self.last_errno,
            self.last_read_ms
        )?;
        writeln!(
            writer,
            "frames={} valid={} bad={} discarded={} pending={} retained={} overwritten={}",
            self.frames,
            self.valid,
            self.bad,
            self.discarded,
            self.pending,
            self.retained,
            self.frames.saturating_sub(self.retained as u64)
        )?;
        self.gps_ready.write(writer)?;
        let start = (self.next + CAPACITY - self.retained) % CAPACITY;
        for i in 0..self.retained {
            let r = self.records[(start + i) % CAPACITY];
            writeln!(
                writer,
                "seq={} sample_ms={} len={} cmd={:02x} valid={} kind={} state={:?} version={:?}",
                r.sequence, r.sample_ms, r.length, r.command, r.valid, r.kind, r.state, r.version
            )?;
        }
        Ok(())
    }
}

/// Publication never waits for a Binder reader; later publication catches up
/// cumulative counters and any records still in the owner's fixed ring.
#[derive(Default)]
pub(crate) struct Diagnostics {
    snapshot: Mutex<Snapshot>,
    skipped: AtomicU64,
}

impl Diagnostics {
    pub(crate) fn publish(&self, snapshot: Snapshot) {
        match self.snapshot.try_lock() {
            Ok(mut current) => *current = snapshot,
            Err(_) => {
                self.skipped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Authorization and arguments precede the snapshot. No mutex is held while
    /// formatting or writing to the caller's potentially slow/broken descriptor.
    pub(crate) fn dump(&self, uid: u32, writer: &mut dyn Write, args: &[&CStr]) -> io::Result<()> {
        if uid != 0 && uid != 2000 {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        if args.len() > 1 || args.first().is_some_and(|arg| arg.to_bytes() != b"--mcu") {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let snapshot = *self
            .snapshot
            .try_lock()
            .map_err(|_| io::Error::from(io::ErrorKind::WouldBlock))?;
        snapshot.write(writer, self.skipped.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcu::{checksum, Item, Parser};

    fn frame(payload: &[u8]) -> Frame {
        let mut bytes = vec![0x5a, 0xa5, (payload.len() + 1) as u8];
        bytes.extend_from_slice(payload);
        bytes.push(checksum(payload));
        match Parser::new().push(&bytes).pop().unwrap() {
            Item::Frame(frame) => frame,
            _ => panic!("fixture not valid"),
        }
    }

    #[test]
    fn dump_distinguishes_no_gps_ready_write_from_missing_log_lines() {
        let diagnostics = Diagnostics::default();
        let mut output = Vec::new();
        diagnostics.dump(0, &mut output, &[]).unwrap();
        assert!(String::from_utf8(output).unwrap().contains("gps-ready-tx complete=0 errors=0"));
    }

    #[test]
    fn gps_write_returns_are_bounded_and_survive_skipped_publication_and_reopen() {
        use crate::mcu::gps_ready::{Notification, Reason};
        let mut evidence = GpsReadyTxEvidence::default();
        evidence.record_return(Notification { sequence: 1, reason: Reason::BootInit }, 1, 0, 100, true);
        evidence.record_return(Notification { sequence: 2, reason: Reason::BootInit }, 1, 0, 1100, true);
        evidence.record_return(Notification { sequence: 1, reason: Reason::Wake }, 2, 3, 9000, false);
        let diagnostics = Diagnostics::default();
        let held = diagnostics.snapshot.lock().unwrap();
        diagnostics.publish(Snapshot::default().with_gps_ready(evidence));
        drop(held);
        // A fresh UART has zero RX counters but carries the owner's TX history.
        diagnostics.publish(Snapshot { session: 3, ..Snapshot::default() }.with_gps_ready(evidence));
        let mut output = Vec::new();
        diagnostics.dump(2000, &mut output, &[]).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("session=3 publish_skipped=1"));
        assert!(output.contains("gps-ready-tx complete=2 errors=1 retained=2 timebase=daemon-monotonic-post-write-ms boundary=write_all-return mcu_ack=unobserved"));
        assert!(output.contains("gps-ready-return reason=boot-init sequence=2 session=1 generation=0 sample_ms=1100 phase=write-complete"));
        assert!(output.contains("gps-ready-return reason=wake sequence=1 session=2 generation=3 sample_ms=9000 phase=write-error"));
        assert_eq!(output.lines().filter(|line| line.starts_with("gps-ready-return ")).count(), 2);
        assert!(!output.contains("sample_ms=100 "), "only the last two TX returns are retained");
        assert!(output.contains("reads=0 bytes=0 zero=0 errors=0"), "TX failures are not RX errors");
        assert!(std::mem::size_of::<Snapshot>() <= 4096);
    }

    #[test]
    fn gps_counters_saturate_and_dump_stays_bounded_at_maximum_values() {
        use crate::mcu::gps_ready::{Notification, Reason};
        let mut evidence = GpsReadyTxEvidence { complete: u64::MAX, errors: u64::MAX, ..Default::default() };
        for complete in [true, false] {
            evidence.record_return(Notification { sequence: 2, reason: Reason::Wake },
                u64::MAX, u64::MAX, u64::MAX, complete);
        }
        assert_eq!((evidence.complete, evidence.errors), (u64::MAX, u64::MAX));
        let mut output = Vec::new();
        evidence.write(&mut output).unwrap();
        assert!(output.len() < 700);
    }

    #[test]
    fn production_owner_records_actual_return_before_logs_and_attaches_every_snapshot() {
        let source = include_str!("../binder.rs");
        let owner = source.split("let mut diagnostic_session = 0u64;").nth(1).unwrap()
            .split("fn set_mcu_connected(").next().unwrap();
        let retained = owner.find("let mut gps_ready_tx = crate::mcu::diagnostics::GpsReadyTxEvidence::default();").unwrap();
        assert!(retained < owner.find("gps_ready.disconnected();").unwrap(), "history must live outside the reopen loop");
        let send = owner.find("let written = session.send_gps_ready();").unwrap();
        let record = owner.find("gps_ready_tx.record_return(notification, diagnostic_session,").unwrap();
        let log = owner[send..].find("log_both(written.is_err()").unwrap() + send;
        assert!(send < record && record < log);
        assert!(owner[record..log].contains("session.generation(), elapsed_ms(started), written.is_ok());"));
        assert_eq!(owner.matches("gps_ready_tx.record_return(").count(), 1);
        assert_eq!(owner.matches("diagnostics.publish(session.diagnostics().with_gps_ready(gps_ready_tx));").count(), 2);
        assert!(!owner.contains("diagnostics.publish(session.diagnostics());"));
    }

    #[test]
    fn bounded_ring_and_payload_redaction() {
        assert!(std::mem::size_of::<Snapshot>() <= 4096);
        let mut snapshot = Snapshot::default();
        for i in 0..35 {
            snapshot.frame(&frame(&[0x12, i]), true, i as u64);
        }
        assert_eq!((snapshot.frames, snapshot.retained), (35, 32));
        let mut output = Vec::new();
        snapshot.write(&mut output, 0).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.lines().find(|line| line.starts_with("seq=")).unwrap().starts_with("seq=4 "));
        assert!(output.contains("overwritten=3"));
        assert!(output.lines().last().unwrap().starts_with("seq=35 "));
        assert!(output.len() < 8192);
        let worst = Snapshot {
            frames: u64::MAX,
            read_calls: u64::MAX,
            rx_bytes: u64::MAX,
            retained: CAPACITY,
            records: [Record {
                sequence: u64::MAX,
                sample_ms: u64::MAX,
                length: usize::MAX,
                command: 255,
                valid: true,
                kind: "device-state",
                state: Some(255),
                version: Some((u16::MAX, u32::MAX)),
            }; CAPACITY],
            ..Snapshot::default()
        };
        let mut output = Vec::new();
        worst.write(&mut output, u64::MAX).unwrap();
        assert!(output.len() < 8192, "dump must have a fixed output bound");

        for payload in [
            &[0x10, b'S', b'E', b'C', b'R', b'E', b'T'][..],
            &[0x99, b'S', b'E', b'C', b'R', b'E', b'T'],
            &[0x33, 1, 2, 3, 4, 5, 6, 7],
            &[0x12, 4, 99],
            &[0x05, 4, 99],
        ] {
            let mut snapshot = Snapshot::default();
            snapshot.frame(&frame(payload), true, 1);
            let record = snapshot.records[0];
            assert_eq!(record.state, None);
            assert_eq!(record.version, None);
            assert!(matches!(record.kind, "redacted" | "key-redacted"));
            let mut output = Vec::new();
            snapshot.write(&mut output, 0).unwrap();
            assert!(!String::from_utf8(output).unwrap().contains("SECRET"));
        }
        let mut snapshot = Snapshot::default();
        snapshot.frame(&frame(&[0x33, 1, 2, 3, 4, 5, 6]), false, 1);
        assert_eq!(
            snapshot.records[0].version, None,
            "bad checksum must not validate fields"
        );
    }

    #[test]
    fn dump_authorization_arguments_contention_and_unlocked_writer() {
        let diagnostics = Diagnostics::default();
        let mut out = Vec::new();
        for uid in [1000, 10000, u32::MAX] {
            assert_eq!(
                diagnostics.dump(uid, &mut out, &[]).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
        assert!(out.is_empty());
        let invalid = std::ffi::CString::new("--raw").unwrap();
        assert_eq!(
            diagnostics
                .dump(0, &mut out, &[&invalid])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let arg = std::ffi::CString::new("--mcu").unwrap();
        assert_eq!(
            diagnostics
                .dump(0, &mut out, &[&arg, &arg])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(out.is_empty());
        let held = diagnostics.snapshot.lock().unwrap();
        diagnostics.publish(Snapshot::default());
        assert_eq!(diagnostics.skipped.load(Ordering::Relaxed), 1);
        assert_eq!(
            diagnostics.dump(0, &mut out, &[]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(held);
        struct Reentrant<'a>(&'a Diagnostics);
        impl Write for Reentrant<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                assert!(
                    self.0.snapshot.try_lock().is_ok(),
                    "dump retained lock during I/O"
                );
                self.0.publish(Snapshot {
                    session: 9,
                    ..Snapshot::default()
                });
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        diagnostics
            .dump(2000, &mut Reentrant(&diagnostics), &[&arg])
            .unwrap();
        assert_eq!(diagnostics.snapshot.lock().unwrap().session, 9);
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            diagnostics.dump(0, &mut Broken, &[]).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(diagnostics.snapshot.try_lock().is_ok());
    }
}
