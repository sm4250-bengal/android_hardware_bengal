// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! JSONL capture — the fixture the whole CAN stack is still waiting for.
//!
//! One JSON object per line, the schema the device-1 Go receiver used
//! (`~/android/hardware/xyauto_can/receiver/parser.go:13-31`) so a desk replay
//! tool reads either: `sequence`, `time` (RFC 3339 UTC), `event`, and per kind
//! `raw`, `type`, `length`, `data`, `checksum`, `checksum_ok`, `reason`,
//! `profile`, `semantic`, `fields`. Raw bytes are ALWAYS kept, decoded or not:
//! every decode is a hypothesis until a capture confirms it, and a capture
//! without the bytes cannot correct a wrong hypothesis later.
//!
//! No serde: this crate has zero dependencies. Strings are escaped per RFC 8259;
//! numbers are integers; nothing here is a float.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A JSON scalar or pre-rendered object.
#[derive(Clone, Debug)]
pub enum Json {
    /// A string (escaped on output).
    Str(String),
    /// An integer.
    Int(i64),
    /// A boolean.
    Bool(bool),
    /// Already-valid JSON text (an object built by the caller).
    Raw(String),
}

/// Escape `text` as a JSON string literal including the quotes.
pub fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Render `pairs` as one JSON object.
pub fn json_object(pairs: &[(&str, Json)]) -> String {
    let mut out = String::from("{");
    for (i, (key, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&json_string(key));
        out.push(':');
        match value {
            Json::Str(s) => out.push_str(&json_string(s)),
            Json::Int(n) => out.push_str(&n.to_string()),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Raw(r) => out.push_str(r),
        }
    }
    out.push('}');
    out
}

/// RFC 3339 UTC timestamp (`2026-09-05T04:12:33.123Z`) without a date crate.
pub fn rfc3339_utc(now: SystemTime) -> String {
    let dur = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let millis = dur.subsec_millis();
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-from-civil inverse (proleptic Gregorian).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// An open capture file.
pub struct Capture {
    file: File,
    path: PathBuf,
    sequence: u64,
    remaining: Option<u64>,
}

impl Capture {
    /// Create `<dir>/can-<unix-seconds>[-n].jsonl`, making `dir` if needed.
    pub fn create(dir: &Path) -> io::Result<Capture> {
        fs::create_dir_all(dir)?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for attempt in 0..100u32 {
            let name = if attempt == 0 {
                format!("can-{stamp}.jsonl")
            } else {
                format!("can-{stamp}-{attempt}.jsonl")
            };
            let path = dir.join(name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Capture {
                        file,
                        path,
                        sequence: 0,
                        remaining: None,
                    })
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not find a free capture file name",
        ))
    }

    /// Create a capture whose total attempted writes cannot exceed `budget`.
    /// A partial/failed write also consumes its reservation; retries never grow it.
    pub fn create_bounded(dir: &Path, budget: u64) -> io::Result<Capture> {
        let mut capture = Self::create(dir)?;
        capture.remaining = Some(budget);
        Ok(capture)
    }

    /// Open an explicit file path (desk use), truncating.
    pub fn at(path: &Path) -> io::Result<Capture> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        Ok(Capture {
            file: File::create(path)?,
            path: path.to_path_buf(),
            sequence: 0,
            remaining: None,
        })
    }

    /// Where the bytes go.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Lines written so far.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Append one event; `sequence` and `time` are added here.
    pub fn record(&mut self, event: &str, pairs: &[(&str, Json)]) -> io::Result<()> {
        self.sequence += 1;
        let mut all: Vec<(&str, Json)> = Vec::with_capacity(pairs.len() + 3);
        all.push(("sequence", Json::Int(self.sequence as i64)));
        all.push(("time", Json::Str(rfc3339_utc(SystemTime::now()))));
        all.push(("event", Json::Str(event.to_owned())));
        all.extend(pairs.iter().cloned());
        let mut line = json_object(&all);
        line.push('\n');
        if let Some(remaining) = &mut self.remaining {
            if line.len() as u64 > *remaining {
                return Err(io::Error::other("capture budget exhausted"));
            }
            *remaining -= line.len() as u64;
        }
        self.file.write_all(line.as_bytes())
    }

    /// Flush to the file system (called from the tell tick).
    pub fn sync(&mut self) -> io::Result<()> {
        self.file.flush()?;
        self.file.sync_data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn json_escaping_covers_quotes_backslashes_and_controls() {
        assert_eq!(json_string("a\"b\\c\nd\u{1}"), "\"a\\\"b\\\\c\\nd\\u0001\"");
        assert_eq!(
            json_object(&[("k", Json::Int(-3)), ("b", Json::Bool(true)), ("r", Json::Raw("{}".into()))]),
            "{\"k\":-3,\"b\":true,\"r\":{}}"
        );
    }

    #[test]
    fn rfc3339_matches_known_instants() {
        assert_eq!(rfc3339_utc(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        // 2026-09-05T04:00:00Z = 1788580800
        assert_eq!(
            rfc3339_utc(UNIX_EPOCH + Duration::from_millis(1_788_580_800_123)),
            "2026-09-05T04:00:00.123Z"
        );
        // Leap day 2024-02-29T12:00:00Z = 1709208000
        assert_eq!(
            rfc3339_utc(UNIX_EPOCH + Duration::from_secs(1_709_208_000)),
            "2024-02-29T12:00:00.000Z"
        );
    }

    #[test]
    fn capture_writes_one_object_per_line_with_sequence() {
        let dir = std::env::temp_dir().join(format!("bengal-hud-can-capture-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut cap = Capture::create(&dir).unwrap();
        cap.record("frame", &[("raw", Json::Str("2e1000ef".into())), ("checksum_ok", Json::Bool(true))]).unwrap();
        cap.record("tell", &[("rx", Json::Int(4))]).unwrap();
        cap.sync().unwrap();
        let text = fs::read_to_string(cap.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("{\"sequence\":1,\"time\":\""));
        assert!(lines[0].ends_with("\"event\":\"frame\",\"raw\":\"2e1000ef\",\"checksum_ok\":true}"));
        assert!(lines[1].contains("\"sequence\":2"));
        // A second capture in the same second gets a distinct name.
        let cap2 = Capture::create(&dir).unwrap();
        assert_ne!(cap2.path(), cap.path());
        let _ = fs::remove_dir_all(&dir);
    }
}
