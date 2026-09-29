// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Small, fail-closed vehicle selector backed by a factual TSV registry.
//!
//! The registry is an inventory, not an adapter implementation.  A selected
//! `not_ported` row is persisted for a future adapter but never produces an
//! operational profile unless the compiled bundle contains its exact box,
//! brand and model binding. The legacy XinPu/VW decoder remains supported.

use super::capture::{json_object, Json};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::os::unix::fs::OpenOptionsExt;

const HEADER: &str = "id\tmakerId\tmaker\tbrandId\tbrand\tmodel\tstatus\tprofile";
const MAX_CATALOGUE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SELECTION_BYTES: usize = 128;
const MAX_ROWS: usize = 8_192;
const MAX_PAGE: i32 = 32;
const MAX_JSON_BYTES: usize = 128 * 1024;
const MAX_LABEL_BYTES: usize = 2_048;
const KNOWN_PROFILE: &str = "xinpu-vw-pq";
const SUPPORTED_USER: &str = "2009002";

#[derive(Clone, Debug, Eq, PartialEq)]
struct VehicleItem {
    id: String,
    maker_id: String,
    maker: String,
    brand_id: String,
    brand: String,
    model: String,
    status: String,
    profile: String,
}

/// Persistent selection and its bounded factual catalogue.
pub struct VehicleSelection {
    items: Vec<VehicleItem>,
    selected: Option<String>,
    selection_path: PathBuf,
    revision: u64,
    catalogue_error: String,
    error: String,
}

impl Default for VehicleSelection {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            selected: None,
            selection_path: PathBuf::new(),
            revision: 0,
            catalogue_error: String::new(),
            error: String::new(),
        }
    }
}

impl VehicleSelection {
    /// Load a registry and its persisted ID.  Load failures are retained in
    /// `state_json`, while an empty inventory makes the daemon fail closed.
    pub fn load(catalogue_path: &Path, selection_path: &Path) -> Self {
        let mut result = Self {
            selection_path: selection_path.to_path_buf(),
            ..Self::default()
        };
        match read_catalogue(catalogue_path) {
            Ok(items) => result.items = items,
            Err(error) => {
                result.catalogue_error = error;
                return result;
            }
        }
        match read_selection(selection_path) {
            Ok(Some(id)) => match result.selectable_item(&id) {
                Some(_) => result.selected = Some(id),
                None => result.error = format!("persisted vehicle ID is not selectable: {id}"),
            },
            Ok(None) => {}
            Err(error) => result.error = error,
        }
        result
    }

