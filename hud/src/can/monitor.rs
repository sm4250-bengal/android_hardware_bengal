// SPDX-License-Identifier: Apache-2.0
//! Bounded observation of the existing CAN reader; never owns a UART.

use super::capture::{json_object, rfc3339_utc, Json};
use super::envelope::encode_hex;
use super::requests::{OutboundRequest, RequestKind, RequestPolicy, XINPU_VW_PQ_USER};
use super::selection::VehicleSelection;
use super::controls::{self, ControlEvent, PendingControl};
#[cfg(test)]
use super::requests::REQUEST_TTL_MS;
use super::reader::Stats;
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

/// Fixed history capacity, independent of UI speed.
pub const RECORD_CAPACITY: usize = 256;
/// Upper bound on one Binder reply (32 * 8 KiB before UTF16 encoding).
pub const BATCH_LIMIT: usize = 32;
/// Maximum size of one diagnostic record.
pub const RECORD_LIMIT: usize = 8192;
/// Export is chunked; clients cannot allocate arbitrary amounts in the daemon.
pub const EXPORT_CHUNK: usize = 65536;
/// Shared observer; UART ownership stays with runtime::run.
pub type SharedMonitor = Arc<Mutex<Monitor>>;

/// Stable cursor plus complete JSON object.
#[derive(Clone, Debug)]
pub struct Record {
    /// Process-local sequence, starting at one.
    pub sequence: u64,
    /// Timestamped JSONL record, without the newline.
    pub json: String,
}

/// A request lifecycle event waiting for the reader to persist to disk.
#[derive(Clone, Debug)]
pub(crate) struct RequestEvent {
    /// Lifecycle name (`queued`, `cancelled`, `write_complete`, ...).
    pub event: &'static str,
    /// Binder-visible refresh id.
    pub request_id: u64,
    /// Request kind for per-frame events, if any.
    pub kind: Option<RequestKind>,
    /// Bounded diagnostic detail.
    pub detail: String,
    /// Actual bytes attempted on the host UART, when applicable.
    pub raw: Option<String>,
    /// Number reported by the host write syscall, when known.
    pub written: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
struct StatusRefresh {
    request_id: u64,
    remaining: usize,
}

const REQUEST_EVENT_CAPACITY: usize = 64;

#[cfg(test)]
mod control_tests {
    use super::*;

