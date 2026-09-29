// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Device-3 USB role, internal-hub power, and watchdog ownership.
//!
//! TLMM 85 selects the USB role (`LOW = host`). TLMM 34 controls only OTG
//! jack VBUS; it is not internal-hub power. The internal `1a40:0101` hub is
//! switched exclusively through `1-1-portN/disable`.

use crate::gpio::chardev::OutputLines;
use crate::mcu::serial::PlatformHooks;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// USB role-selector TLMM offset. A physical low selects host mode.
pub const USB_ROLE_LINE: u32 = 85;
/// OTG jack VBUS-out TLMM offset (not the internal hub supply).
pub const OTG_VBUS_LINE: u32 = 34;
/// Delay from daemon boot to its first hub health check.
pub const BOOT_CHECK_MS: u64 = 15_000;
/// Checks relative to every confirmed ACC-on transition.
pub const ACC_ON_CHECKS_MS: &[u64] = &[5_000, 20_000, 60_000];
/// Period between hub checks while ACC remains on.
pub const PERIODIC_CHECK_MS: u64 = 60_000;
/// Internal hub-port off interval during one recovery pulse.
pub const PORT_OFF_MS: u64 = 3_000;
/// First post-pulse enumeration delay.
pub const RECHECK_MS: u64 = 8_000;
/// Additional delay before declaring one pulse unsuccessful.
pub const RECHECK_EXTRA_MS: u64 = 6_000;
const MAX_PULSES: usize = 2;
const PULSE_BUDGET: usize = 2;
const PULSE_BUDGET_WINDOW_MS: u64 = 10 * 60_000;

/// Cached physical role selected through TLMM 85.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbRole {
    /// No role has been successfully applied by this owner yet.
    Unknown,
    /// TLMM 85 low.
    Host,
    /// TLMM 85 high.
    Device,
}

/// One USB `idVendor:idProduct` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbId {
    /// Four lower-case hexadecimal vendor digits.
    pub vendor: String,
    /// Four lower-case hexadecimal product digits.
    pub product: String,
}

impl UsbId {
    fn new(vendor: impl Into<String>, product: impl Into<String>) -> Self {
        Self {
            vendor: vendor.into(),
            product: product.into(),
        }
    }
}

fn expected_devices() -> [UsbId; 2] {
    [UsbId::new("0bda", "8153"), UsbId::new("214b", "7250")]
}

fn log_stick() -> UsbId {
    UsbId::new("0781", "5581")
}

/// One internal hub port and the direct child currently attached to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubPort {
    /// Sysfs directory basename, for example `1-1-port2`.
    pub name: String,
    /// Direct child's USB ID, when readable.
    pub device: Option<UsbId>,
    /// Current `disable` leaf value.
    pub disabled: bool,
}

impl HubPort {
    /// Numeric suffix from an internal `1-1-portN` name.
    pub fn number(&self) -> Option<u32> {
        self.name.strip_prefix("1-1-port")?.parse().ok()
    }

    /// True when this direct child is the diagnostic log stick.
    pub fn carries_log_stick(&self) -> bool {
        self.device.as_ref() == Some(&log_stick())
    }
}

/// The entire sysfs surface used by USB policy.
///
/// Tests implement this trait with an in-memory event log; they never see a
/// path and therefore cannot touch the desk's real sysfs tree.
pub trait HubSysfs {
    /// Enumerate internal-hub ports in stable name order.
    fn ports(&mut self) -> io::Result<Vec<HubPort>>;
    /// Whether any expected device is present anywhere in the USB tree.
    fn any_present(&mut self, expected: &[UsbId]) -> io::Result<bool>;
    /// Write one port's `disable` leaf (`true` means off).
    fn set_disabled(&mut self, port: &HubPort, disabled: bool) -> io::Result<()>;
}

/// Injectable blocking delay used only inside a watchdog pass.
pub trait Sleeper {
    /// Sleep for a deterministic duration.
    fn sleep(&mut self, duration: Duration);
}

/// Production sleeper. Desk tests use a recorder instead.
#[derive(Debug, Default)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Production internal-hub sysfs backend.
#[derive(Debug, Clone)]
pub struct LinuxHubSysfs {
    devices: PathBuf,
}

impl LinuxHubSysfs {
    /// Use `<sysfs_root>/bus/usb/devices`.
    pub fn new(sysfs_root: &Path) -> Self {
        Self {
            devices: sysfs_root.join("bus/usb/devices"),
        }
    }

    fn word(path: &Path) -> io::Result<String> {
        Ok(fs::read_to_string(path)?.trim().to_ascii_lowercase())
    }
}

