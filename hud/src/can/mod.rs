// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! The vehicle-CAN decoder box on GENI SE5 — receive by default, bounded
//! typed status refresh and generated controls behind explicit runtime gates.
//!
//! # What the hardware is
//!
//! CAN never reaches this SoC as CAN. An external, car-specific decoder box
//! translates the vehicle bus into a proprietary **UART byte protocol**, on the
//! Raise/XinPu family at 38400 8N1. That family fact is OEM-derived and
//! corroborated by third-party sources; the box on **device 1** was labelled
//! Simple Soft `RP5-VW-001`. Device 3 now has a retained in-car capture (see
//! the September 11 VW parity report); physical interpretation stays unverified.
//! The product resolves GENI SE5, MMIO `0x4a94000`, TLMM 16 TX / 17 RX, and
//! requires `/dev/ttyHS3`. Historical stock `/dev/ttyHS2` numbering is not the
//! product's port authority. `/dev/ttyHS2` on our DT is Bluetooth
//! (docs/research/2026-09-05-canbus-oem-app-and-protocol.md §1.2, §5). The OEM
//! stack was `system_server`'s `CanbusMonitor` → `canbus_service` → a system-UID
//! host APK → one VirtualAPK plugin per box maker/brand; this module replaces
//! all of that with data profiles and a typed snapshot
//! (docs/issues/can/issues/03-hud-can-module.md).
//!
//! # Receive-only by default
//!
//! 1. **Type.** The legacy [`reader::Reader::run`] path is generic over
//!    [`std::io::Read`] only.  The separate request pump is typed `Read + Write`
//!    and is reached only by an explicit, bounded request property.
//! 2. **File.** [`port::open_readonly`] opens the tty `O_RDONLY | O_NOCTTY`; a
//!    write on that fd is `EBADF` before the kernel even looks at the tty.
//! 3. **Policy.** Exact `/dev/ttyHS3` has its own CAN device type. The request
//!    path opens one nonblocking read/write fd only when explicitly enabled.
//!
//! The OEM's per-frame `FF` acknowledgement and its `0x90` request are evidence
//! about OEM behaviour: this tree permits only the separately reviewed typed
//! status-request slice in [`requests`] and compiled scalar vehicle controls,
//! behind [`REQUESTS_PROPERTY`] and live ACC/SE5/profile/session gates. It has
//! no raw TX API, automatic ACK/startup replay or calibration route; host write
//! completion never claims vehicle receipt. Generated data and exact OEM model
//! linkage are in `can/generated/`; only those partial slices are implemented.
//!
//! # Port identity
//!
//! Resolved by MMIO through [`select_can_path`], never by `ttyHS` digit: on this
//! DT `ttyHS2` is *Bluetooth's* SE3 and `ttyHS1` the MCU's SE0
//! (`mem:mcu-on-se0-bt-on-se3`). Only [`PortAuthority::Se5Mmio`] is positive
//! identity.
//!
//! The desk CLI's [`PortAuthority::NameFallback`] is **UNPROVEN and is trusted** — that is the
//! whole point of naming it. It is not "never trusted": when sysfs proves
//! nothing, the configured name IS opened. What bounds that trust is only:
//!
//! * the name must survive an explicit refusal against every MMIO we have
//!   proven belongs to someone else (SE0, SE3), compared **after** lexical
//!   normalisation and symlink resolution, so an alias such as
//!   `/dev/can-box -> /dev/ttyHS1` cannot slip past the refusal; and
//! * the name must have the shape [`DEV_DIR`]`/ttyHS<N>` — this slice will not
//!   enable an arbitrary, unknown hardware port on somebody's say-so.
//!
//! Nothing in a name fallback proves SE5. The production reader checks MMIO
//! identity even though `/dev/ttyHS3` is now measured on device 3.
//!
//! # Product activation
//!
//! HUD starts [`runtime`] on its own thread, after the CPU-latency watcher.
//! [`ENABLE_PROPERTY`] defaults absent/false: no tty open or capture creation.
//! Production requires positive SE5 MMIO identity AND exact `/dev/ttyHS3`;
//! it never opens a name fallback. Disable/ACC-off closes the tty within the
//! existing one-second read timeout. JSONL is the evidence sink, limited to
//! 8 MiB for the process; capture failure stops capture while RX continues.
//! No profile field is verified. The only decoded value that can trigger an
//! action is a steering-wheel VOLUME_UP/VOLUME_DOWN key, and only with
//! [`KEYS_PROPERTY`] on ([`keys`], parity plan §4.1 step A); with it off (the
//! default) nothing decoded triggers anything.