    /// Return one deterministic page of the full registry.
    pub fn catalogue_json(&self, offset: i32, limit: i32) -> Result<String, String> {
        if !self.catalogue_error.is_empty() {
            return Err(self.catalogue_error.clone());
        }
        if offset < 0 {
            return Err("catalogue offset must not be negative".to_string());
        }
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err("catalogue limit must be between 1 and 32".to_string());
        }
        let start =
            usize::try_from(offset).map_err(|_| "catalogue offset is too large".to_string())?;
        let end = start.saturating_add(limit as usize).min(self.items.len());
        let mut items = String::from("[");
        for (index, item) in self.items.get(start..end).unwrap_or(&[]).iter().enumerate() {
            if index > 0 {
                items.push(',');
            }
            items.push_str(&item_json(item));
        }
        items.push(']');
        let result = json_object(&[
            ("total", Json::Int(self.items.len() as i64)),
            ("offset", Json::Int(i64::from(offset))),
            ("items", Json::Raw(items)),
        ]);
        if result.len() > MAX_JSON_BYTES {
            return Err("catalogue page exceeds JSON bound".to_string());
        }
        Ok(result)
    }

    /// Return selected item, revision, and operational support state.
    pub fn state_json(&self) -> String {
        let selected = self
            .selected
            .as_deref()
            .and_then(|id| self.selectable_item(id))
            .map(item_json)
            .unwrap_or_else(|| "null".to_string());
        json_object(&[
            ("selected", Json::Raw(selected)),
            ("revision", Json::Int(self.revision as i64)),
            (
                "error",
                Json::Str(if self.catalogue_error.is_empty() {
                    self.error.clone()
                } else {
                    self.catalogue_error.clone()
                }),
            ),
            ("supported", Json::Bool(self.active_profile().is_some())),
        ])
    }

    /// Persist a selection ID first, then update memory.  Empty clears the
    /// file.  Unsupported-but-known rows are intentionally selectable and
    /// persistable, but have no active profile.
    pub fn select(&mut self, id: &str) -> Result<bool, String> {
        if !self.catalogue_error.is_empty() {
            return Err(self.catalogue_error.clone());
        }
        if id.is_empty() {
            if self.selected.is_none() && self.error.is_empty() {
                return Ok(false);
            }
            let warning = persist_selection(&self.selection_path, "")?;
            self.selected = None;
            self.error = warning;
            self.revision = self.revision.saturating_add(1);
            return Ok(true);
        }
        let item = self
            .selectable_item(id)
            .ok_or_else(|| format!("vehicle ID is unknown or unavailable: {id}"))?;
        if self.selected.as_deref() == Some(item.id.as_str()) {
            return Ok(false);
        }
        let warning = persist_selection(&self.selection_path, &item.id)?;
        self.selected = Some(item.id.clone());
        self.error = warning;
        self.revision = self.revision.saturating_add(1);
        Ok(true)
    }

    /// Resolve the exact compiled model, or the legacy XinPu/VW profile.
    pub fn active_profile(&self) -> Option<&'static str> {
        if !self.catalogue_error.is_empty() {
            return None;
        }
        let id = self.selected.as_deref()?;
        let item = self.selectable_item(id)?;
        if let Some(profile) = super::controls::binding(&item.id, &item.maker_id, &item.brand_id) {
            Some(profile.id.as_str())
        } else if item.profile == KNOWN_PROFILE && item.id == SUPPORTED_USER {
            Some(KNOWN_PROFILE)
        } else {
            None
        }
    }

    /// Monotonic in-process selection revision; loading starts at zero.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Exact persisted catalogue ID, independently of adapter availability.
    pub(crate) fn selected_id(&self) -> Option<&str> { self.selected.as_deref() }

    fn selectable_item(&self, id: &str) -> Option<&VehicleItem> {
        self.items.iter().find(|item| {
            item.id == id && (item.status == "available" || item.status == "not_ported")
        })
    }
}

fn read_bounded(path: &Path, limit: usize, what: &str) -> Result<Vec<u8>, String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("cannot stat {what}: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("{what} must not be a symlink"));
    }
    if !metadata.is_file() {
        return Err(format!("{what} is not a regular file"));
    }
    if metadata.len() > limit as u64 {
        return Err(format!("{what} exceeds {limit} byte bound"));
    }
    let file = File::open(path).map_err(|error| format!("cannot open {what}: {error}"))?;
    let mut bytes = Vec::with_capacity(metadata.len().min(limit as u64) as usize);
    file.take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {what}: {error}"))?;
    if bytes.len() > limit {
        return Err(format!("{what} exceeds {limit} byte bound"));
    }
    Ok(bytes)
}

