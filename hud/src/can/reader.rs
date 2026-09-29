// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! The receive loop: bytes in, frames/decodes/capture out, one tell per
//! [`super::TELL_INTERVAL_MS`]. Legacy entry points remain generic over
//! [`Read`]; the optional status-refresh entry point adds a bounded `Write`
//! pump on the same fd.

use std::io::{self, Read, Write};

use super::capture::{json_object, Capture, Json};
use super::decode::{decode, Decoded, KeyEvent, Snapshot};
use super::envelope::{encode_hex, Frame, Item, Parser};
use super::profile::Profile;
use super::TELL_INTERVAL_MS;

/// Read size per `read()`; the MCU reader uses 64 too.
pub const READ_CHUNK: usize = 64;

/// Counters the tell line reports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Bytes received.
    pub bytes: u64,
    /// Checksum-valid frames.
    pub frames_ok: u64,
    /// Candidates with a bad checksum.
    pub frames_bad: u64,
    /// Bytes discarded hunting for a header.
    pub discarded: u64,
    /// Successful `read()` calls that returned data.
    pub reads: u64,
    /// When the last byte arrived, in the caller's clock.
    pub last_rx_ms: Option<u64>,
}

/// Where log lines go (kmsg+logcat in the daemon, stderr at the desk).
pub trait Sink {
    /// One line; `error` selects the kernel/liblog level.
    fn line(&mut self, error: bool, message: &str);

    /// A checksum-valid frame decoded as the profile's key frame, after it was
    /// recorded, in the reader's clock. The default ignores it, which is the
    /// behaviour before `can::keys` existed; only the daemon's sink acts on it
    /// (behind `KEYS_PROPERTY`).
    fn key(&mut self, _key: &KeyEvent, _now_ms: u64) {}

    /// Notification after the old reader state is quiesced and reset for a
    /// new vehicle selection. Key injectors use this to discard held state.
    fn vehicle_changed(&mut self, _revision: u64) {}
}

/// A sink that collects lines (tests) or prints them (desk).
#[derive(Default)]
pub struct VecSink(pub Vec<(bool, String)>);

impl Sink for VecSink {
    fn line(&mut self, error: bool, message: &str) {
        self.0.push((error, message.to_owned()));
    }
}

/// What one `feed` produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A valid frame and what the profile made of it.
    Frame(Frame, Decoded),
    /// A candidate with a bad checksum.
    BadChecksum(Frame),
    /// Bytes skipped.
    Discarded(usize),
}

/// The reader.
pub struct Reader {
    parser: Parser,
    profile: Option<Profile>,
    capture: Option<Capture>,
    /// Counters.
    pub stats: Stats,
    /// Latest decoded values.
    pub snapshot: Snapshot,
    port_label: String,
    mmio: String,
    baud: u32,
    next_tell_ms: u64,
    first_frame_logged: bool,
    capture_failure: Option<String>,
    capture_stopped: bool,
    monitor: Option<super::monitor::SharedMonitor>,
    profile_revision: u64,
}

impl Reader {
    /// A reader for one port. `profile == None` runs envelope-only (frames are
    /// counted and captured, nothing decoded).
    pub fn new(
        profile: Option<Profile>,
        capture: Option<Capture>,
        port_label: impl Into<String>,
        mmio: impl Into<String>,
        baud: u32,
    ) -> Self {
        Reader {
            parser: Parser::with_framing(profile.as_ref()
                .and_then(|p| super::envelope::Framing::parse(&p.envelope)).unwrap_or_default()),
            profile,
            capture,
            stats: Stats::default(),
            snapshot: Snapshot::default(),
            port_label: port_label.into(),
            mmio: mmio.into(),
            baud,
            next_tell_ms: 0,
            first_frame_logged: false,
            capture_failure: None,
            capture_stopped: false,
            monitor: None,
            profile_revision: 0,
        }
    }

    /// Observe the existing reader without creating another serial owner.
    pub fn observe(&mut self, monitor: super::monitor::SharedMonitor) {
        self.monitor = Some(monitor);
        self.publish_status();
    }

