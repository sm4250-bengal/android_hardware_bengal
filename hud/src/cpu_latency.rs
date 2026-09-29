//! CPU wake-up latency hold: keep `/dev/cpu_dma_latency` open at a bound while the
//! display is on, so the two `cpu-sleep` idle states of this DT (exit latency
//! 666 µs / 621 µs, `min-residency` 1182 / 1110 µs) are not entered between the
//! frame pipeline's cross-CPU hops.
//!
//! Why (measured, 2026-09-05, docs/research/2026-09-05-perf-parity-d3-0118.md §7.5):
//! with those two states disabled on all eight CPUs, SurfaceFlinger's missed frames
//! fell 26.6 % → 15.0 %, its long-GPU jank 162 → 107 and the app's jank 23 % → 16 %,
//! reproduced in two runs with the cpuidle counters as the control (1,285 deep
//! entries/s → 0). A 3× GPU clock moved the same numbers by ~10 %.
//!
//! Kernel contract (kernel/gki-clean, 6.18):
//!   * `kernel/power/qos.c:384-395` — a write of exactly `sizeof(s32)` bytes is the
//!     binary value (anything else is parsed as ASCII hex); the constraint lives as
//!     long as the fd is open and goes away on close.
//!   * Honoured by the idle governors (`drivers/cpuidle/governor.c:110-114`,
//!     `governors/menu.c:222`, `governors/teo.c:272`) and by the CPU power-domain
//!     governor (`drivers/pmdomain/governor.c:378`), i.e. by every AWAKE idle decision.
//!   * Ignored by suspend-to-idle: `drivers/cpuidle/cpuidle.c:192-201` picks
//!     `find_deepest_state(drv, dev, U64_MAX, 0, true)` and `drivers/pmdomain/core.c:
//!     1428-1429` picks "the deepest state when suspending" with no governor. So an fd
//!     still open when the unit suspends changes nothing about standby depth; the
//!     screen-off release below is about awake-idle power, not about standby.
//!
//! Cfg-free on purpose: policy, governor and the two adapters (`DrmDpms`, `DevNode`)
//! are host-testable against a fake sysfs tree and a temp file; `binder.rs` supplies
//! the property reader, the ACC state and the thread (mem:hud-binder-desk-test).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The kernel's PM-QoS CPU latency node (misc device 10:260).
pub const NODE_PATH: &str = "/dev/cpu_dma_latency";
/// Runtime knob, microseconds; absent → `DEFAULT_MICROS`; `0` → never hold.
/// `vendor_bengal_hud_config_prop` in property_contexts, so only vendor domains
/// (and a permissive userdebug shell) can set it — an A/B without a reflash.
pub const PROPERTY: &str = "vendor.bengal.hud.cpu_latency_us";
/// Below both `cpu-sleep` exit latencies (621/666 µs) and above WFI (1 µs): keeps
/// WFI available, excludes exactly the two states the experiment disabled.
pub const DEFAULT_MICROS: u32 = 500;
/// Display-state poll cadence. One sysfs read of ~4 bytes per second.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// After a failed open/write (typically EACCES before ueventd applied the node's
/// mode), retry no more often than this — one kmsg line per attempt.
pub const RETRY_BACKOFF_MS: u64 = 30_000;

/// Something that can carry one CPU latency constraint.
pub trait LatencyNode {
    /// Hold `micros`. Idempotent for an equal bound; a different bound re-arms.
    fn hold(&mut self, micros: u32) -> io::Result<()>;
    /// Drop the constraint (close the fd). No-op when nothing is held.
    fn release(&mut self);
    /// The bound currently held, if any.
    fn held(&self) -> Option<u32>;
}

/// The production node: one open fd on `/dev/cpu_dma_latency`.
pub struct DevNode {
    path: PathBuf,
    held: Option<(File, u32)>,
}

