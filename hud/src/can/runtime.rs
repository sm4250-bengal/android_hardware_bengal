// SPDX-License-Identifier: Apache-2.0
//! Default-off product integration. The desk CLI's name fallback is never authority here.

use std::path::{Path, PathBuf};
use std::io::Write;
use std::time::{Duration, Instant};

use super::capture::Capture;
use super::profile::Profile;
use super::reader::{Reader, Sink};
use super::{select_can_path, CAPTURE_DIR, DEFAULT_DEVICE, PROFILE_PATH, SE5_MMIO};

fn selection_state(monitor: Option<&super::monitor::SharedMonitor>) -> (u64, Option<&'static str>) {
    match monitor {
        // The unobserved desk-only legacy reader retains its explicit profile.
        None => (0, Some("xinpu-vw-pq")),
        Some(monitor) => monitor.lock().ok()
            .map(|m| (m.vehicle.revision(), m.vehicle.active_profile()))
            .unwrap_or((0, None)),
    }
}

fn load_profile(name: Option<&str>, sink: &mut dyn Sink) -> Option<Profile> {
    if name != Some("xinpu-vw-pq") {
        let generated = super::controls::profile(name?)?;
        // Only generated observations apply to this model: no VW keys,
        // dashboard messages, refreshes, or startup writes are inherited.
        return Some(Profile { id: generated.id.clone(), display: generated.display.clone(),
            baud: generated.baud, envelope: generated.envelope.clone(), request: None, verified: false,
            keys: None, messages: Vec::new() });
    }
    match std::fs::read_to_string(PROFILE_PATH)
        .map_err(|error| error.to_string())
        .and_then(|text| Profile::parse(&text).map_err(|error| error.to_string()))
    {
        Ok(profile) => Some(profile),
        Err(error) => {
            sink.line(true, &format!("canuart: profile unavailable: {error}; envelope-only"));
            None
        }
    }
}

fn selected_baud(name: Option<&str>) -> u32 {
    name.and_then(super::controls::profile).map_or(38_400, |p| p.baud)
}

/// Retry failed discovery/open at most once per thirty seconds.
pub const RETRY_MS: u64 = 30_000;
/// Maximum JSONL bytes for this process, across disable/ACC/reconnect cycles.
pub const CAPTURE_BUDGET: u64 = 8 * 1024 * 1024;
/// Poll disabled state without holding a tty or writing capture records.
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Scheduling state shared by the product loop and its host controls.
#[derive(Default)]
pub struct Gate {
    next_attempt_ms: u64,
    last_failure: Option<String>,
}

impl Gate {
    /// Unknown ACC is allowed; a confirmed ACC-off or absent/false property is not.
    pub fn active(enabled: bool, acc: Option<bool>) -> bool {
        enabled && acc != Some(false)
    }

    /// Reserve one attempt, including failures before the tty open.
    pub fn attempt(&mut self, now_ms: u64, enabled: bool, acc: Option<bool>) -> bool {
        if !Self::active(enabled, acc) || now_ms < self.next_attempt_ms {
            return false;
        }
        self.next_attempt_ms = now_ms.saturating_add(RETRY_MS);
        true
    }

    /// Emit a changed failure once; repeated ENOENT/identity failures stay quiet.
    pub fn failure(&mut self, error: String, sink: &mut dyn Sink) {
        if self.last_failure.as_ref() != Some(&error) {
            sink.line(true, &format!("canuart: unavailable: {error}; retry=30s"));
            self.last_failure = Some(error);
        }
    }
}