    fn publish_status(&self) {
        if let Some(monitor) = &self.monitor {
            if let Ok(mut m) = monitor.lock() {
                // A Binder selection can race the end of a read callback. Do
                // not publish old-generation counters/snapshots after that
                // selection has taken effect; the next runtime iteration
                // resets this reader before accepting new bytes.
                if m.vehicle.revision() != self.profile_revision {
                    return;
                }
                m.stats = self.stats.clone();
                if let Some(path) = self.capture_path() {
                    m.capture_name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                }
                m.capture_state = if self.capture.is_some() { "active" }
                    else if self.capture_stopped { "stopped" } else { "off" }.into();
            }
        }
    }

    /// The active profile id, or `none`.
    pub fn profile_id(&self) -> &str {
        self.profile.as_ref().map_or("none", |p| p.id.as_str())
    }

    /// Selection generation associated with this parser/snapshot state.
    pub fn profile_revision(&self) -> u64 {
        self.profile_revision
    }

    /// Associate a newly created reader with the already-selected generation.
    pub fn set_profile_revision(&mut self, revision: u64) {
        self.profile_revision = revision;
    }

    /// Replace the selected profile while retaining the one process-wide
    /// capture file/budget. Parser, decoded snapshot, key state, counters and
    /// first-frame state are all reset so old-car data cannot cross the epoch.
    pub fn switch_profile(&mut self, profile: Option<Profile>, revision: u64, sink: &mut dyn Sink) {
        if self.profile_revision == revision {
            return;
        }
        let old_profile = self.profile_id().to_owned();
        let new_profile = profile.as_ref().map_or("none", |value| value.id.as_str()).to_owned();
        self.record_capture_only("vehicle_changed", &[
            ("revision", Json::Int(revision as i64)),
            ("old_profile", Json::Str(old_profile.clone())),
            ("profile", Json::Str(new_profile.clone())),
        ]);
        self.profile = profile;
        self.profile_revision = revision;
        self.parser = Parser::with_framing(self.profile.as_ref()
            .and_then(|p| super::envelope::Framing::parse(&p.envelope)).unwrap_or_default());
        if let Some(profile) = &self.profile { self.baud = profile.baud; }
        self.stats = Stats::default();
        self.snapshot = Snapshot::default();
        self.next_tell_ms = 0;
        self.first_frame_logged = false;
        sink.vehicle_changed(revision);
        sink.line(false, &format!("canuart: vehicle profile changed revision={} old={} new={}", revision, old_profile, new_profile));
        self.report_capture_failure(sink);
        self.publish_status();
    }

    /// The capture file path, if capturing.
    pub fn capture_path(&self) -> Option<&std::path::Path> {
        self.capture.as_ref().map(Capture::path)
    }

    /// The tell line. Fixed field order; the flight reads it with a regex.
    pub fn tell(&self, now_ms: u64) -> String {
        let last = match self.stats.last_rx_ms {
            Some(t) => format!("{}", now_ms.saturating_sub(t)),
            None => "never".to_owned(),
        };
        format!(
            "canuart: port={} mmio={} baud={} rx={} ok={} bad={} discard={} last={} profile={} fields={} capture={}",
            self.port_label,
            self.mmio,
            self.baud,
            self.stats.bytes,
            self.stats.frames_ok,
            self.stats.frames_bad,
            self.stats.discarded,
            last,
            self.profile_id(),
            self.snapshot.len(),
            if self.capture.is_some() { "active" } else if self.capture_stopped { "stopped" } else { "off" }
        )
    }

