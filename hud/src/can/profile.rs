// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Per-car decode profiles — data, not code.
//!
//! The OEM shipped 409 plugin APKs (DEX + resources, one per box maker/brand,
//! loaded into a system-UID process with VirtualAPK). We ship a text profile per
//! box protocol + car instead, transcoded from the decompiled plugin: which
//! frame type/subtype carries what, at which offset, with which scale — and a
//! `verified` flag per field that stays `false` until an in-car capture has
//! confirmed it (docs/issues/can/issues/03-hud-can-module.md; the v1 profile
//! is `hud/can/profiles/xinpu-vw-pq.toml`).
//!
//! # The TOML subset (this crate has zero dependencies, so it parses its own)
//!
//! * `key = value` with `value` ∈ `"string"` (escapes `\"` `\\` `\n` `\t`),
//!   integer (decimal, `0x` hex, `_` separators, leading `-`), `true`/`false`,
//!   or a single-line array of those scalars;
//! * `[table]` / `[a.b.c]` headers, and `[[array.of.tables]]` headers whose
//!   dotted parents resolve to the *last* element of an enclosing array of
//!   tables (the one TOML rule a profile needs: `[[messages]]` then
//!   `[[messages.fields]]`);
//! * `#` comments outside strings; blank lines.
//!
//! Anything else — inline tables, multi-line strings, floats, dates — is a
//! parse error naming the line. A profile that does not parse is refused whole;
//! the reader then runs envelope-only (frames counted and captured, nothing
//! decoded) and says so once.

use std::collections::BTreeMap;
use std::fmt;

// ------------------------------------------------------------- TOML subset

/// A parsed TOML-subset value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// A quoted string.
    Str(String),
    /// A 64-bit integer.
    Int(i64),
    /// `true` / `false`.
    Bool(bool),
    /// A single-line array of scalars (or of tables, for `[[...]]`).
    Array(Vec<Value>),
    /// A table.
    Table(BTreeMap<String, Value>),
}

impl Value {
    fn as_table_mut(&mut self) -> Option<&mut BTreeMap<String, Value>> {
        match self {
            Value::Table(t) => Some(t),
            _ => None,
        }
    }

    /// Table lookup helper.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Table(t) => t.get(key),
            _ => None,
        }
    }

    /// Integer accessor.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// String accessor.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Bool accessor.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Array accessor.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a.as_slice()),
            _ => None,
        }
    }
}

/// A parse or validation error, with the 1-based line where known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileError {
    /// 1-based source line; 0 when the error is structural (missing key).
    pub line: usize,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line > 0 {
            write!(f, "line {}: {}", self.line, self.message)
        } else {
            write!(f, "{}", self.message)
        }
    }
}

impl std::error::Error for ProfileError {}

fn err(line: usize, message: impl Into<String>) -> ProfileError {
    ProfileError {
        line,
        message: message.into(),
    }
}

/// Parse the TOML subset into a root table.
pub fn parse_toml(text: &str) -> Result<Value, ProfileError> {
    let mut root = Value::Table(BTreeMap::new());
    // The header path currently open; each element is (key, is_array_element).
    let mut current: Vec<String> = Vec::new();
    for (index, raw_line) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(inner) = line.strip_prefix("[[") {
            let Some(path) = inner.strip_suffix("]]") else {
                return Err(err(line_no, "unterminated [[ header"));
            };
            let keys = split_dotted(path, line_no)?;
            let (parent, last) = keys.split_at(keys.len() - 1);
            let parent_table = descend(&mut root, parent, line_no)?;
            let entry = parent_table
                .entry(last[0].clone())
                .or_insert_with(|| Value::Array(Vec::new()));
            let Value::Array(array) = entry else {
                return Err(err(line_no, format!("`{}` is not an array of tables", last[0])));
            };
            array.push(Value::Table(BTreeMap::new()));
            current = keys;
            continue;
        }
        if let Some(inner) = line.strip_prefix('[') {
            let Some(path) = inner.strip_suffix(']') else {
                return Err(err(line_no, "unterminated [ header"));
            };
            let keys = split_dotted(path, line_no)?;
            descend(&mut root, &keys, line_no)?;
            current = keys;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(err(line_no, "expected `key = value` or a [header]"));
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(err(line_no, format!("bad key `{key}`")));
        }
        let value = parse_value(value.trim(), line_no)?;
        let table = descend(&mut root, &current, line_no)?;
        if table.insert(key.to_owned(), value).is_some() {
            return Err(err(line_no, format!("duplicate key `{key}`")));
        }
    }
    Ok(root)
}