    fn selected(id: &str) -> Monitor {
        let dir = std::env::temp_dir().join(format!("can-controls-{}-{}", std::process::id(), SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("registry"), "id\tmakerId\tmaker\tbrandId\tbrand\tmodel\tstatus\tprofile\n2009002\t2\tXinPu\t9\tVolkswagen\tPQ\tavailable\txinpu-vw-pq\n1001001\t1\tRaise\t1\tVolkswagen\tPQ\tnot_ported\t\n1002001\t1\tRaise\t2\tToyota\tYaris\tnot_ported\t\n").unwrap();
        let mut m = Monitor::new("test".into());
        m.vehicle = VehicleSelection::load(&dir.join("registry"), &dir.join("selected"));
        m.select_vehicle(id).unwrap();
        // Persistence has been tested by selection.rs; this helper needs only
        // the loaded exact catalogue and selected ID after construction.
        fs::remove_dir_all(dir).unwrap();
        m.state(true, "listening", "test");
        m
    }

    fn queue(m: &mut Monitor, id: &str, value: i32) -> Result<u64, String> {
        m.set_vehicle_control(&m.vehicle_control_session(), m.vehicle.revision(), id, value)
    }

    #[test]
    fn controls_validate_session_model_bounds_and_never_confirm_host_writes() {
        let mut m = selected("2009002");
        assert!(queue(&mut m, "amp.volume", 3).is_err());
        m.set_control_gates(true, true, true, true);
        assert!(m.set_vehicle_control("old", m.vehicle.revision(), "amp.volume", 3).is_err());
        assert!(m.set_vehicle_control(&m.vehicle_control_session(), m.vehicle.revision() + 1, "amp.volume", 3).is_err());
        assert!(queue(&mut m, "parking.mode", 1).is_err());
        assert!(queue(&mut m, "amp.volume", 31).is_err());
        assert!(queue(&mut m, "amp.volume", -1).is_err());
        queue(&mut m, "amp.volume", 3).unwrap();
        let mut wire = Vec::new();
        assert!(m.write_vehicle_control(&mut wire, 0));
        assert_eq!(wire, super::super::envelope::encode_frame(0xa0, &[0, 3]));
        assert!(m.control_values.is_empty(), "host write cannot confirm observed state");
        let caps = m.vehicle_capabilities_json();
        assert!(caps.len() < 65536 && caps.contains("\"verified\":false") && caps.contains("\"value\":null"));
        m.observe_vehicle_controls(m.vehicle.revision() + 1, 0x27, &[0, 3, 0, 0, 0, 0, 0, 0], 1);
        m.observe_vehicle_controls(m.vehicle.revision(), 0x27, &[0, 3], 1);
        assert!(m.control_values.is_empty());
        m.observe_vehicle_controls(m.vehicle.revision(), 0x27, &[0, 3, 0, 0, 0, 0, 0, 0], 2);
        assert_eq!(m.control_values.get("amp.volume"), Some(&(3, 2)));
        assert!(m.drain_control_events().iter().any(|e| e.event == "control_write_complete"));
    }

    #[test]
    fn capability_reply_never_exceeds_bound_even_for_an_invalid_session_label() {
        let mut m = selected("2009002");
        m.session = "\\\"".repeat(65536);
        let caps = m.vehicle_capabilities_json();
        assert!(caps.len() <= 65536);
        assert!(caps.contains("Capabilities exceed JSON bound"));
        assert!(caps.contains("\"writable\":false"));
    }

    #[test]
    fn second_profile_observes_only_its_fields_and_cannot_refresh_vw() {
        let mut m = selected("1001001");
        assert_eq!(m.vehicle.active_profile(), Some("raise-vw-pq"));
        assert!(m.vehicle_capabilities_json().contains("\"refresh_supported\":false"));
        m.set_request_gates(true, true, true, false);
        m.set_control_gates(true, true, true, true);
        assert!(m.request_polo_status_refresh().is_err());
        assert!(queue(&mut m, "amp.volume", 1).is_err());
        assert!(queue(&mut m, "parking.mode", 2).is_err());
        queue(&mut m, "parking.mode", 1).unwrap();
        let mut wire = Vec::new();
        assert!(m.write_vehicle_control(&mut wire, 0));
        assert_eq!(wire, super::super::envelope::encode_frame(0xc6, &[2, 1]));
        m.observe_vehicle_controls(m.vehicle.revision(), 0x27, &[0; 8], 0);
        assert!(m.control_values.is_empty());
        m.observe_vehicle_controls(m.vehicle.revision(), 0x25, &[3, 0], 1);
        assert_eq!(m.control_values.get("parking.radar_sound"), Some(&(1, 1)));
        assert!(!m.control_values.contains_key("parking.mode"), "write-only has no fabricated RX");
        assert!(selected("1002001").vehicle.active_profile().is_none());
    }

    #[test]
    fn controls_cancel_on_every_gate_bound_queue_and_share_refresh_rate_limit() {
        let mut m = selected("2009002");
        let mut wire = Vec::new();
        for gates in [(false, true, true, true), (true, false, true, true), (true, true, false, true), (true, true, true, false)] {
            m.set_control_gates(true, true, true, true);
            queue(&mut m, "amp.volume", 3).unwrap();
            m.set_control_gates(gates.0, gates.1, gates.2, gates.3);
            assert!(!m.write_vehicle_control(&mut wire, 0));
            assert!(m.control_queue.is_empty());
        }
        m.set_control_gates(true, true, true, true);
        for _ in 0..controls::QUEUE_CAPACITY { queue(&mut m, "amp.volume", 3).unwrap(); }
        assert!(queue(&mut m, "amp.volume", 3).unwrap_err().contains("full"));
        assert!(m.write_vehicle_control(&mut wire, 0));
        assert!(!m.write_vehicle_control(&mut wire, 249));
        assert!(m.write_vehicle_control(&mut wire, 250));
        assert!(!m.write_vehicle_control(&mut wire, 10_000));
        assert!(m.control_queue.is_empty());
        m.set_request_gates(true, true, true, true);
        m.request_polo_status_refresh().unwrap();
        assert!(m.poll_status_request(499).is_none());
        assert!(m.poll_status_request(500).is_some());
        queue(&mut m, "amp.volume", 3).unwrap();
        assert!(!m.write_vehicle_control(&mut wire, 749));
        let old_session = m.vehicle_control_session();
        m.reset_request_session("reconnect");
        assert!(m.control_queue.is_empty());
        assert_ne!(m.vehicle_control_session(), old_session);
        m.set_control_gates(true, true, true, true);
        assert!(m.set_vehicle_control(&old_session, m.vehicle.revision(), "amp.volume", 3).is_err());
        assert!(!m.write_vehicle_control(&mut wire, 750));
    }

    #[test]
    fn partial_write_cancels_queue_without_retry_and_rx_disable_is_immediate() {
        struct Partial(usize);
        impl std::io::Write for Partial {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> { self.0 += 1; Ok(1) }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
        }
        let mut m = selected("2009002");
        m.set_control_gates(true, true, true, true);
        queue(&mut m, "amp.volume", 3).unwrap();
        queue(&mut m, "amp.volume", 4).unwrap();
        let mut io = Partial(0);
        assert!(m.write_vehicle_control(&mut io, 0));
        assert!(!m.write_vehicle_control(&mut io, 250));
        assert_eq!(io.0, 1);
        assert!(m.control_queue.is_empty());
        let events = m.drain_control_events();
        assert!(events.iter().any(|e| e.event == "control_write_failed"));
        assert!(events.iter().any(|e| e.event == "control_cancelled"));
        m.set_control_gates(true, true, true, true);
        queue(&mut m, "amp.volume", 3).unwrap();
        m.requested = Some(false);
        assert!(!m.write_vehicle_control(&mut io, 500));
    }
}

/// Pure bounded state exposed by the Android adapter.
pub struct Monitor {
    /// User-selected vehicle; empty by default so no car profile is assumed.
    pub vehicle: VehicleSelection,
    /// Distinguishes daemon restarts from an empty polling batch.
    pub session: String,
    base_session: String,
    /// Explicit UI override; None preserves the original default-off property.
    pub requested: Option<bool>,
    /// Effective request sampled by the reader.
    pub enabled: bool,
    /// disabled / waiting / listening / acc_off / error.
    pub state: String,
    /// Why data is unavailable, if known.
    pub detail: String,
    /// Counters from the sole parser.
    pub stats: Stats,
    /// Basename only, not an arbitrary export path.
    pub capture_name: String,
    /// off / active / stopped.
    pub capture_state: String,
    records: VecDeque<Record>,
    sequence: u64,
    started: Instant,
    request_policy: RequestPolicy,
    status_refresh: Option<StatusRefresh>,
    next_request_id: u64,
    request_events: VecDeque<RequestEvent>,
    request_session_reset: bool,
    dropped_request_events: u64,
    control_queue: VecDeque<PendingControl>,
    control_events: VecDeque<ControlEvent>,
    control_values: std::collections::BTreeMap<String, (i32, u64)>,
    control_reason: String,
    control_epoch: u64,
    control_session_reset: bool,
    last_tx_ms: Option<u64>,
}

impl Monitor {
    /// Create without opening any device or capture.
    pub fn new(session: String) -> Self {
        Self { base_session: session.clone(), session, vehicle: VehicleSelection::default(), requested: None, enabled: false,
            state: "disabled".into(), detail: "Receive-only monitoring is stopped".into(),
            stats: Stats::default(), capture_name: String::new(), capture_state: "off".into(),
            records: VecDeque::new(), sequence: 0, started: Instant::now(),
            request_policy: RequestPolicy::default(), status_refresh: None,
            next_request_id: 1, request_events: VecDeque::new(), request_session_reset: false,
            dropped_request_events: 0, control_queue: VecDeque::new(), control_events: VecDeque::new(),
            control_values: std::collections::BTreeMap::new(), control_reason: "CAN writer unavailable".into(),
            control_epoch: 0, control_session_reset: true, last_tx_ms: None }
    }

