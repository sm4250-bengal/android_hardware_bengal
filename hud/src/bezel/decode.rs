// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The Bengal device-3 project

//! Turn raw Goodix evdev traffic into bezel presses.
//!
//! MEASURED, D3-0114 capture pass 1 (2026-09-04): all five left-bezel buttons
//! report through the touchscreen as contacts at **negative X** — off the panel
//! to the left, X in −39..−25 — in five well-separated Y bands matching the
//! physical top-to-bottom order:
//!
//! ```text
//!   Power       Y ~94-124
//!   Home        Y ~189-192
//!   Back        Y ~264-279
//!   Volume Up   Y ~346-361
//!   Volume Down Y ~428-438
//! ```
//!
//! Android receives every one of them and DISCARDS it: the same capture logged
//! `InputReader out-of-frame drops: 15` against exactly 15 contacts. The
//! hardware, the driver and the events are all fine; they die at the input
//! layer for want of a mapping. This decoder is the daemon's own view of that
//! traffic, so the presses can be observed and eventually mapped.
//!
//! **This module deliberately does NOT know which button is which.** It reports
//! a press at a coordinate. Naming is a mapping, the mapping is per-unit, and
//! it is learned rather than compiled in — see
//! `docs/research/2026-09-04-button-binding-backend-design.md`. Baking the five
//! bands above into the daemon would defeat the feature they are evidence for,
//! and they are one capture pass old besides.

/// Linux input event types and codes this decoder cares about.
mod codes {
    pub const EV_SYN: u16 = 0x00;
    pub const EV_KEY: u16 = 0x01;
    pub const EV_ABS: u16 = 0x03;

    pub const SYN_REPORT: u16 = 0x00;
    pub const SYN_DROPPED: u16 = 0x03;
    pub const ABS_MT_SLOT: u16 = 0x2f;
    pub const ABS_MT_POSITION_X: u16 = 0x35;
    pub const ABS_MT_POSITION_Y: u16 = 0x36;
    pub const ABS_MT_TRACKING_ID: u16 = 0x39;
    /// Pointer-emulation axes. The type-B slot axes remain authoritative.
    #[cfg(test)]
    pub const ABS_X: u16 = 0x00;
    #[cfg(test)]
    pub const ABS_Y: u16 = 0x01;
    pub const BTN_TOUCH: u16 = 0x14a;
}

/// D3-0114's Goodix capability report advertises slots 0 through 9.
const SLOT_COUNT: usize = 10;

/// Largest movement accepted as one intentional bezel press.
///
/// The two D3-0114 passes place the four bindable buttons within 16 px per
/// button. That measured spread is a conservative tolerance for a contact
/// that drifts while pressed; it is not a button band or a function mapping.
const MAX_CONTACT_DRIFT_PX: i32 = 16;

/// One completed bezel contact, in raw touchscreen coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BezelPress {
    /// Off-panel X; always negative for a bezel contact.
    pub x: i32,
    /// Y position, which is what distinguishes one button from another.
    pub y: i32,
}

