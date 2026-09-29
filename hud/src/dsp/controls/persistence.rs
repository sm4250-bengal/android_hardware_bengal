// SPDX-License-Identifier: Apache-2.0
//! Bounded semantic user intent. No addresses, coefficients or policy volume.

use super::{Change, DspOutput};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

const HEADER: &str = "BENGAL_DSP_SETTINGS 1\n";
const MAX_BYTES: u64 = 4096;

#[derive(Default)]
pub(super) struct Settings {
    values: BTreeMap<(u8, i32), i32>,
}

fn key_value(change: Change) -> ((u8, i32), i32) {
    match change {
        Change::Mute(o, v) => ((1, o.raw() as i32), i32::from(v)),
        Change::Phase(o, v) => ((2, o.raw() as i32), i32::from(v)),
        Change::Delay(o, v) => ((3, o.raw() as i32), v),
        Change::Gain(o, v) => ((4, o.raw() as i32), v),
        Change::Eq(b, v) => ((5, b), v),
        Change::Bass(v) => ((6, 0), v),
        Change::Treble(v) => ((7, 0), v),
        Change::Loudness(v) => ((8, 0), i32::from(v.is_some())),
    }
}

fn change(op: u8, channel: i32, value: i32) -> Result<Change, String> {
    if (1..=4).contains(&op) {
        let output = DspOutput::from_raw(channel).ok_or("invalid DSP output")?;
        return match (op, value) {
            (1, 0..=1) => Ok(Change::Mute(output, value != 0)),
            (2, 0..=1) => Ok(Change::Phase(output, value != 0)),
            (3, 0..=145) if output.has_delay_cell() => Ok(Change::Delay(output, value)),
            (4, -200..=200) => Ok(Change::Gain(output, value)),
            _ => Err("invalid DSP output setting".into()),
        };
    }
    match (op, channel, value) {
        (5, 0..=3, -20..=20) => Ok(Change::Eq(channel, value)),
        (6, 0, -32..=32) => Ok(Change::Bass(value)),
        (7, 0, -32..=32) => Ok(Change::Treble(value)),
        // Some(0) means enabled intent ONLY, never a saved volume basis.
        (8, 0, 0..=1) => Ok(Change::Loudness((value != 0).then_some(0))),
        _ => Err("unknown or out-of-range DSP setting".into()),
    }
}