impl HubSysfs for LinuxHubSysfs {
    fn ports(&mut self) -> io::Result<Vec<HubPort>> {
        let root = self.devices.join("1-1/1-1:1.0");
        let mut ports = Vec::new();
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(ports),
            Err(error) => return Err(io::Error::new(error.kind(), format!("{}: {error}", root.display()))),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("1-1-port") || !entry.path().join("disable").is_file() {
                continue;
            }
            let disabled = Self::word(&entry.path().join("disable"))? == "1";
            let device = fs::read_link(entry.path().join("device"))
                .ok()
                .and_then(|target| target.file_name().map(|name| name.to_owned()))
                .and_then(|name| {
                    let dir = self.devices.join(name);
                    Some(UsbId::new(
                        Self::word(&dir.join("idVendor")).ok()?,
                        Self::word(&dir.join("idProduct")).ok()?,
                    ))
                });
            ports.push(HubPort {
                name,
                device,
                disabled,
            });
        }
        ports.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(ports)
    }

    fn any_present(&mut self, expected: &[UsbId]) -> io::Result<bool> {
        for entry in fs::read_dir(&self.devices)? {
            let dir = entry?.path();
            let (Ok(vendor), Ok(product)) = (
                Self::word(&dir.join("idVendor")),
                Self::word(&dir.join("idProduct")),
            ) else {
                continue;
            };
            if expected
                .iter()
                .any(|want| want.vendor == vendor && want.product == product)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn set_disabled(&mut self, port: &HubPort, disabled: bool) -> io::Result<()> {
        fs::write(
            self.devices
                .join("1-1/1-1:1.0")
                .join(&port.name)
                .join("disable"),
            if disabled { b"1\n" } else { b"0\n" },
        )
    }
}

/// Kernel-owned board USB: observe the controller, never reclaim its GPIOs.
/// Init owns gadget teardown/setup when the optional role module is present.
pub struct KernelRole {
    root: PathBuf,
}

impl KernelRole {
    /// Paths are rooted for deterministic, hardware-free tests.
    pub fn new(sysfs: &Path) -> Self {
        Self { root: sysfs.to_owned() }
    }

    fn controller_role(&self) -> io::Result<UsbRole> {
        // The bound board driver publishes the last completed role transition.
        // Its setter waits for DWC3 before committing this value; the GPIO
        // readbacks below separately prove binding and the physical route.
        // Production HALs cannot read DWC3's root-only debugfs mode file.
        let params = self.root.join("module/bengal_usb_role/parameters");
        match fs::read_to_string(params.join("role")) {
            Ok(text) => {
                let enabled = fs::read_to_string(params.join("host_vbus"))?;
                if !matches!(enabled.trim(), "0" | "1") {
                    return Err(io::Error::new(io::ErrorKind::NotConnected,
                        "USB role controller is not bound"));
                }
                return Self::parse_role(&text);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        // Older fixed-role drops have no board role controller. The DT only
        // answers for a fixed host/peripheral mode; `otg` remains unknown.
        let path = self.root.join("kernel/debug/usb/4e00000.usb/mode");
        let text = fs::read_to_string(&path).or_else(|error| {
            if !matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied) {
                return Err(error);
            }
            fs::read_to_string(self.root.join("firmware/devicetree/base/soc@0/usb@4e00000/dr_mode"))
        })?;
        Self::parse_role(&text)
    }

    fn parse_role(text: &str) -> io::Result<UsbRole> {
        match text.trim_matches(|c: char| c == '\0' || c.is_whitespace()) {
            "host" => Ok(UsbRole::Host),
            "device" | "peripheral" => Ok(UsbRole::Device),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "USB controller role is unknown")),
        }
    }

    fn switch_available(&self) -> bool {
        // A loaded-but-unbound module also publishes parameters. Its measured
        // GPIO readback is -1 until probe owns the board, so file presence alone
        // must not activate the controls.
        self.root.join("module/bengal_usb_role/parameters/role").is_file()
            && matches!(std::fs::read_to_string(self.root.join("module/bengal_usb_role/parameters/host_vbus"))
                .ok().as_deref().map(str::trim), Some("0" | "1"))
    }

    fn limitation(&self) -> String {
        match self.controller_role() {
            Err(error) => format!("Cannot read USB controller role: {error}"),
            Ok(_) if self.switch_available() => String::new(),
            Ok(role) => format!("USB {} mode; switching is unavailable in this boot (no kernel role controller).",
                if role == UsbRole::Device { "device" } else { "host" }),
        }
    }

    fn set_role(&self, host: bool) -> io::Result<()> {
        let desired = if host { UsbRole::Host } else { UsbRole::Device };
        if !self.switch_available() {
            // A no-op remains harmless on a static-role boot. Do not save a
            // preference which this boot has no means to implement.
            if self.controller_role()? == desired { return Ok(()); }
            return Err(io::Error::new(io::ErrorKind::Unsupported, self.limitation()));
        }
        if self.role_confirmed(host)? { return Self::save_role(host); }
        // Do not write the module directly: init must unbind configfs first.
        write_usb_property(false, host)?;
        for _ in 0..40 {
            if self.role_confirmed(host)? { return Self::save_role(host); }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, "USB role request did not reach the controller"))
    }

    fn role_confirmed(&self, host: bool) -> io::Result<bool> {
        let params = self.root.join("module/bengal_usb_role/parameters");
        Ok(self.controller_role()? == if host { UsbRole::Host } else { UsbRole::Device }
            && fs::read_to_string(params.join("role"))?.trim() == if host { "host" } else { "device" }
            && fs::read_to_string(params.join("host_vbus"))?.trim() == "1"
            && fs::read_to_string(params.join("role_mux"))?.trim() == if host { "0" } else { "1" })
    }

    fn save_role(host: bool) -> io::Result<()> {
        write_usb_property(true, host).map_err(|error| io::Error::new(error.kind(),
            format!("USB role applied, but saving its boot preference failed: {error}")))
    }
}

