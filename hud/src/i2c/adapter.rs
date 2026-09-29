// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Resolve an I2C adapter by name / device-tree node, never by number.
//!
//! Reads sysfs only. Opens no bus, and touches nothing under `/dev`.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// One I2C adapter as sysfs describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adapter {
    /// The `N` of `i2c-N`. Informational — never a selector.
    pub number: u32,
    /// `/sys/class/i2c-dev/i2c-N/name`.
    ///
    /// For an `i2c-gpio` bus this is the device-tree node name, because
    /// `i2c-gpio.c:422` does `strscpy(adap->name, dev_name(dev))` whenever the
    /// device has a firmware node. So our four bit-banged buses report
    /// `i2c-touch`, `i2c-fm`, `i2c-aux`, `i2c-dsp`.
    pub name: String,
    /// The `of_node` symlink target, when the adapter has one.
    pub of_node: Option<String>,
    /// `/dev/i2c-N`.
    pub path: PathBuf,
    /// Which needle matched, for the log line and for `TunerState.i2cAdapter`.
    pub matched_by: MatchedBy,
}

impl fmt::Display for Adapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.path.display(), self.name)
    }
}

/// How [`resolve`] picked an adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchedBy {
    /// The adapter's `name` equalled a needle.
    Name(String),
    /// The adapter's `of_node` path ended in a needle.
    OfNode(String),
    /// An explicit override, for bring-up.
    Override,
}

/// Why an adapter could not be resolved.
#[derive(Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// `/sys/class/i2c-dev` does not exist.
    ///
    /// On this device that is the EXPECTED state until `i2c-dev.ko` ships:
    /// `CONFIG_I2C_CHARDEV` is "not set" in the GKI build config *and* in the
    /// flashed Image, so there is no i2c chardev class at all. The message
    /// says so, because "no such directory" on its own sends the reader
    /// hunting for a missing bus instead of a missing module.
    NoChardevClass,
    /// The class exists but no adapter matched any needle.
    NoMatch {
        /// What was looked for.
        needles: Vec<String>,
        /// What was there, for the log line.
        seen: Vec<String>,
    },
    /// The override named an adapter that is not there.
    OverrideMissing(String),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::NoChardevClass => write!(
                f,
                "no /sys/class/i2c-dev: this kernel has no i2c chardev \
                 (CONFIG_I2C_CHARDEV is not set in the GKI) -- \
                 i2c-dev.ko must be in the package's modules.load"
            ),
            ResolveError::NoMatch { needles, seen } => write!(
                f,
                "no i2c adapter matched {:?}; present: {:?}",
                needles, seen
            ),
            ResolveError::OverrideMissing(name) => {
                write!(f, "override named adapter {:?}, which is not present", name)
            }
        }
    }
}

impl std::error::Error for ResolveError {}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Describe every adapter under `<sysfs_root>/class/i2c-dev`, in number order.
///
/// An entry whose `name` cannot be read is skipped, not an error: sysfs
/// entries appear and disappear while we walk them.
pub fn list(sysfs_root: &Path) -> Result<Vec<Adapter>, ResolveError> {
    let class = sysfs_root.join("class/i2c-dev");
    let entries = match fs::read_dir(&class) {
        Ok(e) => e,
        Err(_) => return Err(ResolveError::NoChardevClass),
    };

    let mut out: Vec<Adapter> = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let base = match file_name.to_str() {
            Some(b) => b,
            None => continue,
        };
        let number: u32 = match base.strip_prefix("i2c-").and_then(|n| n.parse().ok()) {
            Some(n) => n,
            None => continue,
        };
        let dir = class.join(base);
        let name = match read_trimmed(&dir.join("name")) {
            Some(n) => n,
            None => continue,
        };
        // Two places carry of_node; the second is the one that exists when the
        // class entry's `device` link points at the adapter rather than its
        // parent. A missing of_node is not an error -- an adapter with no
        // device-tree node simply cannot match an of_node needle.
        let of_node = fs::read_link(dir.join("device/of_node"))
            .ok()
            .or_else(|| {
                fs::read_link(
                    sysfs_root
                        .join("bus/i2c/devices")
                        .join(base)
                        .join("of_node"),
                )
                .ok()
            })
            .map(|p| p.to_string_lossy().into_owned());

        out.push(Adapter {
            number,
            name,
            of_node,
            path: PathBuf::from(format!("/dev/i2c-{}", number)),
            matched_by: MatchedBy::Override, // replaced by resolve()
        });
    }
    out.sort_by_key(|a| a.number);
    Ok(out)
}

