// SPDX-License-Identifier: Apache-2.0
//! Compiled, model-bound controls. No file descriptor or arbitrary TX interface.

use super::capture::{json_object, Json};
use super::envelope::Framing;
use super::profile::{parse_toml, Value};
use std::sync::OnceLock;

pub(crate) const MAX_CONTROLS: usize = 128;
const MAX_PROFILES: usize = 256;
pub(crate) const QUEUE_CAPACITY: usize = 8;
pub(crate) const TTL_MS: u64 = 5_000;
pub(crate) const INTERVAL_MS: u64 = 250;

#[derive(Debug)]
pub(crate) struct ControlProfile {
    pub id: String,
    pub display: String,
    pub selection_id: String,
    pub maker_id: String,
    pub brand_id: String,
    pub baud: u32,
    pub envelope: String,
    pub controls: Vec<Control>,
}

#[derive(Debug)]
pub(crate) struct Control {
    pub id: String,
    label: String,
    group: String,
    kind: String,
    min: u8,
    max: u8,
    choices: Vec<(u8, String)>,
    framing: Framing,
    tx_type: Option<u8>,
    tx_command: Option<u8>,
    options: Vec<(u8, u8, Vec<u8>)>,
    rx: Option<Observation>,
}

#[derive(Debug)]
struct Observation {
    message_type: u8,
    length: u8,
    index: u8,
    start: u8,
    end: u8,
    equals: Option<u8>,
    min_length: bool,
    matches: Option<(u8, u8)>,
}

/// Parse once; an invalid bundle enables no generated model or command.
pub(crate) fn bundle() -> &'static Result<Vec<ControlProfile>, String> {
    static BUNDLE: OnceLock<Result<Vec<ControlProfile>, String>> = OnceLock::new();
    BUNDLE.get_or_init(|| {
        // Test-only candidate activation exercises selection/monitor/runtime
        // together without modifying another worker's generated source files.
        #[cfg(test)]
        if let Some(path) = std::env::var_os("BENGAL_CAN_TEST_BUNDLE") {
            let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            return parse_bundle(&text);
        }
        parse_bundle(include_str!("../../can/generated/controls.toml"))
    })
}

pub(crate) fn profile(id: &str) -> Option<&'static ControlProfile> {
    bundle().as_ref().ok()?.iter().find(|p| p.id == id)
}

/// Match every part of the source-linked compiled identity, never just a marque.
pub(crate) fn binding(id: &str, maker: &str, brand: &str) -> Option<&'static ControlProfile> {
    bundle().as_ref().ok()?.iter().find(|p| p.selection_id == id && p.maker_id == maker && p.brand_id == brand)
}

fn string(value: &Value, key: &str) -> Result<String, String> {
    let s = value.get(key).and_then(Value::as_str).ok_or_else(|| format!("missing string {key}"))?;
    if s.is_empty() || s.len() > 128 || s.chars().any(char::is_control) {
        return Err(format!("invalid bounded string {key}"));
    }
    Ok(s.to_owned())
}

fn integer(value: &Value, key: &str) -> Result<i64, String> {
    value.get(key).and_then(Value::as_int).ok_or_else(|| format!("missing integer {key}"))
}

fn byte(value: &Value, key: &str) -> Result<u8, String> {
    u8::try_from(integer(value, key)?).map_err(|_| format!("{key} outside byte range"))
}

fn array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value], String> {
    value.get(key).and_then(Value::as_array).ok_or_else(|| format!("missing array {key}"))
}