    /// Feed one read chunk.
    pub fn feed(&mut self, bytes: &[u8], now_ms: u64, sink: &mut dyn Sink) -> Vec<Outcome> {
        if bytes.is_empty() {
            return Vec::new();
        }
        // The runtime may have read bytes just before a Binder vehicle
        // selection. Drop that stale chunk before it reaches parser state,
        // monitor history, or key injection.
        if let Some(monitor) = &self.monitor {
            match monitor.lock() {
                Ok(m) if m.vehicle.revision() == self.profile_revision => {}
                Ok(_) | Err(_) => return Vec::new(),
            }
        }
        self.stats.bytes += bytes.len() as u64;
        self.stats.reads += 1;
        self.stats.last_rx_ms = Some(now_ms);
        // Preserve partial/unknown envelopes too. A raw RX row is a serial
        // chunk, NOT a second frame; consumers must not double-count it.
        self.record("rx", &[("raw", Json::Str(encode_hex(bytes))),
            ("monotonic_ms", Json::Int(now_ms as i64))]);
        let mut outcomes = Vec::new();
        for item in self.parser.push(bytes) {
            match item {
                Item::Frame(frame) => {
                    self.stats.frames_ok += 1;
                    if let Some(monitor) = &self.monitor {
                        if let Ok(mut m) = monitor.lock() {
                            m.observe_vehicle_controls(self.profile_revision, frame.message_type, &frame.data, now_ms);
                        }
                    }
                    let decoded = match &self.profile {
                        Some(p) => decode(p, &frame),
                        None => Decoded::default(),
                    };
                    self.snapshot.update(&decoded, now_ms);
                    if !self.first_frame_logged {
                        self.first_frame_logged = true;
                        sink.line(
                            false,
                            &format!(
                                "canuart: first valid frame type=0x{:02x} len={} at={}ms groups={}",
                                frame.message_type,
                                frame.data.len(),
                                now_ms,
                                if decoded.groups.is_empty() { "-".to_owned() } else { decoded.groups.join("+") }
                            ),
                        );
                    }
                    self.record_frame(&frame, &decoded, true);
                    if let Some(key) = &decoded.key {
                        // Serialize the epoch check and callback with
                        // Monitor::select_vehicle. This guarantees that a
                        // stale key cannot inject after selection returns.
                        if let Some(monitor) = &self.monitor {
                            if let Ok(m) = monitor.lock() {
                                if m.vehicle.revision() == self.profile_revision {
                                    sink.key(key, now_ms);
                                }
                            }
                        } else {
                            sink.key(key, now_ms);
                        }
                    }
                    outcomes.push(Outcome::Frame(frame, decoded));
                }
                Item::BadChecksum(frame) => {
                    self.stats.frames_bad += 1;
                    self.record_frame(&frame, &Decoded::default(), false);
                    outcomes.push(Outcome::BadChecksum(frame));
                }
                Item::Discarded(raw) => {
                    self.stats.discarded += raw.len() as u64;
                    self.record("discard", &[("raw", Json::Str(encode_hex(&raw))), ("reason", Json::Str("no 0x2e header".into()))]);
                    outcomes.push(Outcome::Discarded(raw.len()));
                }
                Item::Incomplete(_) => {}
            }
        }
        self.report_capture_failure(sink);
        self.publish_status();
        outcomes
    }

    /// Emit the tell when due; returns it when emitted.
    pub fn tick(&mut self, now_ms: u64, sink: &mut dyn Sink) -> Option<String> {
        if now_ms < self.next_tell_ms {
            return None;
        }
        self.next_tell_ms = now_ms + TELL_INTERVAL_MS;
        let line = self.tell(now_ms);
        sink.line(false, &line);
        self.record(
            "tell",
            &[
                ("rx", Json::Int(self.stats.bytes as i64)),
                ("ok", Json::Int(self.stats.frames_ok as i64)),
                ("bad", Json::Int(self.stats.frames_bad as i64)),
                ("discard", Json::Int(self.stats.discarded as i64)),
                ("fields", Json::Int(self.snapshot.len() as i64)),
            ],
        );
        if let Some(cap) = &mut self.capture {
            if let Err(error) = cap.sync() {
                self.capture_failure = Some(format!("sync: {error}"));
                self.capture = None;
            }
        }
        self.report_capture_failure(sink);
        self.publish_status();
        Some(line)
    }

    /// Drain the port until `stop()` says so or the read fails.
    ///
    /// A zero-length read is the VTIME timeout (nothing arrived this second) —
    /// not EOF for a tty — so it only ticks. The daemon stops on disable or
    /// ACC-off; the desk binary passes a deadline.
    pub fn run<R: Read>(
        &mut self,
        io: R,
        clock: impl Fn() -> u64,
        sink: &mut dyn Sink,
        stop: impl Fn() -> bool,
    ) -> io::Result<()> {
        self.run_loop(io, clock, sink, stop, false, false, |_, _, _, _| {})
    }

    /// Product loop: also check disable/ACC after a blocking read, before
    /// capturing its bytes. Unlike the desk single-pass callback, `stop` here
    /// is a state query and may be called more than once per iteration.
    pub fn run_interruptible<R: Read>(
        &mut self,
        io: R,
        clock: impl Fn() -> u64,
        sink: &mut dyn Sink,
        stop: impl Fn() -> bool,
    ) -> io::Result<()> {
        if stop() { return Ok(()); }
        self.run_loop(io, clock, sink, stop, true, false, |_, _, _, _| {})
    }

