// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The Bengal device-3 project

//! Button observation, learning and binding — the logic half, with no Binder
//! and no I/O so it can be tested on the host.
//!
//! The daemon sees presses (`bezel::BezelReader` today, the MCU if it ever
//! answers), this decides what they mean, and `binder.rs` is a thin adapter.
//! Same split as `xyauto_dsp` and `fm::service`.
//!
//! WHAT THIS OWNS: the bounded ring of recent presses, learn mode, the binding
//! table and its validation, press → keycode matching, and — since the table is
//! useless if it dies with the process — reading and writing that table to
//! disk. It does NOT inject: injection stays behind the existing gated uinput
//! path, and this module only ever *answers* which key a press means.
//!
//! # Why the persistence lives here and not in `binder.rs`
//!
//! `binder.rs` is behind `#[cfg(feature = "binder")]` and cannot be compiled at
//! this desk at all — it needs `libbinder_rs` and a Soong-generated AIDL crate.
//! Anything put there is unreachable by `cargo test`, which is the only fast
//! gate this project has. Storage is exactly the kind of code that fails in
//! ways a review does not catch (a torn file, a label that eats its own
//! delimiter, a loader that trusts what a writer would have refused), so it
//! lives in this module, where a test can drive it against a temp directory in
//! milliseconds. `binder.rs` keeps the one job it cannot delegate: saying where
//! the file is, and logging what happened at startup.
//!
//! # Why the daemon is the system of record
//!
//! `IHud.aidl` says so — *"The daemon persists the table; the app is not the
//! system of record"* — and until this module existed that sentence was false.
//! It is also the right side of the argument, for a reason that is specific to
//! this unit: boot to a running app was **measured at 52 s**
//! (`mem:bengal-boot-time-first-measurement-2026-09-04`). If the app owned the
//! table, every one of the five bezel buttons would be dead for the first ~52 s
//! after the key turns — which is exactly the window in which a driver reaches
//! for HOME. The daemon has the table before Binder is even up.
//!
//! Note that `sepolicy/vendor/bengal_hud.te` used to carry the opposite
//! position ("no /data access. State belongs to the HU service app"). That
//! comment predates the V2 button interface, and its stated reason — a HAL
//! disagreeing with the app after a factory reset — does not hold: a factory
//! reset wipes `/data/vendor` along with the app's data, so there is no
//! surviving copy to disagree with. The policy and that comment are corrected
//! on this branch.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub(crate) mod actions;

/// Narrow system destination, independent of Android/Linux key numbering.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ButtonSystemAction {
    /// No system destination (including every legacy binding).
    #[default]
    None,
    /// Ask the private framework callback to show global actions.
    ShowGlobalActions,
}

/// Where a press was observed. Mirrors `vendor.xyauto.hud.ButtonSource`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ButtonSource {
    /// Source not (yet) determined.
    Unknown,
    /// A decoded MCU UART key frame.
    Mcu,
    /// A touchscreen contact off the left edge of the panel.
    Bezel,
}

/// One observed press, before naming or policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ButtonEvent {
    /// Monotonic per-daemon-lifetime counter; resets when the daemon restarts.
    pub sequence: i64,
    /// `CLOCK_BOOTTIME` milliseconds, so it stays sane across suspend.
    pub elapsed_realtime_ms: i64,
    /// Which transport saw it. Never shown to the owner.
    pub source: ButtonSource,
    /// MCU only; zero for a bezel press.
    pub raw_code: i32,
    /// Bezel only; zero for MCU. `x` is negative for a real bezel contact.
    pub x: i32,
    /// Bezel only; the coordinate that distinguishes one button from another.
    pub y: i32,
    /// How long the button was held, filled in on release only; zero on press.
    pub press_duration_ms: i32,
    /// The key this press resolved to, or zero when unbound.
    pub android_key_code: i32,
    /// Descriptive only; reading an event never dispatches it.
    pub system_action: ButtonSystemAction,
}

/// One learned mapping.
///
/// A bezel binding is a RANGE because the same button never reports the same
/// `y` twice — measured spread up to 16 px within a band, bands ~50 px apart.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ButtonBinding {
    /// Which transport this binding matches against.
    pub source: ButtonSource,
    /// MCU only: exact raw code to match. Zero for a bezel binding.
    pub raw_code: i32,
    /// Bezel only: inclusive lower bound of the contact's `y`.
    pub y_min: i32,
    /// Bezel only: inclusive upper bound of the contact's `y`.
    pub y_max: i32,
    /// Android key code to inject; zero means "learned but not assigned".
    pub android_key_code: i32,
    /// Owner-facing name. The daemon treats it as opaque and never parses it.
    pub label: String,
    /// Mutually exclusive with a nonzero Android key.
    pub system_action: ButtonSystemAction,
}

/// Why a proposed binding table was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BindingError {
    /// A binding may name only one destination.
    ConflictingDestinations,
    /// A system destination may be claimed only once.
    DuplicateSystemAction,
    /// `y_min > y_max`, which can never match anything.
    InvertedRange {
        /// The offending binding's owner-facing name.
        label: String,
    },
    /// Two bezel bindings claim overlapping contacts. Which one wins would
    /// depend on table order, and table order is not something an owner can
    /// see, let alone debug.
    OverlappingRanges {
        /// One of the two overlapping bindings.
        first: String,
        /// The other overlapping binding.
        second: String,
    },
    /// One Android key claimed by two buttons. The owner pressing either would
    /// get the same action, which is never what they meant to configure.
    DuplicateKeyCode {
        /// The Android key code claimed twice.
        code: i32,
    },
}

impl std::fmt::Display for BindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConflictingDestinations => write!(f, "key and system action are mutually exclusive"),
            Self::DuplicateSystemAction => write!(f, "system action is claimed by two bindings"),
            Self::InvertedRange { label } => {
                write!(f, "binding {label:?} has y_min greater than y_max")
            }
            Self::OverlappingRanges { first, second } => {
                write!(f, "bindings {first:?} and {second:?} claim overlapping y ranges")
            }
            Self::DuplicateKeyCode { code } => {
                write!(f, "android key code {code} is claimed by two bindings")
            }
        }
    }
}

/// Check a whole proposed table, as a set.
///
/// **This is the single copy of the rules.** `ButtonService::set_bindings` runs
/// it on what an app sends over Binder and `ButtonStore::read` runs it on what
/// comes back off the disk, so a file that has been hand-edited into a state
/// the interface would have rejected is rejected too. That is not paranoia
/// about the owner: it is that `/data/vendor/bengal-hud/button-bindings` is a
/// plain text file on a rooted development unit, and "it came from disk" is not
/// evidence of anything. A second, drifting copy of these rules in the loader
/// is the failure this function exists to make impossible — see
/// `the_file_path_and_the_binder_path_refuse_exactly_the_same_tables`.
///
/// Overlap and duplicate keys are properties of the *combination*, not of any
/// single entry, which is why the table is replaced whole rather than edited.
pub fn validate(bindings: &[ButtonBinding]) -> Result<(), BindingError> {
    for binding in bindings {
        if binding.android_key_code != 0 && binding.system_action != ButtonSystemAction::None {
            return Err(BindingError::ConflictingDestinations);
        }
        if binding.source == ButtonSource::Bezel && binding.y_min > binding.y_max {
            return Err(BindingError::InvertedRange {
                label: binding.label.clone(),
            });
        }
    }
    for (i, a) in bindings.iter().enumerate() {
        for b in bindings.iter().skip(i + 1) {
            if a.system_action != ButtonSystemAction::None && a.system_action == b.system_action {
                return Err(BindingError::DuplicateSystemAction);
            }
            if a.source == ButtonSource::Bezel
                && b.source == ButtonSource::Bezel
                && a.y_min <= b.y_max
                && b.y_min <= a.y_max
            {
                return Err(BindingError::OverlappingRanges {
                    first: a.label.clone(),
                    second: b.label.clone(),
                });
            }
            // Zero means "learned but not assigned"; many buttons may sit
            // in that state at once during setup, so it is not a duplicate.
            if a.android_key_code != 0 && a.android_key_code == b.android_key_code {
                return Err(BindingError::DuplicateKeyCode {
                    code: a.android_key_code,
                });
            }
        }
    }
    Ok(())
}

// ---- persistence ----------------------------------------------------------
//
// Format, storage location and the crash story. Everything below is plain std:
// this crate has zero Cargo dependencies on purpose (see Cargo.toml), so there
// is no serde, and there will not be one.

/// Where the table lives on the unit.
///
/// `/data/vendor` and not `/data/misc` or a `/vendor` file, for four reasons
/// that were checked rather than assumed:
///
/// 1. **`/vendor` is read-only.** The whole partition is mounted `ro` and is
///    replaced wholesale by a flash; a table written there would not survive
///    the next package, which is the opposite of the requirement.
/// 2. **It exists early enough.** `init.rc:729` creates `/data/vendor` in
///    `on post-fs-data`, and `class_start hal` — which starts this daemon — is
///    in `on boot` (`init.rc:1284`). The directory is there before the process
///    is.
/// 3. **It is device-encrypted, not credential-encrypted.** `init.rc:729`
///    creates it `encryption=Require` under the device key, so it is readable
///    at boot without any user having unlocked anything. A bezel button that
///    only works after somebody unlocks the screen is not a bezel button, and
///    this unit deliberately has no lock screen at all.
/// 4. **The tree already does this.** `vendor_bengal3_usb_trace_data_file`
///    (`sepolicy/vendor/usb_trace.te`) and the tqftpserv one are the same
///    shape: a dedicated type, one `file_contexts` line, an init `mkdir`.
///
/// SELINUX: this path needs `vendor_bengal_hud_data_file` plus the matching
/// `file_contexts` line and an init `mkdir`, all of which land on this branch.
/// Without them the daemon is `vendor_bengal_hud` writing into an unlabelled
/// `/data/vendor` child, which works permissive and fails at enforcing — the
/// wired-but-unpolicied state `mem:bengal-inert-landing-state-2026-09-04`
/// forbids. If the policy half is dropped, drop the `binder.rs` wiring too and
/// land this module inert.
pub const DEFAULT_BINDINGS_PATH: &str = "/data/vendor/bengal-hud/button-bindings";

/// First line of the file. Version is part of the text, not the filename.
const FORMAT_HEADER: &str = "bengal-hud button-bindings 1";
const ACTION_FORMAT_HEADER: &str = "bengal-hud button-bindings 2";

/// Last line's tag; `end<TAB><count>` closes a complete file.
const TERMINATOR: &str = "end";

/// Refuse to read — or write — anything larger than this.
///
/// The unit has five physical buttons. A real table is under 200 bytes, so
/// 64 KiB is four hundred times any legitimate file while still bounding what a
/// corrupt length field can make the daemon allocate at boot. **The writer
/// checks the same limit**, so anything this daemon successfully saves is
/// something it can read back; a cap enforced on only one side would make the
/// daemon reject its own file after a reboot, which is a nastier bug than the
/// one the cap prevents.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// Suffix of the write-and-rename staging file. Same directory, always.
const TEMP_SUFFIX: &str = ".new";

