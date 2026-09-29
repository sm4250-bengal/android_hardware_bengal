// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Steering-wheel keys from the decoder box, turned into key taps.
//!
//! # What the box sends
//!
//! Frame `0x20`, DATA = `[code, state]`, with state **`00` up, `01` down,
//! `02` long press**. That is the OEM plugin's `com.canbus.json.key.KeyState`
//! (`KeyState.<clinit>` at dex `0x047b28` of `plugin/2/9/P1_301.apk`), which
//! its key decoder reads. The older `DOWN=1 UP=2 LONG=3 ROTATION=4` table in
//! earlier notes was the plugin's *unused* `ENUM_KEY.KEY_STATE`. Two public
//! Raise-VW implementations agree with `00/01/02`
//! (docs/research/2026-09-11-can-vw-pq-parity-plan.md §1, §2.3). This module
//! reads the **profile's state names** (`up`/`down`/`long`), never raw
//! numbers, so the numbers live in one data file.
//!
//! # The policy, and where it deviates from the OEM
//!
//! The OEM acted on DOWN and on **every** LONG frame, for every key, and
//! ignored UP (`CanbusKey_P.sendKey`). Here:
//!
//! * **DOWN** taps once (press + release). A repeated DOWN of the held code
//!   is a retransmission until UP or hold timeout. Captured retransmissions
//!   arrive 120–140 ms apart, so a 100 ms dedupe window was insufficient.
//! * **LONG** repeats only for actions whose [`repeat_policy`] is
//!   [`Repeat::OnLong`] (volume). It does so at most once per
//!   [`REPEAT_MIN_INTERVAL_MS`] and at most [`MAX_REPEATS_PER_HOLD`] times per
//!   hold. A box that floods LONG frames therefore cannot ramp faster than
//!   10 steps/s, and a lost UP cannot run the volume away. Every other action
//!   taps once per physical press; holding NEXT must not skip ten tracks. This
//!   deviates from the OEM on purpose.
//! * A LONG with no DOWN before it (the DOWN frame was lost) starts a hold and
//!   taps once, because the press did happen.
//! * **UP** ends the hold and **never** taps. It ends any hold regardless of
//!   code: one Raise-VW implementation releases on state `00` whatever the code
//!   byte says.
//! * A hold with no frame for [`HOLD_TIMEOUT_MS`] is forgotten.
//! * A frame whose state has no name in the profile is ignored.
//!
//! # Gates, in order ([`CanKeyInjector::handle`])
//!
//! 1. [`super::KEYS_PROPERTY`], default off. An observed disabled frame clears
//!    any stale hold without processing that frame: no emission, device open
//!    or log line. A release received while disabled cannot suppress the next
//!    enabled press. Property changes without a key frame are not observed.
//! 2. The action must have an Android key code ([`android_code`]).
//! 3. `ro.vendor.bengal.hud.inject_keys`, shared with the bezel path.
//! 4. [`can_key`] permits only volume, mute, next/previous, call and voice.
//!    The bezel binding allowlist remains separate. CAN can never emit
//!    HOME, BACK or POWER. SOURCE has no Android key and remains inert.
//!
//! Logging is once per condition, never per press. The daemon's stderr is
//! `/dev/kmsg` at 10 records per 5 s per fd
//! (`mem:bengal-kmsg-ratelimit-stdio-to-kmsg-silent-drops`).

use std::io;

use super::decode::KeyEvent;
use crate::mcu::uinput::{Key, KeyEmitter};

/// Minimum spacing between two LONG-driven repeats of one hold.
pub const REPEAT_MIN_INTERVAL_MS: u64 = 100;
/// Most LONG-driven repeats one hold may produce (Android media volume has
/// fewer steps than this, so a full sweep still fits).
pub const MAX_REPEATS_PER_HOLD: u32 = 30;
/// A hold that has seen no frame for this long is forgotten.
pub const HOLD_TIMEOUT_MS: u64 = 2_000;

/// A key state as the profile names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Released.
    Up,
    /// Pressed.
    Down,
    /// Still held (the box's long-press/repeat frame).
    Long,
    /// A state byte the profile does not name.
    Unknown,
}

impl Phase {
    /// Map the profile's state name (`keys.states`) to a phase.
    pub fn from_state_name(name: Option<&str>) -> Self {
        match name {
            Some("up") => Self::Up,
            Some("down") => Self::Down,
            Some("long") => Self::Long,
            _ => Self::Unknown,
        }
    }
}

