use std::collections::BTreeMap;

use bengal_hud::can::decode::{decode, Decoded, Signal};
use bengal_hud::can::envelope::{encode_frame, Frame, Item, Parser};
use bengal_hud::can::profile::Profile;

fn profile() -> Profile {
    Profile::parse(include_str!("../can/profiles/xinpu-vw-pq.toml")).unwrap()
}

fn frame(message_type: u8, data: &[u8]) -> Frame {
    let mut parser = Parser::new();
    let items = parser.push(&encode_frame(message_type, data));
    assert_eq!(items.len(), 1);
    match items.into_iter().next().unwrap() {
        Item::Frame(frame) => frame,
        other => panic!("expected a valid frame, got {other:?}"),
    }
}

fn signal<'a>(decoded: &'a Decoded, name: &str) -> &'a Signal {
    decoded
        .signals
        .iter()
        .find(|signal| signal.name == name)
        .unwrap_or_else(|| panic!("missing signal {name}; got {:?}", decoded.signals))
}

fn assert_signal(decoded: &Decoded, name: &str, raw: i64, value: &str, unit: &str) {
    let field = signal(decoded, name);
    assert_eq!(field.raw, raw, "{name}");
    assert_eq!(field.scaled_text(), value, "{name} scaled value");
    assert_eq!(field.unit, unit, "{name} unit");
    assert!(!field.verified, "{name} must remain unverified");
}

#[test]
fn alerts_decode_positive_zero_and_bit_isolation() {
    let p = profile();

    let both = decode(&p, &frame(0x41, &[0x03, 0xc0]));
    assert_eq!(both.groups, vec!["vehicle_flags"]);
    assert_signal(&both, "flags_raw", 0xc0, "192", "raw");
    assert_signal(&both, "low_fuel", 1, "1", "flag");
    assert_signal(&both, "low_battery", 1, "1", "flag");

    let zero = decode(&p, &frame(0x41, &[0x03, 0x00]));
    assert_signal(&zero, "low_fuel", 0, "0", "flag");
    assert_signal(&zero, "low_battery", 0, "0", "flag");

    let fuel_only = decode(&p, &frame(0x41, &[0x03, 0x80]));
    assert_eq!(signal(&fuel_only, "low_fuel").raw, 1);
    assert_eq!(signal(&fuel_only, "low_battery").raw, 0);

    let battery_only = decode(&p, &frame(0x41, &[0x03, 0x40]));
    assert_eq!(signal(&battery_only, "low_fuel").raw, 0);
    assert_eq!(signal(&battery_only, "low_battery").raw, 1);
}

#[test]
fn tpms_decodes_only_documented_status_bits() {
    let p = profile();
    let d = decode(&p, &frame(0x37, &[0x10, 0xff, 0xff]));
    assert_eq!(d.groups, vec!["tyres"]);
    assert!(d.signals.len() >= 8);
    assert_signal(&d, "tyre_flags_raw0", 0x10, "16", "raw");
    assert_signal(&d, "tyre_flags_raw1", 0xff, "255", "raw");
    assert_signal(&d, "tyre_flags_raw2", 0xff, "255", "raw");
    assert_signal(&d, "detection_fault", 1, "1", "flag");
    for name in [
        "tyre_front_left_abnormal",
        "tyre_front_right_abnormal",
        "tyre_rear_left_abnormal",
        "tyre_rear_right_abnormal",
    ] {
        assert_signal(&d, name, 0, "0", "flag");
    }

    for (bit, name) in [
        (3, "tyre_front_left_abnormal"),
        (2, "tyre_front_right_abnormal"),
        (1, "tyre_rear_left_abnormal"),
        (0, "tyre_rear_right_abnormal"),
    ] {
        let d = decode(&p, &frame(0x37, &[1 << bit, 0, 0]));
        assert_eq!(signal(&d, name).raw, 1, "isolated {name}");
        assert!(d
            .signals
            .iter()
            .filter(|field| !field.name.starts_with("tyre_flags_raw"))
            .filter(|field| field.raw != 0)
            .all(|field| field.name == name));
    }
    for bit in [5, 6, 7] {
        let d = decode(&p, &frame(0x37, &[1 << bit, 0, 0]));
        assert_signal(
            &d,
            "tyre_flags_raw0",
            1 << bit,
            &(1 << bit).to_string(),
            "raw",
        );
        assert!(d
            .signals
            .iter()
            .filter(|field| !field.name.starts_with("tyre_flags_raw"))
            .all(|field| field.raw == 0));
    }
    assert!(d
        .signals
        .iter()
        .filter(|field| field.name != "tyre_flags_raw0"
            && field.name != "tyre_flags_raw1"
            && field.name != "tyre_flags_raw2")
        .all(|field| !field.name.contains("pressure")));
}