// A HAL must not exec vendor toybox without a domain transition. Send exactly
// the same property request in-process; init remains the configfs/role owner.
// Public Bionic ABI: int __system_property_set(const char *, const char *).
#[cfg(target_os = "android")]
fn write_usb_property(persistent: bool, host: bool) -> io::Result<()> {
    unsafe extern "C" {
        fn __system_property_set(name: *const std::ffi::c_char,
                                 value: *const std::ffi::c_char) -> std::ffi::c_int;
    }
    write_usb_property_using(persistent, host, |name, value| {
        // SAFETY: both inputs are live, static, NUL-terminated C strings. The
        // property API consumes them synchronously and retains no pointers.
        unsafe { __system_property_set(name.as_ptr(), value.as_ptr()) }
    })
}

#[cfg(not(target_os = "android"))]
fn write_usb_property(_persistent: bool, _host: bool) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Android property service unavailable"))
}

#[cfg(any(target_os = "android", test))]
fn write_usb_property_using(persistent: bool, host: bool,
    write: impl FnOnce(&std::ffi::CStr, &std::ffi::CStr) -> i32) -> io::Result<()> {
    let name = if persistent { c"persist.vendor.bengal3.usb.role" } else { c"vendor.bengal3.usb.role" };
    let value = if host { c"host" } else { c"device" };
    if write(name, value) == 0 { Ok(()) } else {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "USB role property request was refused"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    Boot,
    AccOn,
    Periodic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Scheduled {
    due_ms: u64,
    trigger: Trigger,
}

/// Outcome of one watchdog pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WatchdogStatus {
    /// An expected LAN device or downstream hub enumerated.
    pub healthy: bool,
    /// Number of three-second port cycles performed.
    pub pulses: usize,
    /// A periodic repair was suppressed by the two-per-ten-minute budget.
    pub budget_exhausted: bool,
}

/// USB owner shared by the ACC callbacks and the daemon's timed poll.
pub struct UsbOwner<G, H, S> {
    gpio: G,
    hub: H,
    sleeper: S,
    acc_on: bool,
    schedule: Vec<Scheduled>,
    pulses: Vec<u64>,
    role: UsbRole,
    kernel_role: Option<KernelRole>,
}

impl<G: OutputLines, H: HubSysfs, S: Sleeper> UsbOwner<G, H, S> {
    /// Construct an ACC-on owner and arm the boot check for `now + 15 s`.
    pub fn new(gpio: G, hub: H, sleeper: S, now_ms: u64) -> Self {
        Self {
            gpio,
            hub,
            sleeper,
            acc_on: true,
            schedule: vec![
                Scheduled {
                    due_ms: now_ms.saturating_add(BOOT_CHECK_MS),
                    trigger: Trigger::Boot,
                },
                Scheduled {
                    due_ms: now_ms.saturating_add(PERIODIC_CHECK_MS),
                    trigger: Trigger::Periodic,
                },
            ],
            pulses: Vec::new(),
            role: UsbRole::Unknown,
            kernel_role: None,
        }
    }

    /// Production IHud uses the kernel owner, not the legacy raw-GPIO path.
    pub fn with_kernel_role(mut self, sysfs: &Path) -> Self {
        self.kernel_role = Some(KernelRole::new(sysfs));
        self.schedule.clear();
        self
    }

    /// The observed control source and any switching limitation.
    pub fn role_details(&self) -> (String, String) {
        match &self.kernel_role {
            Some(role) => ("DWC3 / kernel-owned USB".into(), role.limitation()),
            None => ("/dev/gpiochip0".into(), String::new()),
        }
    }

    /// Last successfully applied TLMM-85 role.
    pub fn role(&self) -> UsbRole {
        if let Some(role) = &self.kernel_role {
            return role.controller_role().unwrap_or(UsbRole::Unknown);
        }
        self.role
    }

    /// Drive the stock role mapping: 85 low/high for host/device, then 34 high.
    pub fn set_role(&mut self, host: bool) -> io::Result<()> {
        if let Some(role) = &self.kernel_role { return role.set_role(host); }
        let selector = self.gpio.drive(USB_ROLE_LINE, !host);
        let vbus = self.gpio.drive(OTG_VBUS_LINE, true);
        let result = selector.and(vbus);
        if result.is_ok() {
            self.role = if host { UsbRole::Host } else { UsbRole::Device };
        }
        result
    }

    /// Run one caller-triggered, unbudgeted hub health/recovery pass.
    pub fn check_hub(&mut self, now_ms: u64) -> io::Result<WatchdogStatus> {
        if let Some(role) = &self.kernel_role {
            if role.controller_role()? != UsbRole::Host {
                return Err(io::Error::new(io::ErrorKind::Unsupported,
                    "Not applicable: USB device mode disconnects the internal host hub; no reset attempted"));
            }
            // The old LAN-dongle IDs are device-1 accessories, not device-3
            // health requirements. Never power-cycle arbitrary user devices.
            let healthy = !self.hub.ports()?.is_empty()
                && self.hub.any_present(&[UsbId::new("1a40", "0101")])?;
            return Ok(WatchdogStatus { healthy, ..WatchdogStatus::default() });
        }
        self.watchdog(now_ms, false)
    }

    /// Read the current internal-hub port census.
    pub fn hub_ports(&mut self) -> io::Result<Vec<HubPort>> {
        if let Some(role) = &self.kernel_role {
            if role.controller_role()? != UsbRole::Host { return Ok(Vec::new()); }
        }
        self.hub.ports()
    }

    /// Set exactly one numbered internal port, refusing the log-stick port.
    pub fn set_hub_port_enabled(&mut self, port_number: u32, enabled: bool) -> io::Result<HubPort> {
        let mut port = self
            .hub_ports()?
            .into_iter()
            .find(|port| port.number() == Some(port_number))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown internal hub port"))?;
        if port.carries_log_stick() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "log-stick port is owned by ordered ACC-off quiescence",
            ));
        }
        self.hub.set_disabled(&port, !enabled)?;
        port.disabled = !enabled;
        Ok(port)
    }

    /// Restore every hub port, assert host role, and arm +5/+20/+60 checks.
    pub fn acc_on(&mut self, now_ms: u64) -> io::Result<()> {
        if self.kernel_role.is_some() { return Ok(()); } // IAccPowerChannel owns production USB PM.
        self.acc_on = true;
        self.schedule.retain(|item| item.trigger == Trigger::Boot);
        for offset in ACC_ON_CHECKS_MS {
            self.schedule.push(Scheduled {
                due_ms: now_ms.saturating_add(*offset),
                trigger: Trigger::AccOn,
            });
        }
        self.schedule.push(Scheduled {
            due_ms: now_ms.saturating_add(PERIODIC_CHECK_MS),
            trigger: Trigger::Periodic,
        });
        let ports_result = self
            .hub
            .ports()
            .and_then(|ports| self.set_ports(&ports, false));
        let role_result = self.set_role(true);
        ports_result.and(role_result)
    }

    /// Turn every internal-hub port off before sleep, then lower OTG VBUS.
    pub fn acc_off(&mut self) -> io::Result<()> {
        if self.kernel_role.is_some() { return Ok(()); }
        self.acc_on = false;
        self.schedule.clear();
        let ports_result = self
            .hub
            .ports()
            .and_then(|ports| self.set_ports(&ports, true));
        let vbus_result = self.gpio.drive(OTG_VBUS_LINE, false);
        ports_result.and(vbus_result)
    }

    /// Run all timed work due at `now_ms`.
    pub fn advance(&mut self, now_ms: u64) -> io::Result<Vec<WatchdogStatus>> {
        self.schedule.sort_by_key(|item| item.due_ms);
        let mut due = Vec::new();
        let mut future = Vec::new();
        for item in self.schedule.drain(..) {
            if item.due_ms <= now_ms {
                due.push(item);
            } else {
                future.push(item);
            }
        }
        self.schedule = future;
        let mut statuses = Vec::new();
        let mut acc_on_healthy = false;
        for item in due {
            if !self.acc_on {
                continue;
            }
            if item.trigger == Trigger::AccOn && acc_on_healthy {
                continue;
            }
            let status = self.watchdog(now_ms, item.trigger == Trigger::Periodic)?;
            if item.trigger == Trigger::AccOn && status.healthy {
                acc_on_healthy = true;
                self.schedule.retain(|next| next.trigger != Trigger::AccOn);
            }
            if item.trigger == Trigger::Periodic {
                self.schedule.push(Scheduled {
                    due_ms: item.due_ms.saturating_add(PERIODIC_CHECK_MS),
                    trigger: Trigger::Periodic,
                });
            }
            statuses.push(status);
        }
        Ok(statuses)
    }

    fn set_ports(&mut self, ports: &[HubPort], disabled: bool) -> io::Result<()> {
        let mut first_error = None;
        for port in ports {
            if let Err(error) = self.hub.set_disabled(port, disabled) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn watchdog(&mut self, now_ms: u64, budgeted: bool) -> io::Result<WatchdogStatus> {
        let expected = expected_devices();
        if self.hub.any_present(&expected)? {
            return Ok(WatchdogStatus {
                healthy: true,
                ..WatchdogStatus::default()
            });
        }
        self.pulses
            .retain(|at| now_ms.saturating_sub(*at) < PULSE_BUDGET_WINDOW_MS);
        let available = PULSE_BUDGET.saturating_sub(self.pulses.len());
        if budgeted && available == 0 {
            return Ok(WatchdogStatus {
                budget_exhausted: true,
                ..WatchdogStatus::default()
            });
        }
        let max_pulses = if budgeted {
            MAX_PULSES.min(available)
        } else {
            MAX_PULSES
        };
        let ports = self.hub.ports()?;
        let targets = ports_to_cycle(&ports, &expected);
        if targets.is_empty() {
            return Ok(WatchdogStatus::default());
        }
        let mut status = WatchdogStatus::default();
        for _ in 0..max_pulses {
            if let Err(error) = self.set_ports(&targets, true) {
                let _ = self.set_ports(&targets, false);
                return Err(error);
            }
            self.sleeper.sleep(Duration::from_millis(PORT_OFF_MS));
            self.set_ports(&targets, false)?;
            self.pulses.push(now_ms);
            status.pulses += 1;
            self.sleeper.sleep(Duration::from_millis(RECHECK_MS));
            if self.hub.any_present(&expected)? {
                status.healthy = true;
                return Ok(status);
            }
            self.sleeper.sleep(Duration::from_millis(RECHECK_EXTRA_MS));
            if self.hub.any_present(&expected)? {
                status.healthy = true;
                return Ok(status);
            }
        }
        Ok(status)
    }
}

fn ports_to_cycle(ports: &[HubPort], expected: &[UsbId]) -> Vec<HubPort> {
    let carrying: Vec<_> = ports
        .iter()
        .filter(|port| port.device.as_ref().is_some_and(|id| expected.contains(id)))
        .cloned()
        .collect();
    if !carrying.is_empty() {
        return carrying;
    }
    let stick = log_stick();
    ports
        .iter()
        .filter(|port| port.device.as_ref() != Some(&stick))
        .cloned()
        .collect()
}

/// ACC policy adapter: the state machine's final-stage USB hook disables all
/// ports, while resume restores host ownership and arms timed checks.
pub struct HudUsbHooks<G, H, S> {
    owner: UsbOwner<G, H, S>,
    now_ms: u64,
}

impl<G, H, S> HudUsbHooks<G, H, S> {
    /// Wrap a USB owner for use as `AccSession` platform hooks.
    pub fn new(owner: UsbOwner<G, H, S>) -> Self {
        Self { owner, now_ms: 0 }
    }
}

impl<G: OutputLines, H: HubSysfs, S: Sleeper> PlatformHooks for HudUsbHooks<G, H, S> {
    fn enter_acc_off_grace(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn quiesce_radio(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn detach_stick_sink(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn quiesce_usb(&mut self) -> io::Result<()> {
        self.owner.acc_off()
    }

    fn resume(&mut self) -> io::Result<()> {
        self.owner.acc_on(self.now_ms)
    }

    fn observe_time(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
    }

    fn run_due(&mut self, now_ms: u64) -> io::Result<()> {
        self.owner.advance(now_ms).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn role_properties_use_exact_native_service_keys_and_report_refusal() {
        for persistent in [false, true] {
            for host in [false, true] {
                let mut calls = 0;
                write_usb_property_using(persistent, host, |key, value| {
                    calls += 1;
                    assert_eq!(key.to_str().unwrap(), if persistent {
                        "persist.vendor.bengal3.usb.role" } else { "vendor.bengal3.usb.role" });
                    assert_eq!(value.to_str().unwrap(), if host { "host" } else { "device" });
                    0
                }).unwrap();
                assert_eq!(calls, 1);
                assert_eq!(write_usb_property_using(persistent, host, |_, _| -1)
                    .unwrap_err().kind(), io::ErrorKind::PermissionDenied);
            }
        }
    }

    #[test]
    fn absent_internal_hub_is_an_empty_census() {
        let dir = std::env::temp_dir().join(format!("hud-no-hub-{}", std::process::id()));
        fs::create_dir_all(dir.join("bus/usb/devices")).unwrap();
        let result = LinuxHubSysfs::new(&dir).ports();
        fs::remove_dir_all(dir).unwrap();
        assert_eq!(result.unwrap(), Vec::new());
    }

    #[test]
    fn kernel_owned_device_mode_never_claims_gpio_or_repairs_absent_hub() {
        let dir = std::env::temp_dir().join(format!("hud-device-role-{}", std::process::id()));
        let dt = dir.join("firmware/devicetree/base/soc@0/usb@4e00000");
        fs::create_dir_all(&dt).unwrap();
        fs::write(dt.join("dr_mode"), b"peripheral\0").unwrap();
        let mut usb = owner(HubMock::default()).with_kernel_role(&dir);
        assert_eq!(usb.role(), UsbRole::Device);
        assert!(usb.hub_ports().unwrap().is_empty());
        assert!(usb.check_hub(0).unwrap_err().to_string().contains("Not applicable"));
        assert_eq!(usb.set_role(true).unwrap_err().kind(), io::ErrorKind::Unsupported);
        usb.set_role(false).unwrap(); // Already applied, no electrical action.
        usb.acc_off().unwrap();
        usb.acc_on(1).unwrap();
        usb.advance(60_000).unwrap();
        assert!(usb.gpio.0.is_empty());
        assert!(usb.hub.events.is_empty());
        assert_eq!(usb.hub.health_checks, 0);
        assert!(usb.sleeper.0.is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn kernel_host_check_is_read_only_and_does_not_require_old_lan_accessories() {
        let dir = std::env::temp_dir().join(format!("hud-host-role-{}", std::process::id()));
        let mode = dir.join("kernel/debug/usb/4e00000.usb");
        fs::create_dir_all(&mode).unwrap();
        fs::write(mode.join("mode"), "host\n").unwrap();
        let mut usb = owner(HubMock {
            ports: vec![port("1-1-port1", None)],
            present: VecDeque::from([true, false]),
            ..HubMock::default()
        }).with_kernel_role(&dir);
        let status = usb.check_hub(0).unwrap();
        assert!(status.healthy);
        assert_eq!(status.pulses, 0);
        assert!(!usb.check_hub(1).unwrap().healthy);
        assert!(usb.hub.events.is_empty());
        assert!(usb.gpio.0.is_empty());
        fs::write(mode.join("mode"), "unrecognised\n").unwrap();
        assert_eq!(usb.role(), UsbRole::Unknown);
        assert!(usb.hub_ports().is_err());
        assert!(usb.set_role(true).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn kernel_role_reads_bound_owner_without_debugfs_access() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("hud-role-denied-{}", std::process::id()));
        let params = dir.join("module/bengal_usb_role/parameters");
        let debug = dir.join("kernel/debug/usb/4e00000.usb/mode");
        fs::create_dir_all(&params).unwrap();
        fs::create_dir_all(debug.parent().unwrap()).unwrap();
        fs::write(params.join("role"), "host").unwrap();
        fs::write(params.join("host_vbus"), "1").unwrap();
        fs::write(params.join("role_mux"), "0").unwrap();
        fs::write(&debug, "host\n").unwrap();
        fs::set_permissions(&debug, fs::Permissions::from_mode(0o0)).unwrap();
        let denied = fs::read_to_string(&debug).unwrap_err().kind();
        let mut usb = owner(HubMock::default()).with_kernel_role(&dir);
        let observed = usb.role();
        let detail = usb.role_details();
        let ports = usb.hub_ports();
        let confirmed = usb.kernel_role.as_ref().unwrap().role_confirmed(true);
        fs::set_permissions(&debug, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_dir_all(dir).unwrap();

        assert_eq!(denied, io::ErrorKind::PermissionDenied, "run as an unprivileged user");
        assert_eq!(observed, UsbRole::Host);
        assert!(detail.1.is_empty(), "{}", detail.1);
        assert!(ports.unwrap().is_empty());
        assert!(confirmed.unwrap());
        assert!(usb.gpio.0.is_empty());
    }

    #[test]
    fn inaccessible_debugfs_only_falls_back_to_a_fixed_dt_role() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("hud-static-denied-{}", std::process::id()));
        let debug = dir.join("kernel/debug/usb/4e00000.usb/mode");
        let dt = dir.join("firmware/devicetree/base/soc@0/usb@4e00000/dr_mode");
        fs::create_dir_all(debug.parent().unwrap()).unwrap();
        fs::create_dir_all(dt.parent().unwrap()).unwrap();
        fs::write(&debug, "device\n").unwrap();
        fs::set_permissions(&debug, fs::Permissions::from_mode(0o0)).unwrap();
        fs::write(&dt, b"peripheral\0").unwrap();
        let role = KernelRole::new(&dir);
        let fixed = role.controller_role();
        fs::write(&dt, b"otg\0").unwrap();
        let dynamic = role.controller_role();
        fs::set_permissions(&debug, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_dir_all(dir).unwrap();
        assert_eq!(fixed.unwrap(), UsbRole::Device);
        assert_eq!(dynamic.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn kernel_role_requires_bound_owner_and_confirmed_routing() {
        let dir = std::env::temp_dir().join(format!("hud-confirm-role-{}", std::process::id()));
        let params = dir.join("module/bengal_usb_role/parameters");
        let controller = dir.join("kernel/debug/usb/4e00000.usb");
        fs::create_dir_all(&params).unwrap();
        fs::create_dir_all(&controller).unwrap();
        fs::write(controller.join("mode"), "device\n").unwrap();
        fs::write(params.join("role"), "host\n").unwrap();
        fs::write(params.join("host_vbus"), "-1\n").unwrap();
        fs::write(params.join("role_mux"), "-1\n").unwrap();
        let role = KernelRole::new(&dir);
        assert!(!role.switch_available(), "loaded is not necessarily bound");
        assert_eq!(role.controller_role().unwrap_err().kind(), io::ErrorKind::NotConnected);
        fs::write(params.join("host_vbus"), "1\n").unwrap();
        assert!(role.switch_available());
        assert!(!role.role_confirmed(false).unwrap(), "controller alone is insufficient");
        fs::write(params.join("role"), "device\n").unwrap();
        fs::write(params.join("role_mux"), "0\n").unwrap();
        assert!(!role.role_confirmed(false).unwrap(), "mux must agree");
        fs::write(params.join("role_mux"), "1\n").unwrap();
        assert!(role.role_confirmed(false).unwrap());
        fs::write(params.join("host_vbus"), "0\n").unwrap();
        assert!(!role.role_confirmed(false).unwrap(), "suspended routing is not live");
        fs::write(controller.join("mode"), "host\n").unwrap();
        fs::write(params.join("role"), "host\n").unwrap();
        fs::write(params.join("role_mux"), "0\n").unwrap();
        fs::write(params.join("host_vbus"), "1\n").unwrap();
        assert!(role.role_confirmed(true).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }

    #[derive(Default)]
    struct GpioMock(Vec<(u32, bool)>);

    impl OutputLines for GpioMock {
        fn drive(&mut self, offset: u32, high: bool) -> io::Result<()> {
            self.0.push((offset, high));
            Ok(())
        }
    }

    #[derive(Default)]
    struct HubMock {
        ports: Vec<HubPort>,
        present: VecDeque<bool>,
        events: Vec<String>,
        health_checks: usize,
    }

    impl HubSysfs for HubMock {
        fn ports(&mut self) -> io::Result<Vec<HubPort>> {
            Ok(self.ports.clone())
        }

        fn any_present(&mut self, _expected: &[UsbId]) -> io::Result<bool> {
            self.health_checks += 1;
            Ok(self.present.pop_front().unwrap_or(false))
        }

        fn set_disabled(&mut self, port: &HubPort, disabled: bool) -> io::Result<()> {
            self.events
                .push(format!("{}={}", port.name, if disabled { 1 } else { 0 }));
            Ok(())
        }
    }

    #[derive(Default)]
    struct SleepMock(Vec<u64>);

    impl Sleeper for SleepMock {
        fn sleep(&mut self, duration: Duration) {
            self.0.push(duration.as_millis() as u64);
        }
    }

    fn port(name: &str, id: Option<(&str, &str)>) -> HubPort {
        HubPort {
            name: name.into(),
            device: id.map(|(vendor, product)| UsbId::new(vendor, product)),
            disabled: false,
        }
    }

    fn owner(hub: HubMock) -> UsbOwner<GpioMock, HubMock, SleepMock> {
        UsbOwner::new(GpioMock::default(), hub, SleepMock::default(), 0)
    }

    #[test]
    fn go_role_sequence_is_85_then_34_with_low_meaning_host() {
        let mut usb = owner(HubMock::default());
        assert_eq!(usb.role(), UsbRole::Unknown);
        usb.set_role(true).unwrap();
        usb.set_role(false).unwrap();
        assert_eq!(usb.role(), UsbRole::Device);
        assert_eq!(
            usb.gpio.0,
            vec![(85, false), (34, true), (85, true), (34, true)]
        );
    }

    #[test]
    fn explicit_port_control_uses_sysfs_and_refuses_the_log_stick() {
        let hub = HubMock {
            ports: vec![
                port("1-1-port1", Some(("0781", "5581"))),
                port("1-1-port2", None),
            ],
            ..HubMock::default()
        };
        let mut usb = owner(hub);
        let changed = usb.set_hub_port_enabled(2, false).unwrap();
        assert_eq!(changed.number(), Some(2));
        assert!(changed.disabled);
        assert_eq!(usb.hub.events, ["1-1-port2=1"]);

        let error = usb.set_hub_port_enabled(1, false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(usb.hub.events, ["1-1-port2=1"]);
    }

    #[test]
    fn acc_off_disables_every_port_before_lowering_otg_vbus() {
        let hub = HubMock {
            ports: vec![
                port("1-1-port1", Some(("0781", "5581"))),
                port("1-1-port2", None),
            ],
            ..HubMock::default()
        };
        let mut usb = owner(hub);
        usb.acc_off().unwrap();
        assert_eq!(usb.hub.events, ["1-1-port1=1", "1-1-port2=1"]);
        assert_eq!(usb.gpio.0, [(34, false)]);
        assert!(usb.schedule.is_empty());
    }

    #[test]
    fn acc_on_restores_all_ports_then_host_role_and_arms_go_offsets() {
        let hub = HubMock {
            ports: vec![port("1-1-port1", None), port("1-1-port2", None)],
            ..HubMock::default()
        };
        let mut usb = owner(hub);
        usb.acc_off().unwrap();
        usb.hub.events.clear();
        usb.gpio.0.clear();
        usb.acc_on(100).unwrap();
        assert_eq!(usb.hub.events, ["1-1-port1=0", "1-1-port2=0"]);
        assert_eq!(usb.gpio.0, [(85, false), (34, true)]);
        let mut due: Vec<_> = usb.schedule.iter().map(|item| item.due_ms).collect();
        due.sort_unstable();
        assert_eq!(due, [5_100, 20_100, 60_100, 60_100]);
    }

    #[test]
    fn watchdog_cycles_expected_port_with_go_sleep_sequence() {
        let hub = HubMock {
            ports: vec![
                port("1-1-port1", Some(("0781", "5581"))),
                port("1-1-port2", Some(("0bda", "8153"))),
                port("1-1-port3", None),
            ],
            present: VecDeque::from([false, false, false, false, false]),
            ..HubMock::default()
        };
        let mut usb = owner(hub);
        let status = usb.watchdog(0, false).unwrap();
        assert_eq!(status.pulses, 2);
        assert_eq!(
            usb.hub.events,
            ["1-1-port2=1", "1-1-port2=0", "1-1-port2=1", "1-1-port2=0"]
        );
        assert_eq!(usb.sleeper.0, [3_000, 8_000, 6_000, 3_000, 8_000, 6_000]);
        assert!(usb.gpio.0.is_empty(), "GPIO 34 is not hub power");
    }

    #[test]
    fn missing_expected_device_cycles_every_port_except_log_stick() {
        let hub = HubMock {
            ports: vec![
                port("1-1-port1", Some(("0781", "5581"))),
                port("1-1-port2", None),
                port("1-1-port3", None),
            ],
            present: VecDeque::from([false, true]),
            ..HubMock::default()
        };
        let mut usb = owner(hub);
        let status = usb.watchdog(0, false).unwrap();
        assert_eq!(
            status,
            WatchdogStatus {
                healthy: true,
                pulses: 1,
                budget_exhausted: false
            }
        );
        assert_eq!(
            usb.hub.events,
            ["1-1-port2=1", "1-1-port3=1", "1-1-port2=0", "1-1-port3=0"]
        );
    }

    #[test]
    fn no_port_nodes_never_falls_back_to_otg_vbus() {
        let mut usb = owner(HubMock {
            present: VecDeque::from([false]),
            ..HubMock::default()
        });
        assert_eq!(usb.watchdog(0, false).unwrap().pulses, 0);
        assert!(usb.gpio.0.is_empty());
        assert!(usb.sleeper.0.is_empty());
    }

    #[test]
    fn boot_acc_on_and_periodic_schedule_matches_go_sequences() {
        let mut usb = owner(HubMock::default());
        assert_eq!(
            usb.schedule
                .iter()
                .map(|item| item.due_ms)
                .collect::<Vec<_>>(),
            [15_000, 60_000]
        );
        assert!(usb.advance(14_999).unwrap().is_empty());
        assert_eq!(usb.advance(15_000).unwrap().len(), 1);
        usb.acc_on(20_000).unwrap();
        assert_eq!(usb.advance(24_999).unwrap().len(), 0);
        assert_eq!(usb.advance(25_000).unwrap().len(), 1);
        assert_eq!(usb.advance(40_000).unwrap().len(), 1);
        assert_eq!(usb.advance(80_000).unwrap().len(), 2); // +60 and periodic
        assert_eq!(usb.advance(140_000).unwrap().len(), 1);
        assert_eq!(usb.hub.health_checks, 6);
    }

    #[test]
    fn acc_platform_callbacks_own_quiesce_resume_and_due_work() {
        let hub = HubMock {
            ports: vec![port("1-1-port1", None), port("1-1-port2", None)],
            present: VecDeque::from([true]),
            ..HubMock::default()
        };
        let mut hooks = HudUsbHooks::new(owner(hub));
        hooks.observe_time(1_000);
        hooks.quiesce_usb().unwrap();
        assert_eq!(hooks.owner.hub.events, ["1-1-port1=1", "1-1-port2=1"]);
        hooks.owner.hub.events.clear();
        hooks.resume().unwrap();
        assert_eq!(hooks.owner.hub.events, ["1-1-port1=0", "1-1-port2=0"]);
        hooks.run_due(6_000).unwrap();
        assert_eq!(hooks.owner.hub.health_checks, 1);
    }

    #[test]
    fn healthy_acc_on_check_cancels_later_acc_on_checks() {
        let mut usb = owner(HubMock {
            present: VecDeque::from([true]),
            ..HubMock::default()
        });
        usb.schedule.clear();
        usb.acc_on(0).unwrap();
        assert!(usb.advance(5_000).unwrap()[0].healthy);
        assert_eq!(usb.advance(60_000).unwrap().len(), 1, "periodic remains");
    }

    #[test]
    fn periodic_pulses_share_two_per_ten_minute_budget() {
        let hub = HubMock {
            ports: vec![port("1-1-port2", None)],
            ..HubMock::default()
        };
        let mut usb = owner(hub);
        assert_eq!(usb.watchdog(0, true).unwrap().pulses, 2);
        assert!(usb.watchdog(60_000, true).unwrap().budget_exhausted);
        assert_eq!(usb.watchdog(600_000, true).unwrap().pulses, 2);
    }
}