/// Resolve one adapter.
///
/// `needles` are tried **in order**, and for each needle the name is tried
/// before the device-tree node — so the caller states a preference
/// (`["i2c-fm", "fm_8035"]` = "our node name, else the stock one") rather than
/// hoping the sysfs walk order is stable.
///
/// `override_name` short-circuits everything: it is for bring-up, when the
/// board is not yet described the way the needles expect. It fails loudly if
/// the named adapter is absent rather than silently falling back — an override
/// that quietly does not apply is worse than no override.
pub fn resolve(
    sysfs_root: &Path,
    needles: &[&str],
    override_name: Option<&str>,
) -> Result<Adapter, ResolveError> {
    let adapters = list(sysfs_root)?;

    if let Some(want) = override_name {
        return adapters
            .into_iter()
            .find(|a| a.name == want)
            .map(|mut a| {
                a.matched_by = MatchedBy::Override;
                a
            })
            .ok_or_else(|| ResolveError::OverrideMissing(want.to_string()));
    }

    for needle in needles {
        if let Some(mut a) = adapters.iter().find(|a| a.name == *needle).cloned() {
            a.matched_by = MatchedBy::Name((*needle).to_string());
            return Ok(a);
        }
        if let Some(mut a) = adapters
            .iter()
            .find(|a| match &a.of_node {
                // endswith, not equals: of_node is a full path such as
                // /sys/firmware/devicetree/base/i2c-fm.
                Some(node) => node.rsplit('/').next() == Some(*needle),
                None => false,
            })
            .cloned()
        {
            a.matched_by = MatchedBy::OfNode((*needle).to_string());
            return Ok(a);
        }
    }

    Err(ResolveError::NoMatch {
        needles: needles.iter().map(|n| n.to_string()).collect(),
        seen: adapters.iter().map(|a| a.name.clone()).collect(),
    })
}

/// Needles for the FM bus, in preference order.
///
/// `i2c-fm` is our own device-tree node name
/// (`kernel/dts/sm4250-xyauto-hu.dts:183`). `fm_8035` is what the stock tree
/// calls the tuner's client node, kept so that a tree transcribed verbatim
/// from stock still resolves.
pub const FM_NEEDLES: &[&str] = &["i2c-fm", "fm_8035"];