#[test]
fn trip_speeds_are_little_endian_hundredths_and_zero_safe() {
    let p = profile();
    let d = decode(&p, &frame(0x7d, &[0x03, 0x34, 0x12, 0x78, 0x56]));
    assert_eq!(d.groups, vec!["trip_speeds"]);
    assert_signal(&d, "instantaneous_speed", 0x1234, "46.6", "km/h");
    assert_signal(&d, "average_speed", 0x5678, "221.36", "km/h");

    let zero = decode(&p, &frame(0x7d, &[0x03, 0, 0, 0, 0]));
    assert_signal(&zero, "instantaneous_speed", 0, "0", "km/h");
    assert_signal(&zero, "average_speed", 0, "0", "km/h");
}

#[test]
fn amplifier_exposes_all_eight_raw_indices_without_gain_claims() {
    let p = profile();
    let d = decode(&p, &frame(0x27, &[2, 7, 8, 9, 10, 11, 12, 13]));
    assert_eq!(d.groups, vec!["amplifier"]);
    assert!(d.signals.len() >= 10);
    assert_signal(&d, "amp_raw0", 2, "2", "raw");
    assert_signal(&d, "amp_raw1", 7, "7", "raw");
    for (index, name) in [
        "oem_amp_type",
        "oem_amp_volume",
        "oem_amp_balance",
        "oem_amp_fader",
        "oem_amp_bass",
        "oem_amp_mid",
        "oem_amp_treble",
        "oem_amp_speed_dependent_volume",
    ]
    .into_iter()
    .enumerate()
    {
        assert_signal(
            &d,
            name,
            [2, 7, 8, 9, 10, 11, 12, 13][index],
            &index_value(index),
            "raw",
        );
    }

    let zero = decode(&p, &frame(0x27, &[0; 8]));
    assert!(zero
        .signals
        .iter()
        .all(|field| field.raw == 0 && field.scaled_text() == "0"));
}

fn index_value(index: usize) -> String {
    [2, 7, 8, 9, 10, 11, 12, 13][index].to_string()
}

#[test]
fn truncated_and_wrong_subtype_vectors_decode_nothing() {
    let p = profile();
    for (message_type, data) in [
        (0x41, vec![0x03]),
        (0x37, vec![0x10, 0xff]),
        (0x27, vec![0; 7]),
        (0x7d, vec![0x03, 0x01, 0x00, 0x02]),
        (0x7d, vec![0x02, 0x01, 0x00, 0x02, 0x00]),
        (0x41, vec![0x02, 0xc0]),
    ] {
        let d = decode(&p, &frame(message_type, &data));
        assert!(
            d.signals.is_empty(),
            "unexpected decode for 0x{message_type:02x}: {d:?}"
        );
    }

    let mut parser = Parser::new();
    let encoded = encode_frame(0x7d, &[0x03, 0x34, 0x12, 0x78, 0x56]);
    assert!(parser.push(&encoded[..encoded.len() - 1]).is_empty());
    assert!(matches!(parser.finish().as_slice(), [Item::Incomplete(_)]));
}

#[test]
fn existing_vehicle_key_and_radar_decodes_remain_unchanged() {
    let p = profile();
    let vehicle = decode(
        &p,
        &frame(
            0x41,
            &[
                0x02, 0x0b, 0xb8, 0x13, 0x88, 0x05, 0x69, 0xff, 0xec, 0x01, 0xe2, 0x40, 0x2a,
            ],
        ),
    );
    assert_signal(&vehicle, "engine_rpm", 3000, "3000", "rpm");
    assert_signal(&vehicle, "speed", 5000, "50", "km/h");
    assert_signal(&vehicle, "battery_voltage", 1385, "13.85", "V");
    assert_signal(&vehicle, "outside_temperature", -20, "-2", "degC");
    assert_signal(&vehicle, "odometer", 123456, "123456", "km");
    assert_signal(&vehicle, "fuel", 42, "42", "l");

    let key = decode(&p, &frame(0x20, &[0x03, 0x01])).key.unwrap();
    assert_eq!(key.action.as_deref(), Some("NEXT"));
    assert_eq!(key.state_name.as_deref(), Some("down"));

    let rear = decode(&p, &frame(0x22, &[0, 3, 5, 0]));
    let rear_values: BTreeMap<_, _> = rear
        .signals
        .iter()
        .map(|field| (field.name.as_str(), field.raw))
        .collect();
    assert_eq!(rear.groups, vec!["radar_rear"]);
    assert_eq!(rear_values["radar_rear_left_mid"], 3);
    assert_eq!(rear_values["radar_rear_right_mid"], 5);

    let front = decode(&p, &frame(0x23, &[1, 0, 0, 10]));
    assert_eq!(front.groups, vec!["radar_front"]);
    assert_eq!(front.signals[0].name, "radar_front_left");
    assert_eq!(front.signals[3].raw, 10);
}