fn read_catalogue(path: &Path) -> Result<Vec<VehicleItem>, String> {
    let bytes = read_bounded(path, MAX_CATALOGUE_BYTES, "vehicle catalogue")?;
    let text =
        String::from_utf8(bytes).map_err(|_| "vehicle catalogue is not UTF-8".to_string())?;
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return Err("vehicle catalogue header does not match schema".to_string());
    }
    let mut items = Vec::new();
    for (line_index, line) in lines.enumerate() {
        if line.is_empty() {
            return Err(format!(
                "vehicle catalogue line {} is empty",
                line_index + 2
            ));
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 8 {
            return Err(format!(
                "vehicle catalogue line {} has {} fields",
                line_index + 2,
                fields.len()
            ));
        }
        let item = VehicleItem {
            id: decimal(fields[0], "id", line_index + 2)?,
            maker_id: decimal(fields[1], "makerId", line_index + 2)?,
            maker: field(fields[2], "maker", line_index + 2, false)?,
            brand_id: decimal(fields[3], "brandId", line_index + 2)?,
            brand: field(fields[4], "brand", line_index + 2, false)?,
            model: field(fields[5], "model", line_index + 2, false)?,
            status: field(fields[6], "status", line_index + 2, false)?,
            profile: field(fields[7], "profile", line_index + 2, true)?,
        };
        if !matches!(
            item.status.as_str(),
            "available" | "not_ported" | "missing" | "ambiguous" | "none"
        ) {
            return Err(format!(
                "vehicle catalogue line {} has unknown status",
                line_index + 2
            ));
        }
        // A label alone never enables a protocol: require the exact compiled
        // model/box/brand binding, with the existing XinPu decoder as fallback.
        let expected = super::controls::binding(&item.id, &item.maker_id, &item.brand_id)
            .map(|profile| profile.id.as_str())
            .or_else(|| {
                (item.id == SUPPORTED_USER && item.maker_id == "2" && item.brand_id == "9")
                    .then_some(KNOWN_PROFILE)
            });
        if item.status == "available" && expected != Some(item.profile.as_str()) {
            return Err(format!(
                "vehicle catalogue line {} lacks an exact compiled adapter binding",
                line_index + 2
            ));
        }
        if item.status != "available" && !item.profile.is_empty() {
            return Err(format!(
                "vehicle catalogue line {} has profile on unavailable row",
                line_index + 2
            ));
        }
        items.push(item);
        if items.len() > MAX_ROWS {
            return Err("vehicle catalogue exceeds row bound".to_string());
        }
    }
    if items.is_empty() {
        return Err("vehicle catalogue has no rows".to_string());
    }
    items.sort_by(|left, right| {
        (
            &left.id,
            &left.maker_id,
            &left.brand_id,
            &left.model,
            &left.maker,
            &left.brand,
        )
            .cmp(&(
                &right.id,
                &right.maker_id,
                &right.brand_id,
                &right.model,
                &right.maker,
                &right.brand,
            ))
    });
    let mut index = 0;
    while index < items.len() {
        let mut end = index + 1;
        while end < items.len() && items[end].id == items[index].id {
            end += 1;
        }
        if end - index > 1 {
            for item in &mut items[index..end] {
                item.status = "ambiguous".to_string();
                item.profile.clear();
            }
        }
        index = end;
    }
    Ok(items)
}

fn decimal(value: &str, field_name: &str, line: usize) -> Result<String, String> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) || value.len() > 12 {
        return Err(format!(
            "vehicle catalogue line {line} has invalid {field_name}"
        ));
    }
    Ok(value.to_string())
}

fn field(value: &str, field_name: &str, line: usize, allow_empty: bool) -> Result<String, String> {
    if (!allow_empty && value.is_empty())
        || value.len() > MAX_LABEL_BYTES
        || value
            .bytes()
            .any(|byte| matches!(byte, b'\r' | b'\n' | b'\t'))
    {
        return Err(format!(
            "vehicle catalogue line {line} has invalid {field_name}"
        ));
    }
    Ok(value.to_string())
}

fn read_selection(path: &Path) -> Result<Option<String>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot stat selection file: {error}")),
    };
    if metadata.file_type().is_symlink() {
        return Err("selection file must not be a symlink".to_string());
    }
    let bytes = read_bounded(path, MAX_SELECTION_BYTES, "selection file")?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let text = String::from_utf8(bytes).map_err(|_| "selection file is not UTF-8".to_string())?;
    let id = text.strip_suffix('\n').unwrap_or(&text);
    if id.is_empty()
        || id
            .bytes()
            .any(|byte| matches!(byte, b'\r' | b'\n' | b'\t' | b'/' | b'\\'))
    {
        return Err("selection file contains an invalid ID".to_string());
    }
    Ok(Some(decimal(id, "selection ID", 1)?))
}

fn persist_selection(path: &Path, id: &str) -> Result<String, String> {
    if path.as_os_str().is_empty() {
        return Err("selection path is empty".to_string());
    }
    if id.len() > MAX_SELECTION_BYTES {
        return Err("selection ID exceeds bound".to_string());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "selection path has no parent".to_string())?;
    let file_name = path
        .file_name()
        .ok_or_else(|| "selection path has no filename".to_string())?;
    let directory = File::open(parent)
        .map_err(|error| format!("cannot open selection directory: {error}"))?;
    for attempt in 0..8_u8 {
        let temporary = parent.join(format!(
            ".{}.tmp-{}-{attempt}",
            file_name.to_string_lossy(),
            std::process::id()
        ));
        let opened = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary);
        let mut file = match opened {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("cannot create selection temporary: {error}")),
        };
        let write_result = file.write_all(id.as_bytes()).and_then(|_| file.sync_all());
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary);
            return Err(format!("cannot write selection: {error}"));
        }
        if let Err(error) = fs::rename(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(format!("cannot atomically replace selection: {error}"));
        }
        // Rename has committed: memory must adopt the new ID even if the
        // durability barrier fails. Report that failure as a visible warning,
        // never claim the old ID is still selected while disk holds the new.
        return Ok(match directory.sync_all() {
            Ok(()) => String::new(),
            Err(error) => format!("Vehicle saved, but directory sync failed: {error}"),
        });
    }
    Err("cannot allocate selection temporary".to_string())
}