/// Needles for the DSP bus. Its DT node is `status = "disabled"` until 4c.
pub const DSP_NEEDLES: &[&str] = &["i2c-dsp", "adau1701"];

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Build a fake `/sys` with the shape the real one has:
    /// `class/i2c-dev/i2c-N/{name}` plus a `bus/i2c/devices/i2c-N/of_node`
    /// symlink for the buses that have a device-tree node.
    fn fake_sysfs(adapters: &[(u32, &str, bool)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "bengal-hud-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("class/i2c-dev")).unwrap();
        fs::create_dir_all(root.join("bus/i2c/devices")).unwrap();
        fs::create_dir_all(root.join("firmware/devicetree/base")).unwrap();
        for (n, name, has_of_node) in adapters {
            let dir = root.join(format!("class/i2c-dev/i2c-{}", n));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("name"), format!("{}\n", name)).unwrap();
            if *has_of_node {
                let target = root.join("firmware/devicetree/base").join(name);
                fs::create_dir_all(&target).unwrap();
                let link = root.join(format!("bus/i2c/devices/i2c-{}", n));
                fs::create_dir_all(&link).unwrap();
                std::os::unix::fs::symlink(&target, link.join("of_node")).unwrap();
            }
        }
        root
    }

    /// The D3-0043 mapping: touch registered first.
    fn d3_0043() -> PathBuf {
        fake_sysfs(&[
            (0, "i2c-touch", true),
            (1, "i2c-fm", true),
            (2, "i2c-aux", true),
        ])
    }

    /// The D3-0015 mapping: fm registered first. SAME BOARD, SAME DT.
    fn d3_0015() -> PathBuf {
        fake_sysfs(&[
            (0, "i2c-fm", true),
            (1, "i2c-aux", true),
            (3, "i2c-touch", true),
        ])
    }

    #[test]
    fn resolves_fm_on_both_measured_mappings() {
        // This is the whole point of the module: the two boots below really
        // happened on this board and gave the FM bus different numbers.
        let a = resolve(&d3_0043(), FM_NEEDLES, None).unwrap();
        assert_eq!(a.number, 1);
        assert_eq!(a.path, PathBuf::from("/dev/i2c-1"));

        let b = resolve(&d3_0015(), FM_NEEDLES, None).unwrap();
        assert_eq!(b.number, 0);
        assert_eq!(b.path, PathBuf::from("/dev/i2c-0"));

        // ...and in both cases it is the same bus by name.
        assert_eq!(a.name, "i2c-fm");
        assert_eq!(b.name, "i2c-fm");
    }

    #[test]
    fn a_hardcoded_number_would_have_picked_the_touch_bus() {
        // The negative control for the bug this module exists to prevent: on
        // the D3-0015 mapping, "the FM tuner is on i2c-1" lands on i2c-aux.
        let adapters = list(&d3_0015()).unwrap();
        let one = adapters.iter().find(|a| a.number == 1).unwrap();
        assert_ne!(one.name, "i2c-fm");
        assert_eq!(one.name, "i2c-aux");
    }

    #[test]
    fn falls_back_to_of_node_when_the_name_differs() {
        // A tree transcribed from stock: the adapter name is the stock client
        // node, but the of_node still ends in our needle.
        let root = fake_sysfs(&[(0, "i2c-fm", true)]);
        // rename the class `name` so only the of_node can match
        fs::write(root.join("class/i2c-dev/i2c-0/name"), "unrelated\n").unwrap();
        let a = resolve(&root, FM_NEEDLES, None).unwrap();
        assert_eq!(a.matched_by, MatchedBy::OfNode("i2c-fm".to_string()));
    }

    #[test]
    fn needle_order_decides_not_walk_order() {
        let root = fake_sysfs(&[(0, "fm_8035", false), (1, "i2c-fm", false)]);
        let a = resolve(&root, FM_NEEDLES, None).unwrap();
        assert_eq!(a.name, "i2c-fm", "the first needle must win, not i2c-0");
    }

    #[test]
    fn missing_chardev_class_says_why() {
        let root = std::env::temp_dir().join("bengal-hud-test-absent");
        let _ = fs::remove_dir_all(&root);
        let err = resolve(&root, FM_NEEDLES, None).unwrap_err();
        assert_eq!(err, ResolveError::NoChardevClass);
        // The message must name the module, not just the directory: on this
        // device "no such directory" is the expected state until i2c-dev.ko
        // ships, and a reader who is not told that goes bus-hunting.
        assert!(err.to_string().contains("i2c-dev.ko"));
        assert!(err.to_string().contains("CONFIG_I2C_CHARDEV"));
    }

    #[test]
    fn no_match_reports_what_was_there() {
        let root = fake_sysfs(&[(0, "i2c-touch", false)]);
        match resolve(&root, FM_NEEDLES, None).unwrap_err() {
            ResolveError::NoMatch { seen, .. } => assert_eq!(seen, vec!["i2c-touch"]),
            other => panic!("wrong error: {:?}", other),
        }
    }

    #[test]
    fn override_fails_loudly_when_absent() {
        let root = fake_sysfs(&[(0, "i2c-fm", false)]);
        assert_eq!(
            resolve(&root, FM_NEEDLES, Some("i2c-nope")).unwrap_err(),
            ResolveError::OverrideMissing("i2c-nope".to_string())
        );
        // and applies when present
        assert_eq!(
            resolve(&root, FM_NEEDLES, Some("i2c-fm")).unwrap().number,
            0
        );
    }
}
