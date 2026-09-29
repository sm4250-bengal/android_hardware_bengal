// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Apply a [`Profile`] to a checksum-valid frame; keep the latest values.

use std::collections::BTreeMap;

use super::envelope::Frame;
use super::profile::{Endian, Field, Keys, Profile};

/// One decoded quantity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signal {
    /// Message group (`vehicle`, `doors`, ...).
    pub group: String,
    /// Field name (`battery_voltage`, ...).
    pub name: String,
    /// Raw integer as extracted (before scaling).
    pub raw: i64,
    /// Physical value = raw × scale_num / scale_den.
    pub scale_num: i64,
    /// See `scale_num`.
    pub scale_den: i64,
    /// Unit label.
    pub unit: String,
    /// Whether an in-car capture confirmed this decode.
    pub verified: bool,
}

impl Signal {
    /// The physical value as exact decimal text (`"12.34"`), no floats.
    pub fn scaled_text(&self) -> String {
        format_scaled(self.raw, self.scale_num, self.scale_den)
    }
}

/// A steering-wheel key frame as the profile reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    /// Box key code.
    pub code: u8,
    /// Box state byte.
    pub state: u8,
    /// Profile's name for the state, if listed.
    pub state_name: Option<String>,
    /// Profile's action for the code, if mapped.
    pub action: Option<String>,
}

/// Everything one frame yielded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Decoded {
    /// Field values, in profile order.
    pub signals: Vec<Signal>,
    /// A key event, when the frame is the profile's key frame.
    pub key: Option<KeyEvent>,
    /// Groups touched (for the capture's `semantic` label).
    pub groups: Vec<String>,
}

/// Decode `frame` with `profile`. A frame no message covers yields an empty
/// [`Decoded`] — that is data, not an error: the capture keeps the raw bytes.
pub fn decode(profile: &Profile, frame: &Frame) -> Decoded {
    let mut out = Decoded::default();
    for message in profile.messages_for(frame.message_type, &frame.data) {
        out.groups.push(message.name.clone());
        for field in &message.fields {
            if let Some(raw) = extract(field, &frame.data) {
                out.signals.push(Signal {
                    group: message.name.clone(),
                    name: field.name.clone(),
                    raw,
                    scale_num: field.scale_num,
                    scale_den: field.scale_den,
                    unit: field.unit.clone(),
                    verified: field.verified,
                });
            }
        }
    }
    if let Some(keys) = &profile.keys {
        if frame.message_type == keys.message_type && frame.data.len() >= 2 {
            out.key = Some(key_event(keys, frame.data[0], frame.data[1]));
            out.groups.push("key".to_owned());
        }
    }
    out
}

fn key_event(keys: &Keys, code: u8, state: u8) -> KeyEvent {
    KeyEvent {
        code,
        state,
        state_name: keys
            .states
            .iter()
            .find(|(_, v)| *v == state)
            .map(|(n, _)| n.clone()),
        action: keys
            .map
            .iter()
            .find(|m| m.code == code)
            .map(|m| m.action.clone()),
    }
}

/// Pull one field out of DATA. `None` when the frame is too short (the
/// profile's `min_len` guard normally prevents that).
pub fn extract(field: &Field, data: &[u8]) -> Option<i64> {
    let bytes = data.get(field.offset..field.offset + field.width)?;
    let mut value: u64 = 0;
    match field.endian {
        Endian::Big => {
            for b in bytes {
                value = (value << 8) | u64::from(*b);
            }
        }
        Endian::Little => {
            for b in bytes.iter().rev() {
                value = (value << 8) | u64::from(*b);
            }
        }
    }
    if let Some(bit) = field.bit {
        return Some(((value >> bit) & 1) as i64);
    }
    if field.signed {
        let bits = field.width * 8;
        let sign = 1u64 << (bits - 1);
        if value & sign != 0 {
            return Some(value as i64 - (1i64 << bits));
        }
    }
    Some(value as i64)
}