pub mod capture;
mod controls;
pub mod monitor;
pub mod decode;
pub mod envelope;
pub mod keys;
pub mod port;
pub mod profile;
pub mod requests;
pub mod reader;
pub mod runtime;
pub mod selection;

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::mcu::{resolve_port, SE0_MMIO, SE3_MMIO_NOT_OURS};

/// SE5's MMIO address — the decoder box's UART. The port is resolved by this.
pub const SE5_MMIO: &str = "4a94000";

/// The name the `serial3` alias is *expected* to produce on a
/// `BENGAL_DT_CAN_UART=1` DT — **inferred from `qcom_geni_serial.c` and the
/// merged-tree check `kernel/scripts/dt-can-uart-check.py`, never flown and
/// therefore never measured.** Three lineages give this one engine three
/// different digits (stock `ttyHS2`, device-1 mainline `ttyHS1`, our knob
/// `ttyHS3`), which is exactly why identity comes from [`SE5_MMIO`] and this
/// constant is also the exact product DAC/SELinux contract, never identity alone.
pub const DEFAULT_DEVICE: &str = "/dev/ttyHS3";

/// Init's configured name, pinned by `check-bengal3-can-tty.sh`. Runtime still
/// requires positive MMIO identity before opening it.
pub const DEVICE_ENV: &str = "BENGAL_HUD_CAN_DEVICE";

/// Typed boolean property arming runtime; absent or false never opens the port.
pub const ENABLE_PROPERTY: &str = "persist.vendor.bengal.hud.can.enable";

/// Typed boolean opt-in for steering-wheel key injection ([`keys`]; step A =
/// VOLUME_UP/VOLUME_DOWN only). Absent or false: no key state machine step, no
/// uinput device, no log line -- the behaviour before [`keys`] existed. Read
/// on each key frame, so `setprop` acts without a HUD restart. Injection also
/// needs `ro.vendor.bengal.hud.inject_keys`, through the bezel path's gate.
/// No `property_contexts` line exists for it yet (the unit runs permissive);
/// enforcing needs `… vendor_bengal_hud_config_prop:s0 exact bool` beside
/// [`ENABLE_PROPERTY`]'s.
pub const KEYS_PROPERTY: &str = "persist.vendor.bengal.hud.can.keys";

/// Explicit, default-off opt-in for Polo status-request refreshes.  This does
/// not itself open a port or queue a request; runtime still requires positive
/// SE5 identity, ACC-on, and the exact XinPu/VW/PQ profile before every write.
pub const REQUESTS_PROPERTY: &str = "persist.vendor.bengal.hud.can.requests";

/// Production vehicle registry loaded by the owner before monitor startup.
pub const VEHICLE_REGISTRY_PATH: &str = "/vendor/etc/bengal/can/vehicles.tsv";
/// Persisted selected vehicle id, written atomically by [`selection`].
pub const VEHICLE_SELECTION_PATH: &str = "/data/vendor/bengal-hud/can-vehicle";

/// Product install path for the unverified XinPu profile.
pub const PROFILE_PATH: &str = "/vendor/etc/bengal/can/profiles/xinpu-vw-pq.toml";

/// Runtime capture directory. `init` creates
/// `/data/vendor/bengal-hud` 0700 system:system (`bengal-hud.rc` on `main`);
/// this subdirectory would inherit the parent's `vendor_bengal_hud_data_file`
/// label. Runtime creates at most one file per process, after a proven open.
pub const CAPTURE_DIR: &str = "/data/vendor/bengal-hud/can-capture";

/// Cadence of the `canuart:` tell line.
pub const TELL_INTERVAL_MS: u64 = 30_000;

/// The directory device nodes live in. Production is `/dev`; the unit tests
/// pass a fixture directory to [`select_can_path_under`] so the alias/shape
/// rules can be exercised without root and without touching the real `/dev`.
pub const DEV_DIR: &str = "/dev";

/// Which authority chose the port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PortAuthority {
    /// sysfs proved the node fronts SE5. Positive identity.
    Se5Mmio,
    /// sysfs proved **nothing** about the configured name, and it is being
    /// trusted anyway after surviving the SE0/SE3 alias refusal and the
    /// `/dev/ttyHS<N>` shape check. **This is not SE5 identity** and must never
    /// be reported as one; a caller that needs identity checks
    /// [`PortAuthority::is_proven`].
    NameFallback,
}

