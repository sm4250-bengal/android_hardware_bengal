// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! MCU (STM32 on GENI SE0) — codec, transmit gate and ACC lifecycle.
//!
//! # What is here and what is not
//!
//! A streaming frame parser, the typed events it produces, the port resolver,
//! and [`acc`]'s pure state machine. Only the two fixed stock ACC lifecycle
//! replies are transmittable; [`may_transmit`] refuses every other frame.
//!
//! # The wire format
//!
//! ```text
//!   5a a5   LEN   CMD  [payload …]   CS
//!   \___/    |     \_________________/
//!  preamble  |      LEN - 1 bytes, checksummed
//!            |
//!            LEN = 1 (CMD) + payload + 1 (CS);  total frame = LEN + 3
//!
//!   CS = (0 - (CMD + Σ payload)) & 0xff        two's complement, LEN excluded
//! ```
//!
//! Worked from a real device-3 capture (D3-0043 `result.log:114`):
//! `5a a5 03 12 04 ea` — LEN 3, so 6 bytes total; `0x12 + 0x04 = 0x16`;
//! `-0x16 & 0xff = 0xea`. ✓
//!
//! # Two oracles, one algorithm
//!
//! This parser is a third implementation of a format that already has two, and
//! they agree byte for byte:
//!
//! * `~/android/apps/mcu-toolbox/bridge/main.go` — `checksumOK()` at :2365 and
//!   the scanner at :2430, whose own comment says `total = buf[2] + 3` "matches
//!   `NewkeypadMonitor.onDataReceived()`", i.e. the stock Android service.
//! * `kernel/initramfs/diagnostic-init.c` — `mcu_checksum()` :2251,
//!   `mcu_next_frame()` :2286, `mcu_frame_ok()` :2322. This is PID 1, and it is
//!   what produced the capture the tests below replay.
//!
//! `tools/decoder-agreement.sh` lifts PID 1's three functions out of that file,
//! compiles them standalone, runs them over the same captures as
//! [`tests::GOLDEN`] and refuses to pass if the two disagree. So "they agree" is
//! a check, not a claim.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

pub mod acc;
pub mod gps_ready;
// The default Cargo library has no Binder consumer; Android and its tests do.
#[cfg_attr(not(feature = "binder"), allow(dead_code))]
pub(crate) mod diagnostics;
#[cfg(test)]
mod device1_fixtures;
pub mod serial;
pub mod uinput;

#[cfg(test)]
pub(crate) mod acc_go_fixtures;

// ---------------------------------------------------------------- the port

/// SE0's MMIO address. **The port is resolved by this, never by `ttyHS` name.**
///
/// D15 §10.5, measured: D3-0020 enumerated SE0 as `ttyHS0`, D3-0021 and
/// D3-0023 as `ttyHS1`, and the upstream tree has no `ttyHS0` at all. The name
/// is allocation order; the MMIO address is a board fact. Same class of rule as
/// [`crate::i2c::adapter`]'s — resolve by what a thing *is*.
///
/// D3-0043 `result.log:105-106` shows PID 1 doing exactly this:
/// `ttyHS1=4a80000.serial:0.0` → `/dev/ttyHS1 dev 237:1 fronts 4a80000`.
pub const SE0_MMIO: &str = "4a80000";

/// Bluetooth is SE3, and this daemon must never open it
/// (`mem:mcu-on-se0-bt-on-se3`; older docs saying "SE3 = MCU" are wrong on the
/// port). A constant so that a resolver bug landing here is loud.
pub const SE3_MMIO_NOT_OURS: &str = "4a8c000";

/// Find the one tty whose sysfs device link carries `mmio` as a delimited hex
/// token.
///
/// Walks `<sysfs_root>/class/tty/*/device`, so it answers "which node is the
/// SE0 UART" from the board description rather than from enumeration order. A
/// missing or unreadable class proves nothing and returns `Ok(None)`; multiple
/// exact matches are ambiguous and explicitly refused.
pub fn resolve_port(sysfs_root: &Path, mmio: &str) -> io::Result<Option<PathBuf>> {
    let class = sysfs_root.join("class/tty");
    let Ok(entries) = std::fs::read_dir(class) else {
        return Ok(None);
    };
    let mut found: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let link = match std::fs::read_link(entry.path().join("device")) {
            Ok(l) => l,
            Err(_) => continue,
        };
        if link
            .to_string_lossy()
            .split(|ch: char| !ch.is_ascii_hexdigit())
            .any(|token| token == mmio)
        {
            let candidate = PathBuf::from(format!("/dev/{}", name));
            if let Some(previous) = found {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "multiple tty nodes front MMIO 0x{mmio}: {} and {}",
                        previous.display(),
                        candidate.display()
                    ),
                ));
            }
            found = Some(candidate);
        }
    }
    Ok(found)
}

// ------------------------------------------------------------- the codec

/// Frame preamble, byte 0.
pub const H0: u8 = 0x5a;
/// Frame preamble, byte 1.
pub const H1: u8 = 0xa5;
/// Frame preamble.
pub const PREAMBLE: [u8; 2] = [H0, H1];

/// Largest total frame length. `bridge/main.go:45 maxFrame = 258`, mirrored by
/// `diagnostic-init.c:2249 MCU_FRAME_MAX`.
pub const FRAME_MAX: usize = 258;

/// Smallest total frame length: preamble + LEN + CMD + CS.
pub const FRAME_MIN: usize = 5;

/// How much unparsed input the parser will hold before dropping it.
///
/// The bridge uses 4096 (`main.go:2494`). A stream that never contains a valid
/// header is a stream we are misreading, and holding it forever turns a wiring
/// fault into an OOM.
pub const MAX_PENDING: usize = 4096;

/// Two's complement of the sum of `payload` (CMD included, LEN excluded).
///
/// Identical to `bridge/main.go:2365 checksumOK` and to
/// `diagnostic-init.c:2251 mcu_checksum`.
pub fn checksum(payload: &[u8]) -> u8 {
    let sum = payload.iter().fold(0u8, |a, b| a.wrapping_add(*b));
    sum.wrapping_neg()
}

/// A complete frame, preamble through checksum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    raw: Vec<u8>,
}

impl Frame {
    /// The whole frame including preamble, LEN and checksum.
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    /// `CMD` and everything after it, up to but excluding the checksum.
    ///
    /// This is what the bridge calls the payload and what its
    /// `inputInjector.handle` dispatches on; always at least one byte, because
    /// [`FRAME_MIN`] is 5.
    pub fn payload(&self) -> &[u8] {
        &self.raw[3..self.raw.len() - 1]
    }

    /// The command byte.
    pub fn command(&self) -> u8 {
        self.raw[3]
    }

    /// Does the trailing byte match [`checksum`] over the payload?
    pub fn checksum_ok(&self) -> bool {
        *self.raw.last().unwrap() == checksum(self.payload())
    }

    /// Decode the payload into a typed event.
    pub fn event(&self) -> Event {
        Event::decode(self.payload())
    }
}

impl fmt::Display for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, b) in self.raw.iter().enumerate() {
            if i > 0 {
                write!(f, " ")?;
            }
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

/// Why bytes were thrown away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discard {
    /// Bytes before a preamble.
    BeforeHeader,
    /// `5a a5` followed by a LEN that cannot be a frame. One byte is dropped
    /// and the hunt resumes — the `5a` was not a header after all.
    ImpossibleLength(u8),
    /// The pending buffer passed [`MAX_PENDING`] without yielding a frame.
    BufferLimit,
}