fn item_json(item: &VehicleItem) -> String {
    let generated = if matches!(item.status.as_str(), "available" | "not_ported") {
        super::controls::binding(&item.id, &item.maker_id, &item.brand_id)
    } else { None };
    json_object(&[
        ("id", Json::Str(item.id.clone())),
        ("makerId", Json::Str(item.maker_id.clone())),
        ("maker", Json::Str(item.maker.clone())),
        ("brandId", Json::Str(item.brand_id.clone())),
        ("brand", Json::Str(item.brand.clone())),
        ("model", Json::Str(item.model.clone())),
        ("status", Json::Str(if generated.is_some() { "available".into() } else { item.status.clone() })),
        ("profile", Json::Str(generated.map_or_else(|| item.profile.clone(), |p| p.id.clone()))),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("bengal-selection-{label}-{stamp}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn registry(rows: &[&str]) -> String {
        let mut value = format!("{HEADER}\n");
        for row in rows {
            value.push_str(row);
            value.push('\n');
        }
        value
    }

    #[test]
    fn default_is_empty_and_fail_closed() {
        let selection = VehicleSelection::default();
        assert_eq!(selection.revision(), 0);
        assert!(selection.active_profile().is_none());
        assert!(selection.state_json().contains("\"selected\":null"));
        assert!(selection.state_json().contains("\"supported\":false"));
    }

    #[test]
    fn save_reload_and_clear_only_change_on_real_selection() {
        let dir = temp_dir("persist");
        let catalogue = dir.join("vehicles.tsv");
        let selected = dir.join("selected");
        fs::write(
            &catalogue,
            registry(&[
                "2009002\t2\tXinPu\t9\tVolkswagen\tPolo\tavailable\txinpu-vw-pq",
                "1001001\t1\tRaise\t1\tToyota\tYaris\tnot_ported\t",
            ]),
        )
        .unwrap();
        let mut state = VehicleSelection::load(&catalogue, &selected);
        assert!(state.select("2009002").unwrap());
        assert!(!state.select("2009002").unwrap());
        assert_eq!(state.active_profile(), Some(KNOWN_PROFILE));
        assert_eq!(fs::read_to_string(&selected).unwrap(), "2009002");
        let reloaded = VehicleSelection::load(&catalogue, &selected);
        assert_eq!(reloaded.active_profile(), Some(KNOWN_PROFILE));
        assert!(state.select("").unwrap());
        assert_eq!(fs::read(&selected).unwrap(), b"");
        assert!(state.active_profile().is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unsupported_rows_are_saved_but_not_operational() {
        let dir = temp_dir("unsupported");
        let catalogue = dir.join("vehicles.tsv");
        let selected = dir.join("selected");
        fs::write(
            &catalogue,
            registry(&["1002001\t1\tRaise\t2\tToyota\tYaris\tnot_ported\t"]),
        )
        .unwrap();
        let mut state = VehicleSelection::load(&catalogue, &selected);
        assert!(state.select("1002001").unwrap());
        assert!(state.active_profile().is_none());
        assert!(state.state_json().contains("\"supported\":false"));
        assert_eq!(fs::read_to_string(selected).unwrap(), "1002001");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ambiguity_missing_and_traversal_are_unselectable() {
        let dir = temp_dir("invalid");
        let catalogue = dir.join("vehicles.tsv");
        let selected = dir.join("selected");
        fs::write(
            &catalogue,
            registry(&[
                "3053001\t3\tHiworld\t53\tLeopaard\tFirst\tambiguous\t",
                "3053001\t3\tHiworld\t53\tLeopaard\tSecond\tambiguous\t",
                "4004001\t4\tRaise\t4\tFord\tMissing\tmissing\t",
            ]),
        )
        .unwrap();
        let mut state = VehicleSelection::load(&catalogue, &selected);
        assert!(state.select("3053001").is_err());
        assert!(state.select("4004001").is_err());
        assert!(state.select("../escape").is_err());
        let page = state.catalogue_json(0, 32).unwrap();
        assert!(page.contains("3053001"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_and_oversized_registries_fail_closed() {
        let dir = temp_dir("bounded");
        let catalogue = dir.join("vehicles.tsv");
        let selected = dir.join("selected");
        fs::write(&catalogue, b"bad\n").unwrap();
        let state = VehicleSelection::load(&catalogue, &selected);
        assert!(state.catalogue_json(0, 1).is_err());
        assert!(state.state_json().contains("\"supported\":false"));
        fs::write(&catalogue, vec![b'x'; MAX_CATALOGUE_BYTES + 1]).unwrap();
        let oversized = VehicleSelection::load(&catalogue, &selected);
        assert!(oversized.catalogue_json(0, 1).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn known_id_with_the_wrong_box_or_brand_cannot_enable_vw_commands() {
        let dir = temp_dir("wrong-box");
        let catalogue = dir.join("vehicles.tsv");
        for row in [
            "2009002\t3\tHiworld\t9\tVolkswagen\tWrong box\tavailable\txinpu-vw-pq",
            "2009002\t2\tXinPu\t1\tToyota\tWrong brand\tavailable\txinpu-vw-pq",
            "1001001\t2\tXinPu\t1\tVolkswagen\tWrong box\tavailable\traise-vw-pq",
            "1001001\t1\tRaise\t9\tVolkswagen\tWrong brand\tavailable\traise-vw-pq",
            "1001001\t1\tRaise\t1\tVolkswagen\tWrong profile\tavailable\txinpu-vw-pq",
            "1001999\t1\tRaise\t1\tVolkswagen\tUnported model\tavailable\traise-vw-pq",
        ] {
            fs::write(&catalogue, registry(&[row])).unwrap();
            let mut state = VehicleSelection::load(&catalogue, &dir.join("selected"));
            assert!(state.catalogue_json(0, 1).is_err());
            assert!(state.select("2009002").is_err());
            assert!(state.active_profile().is_none());
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_persisted_id_does_not_hide_catalogue_and_can_be_repaired() {
        let dir = temp_dir("recover");
        let catalogue = dir.join("vehicles.tsv");
        let selected = dir.join("selected");
        fs::write(
            &catalogue,
            registry(&["2009002\t2\tXinPu\t9\tVolkswagen\tPolo\tavailable\txinpu-vw-pq"]),
        )
        .unwrap();
        fs::write(&selected, b"../stale").unwrap();
        let mut state = VehicleSelection::load(&catalogue, &selected);
        assert!(state.catalogue_json(0, 1).is_ok());
        assert!(state.select("2009002").unwrap());
        assert_eq!(state.active_profile(), Some(KNOWN_PROFILE));
        assert!(state.select("").unwrap());
        assert_eq!(fs::read(&selected).unwrap(), b"");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_persistence_leaves_old_selection() {
        let dir = temp_dir("failure");
        let catalogue = dir.join("vehicles.tsv");
        let selected = dir.join("selected");
        fs::write(
            &catalogue,
            registry(&[
                "2009002\t2\tXinPu\t9\tVolkswagen\tPolo\tavailable\txinpu-vw-pq",
                "1001001\t1\tRaise\t1\tToyota\tYaris\tnot_ported\t",
            ]),
        )
        .unwrap();
        let mut state = VehicleSelection::load(&catalogue, &selected);
        assert!(state.select("2009002").unwrap());
        assert_eq!(state.active_profile(), Some(KNOWN_PROFILE));
        fs::remove_file(&selected).unwrap();
        fs::create_dir(&selected).unwrap();
        assert!(state.select("1001001").is_err());
        assert_eq!(state.selected.as_deref(), Some("2009002"));
        assert_eq!(state.revision(), 1);
        let _ = fs::remove_dir_all(dir);
    }
}