    /// Product loop variant that gives a single read/write fd to a bounded
    /// request pump.  RX parsing remains in this same loop; the pump is called
    /// before each read and must make at most one nonblocking write attempt.
    pub fn run_with_requests<R: Read + Write, P: FnMut(&mut R, u64, &mut Self, &mut dyn Sink)>(
        &mut self,
        io: R,
        clock: impl Fn() -> u64,
        sink: &mut dyn Sink,
        stop: impl Fn() -> bool,
        pump: P,
    ) -> io::Result<()> {
        if stop() {
            return Ok(());
        }
        self.run_loop(io, clock, sink, stop, true, true, pump)
    }

    fn run_loop<R: Read, P: FnMut(&mut R, u64, &mut Self, &mut dyn Sink)>(
        &mut self,
        mut io: R,
        clock: impl Fn() -> u64,
        sink: &mut dyn Sink,
        stop: impl Fn() -> bool,
        check_after_read: bool,
        request_mode: bool,
        mut pump: P,
    ) -> io::Result<()> {
        let mut buf = [0u8; READ_CHUNK];
        self.tick(clock(), sink);
        while !stop() {
            pump(&mut io, clock(), self, sink);
            if request_mode && stop() { break; }
            match io.read(&mut buf) {
                Ok(0) if request_mode => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Ok(0) => {}
                Ok(n) => {
                    if check_after_read && stop() { break; }
                    let now = clock();
                    self.feed(&buf[..n], now, sink);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(e),
            }
            self.tick(clock(), sink);
        }
        Ok(())
    }

    fn record_frame(&mut self, frame: &Frame, decoded: &Decoded, ok: bool) {
        let mut fields = Vec::new();
        for s in &decoded.signals {
            fields.push((
                s.name.as_str(),
                Json::Raw(json_object(&[
                    ("raw", Json::Int(s.raw)),
                    ("value", Json::Str(s.scaled_text())),
                    ("unit", Json::Str(s.unit.clone())),
                    ("verified", Json::Bool(s.verified)),
                ])),
            ));
        }
        let mut pairs: Vec<(&str, Json)> = vec![
            ("raw", Json::Str(encode_hex(&frame.raw))),
            ("type", Json::Int(i64::from(frame.message_type))),
            ("length", Json::Int(frame.data.len() as i64)),
            ("data", Json::Str(encode_hex(&frame.data))),
            ("checksum", Json::Int(i64::from(frame.checksum))),
            ("checksum_ok", Json::Bool(ok)),
        ];
        if !ok {
            pairs.push(("expected_checksum", Json::Int(i64::from(frame.expected_checksum))));
        }
        let profile = self.profile_id().to_owned();
        pairs.push(("profile", Json::Str(profile)));
        if !decoded.groups.is_empty() {
            pairs.push(("semantic", Json::Str(decoded.groups.join("+"))));
        }
        if let Some(key) = &decoded.key {
            pairs.push((
                "key",
                Json::Raw(json_object(&[
                    ("code", Json::Int(i64::from(key.code))),
                    ("state", Json::Int(i64::from(key.state))),
                    ("state_name", Json::Str(key.state_name.clone().unwrap_or_default())),
                    ("action", Json::Str(key.action.clone().unwrap_or_default())),
                ])),
            ));
        }
        if !fields.is_empty() {
            pairs.push(("fields", Json::Raw(json_object(&fields))));
        }
        self.record(if ok { "frame" } else { "bad_checksum" }, &pairs);
    }

    fn record(&mut self, event: &str, pairs: &[(&str, Json)]) {
        if let Some(monitor) = &self.monitor {
            if let Ok(mut m) = monitor.lock() {
                // Capture remains useful historical evidence across a vehicle
                // switch, but live UI records are generation-scoped.
                if m.vehicle.revision() == self.profile_revision {
                    m.record(event, pairs);
                }
            }
        }
        if let Some(cap) = &mut self.capture {
            // A full /data is not a reason to stop reading; the counters and
            // the tell still work.
            if let Err(error) = cap.record(event, pairs) {
                self.capture_failure = Some(error.to_string());
                self.capture = None;
            }
        }
    }

    /// Persist a monitor-owned host request event without recording it twice
    /// in the monitor's live history.  A successful host write is not vehicle
    /// acceptance; the event carries that distinction explicitly.
    pub(super) fn record_request_event(
        &mut self,
        event: &super::monitor::RequestEvent,
        sink: &mut dyn Sink,
    ) {
        let kind = event.kind.map_or_else(|| "batch".to_owned(), |kind| format!("{kind:?}"));
        self.record_capture_only(
            event.event,
            &[
                ("request_id", Json::Int(event.request_id as i64)),
                ("kind", Json::Str(kind.clone())),
                ("host_write", Json::Bool(
                    event.event == "write_attempt"
                        || event.event == "write_complete"
                        || event.event == "partial"
                        || event.event == "error",
                )),
                ("vehicle_accepted", Json::Str("unknown".into())),
                ("detail", Json::Str(event.detail.clone())),
                ("requested_raw", Json::Str(event.raw.clone().unwrap_or_default())),
                ("written_bytes", Json::Int(event.written.map_or(-1, |value| value as i64))),
            ],
        );
        let level = event.event == "partial" || event.event == "error";
        sink.line(level, &format!("canuart: request {} id={} kind={} detail={}", event.event, event.request_id, kind, event.detail));
        self.report_capture_failure(sink);
    }

    pub(super) fn record_control_event(&mut self, event: &super::controls::ControlEvent, sink: &mut dyn Sink) {
        self.record_capture_only(event.event, &event.pairs);
        sink.line(event.event == "control_write_failed", &format!("canuart: {} {}", event.event, json_object(&event.pairs)));
        self.report_capture_failure(sink);
    }

    fn record_capture_only(&mut self, event: &str, pairs: &[(&str, Json)]) {
        if let Some(cap) = &mut self.capture {
            if let Err(error) = cap.record(event, pairs) {
                self.capture_failure = Some(error.to_string());
                self.capture = None;
            }
        }
    }

    /// Preserve a requested-but-unavailable sink as stopped, not unrequested.
    pub(super) fn capture_creation_failed(&mut self, error: io::Error, sink: &mut dyn Sink) {
        self.capture_failure = Some(format!("create: {error}"));
        self.report_capture_failure(sink);
    }

    fn report_capture_failure(&mut self, sink: &mut dyn Sink) {
        if let Some(error) = self.capture_failure.take() {
            self.capture_stopped = true;
            sink.line(true, &format!("canuart: capture stopped: {error}; RX continues"));
            if let Some(monitor) = &self.monitor {
                if let Ok(mut m) = monitor.lock() {
                    if m.vehicle.revision() == self.profile_revision {
                        m.record("capture_error", &[("detail", Json::Str(error))]);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::envelope::encode_frame;
    use super::*;
    use std::fs;
    use std::io::Cursor;

    #[test]
    fn capture_budget_stops_once_without_stopping_rx_or_deleting_old_files() {
        let dir = std::env::temp_dir().join(format!("can-budget-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let old = dir.join("owner-capture.jsonl");
        fs::write(&old, "owner evidence").unwrap();
        let cap = Capture::create_bounded(&dir, 512).unwrap();
        let path = cap.path().to_owned();
        let mut reader = Reader::new(None, Some(cap), "p", "m", 38400);
        let mut sink = VecSink::default();
        for n in 0..100 {
            reader.feed(&encode_frame(0x41, &[1, 3]), n, &mut sink);
        }
        assert_eq!(reader.stats.frames_ok, 100);
        assert!(reader.capture_path().is_none());
        assert!(reader.tell(100).ends_with("capture=stopped"));
        assert!(fs::metadata(&path).unwrap().len() <= 512);
        assert!(fs::metadata(&path).unwrap().len() > 0);
        assert_eq!(sink.0.iter().filter(|(_, s)| s.contains("capture stopped: capture budget exhausted")).count(), 1);
        assert_eq!(fs::read_to_string(old).unwrap(), "owner evidence");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn capture_write_error_is_reported_once_and_rx_continues() {
        let cap = Capture::at(std::path::Path::new("/dev/full")).unwrap();
        let mut reader = Reader::new(None, Some(cap), "p", "m", 38400);
        let mut sink = VecSink::default();
        for n in 0..3 { reader.feed(&encode_frame(0x41, &[1, 3]), n, &mut sink); }
        assert_eq!(reader.stats.frames_ok, 3);
        assert_eq!(sink.0.iter().filter(|(_, s)| s.contains("capture stopped:")).count(), 1);
        assert!(reader.capture_path().is_none());
    }

    #[test]
    fn stop_after_a_blocking_read_discards_the_late_chunk() {
        struct DisableOnRead<'a>(&'a std::cell::Cell<bool>);
        impl Read for DisableOnRead<'_> {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                out[..4].copy_from_slice(&[0x2e, 0x10, 0, 0xef]);
                self.0.set(true);
                Ok(4)
            }
        }
        let stop = std::cell::Cell::new(false);
        let mut reader = Reader::new(None, None, "p", "m", 38400);
        reader.run_interruptible(DisableOnRead(&stop), || 0, &mut VecSink::default(), || stop.get()).unwrap();
        assert_eq!(reader.stats.bytes, 0);
    }

    fn profile() -> Profile {
        Profile::parse(include_str!("../../can/profiles/xinpu-vw-pq.toml")).unwrap()
    }

    #[test]
    fn generic_reader_uses_selected_framing_and_drops_partial_old_frames() {
        let mut generated = profile();
        generated.id = "oem-vag-7021017".into();
        generated.envelope = "hiworld-5aa5".into();
        generated.keys = None;
        generated.request = None;
        generated.messages.clear();
        let mut reader = Reader::new(Some(generated), None, "desk", "4a94000", 38400);
        let mut sink = VecSink::default();
        let frame = super::super::envelope::Framing::Hiworld5aa5.encode(0x20, &[1, 1]).unwrap();
        reader.feed(&frame, 0, &mut sink);
        assert_eq!(reader.stats.frames_ok, 1);
        assert!(reader.snapshot.last_key().is_none());
        reader.feed(&frame[..3], 1, &mut sink);
        reader.switch_profile(Some(profile()), 1, &mut sink);
        reader.feed(&frame[3..], 2, &mut sink);
        assert_eq!(reader.stats.frames_ok, 0);
        reader.feed(&encode_frame(0x20, &[1, 1]), 3, &mut sink);
        assert_eq!(reader.stats.frames_ok, 1);
        assert!(reader.snapshot.last_key().is_some());
    }

    #[test]
    fn xinpu_audi_packed_reader_is_receive_only_without_vw_key_inheritance() {
        let mut audi = profile();
        audi.id = "oem-vag-2001001".into();
        audi.envelope = "xinpu-aa55-packed-rx".into();
        audi.keys = None;
        audi.request = None;
        audi.messages.clear();
        let mut reader = Reader::new(Some(audi), None, "desk", "4a94000", 38400);
        let frames = reader.feed(&[0xaa, 0x55, 6, 0, 0, 7, 0, 0, 0, 12], 0, &mut VecSink::default());
        assert!(matches!(frames.as_slice(), [Outcome::Frame(f, _)] if f.message_type == 0 && f.data[2] == 7));
        assert_eq!(reader.stats.frames_ok, 1);
        assert!(reader.snapshot.last_key().is_none());
    }

    #[test]
    fn feed_counts_decodes_and_tells() {
        let mut r = Reader::new(Some(profile()), None, "/dev/ttyHS3", "4a94000", 38400);
        let mut sink = VecSink::default();
        let mut bytes = vec![0x99, 0x98];
        bytes.extend(encode_frame(0x41, &[0x01, 0x03]));
        bytes.extend(encode_frame(0x20, &[0x01, 0x01]));
        let mut bad = encode_frame(0x41, &[0x01, 0x03]);
        *bad.last_mut().unwrap() ^= 0xff;
        bytes.extend(bad);
        let outcomes = r.feed(&bytes, 1000, &mut sink);
        assert_eq!(r.stats.frames_ok, 2);
        assert_eq!(r.stats.frames_bad, 1);
        // 2 leading noise bytes, plus the 5 bytes after the bad candidate's
        // header: the parser advances ONE byte past a bad checksum (an
        // embedded real header could follow) and the remainder has no 0x2e,
        // so it is discarded -- by design, and the capture keeps every byte.
        assert_eq!(r.stats.discarded, 7);
        assert!(matches!(outcomes[0], Outcome::Discarded(2)));
        assert_eq!(r.snapshot.get("door_front_left").unwrap().0.raw, 1);
        assert_eq!(r.snapshot.last_key().unwrap().0.action.as_deref(), Some("VOLUME_UP"));
        assert_eq!(sink.0.len(), 1, "exactly one 'first valid frame' line");
        assert!(sink.0[0].1.starts_with("canuart: first valid frame type=0x41"));
        let tell = r.tell(3000);
        assert_eq!(
            tell,
            format!("canuart: port=/dev/ttyHS3 mmio=4a94000 baud=38400 rx={} ok=2 bad=1 discard=7 last=2000 profile=xinpu-vw-pq fields=5 capture=off", bytes.len())
        );
    }

    /// Records what the reader hands to [`Sink::key`].
    #[derive(Default)]
    struct KeySink {
        lines: usize,
        keys: Vec<(u8, u8, Option<String>, u64)>,
    }
    impl Sink for KeySink {
        fn line(&mut self, _error: bool, _message: &str) {
            self.lines += 1;
        }
        fn key(&mut self, key: &KeyEvent, now_ms: u64) {
            self.keys.push((key.code, key.state, key.state_name.clone(), now_ms));
        }
    }

    #[test]
    fn only_valid_key_frames_reach_the_key_hook_in_the_reader_clock() {
        let mut r = Reader::new(Some(profile()), None, "p", "m", 38400);
        let mut sink = KeySink::default();
        let mut bytes = encode_frame(0x41, &[0x01, 0x03]);
        bytes.extend(encode_frame(0x20, &[0x01, 0x02]));
        let mut bad_key = encode_frame(0x20, &[0x02, 0x01]);
        *bad_key.last_mut().unwrap() ^= 0xff;
        bytes.extend(bad_key);
        r.feed(&bytes, 1234, &mut sink);
        r.feed(&encode_frame(0x20, &[0x01, 0x00]), 1500, &mut sink);
        assert_eq!(
            sink.keys,
            vec![
                (0x01, 0x02, Some("long".to_owned()), 1234),
                (0x01, 0x00, Some("up".to_owned()), 1500),
            ],
            "the door frame and the bad-checksum key frame must not reach the hook"
        );
        // Envelope-only (no profile): no key frame is recognised, none handed on.
        let mut bare = Reader::new(None, None, "p", "m", 38400);
        let mut none = KeySink::default();
        bare.feed(&encode_frame(0x20, &[0x01, 0x01]), 1, &mut none);
        assert!(none.keys.is_empty());
    }

    #[test]
    fn a_sink_without_a_key_method_is_unchanged_by_key_frames() {
        // VecSink (desk binaries, tests) keeps the default no-op: the same lines
        // as before the hook existed.
        let mut r = Reader::new(Some(profile()), None, "p", "m", 38400);
        let mut sink = VecSink::default();
        r.feed(&encode_frame(0x20, &[0x01, 0x01]), 1, &mut sink);
        r.feed(&encode_frame(0x20, &[0x01, 0x00]), 2, &mut sink);
        assert_eq!(sink.0.len(), 1);
        assert!(sink.0[0].1.starts_with("canuart: first valid frame type=0x20"));
    }

    #[test]
    fn tell_says_never_before_the_first_byte_and_is_rate_limited() {
        let mut r = Reader::new(None, None, "/dev/ttyHS3", "4a94000", 38400);
        let mut sink = VecSink::default();
        assert!(r.tell(0).contains("last=never profile=none"));
        assert!(r.tick(0, &mut sink).is_some());
        assert!(r.tick(TELL_INTERVAL_MS - 1, &mut sink).is_none());
        assert!(r.tick(TELL_INTERVAL_MS, &mut sink).is_some());
        assert_eq!(sink.0.len(), 2);
    }

    #[test]
    fn envelope_only_mode_counts_but_decodes_nothing() {
        let mut r = Reader::new(None, None, "p", "m", 38400);
        let mut sink = VecSink::default();
        r.feed(&encode_frame(0x41, &[0x02; 13]), 5, &mut sink);
        assert_eq!(r.stats.frames_ok, 1);
        assert!(r.snapshot.is_empty());
    }

    #[test]
    fn run_over_a_split_stream_reassembles_and_stops_on_deadline() {
        let mut stream = Vec::new();
        for _ in 0..5 {
            stream.extend(encode_frame(0x26, &[0x34, 0x12]));
        }
        let cursor = Cursor::new(stream);
        let mut r = Reader::new(Some(profile()), None, "p", "m", 38400);
        let mut sink = VecSink::default();
        let counter = std::cell::Cell::new(0u64);
        let clock = || {
            counter.set(counter.get() + 1);
            counter.get()
        };
        // Cursor returns Ok(0) at EOF forever; stop after a few ticks.
        let stop = || counter.get() > 40;
        r.run(cursor, clock, &mut sink, stop).unwrap();
        assert_eq!(r.stats.frames_ok, 5);
        assert_eq!(r.snapshot.get("steering_angle_raw").unwrap().0.raw, 0x1234);
    }

    #[test]
    fn capture_holds_every_byte_decoded_or_not() {
        let dir = std::env::temp_dir().join(format!("bengal-hud-can-reader-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let cap = Capture::create(&dir).unwrap();
        let mut r = Reader::new(Some(profile()), Some(cap), "p", "m", 38400);
        let mut sink = VecSink::default();
        let mut bytes = vec![0x00];
        bytes.extend(encode_frame(0x41, &[0x02, 0x0b, 0xb8, 0x13, 0x88, 0x05, 0x69, 0xff, 0xec, 0x01, 0xe2, 0x40, 0x2a]));
        bytes.extend(encode_frame(0x7e, &[9]));
        r.feed(&bytes, 10, &mut sink);
        r.tick(10, &mut sink);
        let path = r.capture_path().unwrap().to_path_buf();
        drop(r);
        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 5, "{text}");
        assert!(lines[0].contains("\"event\":\"rx\""));
        assert!(lines[0].contains(&format!("\"raw\":\"{}\"", encode_hex(&bytes))));
        let lines = &lines[1..];
        assert!(lines[0].contains("\"event\":\"discard\"") && lines[0].contains("\"raw\":\"00\""));
        assert!(lines[1].contains("\"event\":\"frame\"") && lines[1].contains("\"semantic\":\"vehicle\""));
        assert!(lines[1].contains("\"battery_voltage\":{\"raw\":1385,\"value\":\"13.85\",\"unit\":\"V\",\"verified\":false}"));
        assert!(lines[2].contains("\"type\":126") && !lines[2].contains("semantic"), "undecoded frame keeps raw only: {}", lines[2]);
        assert!(lines[3].contains("\"event\":\"tell\""));
        // Concatenated raw fields == every byte fed, in order.
        let mut raw_hex = String::new();
        for line in &lines[..3] {
            let start = line.find("\"raw\":\"").unwrap() + 7;
            let end = line[start..].find('"').unwrap() + start;
            raw_hex.push_str(&line[start..end]);
        }
        assert_eq!(raw_hex, encode_hex(&bytes));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn observer_keeps_partial_rx_and_updates_counts_when_capture_is_unavailable() {
        let monitor = std::sync::Arc::new(std::sync::Mutex::new(super::super::monitor::Monitor::new("test".into())));
        let mut reader = Reader::new(None, None, "p", "m", 38400);
        let mut sink = VecSink::default();
        reader.capture_creation_failed(io::Error::from_raw_os_error(28), &mut sink);
        reader.observe(monitor.clone());
        reader.feed(&[0x2e, 0x41], 3, &mut sink);
        let m = monitor.lock().unwrap();
        assert_eq!(m.stats.bytes, 2);
        assert_eq!(m.stats.frames_ok, 0);
        assert_eq!(m.capture_state, "stopped");
        let rows = m.batch(0, 32).unwrap().1;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].json.contains("\"raw\":\"2e41\""));
    }

    #[test]
    fn feed_drops_bytes_from_the_old_vehicle_generation_after_selection() {
        let dir = std::env::temp_dir().join(format!("bengal-can-epoch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("vehicles.tsv"),
            "id\tmakerId\tmaker\tbrandId\tbrand\tmodel\tstatus\tprofile\n"
                .to_owned()
                + "2009002\t2\tXinPu\t9\tVolkswagen\tPQ\tavailable\txinpu-vw-pq\n",
        )
        .unwrap();
        let mut monitor = super::super::monitor::Monitor::new("epoch".into());
        monitor.vehicle = super::super::selection::VehicleSelection::load(
            &dir.join("vehicles.tsv"),
            &dir.join("selected"),
        );
        let monitor = std::sync::Arc::new(std::sync::Mutex::new(monitor));
        let mut reader = Reader::new(Some(profile()), None, "p", "m", 38400);
        reader.observe(monitor.clone());
        monitor.lock().unwrap().select_vehicle("2009002").unwrap();
        let outcomes = reader.feed(&encode_frame(0x41, &[1, 3]), 1, &mut VecSink::default());
        assert!(outcomes.is_empty());
        assert_eq!(reader.stats.bytes, 0);
        assert_eq!(monitor.lock().unwrap().stats.bytes, 0);
        let _ = fs::remove_dir_all(dir);
    }
}