/// Whether holding a key repeats its action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repeat {
    /// One tap per physical press.
    Never,
    /// Tap on DOWN, then again on LONG frames (bounded).
    OnLong,
}

/// Repeat policy per profile action.
pub fn repeat_policy(action: &str) -> Repeat {
    match action {
        "VOLUME_UP" | "VOLUME_DOWN" => Repeat::OnLong,
        _ => Repeat::Never,
    }
}

/// The Android `KeyEvent` code for a profile action, or `None` for an action
/// with no Android key (`SOURCE`). Codes are the `android.view.KeyEvent`
/// constants; [`can_key`] applies the separate CAN emission allowlist.
pub fn android_code(action: &str) -> Option<i32> {
    match action {
        "VOLUME_UP" => Some(24),
        "VOLUME_DOWN" => Some(25),
        "MUTE" => Some(164),
        "NEXT" => Some(87),
        "PREVIOUS" => Some(88),
        "PHONE" => Some(5),
        "VOICE" => Some(231),
        _ => None,
    }
}

/// CAN-only Android-to-Linux key allowlist. Never expands bezel bindings.
pub fn can_key(android_code: i32) -> Option<Key> {
    match android_code {
        24 => Some(Key::VolumeUp),
        25 => Some(Key::VolumeDown),
        164 => Some(Key::VolumeMute),
        87 => Some(Key::MediaNext),
        88 => Some(Key::MediaPrevious),
        5 => Some(Key::Call),
        231 => Some(Key::VoiceAssist),
        _ => None,
    }
}

/// One tap the machine decided on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tap {
    /// Profile action (`VOLUME_UP`, ...).
    pub action: String,
    /// `false` for the press itself, `true` for a LONG-driven repeat.
    pub repeat: bool,
}

#[derive(Clone, Debug)]
struct Held {
    code: u8,
    last_frame_ms: u64,
    last_tap_ms: u64,
    repeats: u32,
}

/// The pure key state machine. No I/O, no clock of its own: the caller passes
/// the reader's monotonic milliseconds.
#[derive(Clone, Debug, Default)]
pub struct KeyMachine {
    held: Option<Held>,
}

impl KeyMachine {
    /// Fold one decoded key frame in; return the tap to perform, if any.
    pub fn on_key(&mut self, key: &KeyEvent, now_ms: u64) -> Option<Tap> {
        if let Some(held) = &self.held {
            if now_ms.saturating_sub(held.last_frame_ms) > HOLD_TIMEOUT_MS {
                self.held = None;
            }
        }
        match Phase::from_state_name(key.state_name.as_deref()) {
            Phase::Unknown => None,
            Phase::Up => {
                self.held = None;
                None
            }
            Phase::Down => {
                if let Some(held) = self.held.as_mut() {
                    if held.code == key.code {
                        held.last_frame_ms = now_ms;
                        return None;
                    }
                }
                self.start(key, now_ms)
            }
            Phase::Long => {
                let Some(held) = self.held.as_mut().filter(|h| h.code == key.code) else {
                    return self.start(key, now_ms);
                };
                held.last_frame_ms = now_ms;
                let action = key.action.as_deref()?;
                if repeat_policy(action) != Repeat::OnLong
                    || held.repeats >= MAX_REPEATS_PER_HOLD
                    || now_ms.saturating_sub(held.last_tap_ms) < REPEAT_MIN_INTERVAL_MS
                {
                    return None;
                }
                held.repeats += 1;
                held.last_tap_ms = now_ms;
                Some(Tap { action: action.to_owned(), repeat: true })
            }
        }
    }

    fn start(&mut self, key: &KeyEvent, now_ms: u64) -> Option<Tap> {
        self.held = Some(Held {
            code: key.code,
            last_frame_ms: now_ms,
            last_tap_ms: now_ms,
            repeats: 0,
        });
        key.action
            .as_deref()
            .map(|action| Tap { action: action.to_owned(), repeat: false })
    }
}

/// What [`CanKeyInjector::handle`] did with one key frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Handled {
    /// [`super::KEYS_PROPERTY`] is off: stale hold cleared, no external action.
    Off,
    /// The machine decided no tap (UP, dedupe, bounded repeat, unnamed state).
    NoTap,
    /// The action has no Android key.
    NoAndroidKey(String),
    /// `ro.vendor.bengal.hud.inject_keys` is off.
    InjectionDisabled,
    /// This Android key is outside the CAN allowlist.
    NotEmittable(i32),
    /// The uinput device could not be opened or written.
    EmitFailed,
    /// Press and release were written for this key.
    Emitted(Key),
}