/// Require positive SE5 identity AND the exact node covered by product DAC/SELinux.
/// A configured name alone, a different SE5 minor, or a device symlink is refused.
pub fn product_path(sysfs: &Path, configured: &Path) -> Result<PathBuf, String> {
    if configured.as_os_str() != Path::new(DEFAULT_DEVICE).as_os_str() {
        return Err(format!("product CAN path must be exactly {DEFAULT_DEVICE}"));
    }
    let (path, authority) = select_can_path(sysfs, configured)?;
    if !authority.is_proven() {
        return Err(format!("SE5 MMIO=0x{SE5_MMIO} identity unproven; {DEFAULT_DEVICE} not opened"));
    }
    if path != Path::new(DEFAULT_DEVICE) {
        return Err(format!("SE5 resolved to {}, product policy covers only {DEFAULT_DEVICE}", path.display()));
    }
    Ok(path)
}

/// Run on its own HUD thread. `enabled` is a bounded property read; `acc` only
/// snapshots MCU state. No Binder callback, MCU I/O or vehicle action occurs here.
pub fn run(sysfs: &Path, configured: &Path, enabled: impl Fn() -> bool, acc: impl Fn() -> Option<bool>, sink: &mut dyn Sink) {
    run_inner(sysfs, configured, enabled, acc, sink, None)
}

/// Same single-reader loop, with a bounded UI observer and transient UI enable.
pub fn run_observed(sysfs: &Path, configured: &Path, enabled: impl Fn() -> bool,
    acc: impl Fn() -> Option<bool>, sink: &mut dyn Sink, monitor: super::monitor::SharedMonitor) {
    run_inner(sysfs, configured, enabled, acc, sink, Some(monitor))
}

/// Same single-reader loop with the explicitly opt-in, same-fd status writer.
/// The request property is sampled before opening and before every write;
/// changing its value causes the loop to close and reopen in the new mode.
pub fn run_observed_with_requests(
    sysfs: &Path,
    configured: &Path,
    enabled: impl Fn() -> bool,
    acc: impl Fn() -> Option<bool>,
    request_enabled: impl Fn() -> bool,
    sink: &mut dyn Sink,
    monitor: super::monitor::SharedMonitor,
) {
    run_inner_requests(sysfs, configured, enabled, acc, request_enabled, sink, monitor)
}