/// Incremental decoder for one evdev stream.
///
/// The Goodix driver uses type-B multitouch: `ABS_MT_SLOT` selects a contact,
/// `ABS_MT_TRACKING_ID` starts or finishes it, and `SYN_REPORT` commits the
/// frame. `ABS_X`/`ABS_Y` and `BTN_TOUCH` are pointer emulation and must never
/// change a slot's state. After `SYN_DROPPED`, only a post-fence
/// `BTN_TOUCH=0` is used as all-up resynchronization evidence.
#[derive(Debug)]
pub struct BezelDecoder {
    slots: [Slot; SLOT_COUNT],
    current_slot: Option<usize>,
    type_b_seen: bool,
    legacy_x: Option<i32>,
    legacy_y: Option<i32>,
    cancelled: bool,
    desynced: bool,
    drop_fence_seen: bool,
    explicit_all_up: bool,
    completed: Option<BezelPress>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Slot {
    active: bool,
    tracking_id: Option<i32>,
    x: Option<i32>,
    y: Option<i32>,
    origin: Option<(i32, i32)>,
    released: bool,
}

impl Default for BezelDecoder {
    fn default() -> Self {
        Self {
            slots: [Slot::default(); SLOT_COUNT],
            // D3-0114's one-finger trace has no ABS_MT_SLOT because slot zero
            // is already selected. The capability report still proves type-B.
            current_slot: Some(0),
            type_b_seen: false,
            legacy_x: None,
            legacy_y: None,
            cancelled: false,
            desynced: false,
            drop_fence_seen: false,
            explicit_all_up: false,
            completed: None,
        }
    }
}

impl BezelDecoder {
    /// A fresh decoder with no partial contact.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one event; returns a press when one completes.
    ///
    /// Only one isolated contact that stays at negative X can produce a press.
    /// Any on-panel position, second contact, excess movement, invalid slot,
    /// or dropped frame cancels the whole gesture until every slot is up.
    pub fn ingest(&mut self, event_type: u16, code: u16, value: i32) -> Option<BezelPress> {
        if self.desynced {
            if event_type == codes::EV_SYN && code == codes::SYN_DROPPED {
                self.drop_sync();
                return None;
            }
            if event_type == codes::EV_SYN && code == codes::SYN_REPORT {
                return self.finish_frame();
            }
            if self.drop_fence_seen
                && event_type == codes::EV_KEY
                && code == codes::BTN_TOUCH
                && value == 0
            {
                self.explicit_all_up = true;
            }
            return None;
        }
        match (event_type, code) {
            (codes::EV_ABS, codes::ABS_MT_SLOT) => {
                self.begin_type_b();
                self.select_slot(value);
            }
            (codes::EV_ABS, codes::ABS_MT_TRACKING_ID) => {
                self.begin_type_b();
                self.track(value);
            }
            (codes::EV_ABS, codes::ABS_MT_POSITION_X) => self.mt_position(true, value),
            (codes::EV_ABS, codes::ABS_MT_POSITION_Y) => self.mt_position(false, value),
            // Old host recordings encoded a one-finger contact with MT axes
            // plus BTN_TOUCH but no tracking ID. Keep that narrow fallback
            // until a type-B marker is seen; real D3 traffic always selects
            // the type-B path, and ABS_X/Y remain ignored in both paths.
            (codes::EV_KEY, codes::BTN_TOUCH) if !self.type_b_seen => self.legacy_touch(value),
            (codes::EV_SYN, codes::SYN_DROPPED) => self.drop_sync(),
            (codes::EV_SYN, codes::SYN_REPORT) => return self.finish_frame(),
            // BTN_TOUCH and ABS_X/Y are the driver's pointer emulation. They
            // can describe a different contact after slot selection changes.
            _ => {}
        }
        None
    }

    fn begin_type_b(&mut self) {
        if self.type_b_seen {
            return;
        }
        self.type_b_seen = true;
        self.legacy_x = None;
        self.legacy_y = None;
        if self.slots[0].active {
            self.cancel();
            self.slots = [Slot::default(); SLOT_COUNT];
        }
    }

    fn mt_position(&mut self, is_x: bool, value: i32) {
        if self.type_b_seen || self.slots[0].active {
            self.position(is_x, value);
        } else if is_x {
            self.legacy_x = Some(value);
        } else {
            self.legacy_y = Some(value);
        }
    }

    fn legacy_touch(&mut self, value: i32) {
        if value == 0 {
            self.track(-1);
            return;
        }
        if value != 1 || self.desynced || self.slots[0].active {
            return;
        }
        let (Some(x), Some(y)) = (self.legacy_x, self.legacy_y) else {
            self.cancel();
            return;
        };
        self.slots[0] = Slot {
            active: true,
            x: Some(x),
            y: Some(y),
            origin: Some((x, y)),
            ..Slot::default()
        };
        if x >= 0 {
            self.cancel();
        }
    }

    fn select_slot(&mut self, value: i32) {
        self.current_slot = usize::try_from(value)
            .ok()
            .filter(|slot| *slot < SLOT_COUNT);
        if self.current_slot.is_none() {
            self.drop_sync();
        }
    }

    fn track(&mut self, value: i32) {
        let Some(slot) = self.current_slot else {
            self.drop_sync();
            return;
        };
        if value < 0 {
            let previous = &mut self.slots[slot];
            if previous.active {
                previous.active = false;
                previous.tracking_id = None;
                previous.released = true;
            }
            if !self.type_b_seen {
                self.legacy_x = None;
                self.legacy_y = None;
            }
            return;
        }

        let replacing_live_slot =
            self.slots[slot].active && self.slots[slot].tracking_id != Some(value);
        if !self.slots[slot].active {
            // A release is not a completed gesture until SYN_REPORT. Any new
            // contact in that frame means there was no global all-up boundary.
            if self.completed.is_some() {
                self.cancel();
            }
            let released_before_frame = self.slots.iter().any(|slot| slot.released);
            self.slots[slot].active = true;
            self.slots[slot].tracking_id = Some(value);
            self.slots[slot].origin = None;
            self.slots[slot].released = false;
            if released_before_frame {
                self.cancel();
            }
            if self.active_slots() > 1 {
                self.cancel();
            }
        } else {
            self.slots[slot].tracking_id = Some(value);
            if replacing_live_slot {
                self.cancel();
            }
        }
    }