impl PortAuthority {
    /// True only when sysfs positively identified the port by MMIO. A
    /// [`PortAuthority::NameFallback`] port is opened, but its identity is
    /// unproven — production activation must run its own identity and policy
    /// checks before treating anything read from it as vehicle data.
    pub fn is_proven(self) -> bool {
        matches!(self, PortAuthority::Se5Mmio)
    }
}

/// Lexical normalisation: absolute only, `..` refused outright rather than
/// resolved — a fallback name that has to climb is a name we do not want to
/// reason about — and a canonical `PathBuf` to return and compare.
///
/// **What this function does NOT add:** `std::path::Components` already elides
/// interior `.` components, repeated separators and a trailing separator, and
/// `Path`'s `PartialEq` compares component-wise, so `/dev/./ttyHS1`,
/// `/dev//ttyHS1` and `/dev/ttyHS1/` are already equal to `/dev/ttyHS1`
/// without any help from us. Measured, not assumed: a mutant that made this
/// function the identity left the `.`-spelling refusal GREEN
/// (`kernel/out/.tmp/claude-can-core/logs/mutants-normalise.log`), which is why
/// there is no unit test claiming credit for that spelling. The two rules that
/// ARE ours — absolute-only and `..`-free — are covered by
/// [`tests::a_fallback_that_is_not_a_dev_ttyhs_node_is_refused_outright`],
/// which does go red on a mutant.
///
/// The `CurDir` arm below is unreachable for the absolute paths that get this
/// far (`Components` only yields `CurDir` as the first component of a relative
/// path, and those are rejected on the line above); it is present because the
/// match is exhaustive rather than catch-all, so a future `Component` variant
/// is a compile error and not a silent accept.
fn normalize_lexical(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => out.push(part),
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// `ttyHS` followed by at least one decimal digit and nothing else.
fn is_ttyhs_node_name(name: &str) -> bool {
    match name.strip_prefix("ttyHS") {
        Some(digits) => !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// The shape an accepted unproven fallback must have: a direct child of
/// `dev_dir` named `ttyHS<N>`.
///
/// This is the "explicitly constrain accepted fallback paths" half of the
/// refusal. `/dev/can-box`, `/dev/serial/by-id/…`, a relative name and anything
/// containing `..` are all refused, because a fallback is by definition a port
/// sysfs could not identify and this task does not enable unknown hardware
/// ports.
fn accepted_fallback(dev_dir: &Path, configured_fallback: &Path) -> Result<PathBuf, String> {
    let dev = normalize_lexical(dev_dir)
        .ok_or_else(|| format!("device directory {} is not an absolute path", dev_dir.display()))?;
    let candidate = normalize_lexical(configured_fallback).ok_or_else(|| {
        format!(
            "configured-name fallback={} is not an absolute `..`-free path; refusing",
            configured_fallback.display()
        )
    })?;
    let named_right = candidate
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_ttyhs_node_name);
    if candidate.parent() != Some(dev.as_path()) || !named_right {
        return Err(format!(
            "configured-name fallback={} is not a {}/ttyHS<N> node; refusing to enable an unknown port",
            configured_fallback.display(),
            dev.display()
        ));
    }
    Ok(candidate)
}

/// Do two paths name the same node? Lexically equal, or equal once symlinks are
/// resolved.
///
/// The second arm is what stops an alias defeating the refusal: a
/// `/dev/ttyHS9 -> /dev/ttyHS1` symlink is lexically unequal to the SE0 node
/// `resolve_port` proved, and without `canonicalize` it would be accepted as an
/// unproven fallback and then opened — the MCU's port. `canonicalize` needs
/// both paths to exist; when either does not, no alias can exist between them
/// on this boot either, so `false` is the right answer.
fn same_node(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

/// `resolve_port` builds its answer as `/dev/<class-entry-name>`; re-root it on
/// `dev_dir` so the unit tests can use a fixture directory. With the production
/// `dev_dir` (`/dev`) this is the identity.
fn rebase(dev_dir: &Path, resolved: &Path) -> PathBuf {
    resolved
        .file_name()
        .map_or_else(|| resolved.to_path_buf(), |name| dev_dir.join(name))
}

/// Choose the CAN tty: SE5 by MMIO, else the configured name unless sysfs
/// proves that name is the MCU's SE0 or Bluetooth's SE3.
///
/// Mirrors `binder.rs::select_mcu_path` for the second port; the refusal set
/// is larger because a CAN reader that opened the MCU port would eat ACC
/// frames, and one that opened SE3 would fight `hci_uart`.
///
/// Only [`PortAuthority::Se5Mmio`] is identity. The fallback arm trusts a name
/// sysfs could not identify, bounded by [`accepted_fallback`]'s shape rule and
/// by an alias-aware ([`same_node`]) refusal against SE0 and SE3.
pub fn select_can_path(
    sysfs_root: &Path,
    configured_fallback: &Path,
) -> Result<(PathBuf, PortAuthority), String> {
    select_can_path_under(Path::new(DEV_DIR), sysfs_root, configured_fallback)
}

/// [`select_can_path`] with the device directory injected, so the alias and
/// shape rules are testable without root and without touching the real `/dev`.
pub fn select_can_path_under(
    dev_dir: &Path,
    sysfs_root: &Path,
    configured_fallback: &Path,
) -> Result<(PathBuf, PortAuthority), String> {
    match resolve_port(sysfs_root, SE5_MMIO) {
        Ok(Some(path)) => Ok((rebase(dev_dir, &path), PortAuthority::Se5Mmio)),
        Err(error) => Err(format!("SE5 MMIO resolution refused: {error}")),
        Ok(None) => {
            let candidate = accepted_fallback(dev_dir, configured_fallback)?;
            for (mmio, what) in [
                (SE0_MMIO, "the MCU's SE0"),
                (SE3_MMIO_NOT_OURS, "Bluetooth's SE3"),
            ] {
                match resolve_port(sysfs_root, mmio) {
                    Ok(Some(path)) => {
                        let proven = rebase(dev_dir, &path);
                        if same_node(&proven, &candidate) {
                            return Err(format!(
                                "configured-name fallback={} resolves to {} = {what} MMIO=0x{mmio}; refusing",
                                configured_fallback.display(),
                                proven.display()
                            ));
                        }
                    }
                    Err(error) => {
                        return Err(format!("MMIO 0x{mmio} resolution refused: {error}"));
                    }
                    Ok(None) => {}
                }
            }
            Ok((candidate, PortAuthority::NameFallback))
        }
    }
}

/// Convenience for the desk binaries and tests: `Ok` path or an `io::Error`
/// carrying the refusal text.
pub fn select_can_path_io(
    sysfs_root: &Path,
    configured_fallback: &Path,
) -> io::Result<(PathBuf, PortAuthority)> {
    select_can_path(sysfs_root, configured_fallback)
        .map_err(|reason| io::Error::new(io::ErrorKind::PermissionDenied, reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    struct RemoveTree(PathBuf);
    impl Drop for RemoveTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(name: &str) -> (PathBuf, RemoveTree) {
        let root = std::env::temp_dir().join(format!(
            "bengal-hud-can-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("class/tty")).unwrap();
        fs::create_dir_all(root.join("dev")).unwrap();
        let guard = RemoveTree(root.clone());
        (root, guard)
    }

    /// The fixture stand-in for `/dev`.
    fn dev(root: &Path) -> PathBuf {
        root.join("dev")
    }

    /// A plain file standing in for a device node (a desk test cannot `mknod`).
    fn node(root: &Path, name: &str) -> PathBuf {
        let path = dev(root).join(name);
        fs::write(&path, b"").unwrap();
        path
    }

    fn link(root: &Path, tty: &str, mmio: &str) {
        let tty_dir = root.join("class/tty").join(tty);
        fs::create_dir_all(&tty_dir).unwrap();
        let target = root.join(format!("devices/platform/soc@0/{mmio}.serial"));
        fs::create_dir_all(&target).unwrap();
        symlink(&target, tty_dir.join("device")).unwrap();
    }

    #[test]
    fn se5_wins_by_mmio_whatever_the_configured_name_says() {
        let (root, _g) = fixture("se5");
        link(&root, "ttyHS1", SE0_MMIO);
        link(&root, "ttyHS2", SE3_MMIO_NOT_OURS);
        link(&root, "ttyHS7", SE5_MMIO);
        let (path, authority) =
            select_can_path(&root, Path::new(DEFAULT_DEVICE)).expect("resolve SE5");
        assert_eq!(path, PathBuf::from("/dev/ttyHS7"));
        assert_eq!(authority, PortAuthority::Se5Mmio);
    }

    #[test]
    fn unproven_name_is_used_and_labelled_as_fallback() {
        let (root, _g) = fixture("unproven");
        link(&root, "ttyHS1", SE0_MMIO);
        let (path, authority) =
            select_can_path(&root, Path::new(DEFAULT_DEVICE)).expect("fallback");
        assert_eq!(path, PathBuf::from(DEFAULT_DEVICE));
        assert_eq!(authority, PortAuthority::NameFallback);
    }

    #[test]
    fn a_name_sysfs_proves_is_bluetooth_se3_is_refused() {
        let (root, _g) = fixture("se3");
        link(&root, "ttyHS3", SE3_MMIO_NOT_OURS);
        let refusal = select_can_path(&root, Path::new(DEFAULT_DEVICE))
            .expect_err("SE3 must be refused");
        assert!(refusal.contains("Bluetooth's SE3"), "{refusal}");
    }

    #[test]
    fn a_name_sysfs_proves_is_the_mcu_se0_is_refused() {
        let (root, _g) = fixture("se0");
        link(&root, "ttyHS3", SE0_MMIO);
        let refusal = select_can_path(&root, Path::new(DEFAULT_DEVICE))
            .expect_err("SE0 must be refused");
        assert!(refusal.contains("MCU's SE0"), "{refusal}");
    }

    #[test]
    fn two_se5_claimants_are_refused_not_chosen_by_name() {
        let (root, _g) = fixture("dup");
        link(&root, "ttyHS3", SE5_MMIO);
        link(&root, "ttyHS4", SE5_MMIO);
        let refusal = select_can_path(&root, Path::new(DEFAULT_DEVICE))
            .expect_err("duplicate SE5 must be refused");
        assert!(refusal.contains("SE5 MMIO resolution refused"), "{refusal}");
    }

    // ---------------------------------------------- alias / shape refusals

    #[test]
    fn a_symlink_alias_onto_the_mcu_port_does_not_defeat_the_refusal() {
        // The hole this closes: the refusal used to be a literal `PathBuf`
        // comparison, so `/dev/ttyHS9 -> /dev/ttyHS1` was lexically unequal to
        // the SE0 node sysfs proved, fell through to the fallback arm and would
        // have been opened -- the MCU's port, read by the CAN reader.
        let (root, _g) = fixture("alias-se0");
        link(&root, "ttyHS1", SE0_MMIO);
        let real = node(&root, "ttyHS1");
        symlink(&real, dev(&root).join("ttyHS9")).unwrap();
        let refusal =
            select_can_path_under(&dev(&root), &root, &dev(&root).join("ttyHS9"))
                .expect_err("an alias of the MCU port must be refused");
        assert!(refusal.contains("MCU's SE0"), "{refusal}");
        assert!(refusal.contains("resolves to"), "{refusal}");
    }

    #[test]
    fn a_symlink_alias_onto_bluetooth_se3_does_not_defeat_the_refusal() {
        let (root, _g) = fixture("alias-se3");
        link(&root, "ttyHS2", SE3_MMIO_NOT_OURS);
        let real = node(&root, "ttyHS2");
        symlink(&real, dev(&root).join("ttyHS7")).unwrap();
        let refusal =
            select_can_path_under(&dev(&root), &root, &dev(&root).join("ttyHS7"))
                .expect_err("an alias of Bluetooth's port must be refused");
        assert!(refusal.contains("Bluetooth's SE3"), "{refusal}");
    }

    #[test]
    fn a_fallback_that_is_not_a_dev_ttyhs_node_is_refused_outright() {
        // No SE5, no SE0, no SE3 in sysfs: the ONLY thing standing between the
        // caller and an arbitrary open is the shape rule.
        let (root, _g) = fixture("shape");
        for bad in [
            "/dev/can-box",              // an alias name, not a tty node
            "/dev/serial/by-id/xinpu",   // not a direct child of /dev
            "/dev/ttyUSB0",              // a real port, but not ours to enable
            "/dev/ttyHS",                // no digit
            "/dev/ttyHS3x",              // trailing junk
            "/etc/passwd",               // not even a device
            "/dev/../etc/passwd",        // climbing
            "ttyHS3",                    // relative
        ] {
            let refusal = select_can_path(&root, Path::new(bad))
                .expect_err(&format!("{bad} must be refused, not opened"));
            assert!(
                refusal.contains("refusing"),
                "{bad}: refusal text unhelpful: {refusal}"
            );
        }
    }

    #[test]
    fn name_fallback_is_not_positive_identity() {
        let (root, _g) = fixture("authority");
        link(&root, "ttyHS1", SE0_MMIO);
        let (path, authority) =
            select_can_path(&root, Path::new(DEFAULT_DEVICE)).expect("fallback");
        assert_eq!(path, PathBuf::from(DEFAULT_DEVICE));
        assert!(!authority.is_proven(), "a name fallback must never claim identity");
        link(&root, "ttyHS9", SE5_MMIO);
        let (_, proven) = select_can_path(&root, Path::new(DEFAULT_DEVICE)).expect("SE5");
        assert!(proven.is_proven());
    }
}