    /// Clock shared with the reader, not a wall-clock duration calculation.
    pub fn elapsed_ms(&self) -> u64 { self.started.elapsed().as_millis() as u64 }

    /// Last byte age, or -1 before any received byte.
    pub fn last_rx_age_ms(&self) -> i64 {
        self.stats.last_rx_ms.map_or(-1, |t| self.elapsed_ms().saturating_sub(t) as i64)
    }

    /// Change status without replenishing the recording budget.
    pub fn state(&mut self, enabled: bool, state: &str, detail: &str) {
        self.enabled = enabled;
        if self.state != state || self.detail != detail {
            self.state = state.into();
            self.detail = detail.chars().take(1024).collect();
            self.record("state", &[("state", Json::Str(self.state.clone())),
                ("detail", Json::Str(self.detail.clone()))]);
        }
    }

    /// Append to bounded live history, also when disk recording has stopped.
    pub fn record(&mut self, event: &str, pairs: &[(&str, Json)]) {
        self.sequence += 1;
        let mut all = vec![("sequence", Json::Int(self.sequence as i64)),
            ("time", Json::Str(rfc3339_utc(SystemTime::now()))),
            ("event", Json::Str(event.into()))];
        all.extend_from_slice(pairs);
        let mut json = json_object(&all);
        if json.len() > RECORD_LIMIT {
            json = json_object(&[("sequence", Json::Int(self.sequence as i64)),
                ("event", Json::Str("oversize_record".into())),
                ("original_bytes", Json::Int(json.len() as i64))]);
        }
        self.records.push_back(Record { sequence: self.sequence, json });
        if self.records.len() > RECORD_CAPACITY { self.records.pop_front(); }
    }

    /// Earliest available cursor, or zero when no record exists.
    pub fn oldest(&self) -> u64 { self.records.front().map_or(0, |r| r.sequence) }
    /// Latest recorded cursor, not necessarily included in a limited batch.
    pub fn latest(&self) -> u64 { self.sequence }

    /// Persist and apply a vehicle selection. A changed selection immediately
    /// invalidates pending TX and records the transition; runtime observes the
    /// revision and resets the old reader/parser before reading again.
    pub fn select_vehicle(&mut self, id: &str) -> Result<(), String> {
        let old_profile = self.vehicle.active_profile().unwrap_or("none");
        let changed = self.vehicle.select(id)?;
        if changed {
            let new_profile = self.vehicle.active_profile().unwrap_or("none");
            let revision = self.vehicle.revision();
            self.session = format!("{}-vehicle-{}", self.base_session, revision);
            self.records.clear();
            self.sequence = 0;
            self.stats = Stats::default();
            self.control_values.clear();
            self.reset_request_session("vehicle selection changed");
            self.state(false, "switching", "Vehicle selection changed; waiting for the CAN reader");
            self.record("vehicle_changed", &[
                ("selection_id", Json::Str(id.into())),
                ("old_profile", Json::Str(old_profile.into())),
                ("profile", Json::Str(new_profile.into())),
                ("revision", Json::Int(revision as i64)),
            ]);
        }
        Ok(())
    }

