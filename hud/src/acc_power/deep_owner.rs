// SPDX-License-Identifier: Apache-2.0
//! Cycle-owned selection only. Android SystemSuspend remains the sole mem writer.
//! Called by the bounded preparation worker, never by the MCU reader.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use super::participants::DeepOwner;
use super::transport::boottime_ms;

const SELECTOR: &str = "/sys/power/mem_sleep";
const PARAMS: &str = "/sys/module/bengal_deep_suspend/parameters";
const PWRKEY: &str = "/sys/bus/platform/drivers/pm8941-pwrkey/1c40000.spmi:pmic@0:pon@800:pwrkey/power/wakeup";

fn small_text(path: &str) -> io::Result<String> {
    let mut bytes = Vec::new();
    File::open(path)?.take(257).read_to_end(&mut bytes)?;
    if bytes.len() > 256 { return Err(io::Error::other("oversized deep status")); }
    if bytes.last() == Some(&b'\n') { bytes.pop(); }
    if bytes.is_empty() || !bytes.iter().all(|v| (32..=126).contains(v)) {
        return Err(io::Error::other("invalid deep status bytes"));
    }
    String::from_utf8(bytes).map_err(io::Error::other)
}

/// Driver existence alone does not prove its actual device/OF binding.
fn bound(driver: &str, node: &str) -> bool {
    let Ok(expected_driver) = fs::canonicalize(driver) else { return false; };
    let Ok(expected_node) = fs::canonicalize(node) else { return false; };
    let Ok(entries) = fs::read_dir(driver) else { return false; };
    let mut matches = 0;
    for (n, entry) in entries.enumerate() {
        if n >= 128 { return false; }
        let Ok(entry) = entry else { return false; };
        let path = entry.path();
        if fs::canonicalize(path.join("driver")).is_ok_and(|p| p == expected_driver)
            && fs::canonicalize(path.join("of_node")).is_ok_and(|p| p == expected_node) {
            matches += 1;
        }
    }
    matches == 1
}

trait Platform: Send + 'static {
    fn now(&self) -> Option<u64>;
    fn eligible(&self) -> bool;
    fn selector(&self) -> Option<String>;
    fn select(&mut self, value: &str) -> bool;
}

struct Sysfs;
impl Platform for Sysfs {
    fn now(&self) -> Option<u64> { boottime_ms().ok() }
    fn eligible(&self) -> bool {
        let equals = |path: &str, expected: &str| small_text(path).is_ok_and(|v| v == expected);
        if !equals("/proc/sys/kernel/osrelease", "6.18.32-4k-gd84cfa4479d9")
            || !equals("/sys/devices/system/cpu/online", "0-7")
            || !equals(PWRKEY, "enabled") { return false; }
        for (name, value) in [
            ("guard_registered", "Y"), ("guard_coverage", "255"),
            ("guard_failed", "0"), ("ops_registered", "Y"),
            ("require_hooks", "Y"), ("force_register", "N"),
            ("state_name", "system-pc"), ("production_idle_guard", "Y"),
        ] {
            if !equals(&format!("{PARAMS}/{name}"), value) { return false; }
        }
        [
            ("/sys/bus/platform/drivers/qcom_apcs_ipc", "/sys/firmware/devicetree/base/soc@0/mailbox@f011000"),
            ("/sys/bus/platform/drivers/qcom_mpm", "/sys/firmware/devicetree/base/remoteproc/interrupt-controller"),
            ("/sys/bus/rpmsg/drivers/qcom_smd_rpm", "/sys/firmware/devicetree/base/remoteproc/glink-edge/rpm-requests"),
        ].iter().all(|(driver, node)| bound(driver, node))
    }
    fn selector(&self) -> Option<String> { small_text(SELECTOR).ok() }
    fn select(&mut self, value: &str) -> bool {
        // Never creates a file, and never opens /sys/power/state.
        OpenOptions::new().write(true).open(Path::new(SELECTOR))
            .and_then(|mut fd| fd.write_all(value.as_bytes())).is_ok()
    }
}