/// Exact decimal rendering of `raw × num / den`, up to six fraction digits,
/// trailing zeros trimmed. Integer arithmetic only.
pub fn format_scaled(raw: i64, num: i64, den: i64) -> String {
    if den == 0 {
        return format!("{raw}");
    }
    let numerator = i128::from(raw) * i128::from(num);
    let den = i128::from(den);
    let negative = (numerator < 0) != (den < 0) && numerator != 0;
    let (n, d) = (numerator.abs(), den.abs());
    let whole = n / d;
    let mut rem = n % d;
    let mut frac = String::new();
    for _ in 0..6 {
        if rem == 0 {
            break;
        }
        rem *= 10;
        frac.push(char::from(b'0' + (rem / d) as u8));
        rem %= d;
    }
    let mut text = String::new();
    if negative {
        text.push('-');
    }
    text.push_str(&whole.to_string());
    if !frac.is_empty() {
        text.push('.');
        text.push_str(&frac);
    }
    text
}

/// Latest value per field, with when it arrived.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    values: BTreeMap<String, (Signal, u64)>,
    last_key: Option<(KeyEvent, u64)>,
}

impl Snapshot {
    /// Fold one frame's result in.
    pub fn update(&mut self, decoded: &Decoded, now_ms: u64) {
        for signal in &decoded.signals {
            self.values
                .insert(signal.name.clone(), (signal.clone(), now_ms));
        }
        if let Some(key) = &decoded.key {
            self.last_key = Some((key.clone(), now_ms));
        }
    }

    /// A field's latest value and age.
    pub fn get(&self, name: &str) -> Option<(&Signal, u64)> {
        self.values.get(name).map(|(s, t)| (s, *t))
    }