    fn position(&mut self, is_x: bool, value: i32) {
        let Some(slot) = self.current_slot else {
            return;
        };
        if self.desynced || !self.slots[slot].active {
            return;
        }
        if is_x && value >= 0 {
            self.cancel();
        }
        if is_x {
            self.slots[slot].x = Some(value);
        } else {
            self.slots[slot].y = Some(value);
        }
        let Some((x, y)) = self.slots[slot].x.zip(self.slots[slot].y) else {
            return;
        };
        let Some((origin_x, origin_y)) = self.slots[slot].origin else {
            return;
        };
        let cancel = (i64::from(x) - i64::from(origin_x)).unsigned_abs()
            > MAX_CONTACT_DRIFT_PX as u64
            || (i64::from(y) - i64::from(origin_y)).unsigned_abs() > MAX_CONTACT_DRIFT_PX as u64;
        if cancel {
            self.cancel();
        }
    }

    fn drop_sync(&mut self) {
        self.desynced = true;
        self.drop_fence_seen = false;
        self.explicit_all_up = false;
        self.slots = [Slot::default(); SLOT_COUNT];
        self.legacy_x = None;
        self.legacy_y = None;
        self.cancel();
    }

    fn finish_frame(&mut self) -> Option<BezelPress> {
        if self.desynced {
            if !self.drop_fence_seen {
                self.drop_fence_seen = true;
            } else if self.explicit_all_up {
                self.desynced = false;
                self.drop_fence_seen = false;
                self.explicit_all_up = false;
                self.cancelled = false;
                self.completed = None;
            }
            return None;
        }
        for slot in 0..SLOT_COUNT {
            self.settle_slot(slot);
        }
        if self.active_slots() != 0 {
            return None;
        }
        let press = (!self.cancelled).then_some(self.completed).flatten();
        self.cancelled = false;
        self.completed = None;
        for slot in &mut self.slots {
            slot.released = false;
            slot.origin = None;
        }
        press
    }

    fn settle_slot(&mut self, slot: usize) {
        if !self.slots[slot].active && !self.slots[slot].released {
            return;
        }
        let Some((x, y)) = self.slots[slot].x.zip(self.slots[slot].y) else {
            return;
        };
        if x >= 0 {
            self.cancel();
            return;
        }
        if let Some((origin_x, origin_y)) = self.slots[slot].origin {
            if (i64::from(x) - i64::from(origin_x)).unsigned_abs() > MAX_CONTACT_DRIFT_PX as u64
                || (i64::from(y) - i64::from(origin_y)).unsigned_abs() > MAX_CONTACT_DRIFT_PX as u64
            {
                self.cancel();
            }
        } else {
            self.slots[slot].origin = Some((x, y));
        }
        if self.slots[slot].released && !self.cancelled {
            self.completed = Some(BezelPress { x, y });
        }
    }

    fn active_slots(&self) -> usize {
        self.slots.iter().filter(|slot| slot.active).count()
    }

    fn cancel(&mut self) {
        self.cancelled = true;
        self.completed = None;
    }
}

#[cfg(test)]
mod tests {
    use super::codes::*;
    use super::*;