/// What [`Parser::push`] produces, in stream order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// A complete frame whose checksum matched.
    Frame(Frame),
    /// A complete frame whose checksum did **not** match. Kept rather than
    /// dropped: a rising bad-frame count is the tell for a baud-rate or wiring
    /// fault, and PID 1 reports exactly this pair ("frames valid N bad M").
    BadChecksum(Frame),
    /// Bytes that could not begin a frame.
    Discarded {
        /// How many.
        count: usize,
        /// Why.
        reason: Discard,
    },
}

/// Running totals, in PID 1's shape so the two can be compared directly.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Checksum-valid frames.
    pub valid: u64,
    /// Complete frames with a bad checksum.
    pub bad: u64,
    /// Bytes discarded.
    pub discarded: u64,
}

/// A resynchronising, restartable frame parser.
///
/// Feed it whatever `read()` returned; it keeps the tail that is not yet a
/// whole frame. **Splitting a frame across reads is the normal case**, not an
/// edge case: the MCU polls, the tty delivers in whatever chunks the driver
/// felt like, and a parser that assumes read boundaries are frame boundaries
/// works on the desk and fails on the unit.
#[derive(Debug, Default)]
pub struct Parser {
    pending: Vec<u8>,
    stats: Stats,
}

impl Parser {
    /// A parser with an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Totals since construction.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Bytes held back waiting for the rest of a frame.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Feed bytes; get back everything that became decidable.
    ///
    /// The algorithm is `bridge/main.go:2430` and `diagnostic-init.c:2286`,
    /// which are the same algorithm:
    ///
    /// 1. hunt for `5a a5`; bytes before it are [`Discard::BeforeHeader`];
    /// 2. a lone `5a` at the very end is **kept** — the `a5` may be in the next
    ///    read;
    /// 3. `total = LEN + 3`; outside `5..=258` the `5a` was not a header, so
    ///    drop exactly one byte and resume hunting;
    /// 4. short of `total` bytes, keep everything and wait;
    /// 5. otherwise emit the frame, valid or not, and continue after it.
    pub fn push(&mut self, data: &[u8]) -> Vec<Item> {
        self.pending.extend_from_slice(data);
        let mut out = Vec::new();
        let mut i = 0usize;

        loop {
            // 1/2: hunt for the preamble.
            let mut start = None;
            let mut j = i;
            while j + 1 < self.pending.len() {
                if self.pending[j] == H0 && self.pending[j + 1] == H1 {
                    start = Some(j);
                    break;
                }
                j += 1;
            }
            let start = match start {
                Some(s) => s,
                None => {
                    // Keep a trailing lone 5a; drop the rest.
                    let keep = if self.pending.last() == Some(&H0) {
                        1
                    } else {
                        0
                    };
                    let drop_from = i;
                    let drop_to = self.pending.len() - keep;
                    if drop_to > drop_from {
                        let count = drop_to - drop_from;
                        out.push(Item::Discarded {
                            count,
                            reason: Discard::BeforeHeader,
                        });
                        self.stats.discarded += count as u64;
                    }
                    i = drop_to;
                    break;
                }
            };
            if start > i {
                let count = start - i;
                out.push(Item::Discarded {
                    count,
                    reason: Discard::BeforeHeader,
                });
                self.stats.discarded += count as u64;
                i = start;
            }

            // need LEN
            if i + 2 >= self.pending.len() {
                break;
            }
            // 3: declared length
            let total = self.pending[i + 2] as usize + 3;
            if !(FRAME_MIN..=FRAME_MAX).contains(&total) {
                out.push(Item::Discarded {
                    count: 1,
                    reason: Discard::ImpossibleLength(self.pending[i + 2]),
                });
                self.stats.discarded += 1;
                i += 1;
                continue;
            }
            // 4: complete?
            if self.pending.len() - i < total {
                break;
            }
            // 5: emit
            let frame = Frame {
                raw: self.pending[i..i + total].to_vec(),
            };
            if frame.checksum_ok() {
                self.stats.valid += 1;
                out.push(Item::Frame(frame));
            } else {
                self.stats.bad += 1;
                out.push(Item::BadChecksum(frame));
            }
            i += total;
        }

        self.pending.drain(..i);

        if self.pending.len() > MAX_PENDING {
            let count = self.pending.len();
            self.pending.clear();
            self.stats.discarded += count as u64;
            out.push(Item::Discarded {
                count,
                reason: Discard::BufferLimit,
            });
        }
        out
    }
}

// -------------------------------------------------------------- the events

/// Press or release, from the second payload byte of a `0x10` button frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonState {
    /// `0x01`.
    Press,
    /// `0x02`.
    Release,
    /// Anything else.
    Other(u8),
}

/// A decoded MCU payload.
///
/// # The source is the stock service, not the bridge
///
/// Every arm below cites `NewkeypadMonitor.java` — the decompiled OEM
/// `services.jar` class, at
/// `~/android/archive/PGM-factory-non-AOSP-inventory/evidence/framework-hooks/`
/// (12 461 lines), with the smali beside it under
/// `~/android/docs/xyauto-mcu/hardware/evidence/`. `bridge/main.go:2145` was
/// the first-pass oracle and is a **subset**: it dispatches `12 03/04/0d`, some
/// `05` values and the `0x10` family, and drops the rest. Reading the stock
/// source turned three "unknown" frames in the D3-0043 capture into headlamp
/// states and a clock report.
///
/// Two kinds of "we do not know" are kept apart on purpose:
///
/// * [`Event::DeviceStateUnhandled`] / [`Event::PowerKeyUnhandled`] — the
///   stock service *receives* this and has no branch for it. We know what the
///   byte is; nobody knows what it means.
/// * [`Event::Unknown`] — a command byte outside every table here.
///
/// Collapsing those two would lose the distinction between "the OEM ignores
/// this too" and "we have not looked yet".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// `12 04` — ignition on. The MCU sends it **three times**; the consumer
    /// debounces (D15 §10.5).
    AccOn,
    /// `12 03` — ignition off.
    AccOff,
    /// `12 0d` — wake signal after a confirmed off. Correlated with the PMIC
    /// KEY_POWER event; **never** treat the announcement frame alone as the
    /// ACC-on confirmation across a resume (D15 §4.5).
    AccWake,
    /// `12 01` — headlamps off. `onDevice()` case 1 posts `HeadlampOffrunnable`
    /// on a delay.
    HeadlampOff,
    /// `12 02` — headlamps on. `onDevice()` case 2 posts `HeadlampOnrunnable`
    /// immediately.
    HeadlampOn,
    /// `12 05` — brake released. `onDevice()` case 5 clears `Settings.System
    /// "Brake"` and broadcasts `ACTION_BRAKE_LEAVE`.
    BrakeReleased,
    /// `12 06` — brake applied. `onDevice()` case 6.
    BrakeApplied,
    /// A `12 <state>` the stock service receives and **does nothing with**.
    ///
    /// `onDevice()` has cases 1..=6 plus a `state == 13` branch, an explicit
    /// `state != 16 && state != 17` guard, and **no `default:`** — read
    /// directly, `NewkeypadMonitor.java:10564-10730`. So a state outside that
    /// set is wrapped by `doforDevInfo`, handed to `onResult` → `onDevice`,
    /// and dropped.
    ///
    /// Device 3 sends `12 07` at boot. It is not noise and it is not an error;
    /// it is a state this MCU reports that this Android build never had a
    /// meaning for. Surfacing it is the point (§2.3d of the skeleton doc).
    DeviceStateUnhandled(u8),
    /// `21 00` — the MCU asks Android for the time. Stock answers with
    /// `answerTimetoMCU()` (`NewkeypadMonitor.java:9204-9210`).
    ///
    /// **This daemon does not answer it**: clock policy belongs to the HU
    /// service app and the transmit allowlist contains only ACC lifecycle
    /// frames (D18).
    TimeRequest,
    /// `21 01` + seven BCD bytes — the MCU reporting *its* clock.
    ///
    /// Field order and the BCD from `doforTimeInfo`
    /// (`NewkeypadMonitor.java:8535-8556`), which is called with `pos` at the
    /// `01` subcommand byte and then reads `pos+1 .. pos+7`:
    /// second, minute, hour, day, month, year-hundreds, year-units — with
    /// `year = bcd(pos+7) + bcd(pos+6) * 100`.
    TimeReport {
        /// 20xx, already combined from the two BCD bytes.
        year: u16,
        /// 1-12.
        month: u8,
        /// 1-31.
        day: u8,
        /// 0-23.
        hour: u8,
        /// 0-59.
        minute: u8,
        /// 0-59.
        second: u8,
    },
    /// `05 <sub>` — the power-key/control family, `doforPowkeyCtrl`
    /// (`NewkeypadMonitor.java:8212-8268`).
    ///
    /// Stock maps nine specific sub-bytes to an abstract key and lets every
    /// other value fall through with `tmpcmd` still zeroed — i.e. it emits
    /// "key 0", which no handler acts on. So a `05` frame is only meaningful
    /// when [`power_key`] returns something; see
    /// [`Event::PowerKeyUnhandled`].
    PowerKey {
        /// The sub-command byte.
        sub: u8,
        /// The abstract key stock translates it into.
        /// [`POWER_KEY_SHUTDOWN`] (79) is the ACC-off trigger.
        abstract_key: u8,
    },
    /// A `05 <sub>` with no branch in `doforPowkeyCtrl`.
    ///
    /// **`05 11` is one of these**, and naming it matters: this crate used to
    /// call it `AccHeartbeat`. That was wrong — see §2.3d. The stock handler
    /// has no branch for 17, so it produces key 0 and nothing happens. It *is*
    /// emitted about every three seconds while ACC is on, but "periodic" and
    /// "heartbeat the SoC must answer" are different claims and only the first
    /// is evidenced.
    PowerKeyUnhandled(u8),
    /// `33` + six bytes — version reply. `sw` is LE16 of body[0..2], `hw` LE32
    /// of body[2..6] (`diagnostic-init.c:2331 mcu_decode_version`).
    Version {
        /// `xy_mcu_sw_ver`.
        sw: u16,
        /// `xy_mcu_hw_ver`.
        hw: u32,
    },
    /// `10 <state> <code>` — a button.
    Button {
        /// The button's code byte.
        code: u8,
        /// Press or release.
        state: ButtonState,
        /// What it is, or `"unmapped"`.
        name: &'static str,
    },
    /// `10 <state> <code> <magnitude>` — a rotary detent. `magnitude` is
    /// stock's *accelerated movement* count, not a quadrature phase: a slow
    /// detent reports 01, fast turns report 02/03/07/09/0f. Apply each packet
    /// exactly once.
    Knob {
        /// The detent's code byte.
        code: u8,
        /// How far.
        magnitude: u8,
        /// What it is.
        name: &'static str,
    },
    /// A well-formed frame this table does not decode.
    Unknown,
}