    fn vehicle_control_session(&self) -> String {
        format!("{}-tx-{}", self.session, self.control_epoch)
    }

    /// Bounded capabilities for the exact selected model; values are RX observations.
    pub fn vehicle_capabilities_json(&self) -> String {
        let profile = self.vehicle.active_profile().and_then(controls::profile);
        let reason = if profile.is_none() { "Selected controls are not implemented" }
            else if self.requested == Some(false) || !self.enabled { "CAN monitoring is stopped" }
            else { &self.control_reason };
        let controls_json = profile.map_or_else(String::new, |p| p.controls.iter().map(|c| c.json(self.control_values.get(&c.id).copied(), self.elapsed_ms(), reason)).collect::<Vec<_>>().join(","));
        let reply = json_object(&[("schema", Json::Int(1)), ("session", Json::Str(self.vehicle_control_session())),
            ("monitor_session", Json::Str(self.session.clone())),
            ("revision", Json::Int(self.vehicle.revision() as i64)),
            ("selection_id", Json::Str(self.vehicle.selected_id().unwrap_or("").into())),
            ("profile", Json::Str(self.vehicle.active_profile().unwrap_or("").into())),
            ("implemented", Json::Bool(profile.is_some())), ("verified", Json::Bool(false)),
            ("writable", Json::Bool(profile.is_some_and(|p| p.controls.iter().any(|c| c.writable())) && reason.is_empty())), ("reason", Json::Str(reason.into())),
            ("refresh_supported", Json::Bool(self.vehicle.active_profile() == Some("xinpu-vw-pq"))),
            ("controls", Json::Raw(format!("[{controls_json}]")))]);
        if reply.len() > 64 * 1024 {
            // Fail the reply explicitly; never return a silently partial list.
            return json_object(&[("schema", Json::Int(1)), ("implemented", Json::Bool(false)),
                ("writable", Json::Bool(false)), ("reason", Json::Str("Capabilities exceed JSON bound".into())),
                ("controls", Json::Raw("[]".into()))]);
        }
        reply
    }

    /// Queue a typed, session/revision-bound user command. Never writes here.
    pub fn set_vehicle_control(&mut self, session_id: &str, selection_revision: u64, control_id: &str, value: i32) -> Result<u64, String> {
        if session_id != self.vehicle_control_session() || selection_revision != self.vehicle.revision() {
            return Err("stale vehicle control session or selection revision".into());
        }
        if self.requested == Some(false) || !self.enabled { return Err("CAN monitoring is stopped".into()); }
        if !self.control_reason.is_empty() { return Err(self.control_reason.clone()); }
        let profile = self.vehicle.active_profile().and_then(controls::profile).ok_or("selected controls are not implemented")?;
        let control = profile.controls.iter().find(|c| c.id == control_id).ok_or("unknown control for selected model")?;
        let raw = control.frame(value)?;
        self.expire_vehicle_controls(self.elapsed_ms());
        if self.control_queue.len() >= controls::QUEUE_CAPACITY { return Err("vehicle control queue full".into()); }
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        let pending = PendingControl { request_id, control_id: control_id.into(), value, raw,
            session: self.vehicle_control_session(), revision: selection_revision,
            expires: self.elapsed_ms().saturating_add(controls::TTL_MS) };
        self.control_event("control_queued", &pending, "queued; vehicle acceptance unknown", None);
        self.control_queue.push_back(pending);
        Ok(request_id)
    }

    pub(crate) fn set_control_gates(&mut self, enabled: bool, acc_on: bool, se5: bool, profile_matches: bool) {
        let reason = if !enabled || self.requested == Some(false) { "CAN requests or monitoring disabled" }
            else if !acc_on { "ACC off or unknown" }
            else if !se5 { "SE5 identity unproven" }
            else if !profile_matches { "profile changed or unavailable" }
            else { "" };
        if !reason.is_empty() { self.cancel_vehicle_controls(reason); }
        self.control_reason = reason.into();
        if reason.is_empty() { self.control_session_reset = false; }
    }

    pub(crate) fn observe_vehicle_controls(&mut self, revision: u64, message_type: u8, data: &[u8], now: u64) {
        if revision != self.vehicle.revision() { return; }
        let Some(profile) = self.vehicle.active_profile().and_then(controls::profile) else { return; };
        for control in &profile.controls {
            if let Some(value) = control.observe(message_type, data) { self.control_values.insert(control.id.clone(), (value, now)); }
        }
    }

    fn expire_vehicle_controls(&mut self, now: u64) {
        let mut live = VecDeque::new();
        while let Some(pending) = self.control_queue.pop_front() {
            if pending.expires <= now { self.control_event("control_cancelled", &pending, "expired", None); }
            else { live.push_back(pending); }
        }
        self.control_queue = live;
    }