    #[test]
    fn measured_d3_type_b_trace_emits_once_on_the_release_frame() {
        // First contact from D3-0114 pass 1. It stays in slot zero, so the
        // kernel suppresses an unchanged ABS_MT_SLOT event; the default slot
        // is therefore part of the measured protocol, not an assumption.
        let mut decoder = BezelDecoder::new();
        for (event_type, code, value) in [
            (EV_ABS, ABS_MT_TRACKING_ID, 0x16),
            (EV_ABS, ABS_MT_POSITION_X, -30),
            (EV_ABS, ABS_MT_POSITION_Y, 189),
            (EV_KEY, BTN_TOUCH, 1),
            (EV_ABS, ABS_X, -30),
            (EV_ABS, ABS_Y, 189),
            (EV_SYN, SYN_REPORT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
            (EV_KEY, BTN_TOUCH, 0),
        ] {
            assert_eq!(decoder.ingest(event_type, code, value), None);
        }
        assert_eq!(
            decoder.ingest(EV_SYN, SYN_REPORT, 0),
            Some(BezelPress { x: -30, y: 189 })
        );
    }

    #[test]
    fn bezel_to_display_crossover_cancels_the_candidate() {
        let mut decoder = BezelDecoder::new();
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, 7);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, -30);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, 189);
        decoder.ingest(EV_SYN, SYN_REPORT, 0);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, 40);
        decoder.ingest(EV_SYN, SYN_REPORT, 0);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, -1);
        assert_eq!(decoder.ingest(EV_SYN, SYN_REPORT, 0), None);
    }

    #[test]
    fn an_on_panel_x_before_y_cancels_even_if_the_final_x_is_negative() {
        let mut decoder = BezelDecoder::new();
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, 7);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, 40);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, -30);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, 189);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, -1);
        assert_eq!(decoder.ingest(EV_SYN, SYN_REPORT, 0), None);
    }

    #[test]
    fn a_second_finger_cancels_until_every_slot_is_up() {
        let mut decoder = BezelDecoder::new();
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, 7);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, -30);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, 189);
        decoder.ingest(EV_SYN, SYN_REPORT, 0);
        decoder.ingest(EV_ABS, ABS_MT_SLOT, 1);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, 8);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, 500);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, 300);
        decoder.ingest(EV_SYN, SYN_REPORT, 0);
        decoder.ingest(EV_ABS, ABS_MT_SLOT, 0);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, -1);
        assert_eq!(decoder.ingest(EV_SYN, SYN_REPORT, 0), None);
        decoder.ingest(EV_ABS, ABS_MT_SLOT, 1);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, -1);
        assert_eq!(decoder.ingest(EV_SYN, SYN_REPORT, 0), None);
    }

    #[test]
    fn syn_dropped_fails_closed_until_an_all_up_frame_resynchronizes() {
        let mut decoder = BezelDecoder::new();
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, 7);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, -30);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, 189);
        decoder.ingest(EV_SYN, SYN_REPORT, 0);
        decoder.ingest(EV_SYN, SYN_DROPPED, 0);
        // The first report after SYN_DROPPED is only an evdev fence. A
        // BTN_TOUCH=0 on a following complete frame is the all-up evidence.
        assert_eq!(decoder.ingest(EV_SYN, SYN_REPORT, 0), None);
        decoder.ingest(EV_KEY, BTN_TOUCH, 0);
        assert_eq!(decoder.ingest(EV_SYN, SYN_REPORT, 0), None);
        assert_eq!(
            press(&mut decoder, -30, 189),
            Some(BezelPress { x: -30, y: 189 })
        );
    }

    #[test]
    fn a_large_bezel_slide_is_not_a_press() {
        let mut decoder = BezelDecoder::new();
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, 7);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, -30);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, 189);
        decoder.ingest(EV_SYN, SYN_REPORT, 0);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, 206);
        decoder.ingest(EV_SYN, SYN_REPORT, 0);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, -1);
        assert_eq!(decoder.ingest(EV_SYN, SYN_REPORT, 0), None);
    }

    #[test]
    fn pointer_emulation_axes_do_not_overwrite_the_type_b_contact() {
        let mut decoder = BezelDecoder::new();
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, 7);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, -30);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, 189);
        decoder.ingest(EV_ABS, ABS_X, 40);
        decoder.ingest(EV_ABS, ABS_Y, 500);
        decoder.ingest(EV_SYN, SYN_REPORT, 0);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, -1);
        assert_eq!(
            decoder.ingest(EV_SYN, SYN_REPORT, 0),
            Some(BezelPress { x: -30, y: 189 })
        );
    }

    #[test]
    fn stale_axes_before_a_tracking_id_never_make_a_press() {
        let mut decoder = BezelDecoder::new();
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, -30);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, 189);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, 7);
        decoder.ingest(EV_SYN, SYN_REPORT, 0);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, -1);
        assert_eq!(decoder.ingest(EV_SYN, SYN_REPORT, 0), None);
    }

    #[test]
    fn replacing_a_live_slot_fails_closed() {
        let mut decoder = BezelDecoder::new();
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, 7);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, -30);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, 189);
        decoder.ingest(EV_SYN, SYN_REPORT, 0);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, 8);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, -30);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, 189);
        decoder.ingest(EV_SYN, SYN_REPORT, 0);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, -1);
        assert_eq!(decoder.ingest(EV_SYN, SYN_REPORT, 0), None);
    }

    #[test]
    fn slot_reuse_before_the_release_frame_cannot_emit_stale_coordinates() {
        let mut decoder = BezelDecoder::new();
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, 7);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, -30);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, 189);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, -1);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, 8);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, -30);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, -1);
        assert_eq!(decoder.ingest(EV_SYN, SYN_REPORT, 0), None);
    }

    #[test]
    fn an_invalid_slot_fails_closed() {
        let mut decoder = BezelDecoder::new();
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, 7);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, -30);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, 189);
        decoder.ingest(EV_ABS, ABS_MT_SLOT, 10);
        decoder.ingest(EV_SYN, SYN_REPORT, 0);
        decoder.ingest(EV_ABS, ABS_MT_SLOT, 0);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, -1);
        assert_eq!(decoder.ingest(EV_SYN, SYN_REPORT, 0), None);
    }

    /// Replay one complete type-B Goodix contact from D3-0114's protocol.
    fn press(decoder: &mut BezelDecoder, x: i32, y: i32) -> Option<BezelPress> {
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, 1);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, x);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, y);
        decoder.ingest(EV_SYN, SYN_REPORT, 0);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, -1);
        decoder.ingest(EV_SYN, SYN_REPORT, 0)
    }

    #[test]
    fn a_negative_x_contact_is_reported_once_on_release() {
        let mut decoder = BezelDecoder::new();
        assert_eq!(
            press(&mut decoder, -30, 189),
            Some(BezelPress { x: -30, y: 189 })
        );
    }

    #[test]
    fn an_on_panel_touch_is_never_a_bezel_press() {
        // The whole UI runs on positive X. Reporting these would make every
        // tap a button event.
        let mut decoder = BezelDecoder::new();
        assert_eq!(press(&mut decoder, 640, 360), None);
        assert_eq!(press(&mut decoder, 0, 200), None, "x=0 is on the panel");
    }

    #[test]
    fn nothing_is_reported_while_the_finger_is_still_down() {
        let mut decoder = BezelDecoder::new();
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, 1);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, -30);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, 264);
        assert_eq!(decoder.ingest(EV_SYN, SYN_REPORT, 0), None);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, -1);
        // Only the all-up release frame completes it.
        assert_eq!(
            decoder.ingest(EV_SYN, SYN_REPORT, 0),
            Some(BezelPress { x: -30, y: 264 })
        );
    }

    #[test]
    fn coordinates_arriving_after_tracking_id_still_latch() {
        // Tracking ID starts a lifetime; its first SYN_REPORT, not the event
        // order within that frame, decides which cached/new axes are valid.
        let mut decoder = BezelDecoder::new();
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, 1);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, -34);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, 428);
        decoder.ingest(EV_SYN, SYN_REPORT, 0);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, -1);
        assert_eq!(
            decoder.ingest(EV_SYN, SYN_REPORT, 0),
            Some(BezelPress { x: -34, y: 428 })
        );
    }

    #[test]
    fn repeated_coordinates_do_not_cancel_a_stationary_contact() {
        let mut decoder = BezelDecoder::new();
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, 1);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, -30);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, 189);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_X, -30);
        decoder.ingest(EV_ABS, ABS_MT_POSITION_Y, 189);
        decoder.ingest(EV_SYN, SYN_REPORT, 0);
        decoder.ingest(EV_ABS, ABS_MT_TRACKING_ID, -1);
        assert_eq!(
            decoder.ingest(EV_SYN, SYN_REPORT, 0),
            Some(BezelPress { x: -30, y: 189 })
        );
    }

    #[test]
    fn the_five_measured_bands_decode_in_physical_order() {
        // The D3-0114 capture, replayed. This asserts the DECODER, not a
        // mapping: it proves every measured contact survives decoding, and
        // that their Y values stay ordered and separated enough to be
        // distinguishable later.
        let capture = [(-25, 94), (-33, 192), (-34, 279), (-37, 361), (-39, 438)];
        let mut decoder = BezelDecoder::new();
        let mut seen = Vec::new();
        for (x, y) in capture {
            seen.push(press(&mut decoder, x, y).expect("every measured contact decodes"));
        }
        assert_eq!(seen.len(), 5);
        for pair in seen.windows(2) {
            assert!(
                pair[1].y - pair[0].y > 50,
                "bands must stay separable: {pair:?}"
            );
        }
    }

    #[test]
    fn an_unrelated_event_stream_produces_nothing() {
        // A negative control: key traffic from some other device must not be
        // mistaken for bezel activity.
        let mut decoder = BezelDecoder::new();
        for code in [0x1c_u16, 0x66, 0x73, 0x72] {
            assert_eq!(decoder.ingest(EV_KEY, code, 1), None);
            assert_eq!(decoder.ingest(EV_KEY, code, 0), None);
        }
    }
}
