// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The Bengal device-3 project

//! The bezel button source: the daemon's own read of the Goodix touchscreen.
//!
//! WHY THIS EXISTS. The five left-bezel buttons do not reach any app. Measured
//! on D3-0114: fifteen presses arrived as touch contacts at negative X and
//! Android discarded every one — `InputReader out-of-frame drops: 15`. An
//! ordinary app cannot see them (they never reach it) and cannot read the raw
//! node (that needs privilege), so the DAEMON has to be the recorder and the
//! app is its UI. See `docs/research/2026-09-04-button-binding-backend-design.md`.
//!
//! WHAT THIS IS NOT. It does not name buttons, own a mapping, or inject keys.
//! It reports "a bezel contact happened at (x, y)". Naming is per-unit and is
//! learned; injection stays behind the existing gated uinput path.

pub mod decode;

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use decode::{BezelDecoder, BezelPress};

/// Size of one `struct input_event` on a 64-bit kernel: two 64-bit time fields
/// plus `u16`, `u16`, `i32`.
const INPUT_EVENT_BYTES: usize = 24;

/// Startup attempts for locating and opening the Goodix event node.
pub const OPEN_ATTEMPTS: u32 = 10;
/// Delay between startup attempts.
pub const OPEN_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Why locating or opening the touchscreen did not complete.
#[derive(Debug)]
pub enum OpenTouchError {
    /// The resolver saw multiple matching event devices, so choosing one would guess.
    Resolution(io::Error),
    /// A transient startup condition did not settle within the bounded retry window.
    Failed {
        /// Number of attempts made.
        attempts: u32,
        /// The final error observed.
        error: io::Error,
    },
}

/// Resolve and open the touchscreen, retrying only startup-race errors.
///
/// A missing node, its temporary DAC denial, and an interrupted open are all
/// expected while ueventd and the HAL class start concurrently. Resolution
/// ambiguity is stable input, so it returns immediately and never reaches the
/// opener.
pub fn open_touch_with<T, F, S>(
    input_root: &Path,
    needles: &[&str],
    attempts: u32,
    delay: Duration,
    sleeper: &mut S,
    mut open: F,
) -> Result<(T, u32), OpenTouchError>
where
    F: FnMut(&Path) -> io::Result<T>,
    S: crate::usb::Sleeper,
{
    let attempts = attempts.max(1);
    for attempt in 1..=attempts {
        let result = match resolve_touch_node(input_root, needles) {
            Ok(Some(node)) => open(&node),
            Ok(None) => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no touchscreen matched",
            )),
            Err(error) => return Err(OpenTouchError::Resolution(error)),
        };
        match result {
            Ok(value) => return Ok((value, attempt)),
            Err(error)
                if attempt < attempts
                    && matches!(
                        error.kind(),
                        io::ErrorKind::NotFound
                            | io::ErrorKind::PermissionDenied
                            | io::ErrorKind::Interrupted
                    ) =>
            {
                sleeper.sleep(delay);
            }
            Err(error) => {
                return Err(OpenTouchError::Failed {
                    attempts: attempt,
                    error,
                })
            }
        }
    }
    unreachable!("attempt count is clamped to one or more")
}

/// Resolve the touchscreen's event node by IDENTITY, never by number.
///
/// `event1` is today's number and nothing guarantees tomorrow's: the MCU tty
/// taught this expensively, where SE0 was `ttyHS0` on D3-0020 and `ttyHS1`
/// afterwards, and a name-based open would have put the daemon on Bluetooth's
/// UART. The same discipline applies here — match on the device NAME exposed by
/// the input core, and report which node was chosen.
pub fn resolve_touch_node(input_root: &Path, needles: &[&str]) -> io::Result<Option<PathBuf>> {
    let Ok(entries) = std::fs::read_dir(input_root) else {
        return Ok(None);
    };
    let mut found: Option<PathBuf> = None;
    for entry in entries.flatten() {
        // Only evdev handler nodes are openable as /dev/input/eventN. The
        // sibling `inputN` class entries describe the same device through its
        // PARENT: on the live unit input1's device/name is the i2c client
        // "gt911", so without this filter the panel matched twice (input1 via
        // the hedge needle, event1 via "goodix") and the ambiguity guard
        // refused it. Measured on D3-0118, 2026-09-05.
        let is_event_node = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with("event"));
        if !is_event_node {
            continue;
        }
        let name_path = entry.path().join("device/name");
        let Ok(name) = std::fs::read_to_string(&name_path) else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        if !needles.iter().any(|needle| name.contains(&needle.to_ascii_lowercase())) {
            continue;
        }
        let Some(node) = entry.file_name().to_str().map(|n| PathBuf::from("/dev/input").join(n))
        else {
            continue;
        };
        if let Some(previous) = found {
            // Two touchscreens is not a situation to guess at. Refuse rather
            // than pick, exactly as the MCU resolver now refuses an ambiguous
            // MMIO match instead of taking the lexicographically smaller name.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "multiple input devices match {needles:?}: {} and {}",
                    previous.display(),
                    node.display()
                ),
            ));
        }
        found = Some(node);
    }
    Ok(found)
}