fn strip_comment(line: &str) -> &str {
    let mut in_str = false;
    let mut escaped = false;
    for (i, ch) in line.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
        } else if ch == '"' {
            in_str = true;
        } else if ch == '#' {
            return &line[..i];
        }
    }
    line
}

fn split_dotted(path: &str, line_no: usize) -> Result<Vec<String>, ProfileError> {
    let keys: Vec<String> = path.split('.').map(|k| k.trim().to_owned()).collect();
    if keys.iter().any(|k| k.is_empty()) {
        return Err(err(line_no, format!("bad header `{path}`")));
    }
    Ok(keys)
}

/// Walk `keys` from the root, creating tables as needed; an array of tables
/// on the way resolves to its last element (TOML's rule for dotted headers).
fn descend<'a>(
    root: &'a mut Value,
    keys: &[String],
    line_no: usize,
) -> Result<&'a mut BTreeMap<String, Value>, ProfileError> {
    let mut node = root;
    for key in keys {
        let table = node
            .as_table_mut()
            .ok_or_else(|| err(line_no, format!("`{key}` is inside a non-table")))?;
        let child = table
            .entry(key.clone())
            .or_insert_with(|| Value::Table(BTreeMap::new()));
        node = match child {
            Value::Array(array) => array
                .last_mut()
                .ok_or_else(|| err(line_no, format!("`{key}` is an empty array of tables")))?,
            other => other,
        };
    }
    node.as_table_mut()
        .ok_or_else(|| err(line_no, "header does not name a table"))
}

fn parse_value(text: &str, line_no: usize) -> Result<Value, ProfileError> {
    if text.is_empty() {
        return Err(err(line_no, "missing value"));
    }
    if let Some(rest) = text.strip_prefix('"') {
        return parse_string(rest, line_no).map(|(s, tail)| {
            debug_assert!(tail.trim().is_empty());
            Value::Str(s)
        });
    }
    if text == "true" {
        return Ok(Value::Bool(true));
    }
    if text == "false" {
        return Ok(Value::Bool(false));
    }
    if let Some(inner) = text.strip_prefix('[') {
        let Some(inner) = inner.strip_suffix(']') else {
            return Err(err(line_no, "unterminated array"));
        };
        let mut items = Vec::new();
        let mut rest = inner.trim();
        while !rest.is_empty() {
            let (item, tail) = if let Some(s) = rest.strip_prefix('"') {
                let (s, tail) = parse_string(s, line_no)?;
                (Value::Str(s), tail)
            } else {
                let end = rest.find(',').unwrap_or(rest.len());
                (parse_value(rest[..end].trim(), line_no)?, &rest[end..])
            };
            items.push(item);
            rest = tail.trim_start();
            if let Some(after) = rest.strip_prefix(',') {
                rest = after.trim_start();
            } else if !rest.is_empty() {
                return Err(err(line_no, "expected `,` between array items"));
            }
        }
        return Ok(Value::Array(items));
    }
    parse_int(text)
        .map(Value::Int)
        .ok_or_else(|| err(line_no, format!("unsupported value `{text}` (string, integer, bool or array)")))
}