impl DevNode {
    /// A node at `path`, holding nothing yet.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            held: None,
        }
    }

    /// The production node, [`NODE_PATH`].
    pub fn default_node() -> Self {
        Self::new(NODE_PATH)
    }

    /// Where this node lives.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl LatencyNode for DevNode {
    fn hold(&mut self, micros: u32) -> io::Result<()> {
        if let Some((file, current)) = self.held.as_mut() {
            if *current == micros {
                return Ok(());
            }
            // Same fd, new value: qos.c's write path updates the existing request.
            file.write_all(&micros.to_le_bytes())?;
            *current = micros;
            return Ok(());
        }
        // Rust's std opens with O_CLOEXEC; write-only is all the node needs.
        let mut file = OpenOptions::new().write(true).open(&self.path)?;
        file.write_all(&micros.to_le_bytes())?;
        self.held = Some((file, micros));
        Ok(())
    }

    fn release(&mut self) {
        // Dropping the File closes the fd; the kernel removes the request on release.
        self.held = None;
    }

    fn held(&self) -> Option<u32> {
        self.held.as_ref().map(|(_, micros)| *micros)
    }
}

/// Whether the panel is being scanned out.
pub trait DisplayProbe {
    /// `Some(true)` on, `Some(false)` off, `None` unknown (unreadable / unparsable).
    fn is_on(&mut self) -> Option<bool>;
}

/// The DRM connector's legacy `dpms` attribute — the kernel's own record of
/// whether the CRTC is active. Set on every modeset by
/// `drivers/gpu/drm/drm_atomic_helper.c:1414-1422` (`connector->dpms = mode`,
/// ON iff `crtc->state->active`), read back by `drm_sysfs.c:242-251`, and the
/// screen-off path is a modeset: hwc3 `ComposerClient::setPowerMode(OFF)` →
/// `HwcDisplay::SetDisplayEnabled(false)` drops the CRTC's ACTIVE property.
/// The msm DSI connector is named `card<N>-DSI-1` (`drm_connector.c:110,267`);
/// on D3-0118 it is `card1-DSI-1` — `card0` is the render-only KMS node with no
/// connector (lead's read-only `ls /sys/class/drm`, 2026-09-05 03:2x), which is
/// why discovery walks every entry that has a `dpms` file instead of assuming
/// a card number.
pub struct DrmDpms {
    dpms: PathBuf,
}

impl DrmDpms {
    /// Find the panel's connector under `<sysfs>/class/drm`: the DSI connector if
    /// there is one, else the first connector whose `status` is `connected`.
    pub fn discover(sysfs_root: &Path) -> Option<Self> {
        let class = sysfs_root.join("class").join("drm");
        let mut connectors: Vec<PathBuf> = fs::read_dir(&class)
            .ok()?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.join("dpms").is_file())
            .collect();
        connectors.sort();
        let dsi = connectors.iter().find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.contains("-DSI-"))
                .unwrap_or(false)
        });
        let chosen = match dsi {
            Some(path) => path.clone(),
            None => connectors.into_iter().find(|path| {
                fs::read_to_string(path.join("status"))
                    .map(|status| status.trim() == "connected")
                    .unwrap_or(false)
            })?,
        };
        Some(Self {
            dpms: chosen.join("dpms"),
        })
    }

    /// A probe on an explicit `dpms` attribute (tests; no discovery).
    pub fn at(dpms: impl Into<PathBuf>) -> Self {
        Self { dpms: dpms.into() }
    }

    /// The `dpms` attribute being read.
    pub fn path(&self) -> &Path {
        &self.dpms
    }

    /// `drm_get_dpms_name()` spellings; anything else is "unknown", never "on".
    pub fn parse(text: &str) -> Option<bool> {
        match text.trim() {
            "On" => Some(true),
            "Off" | "Standby" | "Suspend" => Some(false),
            _ => None,
        }
    }
}

impl DisplayProbe for DrmDpms {
    fn is_on(&mut self) -> Option<bool> {
        fs::read_to_string(&self.dpms)
            .ok()
            .and_then(|text| Self::parse(&text))
    }
}