pub(crate) fn parse_bundle(text: &str) -> Result<Vec<ControlProfile>, String> {
    if text.len() > 4 * 1024 * 1024 { return Err("control bundle too large".into()); }
    let root = parse_toml(text).map_err(|e| e.to_string())?;
    if integer(&root, "schema")? != 1 { return Err("unknown control schema".into()); }
    let entries = array(&root, "profiles")?;
    if entries.is_empty() || entries.len() > MAX_PROFILES { return Err("profile count outside bound".into()); }
    let mut profiles: Vec<ControlProfile> = Vec::new();
    for entry in entries {
        let id = string(entry, "id")?;
        let selection_id = string(entry, "selection_id")?;
        let maker = string(entry, "maker_id")?;
        let brand = string(entry, "brand_id")?;
        if [&selection_id, &maker, &brand].iter().any(|s| !s.bytes().all(|b| b.is_ascii_digit())) {
            return Err("invalid compiled box/brand binding".into());
        }
        // Preserve the two established IDs; new IDs name their exact OEM model.
        let expected_id = match (selection_id.as_str(), maker.as_str(), brand.as_str()) {
            ("2009002", "2", "9") => "xinpu-vw-pq".into(),
            ("1001001", "1", "1") => "raise-vw-pq".into(),
            _ => format!("oem-vag-{selection_id}"),
        };
        let envelope = string(entry, "envelope")?;
        let framing = Framing::parse(&envelope).ok_or("unimplemented framing")?;
        if framing == Framing::XinpuAa55PackedRx
            && (selection_id.as_str(), maker.as_str(), brand.as_str()) != ("2001001", "2", "1") {
            return Err("packed RX framing requires exact XinPu Audi binding".into());
        }
        let baud = u32::try_from(integer(entry, "baud")?).map_err(|_| "invalid baud")?;
        if id != expected_id || baud != 38400 {
            return Err("unimplemented model/framing binding".into());
        }
        if profiles.iter().any(|p| p.id == id || p.selection_id == selection_id) { return Err("duplicate profile".into()); }
        let entries = array(entry, "controls")?;
        if entries.is_empty() || entries.len() > MAX_CONTROLS { return Err("control count outside bound".into()); }
        let mut controls: Vec<Control> = Vec::new();
        for entry in entries {
            let id = string(entry, "id")?;
            if !id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-')) || controls.iter().any(|c| c.id == id) {
                return Err("invalid or duplicate control ID".into());
            }
            let kind = string(entry, "kind")?;
            if !framing.supports_tx() && kind != "readout" { return Err("receive-only framing forbids write controls".into()); }
            let mut choices = Vec::new();
            let mut options = Vec::new();
            let (min, max) = match kind.as_str() {
                "range" => (byte(entry, "min")?, byte(entry, "max")?),
                "choice" => {
                    if entry.get("options").is_some() {
                        if entry.get("choices").is_some() || entry.get("choice_labels").is_some()
                            || entry.get("tx_type").is_some() || entry.get("tx_command").is_some() {
                            return Err("mixed option and scalar TX definitions".into());
                        }
                        let values = array(entry, "options")?;
                        if values.is_empty() || values.len() > 256 { return Err("invalid options".into()); }
                        for option in values {
                            let value = byte(option, "value")?;
                            if choices.iter().any(|(v, _)| *v == value) { return Err("duplicate choice".into()); }
                            let payload = array(option, "payload")?;
                            if payload.is_empty() || payload.len() > 64 { return Err("option payload outside bound".into()); }
                            let payload = payload.iter().map(|v| v.as_int().and_then(|n| u8::try_from(n).ok()).ok_or("invalid payload byte"))
                                .collect::<Result<Vec<_>, _>>()?;
                            choices.push((value, string(option, "label")?));
                            options.push((value, byte(option, "tx_type")?, payload));
                        }
                    } else {
                        let values = array(entry, "choices")?;
                        let labels = array(entry, "choice_labels")?;
                        if values.is_empty() || values.len() > 256 || values.len() != labels.len() { return Err("invalid choices".into()); }
                        for (value, label) in values.iter().zip(labels) {
                            let value = value.as_int().and_then(|n| u8::try_from(n).ok()).ok_or("choice outside byte range")?;
                            let label = label.as_str().filter(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control)).ok_or("invalid choice label")?;
                            if choices.iter().any(|(v, _)| *v == value) { return Err("duplicate choice".into()); }
                            choices.push((value, label.to_owned()));
                        }
                    }
                    (*choices.iter().map(|(v, _)| v).min().unwrap(), *choices.iter().map(|(v, _)| v).max().unwrap())
                }
                "readout" => (0, if entry.get("rx_equals").is_some() { 1 } else { 255 }),
                _ => return Err("unimplemented control kind".into()),
            };
            if min > max { return Err("reversed range".into()); }
            if kind != "choice" && entry.get("options").is_some() { return Err("options require choice kind".into()); }
            let rx_keys = ["rx_type", "rx_length", "rx_index", "rx_start_bit", "rx_end_bit", "rx_equals", "rx_min_length", "rx_match_index", "rx_match_value"];
            let rx = if rx_keys.iter().any(|key| entry.get(key).is_some()) {
                let observation = Observation { message_type: byte(entry, "rx_type")?, length: byte(entry, "rx_length")?, index: byte(entry, "rx_index")?, start: byte(entry, "rx_start_bit")?, end: byte(entry, "rx_end_bit")?,
                    equals: entry.get("rx_equals").map(|_| byte(entry, "rx_equals")).transpose()?,
                    min_length: entry.get("rx_min_length").map(|v| v.as_bool().ok_or("invalid rx_min_length")).transpose()?.unwrap_or(false),
                    matches: if entry.get("rx_match_index").is_some() || entry.get("rx_match_value").is_some() {
                        Some((byte(entry, "rx_match_index")?, byte(entry, "rx_match_value")?))
                    } else { None } };
                if observation.index >= observation.length || observation.start > observation.end || observation.end > 7 { return Err("invalid RX bit range/index".into()); }
                if observation.matches.is_some_and(|(index, _)| index >= observation.length) { return Err("RX predicate index outside length".into()); }
                if framing == Framing::XinpuAa55PackedRx && (observation.message_type > 7 || observation.length > 15) {
                    return Err("packed RX type/length outside bitfield bounds".into());
                }
                Some(observation)
            } else { None };
            let tx_type = if kind == "readout" {
                if rx.is_none() || ["tx_type", "tx_command", "choices", "choice_labels", "options"].iter().any(|key| entry.get(key).is_some()) {
                    return Err("readout requires RX and forbids TX".into());
                }
                None
            } else if options.is_empty() { Some(byte(entry, "tx_type")?) } else { None };
            controls.push(Control { id, label: string(entry, "label")?, group: string(entry, "group")?, kind, min, max, choices,
                framing, tx_type, options, tx_command: entry.get("tx_command").map(|_| byte(entry, "tx_command")).transpose()?, rx });
        }
        profiles.push(ControlProfile { id, display: string(entry, "display")?, selection_id, maker_id: maker, brand_id: brand, baud, envelope, controls });
    }
    // Enforce the actual escaped reply bound, including worst-case observed values.
    for profile in &profiles {
        let bytes: usize = profile.controls.iter().map(|c| c.json(Some((255, 0)), u64::MAX, "profile changed or unavailable").len() + 1).sum();
        if bytes > 60 * 1024 { return Err("capabilities exceed JSON bound".into()); }
    }
    Ok(profiles)
}