    fn cancel_vehicle_controls(&mut self, reason: &str) {
        while let Some(pending) = self.control_queue.pop_front() {
            self.control_event("control_cancelled", &pending, reason, None);
        }
    }

    /// Called only by the sole UART owner under the monitor lock. It performs
    /// one nonblocking write with no retries, so cancellation is linearized.
    pub(crate) fn write_vehicle_control(&mut self, io: &mut impl std::io::Write, now: u64) -> bool {
        self.expire_vehicle_controls(now);
        if !self.control_reason.is_empty() || self.requested == Some(false) || !self.enabled
            || self.last_tx_ms.is_some_and(|last| now.saturating_sub(last) < controls::INTERVAL_MS) { return false; }
        let Some(pending) = self.control_queue.pop_front() else { return false; };
        if pending.session != self.vehicle_control_session() || pending.revision != self.vehicle.revision()
            || !self.vehicle.active_profile().and_then(controls::profile).is_some_and(|p| p.controls.iter().any(|c| c.id == pending.control_id && c.frame(pending.value).ok().as_ref() == Some(&pending.raw))) {
            self.control_event("control_cancelled", &pending, "session or profile changed", None);
            return false;
        }
        self.last_tx_ms = Some(now);
        self.control_event("control_write_attempt", &pending, "host write attempted; vehicle acceptance unknown", Some(0));
        match io.write(&pending.raw) {
            Ok(written) if written == pending.raw.len() => self.control_event("control_write_complete", &pending, "host write complete; vehicle acceptance unknown", Some(written)),
            Ok(written) => {
                self.control_event("control_write_failed", &pending, "partial host write; no retry", Some(written));
                self.reset_request_session("partial control write");
            }
            Err(error) => {
                self.control_event("control_write_failed", &pending, &format!("host write failed: {error}; no retry"), None);
                self.reset_request_session("failed control write");
            }
        }
        true
    }

    fn control_event(&mut self, event: &'static str, pending: &PendingControl, detail: &str, written: Option<usize>) {
        let pairs = vec![("request_id", Json::Int(pending.request_id as i64)), ("control_id", Json::Str(pending.control_id.clone())),
            ("value", Json::Int(i64::from(pending.value))), ("detail", Json::Str(detail.chars().take(512).collect())),
            ("raw", Json::Str(encode_hex(&pending.raw))), ("written", Json::Int(written.map_or(-1, |n| n as i64))),
            ("vehicle_accepted", Json::Str("unknown".into()))];
        self.record(event, &pairs);
        if self.control_events.len() >= REQUEST_EVENT_CAPACITY { self.control_events.pop_front(); }
        self.control_events.push_back(ControlEvent { event, pairs });
    }

    pub(crate) fn drain_control_events(&mut self) -> Vec<ControlEvent> { self.control_events.drain(..).collect() }

    /// Queue exactly one six-request Polo status refresh and return its id.
    /// Runtime updates the policy gates before this is called; no bytes are
    /// written here.
    pub fn request_polo_status_refresh(&mut self) -> Result<u64, String> {
        if self.status_refresh.is_some() {
            return Err("status refresh busy".into());
        }
        let kinds = [
            RequestKind::CarInfoVehicle,
            RequestKind::CarInfoAlerts,
            RequestKind::Tyres,
            RequestKind::Amplifier,
            RequestKind::Settings,
            RequestKind::MediaBox,
        ];
        self.request_policy
            .enqueue_batch(&kinds, self.elapsed_ms())
            .map_err(|error| format!("status refresh unavailable: {error}"))?;
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        self.status_refresh = Some(StatusRefresh { request_id, remaining: kinds.len() });
        self.request_event("queued", request_id, None, "six typed status requests", None, None);
        Ok(request_id)
    }

    /// Cancel a pending refresh, recording a bounded reason in live history.
    pub fn cancel_status_refresh(&mut self, reason: &str) {
        if let Some(refresh) = self.status_refresh.take() {
            self.request_policy.cancel_pending();
            self.request_policy.reconnect();
            self.request_event("cancelled", refresh.request_id, None, reason, None, None);
        }
    }

    /// Update request gates immediately before a potential host write.
    pub(crate) fn set_request_gates(
        &mut self,
        enabled: bool,
        acc_on: bool,
        se5_proven: bool,
        profile_matches: bool,
    ) {
        if self.status_refresh.is_some()
            && !(enabled && acc_on && se5_proven && profile_matches)
        {
            let reason = if !enabled { "request property off" }
                else if !acc_on { "ACC-off or unknown" }
                else if !se5_proven { "SE5 identity unproven" }
                else { "profile changed" };
            self.cancel_status_refresh(reason);
        }
        self.request_policy.set_enabled(enabled);
        self.request_policy.set_acc_on(acc_on);
        self.request_policy.set_se5_proven(se5_proven);
        self.request_policy.set_profile_user(if profile_matches {
            Some(XINPU_VW_PQ_USER)
        } else {
            None
        });
        if enabled && acc_on && se5_proven && profile_matches {
            self.request_session_reset = false;
        }
    }