fn run_inner(sysfs: &Path, configured: &Path, property_enabled: impl Fn() -> bool,
    acc: impl Fn() -> Option<bool>, sink: &mut dyn Sink, monitor: Option<super::monitor::SharedMonitor>) {
    let started = Instant::now();
    let clock = || monitor.as_ref().and_then(|m| m.lock().ok().map(|m| m.elapsed_ms()))
        .unwrap_or_else(|| started.elapsed().as_millis() as u64);
    let enabled = || monitor.as_ref().and_then(|m| m.lock().ok().and_then(|m| m.requested))
        .unwrap_or_else(&property_enabled);
    let state = |on: bool, state: &str, detail: &str| {
        if let Some(m) = &monitor { if let Ok(mut m) = m.lock() { m.state(on, state, detail); } }
    };
    let mut gate = Gate::default();
    // A single Reader/capture for the process: toggles and reconnects cannot
    // replenish the budget or create unlimited empty files. Existing captures
    // are never removed. Capture creation failure is permanent for this process.
    let mut reader: Option<Reader> = None;
    loop {
        let on = enabled();
        let (vehicle_revision, selected_profile) = selection_state(monitor.as_ref());
        let active = on && selected_profile.is_some();
        if let Some(existing) = reader.as_mut() {
            if existing.profile_revision() != vehicle_revision {
                existing.switch_profile(load_profile(selected_profile, sink), vehicle_revision, sink);
            }
        }
        let acc_state = acc();
        if !on { state(false, "disabled", "Receive-only monitoring is stopped"); }
        else if selected_profile.is_none() { state(true, "disabled", "Selected profile is not ported; CAN is stopped"); }
        else if acc_state == Some(false) { state(true, "acc_off", "Paused while ACC is off"); }
        else if let Some(error) = &gate.last_failure { state(true, "error", error); }
        else { state(true, "waiting", "Waiting for verified SE5 UART; retry up to 30 seconds"); }
        if !gate.attempt(clock(), active, acc()) {
            std::thread::sleep(POLL_INTERVAL);
            continue;
        }
        let opened = product_path(sysfs, configured).and_then(|path| {
            super::port::open_product_readonly(&path, selected_baud(selected_profile))
                .map_err(|e| format!("open {}: {e}", path.display()))
        });
        let port = match opened {
            Ok(port) => port,
            Err(error) => { gate.failure(error, sink); continue; }
        };
        // Recheck after discovery/open, before creating any capture or reading.
        if !Gate::active(active, acc()) || selection_state(monitor.as_ref()).0 != vehicle_revision { continue; }
        gate.last_failure = None;
        let reader = reader.get_or_insert_with(|| {
            let profile = load_profile(selected_profile, sink);
            let mut reader = new_reader(profile, Capture::create_bounded(Path::new(CAPTURE_DIR), CAPTURE_BUDGET), sink);
            reader.set_profile_revision(vehicle_revision);
            if let Some(monitor) = &monitor { reader.observe(monitor.clone()); }
            reader
        });
        sink.line(false, "canuart: RX started authority=se5-mmio port=/dev/ttyHS3");
        state(true, "listening", &format!("SE5 /dev/ttyHS3 · {} 8N1 · RX only; no transmit or ACK", selected_baud(selected_profile)));
        let result = reader.run_interruptible(port, clock, sink, || {
            !Gate::active(enabled() && selection_state(monitor.as_ref()).1.is_some(), acc())
                || selection_state(monitor.as_ref()).0 != vehicle_revision
        });
        match result {
            Ok(()) => sink.line(false, "canuart: RX stopped (disabled or ACC-off)"),
            Err(error) => gate.failure(format!("read: {error}"), sink),
        }
        // Include a post-session cooldown: a long healthy session followed by
        // a failure must not reopen immediately or spin on a disconnected tty.
        gate.next_attempt_ms = if selection_state(monitor.as_ref()).0 != vehicle_revision {
            clock()
        } else {
            clock().saturating_add(RETRY_MS)
        };
    }
}