struct Selection<P: Platform> { platform: P, owner: Option<u64> }
impl<P: Platform> Selection<P> {
    fn live(&self, deadline: u64) -> bool {
        deadline != 0 && self.platform.now().is_some_and(|now| now < deadline)
    }
}
impl<P: Platform> DeepOwner for Selection<P> {
    fn arm(&mut self, token: u64, deadline: u64) -> bool {
        if token == 0 || !self.live(deadline) || !self.platform.eligible() { return false; }
        if let Some(owner) = self.owner {
            return owner == token && self.platform.selector().as_deref() == Some("s2idle [deep]")
                && self.live(deadline);
        }
        // Never adopt somebody else's deep selection as our lease.
        if self.platform.selector().as_deref() != Some("[s2idle] deep") { return false; }
        if !self.live(deadline) { return false; } // Eligibility reads may consume the budget.
        self.owner = Some(token); // Before I/O: failure may have changed the selector.
        self.platform.select("deep")
            && self.platform.selector().as_deref() == Some("s2idle [deep]")
            && self.platform.eligible() && self.live(deadline)
    }
    fn disarm(&mut self, token: u64) -> bool {
        match self.owner {
            None => true,
            Some(owner) if owner != token => false,
            Some(_) => {
                // Retry is allowed only for our uncertain selection; unrelated
                // owner tokens cannot release it. Do not claim rollback on error.
                if self.platform.selector().as_deref() != Some("[s2idle] deep")
                    && !self.platform.select("s2idle") { return false; }
                if self.platform.selector().as_deref() != Some("[s2idle] deep") { return false; }
                self.owner = None;
                true
            }
        }
    }
}

pub(crate) fn new() -> Box<dyn DeepOwner> {
    Box::new(Selection { platform: Sysfs, owner: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake { now: u64, eligible: bool, selected: String, writes: Vec<String>, fail: bool, delay: u64 }
    impl Platform for Fake {
        fn now(&self) -> Option<u64> { Some(self.now) }
        fn eligible(&self) -> bool { self.eligible }
        fn selector(&self) -> Option<String> { Some(self.selected.clone()) }
        fn select(&mut self, value: &str) -> bool {
            self.writes.push(value.into()); self.now += self.delay;
            self.selected = if value == "deep" { "s2idle [deep]" } else { "[s2idle] deep" }.into();
            !self.fail
        }
    }
    fn ready() -> Selection<Fake> {
        Selection { platform: Fake { now: 1, eligible: true, selected: "[s2idle] deep".into(), writes: vec![], fail: false, delay: 0 }, owner: None }
    }
    #[test] fn lease_and_stale_release() {
        let mut s = ready(); assert!(s.arm(7, 100)); assert!(!s.arm(8, 100));
        assert!(!s.disarm(8)); assert_eq!(s.platform.writes, ["deep"]);
        assert!(s.disarm(7)); assert_eq!(s.platform.writes, ["deep", "s2idle"]);
    }
    #[test] fn uncertain_failed_write_keeps_ownership() {
        let mut s = ready(); s.platform.fail = true; assert!(!s.arm(7, 100));
        assert_eq!(s.owner, Some(7)); assert!(!s.disarm(7));
        assert_eq!(s.owner, Some(7)); s.platform.fail = false; assert!(s.disarm(7));
    }
    #[test] fn late_arm_must_be_rolled_back() {
        let mut s = ready(); s.platform.delay = 200; assert!(!s.arm(7, 100));
        assert_eq!(s.owner, Some(7)); assert!(s.disarm(7));
        assert_eq!(s.platform.selected, "[s2idle] deep");
    }
    #[test] fn no_adoption_or_expired_entry() {
        let mut s = ready(); s.platform.selected = "s2idle [deep]".into();
        assert!(!s.arm(7, 100)); assert!(s.platform.writes.is_empty());
        s.platform.selected = "[s2idle] deep".into();
        assert!(!s.arm(0, 100)); assert!(!s.arm(7, 1)); s.platform.eligible = false;
        assert!(!s.arm(7, 100)); assert!(s.platform.writes.is_empty());
    }
    #[test] fn eligibility_read_expires_before_write() {
        use std::cell::Cell;
        struct Expiring { clock: Cell<u64>, wrote: bool }
        impl Platform for Expiring {
            fn now(&self) -> Option<u64> { Some(self.clock.get()) }
            fn eligible(&self) -> bool { self.clock.set(100); true }
            fn selector(&self) -> Option<String> { Some("[s2idle] deep".into()) }
            fn select(&mut self, _: &str) -> bool { self.wrote = true; true }
        }
        let mut s = Selection { platform: Expiring { clock: Cell::new(1), wrote: false }, owner: None };
        assert!(!s.arm(7, 100));
        assert!(!s.platform.wrote, "never issue selection already known to be expired");
        assert_eq!(s.owner, None);
    }
}