    /// Every field, sorted by name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Signal, u64)> {
        self.values.iter().map(|(k, (s, t))| (k.as_str(), s, *t))
    }

    /// The most recent key frame.
    pub fn last_key(&self) -> Option<(&KeyEvent, u64)> {
        self.last_key.as_ref().map(|(k, t)| (k, *t))
    }

    /// How many fields have ever been seen.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Nothing decoded yet.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::super::envelope::{encode_frame, Item, Parser};
    use super::super::profile::Profile;
    use super::*;

    fn shipped() -> Profile {
        Profile::parse(include_str!("../../can/profiles/xinpu-vw-pq.toml")).unwrap()
    }

    fn frame(message_type: u8, data: &[u8]) -> Frame {
        let items = Parser::new().push(&encode_frame(message_type, data));
        let Item::Frame(frame) = items.into_iter().next().unwrap() else {
            panic!()
        };
        frame
    }

    #[test]
    fn vehicle_status_scales_as_the_oem_plugin_would() {
        // Synthetic: 0x41/0x02, rpm 0x0bb8 = 3000, speed 0x1388 = 50.00 km/h,
        // voltage 0x0569 = 13.85 V, temp 0xffec = -2.0 degC, odo 0x01e240 =
        // 123456 km, fuel 0x2a = 42 l.  Not a capture.
        let data = [
            0x02, 0x0b, 0xb8, 0x13, 0x88, 0x05, 0x69, 0xff, 0xec, 0x01, 0xe2, 0x40, 0x2a,
        ];
        let d = decode(&shipped(), &frame(0x41, &data));
        let by_name: BTreeMap<_, _> = d.signals.iter().map(|s| (s.name.as_str(), s)).collect();
        assert_eq!(by_name["engine_rpm"].scaled_text(), "3000");
        assert_eq!(by_name["speed"].scaled_text(), "50");
        assert_eq!(by_name["battery_voltage"].scaled_text(), "13.85");
        assert_eq!(by_name["outside_temperature"].scaled_text(), "-2");
        assert_eq!(by_name["odometer"].scaled_text(), "123456");
        assert_eq!(by_name["fuel"].scaled_text(), "42");
        assert!(d.signals.iter().all(|s| !s.verified));
        assert_eq!(d.groups, vec!["vehicle".to_owned()]);
    }

    #[test]
    fn a_short_vehicle_frame_decodes_nothing() {
        let d = decode(&shipped(), &frame(0x41, &[0x02; 12]));
        assert!(d.signals.is_empty());
    }

    #[test]
    fn door_bits_come_out_one_flag_each() {
        let d = decode(&shipped(), &frame(0x41, &[0x01, 0b1_0101]));
        let flags: Vec<(&str, i64)> = d.signals.iter().map(|s| (s.name.as_str(), s.raw)).collect();
        assert_eq!(
            flags,
            vec![
                ("door_front_left", 1),
                ("door_front_right", 0),
                ("door_rear_left", 1),
                ("door_rear_right", 0),
                ("trunk", 1),
            ]
        );
    }

    #[test]
    fn steering_angle_is_little_endian_raw() {
        let d = decode(&shipped(), &frame(0x26, &[0x10, 0x27]));
        assert_eq!(d.signals[0].raw, 10000);
    }

    #[test]
    fn key_frame_maps_code_and_state_names() {
        let d = decode(&shipped(), &frame(0x20, &[0x03, 0x01]));
        let key = d.key.expect("key frame");
        assert_eq!(key.action.as_deref(), Some("NEXT"));
        assert_eq!(key.state_name.as_deref(), Some("down"));
        let d = decode(&shipped(), &frame(0x20, &[0x63, 0x09]));
        let key = d.key.unwrap();
        assert_eq!(key.action, None, "an unmapped code is reported, not invented");
        assert_eq!(key.state_name, None);
    }

    #[test]
    fn radar_0x22_is_rear_and_0x23_is_front_as_the_plugin_maps_them() {
        // Synthetic `2E 22 04 00 03 05 00 D1`: rear left-mid 3, right-mid 5.
        let d = decode(&shipped(), &frame(0x22, &[0x00, 0x03, 0x05, 0x00]));
        assert_eq!(d.groups, vec!["radar_rear".to_owned()]);
        let by_name: BTreeMap<_, _> = d.signals.iter().map(|s| (s.name.as_str(), s.raw)).collect();
        assert_eq!(by_name["radar_rear_left_mid"], 3);
        assert_eq!(by_name["radar_rear_right_mid"], 5);
        let d = decode(&shipped(), &frame(0x23, &[0x01, 0x00, 0x00, 0x0a]));
        assert_eq!(d.groups, vec!["radar_front".to_owned()]);
        assert_eq!(d.signals[0].name, "radar_front_left");
        assert_eq!(d.signals[3].raw, 10);
    }

    #[test]
    fn key_states_are_the_plugins_up0_down1_long2() {
        let name = |state| decode(&shipped(), &frame(0x20, &[0x01, state])).key.unwrap().state_name;
        assert_eq!(name(0x00).as_deref(), Some("up"));
        assert_eq!(name(0x01).as_deref(), Some("down"));
        assert_eq!(name(0x02).as_deref(), Some("long"));
        assert_eq!(name(0x03), None, "no state 3 in this box's table");
    }

    #[test]
    fn unknown_type_yields_empty_decoded_not_error() {
        let d = decode(&shipped(), &frame(0x7e, &[1, 2, 3]));
        assert!(d.signals.is_empty() && d.key.is_none() && d.groups.is_empty());
    }

    #[test]
    fn format_scaled_is_exact_integer_arithmetic() {
        assert_eq!(format_scaled(1385, 1, 100), "13.85");
        assert_eq!(format_scaled(-20, 1, 10), "-2");
        assert_eq!(format_scaled(5, 1, 100), "0.05");
        assert_eq!(format_scaled(1, 1, 3), "0.333333");
        assert_eq!(format_scaled(7, 3, 1), "21");
        assert_eq!(format_scaled(0, 1, 100), "0");
    }

    #[test]
    fn snapshot_keeps_latest_and_age() {
        let mut snap = Snapshot::default();
        let p = shipped();
        snap.update(&decode(&p, &frame(0x41, &[0x01, 0x01])), 1000);
        snap.update(&decode(&p, &frame(0x41, &[0x01, 0x00])), 2000);
        let (fl, t) = snap.get("door_front_left").unwrap();
        assert_eq!((fl.raw, t), (0, 2000));
        snap.update(&decode(&p, &frame(0x20, &[0x01, 0x02])), 2500);
        assert_eq!(snap.last_key().unwrap().0.action.as_deref(), Some("VOLUME_UP"));
        assert_eq!(snap.len(), 5);
    }
}