/// What the governor should do this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Hold the node at this many microseconds.
    Hold(u32),
    /// Hold nothing.
    Release,
}

/// The whole policy, pure:
///   * knob 0 → never hold (feature off);
///   * display not provably on (`Some(false)` or unknown) → release — never hold blind;
///   * ACC confirmed off (grace / final stage / suspending) → release early, before
///     the framework blanks the panel;
///   * ACC unknown → no veto: `Idle` is the normal state of a booted unit whose MCU has
///     not spoken yet, and the display being on is the primary truth.
pub fn decide(display_on: Option<bool>, acc_on: Option<bool>, micros: u32) -> Decision {
    if micros == 0 {
        return Decision::Release;
    }
    if display_on != Some(true) {
        return Decision::Release;
    }
    if acc_on == Some(false) {
        return Decision::Release;
    }
    Decision::Hold(micros)
}

/// A state change worth one kmsg line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// The constraint is now held at `micros`.
    Held {
        /// The bound written to the node.
        micros: u32,
    },
    /// The constraint was dropped.
    Released,
    /// Opening or writing the node failed; retried after [`RETRY_BACKOFF_MS`].
    HoldFailed {
        /// The bound that could not be applied.
        micros: u32,
        /// The OS error, for the one kmsg line.
        error: String,
    },
}

/// Applies decisions to a node, reporting only changes.
pub struct Governor<N: LatencyNode> {
    node: N,
    retry_after_ms: Option<u64>,
}

impl<N: LatencyNode> Governor<N> {
    /// A governor over `node`, holding nothing yet.
    pub fn new(node: N) -> Self {
        Self {
            node,
            retry_after_ms: None,
        }
    }

    /// The bound currently held, if any.
    pub fn held(&self) -> Option<u32> {
        self.node.held()
    }

    /// One tick. `now_ms` is any monotonic millisecond clock (the thread passes
    /// its uptime); it only paces the retry after a failed hold.
    pub fn step(&mut self, decision: Decision, now_ms: u64) -> Option<Transition> {
        match decision {
            Decision::Release => {
                self.retry_after_ms = None;
                if self.node.held().is_some() {
                    self.node.release();
                    Some(Transition::Released)
                } else {
                    None
                }
            }
            Decision::Hold(micros) => {
                if self.node.held() == Some(micros) {
                    return None;
                }
                if matches!(self.retry_after_ms, Some(at) if now_ms < at) {
                    return None;
                }
                match self.node.hold(micros) {
                    Ok(()) => {
                        self.retry_after_ms = None;
                        Some(Transition::Held { micros })
                    }
                    Err(error) => {
                        self.retry_after_ms = Some(now_ms.saturating_add(RETRY_BACKOFF_MS));
                        Some(Transition::HoldFailed {
                            micros,
                            error: error.to_string(),
                        })
                    }
                }
            }
        }
    }
}