fn run_inner_requests(
    sysfs: &Path,
    configured: &Path,
    property_enabled: impl Fn() -> bool,
    acc: impl Fn() -> Option<bool>,
    request_enabled: impl Fn() -> bool,
    sink: &mut dyn Sink,
    monitor: super::monitor::SharedMonitor,
) {
    let started = Instant::now();
    let clock = || monitor.lock().ok().map(|m| m.elapsed_ms())
        .unwrap_or_else(|| started.elapsed().as_millis() as u64);
    let enabled = || monitor.lock().ok()
        .and_then(|m| m.requested)
        .unwrap_or_else(&property_enabled);
    let mut gate = Gate::default();
    let mut reader: Option<Reader> = None;
    loop {
        let rx_on = enabled();
        let (vehicle_revision, selected_profile) = selection_state(Some(&monitor));
        let active = rx_on && selected_profile.is_some();
        if let Some(existing) = reader.as_mut() {
            if existing.profile_revision() != vehicle_revision {
                existing.switch_profile(load_profile(selected_profile, sink), vehicle_revision, sink);
            }
        }
        let acc_state = acc();
        if !active {
            if let Ok(mut m) = monitor.lock() {
                m.reset_request_session(if !rx_on { "receive property off" } else { "selected profile is not ported" });
                m.state(false, "disabled", if !rx_on { "CAN monitoring and requests are stopped" } else { "Selected profile is not ported; CAN is stopped" });
            }
        } else if acc_state == Some(false) {
            if let Ok(mut m) = monitor.lock() {
                m.reset_request_session("ACC-off");
                m.state(true, "acc_off", "Paused while ACC is off");
            }
        } else if let Some(error) = &gate.last_failure {
            if let Ok(mut m) = monitor.lock() { m.state(true, "error", error); }
        } else if let Ok(mut m) = monitor.lock() {
            m.state(true, "waiting", "Waiting for verified SE5 UART; retry up to 30 seconds");
        }
        if !gate.attempt(clock(), active, acc_state) {
            drain_request_events(reader.as_mut(), &monitor, sink);
            std::thread::sleep(POLL_INTERVAL);
            continue;
        }
        let request_mode = request_enabled();
        let opened = product_path(sysfs, configured).and_then(|path| {
            let result = if request_mode {
                super::port::open_product_readwrite(&path, selected_baud(selected_profile))
            } else {
                super::port::open_product_readonly(&path, selected_baud(selected_profile))
            };
            result.map_err(|e| format!("open {}: {e}", path.display()))
        });
        let port = match opened {
            Ok(port) => port,
            Err(error) => {
                if let Ok(mut m) = monitor.lock() { m.reset_request_session("SE5 open/identity failure"); }
                gate.failure(error, sink);
                continue;
            }
        };
        if !enabled() || selected_profile.is_none() || acc() == Some(false)
            || request_enabled() != request_mode
            || selection_state(Some(&monitor)).0 != vehicle_revision
        {
            if let Ok(mut m) = monitor.lock() {
                m.reset_request_session("gate changed before session start");
            }
            continue;
        }
        gate.last_failure = None;
        let reader = reader.get_or_insert_with(|| {
            let profile = load_profile(selected_profile, sink);
            let mut reader = new_reader(profile, Capture::create_bounded(Path::new(CAPTURE_DIR), CAPTURE_BUDGET), sink);
            reader.set_profile_revision(vehicle_revision);
            reader.observe(monitor.clone());
            reader
        });
        sink.line(false, &format!("canuart: RX started authority=se5-mmio port=/dev/ttyHS3 request_fd={}", if request_mode { "O_RDWR|O_NONBLOCK" } else { "O_RDONLY" }));
        if request_mode {
            if let Ok(mut m) = monitor.lock() {
                m.state(true, "listening", &format!("SE5 /dev/ttyHS3 · {} 8N1 · bounded typed requests enabled", selected_baud(selected_profile)));
            }
        } else if let Ok(mut m) = monitor.lock() {
            m.state(true, "listening", &format!("SE5 /dev/ttyHS3 · {} 8N1 · RX only; requests off", selected_baud(selected_profile)));
        }
        let monitor_for_pump = monitor.clone();
        let monitor_for_stop = monitor.clone();
        let enabled_ref = &enabled;
        let request_enabled_ref = &request_enabled;
        let acc_ref = &acc;
        let mut pump = move |io: &mut std::fs::File, now_ms: u64, reader: &mut Reader, sink: &mut dyn Sink| {
            drain_request_events(Some(reader), &monitor_for_pump, sink);
            let profile_matches = selected_profile == Some("xinpu-vw-pq")
                && reader.profile_revision() == vehicle_revision
                && reader.profile_id() == "xinpu-vw-pq";
            let rx_enabled = enabled_ref();
            let request_on = request_enabled_ref();
            let acc_on = acc_ref() == Some(true);
            let request = {
                let Ok(mut m) = monitor_for_pump.lock() else { return; };
                let selection_matches = m.vehicle.revision() == vehicle_revision
                    && m.vehicle.active_profile() == selected_profile;
                m.set_request_gates(
                    request_mode && request_on && rx_enabled,
                    acc_on,
                    true,
                    profile_matches && selection_matches,
                );
                let controls_match = selection_matches && selected_profile.and_then(super::controls::profile).is_some()
                    && reader.profile_revision() == vehicle_revision
                    && Some(reader.profile_id()) == selected_profile;
                m.set_control_gates(request_mode && request_on && rx_enabled, acc_on, true, controls_match);
                // Same sole fd and same cancellation lock as refreshes; one
                // nonblocking write only. A shared rate gate covers both.
                if m.write_vehicle_control(io, now_ms) { return; }
                m.poll_status_request(now_ms)
            };
            let Some((request_id, request)) = request else { return; };
            let prewrite_rx = enabled_ref();
            let prewrite_request = request_enabled_ref();
            let prewrite_acc = acc_ref() == Some(true);
            // Serialize Binder cancellation with this ONE nonblocking write.
            // The private fd was opened O_NONBLOCK in captured request_mode;
            // never hold this lock across a blocking write/read or a retry.
            let Ok(mut m) = monitor_for_pump.lock() else { return; };
            let selection_matches = m.vehicle.revision() == vehicle_revision
                && m.vehicle.active_profile() == selected_profile;
            let permitted = m.request_write_permitted(
                request_id,
                request.session,
                vehicle_revision,
                selected_profile,
                request_mode && prewrite_request && prewrite_rx,
                prewrite_acc,
                true,
                profile_matches && selection_matches,
            );
            if !permitted {
                m.reset_request_session("request gate changed before write");
                drop(m);
                drain_request_events(Some(reader), &monitor_for_pump, sink);
                return;
            }
            let expected = request.frame.len();
            m.request_write_attempt(request_id, request.kind, request.frame);
            match io.write(request.frame) {
                Ok(written) if written == expected => {
                    m.request_write_complete(request_id, request.kind, request.frame);
                }
                Ok(written) => {
                    m.request_write_failed(request_id, request.kind, "partial", &format!("host write {written}/{expected} bytes; batch aborted"), request.frame, Some(written));
                }
                Err(error) => {
                    m.request_write_failed(request_id, request.kind, "error", &format!("host write failed: {error}; batch aborted"), request.frame, None);
                }
            }
            drop(m);
            drain_request_events(Some(reader), &monitor_for_pump, sink);
        };
        let result = reader.run_with_requests(
            port,
            clock,
            sink,
            || !enabled_ref() || acc_ref() == Some(false)
                || request_enabled_ref() != request_mode
                || selection_state(Some(&monitor_for_stop)).0 != vehicle_revision,
            &mut pump,
        );
        if let Ok(mut m) = monitor.lock() {
            m.reset_request_session("ACC-off, disabled, or request mode changed");
        }
        drain_request_events(Some(reader), &monitor, sink);
        match result {
            Ok(()) => sink.line(false, "canuart: RX stopped (disabled, ACC-off, or request mode changed)"),
            Err(error) => gate.failure(format!("read: {error}"), sink),
        }
        gate.next_attempt_ms = if request_enabled_ref() != request_mode
            || selection_state(Some(&monitor)).0 != vehicle_revision
        {
            clock()
        } else {
            clock().saturating_add(RETRY_MS)
        };
    }
}