    /// Clear all request gates and start a fresh session after a close/failure.
    pub(crate) fn reset_request_session(&mut self, reason: &str) {
        self.cancel_vehicle_controls(reason);
        self.control_values.clear();
        if !self.control_session_reset {
            self.control_epoch = self.control_epoch.wrapping_add(1);
        }
        self.control_session_reset = true;
        self.control_reason = reason.chars().take(512).collect();
        let had_pending = self.status_refresh.is_some()
            || self.request_policy.pending_len(self.elapsed_ms()) != 0;
        self.cancel_status_refresh(reason);
        if !had_pending && !self.request_session_reset {
            self.request_policy.reconnect();
        }
        self.request_session_reset = true;
        self.request_policy.set_enabled(false);
        self.request_policy.set_acc_on(false);
        self.request_policy.set_se5_proven(false);
        self.request_policy.set_profile_user(None);
    }

    /// Pop one request for the current session without holding the monitor
    /// lock across a subsequent write syscall.
    pub(crate) fn poll_status_request(&mut self, now_ms: u64) -> Option<(u64, OutboundRequest)> {
        if self.last_tx_ms.is_some_and(|last| now_ms.saturating_sub(last) < controls::INTERVAL_MS) { return None; }
        let refresh = self.status_refresh?;
        let request = self.request_policy.poll(now_ms);
        if request.is_none() && self.request_policy.pending_len(now_ms) == 0 {
            self.cancel_status_refresh("status refresh expired");
        }
        request.map(|request| { self.last_tx_ms = Some(now_ms); (refresh.request_id, request) })
    }

    /// Recheck request identity/gates immediately before the write syscall.
    pub(crate) fn request_write_permitted(
        &self,
        request_id: u64,
        session: u64,
        vehicle_revision: u64,
        selected_profile: Option<&str>,
        enabled: bool,
        acc_on: bool,
        se5_proven: bool,
        profile_matches: bool,
    ) -> bool {
        enabled
            && acc_on
            && se5_proven
            && profile_matches
            && self.vehicle.revision() == vehicle_revision
            && self.vehicle.active_profile() == selected_profile
            && self.status_refresh.is_some_and(|refresh| refresh.request_id == request_id)
            && self.request_policy.session() == session
    }

    /// Record the exact bytes handed to the host write syscall.
    pub(crate) fn request_write_attempt(
        &mut self,
        request_id: u64,
        kind: RequestKind,
        raw: &[u8],
    ) {
        self.request_event("write_attempt", request_id, Some(kind), "host write attempted; vehicle acceptance unknown", Some(raw), Some(0));
    }

    /// Record one successful host write; vehicle acceptance remains unknown.
    pub(crate) fn request_write_complete(&mut self, request_id: u64, kind: RequestKind, raw: &[u8]) {
        self.request_event(
            "write_complete",
            request_id,
            Some(kind),
            "host write complete; vehicle acceptance unknown",
            Some(raw),
            Some(raw.len()),
        );
        if let Some(refresh) = &mut self.status_refresh {
            if refresh.request_id == request_id {
                refresh.remaining = refresh.remaining.saturating_sub(1);
                if refresh.remaining == 0 {
                    self.status_refresh = None;
                }
            }
        }
    }

    /// Record a partial or failed host write and cancel the rest of the batch.
    pub(crate) fn request_write_failed(
        &mut self,
        request_id: u64,
        kind: RequestKind,
        event: &'static str,
        detail: &str,
        raw: &[u8],
        written: Option<usize>,
    ) {
        self.request_policy.cancel_pending();
        self.request_policy.reconnect();
        self.status_refresh = None;
        self.request_event(event, request_id, Some(kind), detail, Some(raw), written);
    }

    /// Drain lifecycle events after releasing the monitor lock.
    pub(crate) fn drain_request_events(&mut self) -> Vec<RequestEvent> {
        let mut events = Vec::new();
        if self.dropped_request_events != 0 {
            let dropped = self.dropped_request_events;
            self.dropped_request_events = 0;
            let detail = format!("request event queue dropped {dropped} older event(s)");
            self.record("error", &[
                ("request_id", Json::Int(0)),
                ("kind", Json::Str("batch".into())),
                ("host_write", Json::Bool(false)),
                ("vehicle_accepted", Json::Str("unknown".into())),
                ("detail", Json::Str(detail.clone())),
                ("requested_raw", Json::Str(String::new())),
                ("written_bytes", Json::Int(-1)),
            ]);
            events.push(RequestEvent {
                event: "error",
                request_id: 0,
                kind: None,
                detail,
                raw: None,
                written: None,
            });
        }
        events.extend(self.request_events.drain(..));
        events
    }