/// Names that identify this board's touchscreen.
///
/// MEASURED on the live unit 2026-09-04, not assumed:
///
/// ```text
///   event0 = pm8941_pwrkey
///   event1 = Goodix Capacitive TouchScreen     <- this one
///   event2 = XYAUTO-SM4250-ADAU1701 Headset Jack
/// ```
///
/// Only `goodix` matches the EVENT node. `gt911` DID match on D3-0118 -- but
/// as the i2c parent's name behind the `input1` class entry, which is why the
/// resolver now looks at `event*` entries only. `gt928`/`gt911` are kept because the DT
/// compatible is `goodix,gt928` and a different kernel or panel revision may
/// name the input device after the part — but they are a HEDGE, and the test
/// below pins the string this board really reports so nobody mistakes the
/// hedge for the evidence.
pub const TOUCH_NEEDLES: &[&str] = &["goodix", "gt928", "gt911"];

/// A blocking reader over one evdev node.
pub struct BezelReader {
    file: File,
    decoder: BezelDecoder,
    node: PathBuf,
}

impl BezelReader {
    /// Open a resolved node read-only.
    ///
    /// Read-only is deliberate and worth stating: this reader observes the
    /// touchscreen, it never grabs it. An `EVIOCGRAB` here would take the
    /// panel away from Android.
    pub fn open(node: &Path) -> io::Result<Self> {
        Ok(Self {
            file: File::open(node)?,
            decoder: BezelDecoder::new(),
            node: node.to_path_buf(),
        })
    }

    /// The node this reader is bound to, for the connect log line.
    pub fn node(&self) -> &Path {
        &self.node
    }