/// Why a stored table could not be turned back into bindings.
///
/// Granular on purpose: this text ends up in `kmsg` in front of somebody who
/// has to decide whether the file was hand-edited, half-written or written by a
/// newer build, and "parse error" answers none of those.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FormatError {
    /// The bytes are not UTF-8, so no line structure can be trusted at all.
    NotUtf8,
    /// The first line is missing or is not this format's header. A file written
    /// by a future version lands here, and refusal is the right answer: this
    /// build cannot know what a field it has never seen means.
    BadHeader {
        /// The first line as found, for the log.
        found: String,
    },
    /// No `end <count>` line. This is the shape of a file cut short — the
    /// classic power-cut-mid-write signature the terminator exists to catch.
    MissingTerminator,
    /// The terminator's count disagrees with the number of binding lines.
    CountMismatch {
        /// What the terminator claimed.
        declared: usize,
        /// How many binding lines were actually present.
        found: usize,
    },
    /// A binding line does not have the six expected tab-separated fields.
    BadFieldCount {
        /// 1-based line number in the file.
        line: usize,
        /// How many fields the line actually had.
        found: usize,
    },
    /// A numeric column is not a 32-bit integer.
    BadNumber {
        /// 1-based line number in the file.
        line: usize,
        /// Which column.
        field: &'static str,
        /// The text that failed to parse.
        found: String,
    },
    /// The source column names a transport this build does not know.
    BadSource {
        /// 1-based line number in the file.
        line: usize,
        /// The text found in the column.
        found: String,
    },
    /// A label contains a backslash escape this format does not define.
    BadEscape {
        /// 1-based line number in the file.
        line: usize,
    },
    /// Something follows the terminator. A file may end once.
    TrailingContent {
        /// 1-based line number of the first line after the terminator.
        line: usize,
    },
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotUtf8 => write!(f, "not UTF-8"),
            Self::BadHeader { found } => {
                write!(f, "first line is {found:?}, expected {FORMAT_HEADER:?}")
            }
            Self::MissingTerminator => write!(
                f,
                "no {TERMINATOR:?} line: the file is truncated or was never finished"
            ),
            Self::CountMismatch { declared, found } => write!(
                f,
                "terminator declares {declared} bindings but {found} lines are present"
            ),
            Self::BadFieldCount { line, found } => {
                write!(f, "line {line} has {found} fields, expected 6")
            }
            Self::BadNumber { line, field, found } => {
                write!(f, "line {line} field {field} is not an integer: {found:?}")
            }
            Self::BadSource { line, found } => {
                write!(f, "line {line} has unknown source {found:?}")
            }
            Self::BadEscape { line } => {
                write!(f, "line {line} has an undefined backslash escape in its label")
            }
            Self::TrailingContent { line } => {
                write!(f, "line {line} follows the terminator")
            }
        }
    }
}

/// Why a stored table was not put into force.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoadError {
    /// The file exists but could not be read: permission, I/O, a denial.
    ///
    /// Deliberately distinct from the corruption cases. "SELinux refused me" and
    /// "somebody edited this into nonsense" call for opposite responses, and a
    /// single `Err` would have made them indistinguishable in the log.
    Unreadable {
        /// The underlying error, rendered.
        detail: String,
    },
    /// The bytes are not this format.
    Malformed {
        /// Where the parse gave up.
        error: FormatError,
    },
    /// The file parsed, but describes a table the daemon would have refused
    /// over Binder — overlapping bands, or one Android key claimed twice.
    Invalid {
        /// Which rule it broke.
        error: BindingError,
    },
    /// The file is larger than any legitimate table.
    TooLarge {
        /// The byte limit that was exceeded.
        limit: u64,
    },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable { detail } => write!(f, "unreadable: {detail}"),
            Self::Malformed { error } => write!(f, "malformed: {error}"),
            Self::Invalid { error } => write!(f, "invalid table: {error}"),
            Self::TooLarge { limit } => write!(f, "larger than the {limit}-byte limit"),
        }
    }
}

/// What startup found on disk.
///
/// Returned rather than logged in here because this module has no log sink —
/// `logsink.rs` only formats, and the one `write()` to `/dev/kmsg` is
/// `binder.rs`'s. The caller must say all three of these out loud: a silent
/// `Refused` is a unit whose buttons stopped working for no visible reason.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoadOutcome {
    /// No file. A unit that has never been taught, which is not an error.
    Absent,
    /// Loaded, validated and in force.
    Loaded {
        /// How many bindings came back.
        count: usize,
    },
    /// A file was there and was refused. **The daemon starts with NO
    /// bindings.**
    ///
    /// The alternative — keeping whatever subset parsed — is worse. Dead
    /// buttons are obvious and the owner can re-teach them in a minute; buttons
    /// that do the *wrong* thing are a fault the owner cannot explain and
    /// cannot fix, and on a head unit "the button I press while driving does
    /// something I did not ask for" is the failure that actually matters.
    ///
    /// The refused file is deliberately left on disk. Overwriting it with an
    /// empty table would destroy the only evidence of what went wrong, and the
    /// next `setButtonBindings` replaces it anyway.
    Refused {
        /// Why it was refused.
        error: LoadError,
    },
}

/// Why a `set_bindings` call did not take effect.
///
/// Both variants mean the same thing about state: **nothing changed**. The
/// previous table is still the one in force and still the one on disk. That
/// single guarantee is what lets an app treat a refusal as authoritative and
/// simply re-read, which is what the wizard does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetBindingsError {
    /// The table itself is not acceptable — overlapping bands, one key claimed
    /// twice, an inverted range.
    Invalid(BindingError),
    /// The table is valid but could not be written: no directory, no space, a
    /// read-only `/data`, a denied rename.
    ///
    /// This variant exists because the alternative was worse. The first version
    /// of this module applied such a table in memory anyway and only logged the
    /// failure, on the reasoning that working buttons now beat a refusal for a
    /// reason the owner did not cause. That is wrong for a reason the wizard
    /// author pointed out: the owner is shown a binding that *saved*, and it
    /// silently will not be there after the next start. A refusal the app can
    /// see and report beats a success that is not true.
    NotPersisted {
        /// The underlying error, rendered, with the path.
        detail: String,
    },
}

impl std::fmt::Display for SetBindingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(error) => write!(f, "{error}"),
            Self::NotPersisted { detail } => {
                write!(f, "binding table could not be stored: {detail}")
            }
        }
    }
}

/// The daemon's copy of the binding table on disk.
///
/// # The format
///
/// ```text
/// bengal-hud button-bindings 1
/// bezel<TAB>0<TAB>84<TAB>134<TAB>0<TAB>Power
/// bezel<TAB>0<TAB>177<TAB>202<TAB>3<TAB>Home
/// end<TAB>2
/// ```
///
/// Hand-rolled and line-oriented, because this crate has no dependencies and
/// because a person with `adb shell` and no build tree should be able to read
/// and fix it. Tab-separated so labels can contain spaces, which owners' labels
/// do; `\\`, `\t`, `\n` and `\r` in a label are backslash-escaped, so no label
/// can ever forge a field or a line boundary. An undefined escape is an error
/// rather than a passthrough — a passthrough would make `\\t` and `\t`
/// round-trip to the same bytes and silently corrupt a label containing a
/// backslash.
///
/// # The terminator
///
/// `end<TAB><count>` is what makes a short file *detectably* short. Write-and-
/// rename already means a reader never sees a partial file, but the terminator
/// costs one line and turns "the filesystem betrayed us" from an unfalsifiable
/// worry into a case with a test
/// (`every_truncation_of_a_good_file_is_refused`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ButtonStore {
    path: PathBuf,
}

impl ButtonStore {
    /// A store over an explicit path. Tests pass a temp directory; the daemon
    /// passes [`DEFAULT_BINDINGS_PATH`].
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// A store over [`DEFAULT_BINDINGS_PATH`].
    pub fn default_path() -> Self {
        Self::new(DEFAULT_BINDINGS_PATH)
    }

    /// The file this store reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The staging file `save` renames from.
    ///
    /// Public so a test can assert it is a sibling of the target. That is not a
    /// detail: `rename(2)` is only atomic within one filesystem, so a staging
    /// file in `/tmp` or `/data/local/tmp` would turn the atomic swap this
    /// whole design rests on into a copy that can be interrupted.
    pub fn temp_path(&self) -> PathBuf {
        let mut name = self.path.as_os_str().to_os_string();
        name.push(TEMP_SUFFIX);
        PathBuf::from(name)
    }

    /// Read and validate the stored table.
    ///
    /// `Ok(None)` means there is no file — a unit that has never been taught.
    /// Every other failure is an `Err`: a missing file and an unreadable one
    /// are not the same event and must not log the same way.
    pub fn read(&self) -> Result<Option<Vec<ButtonBinding>>, LoadError> {
        let file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(LoadError::Unreadable {
                    detail: error.to_string(),
                })
            }
        };
        // take(MAX + 1) so an oversized file is detected without reading it
        // all: this runs at boot, and a corrupt length must not be able to make
        // the daemon allocate whatever the file claims.
        let mut bytes = Vec::new();
        if let Err(error) = file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes) {
            return Err(LoadError::Unreadable {
                detail: error.to_string(),
            });
        }
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(LoadError::TooLarge {
                limit: MAX_FILE_BYTES,
            });
        }
        let text = String::from_utf8(bytes).map_err(|_| LoadError::Malformed {
            error: FormatError::NotUtf8,
        })?;
        let bindings = parse_table(&text).map_err(|error| LoadError::Malformed { error })?;
        // The same rules the Binder path applies. A file is not evidence.
        validate(&bindings).map_err(|error| LoadError::Invalid { error })?;
        Ok(Some(bindings))
    }

    /// Write the table so that a power cut leaves either the old one or the new
    /// one, never a half.
    ///
    /// The sequence is the standard durable-replace recipe, and every step is
    /// load-bearing on a device that loses power as a matter of routine — the
    /// owner turns the key off mid-save and that is not an exceptional event:
    ///
    /// 1. serialise fully **in memory** first, so a formatting failure cannot
    ///    happen with a file already open for truncation;
    /// 2. write it all to a sibling staging file;
    /// 3. `fsync` that file — without it the rename can land while the blocks
    ///    are still in cache, and the reboot finds a correctly-named file full
    ///    of zeros, which is the classic ext4 delayed-allocation loss;
    /// 4. `rename(2)` over the target, which is atomic within a filesystem: any
    ///    reader sees the whole old file or the whole new one;
    /// 5. `fsync` the **directory**, because otherwise the rename itself is not
    ///    durable and the old table can come back after a cut.
    ///
    /// The target file is never opened for writing. That is the property
    /// `a_save_never_touches_the_file_already_in_place` pins, by holding an
    /// open handle to the old file across a save.
    pub fn save(&self, bindings: &[ButtonBinding]) -> io::Result<()> {
        let text = serialise_table(bindings);
        if text.len() as u64 > MAX_FILE_BYTES {
            // Refuse to write what `read` would refuse to load. A daemon that
            // saves a file it cannot read back is worse than one that says no.
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "table serialises to {} bytes, over this daemon's {MAX_FILE_BYTES}-byte read limit",
                    text.len()
                ),
            ));
        }
        let directory = self.path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "binding path has no parent directory",
            )
        })?;
        let temp = self.temp_path();
        match Self::write_staging(&temp, text.as_bytes()) {
            Ok(()) => {}
            Err(error) => {
                // Never leave a half-written staging file behind. It is not
                // dangerous — nothing ever reads it — but it is confusing to
                // find, and this daemon should not litter /data.
                let _ = fs::remove_file(&temp);
                return Err(error);
            }
        }
        fs::rename(&temp, &self.path)?;
        // The rename's own durability. A failure here means the bytes are
        // written but their permanence is not guaranteed; that is strictly
        // better than the state before the call, so it is not escalated.
        if let Ok(handle) = fs::File::open(directory) {
            let _ = handle.sync_all();
        }
        Ok(())
    }

    fn write_staging(temp: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut file = fs::File::create(temp)?;
        file.write_all(bytes)?;
        file.sync_all()
    }
}