/// Device-1 protocol-oracle button table: code → name.
///
/// These names decode copied mcu-toolbox fixtures; they are not a device-3
/// input mapping. Device 3's five codes remain unknown until Capture B, and
/// [`uinput::DEVICE3_BUTTONS`] therefore contains no raw codes.
/// Source: `bridge/main.go:2199-2244`.
pub const BUTTONS: &[(u8, &str)] = &[
    (0x01, "left-knob-push/wakeup"),
    (0x2a, "right-knob-push/back"),
    (0x04, "radio"),
    (0x15, "media"),
    (0x05, "mute"),
    (0x19, "tone"),
    (0x08, "nav"),
    (0x0f, "phone"),
    (0x07, "mode"),
    (0x2d, "setup"),
];

/// Device-1 protocol-oracle rotary table. Device 3 has no physical knobs.
/// Source: `bridge/main.go:2183-2192`.
pub const KNOBS: &[(u8, &str)] = &[
    (0x0d, "left-knob-left/volume-down"),
    (0x0c, "left-knob-right/volume-up"),
    (0x03, "right-knob-left/dpad-left"),
    (0x02, "right-knob-right/dpad-right"),
];

fn lookup(table: &[(u8, &'static str)], code: u8) -> Option<&'static str> {
    table.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

/// The `05` sub-command → abstract-key table, `doforPowkeyCtrl`
/// (`NewkeypadMonitor.java:8215-8250`).
///
/// Nine values and no more. Every other sub-byte falls out of the if/else
/// chain with `tmpcmd` still zeroed, so stock emits `key: 0` and no handler
/// acts on it — which is why [`Event::PowerKeyUnhandled`] exists rather than a
/// catch-all `PowerKey { abstract_key: 0 }`.
pub const POWER_KEYS: &[(u8, u8)] = &[
    (0x0d, 51),
    (0x0e, 66),
    (0x10, POWER_KEY_SHUTDOWN),
    (0x12, 56),
    (0x13, 55),
    (0x14, 57),
    (0x15, 64),
    (0x16, 65),
    (0xa4, 78),
];

/// Abstract key 79 (`0x4f`), which `05 10` maps to.
///
/// This is the ACC-off trigger: `NewkeypadMonitor.java:9532` `if (key2 == 79)`
/// broadcasts `action_disconnected_bt`, sets `hasaccoff`, turns on airplane
/// mode and unmounts the udisk; the shutdown runnable (`:970-1007`) then calls
/// `accpoweroff()` and `sendACCOFFtoMCU()`.
///
/// So the ACC-off sequence has **two** stages and they are not the same frame:
/// `12 03` starts the MCU's grace period, and `05 10` some 25 s later is the
/// actual shutdown. Confusing them costs the grace period.
pub const POWER_KEY_SHUTDOWN: u8 = 79;

/// The abstract key stock maps a `05` sub-command to, if any.
pub fn power_key(sub: u8) -> Option<u8> {
    POWER_KEYS.iter().find(|(s, _)| *s == sub).map(|(_, k)| *k)
}

/// `bcd_to_dec` from `NewkeypadMonitor.java:8517`:
/// `((bcd >> 4) & 15) * 10 + (bcd & 15)`.
///
/// Not validated against 0x0a..0x0f nibbles — stock does not validate either,
/// and inventing a stricter rule here would make this decoder disagree with the
/// device for inputs the device can actually produce.
fn bcd_to_dec(bcd: u8) -> u8 {
    ((bcd >> 4) & 0x0f) * 10 + (bcd & 0x0f)
}

impl Event {
    /// Decode a payload (CMD first, checksum already stripped).
    pub fn decode(payload: &[u8]) -> Event {
        match payload {
            // 0x12 -> doforDevInfo -> onResult -> onDevice(state).
            // NewkeypadMonitor.java:9198, :8293, :10564.
            [0x12, s, ..] => match s {
                0x01 => Event::HeadlampOff,
                0x02 => Event::HeadlampOn,
                0x03 => Event::AccOff,
                0x04 => Event::AccOn,
                0x05 => Event::BrakeReleased,
                0x06 => Event::BrakeApplied,
                0x0d => Event::AccWake,
                other => Event::DeviceStateUnhandled(*other),
            },
            // 0x21 -> the time pair. NewkeypadMonitor.java:9204-9210.
            [0x21, 0x00, ..] => Event::TimeRequest,
            [0x21, 0x01, b @ ..] if b.len() >= 7 => Event::TimeReport {
                second: bcd_to_dec(b[0]),
                minute: bcd_to_dec(b[1]),
                hour: bcd_to_dec(b[2]),
                day: bcd_to_dec(b[3]),
                month: bcd_to_dec(b[4]),
                year: bcd_to_dec(b[6]) as u16 + bcd_to_dec(b[5]) as u16 * 100,
            },
            // 0x05 -> doforPowkeyCtrl. NewkeypadMonitor.java:8212.
            [0x05, s, ..] => match power_key(*s) {
                Some(abstract_key) => Event::PowerKey {
                    sub: *s,
                    abstract_key,
                },
                None => Event::PowerKeyUnhandled(*s),
            },
            [0x33, b @ ..] if b.len() >= 6 => Event::Version {
                sw: u16::from_le_bytes([b[0], b[1]]),
                hw: u32::from_le_bytes([b[2], b[3], b[4], b[5]]),
            },
            // A 0x10 payload of four or more bytes is a rotary detent; of
            // exactly three, a button. That split is the stock service's, not
            // ours (bridge/main.go:2176).
            [0x10, _state, code, magnitude, ..] => match lookup(KNOBS, *code) {
                Some(name) => Event::Knob {
                    code: *code,
                    magnitude: *magnitude,
                    name,
                },
                None => Event::Unknown,
            },
            [0x10, state, code] => Event::Button {
                code: *code,
                state: match state {
                    0x01 => ButtonState::Press,
                    0x02 => ButtonState::Release,
                    other => ButtonState::Other(*other),
                },
                name: lookup(BUTTONS, *code).unwrap_or("unmapped"),
            },
            _ => Event::Unknown,
        }
    }
}

// ------------------------------------------------------- the transmit gates

/// A frame the daemon is allowed to transmit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllowedFrame {
    /// What it does.
    pub name: &'static str,
    /// The exact bytes. Nothing else may be sent, ever.
    pub bytes: &'static [u8],
}

/// Fixed stock ACC-off completion (`sendACCOFFtoMCU`).
pub const TX_ACC_OFF_COMPLETE: &[u8] = &[0x5a, 0xa5, 0x03, 0x05, 0x15, 0xe6];

/// Fixed stock ACC-on answer (`sendACCONANStoMCU`).
pub const TX_ACC_ON_ANSWER: &[u8] = &[0x5a, 0xa5, 0x03, 0x05, 0x17, 0xe4];

/// Fixed stock version query that wakes the otherwise-silent MCU peer.
pub const TX_VERSION_QUERY: &[u8] = &[0x5a, 0xa5, 0x02, 0x23, 0xdd];

/// Fixed stock GPS-ready notification (`sendGPSReadytoMCU`).
/// OEM device-3 NewkeypadMonitor sends this during boot/wake initialization;
/// its electrical effect and any MCU acknowledgement are not established.
pub const TX_GPS_READY: &[u8] = &[0x5a, 0xa5, 0x02, 0x64, 0x9c];

/// The complete fixed transmit allowlist for the sole MCU owner.
///
/// Adding a frame here is one reviewable change that must also move
/// `tools/frame-gate.py`'s allowlist;
/// [`tests::only_the_four_fixed_frames_are_allowed`] is the forcing
/// function. The version query is required because the MCU is silent until
/// queried; it is sent once when the daemon takes ownership of SE0. The two
/// Phase-4d frames, when they come
/// (`radio.go:184-189`, asserted in `radio_test.go:29-36`):
///
/// ```text
/// radio on   5a a5 03 05 03 f8
/// radio off  5a a5 03 05 04 f7
/// ```
pub const ALLOW: &[AllowedFrame] = &[
    AllowedFrame {
        name: "GPS-ready notification",
        bytes: TX_GPS_READY,
    },
    AllowedFrame {
        name: "version query",
        bytes: TX_VERSION_QUERY,
    },
    AllowedFrame {
        name: "ACC-off completion",
        bytes: TX_ACC_OFF_COMPLETE,
    },
    AllowedFrame {
        name: "ACC-on answer",
        bytes: TX_ACC_ON_ANSWER,
    },
];

/// Payloads that must never be transmitted, each with the reason.
///
/// `mem:xyauto-mcu-uart-acc`, D15 §7, and the exact-seller command catalogue at
/// `~/android/archive/reference/xyauto-mcu-info/SM4250-MCU-DOSSIER.md:189-228`.
/// These are *command* bytes, not whole frames: a denylist of whole frames is
/// defeated by one byte of padding.
pub const DENY: &[(u8, u8, &str)] = &[
    (
        0x05,
        0x16,
        "the sleep-authorising payload; never send 05 16",
    ),
    (0x05, 0x06, "the ARM power-off payload; never send 05 06"),
    (
        0x05,
        0xa2,
        "the unproven amplifier-on payload; never send 05 A2",
    ),
    (
        0x05,
        0xa3,
        "the unproven amplifier-off payload; never send 05 A3",
    ),
    (0x05, 0xa4, "the sleep-status payload; never send 05 A4"),
    (0x05, 0x0c, "the ARM reboot payload; never send 05 0C"),
    (
        0x07,
        0x00,
        "the CVBS/reverse-camera route family; never send 07 rr",
    ),
    (
        0x04,
        0x06,
        "the persistent delayed-sleep setting; never send 04 06",
    ),
    (
        0x04,
        0x07,
        "the persistent low-voltage cutoff; never send 04 07",
    ),
    (0x3f, 0x01, "the device-identity write; never send 3F 01"),
    (0x79, 0x00, "the provisioning-code family; never send 79"),
    (
        0x84,
        0x00,
        "the delayed-power-off guard hook; never send 84",
    ),
    (0x43, 0x00, "IAP firmware-update entry"),
    (0x44, 0x00, "IAP firmware-update data"),
    (0x46, 0x00, "IAP firmware-update verify"),
    (0x4a, 0x00, "IAP firmware-update finish"),
    (0x4b, 0x00, "IAP firmware-update control"),
    (0x50, 0x00, "IAP firmware-update control"),
];

/// Why a frame was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameRefused {
    /// Not on the allowlist.
    NotAllowed,
    /// On the denylist, with the reason.
    Denied(&'static str),
    /// Does not start with [`PREAMBLE`].
    NotAFrame,
}

impl fmt::Display for FrameRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameRefused::NotAllowed => write!(f, "not on the transmit allowlist"),
            FrameRefused::Denied(why) => write!(f, "denied: {}", why),
            FrameRefused::NotAFrame => write!(f, "not an MCU frame (bad preamble)"),
        }
    }
}