type Opener<E> = Box<dyn FnMut() -> io::Result<E> + Send>;

/// The only path from CAN key frames to uinput: gates plus a lazily opened
/// emitter, exactly like the bezel path opens its own on first injection.
pub struct CanKeyInjector<E> {
    machine: KeyMachine,
    inject_keys: bool,
    emitter: Option<E>,
    open: Opener<E>,
    logged_injection_disabled: bool,
    logged_open_failure: bool,
    logged_not_emittable: Vec<String>,
}

impl<E: KeyEmitter> CanKeyInjector<E> {
    /// `inject_keys` is `ro.vendor.bengal.hud.inject_keys`, read once at boot as
    /// the bezel path does. `open` creates the emitter; it is called only for
    /// a tap that passed every gate, never at startup.
    pub fn new(inject_keys: bool, open: Opener<E>) -> Self {
        Self {
            machine: KeyMachine::default(),
            inject_keys,
            emitter: None,
            open,
            logged_injection_disabled: false,
            logged_open_failure: false,
            logged_not_emittable: Vec::new(),
        }
    }

    /// Whether an emitter has been opened.
    pub fn has_emitter(&self) -> bool {
        self.emitter.is_some()
    }

    /// Forget the previous vehicle's hold/repeat state. All events are taps,
    /// so there is no outstanding pressed uinput key to release.
    pub fn reset(&mut self) {
        self.machine = KeyMachine::default();
    }

    /// Handle one decoded key frame. `enabled` is [`super::KEYS_PROPERTY`],
    /// read by the caller for this frame. `log` receives at most one line per
    /// condition.
    pub fn handle(
        &mut self,
        enabled: bool,
        key: &KeyEvent,
        now_ms: u64,
        log: &mut dyn FnMut(bool, &str),
    ) -> Handled {
        if !enabled {
            // UP may arrive while disabled; retaining the old hold would eat
            // the next enabled DOWN of that code. Never step the disabled frame.
            self.reset();
            return Handled::Off;
        }
        let Some(tap) = self.machine.on_key(key, now_ms) else {
            return Handled::NoTap;
        };
        let Some(code) = android_code(&tap.action) else {
            self.once_per_action(&tap.action, log, || {
                format!("canuart: wheel key action={} has no Android key; not injected", tap.action)
            });
            return Handled::NoAndroidKey(tap.action);
        };
        if !self.inject_keys {
            if !self.logged_injection_disabled {
                self.logged_injection_disabled = true;
                log(false, "canuart: wheel key not injected property=ro.vendor.bengal.hud.inject_keys value=off");
            }
            return Handled::InjectionDisabled;
        }
        let Some(key) = can_key(code) else {
            self.once_per_action(&tap.action, log, || {
                format!("canuart: wheel key action={} android={code} refused outside CAN allowlist", tap.action)
            });
            return Handled::NotEmittable(code);
        };
        if self.emitter.is_none() {
            match (self.open)() {
                Ok(emitter) => {
                    self.logged_open_failure = false;
                    log(false, "canuart: uinput opened for wheel keys (volume/mute/media/call/voice)");
                    self.emitter = Some(emitter);
                }
                Err(error) => {
                    if !self.logged_open_failure {
                        self.logged_open_failure = true;
                        log(true, &format!("canuart: cannot open uinput for wheel keys: {error}"));
                    }
                    return Handled::EmitFailed;
                }
            }
        }
        let Some(emitter) = self.emitter.as_mut() else {
            return Handled::EmitFailed;
        };
        // Every successful action includes its release, independent of CAN UP.
        // Either write may partially fail; discard the device on any error.
        // UinputDevice::drop destroys it, releasing any outstanding key.
        let mut emitted = emitter.emit(key, true);
        if emitted.is_ok() {
            emitted = emitter.emit(key, false);
        }
        match emitted {
            Ok(()) => Handled::Emitted(key),
            Err(error) => {
                log(true, &format!("canuart: wheel key emit failed: {error}"));
                self.emitter = None;
                Handled::EmitFailed
            }
        }
    }