/// Render a table in the v1 format, terminator included.
fn serialise_table(bindings: &[ButtonBinding]) -> String {
    let mut out = String::with_capacity(64 + bindings.len() * 48);
    let typed = bindings.iter().any(|b| b.system_action != ButtonSystemAction::None);
    out.push_str(if typed { ACTION_FORMAT_HEADER } else { FORMAT_HEADER });
    out.push('\n');
    for binding in bindings {
        out.push_str(source_tag(binding.source));
        out.push('\t');
        out.push_str(&binding.raw_code.to_string());
        out.push('\t');
        out.push_str(&binding.y_min.to_string());
        out.push('\t');
        out.push_str(&binding.y_max.to_string());
        out.push('\t');
        out.push_str(&binding.android_key_code.to_string());
        out.push('\t');
        out.push_str(&escape_label(&binding.label));
        if typed {
            out.push('\t');
            out.push_str(match binding.system_action {
                ButtonSystemAction::None => "0",
                ButtonSystemAction::ShowGlobalActions => "1",
            });
        }
        out.push('\n');
    }
    out.push_str(TERMINATOR);
    out.push('\t');
    out.push_str(&bindings.len().to_string());
    out.push('\n');
    out
}

/// Parse the v1 format. Structure only — the table's *rules* are `validate`.
fn parse_table(text: &str) -> Result<Vec<ButtonBinding>, FormatError> {
    // A trailing newline is part of the format, not an empty last line.
    let body = text.strip_suffix('\n').unwrap_or(text);
    let lines: Vec<&str> = body.split('\n').collect();

    // `split` always yields at least one element, so this indexes safely even
    // for an empty file — which lands in `BadHeader` with an empty `found`,
    // exactly as a zero-byte file should.
    let typed = lines[0] == ACTION_FORMAT_HEADER;
    if lines[0] != FORMAT_HEADER && !typed {
        return Err(FormatError::BadHeader {
            found: lines[0].to_string(),
        });
    }

    let mut bindings = Vec::new();
    let mut declared = None;
    for (index, &line) in lines.iter().enumerate().skip(1) {
        let number = index + 1;
        if let Some(count) = line.strip_prefix(TERMINATOR) {
            let count = count.strip_prefix('\t').ok_or(FormatError::BadFieldCount {
                line: number,
                found: 1,
            })?;
            declared = Some(count.parse::<usize>().map_err(|_| FormatError::BadNumber {
                line: number,
                field: "count",
                found: count.to_string(),
            })?);
            // Nothing may follow the end of the file. A second table appended
            // after the terminator is not a merge, it is a corrupt file.
            if index + 1 < lines.len() {
                return Err(FormatError::TrailingContent { line: number + 1 });
            }
            break;
        }
        bindings.push(parse_binding(line, number, typed)?);
    }

    let declared = declared.ok_or(FormatError::MissingTerminator)?;
    if declared != bindings.len() {
        return Err(FormatError::CountMismatch {
            declared,
            found: bindings.len(),
        });
    }
    Ok(bindings)
}

fn parse_binding(line: &str, number: usize, typed: bool) -> Result<ButtonBinding, FormatError> {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() != if typed { 7 } else { 6 } {
        return Err(FormatError::BadFieldCount {
            line: number,
            found: fields.len(),
        });
    }
    let number_field = |text: &str, field: &'static str| {
        text.parse::<i32>().map_err(|_| FormatError::BadNumber {
            line: number,
            field,
            found: text.to_string(),
        })
    };
    Ok(ButtonBinding {
        source: parse_source(fields[0]).ok_or_else(|| FormatError::BadSource {
            line: number,
            found: fields[0].to_string(),
        })?,
        raw_code: number_field(fields[1], "raw_code")?,
        y_min: number_field(fields[2], "y_min")?,
        y_max: number_field(fields[3], "y_max")?,
        android_key_code: number_field(fields[4], "android_key_code")?,
        label: unescape_label(fields[5], number)?,
        system_action: if typed {
            match fields[6] {
                "0" => ButtonSystemAction::None,
                "1" => ButtonSystemAction::ShowGlobalActions,
                other => return Err(FormatError::BadNumber {
                    line: number,
                    field: "system_action (expected 0 or 1)",
                    found: other.to_string(),
                }),
            }
        } else { ButtonSystemAction::None },
    })
}

fn source_tag(source: ButtonSource) -> &'static str {
    match source {
        ButtonSource::Bezel => "bezel",
        ButtonSource::Mcu => "mcu",
        ButtonSource::Unknown => "unknown",
    }
}

fn parse_source(tag: &str) -> Option<ButtonSource> {
    match tag {
        "bezel" => Some(ButtonSource::Bezel),
        "mcu" => Some(ButtonSource::Mcu),
        "unknown" => Some(ButtonSource::Unknown),
        _ => None,
    }
}

fn escape_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    for character in label.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

fn unescape_label(field: &str, number: usize) -> Result<String, FormatError> {
    let mut out = String::with_capacity(field.len());
    let mut characters = field.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            _ => return Err(FormatError::BadEscape { line: number }),
        }
    }
    Ok(out)
}

/// How long learn mode may stay on without being refreshed.
///
/// Learn mode suppresses injection, so a setup app that crashes mid-wizard
/// would otherwise leave the unit's buttons dead until the next reboot with no
/// indication why. Bounded so the failure is temporary and self-healing.
pub const LEARN_MODE_TIMEOUT_MS: i64 = 5 * 60 * 1000;

/// How many presses to keep for `getRecentButtonEvents`.
const RECENT_RING: usize = 64;

/// How many observed presses may wait to be pushed to a subscriber.
///
/// Same size as the ring it shadows, and for the same reason: the queue is
/// drained on every pass of the reader loop, so reaching this bound means
/// delivery has stopped rather than fallen behind. Dropping the oldest is the
/// right loss — `IHud.getRecentButtonEvents` exists precisely so an app that
/// missed a push can still see what happened.
const PENDING_DELIVERY: usize = 64;

/// Presses waiting to be handed to `IHudCallback.onButtonEvent`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PendingDelivery {
    /// The events to deliver, oldest first.
    pub events: Vec<ButtonEvent>,
    /// How many presses were dropped since the previous drain because the
    /// queue was full. Non-zero means a subscriber missed presses and should
    /// be told, once, rather than per press.
    pub dropped: u32,
}


/// The keys this daemon is willing to synthesise.
///
/// Deliberately only four. `mcu::uinput::UinputDevice` registers exactly these
/// with `UI_SET_KEYBIT`, and a key it never registered cannot be emitted, so
/// this enum and that ioctl list have to agree — the test below pins them.
///
/// POWER IS ABSENT ON PURPOSE. It is one of the five bezel buttons and it is
/// learnable, but binding it would let the owner suspend the unit from inside
/// the setup wizard. It stays out of the emit path entirely rather than being
/// gated behind a confirmation the daemon cannot enforce.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundKey {
    /// Android HOME.
    Home,
    /// Android BACK.
    Back,
    /// Media volume up.
    VolumeUp,
    /// Media volume down.
    VolumeDown,
}

impl BoundKey {
    /// Translate an **Android** `KeyEvent` code into a key this daemon emits.
    ///
    /// The two numbering schemes are not the same and the difference is silent:
    /// Android HOME is 3 while Linux `KEY_HOMEPAGE` is 172, Android BACK is 4
    /// while Linux `KEY_BACK` is 158. The AIDL carries the Android number
    /// because that is what an app author knows; uinput needs the Linux one.
    /// Getting this backwards binds the wrong button and looks like a hardware
    /// fault, so it is a named function with a table test rather than an
    /// inline match.
    pub fn from_android_code(code: i32) -> Option<Self> {
        match code {
            3 => Some(Self::Home),
            4 => Some(Self::Back),
            24 => Some(Self::VolumeUp),
            25 => Some(Self::VolumeDown),
            _ => None,
        }
    }
}

/// What the daemon should do about a press that has already been observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmitDecision {
    /// Synthesise this key. The ONLY variant that may open `/dev/uinput`.
    Emit(BoundKey),
    /// Nothing is bound to this press.
    Unbound,
    /// The daemon is not permitted to synthesise keys at all.
    InjectionDisabled,
    /// Bound, but to a key this daemon will never emit — POWER, or a code it
    /// does not recognise.
    NotEmittable {
        /// The Android key code the binding asked for.
        android_key_code: i32,
    },
}

/// Decide what to do with the key `observe_bezel` resolved.
///
/// # Why this is a function and not three lines in the reader thread
///
/// It was three lines in the reader thread, and they were wrong: the bezel path
/// opened `/dev/uinput` on the first bound press **regardless of
/// `ro.vendor.bengal.hud.inject_keys`**, a property the MCU path has always
/// honoured. It shipped as **0** when this was written, so a learned bezel
/// binding would have injected keys the operator had explicitly switched off,
/// and the daemon would have created a virtual keyboard to do it. One gate for
/// "may this daemon synthesise keys at all", and it lives here where a test can
/// reach it.
///
/// The property now ships as **1** (`device.mk:1193`), which is what makes the
/// gate matter in the other direction too: it is the only thing standing
/// between a build that has decided not to synthesise keys and a daemon that
/// does it anyway. Do not treat either value as the permanent one — read
/// device.mk. The tests below cover both.
///
/// # What this deliberately does NOT gate on
///
/// `mcu::uinput::DEVICE3_MAPPING_CONFIRMED`. The MCU path refuses when that is
/// false because the MCU's raw-code table is guessed and an unconfirmed guess
/// would emit the wrong key. A bezel binding is not a guess: the owner taught
/// it by pressing the button, and the y range came off this unit's own glass.
/// Applying the MCU's confirmation flag here would also be permanent — the
/// constant is pinned false by a `const _: () = assert!(!…)` next to it — so it
/// would disable learned bezel keys forever, for a reason that is about a
/// different transport.
pub fn emit_decision(injection_enabled: bool, resolved: Option<i32>) -> EmitDecision {
    let Some(code) = resolved else {
        return EmitDecision::Unbound;
    };
    // Checked BEFORE the code lookup so that "switched off" is reported as
    // switched off, and never as an unrecognised key.
    if !injection_enabled {
        return EmitDecision::InjectionDisabled;
    }
    match BoundKey::from_android_code(code) {
        Some(key) => EmitDecision::Emit(key),
        None => EmitDecision::NotEmittable {
            android_key_code: code,
        },
    }
}

/// Everything the daemon decides about one bezel contact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PressAction {
    /// Presses to push to `IHudCallback.onButtonEvent`, and how many were lost.
    pub delivery: PendingDelivery,
    /// Whether to synthesise a key, and which.
    pub emit: EmitDecision,
    /// Physical-only destination; never reconstructed from diagnostic events.
    pub system_action: ButtonSystemAction,
}