    /// Block until the next completed bezel press.
    ///
    /// Returns `Ok(None)` on a clean end of stream. On-panel touches are
    /// consumed silently: they belong to Android.
    pub fn next_press(&mut self) -> io::Result<Option<BezelPress>> {
        let mut buffer = [0u8; INPUT_EVENT_BYTES];
        loop {
            match self.file.read_exact(&mut buffer) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(error) => return Err(error),
            }
            let event_type = u16::from_ne_bytes([buffer[16], buffer[17]]);
            let code = u16::from_ne_bytes([buffer[18], buffer[19]]);
            let value = i32::from_ne_bytes([buffer[20], buffer[21], buffer[22], buffer[23]]);
            if let Some(press) = self.decoder.ingest(event_type, code, value) {
                return Ok(Some(press));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    #[derive(Default)]
    struct RecordingSleeper {
        slept: Vec<Duration>,
    }

    impl crate::usb::Sleeper for RecordingSleeper {
        fn sleep(&mut self, duration: Duration) {
            self.slept.push(duration);
        }
    }

    #[test]
    fn resolved_node_retries_permission_race_then_opens() {
        let s = scratch("permission-race");
        fake_input_root(&s.0, &[("event4", "Goodix Capacitive TouchScreen")]);
        let mut sleeper = RecordingSleeper::default();
        let mut opens = 0;

        let (reader, attempts) = open_touch_with(
            &s.0,
            TOUCH_NEEDLES,
            4,
            Duration::from_secs(1),
            &mut sleeper,
            |node| {
                opens += 1;
                assert_eq!(node, Path::new("/dev/input/event4"));
                if opens < 3 {
                    Err(io::Error::from(io::ErrorKind::PermissionDenied))
                } else {
                    Ok("reader")
                }
            },
        )
        .unwrap();

        assert_eq!((reader, attempts, opens), ("reader", 3, 3));
        assert_eq!(sleeper.slept, vec![Duration::from_secs(1); 2]);
    }

    #[test]
    fn ambiguous_resolution_is_not_retried_or_opened() {
        let s = scratch("ambiguous-open");
        fake_input_root(&s.0, &[("event2", "Goodix-TS"), ("event7", "Goodix gt911")]);
        let mut sleeper = RecordingSleeper::default();
        let mut opens = 0;

        let error = open_touch_with(
            &s.0,
            TOUCH_NEEDLES,
            4,
            Duration::from_secs(1),
            &mut sleeper,
            |_| {
                opens += 1;
                Ok(())
            },
        )
        .unwrap_err();

        assert!(matches!(error, OpenTouchError::Resolution(_)));
        assert_eq!(opens, 0, "ambiguous input must never reach open");
        assert!(sleeper.slept.is_empty(), "ambiguity must fail immediately");
    }

    #[test]
    fn missing_node_retries_to_the_bound_without_calling_open() {
        let s = scratch("missing-node");
        let mut sleeper = RecordingSleeper::default();
        let mut opens = 0;

        let error = open_touch_with(
            &s.0,
            TOUCH_NEEDLES,
            3,
            Duration::from_secs(1),
            &mut sleeper,
            |_| {
                opens += 1;
                Ok(())
            },
        )
        .unwrap_err();

        match error {
            OpenTouchError::Failed { attempts, error } => {
                assert_eq!(attempts, 3);
                assert_eq!(error.kind(), io::ErrorKind::NotFound);
            }
            OpenTouchError::Resolution(error) => panic!("unexpected resolution error: {error}"),
        }
        assert_eq!(opens, 0);
        assert_eq!(sleeper.slept, vec![Duration::from_secs(1); 2]);
    }

    fn fake_input_root(root: &Path, entries: &[(&str, &str)]) {
        for (node, name) in entries {
            let dir = root.join(node).join("device");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("name"), format!("{name}\n")).unwrap();
        }
    }

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn scratch(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("bengal-bezel-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    #[test]
    fn the_touchscreen_is_found_by_name_not_by_number() {
        let s = scratch("byname");
        // Deliberately NOT event0: a resolver that assumes a number would pass
        // this test by accident, so the match must be on the name.
        fake_input_root(
            &s.0,
            &[
                ("event0", "pmic8xxx_pwrkey"),
                ("event3", "Goodix-TS gt928"),
                ("event5", "XYAUTO-SM4250-ADAU1701 Headset Jack"),
            ],
        );
        let node = resolve_touch_node(&s.0, TOUCH_NEEDLES).unwrap();
        assert_eq!(node, Some(PathBuf::from("/dev/input/event3")));
    }

    #[test]
    fn the_name_this_board_actually_reports_resolves() {
        // The exact strings read off the unit on 2026-09-04. If a future kernel
        // renames the device this test fails loudly, which is the point: the
        // resolver silently finding nothing is the failure mode that would
        // otherwise look like "the buttons stopped working".
        let s = scratch("real");
        fake_input_root(
            &s.0,
            &[
                ("event0", "pm8941_pwrkey"),
                ("event1", "Goodix Capacitive TouchScreen"),
                ("event2", "XYAUTO-SM4250-ADAU1701 Headset Jack"),
            ],
        );
        assert_eq!(
            resolve_touch_node(&s.0, TOUCH_NEEDLES).unwrap(),
            Some(PathBuf::from("/dev/input/event1")),
            "the measured device name must resolve"
        );
    }

    #[test]
    fn the_inputn_sibling_of_the_same_panel_is_not_a_second_device() {
        // Measured on D3-0118 (2026-09-05): /sys/class/input holds BOTH the
        // evdev handler `event1` (device/name = the input device's name) and
        // the class entry `input1`, whose device/ is the i2c PARENT and whose
        // name there is "gt911" -- one of the hedge needles. Walking both made
        // the resolver see one touchscreen twice, and the ambiguity guard then
        // refused it: `refusing to guess a node: ... /dev/input/input1 and
        // /dev/input/event1`. Only event* entries are openable anyway.
        let s = scratch("sibling");
        fake_input_root(
            &s.0,
            &[
                ("event0", "pm8941_pwrkey"),
                ("event1", "Goodix Capacitive TouchScreen"),
                ("event2", "XYAUTO-SM4250-ADAU1701 Headset Jack"),
                ("input0", ""),
                ("input1", "gt911"),
                ("input3", ""),
            ],
        );
        assert_eq!(
            resolve_touch_node(&s.0, TOUCH_NEEDLES).unwrap(),
            Some(PathBuf::from("/dev/input/event1")),
            "the inputN class entry must not count as a second touchscreen"
        );
    }

    #[test]
    fn no_touchscreen_is_not_an_error() {
        // A missing panel proves nothing about the reader; it must report
        // "none" so the caller can log it, not fail the daemon.
        let s = scratch("none");
        fake_input_root(&s.0, &[("event0", "pmic8xxx_pwrkey")]);
        assert_eq!(resolve_touch_node(&s.0, TOUCH_NEEDLES).unwrap(), None);
    }

    #[test]
    fn two_matching_devices_are_refused_rather_than_guessed() {
        let s = scratch("ambig");
        fake_input_root(&s.0, &[("event2", "goodix-ts"), ("event7", "Goodix gt911")]);
        let error = resolve_touch_node(&s.0, TOUCH_NEEDLES).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("multiple input devices match"));
    }

    #[test]
    fn an_unreadable_root_is_none_rather_than_a_panic() {
        assert_eq!(
            resolve_touch_node(Path::new("/definitely/not/here"), TOUCH_NEEDLES).unwrap(),
            None
        );
    }
}