    fn once_per_action(
        &mut self,
        action: &str,
        log: &mut dyn FnMut(bool, &str),
        line: impl FnOnce() -> String,
    ) {
        if !self.logged_not_emittable.iter().any(|a| a == action) {
            self.logged_not_emittable.push(action.to_owned());
            log(false, &line());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::decode::decode;
    use super::super::envelope::{encode_frame, Item, Parser};
    use super::super::profile::Profile;
    use super::*;
    use std::sync::{Arc, Mutex};

    const SHIPPED: &str = include_str!("../../can/profiles/xinpu-vw-pq.toml");

    fn shipped() -> Profile {
        Profile::parse(SHIPPED).expect("shipped profile")
    }

    /// Decode real box bytes (`2E 20 02 code state CS`) with a profile, exactly
    /// as the reader does. Synthetic vectors unless a capture is cited.
    fn key(profile: &Profile, code: u8, state: u8) -> KeyEvent {
        let items = Parser::new().push(&encode_frame(0x20, &[code, state]));
        let Item::Frame(frame) = items.into_iter().next().expect("frame") else {
            panic!("not a frame")
        };
        decode(profile, &frame).key.expect("key frame")
    }

    /// Run `(code, state, t_ms)` through the machine and list the taps.
    fn taps(profile: &Profile, frames: &[(u8, u8, u64)]) -> Vec<(String, bool)> {
        let mut m = KeyMachine::default();
        frames
            .iter()
            .filter_map(|&(c, s, t)| m.on_key(&key(profile, c, s), t))
            .map(|tap| (tap.action, tap.repeat))
            .collect()
    }

    fn vol_up(n_repeats: usize) -> Vec<(String, bool)> {
        let mut v = vec![("VOLUME_UP".to_owned(), false)];
        v.extend(std::iter::repeat(("VOLUME_UP".to_owned(), true)).take(n_repeats));
        v
    }

    #[test]
    fn shipped_profile_names_the_plugin_key_states_and_radar_ids() {
        let p = shipped();
        let keys = p.keys.as_ref().unwrap();
        let mut states = keys.states.clone();
        states.sort_by_key(|(_, v)| *v);
        assert_eq!(
            states,
            vec![("up".into(), 0), ("down".into(), 1), ("long".into(), 2)],
            "KeyState.<clinit> of plugin 2/9: UP=0 DOWN=1 LONG_PRESS=2"
        );
        let name_of = |t: u8| p.messages.iter().find(|m| m.message_type == t).map(|m| m.name.clone());
        assert_eq!(name_of(0x22).as_deref(), Some("radar_rear"), "car_radar_2009002.json: 0x22 = BACK_*");
        assert_eq!(name_of(0x23).as_deref(), Some("radar_front"), "car_radar_2009002.json: 0x23 = FRONT_*");
    }

    #[test]
    fn a_short_press_taps_once_and_up_never_taps() {
        let p = shipped();
        assert_eq!(taps(&p, &[(0x01, 0x01, 0), (0x01, 0x00, 120)]), vol_up(0));
        assert!(taps(&p, &[(0x01, 0x00, 0)]).is_empty(), "UP alone must never tap");
        assert_eq!(
            taps(&p, &[(0x02, 0x01, 0), (0x02, 0x00, 90)]),
            vec![("VOLUME_DOWN".to_owned(), false)]
        );
    }

    #[test]
    fn holding_volume_repeats_on_long_frames() {
        // DOWN, three LONG frames 200 ms apart, UP: the OEM stepped 4 times.
        let p = shipped();
        let frames = [(0x01, 0x01, 0), (0x01, 0x02, 400), (0x01, 0x02, 600), (0x01, 0x02, 800), (0x01, 0x00, 900)];
        assert_eq!(taps(&p, &frames), vol_up(3));
    }

    #[test]
    fn the_old_wrong_state_table_misfires_and_the_fixed_one_does_not() {
        // The table the profile shipped before this change: down=1 up=2 long=3.
        let old = Profile::parse(&SHIPPED.replace(
            "up = 0\ndown = 1\nlong = 2",
            "down = 1\nup = 2\nlong = 3",
        ))
        .unwrap();
        let mut old_states = old.keys.as_ref().unwrap().states.clone();
        old_states.sort_by_key(|(_, v)| *v);
        assert_eq!(
            old_states,
            vec![("down".into(), 1), ("up".into(), 2), ("long".into(), 3)],
            "the mutation must really install the old table"
        );
        let hold = [(0x01, 0x01, 0), (0x01, 0x02, 400), (0x01, 0x02, 600), (0x01, 0x02, 800), (0x01, 0x00, 900)];
        // Old table: every LONG byte 02 reads as "up", so the hold ends at the
        // first repeat, and the real release 00 has no name at all. A held
        // volume key steps once instead of four times.
        assert_eq!(taps(&old, &hold), vol_up(0));
        assert_eq!(key(&old, 0x01, 0x02).state_name.as_deref(), Some("up"));
        assert_eq!(key(&old, 0x01, 0x00).state_name, None);
        // Fixed table: four steps, and 00 is the release.
        assert_eq!(taps(&shipped(), &hold), vol_up(3));
        assert_eq!(key(&shipped(), 0x01, 0x00).state_name.as_deref(), Some("up"));
    }

    #[test]
    fn a_retransmitted_down_is_not_a_second_press() {
        let p = shipped();
        // The box repeats an un-ACKed DOWN 20 ms later: one tap.
        assert_eq!(taps(&p, &[(0x01, 0x01, 0), (0x01, 0x01, 20), (0x01, 0x01, 45)]), vol_up(0));
        // A real second press after a release is a second tap.
        assert_eq!(
            taps(&p, &[(0x01, 0x01, 0), (0x01, 0x00, 50), (0x01, 0x01, 300)]),
            vec![("VOLUME_UP".to_owned(), false), ("VOLUME_UP".to_owned(), false)]
        );
    }

    #[test]
    fn repeats_are_rate_limited_and_bounded_per_hold() {
        let p = shipped();
        // LONG every 20 ms for 10 s, never released: at most one repeat per
        // 100 ms and at most 30 in total.
        let mut frames = vec![(0x01, 0x01, 0)];
        frames.extend((1..=500).map(|i| (0x01, 0x02, i * 20)));
        let out = taps(&p, &frames);
        assert_eq!(out.len(), 1 + MAX_REPEATS_PER_HOLD as usize);
        // Spacing: 30 repeats of a 20-ms flood land 100 ms apart.
        let mut m = KeyMachine::default();
        let times: Vec<u64> = frames
            .iter()
            .filter(|&&(c, s, t)| m.on_key(&key(&p, c, s), t).is_some_and(|tap| tap.repeat))
            .map(|&(_, _, t)| t)
            .collect();
        assert!(times.windows(2).all(|w| w[1] - w[0] >= REPEAT_MIN_INTERVAL_MS), "{times:?}");
    }

    #[test]
    fn media_keys_tap_once_per_press_however_long_they_are_held() {
        let p = shipped();
        let frames = [(0x03, 0x01, 0), (0x03, 0x02, 300), (0x03, 0x02, 500), (0x03, 0x02, 700), (0x03, 0x00, 800)];
        assert_eq!(taps(&p, &frames), vec![("NEXT".to_owned(), false)]);
        // 0x13 is the plugin's second NEXT code.
        assert_eq!(taps(&p, &[(0x13, 0x01, 0)]), vec![("NEXT".to_owned(), false)]);
    }

    #[test]
    fn a_long_without_its_down_is_still_one_press() {
        let p = shipped();
        assert_eq!(taps(&p, &[(0x01, 0x02, 0), (0x01, 0x02, 200), (0x01, 0x00, 300)]), vol_up(1));
        assert_eq!(taps(&p, &[(0x04, 0x02, 0), (0x04, 0x02, 300)]), vec![("PREVIOUS".to_owned(), false)]);
    }

    #[test]
    fn a_silent_hold_times_out_and_unnamed_states_are_ignored() {
        let p = shipped();
        // DOWN, then nothing for > HOLD_TIMEOUT_MS: a later LONG is a new press.
        assert_eq!(taps(&p, &[(0x01, 0x01, 0), (0x01, 0x02, 2_500)]), vol_up(0).into_iter().chain(vol_up(0)).collect::<Vec<_>>());
        // States 03/04 are not in the plugin's table: ignored, hold untouched.
        assert!(taps(&p, &[(0x01, 0x03, 0), (0x01, 0x04, 10)]).is_empty());
        // An unmapped code taps nothing but still tracks the hold.
        assert!(taps(&p, &[(0x63, 0x01, 0), (0x63, 0x02, 300)]).is_empty());
    }

    #[test]
    fn a_new_code_while_another_is_held_is_its_own_press() {
        let p = shipped();
        assert_eq!(
            taps(&p, &[(0x01, 0x01, 0), (0x02, 0x01, 10)]),
            vec![("VOLUME_UP".to_owned(), false), ("VOLUME_DOWN".to_owned(), false)]
        );
    }

    #[test]
    fn android_codes_and_policy_tables() {
        assert_eq!(android_code("VOLUME_UP"), Some(24));
        assert_eq!(android_code("VOLUME_DOWN"), Some(25));
        assert_eq!(android_code("SOURCE"), None);
        assert_eq!(repeat_policy("VOLUME_DOWN"), Repeat::OnLong);
        assert_eq!(repeat_policy("MUTE"), Repeat::Never);
        // Every action the shipped profile names is classified.
        for m in &shipped().keys.unwrap().map {
            assert!(
                android_code(&m.action).is_some() || m.action == "SOURCE",
                "unclassified action {}",
                m.action
            );
        }
        for code in [3, 4, 26, 82, 999] {
            assert_eq!(can_key(code), None, "HOME/BACK/POWER/MENU/arbitrary key");
        }
        for code in [164, 87, 88, 5, 231] {
            assert!(matches!(crate::buttons::emit_decision(true, Some(code)),
                crate::buttons::EmitDecision::NotEmittable { .. }),
                "CAN support must not expand bezel bindings");
        }
    }

    // ---------------------------------------------------------- injector ----

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<(Key, bool)>>>);
    impl KeyEmitter for Recorder {
        fn emit(&mut self, key: Key, pressed: bool) -> io::Result<()> {
            self.0.lock().unwrap().push((key, pressed));
            Ok(())
        }
    }

    struct Rig {
        injector: CanKeyInjector<Recorder>,
        opens: Arc<Mutex<u32>>,
        events: Arc<Mutex<Vec<(Key, bool)>>>,
        lines: Vec<(bool, String)>,
    }

    fn rig(inject_keys: bool) -> Rig {
        let opens = Arc::new(Mutex::new(0));
        let recorder = Recorder::default();
        let events = recorder.0.clone();
        let counter = opens.clone();
        let injector = CanKeyInjector::new(
            inject_keys,
            Box::new(move || {
                *counter.lock().unwrap() += 1;
                Ok(recorder.clone())
            }),
        );
        Rig { injector, opens, events, lines: Vec::new() }
    }

    impl Rig {
        fn press(&mut self, enabled: bool, code: u8, state: u8, t: u64) -> Handled {
            let lines = &mut self.lines;
            self.injector
                .handle(enabled, &key(&shipped(), code, state), t, &mut |e, m| lines.push((e, m.to_owned())))
        }
        fn opens(&self) -> u32 {
            *self.opens.lock().unwrap()
        }
        fn events(&self) -> Vec<(Key, bool)> {
            self.events.lock().unwrap().clone()
        }
    }

    #[test]
    fn fresh_property_off_never_opens_emits_or_logs() {
        let mut r = rig(true);
        for (i, (c, s)) in [(0x01, 0x01), (0x01, 0x02), (0x01, 0x00), (0x03, 0x01)].into_iter().enumerate() {
            assert_eq!(r.press(false, c, s, i as u64 * 300), Handled::Off);
        }
        assert_eq!(r.opens(), 0, "no uinput device");
        assert!(r.events().is_empty(), "no key");
        assert!(r.lines.is_empty(), "no log line");
        assert!(!r.injector.has_emitter());
    }

    #[test]
    fn disabled_release_does_not_suppress_the_next_enabled_press() {
        let mut r = rig(true);
        assert_eq!(r.press(true, 0x01, 0x01, 0), Handled::Emitted(Key::VolumeUp));
        let lines = r.lines.len();
        assert_eq!(r.press(false, 0x01, 0x00, 100), Handled::Off);
        assert_eq!(r.events().len(), 2, "disabled release emits nothing");
        assert_eq!(r.lines.len(), lines, "disabled release logs nothing");
        assert_eq!(r.press(true, 0x01, 0x01, 200), Handled::Emitted(Key::VolumeUp));
        assert_eq!(r.events().len(), 4, "two physical presses, two complete taps");
        assert_eq!(r.opens(), 1, "reuse the existing emitter");
    }

    #[test]
    fn repeated_disabled_frames_neither_emit_nor_seed_an_enabled_hold() {
        let mut r = rig(true);
        assert_eq!(r.press(true, 0x01, 0x01, 0), Handled::Emitted(Key::VolumeUp));
        let lines = r.lines.len();
        // Reset must not depend on observing an UP, or advance a disabled DOWN/LONG.
        for (state, time) in [(0x01, 10), (0x02, 20), (0x02, 30)] {
            assert_eq!(r.press(false, 0x01, state, time), Handled::Off);
        }
        assert_eq!(r.events().len(), 2);
        assert_eq!(r.opens(), 1);
        assert_eq!(r.lines.len(), lines);
        assert_eq!(r.press(true, 0x01, 0x01, 40), Handled::Emitted(Key::VolumeUp));
        assert_eq!(r.press(true, 0x01, 0x01, 50), Handled::NoTap);
        assert_eq!(r.press(true, 0x01, 0x02, 140), Handled::Emitted(Key::VolumeUp));
        assert_eq!(r.press(true, 0x01, 0x02, 150), Handled::NoTap);
        assert_eq!(r.events().len(), 6);
    }

    #[test]
    fn vehicle_change_forgets_hold_without_reopening_uinput() {
        let mut r = rig(true);
        assert_eq!(r.press(true, 0x01, 0x01, 0), Handled::Emitted(Key::VolumeUp));
        assert_eq!(r.press(true, 0x01, 0x01, 10), Handled::NoTap);
        r.injector.reset();
        assert_eq!(r.press(true, 0x01, 0x01, 20), Handled::Emitted(Key::VolumeUp));
        assert_eq!(r.opens(), 1);
        assert_eq!(r.events().len(), 4, "two complete taps, no held key");
    }

    #[test]
    fn inject_keys_off_refuses_through_the_shared_property_and_says_so_once() {
        let mut r = rig(false);
        assert_eq!(r.press(true, 0x01, 0x01, 0), Handled::InjectionDisabled);
        assert_eq!(r.press(true, 0x01, 0x00, 100), Handled::NoTap);
        assert_eq!(r.press(true, 0x02, 0x01, 400), Handled::InjectionDisabled);
        assert_eq!(r.opens(), 0);
        assert!(r.events().is_empty());
        assert_eq!(r.lines.len(), 1, "{:?}", r.lines);
        assert!(r.lines[0].1.contains("inject_keys value=off"));
    }

    #[test]
    fn volume_keys_emit_press_and_release_through_one_lazily_opened_device() {
        let mut r = rig(true);
        assert_eq!(r.press(true, 0x01, 0x01, 0), Handled::Emitted(Key::VolumeUp));
        assert_eq!(r.press(true, 0x01, 0x02, 400), Handled::Emitted(Key::VolumeUp));
        assert_eq!(r.press(true, 0x01, 0x00, 500), Handled::NoTap);
        assert_eq!(r.press(true, 0x02, 0x01, 900), Handled::Emitted(Key::VolumeDown));
        assert_eq!(r.opens(), 1, "opened once, on the first tap");
        assert_eq!(
            r.events(),
            vec![
                (Key::VolumeUp, true), (Key::VolumeUp, false),
                (Key::VolumeUp, true), (Key::VolumeUp, false),
                (Key::VolumeDown, true), (Key::VolumeDown, false),
            ]
        );
        assert_eq!(r.lines.len(), 1, "only the open line: {:?}", r.lines);
    }

    #[test]
    fn mapped_wheel_actions_reach_the_emitter() {
        let mut r = rig(true);
        for (code, want) in [(3, Key::MediaNext), (0x13, Key::MediaNext),
            (4, Key::MediaPrevious), (0x14, Key::MediaPrevious),
            (5, Key::Call), (6, Key::VolumeMute), (8, Key::VoiceAssist)] {
            assert_eq!(r.press(true, code, 1, 0), Handled::Emitted(want),
                "received mapped wheel key {code:#x} was refused");
            // Lost release plus continuous repeated DOWN/LONG must not
            // retrigger media, mute, phone or voice actions.
            for t in (140..10_000).step_by(140) {
                assert_eq!(r.press(true, code, 1, t), Handled::NoTap);
                assert_eq!(r.press(true, code, 2, t + 50), Handled::NoTap);
            }
            assert_eq!(r.press(true, code, 0, 10_100), Handled::NoTap);
        }
        assert_eq!(r.events().len(), 14);
        assert_eq!(r.opens(), 1);
        for tap in r.events().chunks_exact(2) {
            assert_eq!(tap, [(tap[0].0, true), (tap[0].0, false)]);
        }
    }

    #[test]
    fn nonrepeat_key_recovers_after_lost_release_and_silence() {
        let mut r = rig(true);
        assert_eq!(r.press(true, 5, 2, 0), Handled::Emitted(Key::Call));
        assert_eq!(r.press(true, 5, 2, 300), Handled::NoTap);
        assert_eq!(r.press(true, 5, 1, 300 + HOLD_TIMEOUT_MS + 1), Handled::Emitted(Key::Call));
        assert_eq!(r.events().len(), 4, "each action releases immediately");
    }

    #[test]
    fn repeated_down_cannot_reset_volume_repeat_budget() {
        let mut r = rig(true);
        assert_eq!(r.press(true, 1, 1, 0), Handled::Emitted(Key::VolumeUp));
        for t in (140..10_000).step_by(140) {
            assert_eq!(r.press(true, 1, 1, t), Handled::NoTap);
            r.press(true, 1, 2, t + 50);
        }
        assert_eq!(r.events().len(), 2 * (1 + MAX_REPEATS_PER_HOLD as usize));
        assert_eq!(r.press(true, 1, 0, 10_100), Handled::NoTap);
    }

    #[test]
    fn partial_write_failure_destroys_emitter_before_retry() {
        struct Broken { calls: u8, fail_at: u8, drops: Arc<Mutex<u32>> }
        impl KeyEmitter for Broken {
            fn emit(&mut self, _: Key, _: bool) -> io::Result<()> {
                self.calls += 1;
                if self.calls == self.fail_at { Err(io::Error::other("partial write")) }
                else { Ok(()) }
            }
        }
        impl Drop for Broken {
            fn drop(&mut self) { *self.drops.lock().unwrap() += 1; }
        }
        for fail_at in [1, 2] {
            let drops = Arc::new(Mutex::new(0));
            let d = drops.clone();
            let mut injector = CanKeyInjector::new(true, Box::new(move ||
                Ok(Broken { calls: 0, fail_at, drops: d.clone() })));
            assert_eq!(injector.handle(true, &key(&shipped(), 3, 1), 0,
                &mut |_, _| {}), Handled::EmitFailed);
            assert!(!injector.has_emitter());
            assert_eq!(*drops.lock().unwrap(), 1, "destroy even after failed release");
        }
    }

    #[test]
    fn captured_previous_retransmissions_tap_only_once() {
        // can-34-14.jsonl:2-13 in the 2026-09-21 pull. These are RX
        // monotonic times, not the capture's obsolete decoded state labels.
        let p = shipped();
        assert_eq!(taps(&p, &[(4, 1, 14887), (4, 1, 15027),
            (4, 1, 15147), (4, 0, 15287), (4, 0, 15427), (4, 0, 15547)]),
            vec![("PREVIOUS".to_owned(), false)]);
    }

    #[test]
    fn unsupported_actions_are_inert_and_logged_once() {
        let mut r = rig(true);
        for t in [0, 1000] {
            assert_eq!(r.press(true, 0x07, 1, t), Handled::NoAndroidKey("SOURCE".into()));
            assert_eq!(r.press(true, 0x07, 0, t + 50), Handled::NoTap);
        }
        assert_eq!(r.opens(), 0);
        assert!(r.events().is_empty());
        assert_eq!(r.lines.len(), 1);
    }

    #[test]
    fn an_open_failure_is_logged_once_and_retried_on_the_next_tap() {
        let tries = Arc::new(Mutex::new(0u32));
        let t = tries.clone();
        let mut injector: CanKeyInjector<Recorder> = CanKeyInjector::new(
            true,
            Box::new(move || {
                *t.lock().unwrap() += 1;
                Err(io::Error::from_raw_os_error(13))
            }),
        );
        let mut lines = Vec::new();
        let p = shipped();
        for i in 0..3u64 {
            let got = injector.handle(true, &key(&p, 0x01, 0x01), i * 1_000, &mut |e, m| lines.push((e, m.to_owned())));
            assert_eq!(got, Handled::EmitFailed);
            injector.handle(true, &key(&p, 0x01, 0x00), i * 1_000 + 100, &mut |_, _| {});
        }
        assert_eq!(*tries.lock().unwrap(), 3);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].0, "an open failure is an error line");
    }
}