/// Decide everything about one press, in one place, under one lock.
///
/// # Why this is not three lines in the reader thread
///
/// It was, and being there made it untestable: `spawn_bezel_reader` is behind
/// `#[cfg(feature = "binder")]` and its body opens a real `/dev/input` node, so
/// nothing between "a finger landed" and "a key was emitted" could be exercised
/// at the desk. The glue is exactly where the ordering mistakes live, so it
/// belongs here.
///
/// # Why it returns a decision instead of performing one
///
/// The caller holds this service's mutex to call it and must NOT hold that
/// mutex while it transacts on Binder or writes to `/dev/uinput`. Returning a
/// plain value makes that structural: there is nothing here the caller could
/// accidentally do while still locked. Same rule the MCU path follows with
/// `flush_callback_mailbox`.
///
/// # Ordering, which is the part worth pinning
///
/// Observe, then drain, then decide. The drain must come after the observation
/// or the press that just arrived would be delivered one contact late; the
/// emit decision must come last because it is the only step the injection gate
/// can veto, and vetoing it must not suppress the other two.
pub fn handle_press(
    service: &mut ButtonService,
    x: i32,
    y: i32,
    now_ms: i64,
    duration_ms: i32,
    injection_enabled: bool,
) -> PressAction {
    let resolved = service.observe_bezel(x, y, now_ms, duration_ms);
    let system_action = if injection_enabled && x < 0 && !service.learn_mode(now_ms) {
        service.bindings.iter()
            .find(|b| b.source == ButtonSource::Bezel && b.y_min <= y && y <= b.y_max)
            .map(|b| b.system_action)
            .unwrap_or_default()
    } else { ButtonSystemAction::None };
    PressAction {
        delivery: service.drain_pending(),
        emit: emit_decision(injection_enabled, resolved),
        system_action,
    }
}

/// The daemon's button state.
#[derive(Debug)]
pub struct ButtonService {
    bindings: Vec<ButtonBinding>,
    recent: VecDeque<ButtonEvent>,
    next_sequence: i64,
    /// `Some(started_at_ms)` while learning.
    learn_since_ms: Option<i64>,
    /// Where accepted tables are written. `None` for host tests and for any
    /// caller that has no business touching the filesystem — `new()` must stay
    /// I/O-free so the 15 existing behaviour tests cannot accidentally write to
    /// a real path.
    store: Option<ButtonStore>,
    /// Presses observed but not yet handed to a subscriber. Separate from
    /// `recent` because the two answer different questions: `recent` is "what
    /// happened lately", this is "what has not been pushed yet", and an event
    /// leaves this queue exactly once.
    pending: VecDeque<ButtonEvent>,
    /// Presses lost from `pending` since the last drain.
    pending_dropped: u32,
}

impl Default for ButtonService {
    fn default() -> Self {
        Self::new()
    }
}

impl ButtonService {
    /// A service with no bindings, an empty ring, learn mode off and **no
    /// store**. Nothing this returns ever touches the filesystem.
    pub fn new() -> Self {
        Self {
            bindings: Vec::new(),
            recent: VecDeque::with_capacity(RECENT_RING),
            next_sequence: 1,
            learn_since_ms: None,
            store: None,
            pending: VecDeque::with_capacity(PENDING_DELIVERY),
            pending_dropped: 0,
        }
    }

    /// The daemon's startup path: build a service over `store` and put whatever
    /// it holds into force.
    ///
    /// This is what makes `IHud.aidl`'s *"the daemon persists the table"* true.
    /// Before it existed the table lived in a `Vec` and died with the process,
    /// so an owner who taught their five buttons and then turned the key off
    /// found a unit that had forgotten — while the interface documentation told
    /// an app author the opposite.
    ///
    /// The [`LoadOutcome`] is returned rather than discarded because the caller
    /// is the only thing here with a log sink, and all three outcomes are worth
    /// a line: `Absent` explains why a new unit has no bindings, `Loaded`
    /// records how many came back, and `Refused` is the only trace of a table
    /// that existed a moment ago and does not now.
    pub fn with_store(store: ButtonStore) -> (Self, LoadOutcome) {
        let mut service = Self::new();
        let outcome = match store.read() {
            Ok(None) => LoadOutcome::Absent,
            Ok(Some(bindings)) => {
                let count = bindings.len();
                service.bindings = bindings;
                LoadOutcome::Loaded { count }
            }
            // Deliberately no bindings, and deliberately no attempt to repair
            // or delete the file: see LoadOutcome::Refused.
            Err(error) => LoadOutcome::Refused { error },
        };
        service.store = Some(store);
        (service, outcome)
    }

    /// Replace the whole table, or refuse it entirely; persist it if accepted.
    ///
    /// Whole-table so the set can be validated as a set: overlap and duplicate
    /// keys are properties of the combination, not of any single entry, and a
    /// per-entry API would let the table pass through states that are each
    /// legal and jointly wrong.
    ///
    /// `Ok` means the table is in force **and** on disk. `Err` of either kind
    /// means nothing changed: the previous table is still in force and still
    /// stored. There is no third state in which memory and disk disagree, and
    /// that is the whole point — an app is entitled to treat a refusal as
    /// authoritative and just re-read.
    ///
    /// DISK BEFORE MEMORY, deliberately. Writing first is what makes the "or
    /// nothing changed" half true: a failed write returns before `self.bindings`
    /// is touched, so there is no rollback to get wrong. The only window left is
    /// between a successful write and the assignment below, and a crash there
    /// resolves itself at the next start, because the next start reads the disk.
    pub fn set_bindings(
        &mut self,
        bindings: Vec<ButtonBinding>,
    ) -> Result<(), SetBindingsError> {
        validate(&bindings).map_err(SetBindingsError::Invalid)?;
        if let Some(store) = self.store.as_ref() {
            store
                .save(&bindings)
                .map_err(|error| SetBindingsError::NotPersisted {
                    detail: format!("{}: {error}", store.path().display()),
                })?;
        }
        self.bindings = bindings;
        Ok(())
    }

    /// The bindings currently in force, in the order last accepted.
    pub fn bindings(&self) -> &[ButtonBinding] {
        &self.bindings
    }

    /// Enter or leave learn mode. `now_ms` starts the safety timeout.
    pub fn set_learn_mode(&mut self, enabled: bool, now_ms: i64) {
        self.learn_since_ms = enabled.then_some(now_ms);
    }

    /// Whether learning is active, having first expired a stale session.
    pub fn learn_mode(&mut self, now_ms: i64) -> bool {
        if let Some(since) = self.learn_since_ms {
            if now_ms.saturating_sub(since) >= LEARN_MODE_TIMEOUT_MS {
                self.learn_since_ms = None;
            }
        }
        self.learn_since_ms.is_some()
    }

    /// Give up learn mode because the app that asked for it is gone.
    ///
    /// Called from the binder death recipient. Without it, a crashed setup app
    /// strands the buttons for the whole timeout.
    pub fn forget_learner(&mut self) {
        self.learn_since_ms = None;
    }

    /// Record a bezel contact and report the key to inject, if any.
    ///
    /// Returns `None` while learning — the press is still observed and still
    /// queued for subscribers, it is simply not acted on. That suppression is
    /// the whole reason a wizard can teach the button currently bound to Back
    /// without the press navigating the wizard away.
    ///
    /// The return value is about INJECTION ONLY and is not the delivery path.
    /// Every press is queued for `onButtonEvent` regardless of what this
    /// returns, because the two answer different questions and learn mode is
    /// exactly where they diverge: the wizard's live test needs to see the
    /// press it just suppressed. Take the events with [`Self::drain_pending`].
    ///
    /// (This doc comment used to claim a press "reaches subscribers". Until
    /// `drain_pending` existed, nothing dispatched `onButtonEvent` at all —
    /// see the note on that method.)
    pub fn observe_bezel(&mut self, x: i32, y: i32, now_ms: i64, duration_ms: i32) -> Option<i32> {
        let binding = self
            .bindings
            .iter()
            .find(|b| x < 0 && b.source == ButtonSource::Bezel && b.y_min <= y && y <= b.y_max);
        let matched = binding.map(|b| b.android_key_code).unwrap_or(0);
        let system_action = binding.map(|b| b.system_action).unwrap_or_default();
        let event = ButtonEvent {
            sequence: self.next_sequence,
            elapsed_realtime_ms: now_ms,
            source: ButtonSource::Bezel,
            raw_code: 0,
            x,
            y,
            press_duration_ms: duration_ms,
            android_key_code: matched,
            system_action,
        };
        self.next_sequence += 1;
        if self.recent.len() == RECENT_RING {
            self.recent.pop_front();
        }
        self.recent.push_back(event);

        // Queued BEFORE the learn-mode check on purpose: delivery is not
        // injection. A press suppressed by learn mode is the one the wizard
        // most needs to be told about.
        if self.pending.len() == PENDING_DELIVERY {
            self.pending.pop_front();
            self.pending_dropped = self.pending_dropped.saturating_add(1);
        }
        self.pending.push_back(event);

        if self.learn_mode(now_ms) || matched == 0 {
            return None;
        }
        Some(matched)
    }

    /// Take every press waiting to be pushed to `IHudCallback.onButtonEvent`.
    ///
    /// # Why this exists
    ///
    /// `IHud.aidl` gives the callback an `onButtonEvent` and, until now,
    /// nothing ever called it: the reader recorded presses into the ring and
    /// injected bound keys, and a subscriber found out only by polling
    /// `getRecentButtonEvents`. A setup wizard's live test mode — press a
    /// button, see it light up — cannot be built on a poll, and learn mode is
    /// useless without it, because in learn mode the poll is the *only* signal
    /// the press happened at all.
    ///
    /// # Why a queue and not a return value
    ///
    /// The reader must not hold this service's lock across a Binder
    /// transaction, and it must never block a `read()` on one. Draining into a
    /// plain `Vec` lets the caller release the lock first and transact after.
    /// That is the same shape `flush_callback_mailbox` already imposes on the
    /// MCU path.
    ///
    /// An event leaves this queue exactly once: two drains never deliver the
    /// same press twice.
    pub fn drain_pending(&mut self) -> PendingDelivery {
        PendingDelivery {
            events: self.pending.drain(..).collect(),
            dropped: std::mem::take(&mut self.pending_dropped),
        }
    }