/// Parse the knob's property text: decimal microseconds; garbage → `None`
/// (the caller falls back to `DEFAULT_MICROS`, never to "off").
pub fn parse_knob(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    text.parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct FakeNode {
        held: Option<u32>,
        fail: bool,
        holds: u32,
        releases: u32,
    }

    impl FakeNode {
        fn new() -> Self {
            Self {
                held: None,
                fail: false,
                holds: 0,
                releases: 0,
            }
        }
    }

    impl LatencyNode for FakeNode {
        fn hold(&mut self, micros: u32) -> io::Result<()> {
            if self.fail {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "EACCES"));
            }
            self.holds += 1;
            self.held = Some(micros);
            Ok(())
        }
        fn release(&mut self) {
            self.releases += 1;
            self.held = None;
        }
        fn held(&self) -> Option<u32> {
            self.held
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "bengal-hud-cpu-latency-{}-{}-{}",
                tag,
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn connector(root: &Path, name: &str, dpms: &str, status: &str) -> PathBuf {
        let dir = root.join("class").join("drm").join(name);
        fs::create_dir_all(&dir).expect("connector dir");
        fs::write(dir.join("dpms"), format!("{dpms}\n")).expect("dpms");
        fs::write(dir.join("status"), format!("{status}\n")).expect("status");
        dir
    }

    // ---- policy -----------------------------------------------------------

    #[test]
    fn decide_holds_only_when_display_is_provably_on() {
        assert_eq!(decide(Some(true), Some(true), 500), Decision::Hold(500));
        assert_eq!(decide(Some(true), None, 500), Decision::Hold(500));
        assert_eq!(decide(Some(false), Some(true), 500), Decision::Release);
        assert_eq!(decide(None, Some(true), 500), Decision::Release);
        assert_eq!(decide(None, None, 500), Decision::Release);
    }

    #[test]
    fn decide_releases_on_acc_off_and_on_knob_zero() {
        assert_eq!(decide(Some(true), Some(false), 500), Decision::Release);
        assert_eq!(decide(Some(true), Some(true), 0), Decision::Release);
    }

    #[test]
    fn knob_parses_decimal_microseconds_and_rejects_garbage() {
        assert_eq!(parse_knob("500"), Some(500));
        assert_eq!(parse_knob(" 0\n"), Some(0));
        assert_eq!(parse_knob(""), None);
        assert_eq!(parse_knob("0x1f4"), None);
        assert_eq!(parse_knob("-1"), None);
        assert_eq!(parse_knob("five hundred"), None);
    }

    // ---- governor ----------------------------------------------------------

    #[test]
    fn governor_holds_once_and_is_silent_while_nothing_changes() {
        let mut governor = Governor::new(FakeNode::new());
        assert_eq!(
            governor.step(Decision::Hold(500), 0),
            Some(Transition::Held { micros: 500 })
        );
        for tick in 1..10 {
            assert_eq!(governor.step(Decision::Hold(500), tick * 1000), None);
        }
        assert_eq!(governor.held(), Some(500));
        assert_eq!(governor.node.holds, 1);
    }

    #[test]
    fn governor_releases_on_release_and_reholds_after() {
        let mut governor = Governor::new(FakeNode::new());
        assert!(governor.step(Decision::Hold(500), 0).is_some());
        assert_eq!(
            governor.step(Decision::Release, 1000),
            Some(Transition::Released)
        );
        assert_eq!(governor.step(Decision::Release, 2000), None);
        assert_eq!(governor.held(), None);
        assert_eq!(
            governor.step(Decision::Hold(500), 3000),
            Some(Transition::Held { micros: 500 })
        );
        assert_eq!(governor.node.releases, 1);
        assert_eq!(governor.node.holds, 2);
    }

    #[test]
    fn governor_rearms_when_the_bound_changes() {
        let mut governor = Governor::new(FakeNode::new());
        assert!(governor.step(Decision::Hold(500), 0).is_some());
        assert_eq!(
            governor.step(Decision::Hold(300), 1000),
            Some(Transition::Held { micros: 300 })
        );
        assert_eq!(governor.held(), Some(300));
    }

    #[test]
    fn governor_reports_a_failed_hold_once_per_backoff_then_retries() {
        let mut node = FakeNode::new();
        node.fail = true;
        let mut governor = Governor::new(node);
        let first = governor.step(Decision::Hold(500), 0);
        assert!(matches!(
            first,
            Some(Transition::HoldFailed { micros: 500, .. })
        ));
        // Inside the backoff: silent, no attempt.
        assert_eq!(
            governor.step(Decision::Hold(500), RETRY_BACKOFF_MS - 1),
            None
        );
        // At the backoff: one more attempt, still failing, one more line.
        let second = governor.step(Decision::Hold(500), RETRY_BACKOFF_MS);
        assert!(matches!(second, Some(Transition::HoldFailed { .. })));
        // ueventd applied the mode: the next retry succeeds.
        governor.node.fail = false;
        assert_eq!(
            governor.step(Decision::Hold(500), 2 * RETRY_BACKOFF_MS),
            Some(Transition::Held { micros: 500 })
        );
        // A release inside a backoff clears it.
        governor.node.fail = true;
        assert_eq!(
            governor.step(Decision::Release, 2 * RETRY_BACKOFF_MS + 1),
            Some(Transition::Released)
        );
        assert!(matches!(
            governor.step(Decision::Hold(500), 2 * RETRY_BACKOFF_MS + 2),
            Some(Transition::HoldFailed { .. })
        ));
        assert_eq!(
            governor.step(Decision::Release, 2 * RETRY_BACKOFF_MS + 3),
            None
        );
        governor.node.fail = false;
        assert_eq!(
            governor.step(Decision::Hold(500), 2 * RETRY_BACKOFF_MS + 4),
            Some(Transition::Held { micros: 500 })
        );
    }

    // ---- DevNode against a regular file (the kernel node has no offsets; a
    // regular file records every write in order, which is what we assert) ------

    #[test]
    fn devnode_writes_exactly_four_little_endian_bytes_and_closes_on_release() {
        let temp = TempDir::new("node");
        let path = temp.path().join("cpu_dma_latency");
        fs::write(&path, b"").expect("node file");
        let mut node = DevNode::new(&path);
        assert_eq!(node.held(), None);
        node.hold(500).expect("hold");
        assert_eq!(node.held(), Some(500));
        assert_eq!(
            fs::read(&path).expect("read"),
            500u32.to_le_bytes().to_vec()
        );
        // Same bound: no second write.
        node.hold(500).expect("hold again");
        assert_eq!(fs::read(&path).expect("read").len(), 4);
        // New bound on the same fd: appended in a regular file, in-place in the kernel.
        node.hold(300).expect("re-arm");
        let bytes = fs::read(&path).expect("read");
        assert_eq!(bytes.len(), 8);
        assert_eq!(&bytes[4..], &300u32.to_le_bytes());
        assert_eq!(node.held(), Some(300));
        node.release();
        assert_eq!(node.held(), None);
        // After release a hold opens afresh.
        node.hold(500).expect("hold after release");
        assert_eq!(node.held(), Some(500));
    }

    #[test]
    fn devnode_reports_the_open_error_and_holds_nothing() {
        let temp = TempDir::new("missing");
        let mut node = DevNode::new(temp.path().join("absent"));
        let error = node.hold(500).expect_err("must fail");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(node.held(), None);
    }

    // ---- DrmDpms against a fake sysfs tree -------------------------------------

    #[test]
    fn dpms_parse_accepts_the_kernel_spellings_only() {
        assert_eq!(DrmDpms::parse("On\n"), Some(true));
        assert_eq!(DrmDpms::parse("Off\n"), Some(false));
        assert_eq!(DrmDpms::parse("Standby"), Some(false));
        assert_eq!(DrmDpms::parse("Suspend"), Some(false));
        assert_eq!(DrmDpms::parse("on"), None);
        assert_eq!(DrmDpms::parse(""), None);
        assert_eq!(DrmDpms::parse("1"), None);
    }

    #[test]
    fn dpms_discovery_prefers_the_dsi_connector() {
        let temp = TempDir::new("drm");
        connector(temp.path(), "card0-HDMI-A-1", "On", "connected");
        let dsi = connector(temp.path(), "card0-DSI-1", "Off", "connected");
        let mut probe = DrmDpms::discover(temp.path()).expect("connector");
        assert_eq!(probe.path(), dsi.join("dpms"));
        assert_eq!(probe.is_on(), Some(false));
        fs::write(dsi.join("dpms"), "On\n").expect("flip");
        assert_eq!(probe.is_on(), Some(true));
    }

    /// The unit's real layout (read 2026-09-05): `card0` and `card1` are KMS
    /// nodes without a `dpms` file, `renderD128` is the render node, and the
    /// panel hangs off `card1-DSI-1`. Pins that discovery never assumes card0.
    #[test]
    fn dpms_discovery_finds_the_dsi_connector_on_a_secondary_card() {
        let temp = TempDir::new("drm-card1");
        let class = temp.path().join("class").join("drm");
        for node in ["card0", "card1", "renderD128"] {
            fs::create_dir_all(class.join(node)).expect("card dir");
        }
        fs::write(class.join("card0").join("dev"), "226:0\n").expect("card0 dev");
        fs::write(class.join("card1").join("dev"), "226:1\n").expect("card1 dev");
        fs::write(class.join("renderD128").join("dev"), "226:128\n").expect("render dev");
        // Decoy: a CONNECTED, ON connector on card0 that sorts before the panel.
        // The "first connected" fallback would take it; only the DSI preference
        // reaches card1-DSI-1 — so a preference hard-coded to card0 goes red here.
        connector(temp.path(), "card0-HDMI-A-1", "On", "connected");
        let dsi = connector(temp.path(), "card1-DSI-1", "On", "connected");
        let mut probe = DrmDpms::discover(temp.path()).expect("card1-DSI-1 must be found");
        assert_eq!(probe.path(), dsi.join("dpms"));
        assert_eq!(probe.is_on(), Some(true));
        assert_eq!(
            decide(probe.is_on(), None, DEFAULT_MICROS),
            Decision::Hold(500)
        );
    }

    #[test]
    fn dpms_discovery_falls_back_to_the_connected_connector() {
        let temp = TempDir::new("drm-fallback");
        connector(temp.path(), "card0-HDMI-A-1", "Off", "disconnected");
        let panel = connector(temp.path(), "card0-eDP-1", "On", "connected");
        let probe = DrmDpms::discover(temp.path()).expect("connector");
        assert_eq!(probe.path(), panel.join("dpms"));
    }

    #[test]
    fn dpms_discovery_refuses_without_a_candidate_and_reads_unknown_when_gone() {
        let temp = TempDir::new("drm-none");
        assert!(DrmDpms::discover(temp.path()).is_none());
        connector(temp.path(), "card0-HDMI-A-1", "Off", "disconnected");
        assert!(DrmDpms::discover(temp.path()).is_none());
        let mut probe = DrmDpms::at(temp.path().join("nowhere").join("dpms"));
        assert_eq!(probe.is_on(), None);
        assert_eq!(decide(probe.is_on(), Some(true), 500), Decision::Release);
    }

    // ---- the whole chain, as the thread runs it -------------------------------

    #[test]
    fn screen_off_releases_within_one_tick_and_screen_on_reholds() {
        let temp = TempDir::new("chain");
        let dsi = connector(temp.path(), "card0-DSI-1", "On", "connected");
        let mut probe = DrmDpms::discover(temp.path()).expect("connector");
        let mut governor = Governor::new(FakeNode::new());
        let mut now = 0;
        let mut tick = |probe: &mut DrmDpms, governor: &mut Governor<FakeNode>, acc| {
            now += 1000;
            governor.step(decide(probe.is_on(), acc, DEFAULT_MICROS), now)
        };
        assert_eq!(
            tick(&mut probe, &mut governor, None),
            Some(Transition::Held { micros: 500 })
        );
        assert_eq!(tick(&mut probe, &mut governor, Some(true)), None);
        fs::write(dsi.join("dpms"), "Off\n").expect("blank");
        assert_eq!(
            tick(&mut probe, &mut governor, Some(true)),
            Some(Transition::Released)
        );
        fs::write(dsi.join("dpms"), "On\n").expect("unblank");
        assert_eq!(
            tick(&mut probe, &mut governor, Some(true)),
            Some(Transition::Held { micros: 500 })
        );
        // ACC off releases even with the panel still on (12 03 grace precedes the blank).
        assert_eq!(
            tick(&mut probe, &mut governor, Some(false)),
            Some(Transition::Released)
        );
        assert_eq!(tick(&mut probe, &mut governor, Some(false)), None);
    }
}