    fn request_event(
        &mut self,
        event: &'static str,
        request_id: u64,
        kind: Option<RequestKind>,
        detail: &str,
        raw: Option<&[u8]>,
        written: Option<usize>,
    ) {
        let detail: String = detail.chars().take(512).collect();
        let kind_text = kind.map_or_else(|| "batch".to_owned(), |value| format!("{value:?}"));
        self.record(event, &[
            ("request_id", Json::Int(request_id as i64)),
            ("kind", Json::Str(kind_text)),
            ("host_write", Json::Bool(event == "write_attempt" || event == "write_complete" || event == "partial" || event == "error")),
            ("vehicle_accepted", Json::Str("unknown".into())),
            ("detail", Json::Str(detail.clone())),
            ("requested_raw", Json::Str(raw.map_or_else(String::new, encode_hex))),
            ("written_bytes", Json::Int(written.map_or(-1, |value| value as i64))),
        ]);
        if self.request_events.len() >= REQUEST_EVENT_CAPACITY {
            self.request_events.pop_front();
            self.dropped_request_events = self.dropped_request_events.saturating_add(1);
        }
        self.request_events.push_back(RequestEvent {
            event,
            request_id,
            kind,
            detail,
            raw: raw.map(encode_hex),
            written,
        });
    }

    /// Returns earliest unread records; slow clients detect overwritten history.
    pub fn batch(&self, after: i64, limit: i32) -> Result<(bool, Vec<Record>), &'static str> {
        if after < 0 || limit < 1 || limit as usize > BATCH_LIMIT { return Err("invalid cursor or limit"); }
        let after = after as u64;
        let gap = (after != 0 && after.saturating_add(1) < self.oldest()) || after > self.latest();
        let cursor = if after > self.latest() { 0 } else { after };
        Ok((gap, self.records.iter().filter(|r| r.sequence > cursor)
            .take(limit as usize).cloned().collect()))
    }
}

/// Only capture-generated basenames, no path traversal or alternate suffixes.
pub fn valid_log_name(name: &str) -> bool {
    let Some(body) = name.strip_prefix("can-").and_then(|s| s.strip_suffix(".jsonl")) else { return false; };
    let parts: Vec<_> = body.split('-').collect();
    name.len() <= 64 && (1..=2).contains(&parts.len())
        && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// List at most the newest 100 regular logs; existing files are never deleted.
pub fn logs(dir: &Path) -> io::Result<Vec<(String, u64)>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut result = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !valid_log_name(&name) || !entry.file_type()?.is_file() { continue; }
        let bytes = entry.metadata()?.len();
        if bytes > super::runtime::CAPTURE_BUDGET { continue; }
        result.push((name, bytes));
        result.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        result.truncate(100);
    }
    Ok(result)
}