    /// Last `max_count` presses, oldest first. `<= 0` means the whole ring.
    pub fn recent(&self, max_count: i32) -> Vec<ButtonEvent> {
        let take = if max_count <= 0 {
            self.recent.len()
        } else {
            (max_count as usize).min(self.recent.len())
        };
        self.recent.iter().skip(self.recent.len() - take).copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_enabled_physical_power_matches_act_and_learning_stays_observable() {
        let mut service = ButtonService::new();
        let mut bindings = measured_bindings();
        bindings[0].system_action = ButtonSystemAction::ShowGlobalActions;
        service.set_bindings(bindings).unwrap();
        let press = handle_press(&mut service, -30, 110, 10, 0, true);
        assert_eq!(press.system_action, ButtonSystemAction::ShowGlobalActions);
        assert_eq!(press.emit, EmitDecision::Unbound);
        service.set_learn_mode(true, 20);
        let learning = handle_press(&mut service, -30, 110, 30, 0, true);
        assert_eq!(learning.system_action, ButtonSystemAction::None);
        assert_eq!(learning.delivery.events[0].system_action, ButtonSystemAction::ShowGlobalActions);
        service.set_learn_mode(false, 40);
        for (x, y, enabled) in [(1, 110, true), (-30, 500, true), (-30, 110, false), (-30, 190, true)] {
            assert_eq!(handle_press(&mut service, x, y, 50, 0, enabled).system_action, ButtonSystemAction::None);
        }
        assert!(!service.recent(0).is_empty());
    }

    #[test]
    fn typed_power_table_reloads_and_preserves_all_legacy_bindings() {
        let legacy = measured_bindings();
        let mut upgraded = legacy.clone();
        upgraded[0].system_action = ButtonSystemAction::ShowGlobalActions;
        let path = std::env::temp_dir().join(format!("power-bindings-{}", std::process::id()));
        let store = ButtonStore::new(&path);
        let (mut service, _) = ButtonService::with_store(store.clone());
        service.set_bindings(legacy.clone()).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(before.starts_with(b"bengal-hud button-bindings 1\n"));
        service.set_bindings(upgraded.clone()).unwrap();
        let (reloaded, outcome) = ButtonService::with_store(store);
        assert_eq!(outcome, LoadOutcome::Loaded { count: 5 });
        assert_eq!(reloaded.bindings(), upgraded);
        assert_eq!(&reloaded.bindings()[1..], &legacy[1..]);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn typed_destination_conflicts_leave_disk_and_active_table_intact() {
        let temp = TempDir::new("action-conflict");
        let store = ButtonStore::new(temp.file());
        let (mut service, _) = ButtonService::with_store(store.clone());
        let legacy = measured_bindings();
        service.set_bindings(legacy.clone()).unwrap();
        let bytes = fs::read(store.path()).unwrap();
        let mut bad = legacy.clone();
        bad[1].system_action = ButtonSystemAction::ShowGlobalActions;
        assert!(service.set_bindings(bad).is_err());
        assert_eq!(service.bindings(), legacy);
        assert_eq!(fs::read(store.path()).unwrap(), bytes);
        let mut duplicate = legacy.clone();
        duplicate[0].system_action = ButtonSystemAction::ShowGlobalActions;
        duplicate[1].android_key_code = 0;
        duplicate[1].system_action = ButtonSystemAction::ShowGlobalActions;
        assert!(service.set_bindings(duplicate).is_err());
        assert_eq!(service.bindings(), legacy);
        assert_eq!(fs::read(store.path()).unwrap(), bytes);
    }

    /// The measured bands, as an app would learn them after two passes.
    fn measured_bindings() -> Vec<ButtonBinding> {
        [
            ("Power", 84, 134, 0),
            ("Home", 177, 202, 3),
            ("Back", 253, 289, 4),
            ("Volume Up", 336, 371, 24),
            ("Volume Down", 418, 448, 25),
        ]
        .into_iter()
        .map(|(label, y_min, y_max, code)| ButtonBinding {
            source: ButtonSource::Bezel,
            raw_code: 0,
            y_min,
            y_max,
            android_key_code: code,
            label: label.to_string(),
            system_action: ButtonSystemAction::None,
        })
        .collect()
    }


    #[test]
    fn the_four_android_key_codes_map_to_the_four_emittable_keys() {
        // Android KeyEvent numbering, not Linux input numbering. Pinned because
        // the two disagree silently and a swap looks like broken hardware.
        assert_eq!(BoundKey::from_android_code(3), Some(BoundKey::Home));
        assert_eq!(BoundKey::from_android_code(4), Some(BoundKey::Back));
        assert_eq!(BoundKey::from_android_code(24), Some(BoundKey::VolumeUp));
        assert_eq!(BoundKey::from_android_code(25), Some(BoundKey::VolumeDown));
    }

    #[test]
    fn power_and_unassigned_are_not_emittable() {
        // 26 is Android KEYCODE_POWER. It must not resolve: binding it would
        // let the owner suspend the unit from inside the setup wizard.
        assert_eq!(BoundKey::from_android_code(26), None, "POWER must never emit");
        assert_eq!(BoundKey::from_android_code(0), None, "0 = unassigned");
        // The Linux codes, fed in by mistake, must not silently work either.
        for linux_code in [102, 158, 115, 114] {
            assert_eq!(
                BoundKey::from_android_code(linux_code),
                None,
                "Linux code {linux_code} must not be mistaken for an Android one"
            );
        }
    }

    #[test]
    fn a_press_in_a_bound_band_yields_its_key() {
        let mut service = ButtonService::new();
        service.set_bindings(measured_bindings()).unwrap();
        // 189 is a real measured HOME contact.
        assert_eq!(service.observe_bezel(-30, 189, 1_000, 40), Some(3));
    }

    #[test]
    fn every_measured_contact_lands_in_its_own_band() {
        // Both capture passes, 2026-09-04. This is the check that the learned
        // ranges actually cover the hardware they were learned from.
        let mut service = ButtonService::new();
        service.set_bindings(measured_bindings()).unwrap();
        let capture: &[(i32, i32)] = &[
            (94, 0), (112, 0), (124, 0), (124, 0),
            (187, 3), (187, 3), (189, 3), (190, 3), (191, 3), (192, 3),
            (263, 4), (264, 4), (268, 4), (276, 4), (279, 4),
            (346, 24), (347, 24), (353, 24), (354, 24), (361, 24),
            (428, 25), (433, 25), (433, 25), (434, 25), (438, 25),
        ];
        for (y, expected) in capture {
            let got = service.observe_bezel(-30, *y, 1_000, 40);
            // Power is bound to 0 = "learned but not assigned", so it injects
            // nothing; the others must resolve to their measured key.
            let want = (*expected != 0).then_some(*expected);
            assert_eq!(got, want, "y={y} should resolve to {expected}");
        }
    }

    #[test]
    fn an_unbound_press_injects_nothing_but_is_still_recorded() {
        let mut service = ButtonService::new();
        assert_eq!(service.observe_bezel(-30, 189, 1_000, 40), None);
        let recent = service.recent(0);
        assert_eq!(recent.len(), 1, "an unbound press is still observable");
        assert_eq!(recent[0].android_key_code, 0);
        assert_eq!(recent[0].y, 189);
    }

    #[test]
    fn learn_mode_suppresses_injection_but_not_observation() {
        // The reason this exists: teaching the wizard which button means Back
        // requires pressing the button already bound to Back. If injection
        // stayed live that press would navigate the wizard away.
        let mut service = ButtonService::new();
        service.set_bindings(measured_bindings()).unwrap();
        service.set_learn_mode(true, 1_000);
        assert_eq!(service.observe_bezel(-30, 264, 1_100, 40), None, "must not inject BACK");
        assert_eq!(service.recent(0).len(), 1, "but the press must still be seen");
        assert_eq!(service.recent(0)[0].android_key_code, 4, "and report what it would have been");
    }

    #[test]
    fn learn_mode_expires_so_a_crashed_app_cannot_strand_the_buttons() {
        let mut service = ButtonService::new();
        service.set_bindings(measured_bindings()).unwrap();
        service.set_learn_mode(true, 0);
        assert_eq!(service.observe_bezel(-30, 264, 1_000, 40), None);
        // The setup app never came back.
        let later = LEARN_MODE_TIMEOUT_MS + 1;
        assert!(!service.learn_mode(later), "learn mode must time out");
        assert_eq!(
            service.observe_bezel(-30, 264, later, 40),
            Some(4),
            "the buttons must work again on their own"
        );
    }

    #[test]
    fn a_dead_learner_releases_learn_mode_immediately() {
        let mut service = ButtonService::new();
        service.set_bindings(measured_bindings()).unwrap();
        service.set_learn_mode(true, 0);
        service.forget_learner();
        assert_eq!(service.observe_bezel(-30, 264, 10, 40), Some(4));
    }

    #[test]
    fn overlapping_bands_are_refused() {
        let mut service = ButtonService::new();
        let mut bindings = measured_bindings();
        bindings[1].y_max = 260; // Home now reaches into Back
        let error = service.set_bindings(bindings).unwrap_err();
        assert!(
            matches!(
                error,
                SetBindingsError::Invalid(BindingError::OverlappingRanges { .. })
            ),
            "{error}"
        );
    }

    #[test]
    fn the_same_key_bound_twice_is_refused() {
        let mut service = ButtonService::new();
        let mut bindings = measured_bindings();
        bindings[3].android_key_code = 25; // Vol Up also claims Vol Down
        let error = service.set_bindings(bindings).unwrap_err();
        assert_eq!(
            error,
            SetBindingsError::Invalid(BindingError::DuplicateKeyCode { code: 25 })
        );
    }

    #[test]
    fn many_unassigned_buttons_are_not_duplicates() {
        // Zero means "learned but not assigned" and is the normal mid-setup
        // state for several buttons at once. Treating it as a duplicate would
        // make the wizard unable to save partial progress.
        let mut service = ButtonService::new();
        let mut bindings = measured_bindings();
        for b in &mut bindings {
            b.android_key_code = 0;
        }
        assert!(service.set_bindings(bindings).is_ok());
    }

    #[test]
    fn an_inverted_range_is_refused() {
        let mut service = ButtonService::new();
        let mut bindings = measured_bindings();
        bindings[0].y_min = 200;
        bindings[0].y_max = 100;
        assert!(matches!(
            service.set_bindings(bindings).unwrap_err(),
            SetBindingsError::Invalid(BindingError::InvertedRange { .. })
        ));
    }

    #[test]
    fn a_refused_table_leaves_the_previous_one_in_force() {
        // Refusing must not also disarm the buttons that already worked.
        let mut service = ButtonService::new();
        service.set_bindings(measured_bindings()).unwrap();
        let mut bad = measured_bindings();
        bad[1].y_max = 260;
        assert!(service.set_bindings(bad).is_err());
        assert_eq!(service.observe_bezel(-30, 264, 1_000, 40), Some(4));
        assert_eq!(service.bindings().len(), 5);
    }

    #[test]
    fn the_recent_ring_is_bounded_and_ordered_oldest_first() {
        let mut service = ButtonService::new();
        for i in 0..(RECENT_RING + 10) {
            service.observe_bezel(-30, 189, i as i64, 40);
        }
        let recent = service.recent(0);
        assert_eq!(recent.len(), RECENT_RING, "the ring must stay bounded");
        assert!(
            recent.windows(2).all(|w| w[0].sequence < w[1].sequence),
            "oldest first"
        );
        assert_eq!(
            recent.last().unwrap().sequence,
            (RECENT_RING + 10) as i64,
            "the newest press must survive"
        );
    }

    #[test]
    fn recent_respects_a_positive_cap_and_returns_the_newest() {
        let mut service = ButtonService::new();
        for i in 0..10 {
            service.observe_bezel(-30, 189, i, 40);
        }
        let recent = service.recent(3);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent.last().unwrap().sequence, 10);
        assert_eq!(recent.first().unwrap().sequence, 8);
    }

    // ---- the whole pipeline, over a fake evdev node ------------------------

    /// One `struct input_event` as the kernel lays it out on 64-bit.
    ///
    /// The 16-byte `timeval` is left zeroed: `BezelReader::next_press` reads
    /// type/code/value at offsets 16/18/20 and never looks at the timestamp.
    fn input_event(event_type: u16, code: u16, value: i32) -> [u8; 24] {
        let mut bytes = [0u8; 24];
        bytes[16..18].copy_from_slice(&event_type.to_ne_bytes());
        bytes[18..20].copy_from_slice(&code.to_ne_bytes());
        bytes[20..24].copy_from_slice(&value.to_ne_bytes());
        bytes
    }

    /// One completed contact, in the order the Goodix driver really delivered
    /// it in the D3-0114 capture: coordinates, BTN_TOUCH down, SYN, up.
    ///
    /// The Linux constants are restated here rather than imported from
    /// `bezel::decode`, which keeps them private. That is deliberate: an
    /// independent copy means a typo in the decoder's table shows up as a red
    /// test instead of agreeing with itself.
    fn contact(stream: &mut Vec<u8>, x: i32, y: i32) {
        const EV_SYN: u16 = 0x00;
        const EV_KEY: u16 = 0x01;
        const EV_ABS: u16 = 0x03;
        const SYN_REPORT: u16 = 0x00;
        const BTN_TOUCH: u16 = 0x14a;
        const ABS_MT_POSITION_X: u16 = 0x35;
        const ABS_MT_POSITION_Y: u16 = 0x36;

        stream.extend_from_slice(&input_event(EV_ABS, ABS_MT_POSITION_X, x));
        stream.extend_from_slice(&input_event(EV_ABS, ABS_MT_POSITION_Y, y));
        stream.extend_from_slice(&input_event(EV_KEY, BTN_TOUCH, 1));
        stream.extend_from_slice(&input_event(EV_SYN, SYN_REPORT, 0));
        stream.extend_from_slice(&input_event(EV_KEY, BTN_TOUCH, 0));
        stream.extend_from_slice(&input_event(EV_SYN, SYN_REPORT, 0));
    }

    /// Drive the real reader over a recorded stream and collect every decision.
    ///
    /// A REGULAR FILE is a faithful fake evdev node here, and that is not a
    /// shortcut: `BezelReader::open` is `File::open`, `next_press` is
    /// `read_exact` of fixed-size records, and a clean end of stream is
    /// `Ok(None)`. So the desk can replay a capture through the production
    /// reader with no uinput, no root and no device.
    fn replay(
        stream: &[u8],
        service: &mut ButtonService,
        injection_enabled: bool,
    ) -> Vec<PressAction> {
        let temp = TempDir::new("evdev");
        let node = temp.path.join("event-fake");
        fs::write(&node, stream).unwrap();

        let mut reader = crate::bezel::BezelReader::open(&node).expect("open the fake node");
        let mut actions = Vec::new();
        let mut tick = 0;
        while let Some(press) = reader.next_press().expect("read the fake node") {
            tick += 1;
            actions.push(handle_press(
                service,
                press.x,
                press.y,
                tick * 100,
                40,
                injection_enabled,
            ));
        }
        actions
    }

    #[test]
    fn a_recorded_bezel_stream_drives_the_whole_press_pipeline() {
        // End to end over the production reader: bytes -> decoder -> observe ->
        // delivery queue -> injection decision. These were the lines that had
        // no desk coverage at all, because spawn_bezel_reader is cfg(binder)
        // and opens a real /dev/input node.
        let mut stream = Vec::new();
        contact(&mut stream, -30, 189); // HOME band
        contact(&mut stream, -31, 264); // BACK band
        contact(&mut stream, 640, 300); // ON PANEL: Android's, not ours
        contact(&mut stream, -29, 428); // VOLUME DOWN band

        let mut service = ButtonService::new();
        service.set_bindings(measured_bindings()).unwrap();
        let actions = replay(&stream, &mut service, true);

        assert_eq!(
            actions.len(),
            3,
            "the on-panel contact must never reach the button pipeline"
        );
        assert_eq!(
            actions
                .iter()
                .map(|action| action.emit)
                .collect::<Vec<_>>(),
            vec![
                EmitDecision::Emit(BoundKey::Home),
                EmitDecision::Emit(BoundKey::Back),
                EmitDecision::Emit(BoundKey::VolumeDown),
            ]
        );
        // Exactly one delivered event per press, carrying the resolved key --
        // no batching, no duplication, no loss.
        for (action, expected) in actions.iter().zip([3, 4, 25]) {
            assert_eq!(action.delivery.dropped, 0);
            assert_eq!(action.delivery.events.len(), 1);
            assert_eq!(action.delivery.events[0].android_key_code, expected);
        }
        assert_eq!(
            service.recent(0).len(),
            3,
            "and all three are still pollable"
        );
    }

    #[test]
    fn a_truncated_evdev_record_ends_the_stream_instead_of_being_decoded() {
        // A short read is what a node gives on close or on a driver reset. It
        // must end the loop cleanly, not turn half a record into a press at
        // some arbitrary coordinate.
        let mut stream = Vec::new();
        contact(&mut stream, -30, 189);
        stream.extend_from_slice(&[0u8; 13]); // half an input_event

        let mut service = ButtonService::new();
        service.set_bindings(measured_bindings()).unwrap();
        let actions = replay(&stream, &mut service, true);
        assert_eq!(actions.len(), 1, "only the complete contact is a press");
    }

    #[test]
    fn the_injection_gate_does_not_stop_the_pipeline_over_a_real_stream() {
        // The injection-off configuration. Presses must still decode, still be
        // observed and still be delivered; only the key is withheld. This is
        // not the current shipping value -- device.mk:1193 now says 1 -- but it
        // is a supported one, and it is the half a build turns off when it
        // wants observation without synthesis.
        let mut stream = Vec::new();
        contact(&mut stream, -30, 189);
        contact(&mut stream, -31, 264);

        let mut service = ButtonService::new();
        service.set_bindings(measured_bindings()).unwrap();
        let actions = replay(&stream, &mut service, false);

        assert_eq!(actions.len(), 2);
        for action in &actions {
            assert_eq!(action.emit, EmitDecision::InjectionDisabled);
            assert_eq!(
                action.delivery.events.len(),
                1,
                "the app must still be told about a press the daemon may not inject"
            );
        }
    }

    #[test]
    fn handle_press_observes_before_it_drains() {
        // Ordering, pinned. Drain before observe and the press that just
        // arrived would be delivered one contact late -- which on five buttons
        // means the wizard highlights the previous button every time, a fault
        // that looks like bad hardware.
        let mut service = ButtonService::new();
        service.set_bindings(measured_bindings()).unwrap();
        let action = handle_press(&mut service, -30, 189, 1_000, 40, true);
        assert_eq!(
            action.delivery.events.len(),
            1,
            "the press being handled must be in its own delivery"
        );
        assert_eq!(action.delivery.events[0].y, 189);
    }

    // ---- the injection gate -----------------------------------------------

    #[test]
    fn injection_disabled_emits_nothing_whatever_is_bound() {
        // ro.vendor.bengal.hud.inject_keys shipped as 0 when the bezel path was
        // found to be ignoring it entirely: it opened /dev/uinput on the first
        // bound press regardless, so that image would have synthesised keys the
        // operator had switched off. It now ships as 1 (device.mk:1193), so the
        // gate is what a future build would rely on to turn synthesis back off.
        // Delete the `!injection_enabled` arm and this is the test that reds.
        for code in [3, 4, 24, 25] {
            assert_eq!(
                emit_decision(false, Some(code)),
                EmitDecision::InjectionDisabled,
                "android code {code} must not be emitted with injection off"
            );
        }
    }

    #[test]
    fn injection_enabled_emits_the_four_and_refuses_the_rest() {
        assert_eq!(emit_decision(true, Some(3)), EmitDecision::Emit(BoundKey::Home));
        assert_eq!(emit_decision(true, Some(4)), EmitDecision::Emit(BoundKey::Back));
        assert_eq!(
            emit_decision(true, Some(24)),
            EmitDecision::Emit(BoundKey::VolumeUp)
        );
        assert_eq!(
            emit_decision(true, Some(25)),
            EmitDecision::Emit(BoundKey::VolumeDown)
        );
        // 26 is Android KEYCODE_POWER: learnable, never emittable.
        assert_eq!(
            emit_decision(true, Some(26)),
            EmitDecision::NotEmittable {
                android_key_code: 26
            }
        );
        // A Linux code fed in by mistake must not resolve either.
        assert_eq!(
            emit_decision(true, Some(158)),
            EmitDecision::NotEmittable {
                android_key_code: 158
            }
        );
    }

    #[test]
    fn an_unbound_press_is_unbound_not_disabled() {
        // The two are different events and the daemon logs them differently:
        // one is "nothing is taught here yet", the other is "this unit is not
        // allowed to press keys". Reporting either as the other sends whoever
        // reads the log to the wrong place.
        assert_eq!(emit_decision(true, None), EmitDecision::Unbound);
        assert_eq!(emit_decision(false, None), EmitDecision::Unbound);
    }

    #[test]
    fn a_press_is_still_observed_and_delivered_when_injection_is_disabled() {
        // The gate is on SYNTHESISING KEYS, not on seeing presses. The wizard
        // must still be able to learn buttons on a unit that ships with
        // injection off -- otherwise the property would make the app that
        // configures the feature unusable.
        let mut service = ButtonService::new();
        service.set_bindings(measured_bindings()).unwrap();
        let resolved = service.observe_bezel(-30, 264, 1_000, 40);
        assert_eq!(resolved, Some(4), "the press still RESOLVES");
        assert_eq!(
            emit_decision(false, resolved),
            EmitDecision::InjectionDisabled,
            "it simply must not be emitted"
        );
        assert_eq!(
            service.drain_pending().events.len(),
            1,
            "and it must still reach the app"
        );
    }

    // ---- delivery to onButtonEvent ----------------------------------------

    #[test]
    fn every_observed_press_is_queued_for_delivery_once_and_in_order() {
        // Delete the queueing in observe_bezel, or the drain in the reader,
        // and this is the test that goes red.
        let mut service = ButtonService::new();
        service.set_bindings(measured_bindings()).unwrap();
        service.observe_bezel(-30, 189, 1_000, 40);
        service.observe_bezel(-30, 264, 1_100, 40);
        service.observe_bezel(-30, 428, 1_200, 40);

        let delivery = service.drain_pending();
        assert_eq!(delivery.dropped, 0);
        assert_eq!(
            delivery.events.iter().map(|e| e.y).collect::<Vec<_>>(),
            vec![189, 264, 428],
            "oldest first"
        );
        assert_eq!(
            delivery.events.iter().map(|e| e.android_key_code).collect::<Vec<_>>(),
            vec![3, 4, 25],
            "the app is told which key the press resolved to"
        );
        assert_eq!(
            service.drain_pending(),
            PendingDelivery::default(),
            "a press is delivered exactly once"
        );
    }

    #[test]
    fn a_press_suppressed_by_learn_mode_is_still_queued_for_delivery() {
        // This is the whole reason delivery is not the injection return value.
        // In learn mode the wizard suppresses injection deliberately, so the
        // pushed event is the ONLY signal that the press happened -- move the
        // queueing below the learn-mode check and this goes red.
        let mut service = ButtonService::new();
        service.set_bindings(measured_bindings()).unwrap();
        service.set_learn_mode(true, 1_000);

        assert_eq!(
            service.observe_bezel(-30, 264, 1_100, 40),
            None,
            "must not inject BACK while learning"
        );
        let delivery = service.drain_pending();
        assert_eq!(delivery.events.len(), 1, "but the app must still be told");
        assert_eq!(delivery.events[0].y, 264);
        assert_eq!(
            delivery.events[0].android_key_code, 4,
            "and told what it would have been"
        );
    }

    #[test]
    fn an_unbound_press_is_queued_for_delivery_too() {
        // During setup nothing is bound yet, and those are exactly the presses
        // the wizard needs in order to learn them.
        let mut service = ButtonService::new();
        service.observe_bezel(-30, 189, 1_000, 40);
        let delivery = service.drain_pending();
        assert_eq!(delivery.events.len(), 1);
        assert_eq!(delivery.events[0].android_key_code, 0);
    }

    #[test]
    fn the_two_bounds_are_pinned_at_their_values_not_merely_at_themselves() {
        // The two tests below express their expectations in terms of the very
        // constants they are checking, so they prove the queue and the ring are
        // BOUNDED without proving what they are bounded AT: change
        // PENDING_DELIVERY from 64 to 65 and both stay green. That is a real
        // hole — the lead's mutant found it on 2026-09-05 — and it matters
        // because these numbers are a memory budget in a daemon that runs for
        // the life of the boot, not an arbitrary choice.
        //
        // Everything here is a literal on purpose. If you are changing a bound
        // deliberately, change it here too and say why in the commit.
        assert_eq!(PENDING_DELIVERY, 64, "delivery queue bound changed");
        assert_eq!(RECENT_RING, 64, "getRecentButtonEvents ring bound changed");

        // And the behaviour AT that number, also in literals: 70 presses into a
        // 64-deep queue must yield exactly 64 delivered and exactly 6 dropped.
        let mut service = ButtonService::new();
        for tick in 0..70 {
            service.observe_bezel(-30, 189, tick, 40);
        }
        let delivery = service.drain_pending();
        assert_eq!(delivery.events.len(), 64);
        assert_eq!(delivery.dropped, 6);
        assert_eq!(service.recent(0).len(), 64);
    }

    #[test]
    fn the_delivery_queue_is_bounded_and_reports_what_it_dropped() {
        // An undrained queue must not grow without limit in a daemon that runs
        // for the life of the boot. Dropping the OLDEST is the right loss:
        // getRecentButtonEvents is the documented recovery, and the newest
        // press is the one the owner is waiting on.
        let mut service = ButtonService::new();
        for i in 0..(PENDING_DELIVERY + 10) {
            service.observe_bezel(-30, 189, i as i64, 40);
        }
        let delivery = service.drain_pending();
        assert_eq!(delivery.events.len(), PENDING_DELIVERY, "bounded");
        assert_eq!(delivery.dropped, 10, "and it says how many it lost");
        assert_eq!(
            delivery.events.last().unwrap().sequence,
            (PENDING_DELIVERY + 10) as i64,
            "the newest press survives"
        );
        assert_eq!(
            service.drain_pending().dropped,
            0,
            "the drop count resets with the drain it was reported on"
        );
    }

    #[test]
    fn a_drained_queue_does_not_disturb_the_recent_ring() {
        // The two are separate answers to separate questions; draining the
        // delivery queue must not consume the poll-recovery history.
        let mut service = ButtonService::new();
        service.observe_bezel(-30, 189, 1_000, 40);
        assert_eq!(service.drain_pending().events.len(), 1);
        assert_eq!(
            service.recent(0).len(),
            1,
            "getRecentButtonEvents still answers after a push"
        );
    }

    // ---- persistence ------------------------------------------------------

    use std::io::Seek;
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A scratch directory under `$TMPDIR`, removed by exact path on drop.
    ///
    /// Hand-rolled because this crate has no dependencies, so there is no
    /// `tempfile`. Removal is by the one exact path this instance created and
    /// never by a glob or a family prefix — on 2026-09-04 an `rm -rf …/vh*` in
    /// this project destroyed a different agent's live scratch.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "bengal-hud-buttons-{}-{tag}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("scratch directory");
            Self { path }
        }