impl Control {
    pub(crate) fn writable(&self) -> bool { self.kind != "readout" }

    pub(crate) fn accepts(&self, value: i32) -> bool {
        self.writable() && value >= i32::from(self.min) && value <= i32::from(self.max)
            && (self.kind == "range" || self.choices.iter().any(|(v, _)| i32::from(*v) == value))
    }

    pub(crate) fn frame(&self, value: i32) -> Result<Vec<u8>, String> {
        if !self.accepts(value) { return Err("control value outside allowed range/choices".into()); }
        if let Some((_, message_type, data)) = self.options.iter().find(|(v, _, _)| i32::from(*v) == value) {
            return self.framing.encode(*message_type, data);
        }
        let data = match self.tx_command { Some(command) => vec![command, value as u8], None => vec![value as u8] };
        self.framing.encode(self.tx_type.ok_or("control is read-only")?, &data)
    }

    pub(crate) fn observe(&self, message_type: u8, data: &[u8]) -> Option<i32> {
        let rx = self.rx.as_ref()?;
        if rx.message_type != message_type || data.len() < usize::from(rx.length)
            || (!rx.min_length && data.len() != usize::from(rx.length)) { return None; }
        if rx.matches.is_some_and(|(index, expected)| data[usize::from(index)] != expected) { return None; }
        let mask = ((1_u16 << (rx.end - rx.start + 1)) - 1) as u8;
        let value = (data[usize::from(rx.index)] >> rx.start) & mask;
        Some(rx.equals.map_or(i32::from(value), |expected| i32::from(value == expected)))
    }