/// Read bounded bytes from a regular, no-symlink capture, never an arbitrary path.
pub fn read_log(dir: &Path, name: &str, offset: i64, count: i32) -> io::Result<Vec<u8>> {
    if !valid_log_name(name) || offset < 0 || offset as u64 > super::runtime::CAPTURE_BUDGET
        || count < 1 || count as usize > EXPORT_CHUNK {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid log range"));
    }
    // Linux/Android O_NOFOLLOW | O_NONBLOCK: no following symlinks or blocking on FIFOs.
    let mut file = OpenOptions::new().read(true).custom_flags(0x20000 | 0x800).open(dir.join(name))?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > super::runtime::CAPTURE_BUDGET {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a bounded capture"));
    }
    file.seek(SeekFrom::Start(offset as u64))?;
    let size = (meta.len().saturating_sub(offset as u64) as usize).min(count as usize);
    let mut bytes = vec![0; size];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_history_reports_overrun_and_preserves_earliest_unread() {
        let mut m = Monitor::new("one".into());
        for _ in 0..300 { m.record("rx", &[]); }
        assert_eq!(m.records.len(), RECORD_CAPACITY);
        assert_eq!(m.oldest(), 45);
        let (gap, rows) = m.batch(1, 2).unwrap();
        assert!(gap); assert_eq!(rows[0].sequence, 45); assert_eq!(rows[1].sequence, 46);
        assert!(!m.batch(46, 2).unwrap().0);
        assert!(m.batch(999, 2).unwrap().0);
        assert!(m.batch(0, 33).is_err()); assert!(m.batch(-1, 1).is_err());
    }
    #[test]
    fn status_is_not_fake_received_traffic_and_records_are_bounded() {
        let mut m = Monitor::new("one".into());
        m.state(true, "waiting", "No SE5");
        assert_eq!(m.stats.bytes, 0); assert_eq!(m.last_rx_age_ms(), -1);
        m.record("frame", &[("raw", Json::Str("x".repeat(RECORD_LIMIT * 2)))]);
        assert!(m.records.back().unwrap().json.len() < RECORD_LIMIT);
    }

    #[test]
    fn status_refresh_is_one_batch_busy_until_cancelled_and_records_lifecycle() {
        let mut m = Monitor::new("one".into());
        m.set_request_gates(true, true, true, true);
        let id = m.request_polo_status_refresh().unwrap();
        assert!(m.poll_status_request(REQUEST_TTL_MS + 1).is_none());
        assert_eq!(m.request_polo_status_refresh().unwrap(), id.wrapping_add(1));
        assert_eq!(id, 1);
        assert_eq!(m.request_polo_status_refresh(), Err("status refresh busy".into()));
        m.cancel_status_refresh("test cancellation");
        assert!(m.request_polo_status_refresh().is_ok());
        let rows = m.batch(0, 32).unwrap().1;
        assert!(rows.iter().any(|row| row.json.contains("\"event\":\"queued\"")));
        assert!(rows.iter().any(|row| row.json.contains("\"event\":\"cancelled\"")));
    }

    #[test]
    fn partial_write_aborts_batch_and_marks_host_result_not_vehicle_acceptance() {
        let mut m = Monitor::new("one".into());
        m.set_request_gates(true, true, true, true);
        let id = m.request_polo_status_refresh().unwrap();
        let (_, request) = m.poll_status_request(0).unwrap();
        m.request_write_failed(id, request.kind, "partial", "host write 1/6 bytes; batch aborted", request.frame, Some(1));
        assert!(m.poll_status_request(251).is_none());
        let rows = m.batch(0, 32).unwrap().1;
        let partial = rows.iter().find(|row| row.json.contains("\"event\":\"partial\"")).unwrap();
        assert!(partial.json.contains("\"host_write\":true"));
        assert!(partial.json.contains("\"vehicle_accepted\":\"unknown\""));
        assert!(partial.json.contains("\"requested_raw\":\"2e900241022a\""));
        assert!(partial.json.contains("\"written_bytes\":1"));
    }

    #[test]
    fn vehicle_selection_cancels_dequeued_refresh_and_rejects_old_write() {
        let dir = std::env::temp_dir().join(format!("can-vehicle-monitor-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("vehicles.tsv"),
            "id\tmakerId\tmaker\tbrandId\tbrand\tmodel\tstatus\tprofile\n"
                .to_owned()
                + "2009002\t2\tXinPu\t9\tVolkswagen\tPQ\tavailable\txinpu-vw-pq\n",
        )
        .unwrap();
        let mut m = Monitor::new("selection".into());
        m.vehicle = VehicleSelection::load(&dir.join("vehicles.tsv"), &dir.join("selected"));
        m.set_request_gates(true, true, true, true);
        let request_id = m.request_polo_status_refresh().unwrap();
        let (_, request) = m.poll_status_request(0).unwrap();
        let old_revision = m.vehicle.revision();
        let old_profile = m.vehicle.active_profile();
        m.record("frame", &[("old_car_marker", Json::Bool(true))]);
        m.stats.bytes = 44;
        m.select_vehicle("2009002").unwrap();
        assert_eq!(m.stats, Stats::default());
        assert!(!m.batch(0, 32).unwrap().1.iter().any(|row| row.json.contains("old_car_marker")));
        assert!(m.poll_status_request(1).is_none());
        assert!(!m.request_write_permitted(
            request_id,
            request.session,
            old_revision,
            old_profile,
            true,
            true,
            true,
            true,
        ));
        assert!(m.batch(0, 32).unwrap().1.iter().any(|row| {
            row.json.contains("\"event\":\"cancelled\"")
                && row.json.contains("vehicle selection changed")
        }));
        for _ in 0..32 {
            m.select_vehicle("").unwrap();
            m.select_vehicle("2009002").unwrap();
        }
        assert!(m.session.len() < 40, "session label cannot grow on each switch");
        let _ = fs::remove_dir_all(dir);
    }
    #[test]
    fn export_name_refuses_traversal_and_wrong_files() {
        for s in ["can-123.jsonl", "can-123-2.jsonl"] { assert!(valid_log_name(s)); }
        for s in ["../can-1.jsonl", "can-.jsonl", "can-1-2-3.jsonl", "can-a.jsonl", "/can-1.jsonl"] {
            assert!(!valid_log_name(s));
        }
    }
    #[test]
    fn export_is_bounded_and_refuses_symlink_fifo_and_directory() {
        let dir = std::env::temp_dir().join(format!("can-monitor-export-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("can-1.jsonl"), b"one\ntwo\n").unwrap();
        std::os::unix::fs::symlink("can-1.jsonl", dir.join("can-2.jsonl")).unwrap();
        fs::create_dir(dir.join("can-3.jsonl")).unwrap();
        assert_eq!(read_log(&dir, "can-1.jsonl", 4, 4).unwrap(), b"two\n");
        assert!(read_log(&dir, "can-1.jsonl", 0, 65537).is_err());
        assert!(read_log(&dir, "can-1.jsonl", -1, 1).is_err());
        assert!(read_log(&dir, "can-2.jsonl", 0, 4).is_err());
        assert!(read_log(&dir, "can-3.jsonl", 0, 4).is_err());
        let path = std::ffi::CString::new(dir.join("can-4.jsonl").as_os_str().as_encoded_bytes()).unwrap();
        unsafe extern "C" { fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> i32; }
        assert_eq!(unsafe { mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(read_log(&dir, "can-4.jsonl", 0, 4).is_err());
        assert_eq!(logs(&dir).unwrap(), vec![("can-1.jsonl".into(), 8)]);
        fs::remove_dir_all(dir).unwrap();
    }
}