fn parse_string(after_quote: &str, line_no: usize) -> Result<(String, &str), ProfileError> {
    let mut out = String::new();
    let mut chars = after_quote.char_indices();
    while let Some((i, ch)) = chars.next() {
        match ch {
            '"' => return Ok((out, &after_quote[i + 1..])),
            '\\' => match chars.next() {
                Some((_, '"')) => out.push('"'),
                Some((_, '\\')) => out.push('\\'),
                Some((_, 'n')) => out.push('\n'),
                Some((_, 't')) => out.push('\t'),
                _ => return Err(err(line_no, "unsupported escape in string")),
            },
            other => out.push(other),
        }
    }
    Err(err(line_no, "unterminated string"))
}

fn parse_int(text: &str) -> Option<i64> {
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let body: String = body.chars().filter(|c| *c != '_').collect();
    let value = if let Some(hex) = body.strip_prefix("0x") {
        i64::from_str_radix(hex, 16).ok()?
    } else {
        if body.is_empty() || !body.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        body.parse::<i64>().ok()?
    };
    Some(if negative { -value } else { value })
}

// ---------------------------------------------------------------- profile

/// Byte order of a multi-byte field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endian {
    /// Most significant byte first (the XinPu VW vehicle-status words).
    Big,
    /// Least significant byte first (the XinPu steering angle).
    Little,
}

/// One decoded quantity inside a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    /// Snapshot name, e.g. `battery_voltage`.
    pub name: String,
    /// Offset into the frame's DATA bytes.
    pub offset: usize,
    /// Width in bytes, 1..=4.
    pub width: usize,
    /// Byte order for `width > 1`.
    pub endian: Endian,
    /// Two's-complement interpretation.
    pub signed: bool,
    /// Optional single bit within a 1-byte field (0..=7).
    pub bit: Option<u8>,
    /// Physical value = raw × scale_num / scale_den.
    pub scale_num: i64,
    /// See `scale_num`; never 0.
    pub scale_den: i64,
    /// Unit label for humans and the capture (`V`, `km/h`, `rpm`, `flag`, ...).
    pub unit: String,
    /// `true` only after an in-car capture confirmed the decode.
    pub verified: bool,
}

/// One frame type (and optional first-data-byte subtype) the profile knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// Frame TYPE byte.
    pub message_type: u8,
    /// If set, DATA[0] must equal this for the message to apply.
    pub subtype: Option<u8>,
    /// Snapshot group name, e.g. `vehicle`.
    pub name: String,
    /// The plugin's minimum DATA length guard.
    pub min_len: usize,
    /// Decoded quantities.
    pub fields: Vec<Field>,
}

/// A steering-wheel key code → logical action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyMap {
    /// Box key code (DATA[0] of the key frame).
    pub code: u8,
    /// Logical action name (`VOLUME_UP`, `NEXT`, `PHONE`, ...); no Android
    /// key code here — binding to one is the app's job and a later ticket.
    pub action: String,
}

/// How the profile's key frame is shaped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keys {
    /// Frame TYPE carrying `[code, state]`.
    pub message_type: u8,
    /// Key state values as the box sends them, by name. `can::keys` reads the
    /// names `up`, `down` and `long`; the XinPu box sends 0, 1 and 2.
    pub states: Vec<(String, u8)>,
    /// Code → action.
    pub map: Vec<KeyMap>,
}

/// A whole per-car profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// Short id (`xinpu-vw-pq`).
    pub id: String,
    /// Human name (maker / brand / model as the OEM database spells it).
    pub display: String,
    /// Envelope name; v1 accepts only `raise-2e`.
    pub envelope: String,
    /// UART rate.
    pub baud: u32,
    /// The OEM plugin's request command — documented, never sent.
    pub request: Option<u8>,
    /// Whole-profile verification flag.
    pub verified: bool,
    /// Key frame shape, if the profile has one.
    pub keys: Option<Keys>,
    /// Messages, in file order.
    pub messages: Vec<Message>,
}

/// The only envelope v1 understands.
pub const ENVELOPE_RAISE_2E: &str = "raise-2e";