fn checksum(bytes: &[u8]) -> u64 {
    // FNV-1a detects storage damage; this is not an authentication boundary.
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

impl Settings {
    pub(super) fn record(&mut self, change: Change) -> bool {
        let (key, value) = key_value(change);
        self.values.insert(key, value) != Some(value)
    }

    pub(super) fn loudness_enabled(&self) -> bool {
        self.values.get(&(8, 0)) == Some(&1)
    }

    pub(super) fn changes(&self) -> Vec<Change> {
        self.values.iter().map(|(&(op, channel), &value)| {
            change(op, channel, value).expect("only validated semantic settings are stored")
        }).collect()
    }

    fn encode(&self) -> String {
        let mut text = HEADER.to_owned();
        for ((op, channel), value) in &self.values {
            text.push_str(&format!("{op} {channel} {value}\n"));
        }
        text.push_str(&format!("END {:016x}\n", checksum(text.as_bytes())));
        text
    }

    fn decode(text: &str) -> Result<Self, String> {
        if text.len() as u64 > MAX_BYTES || !text.starts_with(HEADER) || !text.ends_with('\n') {
            return Err("invalid DSP settings size, version or terminator".into());
        }
        let end = text.rfind("END ").ok_or("missing DSP settings checksum")?;
        let expected = format!("END {:016x}\n", checksum(text[..end].as_bytes()));
        if text[end..] != expected {
            return Err("DSP settings checksum mismatch".into());
        }
        let mut settings = Self::default();
        for line in text[HEADER.len()..end].lines() {
            let fields: Vec<_> = line.split(' ').collect();
            if fields.len() != 3 { return Err("invalid DSP settings row".into()); }
            let op = fields[0].parse::<u8>().map_err(|_| "invalid operation")?;
            let channel = fields[1].parse::<i32>().map_err(|_| "invalid channel")?;
            let value = fields[2].parse::<i32>().map_err(|_| "invalid value")?;
            let parsed = change(op, channel, value)?;
            let (key, value) = key_value(parsed);
            if settings.values.insert(key, value).is_some() {
                return Err("duplicate DSP settings row".into());
            }
        }
        Ok(settings)
    }
}

pub(super) struct Store {
    path: PathBuf,
    pub(super) settings: Settings,
    pub(super) load_error: Option<String>,
    dirty: bool,
}

impl Store {
    pub(super) fn load(path: PathBuf) -> Self {
        let result = (|| {
            // Linux O_NOFOLLOW | O_NONBLOCK: symlinks and FIFOs must not turn
            // corrupt settings into a boot hang or an unbounded external read.
            let file = match OpenOptions::new().read(true).custom_flags(0x20000 | 0x800).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Settings::default()),
                Err(error) => return Err(format!("read DSP settings: {error}")),
            };
            if !file.metadata().map_err(|e| e.to_string())?.is_file() {
                return Err("DSP settings is not a regular file".into());
            }
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
            if bytes.len() as u64 > MAX_BYTES { return Err("DSP settings exceeds 4096 bytes".into()); }
            Settings::decode(std::str::from_utf8(&bytes).map_err(|_| "DSP settings is not UTF-8")?)
        })();
        match result {
            Ok(settings) => Self { path, settings, load_error: None, dirty: false },
            Err(error) => Self { path, settings: Settings::default(), load_error: Some(error), dirty: false },
        }
    }

    pub(super) fn record_and_save(&mut self, change: Change) -> Result<(), String> {
        if let Some(error) = &self.load_error { return Err(error.clone()); }
        self.dirty |= self.settings.record(change);
        if !self.dirty { return Ok(()); }
        self.save().map_err(|error| format!("DSP applied but settings save failed: {error}"))?;
        self.dirty = false;
        Ok(())
    }

    fn save(&self) -> io::Result<()> {
        let parent = self.path.parent().ok_or_else(|| io::Error::other("no settings parent"))?;
        let directory = File::open(parent)?;
        let text = self.settings.encode();
        for attempt in 0..8 {
            let temp = parent.join(format!(".dsp-settings-{}-{attempt}.tmp", std::process::id()));
            let mut file = match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            let result = file.write_all(text.as_bytes()).and_then(|_| file.sync_all())
                .and_then(|_| fs::rename(&temp, &self.path));
            if let Err(error) = result {
                let _ = fs::remove_file(&temp);
                return Err(error);
            }
            // A failed directory fsync is visible even though rename committed.
            return directory.sync_all();
        }
        Err(io::Error::other("DSP settings temporary slots exhausted"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn framed(rows: &str) -> String {
        let body = format!("{HEADER}{rows}");
        format!("{body}END {:016x}\n", checksum(body.as_bytes()))
    }

    #[test]
    fn schema_rejects_unknown_duplicate_and_out_of_range_semantics_even_with_valid_checksum() {
        for rows in [
            "1 0 1\n", "1 6 1\n", "1 1 2\n", "2 1 -1\n",
            "3 5 0\n", "3 1 146\n", "3 1 -1\n", "4 1 201\n", "4 1 -201\n",
            "5 4 0\n", "5 0 21\n", "5 0 -21\n", "6 0 33\n", "7 0 -33\n",
            "8 0 63\n", "8 1 1\n", "9 0 0\n", "11 0 0\n",
            "4 1 -100\n4 1 -99\n", "4 1 -100 extra\n", "4  1 -100\n",
        ] {
            assert!(Settings::decode(&framed(rows)).is_err(), "accepted {rows:?}");
        }
        assert!(Settings::decode(&framed("1 5 1\n3 1 145\n4 1 -200\n5 3 -20\n6 0 -32\n7 0 32\n8 0 1\n")).is_ok());
        assert!(Settings::decode(&framed("")).is_ok());
    }

    #[test]
    fn storage_rejects_truncation_version_damage_extra_bytes_and_excess_size() {
        let original = framed("4 1 -100\n");
        for end in 0..original.len() {
            assert!(Settings::decode(&original[..end]).is_err(), "accepted truncation at {end}");
        }
        for index in 0..original.len() {
            let mut bytes = original.as_bytes().to_vec();
            bytes[index] ^= 1;
            assert!(Settings::decode(std::str::from_utf8(&bytes).unwrap()).is_err(), "accepted damage at {index}");
        }
        assert!(Settings::decode(&original.replacen("SETTINGS 1", "SETTINGS 2", 1)).is_err());
        assert!(Settings::decode(&(original + "extra\n")).is_err());
        assert!(Settings::decode(&framed(&"4 1 0\n".repeat(700))).is_err());
    }

    #[test]
    fn atomic_store_keeps_old_open_inode_and_rejects_nonregular_or_symlink_input() {
        let path = super::super::tests::settings_path("atomic");
        let mut store = Store::load(path.clone());
        assert!(store.load_error.is_none());
        assert!(store.settings.changes().is_empty());
        store.record_and_save(Change::Gain(DspOutput::FrontLeft, -100)).unwrap();
        let mut old = File::open(&path).unwrap();
        let old_inode = old.metadata().unwrap().ino();
        store.record_and_save(Change::Gain(DspOutput::FrontLeft, -90)).unwrap();
        assert_ne!(File::open(&path).unwrap().metadata().unwrap().ino(), old_inode);
        let mut old_text = String::new();
        old.read_to_string(&mut old_text).unwrap();
        assert!(old_text.contains("4 1 -100\n"));
        assert!(fs::read_to_string(&path).unwrap().contains("4 1 -90\n"));
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let symlink = path.with_extension("symlink");
        std::os::unix::fs::symlink(&path, &symlink).unwrap();
        assert!(Store::load(symlink).load_error.is_some());
        assert!(Store::load(path.parent().unwrap().to_path_buf()).load_error.is_some());
        fs::write(&path, vec![b'x'; 4097]).unwrap();
        assert!(Store::load(path.clone()).load_error.unwrap().contains("4096"));
        fs::write(&path, [0xff]).unwrap();
        assert!(Store::load(path).load_error.unwrap().contains("UTF-8"));
    }
}