fn drain_request_events(
    reader: Option<&mut Reader>,
    monitor: &super::monitor::SharedMonitor,
    sink: &mut dyn Sink,
) {
    let Some(reader) = reader else { return; };
    let events = monitor.lock().ok().map(|mut m| m.drain_request_events()).unwrap_or_default();
    for event in events {
        reader.record_request_event(&event, sink);
    }
    let controls = monitor.lock().ok().map(|mut m| m.drain_control_events()).unwrap_or_default();
    for event in controls { reader.record_control_event(&event, sink); }
}

fn new_reader(profile: Option<Profile>, capture: std::io::Result<Capture>, sink: &mut dyn Sink) -> Reader {
    let baud = profile.as_ref().map_or(38_400, |p| p.baud);
    match capture {
        Ok(capture) => {
            sink.line(false, &format!("canuart: capture={} budget={CAPTURE_BUDGET}", capture.path().display()));
            Reader::new(profile, Some(capture), DEFAULT_DEVICE, SE5_MMIO, baud)
        }
        Err(error) => {
            let mut reader = Reader::new(profile, None, DEFAULT_DEVICE, SE5_MMIO, baud);
            reader.capture_creation_failed(error, sink);
            reader
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::reader::VecSink;

    #[test]
    fn generated_raise_profile_has_no_vw_decoder_or_keys() {
        let profile = load_profile(Some("raise-vw-pq"), &mut VecSink::default()).unwrap();
        assert_eq!(profile.id, "raise-vw-pq");
        assert_eq!(profile.baud, 38400);
        assert!(profile.messages.is_empty());
        assert!(profile.keys.is_none());
        assert!(profile.request.is_none());
        assert!(!profile.verified);
        assert!(load_profile(Some("unknown"), &mut VecSink::default()).is_none());
    }

    #[test]
    fn every_compiled_generic_profile_keeps_exact_transport_and_no_xinpu_behavior() {
        for generated in super::super::controls::bundle().as_ref().unwrap() {
            if generated.id == "xinpu-vw-pq" { continue; }
            let profile = load_profile(Some(&generated.id), &mut VecSink::default()).unwrap();
            assert_eq!(profile.id, generated.id);
            assert_eq!(profile.baud, generated.baud);
            assert_eq!(profile.envelope, generated.envelope);
            assert!(profile.keys.is_none() && profile.messages.is_empty() && profile.request.is_none());
        }
    }

    #[test]
    fn create_failure_stays_stopped_in_every_tell() {
        let mut sink = VecSink::default();
        let reader = new_reader(None, Err(std::io::Error::from_raw_os_error(28)), &mut sink);
        assert!(sink.0.iter().any(|(_, text)| text.contains("capture stopped: create:")));
        assert!(reader.tell(0).ends_with("capture=stopped"));
        assert!(reader.tell(60_000).ends_with("capture=stopped"));
    }

    #[test]
    fn disabled_and_acc_off_never_reserve_an_open_and_retry_is_bounded() {
        let mut gate = Gate::default();
        assert!(!gate.attempt(0, false, None));
        assert!(!gate.attempt(0, true, Some(false)));
        assert!(gate.attempt(0, true, None));
        assert!(!gate.attempt(RETRY_MS - 1, true, Some(true)));
        assert!(gate.attempt(RETRY_MS, true, Some(true)));
        assert!(!Gate::active(false, Some(true)));
        assert!(!Gate::active(true, Some(false)));
    }

    #[test]
    fn repeated_failure_is_quiet_but_changed_failure_is_truthful() {
        let mut gate = Gate::default();
        let mut sink = VecSink::default();
        gate.failure("identity unproven".into(), &mut sink);
        gate.failure("identity unproven".into(), &mut sink);
        gate.failure("open: ENOENT".into(), &mut sink);
        assert_eq!(sink.0.len(), 2);
        assert!(sink.0[0].1.contains("unproven"));
        assert!(sink.0[1].1.contains("ENOENT"));
    }

    #[test]
    fn product_refuses_unproven_and_nonproduct_names_before_open() {
        let root = std::env::temp_dir().join(format!("can-runtime-identity-{}", std::process::id()));
        std::fs::create_dir_all(root.join("class/tty")).unwrap();
        assert!(product_path(&root, Path::new(DEFAULT_DEVICE)).unwrap_err().contains("unproven"));
        assert!(product_path(&root, Path::new("/dev/ttyHS2")).unwrap_err().contains("exactly"));
        std::fs::create_dir_all(root.join("class/tty/ttyHS3")).unwrap();
        std::os::unix::fs::symlink("../../../4a94000.serial", root.join("class/tty/ttyHS3/device")).unwrap();
        assert_eq!(product_path(&root, Path::new(DEFAULT_DEVICE)).unwrap(), Path::new(DEFAULT_DEVICE));
        std::fs::rename(root.join("class/tty/ttyHS3"), root.join("class/tty/ttyHS9")).unwrap();
        assert!(product_path(&root, Path::new(DEFAULT_DEVICE)).unwrap_err().contains("policy covers only"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