/// The three gates, in order: preamble, denylist, allowlist byte-compare.
///
/// Denylist before allowlist on purpose: they should never overlap, and if they
/// ever do the denylist must win and the reason must reach the log, because
/// that overlap is a bug in the allowlist and silently permitting it is how the
/// one unrecoverable mistake happens.
pub fn may_transmit(frame: &[u8]) -> Result<&'static AllowedFrame, FrameRefused> {
    if frame.len() < 4 || frame[0] != H0 || frame[1] != H1 {
        return Err(FrameRefused::NotAFrame);
    }
    let cmd = frame[3];
    let sub = frame.get(4).copied().unwrap_or(0);
    for (deny_cmd, deny_sub, why) in DENY {
        if cmd == *deny_cmd && (*deny_sub == 0 || sub == *deny_sub) {
            return Err(FrameRefused::Denied(why));
        }
    }
    ALLOW
        .iter()
        .find(|a| a.bytes == frame)
        .ok_or(FrameRefused::NotAllowed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct ScratchSysfs(PathBuf);

    impl ScratchSysfs {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "bengal-hud-tty-{}-{}",
                label,
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchSysfs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn add_fake_tty(root: &Path, tty: &str, device_component: &str) {
        let tty_dir = root.join("class/tty").join(tty);
        fs::create_dir_all(&tty_dir).unwrap();
        let target = root.join("devices/platform/soc@0").join(device_component);
        fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, tty_dir.join("device")).unwrap();
    }

    /// The complete D3-0043 receive stream, reconstructed from PID 1's own log.
    ///
    /// `result.log:112-122` lists eleven frames and `:111` says
    /// "rx 78 bytes in 21 ticks". 11 + 13 + 9×6 = **78** — the frames are the
    /// whole capture with nothing discarded between them, which is also what
    /// `:123` reports: "frames valid 11 bad 0".
    ///
    /// These are real bytes off real hardware. Everything below replays them.
    const D3_0043: &[u8] = &[
        0x5a, 0xa5, 0x08, 0x33, 0x00, 0x22, 0x00, 0x7d, 0xfe, 0x09, 0x27, 0x5a, 0xa5, 0x0a, 0x21,
        0x01, 0x25, 0x00, 0x08, 0x01, 0x01, 0x20, 0x00, 0x8f, 0x5a, 0xa5, 0x03, 0x12, 0x04, 0xea,
        0x5a, 0xa5, 0x03, 0x12, 0x04, 0xea, 0x5a, 0xa5, 0x03, 0x12, 0x01, 0xed, 0x5a, 0xa5, 0x03,
        0x12, 0x04, 0xea, 0x5a, 0xa5, 0x03, 0x12, 0x07, 0xe7, 0x5a, 0xa5, 0x03, 0x12, 0x01, 0xed,
        0x5a, 0xa5, 0x03, 0x12, 0x07, 0xe7, 0x5a, 0xa5, 0x03, 0x12, 0x01, 0xed, 0x5a, 0xa5, 0x03,
        0x05, 0x11, 0xea,
    ];

    /// The agreement contract with PID 1's decoder, regenerated by
    /// `tools/decoder-agreement.sh` from `kernel/initramfs/diagnostic-init.c`.
    ///
    /// If that file's parser ever changes, the script rewrites this golden and
    /// `git diff --exit-code` fails; if THIS parser changes, the test below
    /// fails against the unchanged golden. Either way the two cannot drift
    /// apart quietly.
    const GOLDEN: &str = include_str!("../../tools/decoder-agreement.golden");

    fn canonical(items: &[Item]) -> String {
        let mut s = String::new();
        for item in items {
            match item {
                Item::Frame(f) => s.push_str(&format!("frame ok  {}\n", f)),
                Item::BadChecksum(f) => s.push_str(&format!("frame bad {}\n", f)),
                Item::Discarded { count, reason } => {
                    s.push_str(&format!("discard {} {:?}\n", count, reason))
                }
            }
        }
        s
    }

    #[test]
    fn the_real_capture_decodes_exactly_as_pid_1_reported_it() {
        let mut p = Parser::new();
        let items = p.push(D3_0043);
        // PID 1: "frames valid 11 bad 0".
        assert_eq!(p.stats().valid, 11);
        assert_eq!(p.stats().bad, 0);
        assert_eq!(p.stats().discarded, 0, "the capture is frames end to end");
        assert_eq!(items.len(), 11);
        assert_eq!(p.pending_len(), 0);

        // ...and the eleven frames are the eleven lines of result.log:112-122.
        let hex: Vec<String> = items
            .iter()
            .map(|i| match i {
                Item::Frame(f) => f.to_string(),
                other => panic!("not a valid frame: {:?}", other),
            })
            .collect();
        assert_eq!(hex[0], "5a a5 08 33 00 22 00 7d fe 09 27");
        assert_eq!(hex[1], "5a a5 0a 21 01 25 00 08 01 01 20 00 8f");
        assert_eq!(hex[2], "5a a5 03 12 04 ea");
        assert_eq!(hex[10], "5a a5 03 05 11 ea");
    }

    #[test]
    fn acc_on_is_seen_in_the_real_capture() {
        // result.log:123 -- "12 04 ACC-ON seen".
        let mut p = Parser::new();
        let events: Vec<Event> = p
            .push(D3_0043)
            .iter()
            .filter_map(|i| match i {
                Item::Frame(f) => Some(f.event()),
                _ => None,
            })
            .collect();
        assert_eq!(
            events.iter().filter(|e| **e == Event::AccOn).count(),
            3,
            "the MCU announces ACC-on three times -- D15 §10.5 says debounce"
        );
        // The last frame is 05 11, and it is NOT a heartbeat -- see
        // the_whole_capture_decoded_through_the_stock_table below.
        assert!(events.contains(&Event::PowerKeyUnhandled(0x11)));
    }

    /// Every frame of the real capture, read through the stock service's own
    /// tables. This is the answer to "what did device 3 say at boot".
    #[test]
    fn the_whole_capture_decoded_through_the_stock_table() {
        let mut p = Parser::new();
        let events: Vec<Event> = p
            .push(D3_0043)
            .iter()
            .filter_map(|i| match i {
                Item::Frame(f) => Some(f.event()),
                _ => None,
            })
            .collect();

        assert_eq!(
            events,
            vec![
                // MCU firmware version, answering PID 1's 5a a5 02 23 dd query
                Event::Version {
                    sw: 0x2200,
                    hw: 0x09fe_7d00
                },
                // the MCU volunteering its own clock: 2000-01-01 08:00:25,
                // i.e. an RTC that has never been set
                Event::TimeReport {
                    year: 2000,
                    month: 1,
                    day: 1,
                    hour: 8,
                    minute: 0,
                    second: 25
                },
                Event::AccOn,       // 12 04
                Event::AccOn,       // 12 04  (three times; debounce)
                Event::HeadlampOff, // 12 01  <- NOT "unknown"
                Event::AccOn,       // 12 04
                Event::DeviceStateUnhandled(0x07),
                Event::HeadlampOff, // 12 01
                Event::DeviceStateUnhandled(0x07),
                Event::HeadlampOff, // 12 01
                Event::PowerKeyUnhandled(0x11),
            ]
        );
    }

    #[test]
    fn twelve_oh_one_is_headlamps_off_not_unknown() {
        // The correction this branch exists for. 12 01 was Event::Unknown; the
        // stock service's onDevice() case 1 posts HeadlampOffrunnable
        // (NewkeypadMonitor.java:10581). Device 3 sends it three times at boot
        // -- lights off, which on a bench unit is exactly right.
        assert_eq!(Event::decode(&[0x12, 0x01]), Event::HeadlampOff);
        assert_eq!(Event::decode(&[0x12, 0x02]), Event::HeadlampOn);
    }

    #[test]
    fn twelve_oh_seven_has_no_stock_handler_and_says_so() {
        // onDevice() has cases 1..=6, a state==13 branch, an explicit
        // `state != 16 && state != 17` guard, and NO default -- read directly
        // at NewkeypadMonitor.java:10564-10730. So 07 is received and dropped.
        // We surface it instead of dropping it, but we do not invent a meaning.
        assert_eq!(
            Event::decode(&[0x12, 0x07]),
            Event::DeviceStateUnhandled(0x07)
        );
        // the two the guard excludes explicitly are the same class
        assert_eq!(
            Event::decode(&[0x12, 0x10]),
            Event::DeviceStateUnhandled(0x10)
        );
        assert_eq!(
            Event::decode(&[0x12, 0x11]),
            Event::DeviceStateUnhandled(0x11)
        );
    }

    #[test]
    fn brake_states_come_from_on_device_cases_5_and_6() {
        assert_eq!(Event::decode(&[0x12, 0x05]), Event::BrakeReleased);
        assert_eq!(Event::decode(&[0x12, 0x06]), Event::BrakeApplied);
    }

    #[test]
    fn the_time_pair_decodes_both_directions() {
        // 21 00 -> stock calls answerTimetoMCU(); we only classify it, because
        // answering is a transmit and the allowlist contains no time reply.
        assert_eq!(Event::decode(&[0x21, 0x00]), Event::TimeRequest);

        // 21 01 + 7 BCD bytes, exactly the frame in the capture.
        assert_eq!(
            Event::decode(&[0x21, 0x01, 0x25, 0x00, 0x08, 0x01, 0x01, 0x20, 0x00]),
            Event::TimeReport {
                year: 2000,
                month: 1,
                day: 1,
                hour: 8,
                minute: 0,
                second: 25
            }
        );

        // a plausible real clock, to prove the BCD is BCD and not hex
        assert_eq!(
            Event::decode(&[0x21, 0x01, 0x59, 0x45, 0x23, 0x31, 0x12, 0x20, 0x26]),
            Event::TimeReport {
                year: 2026,
                month: 12,
                day: 31,
                hour: 23,
                minute: 45,
                second: 59
            }
        );

        // truncated: not enough BCD bytes to be a time
        assert_eq!(Event::decode(&[0x21, 0x01, 0x25]), Event::Unknown);
    }

    #[test]
    fn the_power_key_table_is_the_nine_values_stock_branches_on() {
        // doforPowkeyCtrl, NewkeypadMonitor.java:8215-8250.
        for (sub, key) in [
            (0x0du8, 51u8),
            (0x0e, 66),
            (0x10, 79),
            (0x12, 56),
            (0x13, 55),
            (0x14, 57),
            (0x15, 64),
            (0x16, 65),
            (0xa4, 78),
        ] {
            assert_eq!(
                Event::decode(&[0x05, sub]),
                Event::PowerKey {
                    sub,
                    abstract_key: key
                },
                "05 {:#04x}",
                sub
            );
        }
        assert_eq!(POWER_KEYS.len(), 9, "stock branches on exactly nine values");
    }

    #[test]
    fn oh_five_eleven_is_not_a_heartbeat() {
        // THE CORRECTION. This crate called 05 11 AccHeartbeat. There is no
        // branch for 17 in doforPowkeyCtrl, so stock emits key 0 and no
        // handler acts on it. It IS periodic (~3 s while ACC is on, live
        // capture 2026-08-26), but periodic and "a heartbeat the SoC must
        // answer" are different claims and only the first has evidence.
        assert_eq!(power_key(0x11), None);
        assert_eq!(Event::decode(&[0x05, 0x11]), Event::PowerKeyUnhandled(0x11));
    }

    #[test]
    fn oh_five_ten_carries_the_shutdown_key() {
        // 05 10 -> abstract key 79 -> NewkeypadMonitor.java:9532's ACC-off
        // path. It is NOT the same event as 12 03: stock sees 12 03 first and
        // 05 10 about 25 s later (live capture 2026-08-26).
        assert_eq!(
            Event::decode(&[0x05, 0x10]),
            Event::PowerKey {
                sub: 0x10,
                abstract_key: POWER_KEY_SHUTDOWN
            }
        );
        assert_eq!(POWER_KEY_SHUTDOWN, 79);
        assert_ne!(Event::decode(&[0x05, 0x10]), Event::decode(&[0x12, 0x03]));
    }

    #[test]
    fn oh_five_sixteen_is_a_receive_side_key_and_still_never_transmittable() {
        // 05 16 maps to abstract key 65 on RX -- a perfectly ordinary key.
        // That must not soften the TX rule: sending it authorises sleep.
        assert_eq!(
            Event::decode(&[0x05, 0x16]),
            Event::PowerKey {
                sub: 0x16,
                abstract_key: 65
            }
        );
        assert!(matches!(
            may_transmit(&[0x5a, 0xa5, 0x03, 0x05, 0x16, 0xe5]),
            Err(FrameRefused::Denied(_))
        ));
    }

    #[test]
    fn the_version_reply_decodes_to_the_documented_fields() {
        // 5a a5 08 33 00 22 00 7d fe 09 27 -> sw = LE16(00 22), hw = LE32(00 7d fe 09)
        // Device 1 answered ...33 00 23 00 7D FE 09 26 -> sw=0x2300 hw=0x09fe7d00
        // (diagnostic-init.c:2331). Device 3 is one lower in the sw field.
        let mut p = Parser::new();
        let items = p.push(&D3_0043[..11]);
        match items[0] {
            Item::Frame(ref f) => assert_eq!(
                f.event(),
                Event::Version {
                    sw: 0x2200,
                    hw: 0x09fe_7d00
                }
            ),
            ref other => panic!("{:?}", other),
        }
    }

    #[test]
    fn the_bridge_dispatch_would_have_dropped_half_this_capture() {
        // WAS "device_3_emits_two_subcommands_the_bridge_table_ignores", which
        // asserted 12 01 and 12 07 decode to Unknown. That was true of THIS
        // crate before the stock service was read, not of the protocol.
        //
        // bridge/main.go:2145 dispatches only 12 03/04/0d, 05 10, 05 01..06,
        // 05 11 and the 0x10 family. Six of the eleven frames in the real
        // capture fall outside it -- the version reply, the time report, three
        // headlamp states and... nothing else, because 12 07 has no stock
        // handler either. The bridge is not wrong; it is a *subset*, and this
        // test pins how large the gap is so it cannot widen unnoticed.
        let bridge_would_dispatch = |e: &Event| {
            matches!(
                e,
                Event::AccOn
                    | Event::AccOff
                    | Event::AccWake
                    | Event::Button { .. }
                    | Event::Knob { .. }
            )
        };
        let mut p = Parser::new();
        let events: Vec<Event> = p
            .push(D3_0043)
            .iter()
            .filter_map(|i| match i {
                Item::Frame(f) => Some(f.event()),
                _ => None,
            })
            .collect();
        assert_eq!(
            events.iter().filter(|e| bridge_would_dispatch(e)).count(),
            3
        );
        assert_eq!(
            events.iter().filter(|e| !bridge_would_dispatch(e)).count(),
            8
        );
        // ...and of those eight, exactly two carry no meaning even in stock.
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Event::DeviceStateUnhandled(_)))
                .count(),
            2
        );
    }

    #[test]
    fn a_frame_split_across_reads_is_reassembled() {
        // The normal case on a tty, not an edge case. Split the capture at
        // every single byte boundary and require the same eleven frames.
        for cut in 1..D3_0043.len() {
            let mut p = Parser::new();
            let mut items = p.push(&D3_0043[..cut]);
            items.extend(p.push(&D3_0043[cut..]));
            assert_eq!(
                p.stats().valid,
                11,
                "split at {} lost a frame: {:?}",
                cut,
                p.stats()
            );
            assert_eq!(p.stats().bad, 0, "split at {}", cut);
            assert_eq!(p.stats().discarded, 0, "split at {}", cut);
            assert_eq!(items.len(), 11);
        }
    }

    #[test]
    fn a_preamble_split_across_reads_survives() {
        // The specific case the "keep a trailing lone 5a" rule exists for:
        // 5a arrives, a5 does not.
        let mut p = Parser::new();
        assert!(p.push(&[0x5a]).is_empty());
        assert_eq!(p.pending_len(), 1, "the lone 5a must be KEPT");
        let items = p.push(&[0xa5, 0x03, 0x12, 0x04, 0xea]);
        assert_eq!(items.len(), 1);
        assert_eq!(p.stats().valid, 1);
        assert_eq!(p.stats().discarded, 0);
    }

    #[test]
    fn a_corrupted_checksum_is_counted_not_dropped() {
        let mut bad = D3_0043.to_vec();
        // last byte of the third frame: 5a a5 03 12 04 ea -> ..eb
        bad[29] ^= 0x01;
        let mut p = Parser::new();
        let items = p.push(&bad);
        assert_eq!(p.stats().valid, 10);
        assert_eq!(p.stats().bad, 1, "PID 1 reports bad frames, so do we");
        assert_eq!(items.len(), 11, "the frame is still delivered, marked bad");
        match &items[2] {
            Item::BadChecksum(f) => {
                assert_eq!(f.command(), 0x12);
                // and it must NOT be mistaken for an event
                assert_eq!(f.event(), Event::AccOn, "the payload still decodes");
                assert!(!f.checksum_ok());
            }
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn garbage_before_a_frame_is_discarded_and_the_frame_still_arrives() {
        let mut p = Parser::new();
        let mut input = vec![0x00, 0xff, 0x5a, 0x01, 0xde, 0xad];
        input.extend_from_slice(&[0x5a, 0xa5, 0x03, 0x12, 0x04, 0xea]);
        let items = p.push(&input);
        assert_eq!(p.stats().valid, 1);
        assert_eq!(p.stats().discarded, 6);
        assert!(matches!(
            items[0],
            Item::Discarded {
                count: 6,
                reason: Discard::BeforeHeader
            }
        ));
    }

    #[test]
    fn an_impossible_length_costs_one_byte_and_the_hunt_resumes() {
        // 5a a5 00 is not a header: total would be 3, below FRAME_MIN. The
        // rule is drop ONE byte -- dropping three would eat a real preamble
        // that happened to follow.
        let mut p = Parser::new();
        let mut input = vec![0x5a, 0xa5, 0x00];
        input.extend_from_slice(&[0x5a, 0xa5, 0x03, 0x12, 0x04, 0xea]);
        let items = p.push(&input);
        assert!(matches!(
            items[0],
            Item::Discarded {
                count: 1,
                reason: Discard::ImpossibleLength(0x00)
            }
        ));
        assert_eq!(p.stats().valid, 1);
    }

    #[test]
    fn the_checksum_matches_both_oracles_on_every_captured_frame() {
        // bridge/main.go:2365 and diagnostic-init.c:2251 compute
        // (0 - sum(payload)) & 0xff over CMD..last-1. Recompute it here for
        // every frame in the capture rather than trusting the parser.
        let mut p = Parser::new();
        for item in p.push(D3_0043) {
            if let Item::Frame(f) = item {
                let raw = f.raw();
                let sum = raw[3..raw.len() - 1]
                    .iter()
                    .fold(0u8, |a, b| a.wrapping_add(*b));
                assert_eq!(*raw.last().unwrap(), (!sum).wrapping_add(1), "{}", f);
            }
        }
    }

    #[test]
    fn declared_length_counts_command_through_checksum() {
        // LEN = 1 (CMD) + payload + 1 (CS); total = LEN + 3.
        let mut p = Parser::new();
        for item in p.push(D3_0043) {
            if let Item::Frame(f) = item {
                let raw = f.raw();
                assert_eq!(raw.len(), raw[2] as usize + 3, "{}", f);
                assert_eq!(f.payload().len(), raw[2] as usize - 1, "{}", f);
            }
        }
    }

    #[test]
    fn buttons_and_knobs_split_on_payload_length() {
        // Three bytes after 0x10 is a rotary detent, two is a button. That
        // split is the stock service's (bridge/main.go:2176).
        assert_eq!(
            Event::decode(&[0x10, 0x01, 0x0d, 0x01]),
            Event::Knob {
                code: 0x0d,
                magnitude: 1,
                name: "left-knob-left/volume-down"
            }
        );
        assert_eq!(
            Event::decode(&[0x10, 0x01, 0x2a]),
            Event::Button {
                code: 0x2a,
                state: ButtonState::Press,
                name: "right-knob-push/back"
            }
        );
        assert_eq!(
            Event::decode(&[0x10, 0x02, 0x2a]),
            Event::Button {
                code: 0x2a,
                state: ButtonState::Release,
                name: "right-knob-push/back"
            }
        );
        // an unmapped button is named, not dropped
        match Event::decode(&[0x10, 0x01, 0xee]) {
            Event::Button { name, .. } => assert_eq!(name, "unmapped"),
            other => panic!("{:?}", other),
        }
        // a 0x10 rotary code that is not a detent decodes to Unknown, matching
        // the bridge's early return
        assert_eq!(Event::decode(&[0x10, 0x01, 0xee, 0x01]), Event::Unknown);
    }

    #[test]
    fn magnitude_is_carried_through_unchanged() {
        // Stock's doforKeyInfo() reads it as accelerated movement: 01 slow,
        // 02/03/07/09/0f on fast turns. Apply each packet once; never treat it
        // as a quadrature phase.
        for m in [0x01u8, 0x02, 0x03, 0x07, 0x09, 0x0f] {
            match Event::decode(&[0x10, 0x01, 0x0c, m]) {
                Event::Knob { magnitude, .. } => assert_eq!(magnitude, m),
                other => panic!("{:?}", other),
            }
        }
    }

    #[test]
    fn the_pending_buffer_is_bounded() {
        let mut p = Parser::new();
        // 5a repeated is the worst case: every byte could start a preamble,
        // so the "keep a trailing 5a" rule must not turn into "keep them all".
        let items = p.push(&vec![0x5a; MAX_PENDING + 10]);
        assert!(
            p.pending_len() <= MAX_PENDING,
            "pending {}",
            p.pending_len()
        );
        assert!(items.iter().any(|i| matches!(i, Item::Discarded { .. })));
    }

    #[test]
    fn agrees_with_pid_1_on_the_golden_captures() {
        // The generated contract. tools/decoder-agreement.sh lifts
        // mcu_checksum/mcu_next_frame/mcu_frame_ok out of
        // kernel/initramfs/diagnostic-init.c, compiles them, runs them over
        // the same inputs and writes this file. If the two decoders ever
        // disagree, one of the two sides of this assert moves.
        let mut out = String::new();
        for (name, bytes) in golden_inputs() {
            out.push_str(&format!("# {}\n", name));
            let mut p = Parser::new();
            out.push_str(&canonical(&p.push(&bytes)));
            out.push_str(&format!(
                "stats valid={} bad={} discarded={} pending={}\n",
                p.stats().valid,
                p.stats().bad,
                p.stats().discarded,
                p.pending_len()
            ));
        }
        assert_eq!(
            out.trim_end(),
            GOLDEN.trim_end(),
            "this parser and PID 1's disagree; run tools/decoder-agreement.sh"
        );
    }

    /// The inputs both decoders are run over. Keep in step with the same list
    /// in `tools/decoder-agreement.sh`.
    fn golden_inputs() -> Vec<(&'static str, Vec<u8>)> {
        let mut corrupt = D3_0043.to_vec();
        corrupt[29] ^= 0x01;
        vec![
            ("d3-0043-capture", D3_0043.to_vec()),
            ("corrupt-checksum", corrupt),
            (
                "garbage-then-frame",
                vec![
                    0x00, 0xff, 0x5a, 0x01, 0xde, 0xad, 0x5a, 0xa5, 0x03, 0x12, 0x04, 0xea,
                ],
            ),
            (
                "impossible-length",
                vec![0x5a, 0xa5, 0x00, 0x5a, 0xa5, 0x03, 0x12, 0x04, 0xea],
            ),
            (
                "lone-5a-tail",
                vec![0x5a, 0xa5, 0x03, 0x12, 0x04, 0xea, 0x5a],
            ),
        ]
    }

    // ---------------------------------------------------- transmit gates

    #[test]
    fn python_and_rust_fixed_transmit_allowlists_match() {
        let mut python: Vec<_> = include_str!("../../tools/frame-gate.py").lines()
            .map(str::trim).filter(|line| line.starts_with("\"5aa"))
            .map(|line| line.split('"').nth(1).unwrap().to_owned()).collect();
        let mut rust: Vec<String> = ALLOW.iter().map(|frame|
            frame.bytes.iter().map(|byte| format!("{byte:02x}")).collect()).collect();
        python.sort();
        rust.sort();
        assert_eq!(python, rust);
    }

    #[test]
    fn stock_gps_ready_fixed_frame_is_allowed() {
        let frame = [0x5a, 0xa5, 0x02, 0x64, 0x9c];
        assert_eq!(checksum(&[0x64]), frame[4]);
        assert_eq!(may_transmit(&frame).unwrap().name, "GPS-ready notification");
    }

    #[test]
    fn only_the_four_fixed_frames_are_allowed() {
        assert_eq!(ALLOW.len(), 4, "update tools/frame-gate.py with any change");
        assert_eq!(may_transmit(TX_GPS_READY).unwrap().name, "GPS-ready notification");
        assert_eq!(
            may_transmit(TX_VERSION_QUERY).unwrap().name,
            "version query"
        );
        assert_eq!(
            may_transmit(TX_ACC_OFF_COMPLETE).unwrap().name,
            "ACC-off completion"
        );
        assert_eq!(
            may_transmit(TX_ACC_ON_ANSWER).unwrap().name,
            "ACC-on answer"
        );
    }

    #[test]
    fn nothing_transmits_by_default() {
        assert_eq!(
            may_transmit(&[0x5a, 0xa5, 0x03, 0x05, 0x03, 0xf8]),
            Err(FrameRefused::NotAllowed)
        );
    }

    #[test]
    fn the_sleep_authorising_payload_is_denied_by_name() {
        match may_transmit(&[0x5a, 0xa5, 0x03, 0x05, 0x16, 0xe5]) {
            Err(FrameRefused::Denied(why)) => assert!(why.contains("05 16")),
            other => panic!("05 16 was not denied: {:?}", other),
        }
    }

    #[test]
    fn every_guard_hook_forbidden_frame_is_denied() {
        let forbidden = [
            [0x5a, 0xa5, 0x03, 0x05, 0x16, 0xe5],
            [0x5a, 0xa5, 0x03, 0x05, 0x06, 0xf5],
            [0x5a, 0xa5, 0x03, 0x07, 0x08, 0xf1],
            [0x5a, 0xa5, 0x03, 0x84, 0x00, 0x7c],
        ];
        for frame in forbidden {
            assert!(matches!(may_transmit(&frame), Err(FrameRefused::Denied(_))));
        }
    }

    #[test]
    fn unproven_amplifier_on_is_denied_by_name() {
        assert!(matches!(
            may_transmit(&[0x5a, 0xa5, 0x03, 0x05, 0xa2, 0x59]),
            Err(FrameRefused::Denied(_))
        ));
    }

    #[test]
    fn unproven_amplifier_off_is_denied_by_name() {
        assert!(matches!(
            may_transmit(&[0x5a, 0xa5, 0x03, 0x05, 0xa3, 0x58]),
            Err(FrameRefused::Denied(_))
        ));
    }

    #[test]
    fn every_camera_route_is_denied_by_command_family() {
        for route in [0x00u8, 0x07, 0x08, 0x09, 0xff] {
            let frame = [0x5a, 0xa5, 0x03, 0x07, route, 0x00];
            assert!(matches!(may_transmit(&frame), Err(FrameRefused::Denied(_))));
        }
    }

    #[test]
    fn persistent_sleep_delay_is_denied_by_name() {
        assert!(matches!(
            may_transmit(&[0x5a, 0xa5, 0x03, 0x04, 0x06, 0xf6]),
            Err(FrameRefused::Denied(_))
        ));
    }

    #[test]
    fn persistent_low_voltage_cutoff_is_denied_by_name() {
        assert!(matches!(
            may_transmit(&[0x5a, 0xa5, 0x03, 0x04, 0x07, 0xf5]),
            Err(FrameRefused::Denied(_))
        ));
    }

    #[test]
    fn device_identity_write_is_denied_by_name() {
        assert!(matches!(
            may_transmit(&[0x5a, 0xa5, 0x03, 0x3f, 0x01, 0xc0]),
            Err(FrameRefused::Denied(_))
        ));
    }

    #[test]
    fn every_provisioning_code_is_denied_by_command_family() {
        for subcommand in [0x00u8, 0x01, 0xff] {
            let frame = [0x5a, 0xa5, 0x03, 0x79, subcommand, 0x00];
            assert!(matches!(may_transmit(&frame), Err(FrameRefused::Denied(_))));
        }
    }

    #[test]
    fn every_iap_command_is_denied() {
        for cmd in [0x43u8, 0x44, 0x46, 0x4a, 0x4b, 0x50] {
            let frame = [0x5a, 0xa5, 0x03, cmd, 0x00, 0x00];
            assert!(
                matches!(may_transmit(&frame), Err(FrameRefused::Denied(_))),
                "IAP command {:#04x} was not denied",
                cmd
            );
        }
    }

    #[test]
    fn a_denied_command_stays_denied_whatever_follows_it() {
        for tail in [0x00u8, 0x01, 0xff] {
            assert!(matches!(
                may_transmit(&[0x5a, 0xa5, 0x03, 0x43, tail, 0x00]),
                Err(FrameRefused::Denied(_))
            ));
        }
    }

    #[test]
    fn rubbish_is_not_a_frame() {
        assert_eq!(may_transmit(&[]), Err(FrameRefused::NotAFrame));
        assert_eq!(
            may_transmit(&[0x00, 0x01, 0x02, 0x03]),
            Err(FrameRefused::NotAFrame)
        );
        assert_eq!(may_transmit(&[0x5a, 0xa5]), Err(FrameRefused::NotAFrame));
    }

    // ---------------------------------------------------------- the port

    #[test]
    fn se0_is_resolved_by_mmio_not_by_name() {
        let scratch = ScratchSysfs::new("exact");
        let root = scratch.path();
        // The measured trap: SE0 was ttyHS0 on D3-0020 and ttyHS1 on
        // D3-0021/0023 -- and on D3-0043 PID 1 logged ttyHS1=4a80000.serial.
        // Here it is ttyHS1, and ttyHS0 is Bluetooth's SE3.
        for (tty, mmio) in [("ttyHS0", SE3_MMIO_NOT_OURS), ("ttyHS1", SE0_MMIO)] {
            add_fake_tty(root, tty, &format!("{}.serial", mmio));
        }
        assert_eq!(
            resolve_port(&root, SE0_MMIO).unwrap(),
            Some(PathBuf::from("/dev/ttyHS1")),
            "SE0 must be found by MMIO, not by being ttyHS0"
        );
        assert_eq!(
            resolve_port(&root, SE3_MMIO_NOT_OURS).unwrap(),
            Some(PathBuf::from("/dev/ttyHS0"))
        );
    }

    #[test]
    fn adjacent_hex_digits_do_not_turn_nearby_addresses_into_se0() {
        let scratch = ScratchSysfs::new("near-addresses");
        add_fake_tty(scratch.path(), "ttyHS0", "14a80000.serial");
        add_fake_tty(scratch.path(), "ttyHS1", "4a800000.serial");

        assert_eq!(resolve_port(scratch.path(), SE0_MMIO).unwrap(), None);
    }

    #[test]
    fn two_exact_mmio_matches_are_refused_instead_of_name_ordered() {
        let scratch = ScratchSysfs::new("ambiguous");
        add_fake_tty(scratch.path(), "ttyHS0", "4a80000.serial.0");
        add_fake_tty(scratch.path(), "ttyHS1", "4a80000.serial.1");

        let error = resolve_port(scratch.path(), SE0_MMIO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("multiple tty nodes front MMIO"));
    }
}