        fn file(&self) -> PathBuf {
            self.path.join("button-bindings")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Every table `set_bindings` refuses, with the name of the rule it breaks.
    fn refused_tables() -> Vec<(&'static str, Vec<ButtonBinding>)> {
        let mut overlapping = measured_bindings();
        overlapping[1].y_max = 260; // Home reaches into Back
        let mut duplicate = measured_bindings();
        duplicate[3].android_key_code = 25; // Vol Up also claims Vol Down
        let mut inverted = measured_bindings();
        inverted[0].y_min = 200;
        inverted[0].y_max = 100;
        vec![
            ("overlapping bands", overlapping),
            ("one key claimed twice", duplicate),
            ("inverted range", inverted),
        ]
    }

    #[test]
    fn a_taught_table_round_trips_through_the_file() {
        let temp = TempDir::new("round-trip");
        let store = ButtonStore::new(temp.file());
        store.save(&measured_bindings()).unwrap();
        assert_eq!(store.read().unwrap().unwrap(), measured_bindings());
    }

    #[test]
    fn labels_that_could_forge_the_format_round_trip_intact() {
        // The label is owner-facing free text and the daemon never parses it,
        // so it is the one field that can carry the format's own delimiters. If
        // any of these came back changed, an owner's button would silently
        // rename itself — or, worse, a label could inject a whole extra line
        // and give itself a second binding.
        let hostile = [
            "Power",
            "",
            "with space",
            "tab\there",
            "newline\nhere",
            "carriage\rreturn",
            "back\\slash",
            "escaped\\tnot-a-tab",
            "bezel\t0\t0\t0\t3\tinjected",
            "end\t0",
            "音量アップ",
            "Volume ⏶",
            &"x".repeat(500),
        ];
        let table: Vec<ButtonBinding> = hostile
            .iter()
            .enumerate()
            .map(|(index, label)| ButtonBinding {
                source: ButtonSource::Bezel,
                raw_code: 0,
                // Non-overlapping and each key distinct, so validate() passes
                // and the round trip is testing the format, not the rules.
                y_min: index as i32 * 10,
                y_max: index as i32 * 10 + 5,
                android_key_code: 0,
                label: (*label).to_string(),
                system_action: ButtonSystemAction::None,
            })
            .collect();
        let temp = TempDir::new("labels");
        let store = ButtonStore::new(temp.file());
        store.save(&table).unwrap();
        let back = store.read().unwrap().unwrap();
        assert_eq!(back.len(), table.len(), "a label must not add or eat a line");
        assert_eq!(back, table);
    }

    #[test]
    fn the_wizards_slot_labels_round_trip_byte_for_byte() {
        // The HuButtons wizard uses `label` as the SLOT IDENTITY: it writes
        // "1. Top button" … "5. Bottom button" and parses the leading ordinal
        // to put each row on the right piece of glass, because
        // getButtonBindings() is documented unordered. The AIDL allows that —
        // the label is the app's own and the daemon treats it as opaque — so
        // this format must not touch a single byte of it. A lost "." or a
        // collapsed space would move a row to the wrong button, and the owner
        // would see the wizard blame the wrong piece of hardware.
        let wizard_labels = [
            "1. Top button",
            "2. Upper middle button",
            "3. Middle button",
            "4. Lower middle button",
            "5. Bottom button",
        ];
        // Plus the two characters the format itself uses, which is where an
        // escaping bug would actually show up.
        let adversarial = ["6.\tTab in a slot name", "7. Back\\slash in a slot name"];

        let table: Vec<ButtonBinding> = wizard_labels
            .iter()
            .chain(adversarial.iter())
            .enumerate()
            .map(|(index, label)| ButtonBinding {
                source: ButtonSource::Bezel,
                raw_code: 0,
                y_min: index as i32 * 60,
                y_max: index as i32 * 60 + 30,
                android_key_code: 0,
                label: (*label).to_string(),
                system_action: ButtonSystemAction::None,
            })
            .collect();

        let temp = TempDir::new("wizard-labels");
        let store = ButtonStore::new(temp.file());
        store.save(&table).unwrap();
        let back = store.read().unwrap().unwrap();

        assert_eq!(back.len(), table.len());
        for (stored, original) in back.iter().zip(table.iter()) {
            assert_eq!(
                stored.label.as_bytes(),
                original.label.as_bytes(),
                "label must survive byte for byte: {:?}",
                original.label
            );
        }
    }

    #[test]
    fn bindings_survive_a_daemon_restart() {
        // THE point of this module. Delete the `store.read()` arm in
        // `with_store` and this is the test that goes red.
        let temp = TempDir::new("restart");
        let (mut first, outcome) = ButtonService::with_store(ButtonStore::new(temp.file()));
        assert_eq!(outcome, LoadOutcome::Absent, "a fresh unit has no table");
        first.set_bindings(measured_bindings()).unwrap();
        drop(first);

        // A second process over the same path: this is what a reboot looks
        // like from the daemon's side.
        let (mut second, outcome) = ButtonService::with_store(ButtonStore::new(temp.file()));
        assert_eq!(outcome, LoadOutcome::Loaded { count: 5 });
        assert_eq!(
            second.observe_bezel(-30, 264, 1_000, 40),
            Some(4),
            "BACK must still work after a restart"
        );
        assert_eq!(second.bindings(), measured_bindings().as_slice());
    }

    #[test]
    fn a_unit_that_was_never_taught_starts_empty_and_writes_nothing() {
        let temp = TempDir::new("absent");
        let store = ButtonStore::new(temp.file());
        let path = store.path().to_path_buf();
        let (service, outcome) = ButtonService::with_store(store);
        assert_eq!(outcome, LoadOutcome::Absent);
        assert!(service.bindings().is_empty());
        assert!(
            !path.exists(),
            "startup must not create the file; only an accepted table writes"
        );
    }

    #[test]
    fn the_file_path_and_the_binder_path_refuse_exactly_the_same_tables() {
        // "It came from disk" is not evidence. A file is a plain text file on a
        // rooted development unit, and a loader with its own, drifting copy of
        // the rules is the specific failure this pins: both paths must call the
        // one `validate`, and must refuse for the SAME reason.
        let temp = TempDir::new("shared-rules");
        let store = ButtonStore::new(temp.file());
        for (name, table) in refused_tables() {
            let mut service = ButtonService::new();
            let over_binder = match service.set_bindings(table.clone()) {
                Err(SetBindingsError::Invalid(error)) => error,
                other => panic!("{name}: the interface must refuse this: {other:?}"),
            };

            // Written behind the validator's back, exactly as a hand edit would
            // be: `save` is not asked, the bytes just appear.
            fs::write(store.path(), serialise_table(&table)).unwrap();
            let over_disk = match store.read() {
                Err(LoadError::Invalid { error }) => error,
                other => panic!("{name}: a table the interface refuses must not load: {other:?}"),
            };
            assert_eq!(
                over_binder, over_disk,
                "{name}: the two paths must refuse for the same reason"
            );
        }
    }

    #[test]
    fn a_refused_file_leaves_the_buttons_unbound_and_the_evidence_on_disk() {
        let temp = TempDir::new("refused");
        let store = ButtonStore::new(temp.file());
        let mut overlapping = measured_bindings();
        overlapping[1].y_max = 260;
        let bytes = serialise_table(&overlapping);
        fs::write(store.path(), &bytes).unwrap();

        let path = store.path().to_path_buf();
        let (mut service, outcome) = ButtonService::with_store(store);
        assert!(
            matches!(
                outcome,
                LoadOutcome::Refused {
                    error: LoadError::Invalid { .. }
                }
            ),
            "{outcome:?}"
        );
        assert!(
            service.bindings().is_empty(),
            "dead buttons the owner can re-teach beat wrong buttons they cannot explain"
        );
        assert_eq!(
            service.observe_bezel(-30, 264, 1_000, 40),
            None,
            "nothing from a refused table may reach the injector"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            bytes,
            "the refused file is the only evidence of what went wrong; it stays"
        );

        // And the owner can put it right without any tooling.
        service.set_bindings(measured_bindings()).unwrap();
        let (reloaded, outcome) = ButtonService::with_store(ButtonStore::new(&path));
        assert_eq!(outcome, LoadOutcome::Loaded { count: 5 });
        assert_eq!(reloaded.bindings(), measured_bindings().as_slice());
    }

    #[test]
    fn a_save_never_touches_the_file_already_in_place() {
        // Write-and-rename, proven rather than asserted: hold an open handle to
        // the old file across a save. If the implementation opened the target
        // for writing — truncate-then-write, the thing a power cut turns into a
        // half file — the held handle would see the new bytes or a short read.
        // Replace `save`'s rename with a write-in-place and this goes red.
        let temp = TempDir::new("atomic");
        let store = ButtonStore::new(temp.file());
        let first = measured_bindings();
        store.save(&first).unwrap();

        let mut held = fs::File::open(store.path()).unwrap();
        let before_inode = fs::metadata(store.path()).unwrap().ino();

        let mut second = measured_bindings();
        second[1].label = "Menu".to_string();
        store.save(&second).unwrap();

        let mut still_there = String::new();
        held.rewind().unwrap();
        held.read_to_string(&mut still_there).unwrap();
        assert_eq!(
            still_there,
            serialise_table(&first),
            "the old file must be intact and complete, not truncated or rewritten"
        );
        assert_ne!(
            before_inode,
            fs::metadata(store.path()).unwrap().ino(),
            "the target must be a NEW inode renamed into place"
        );
        assert_eq!(store.read().unwrap().unwrap(), second);
    }

    #[test]
    fn the_staging_file_is_a_sibling_and_does_not_outlive_the_save() {
        // rename(2) is atomic only within one filesystem. A staging file in
        // /tmp or /data/local/tmp would silently turn the atomic swap into a
        // copy that a power cut can interrupt.
        let temp = TempDir::new("staging");
        let store = ButtonStore::new(temp.file());
        assert_eq!(
            store.temp_path().parent(),
            store.path().parent(),
            "the staging file must be a sibling of the target"
        );
        store.save(&measured_bindings()).unwrap();
        assert!(
            !store.temp_path().exists(),
            "a successful save leaves no staging file behind"
        );
    }

    #[test]
    fn every_truncation_of_a_good_file_is_refused() {
        // What a power cut would produce if write-and-rename were not there.
        // The terminator is what makes a short file detectably short.
        let temp = TempDir::new("truncated");
        let store = ButtonStore::new(temp.file());
        let full = serialise_table(&measured_bindings());
        // The final newline is cosmetic: a file that lost only that byte still
        // describes the same table and is accepted. Everything shorter must
        // not be.
        for length in 0..full.len() - 1 {
            fs::write(store.path(), &full.as_bytes()[..length]).unwrap();
            match store.read() {
                Err(LoadError::Malformed { .. }) => {}
                other => panic!("a {length}-byte truncation must not load: {other:?}"),
            }
        }
        fs::write(store.path(), &full.as_bytes()[..full.len() - 1]).unwrap();
        assert_eq!(
            store.read().unwrap().unwrap(),
            measured_bindings(),
            "losing only the trailing newline is not corruption"
        );
    }

    #[test]
    fn a_file_from_a_future_version_is_refused_rather_than_guessed_at() {
        let temp = TempDir::new("version");
        let store = ButtonStore::new(temp.file());
        let future = serialise_table(&measured_bindings())
            .replace("button-bindings 1", "button-bindings 999");
        fs::write(store.path(), future).unwrap();
        assert!(
            matches!(
                store.read(),
                Err(LoadError::Malformed {
                    error: FormatError::BadHeader { .. }
                })
            ),
            "this build cannot know what a field it has never seen means"
        );
    }

    #[test]
    fn structural_damage_is_named_rather_than_lumped_together() {
        // Each of these reaches a different person's next move: a hand edit, a
        // half-written file, an appended second table.
        let temp = TempDir::new("structure");
        let store = ButtonStore::new(temp.file());
        let good = serialise_table(&measured_bindings());
        let cases: Vec<(&str, String, FormatError)> = vec![
            (
                "no terminator",
                good.replace("end\t5\n", ""),
                FormatError::MissingTerminator,
            ),
            (
                "count lies",
                good.replace("end\t5", "end\t4"),
                FormatError::CountMismatch {
                    declared: 4,
                    found: 5,
                },
            ),
            (
                "a second table appended",
                format!("{good}{good}"),
                FormatError::TrailingContent { line: 8 },
            ),
            (
                "a field went missing",
                good.replace("bezel\t0\t84\t134\t0\tPower", "bezel\t0\t84\t134\tPower"),
                FormatError::BadFieldCount { line: 2, found: 5 },
            ),
            (
                "a number is not a number",
                good.replace("bezel\t0\t84\t134", "bezel\t0\teighty-four\t134"),
                FormatError::BadNumber {
                    line: 2,
                    field: "y_min",
                    found: "eighty-four".to_string(),
                },
            ),
            (
                "an unknown transport",
                good.replace("bezel\t0\t84", "knob\t0\t84"),
                FormatError::BadSource {
                    line: 2,
                    found: "knob".to_string(),
                },
            ),
            (
                "an undefined escape",
                good.replace("\tPower", "\t\\qPower"),
                FormatError::BadEscape { line: 2 },
            ),
        ];
        for (name, bytes, expected) in cases {
            fs::write(store.path(), &bytes).unwrap();
            match store.read() {
                Err(LoadError::Malformed { error }) => assert_eq!(error, expected, "{name}"),
                other => panic!("{name}: must be refused as malformed, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_file_that_is_not_utf8_is_refused() {
        let temp = TempDir::new("utf8");
        let store = ButtonStore::new(temp.file());
        let mut bytes = serialise_table(&measured_bindings()).into_bytes();
        bytes.extend_from_slice(&[0xff, 0xfe]);
        fs::write(store.path(), &bytes).unwrap();
        assert_eq!(
            store.read(),
            Err(LoadError::Malformed {
                error: FormatError::NotUtf8
            })
        );
    }

    #[test]
    fn an_oversized_file_is_refused_without_being_read_whole() {
        let temp = TempDir::new("oversized");
        let store = ButtonStore::new(temp.file());
        let mut bytes = serialise_table(&measured_bindings()).into_bytes();
        bytes.resize(MAX_FILE_BYTES as usize * 4, b'x');
        fs::write(store.path(), &bytes).unwrap();
        assert_eq!(
            store.read(),
            Err(LoadError::TooLarge {
                limit: MAX_FILE_BYTES
            }),
            "a corrupt file must not decide how much the daemon allocates at boot"
        );
    }

    #[test]
    fn a_table_too_big_to_read_back_is_refused_at_save_time() {
        // The two limits are one limit. A daemon that writes a file it will
        // refuse to load has arranged for its own bindings to vanish at the
        // next reboot, with the failure a whole boot away from its cause.
        let temp = TempDir::new("too-big");
        let store = ButtonStore::new(temp.file());
        let huge: Vec<ButtonBinding> = (0..200)
            .map(|index| ButtonBinding {
                source: ButtonSource::Bezel,
                raw_code: 0,
                y_min: index * 10,
                y_max: index * 10 + 5,
                android_key_code: 0,
                label: "L".repeat(400),
                system_action: ButtonSystemAction::None,
            })
            .collect();
        assert!(serialise_table(&huge).len() as u64 > MAX_FILE_BYTES);

        let mut service = ButtonService::with_store(ButtonStore::new(temp.file())).0;
        let error = service
            .set_bindings(huge)
            .expect_err("the table is legal but the file it makes is unreadable");
        assert!(
            matches!(error, SetBindingsError::NotPersisted { .. }),
            "{error:?}"
        );
        assert!(
            !store.path().exists() && !store.temp_path().exists(),
            "a refused save must leave neither a target nor a staging file"
        );
        assert!(
            service.bindings().is_empty(),
            "and must not apply the table it could not store"
        );
    }

    #[test]
    fn a_save_failure_refuses_the_edit_and_leaves_the_previous_table_in_force() {
        // REVERSED on the wizard author's argument. The first version applied
        // the table in memory anyway and only logged, reasoning that working
        // buttons now beat a refusal the owner did not cause. That shows the
        // owner a binding which SAVED and which will silently not be there
        // after the next start. A refusal the app can see and report beats a
        // success that is not true — and it keeps one rule for every failure:
        // memory and disk never disagree.
        let temp = TempDir::new("unwritable");
        let missing = temp.path.join("never-created-by-init").join("button-bindings");
        let mut service = ButtonService::with_store(ButtonStore::new(&missing)).0;

        match service.set_bindings(measured_bindings()).unwrap_err() {
            SetBindingsError::NotPersisted { detail } => assert!(
                detail.contains("never-created-by-init"),
                "the error must name the path: {detail}"
            ),
            other => panic!("expected a persistence refusal, got {other:?}"),
        }
        assert!(
            service.bindings().is_empty(),
            "a refused save must not leave a half-applied table in memory"
        );
        assert_eq!(
            service.observe_bezel(-30, 264, 1_000, 40),
            None,
            "nothing from the refused table may reach the injector"
        );
    }

    #[test]
    fn a_save_failure_does_not_disturb_the_table_already_in_force() {
        // The stronger half of the same rule: a failing edit on a unit that is
        // already working must leave the working table alone, in memory and on
        // disk both.
        let temp = TempDir::new("unwritable-after-good");
        let store = ButtonStore::new(temp.file());
        let mut service = ButtonService::with_store(ButtonStore::new(temp.file())).0;
        service.set_bindings(measured_bindings()).unwrap();
        let stored = fs::read(store.path()).unwrap();

        // Make the write fail without touching the good file: a table that is
        // valid but serialises past the read-back limit.
        let unwritable: Vec<ButtonBinding> = (0..200)
            .map(|index| ButtonBinding {
                source: ButtonSource::Bezel,
                raw_code: 0,
                y_min: index * 10,
                y_max: index * 10 + 5,
                android_key_code: 0,
                label: "L".repeat(400),
                system_action: ButtonSystemAction::None,
            })
            .collect();
        assert!(matches!(
            service.set_bindings(unwritable).unwrap_err(),
            SetBindingsError::NotPersisted { .. }
        ));

        assert_eq!(
            service.bindings(),
            measured_bindings().as_slice(),
            "the working table stays in force"
        );
        assert_eq!(
            fs::read(store.path()).unwrap(),
            stored,
            "and stays on disk, byte for byte"
        );
        assert_eq!(service.observe_bezel(-30, 264, 1_000, 40), Some(4));
    }

    #[test]
    fn an_emptied_table_persists_as_empty_and_is_not_an_absent_file() {
        // "The owner deleted every binding" and "this unit was never taught"
        // look the same in memory and must not look the same on disk: the first
        // is a decision to keep, the second is a state to leave alone.
        let temp = TempDir::new("empty");
        let mut service = ButtonService::with_store(ButtonStore::new(temp.file())).0;
        service.set_bindings(measured_bindings()).unwrap();
        service.set_bindings(Vec::new()).unwrap();

        let (reloaded, outcome) = ButtonService::with_store(ButtonStore::new(temp.file()));
        assert_eq!(outcome, LoadOutcome::Loaded { count: 0 });
        assert!(reloaded.bindings().is_empty());
    }

    #[test]
    fn a_refused_table_is_never_written() {
        let temp = TempDir::new("refused-not-written");
        let store = ButtonStore::new(temp.file());
        let mut service = ButtonService::with_store(ButtonStore::new(temp.file())).0;
        service.set_bindings(measured_bindings()).unwrap();
        let good = fs::read(store.path()).unwrap();

        let mut overlapping = measured_bindings();
        overlapping[1].y_max = 260;
        assert!(service.set_bindings(overlapping).is_err());
        assert_eq!(
            fs::read(store.path()).unwrap(),
            good,
            "a refused edit must not disturb the stored table either"
        );
    }

    #[test]
    fn the_default_path_is_under_data_vendor() {
        // /vendor is read-only and replaced wholesale by a flash; /data/vendor
        // exists from post-fs-data and is device-encrypted, so it is readable
        // at boot without anyone unlocking anything.
        assert!(
            DEFAULT_BINDINGS_PATH.starts_with("/data/vendor/"),
            "{DEFAULT_BINDINGS_PATH}"
        );
        assert_eq!(
            ButtonStore::default_path().path(),
            Path::new(DEFAULT_BINDINGS_PATH)
        );
    }
}