impl Profile {
    /// Parse and validate profile text.
    pub fn parse(text: &str) -> Result<Profile, ProfileError> {
        let root = parse_toml(text)?;
        let id = req_str(&root, "id")?;
        let display = req_str(&root, "display")?;
        let envelope = req_str(&root, "envelope")?;
        if envelope != ENVELOPE_RAISE_2E {
            return Err(err(0, format!("envelope `{envelope}` unsupported; v1 decodes `{ENVELOPE_RAISE_2E}` only")));
        }
        let baud = req_int(&root, "baud")?;
        if !(1200..=4_000_000).contains(&baud) {
            return Err(err(0, format!("baud {baud} out of range")));
        }
        let request = match root.get("request") {
            Some(v) => Some(byte(v.as_int().ok_or_else(|| err(0, "request must be an integer"))?, "request")?),
            None => None,
        };
        let verified = root.get("verified").and_then(Value::as_bool).unwrap_or(false);
        let keys = match root.get("keys") {
            Some(k) => Some(parse_keys(k)?),
            None => None,
        };
        let mut messages = Vec::new();
        if let Some(list) = root.get("messages") {
            let list = list.as_array().ok_or_else(|| err(0, "`messages` must be [[messages]] tables"))?;
            for (i, m) in list.iter().enumerate() {
                messages.push(parse_message(m, i)?);
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for m in &messages {
            if !seen.insert((m.message_type, m.subtype)) {
                return Err(err(0, format!("duplicate message type 0x{:02x} subtype {:?}", m.message_type, m.subtype)));
            }
        }
        Ok(Profile {
            id,
            display,
            envelope,
            baud: baud as u32,
            request,
            verified,
            keys,
            messages,
        })
    }

    /// Messages applying to a frame: type match, subtype match (if any),
    /// length guard.
    pub fn messages_for(&self, message_type: u8, data: &[u8]) -> Vec<&Message> {
        self.messages
            .iter()
            .filter(|m| m.message_type == message_type)
            .filter(|m| m.subtype.is_none_or(|s| data.first().copied() == Some(s)))
            .filter(|m| data.len() >= m.min_len)
            .collect()
    }
}

fn req_str(t: &Value, key: &str) -> Result<String, ProfileError> {
    t.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| err(0, format!("missing string `{key}`")))
}

fn req_int(t: &Value, key: &str) -> Result<i64, ProfileError> {
    t.get(key)
        .and_then(Value::as_int)
        .ok_or_else(|| err(0, format!("missing integer `{key}`")))
}

fn byte(v: i64, what: &str) -> Result<u8, ProfileError> {
    u8::try_from(v).map_err(|_| err(0, format!("`{what}` = {v} is not a byte")))
}

fn parse_keys(k: &Value) -> Result<Keys, ProfileError> {
    let message_type = byte(req_int(k, "type")?, "keys.type")?;
    let mut states = Vec::new();
    if let Some(Value::Table(t)) = k.get("states") {
        for (name, v) in t {
            states.push((name.clone(), byte(v.as_int().ok_or_else(|| err(0, "keys.states values must be integers"))?, "keys.states")?));
        }
    }
    let mut map = Vec::new();
    if let Some(list) = k.get("map") {
        let list = list.as_array().ok_or_else(|| err(0, "`keys.map` must be [[keys.map]] tables"))?;
        for entry in list {
            let code = byte(req_int(entry, "code")?, "keys.map.code")?;
            let action = req_str(entry, "action")?;
            if map.iter().any(|m: &KeyMap| m.code == code) {
                return Err(err(0, format!("duplicate key code {code}")));
            }
            map.push(KeyMap { code, action });
        }
    }
    Ok(Keys {
        message_type,
        states,
        map,
    })
}

fn parse_message(m: &Value, index: usize) -> Result<Message, ProfileError> {
    let ctx = format!("messages[{index}]");
    let message_type = byte(req_int(m, "type").map_err(|e| err(0, format!("{ctx}: {}", e.message)))?, "type")?;
    let subtype = match m.get("subtype") {
        Some(v) => Some(byte(v.as_int().ok_or_else(|| err(0, format!("{ctx}: subtype must be an integer")))?, "subtype")?),
        None => None,
    };
    let name = req_str(m, "name").map_err(|e| err(0, format!("{ctx}: {}", e.message)))?;
    let min_len = m.get("min_len").and_then(Value::as_int).unwrap_or(0);
    if !(0..=255).contains(&min_len) {
        return Err(err(0, format!("{ctx}: min_len out of range")));
    }
    let mut fields = Vec::new();
    if let Some(list) = m.get("fields") {
        let list = list.as_array().ok_or_else(|| err(0, format!("{ctx}: `fields` must be [[messages.fields]] tables")))?;
        for (fi, f) in list.iter().enumerate() {
            fields.push(parse_field(f, &format!("{ctx}.fields[{fi}]"), min_len as usize)?);
        }
    }
    Ok(Message {
        message_type,
        subtype,
        name,
        min_len: min_len as usize,
        fields,
    })
}

fn parse_field(f: &Value, ctx: &str, min_len: usize) -> Result<Field, ProfileError> {
    let name = req_str(f, "name").map_err(|e| err(0, format!("{ctx}: {}", e.message)))?;
    let offset = req_int(f, "offset").map_err(|e| err(0, format!("{ctx}: {}", e.message)))?;
    let width = f.get("width").and_then(Value::as_int).unwrap_or(1);
    if !(0..=255).contains(&offset) || !(1..=4).contains(&width) {
        return Err(err(0, format!("{ctx}: offset/width out of range")));
    }
    let (offset, width) = (offset as usize, width as usize);
    if offset + width > min_len {
        return Err(err(0, format!("{ctx} `{name}`: offset {offset} + width {width} exceeds min_len {min_len}")));
    }
    let endian = match f.get("endian").and_then(Value::as_str).unwrap_or("be") {
        "be" => Endian::Big,
        "le" => Endian::Little,
        other => return Err(err(0, format!("{ctx}: endian `{other}` (be|le)"))),
    };
    let signed = f.get("signed").and_then(Value::as_bool).unwrap_or(false);
    let bit = match f.get("bit") {
        Some(v) => {
            let b = v.as_int().ok_or_else(|| err(0, format!("{ctx}: bit must be an integer")))?;
            if !(0..=7).contains(&b) || width != 1 {
                return Err(err(0, format!("{ctx}: bit needs width 1 and 0..=7")));
            }
            Some(b as u8)
        }
        None => None,
    };
    let scale_num = f.get("scale_num").and_then(Value::as_int).unwrap_or(1);
    let scale_den = f.get("scale_den").and_then(Value::as_int).unwrap_or(1);
    if scale_den == 0 {
        return Err(err(0, format!("{ctx}: scale_den must not be 0")));
    }
    let unit = f.get("unit").and_then(Value::as_str).unwrap_or("").to_owned();
    let verified = f.get("verified").and_then(Value::as_bool).unwrap_or(false);
    Ok(Field {
        name,
        offset,
        width,
        endian,
        signed,
        bit,
        scale_num,
        scale_den,
        unit,
        verified,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINI: &str = r#"
# a comment
id = "test-profile"
display = "Test / Brand / Model"
envelope = "raise-2e"
baud = 38_400
request = 0x90
verified = false

[keys]
type = 0x20
[keys.states]
up = 0
down = 1
[[keys.map]]
code = 1
action = "VOLUME_UP"
[[keys.map]]
code = 2
action = "VOLUME_DOWN"

[[messages]]
type = 0x41
subtype = 0x02
name = "vehicle"
min_len = 13
[[messages.fields]]
name = "rpm"
offset = 1
width = 2
endian = "be"
unit = "rpm"
[[messages.fields]]
name = "battery_voltage"
offset = 5
width = 2
scale_num = 1
scale_den = 100
unit = "V"
[[messages.fields]]
name = "outside_temp"
offset = 7
width = 2
signed = true
scale_den = 10
unit = "degC"

[[messages]]
type = 0x41
subtype = 0x01
name = "doors"
min_len = 2
[[messages.fields]]
name = "door_fl"
offset = 1
bit = 0
unit = "flag"
"#;

    #[test]
    fn parses_the_subset_including_nested_arrays_of_tables() {
        let p = Profile::parse(MINI).expect("parse");
        assert_eq!(p.id, "test-profile");
        assert_eq!(p.baud, 38400);
        assert_eq!(p.request, Some(0x90));
        let keys = p.keys.as_ref().unwrap();
        assert_eq!(keys.message_type, 0x20);
        assert_eq!(keys.map.len(), 2);
        // Tables are BTreeMaps: names come back sorted, whatever the file order.
        assert_eq!(keys.states, vec![("down".into(), 1), ("up".into(), 0)]);
        assert_eq!(p.messages.len(), 2);
        assert_eq!(p.messages[0].fields.len(), 3);
        assert_eq!(p.messages[0].fields[2].signed, true);
        assert_eq!(p.messages[1].fields[0].bit, Some(0));
    }

    #[test]
    fn messages_for_applies_type_subtype_and_length_guards() {
        let p = Profile::parse(MINI).unwrap();
        assert_eq!(p.messages_for(0x41, &[0x02; 13]).len(), 1);
        assert_eq!(p.messages_for(0x41, &[0x02; 12]).len(), 0, "shorter than min_len");
        assert_eq!(p.messages_for(0x41, &[0x01, 0x00]).len(), 1);
        assert_eq!(p.messages_for(0x42, &[0x02; 13]).len(), 0);
    }

    #[test]
    fn a_field_outside_min_len_is_refused_by_name() {
        let bad = MINI.replace("offset = 5\nwidth = 2\nscale_num = 1", "offset = 12\nwidth = 2\nscale_num = 1");
        let e = Profile::parse(&bad).unwrap_err();
        assert!(e.message.contains("battery_voltage"), "{e}");
    }

    #[test]
    fn unsupported_envelope_and_syntax_are_refused_with_a_line() {
        let e = Profile::parse(&MINI.replace("raise-2e", "hiworld-5aa5")).unwrap_err();
        assert!(e.message.contains("unsupported"), "{e}");
        let e = Profile::parse("id = \"x\"\nbaud = 1.5\n").unwrap_err();
        assert_eq!(e.line, 2, "{e}");
        let e = Profile::parse("id = \"x\"\nid = \"y\"\n").unwrap_err();
        assert!(e.message.contains("duplicate"), "{e}");
        let e = Profile::parse("[[messages]]\ntype = 0x41\n[[messages]]\ntype = 0x41\nname = \"a\"\nid=\"x\"\n").unwrap_err();
        assert!(e.message.contains("missing"), "{e}");
    }

    #[test]
    fn toml_strings_keep_hashes_and_escapes() {
        let v = parse_toml("a = \"x # not a comment \\\" q\" # comment\nb = [1, 0x10, \"s\"]\nc = -7\n").unwrap();
        assert_eq!(v.get("a").unwrap().as_str().unwrap(), "x # not a comment \" q");
        assert_eq!(v.get("b").unwrap().as_array().unwrap().len(), 3);
        assert_eq!(v.get("c").unwrap().as_int(), Some(-7));
    }

    #[test]
    fn the_shipped_profile_parses() {
        let text = include_str!("../../can/profiles/xinpu-vw-pq.toml");
        let p = Profile::parse(text).expect("shipped profile");
        assert_eq!(p.id, "xinpu-vw-pq");
        assert_eq!(p.baud, 38400);
        assert!(!p.verified, "no in-car capture exists; the profile must say so");
        assert!(p.messages.iter().all(|m| m.fields.iter().all(|f| !f.verified)));
        assert_eq!(p.keys.as_ref().map(|k| k.map.len()), Some(10));
    }
}