    pub(crate) fn json(&self, observed: Option<(i32, u64)>, now: u64, gate_reason: &str) -> String {
        // The top-level reply retains the precise gate reason once. Repeating
        // an I/O error for every control must not defeat the bundle JSON bound.
        let reason = if !self.writable() { "Receive-only observation" }
            else if gate_reason.is_empty() { "" }
            else { "Vehicle controls unavailable; see profile reason" };
        let choices = self.choices.iter().map(|(v, label)| json_object(&[("value", Json::Int(i64::from(*v))), ("label", Json::Str(label.clone()))])).collect::<Vec<_>>().join(",");
        json_object(&[("id", Json::Str(self.id.clone())), ("label", Json::Str(self.label.clone())), ("group", Json::Str(self.group.clone())),
            ("kind", Json::Str(self.kind.clone())), ("min", Json::Int(i64::from(self.min))), ("max", Json::Int(i64::from(self.max))),
            ("writable", Json::Bool(self.writable() && gate_reason.is_empty())),
            ("reason", Json::Str(reason.into())),
            ("choices", Json::Raw(format!("[{choices}]"))), ("value", observed.map_or(Json::Raw("null".into()), |(v, _)| Json::Int(i64::from(v)))),
            ("age_ms", Json::Int(observed.map_or(-1, |(_, t)| now.saturating_sub(t).min(i64::MAX as u64) as i64))), ("verified", Json::Bool(false))])
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PendingControl {
    pub request_id: u64,
    pub control_id: String,
    pub value: i32,
    pub raw: Vec<u8>,
    pub session: String,
    pub revision: u64,
    pub expires: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct ControlEvent {
    pub event: &'static str,
    pub pairs: Vec<(&'static str, Json)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compiled_bundle_is_bounded_and_commands_are_typed() {
        let profiles = bundle().as_ref().unwrap();
        assert!((2..=MAX_PROFILES).contains(&profiles.len()));
        for p in profiles {
            assert_eq!(binding(&p.selection_id, &p.maker_id, &p.brand_id).unwrap().id, p.id);
            assert!(binding(&p.selection_id, "wrong", &p.brand_id).is_none());
            assert!(binding(&p.selection_id, &p.maker_id, "wrong").is_none());
            for c in &p.controls {
                assert!(c.frame(-1).is_err());
                assert!(c.frame(256).is_err());
                assert_eq!(c.frame(i32::from(c.min)).is_ok(), c.writable());
            }
        }
        for id in ["amp.volume", "amp.balance", "amp.fader", "amp.bass", "amp.mid", "amp.treble", "amp.speed"] {
            assert!(profile("xinpu-vw-pq").unwrap().controls.iter().any(|c| c.id == id && c.writable()));
        }
        for id in ["parking.mode", "parking.radar_sound"] {
            assert!(profile("raise-vw-pq").unwrap().controls.iter().any(|c| c.id == id));
        }
        assert!(binding("1001001", "2", "1").is_none());
        assert!(binding("1001999", "1", "1").is_none());
    }
    #[test]
    fn invalid_bundle_is_rejected_whole() {
        let source = include_str!("../../can/generated/controls.toml");
        for changed in [source.replace("schema = 1", "schema = 2"), source.replace("baud = 38400", "baud = 9600"), source.replace("raise-2e", "hiworld"), source.replace("tx_type = 160", "tx_type = 256")] {
            assert_ne!(source, changed);
            assert!(parse_bundle(&changed).is_err());
        }
        for changed in [source.replace("maker_id = \"1\"", "maker_id = \"2\""), source.replace("choices = [0, 1]", "choices = [0, 256]"), source.replace("rx_end_bit = 7", "rx_end_bit = 8"), source.replace("rx_index = 1", "rx_index = 8"), source.replace("max = 30", "max = -1")] {
            assert_ne!(source, changed);
            assert!(parse_bundle(&changed).is_err());
        }
    }

    const GENERIC: &str = r#"
schema = 1
[[profiles]]
id = "oem-vag-7021017"
selection_id = "7021017"
maker_id = "7"
brand_id = "21"
display = "Synthetic OuDi test"
baud = 38400
envelope = "hiworld-5aa5"
[[profiles.controls]]
id = "step"
label = "Step"
group = "Test"
kind = "choice"
[[profiles.controls.options]]
value = 0
label = "Down"
tx_type = 160
payload = [0, 255]
[[profiles.controls.options]]
value = 1
label = "Up"
tx_type = 161
payload = [1, 2, 3]
[[profiles.controls]]
id = "readout"
label = "Received"
group = "Test"
kind = "readout"
rx_type = 39
rx_length = 2
rx_index = 1
rx_start_bit = 0
rx_end_bit = 7
rx_equals = 3
rx_min_length = true
"#;

    #[test]
    fn generic_options_and_readouts_preserve_bytes_and_never_transmit_readouts() {
        let profiles = parse_bundle(GENERIC).unwrap();
        let p = &profiles[0];
        assert_eq!((&p.selection_id[..], &p.maker_id[..], &p.brand_id[..]), ("7021017", "7", "21"));
        let choice = &p.controls[0];
        assert_eq!(choice.frame(0).unwrap(), [0x5a, 0xa5, 2, 160, 0, 255, 160]);
        assert_eq!(choice.frame(1).unwrap(), [0x5a, 0xa5, 3, 161, 1, 2, 3, 169]);
        assert!(choice.frame(2).is_err());
        let readout = &p.controls[1];
        for value in 0..=255 { assert!(readout.frame(value).is_err()); }
        assert_eq!(readout.observe(39, &[0]), None);
        assert_eq!(readout.observe(38, &[0, 3]), None);
        assert_eq!(readout.observe(39, &[0, 3, 9]), Some(1));
        assert_eq!(readout.observe(39, &[0, 2]), Some(0));
        let exact = parse_bundle(&GENERIC.replace("rx_min_length = true", "rx_min_length = false")).unwrap();
        assert_eq!(exact[0].controls[1].observe(39, &[0, 3, 9]), None);
        let json = readout.json(None, 0, "");
        assert!(json.contains("\"writable\":false") && json.contains("\"value\":null") && json.contains("\"max\":1"));
        assert!(choice.json(None, 0, "ACC off or unknown").contains("\"writable\":false"));
    }

    #[test]
    fn generic_bundle_rejects_mixed_tx_bad_options_and_oversize_replies() {
        for changed in [
            GENERIC.replace("kind = \"readout\"", "kind = \"readout\"\ntx_type = 1"),
            GENERIC.replace("rx_type = 39", "tx_type = 39"),
            GENERIC.replace("payload = [0, 255]", "payload = []"),
            GENERIC.replace("payload = [0, 255]", "payload = [256]"),
            GENERIC.replace("value = 1", "value = 0"),
            GENERIC.replace("kind = \"choice\"", "kind = \"choice\"\ntx_type = 1"),
            GENERIC.replace("rx_min_length = true", "rx_min_length = 1"),
        ] { assert!(parse_bundle(&changed).is_err()); }
        let prefix = GENERIC.split("[[profiles.controls]]").next().unwrap();
        let choice = "[[profiles.controls]]\nid = \"large\"\nlabel = \"Large\"\ngroup = \"Test\"\nkind = \"choice\"\n";
        let mut source = format!("{prefix}{choice}");
        for value in 0..=255 {
            source.push_str(&format!("[[profiles.controls.options]]\nvalue = {value}\nlabel = \"Option {value}\"\ntx_type = 1\npayload = [1]\n"));
        }
        assert_eq!(parse_bundle(&source).unwrap()[0].controls[0].choices.len(), 256);
        // Escaped labels cost twice their source string length; reject the
        // whole bundle instead of silently truncating a capabilities reply.
        let large = source.replace("Option ", &"\\\"".repeat(60));
        let controls = large.split_once("[[profiles.controls]]").unwrap().1;
        let huge = format!("{large}[[profiles.controls]]{}", controls.replace("id = \"large\"", "id = \"second\""));
        assert!(parse_bundle(&huge).unwrap_err().contains("JSON bound"));
        assert!(parse_bundle(&" ".repeat(4 * 1024 * 1024 + 1)).unwrap_err().contains("too large"));
    }

    #[test]
    fn profile_count_is_dynamic_but_bounded() {
        let mut source = String::from("schema = 1\n");
        let entry = GENERIC.split_once("[[profiles]]").unwrap().1;
        for id in 1..=MAX_PROFILES {
            source.push_str("[[profiles]]");
            source.push_str(&entry.replace("7021017", &id.to_string()));
        }
        assert_eq!(parse_bundle(&source).unwrap().len(), MAX_PROFILES);
        source.push_str("[[profiles]]");
        source.push_str(&entry.replace("7021017", "257"));
        assert!(parse_bundle(&source).unwrap_err().contains("profile count"));
    }

    #[test]
    fn rx_predicate_requires_exact_subtype_before_observing() {
        let source = format!("{GENERIC}\nrx_match_index = 0\nrx_match_value = 1\n");
        let profiles = parse_bundle(&source).unwrap();
        let control = &profiles[0].controls[1];
        assert_eq!(control.observe(39, &[1, 3]), Some(1));
        assert_eq!(control.observe(39, &[1, 2, 9]), Some(0));
        assert_eq!(control.observe(39, &[2, 3]), None);
        assert_eq!(control.observe(39, &[1]), None);
        for changed in [
            source.replace("rx_match_index = 0", "rx_match_index = 2"),
            source.replace("rx_match_value = 1", "rx_match_value = 256"),
            source.replace("rx_match_value = 1", ""),
            source.replace("rx_match_index = 0", ""),
        ] { assert!(parse_bundle(&changed).is_err()); }
    }

    #[test]
    fn xinpu_packed_bundle_only_allows_source_bound_audi_readouts() {
        let prefix = GENERIC.split("[[profiles.controls]]").next().unwrap()
            .replace("7021017", "2001001").replace("maker_id = \"7\"", "maker_id = \"2\"")
            .replace("brand_id = \"21\"", "brand_id = \"1\"")
            .replace("hiworld-5aa5", "xinpu-aa55-packed-rx");
        let readout = GENERIC.rsplit_once("[[profiles.controls]]").unwrap().1
            .replace("rx_type = 39", "rx_type = 0").replace("rx_length = 2", "rx_length = 6")
            .replace("rx_index = 1", "rx_index = 2").replace("rx_equals = 3", "")
            .replace("rx_min_length = true", "");
        let source = format!("{prefix}[[profiles.controls]]{readout}");
        let profiles = parse_bundle(&source).unwrap();
        let control = &profiles[0].controls[0];
        assert_eq!(control.observe(0, &[0, 0, 7, 0, 0, 0]), Some(7));
        assert_eq!(control.observe(0, &[0, 0, 7]), None);
        for value in 0..=255 { assert!(control.frame(value).is_err()); }
        for changed in [
            source.replace("2001001", "2001002"),
            source.replace("kind = \"readout\"", "kind = \"range\"\nmin = 0\nmax = 1\ntx_type = 1"),
            source.replace("kind = \"readout\"", "kind = \"readout\"\ntx_type = 1"),
            source.replace("rx_type = 0", "rx_type = 8"),
            source.replace("rx_length = 6", "rx_length = 16"),
        ] { assert!(parse_bundle(&changed).is_err()); }
    }

    #[test]
    fn measured_control_count_extension_still_rejects_overflow() {
        let mut source = GENERIC.split("[[profiles.controls]]").next().unwrap().to_owned();
        let readout = GENERIC.rsplit_once("[[profiles.controls]]").unwrap().1;
        for index in 0..MAX_CONTROLS {
            source.push_str("[[profiles.controls]]");
            source.push_str(&readout.replace("id = \"readout\"", &format!("id = \"r{index}\"")));
        }
        assert_eq!(parse_bundle(&source).unwrap()[0].controls.len(), MAX_CONTROLS);
        source.push_str("[[profiles.controls]]");
        source.push_str(readout);
        assert!(parse_bundle(&source).unwrap_err().contains("control count"));
    }

    /// The lead/compiler can exercise a candidate without replacing compiled
    /// production data in this worker's isolated source tree.
    #[test]
    fn external_candidate_bundle_when_supplied() {
        let Some(path) = std::env::var_os("BENGAL_CAN_TEST_BUNDLE") else { return; };
        let profiles = parse_bundle(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut readouts = 0;
        let mut choices = 0;
        let mut max_json = 0;
        for p in &profiles {
            let mut size = 0;
            for c in &p.controls {
                size += c.json(Some((255, 0)), u64::MAX, "blocked").len() + 1;
                if c.writable() {
                    if c.kind == "choice" {
                        for (value, _) in &c.choices { assert!(c.frame(i32::from(*value)).is_ok()); choices += 1; }
                    } else { assert!(c.frame(i32::from(c.min)).is_ok()); }
                } else {
                    readouts += 1;
                    for value in 0..=255 { assert!(c.frame(value).is_err()); }
                }
            }
            max_json = max_json.max(size);
        }
        eprintln!("candidate profiles={} readouts={readouts} choices={choices} max_control_json={max_json}", profiles.len());
    }
}
