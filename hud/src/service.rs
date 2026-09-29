// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Host-testable implementation core for `vendor.xyauto.hud.IHud`.
//!
//! The types in this module mirror the V1 AIDL without depending on generated
//! Binder code. [`HudService`] is generic over the existing USB owner and a
//! read-only GPIO census source, so default-feature Cargo tests exercise the
//! same state transitions and hardware traits the Android adapter calls.

use crate::gpio;
use crate::gpio::chardev::OutputLines;
use crate::mcu::acc::State;
use crate::mcu::{ButtonState, Event};
use crate::usb::{HubPort, HubSysfs, Sleeper, UsbOwner, UsbRole, WatchdogStatus};
use std::collections::VecDeque;
use std::io;

/// Maximum number of diagnostic events retained, matching the bridge oracle.
pub const EVENT_RING_CAPACITY: usize = 500;

/// Confirmed lifecycle state exposed to Android.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccState {
    /// No confirmed ignition state yet.
    Unknown,
    /// Ignition is on, including a completed resume.
    On,
    /// MCU grace period after `12 03`.
    OffGrace,
    /// Radio/USB quiescence after `05 10`.
    FinalStage,
    /// Fixed completion emitted and suspend requested.
    Suspending,
}

impl From<State> for AccState {
    fn from(value: State) -> Self {
        match value {
            State::Idle => Self::Unknown,
            State::AccOffGrace => Self::OffGrace,
            State::FinalStage => Self::FinalStage,
            State::Suspending => Self::Suspending,
            State::Resumed => Self::On,
        }
    }
}

/// One confirmed ACC transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccStateEvent {
    /// Monotonic event sequence.
    pub sequence: u64,
    /// Daemon-relative event time.
    pub elapsed_realtime_ms: u64,
    /// New confirmed state.
    pub state: AccState,
    /// Stable transition reason.
    pub reason: String,
}

/// USB role exposed by V1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbMode {
    /// No successful role application has been observed.
    Unknown,
    /// Host role, TLMM 85 low.
    Host,
    /// Device role, TLMM 85 high.
    Device,
}

impl From<UsbRole> for UsbMode {
    fn from(value: UsbRole) -> Self {
        match value {
            UsbRole::Unknown => Self::Unknown,
            UsbRole::Host => Self::Host,
            UsbRole::Device => Self::Device,
        }
    }
}

/// Result of reading or applying USB role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbModeStatus {
    /// Last successfully applied role.
    pub mode: UsbMode,
    /// GPIO chip used by the production backend.
    pub chip: String,
    /// Daemon-relative application time.
    pub applied_elapsed_realtime_ms: u64,
    /// Empty on success, otherwise the hardware error.
    pub error: String,
}

/// One internal hub port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubPortState {
    /// Numeric `1-1-portN` suffix.
    pub port_number: u32,
    /// Sysfs port basename.
    pub sysfs_name: String,
    /// Inverse of the sysfs `disable` value.
    pub enabled: bool,
    /// Direct child path/name when present.
    pub device_path: String,
    /// Direct child USB vendor ID.
    pub vendor_id: String,
    /// Direct child USB product ID.
    pub product_id: String,
    /// Whether the direct child is the diagnostic log stick.
    pub log_stick: bool,
}

impl From<HubPort> for HubPortState {
    fn from(port: HubPort) -> Self {
        let number = port.number().unwrap_or(0);
        let log_stick = port.carries_log_stick();
        let (vendor_id, product_id) = port
            .device
            .as_ref()
            .map(|id| (id.vendor.clone(), id.product.clone()))
            .unwrap_or_default();
        Self {
            port_number: number,
            sysfs_name: port.name.clone(),
            enabled: !port.disabled,
            device_path: port
                .device
                .as_ref()
                .map(|_| port.name.clone())
                .unwrap_or_default(),
            vendor_id,
            product_id,
            log_stick,
        }
    }
}

/// Outcome of one bounded hub watchdog pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbHubStatus {
    /// Caller that initiated the pass.
    pub trigger: String,
    /// Whether an expected downstream device is present.
    pub healthy: bool,
    /// Number of three-second port pulses used.
    pub pulses: usize,
    /// Port census after the pass.
    pub ports: Vec<HubPortState>,
    /// Whether the periodic pulse budget was exhausted.
    pub budget_exhausted: bool,
    /// Empty on success, otherwise the hardware error.
    pub error: String,
}

/// Direction reported by a GPIO census source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpioDirection {
    /// Direction was not read.
    Unknown,
    /// Input.
    Input,
    /// Output.
    Output,
}

/// One non-mutating GPIO census row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpioLineInfo {
    /// TLMM offset.
    pub offset: u32,
    /// Kernel line name.
    pub name: String,
    /// Current consumer.
    pub consumer: String,
    /// Current direction.
    pub direction: GpioDirection,
    /// Physical level, or -1 when unavailable.
    pub value: i32,
    /// Whether a kernel consumer owns the line.
    pub used: bool,
    /// Active-low line-info flag.
    pub active_low: bool,
    /// Project label.
    pub label: String,
    /// Project meaning/evidence.
    pub meaning: String,
    /// Project subsystem group.
    pub group: String,
    /// Current pinmux function.
    pub function: String,
    /// Source of the dynamic value.
    pub source: String,
    /// Whether `bengal-hud` owns the line.
    pub daemon_owned: bool,
}

/// Read-only GPIO census result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpioCensus {
    /// gpiochip node.
    pub chip: String,
    /// gpiochip label.
    pub chip_label: String,
    /// Stable line order.
    pub lines: Vec<GpioLineInfo>,
    /// Non-fatal limitations.
    pub warnings: Vec<String>,
}

/// Source for V1's non-mutating GPIO census.
pub trait GpioCensusSource {
    /// Read the current census.
    fn census(&mut self) -> io::Result<GpioCensus>;
}

/// Curated device-3 inventory enriched with fresh, read-only kernel metadata.
#[derive(Debug, Default)]
pub struct InventoryGpioCensus;

impl GpioCensusSource for InventoryGpioCensus {
    fn census(&mut self) -> io::Result<GpioCensus> {
        Ok(gpio::census::read())
    }
}

/// Stable high-level event class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HudEventType {
    /// Confirmed ACC transition.
    Acc,
    /// Semantic MCU button/knob event.
    McuKey,
    /// USB role application.
    UsbMode,
    /// USB hub action.
    UsbHub,
    /// GPIO census/action.
    Gpio,
    /// Non-fatal warning.
    Warning,
}

/// One bounded diagnostic event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HudEvent {
    /// Monotonic event sequence.
    pub sequence: u64,
    /// Daemon-relative event time.
    pub elapsed_realtime_ms: u64,
    /// Event class.
    pub event_type: HudEventType,
    /// Operation name.
    pub command: String,
    /// Resulting state.
    pub state: String,
    /// Trigger/reason.
    pub reason: String,
    /// Empty on success, otherwise the error.
    pub error: String,
}

/// Semantic MCU key action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McuKeyAction {
    /// Unknown/unsupported action.
    Unknown,
    /// Button press.
    Press,
    /// Button release.
    Release,
    /// Rotary movement toward left/down.
    RotateLeft,
    /// Rotary movement toward right/up.
    RotateRight,
}

/// One decoded MCU key/knob event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McuKeyEvent {
    /// Monotonic event sequence.
    pub sequence: u64,
    /// Daemon-relative event time.
    pub elapsed_realtime_ms: u64,
    /// Raw MCU code.
    pub raw_code: u8,
    /// Recovered semantic name.
    pub name: String,
    /// Semantic action.
    pub action: McuKeyAction,
    /// Suggested Android `KeyEvent` code; zero means app policy decides.
    pub android_key_code: i32,
    /// Rotary acceleration, or one for a button.
    pub magnitude: u8,
}

/// Cached daemon status returned without hardware I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HudStatus {
    /// Daemon start relative to its own timebase (normally zero).
    pub started_elapsed_realtime_ms: u64,
    /// Whether the MCU stream owner reports connected.
    pub mcu_connected: bool,
    /// Complete frames seen.
    pub frame_count: u64,
    /// Checksum-valid frames seen.
    pub valid_frames: u64,
    /// Invalid frames seen.
    pub invalid_frames: u64,
    /// Most recent high-level event sequence.
    pub last_event_sequence: u64,
    /// Confirmed ACC state.
    pub acc_state: AccState,
}

/// Callback seam shared by host tests and the Android Binder adapter.
pub trait HudCallback: Send + Sync {
    /// Stable identity used by unsubscribe.
    fn identity(&self) -> u64;
    /// Confirmed ACC transition.
    fn on_acc_state_changed(&self, event: &AccStateEvent);
    /// Semantic MCU key/knob event.
    fn on_mcu_key_event(&self, event: &McuKeyEvent);
    /// Diagnostic event stream.
    fn on_event(&self, event: &HudEvent);
}

/// USB surface consumed by [`HudService`].
pub trait UsbHardware {
    /// Backend identity and current role-control limitation.
    fn role_details(&self) -> (String, String) { ("/dev/gpiochip0".into(), String::new()) }
    /// Cached role.
    fn role(&self) -> UsbRole;
    /// Apply host/device role.
    fn set_role(&mut self, host: bool) -> io::Result<()>;
    /// Run one bounded watchdog pass.
    fn check_hub(&mut self, now_ms: u64) -> io::Result<WatchdogStatus>;
    /// Current port census.
    fn hub_ports(&mut self) -> io::Result<Vec<HubPort>>;
    /// Apply one port state.
    fn set_hub_port_enabled(&mut self, port: u32, enabled: bool) -> io::Result<HubPort>;
    /// Ordered ACC-off USB quiescence.
    fn acc_off(&mut self) -> io::Result<()>;
    /// Ordered ACC-on USB restoration.
    fn acc_on(&mut self, now_ms: u64) -> io::Result<()>;
}

impl<G: OutputLines, H: HubSysfs, S: Sleeper> UsbHardware for UsbOwner<G, H, S> {
    fn role_details(&self) -> (String, String) { UsbOwner::role_details(self) }
    fn role(&self) -> UsbRole {
        UsbOwner::role(self)
    }

    fn set_role(&mut self, host: bool) -> io::Result<()> {
        UsbOwner::set_role(self, host)
    }

    fn check_hub(&mut self, now_ms: u64) -> io::Result<WatchdogStatus> {
        UsbOwner::check_hub(self, now_ms)
    }

    fn hub_ports(&mut self) -> io::Result<Vec<HubPort>> {
        UsbOwner::hub_ports(self)
    }

    fn set_hub_port_enabled(&mut self, port: u32, enabled: bool) -> io::Result<HubPort> {
        UsbOwner::set_hub_port_enabled(self, port, enabled)
    }

    fn acc_off(&mut self) -> io::Result<()> {
        UsbOwner::acc_off(self)
    }

    fn acc_on(&mut self, now_ms: u64) -> io::Result<()> {
        UsbOwner::acc_on(self, now_ms)
    }
}

/// Host-visible equivalent of every `IHud` V1 method.
pub trait HudApi {
    /// Cached daemon status.
    fn get_status(&self) -> HudStatus;
    /// Register/replace the HU callback and immediately send current ACC state.
    fn subscribe(&mut self, callback: Box<dyn HudCallback>, now_ms: u64);
    /// Remove the callback only if its identity matches.
    fn unsubscribe(&mut self, identity: u64);
    /// Bounded recent events, oldest first.
    fn get_recent_events(&self, max_count: i32) -> Vec<HudEvent>;
    /// Cached USB role.
    fn get_usb_mode(&self) -> UsbModeStatus;
    /// Apply USB role.
    fn set_usb_mode(&mut self, mode: UsbMode, now_ms: u64) -> UsbModeStatus;
    /// Run one hub watchdog pass.
    fn check_usb_hub(&mut self, now_ms: u64) -> UsbHubStatus;
    /// Read hub ports.
    fn get_hub_ports(&mut self) -> io::Result<Vec<HubPortState>>;
    /// Enable/disable one hub port.
    fn set_hub_port_enabled(&mut self, port: u32, enabled: bool) -> io::Result<HubPortState>;
    /// Read GPIO census.
    fn get_gpio_census(&mut self) -> io::Result<GpioCensus>;
}

/// Stateful, dependency-injected implementation of the complete HUD V1 API.
pub struct HudService<U, G> {
    /// Separate fixed metadata snapshot; dumping it never holds the service lock.
    #[cfg_attr(not(feature = "binder"), allow(dead_code))]
    pub(crate) mcu_diagnostics: std::sync::Arc<crate::mcu::diagnostics::Diagnostics>,
    acc_state: AccState,
    usb: U,
    gpio: G,
    callback: Option<Box<dyn HudCallback>>,
    events: VecDeque<HudEvent>,
    next_sequence: u64,
    mcu_connected: bool,
    frame_count: u64,
    valid_frames: u64,
    invalid_frames: u64,
    usb_applied_at_ms: u64,
}

impl<U: UsbHardware, G: GpioCensusSource> HudService<U, G> {
    /// Create a service around the sole USB owner and a read-only GPIO source.
    pub fn new(usb: U, gpio: G) -> Self {
        Self {
            mcu_diagnostics: std::sync::Arc::new(crate::mcu::diagnostics::Diagnostics::default()),
            acc_state: AccState::Unknown,
            usb,
            gpio,
            callback: None,
            events: VecDeque::with_capacity(EVENT_RING_CAPACITY),
            next_sequence: 0,
            mcu_connected: false,
            frame_count: 0,
            valid_frames: 0,
            invalid_frames: 0,
            usb_applied_at_ms: 0,
        }
    }

    /// Mark whether the serial owner currently holds an open MCU stream.
    pub fn set_mcu_connected(&mut self, connected: bool) {
        self.mcu_connected = connected;
    }

    /// Count one bad frame without presenting it as a decoded event.
    pub fn record_invalid_frame(&mut self) {
        self.frame_count = self.frame_count.saturating_add(1);
        self.invalid_frames = self.invalid_frames.saturating_add(1);
    }

    /// Record one checksum-valid decoded MCU event. The UART reader's
    /// [`crate::mcu::serial::AccSession`] is the only ACC state-machine owner;
    /// this service only publishes its already-confirmed transitions.
    pub fn record_mcu_event(&mut self, event: &Event, now_ms: u64) {
        self.frame_count = self.frame_count.saturating_add(1);
        self.valid_frames = self.valid_frames.saturating_add(1);
        let key = match event {
            Event::Button { .. } | Event::Knob { .. } => {
                key_event(event, now_ms, self.allocate_sequence())
            }
            _ => None,
        };
        if let Some(key) = key {
            if let Some(callback) = self.callback.as_ref() {
                callback.on_mcu_key_event(&key);
            }
            self.record_event(HudEvent {
                sequence: key.sequence,
                elapsed_realtime_ms: now_ms,
                event_type: HudEventType::McuKey,
                command: "mcu-key".into(),
                state: format!("{:?}", key.action),
                reason: key.name.clone(),
                error: String::new(),
            });
        }
    }

    /// Publish a state transition received from the sole UART-owned machine.
    /// Binder callbacks remain queued-only through the adapter callback seam.
    pub fn publish_acc_state(&mut self, state: AccState, reason: &str, now_ms: u64) {
        if self.acc_state == state {
            return;
        }
        self.acc_state = state;
        let event = AccStateEvent {
            sequence: self.allocate_sequence(),
            elapsed_realtime_ms: now_ms,
            state,
            reason: reason.into(),
        };
        if let Some(callback) = self.callback.as_ref() {
            callback.on_acc_state_changed(&event);
        }
        self.record_event(HudEvent {
            sequence: event.sequence,
            elapsed_realtime_ms: now_ms,
            event_type: HudEventType::Acc,
            command: "acc-state".into(),
            state: format!("{:?}", event.state),
            reason: event.reason,
            error: String::new(),
        });
    }

    fn allocate_sequence(&mut self) -> u64 {
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.next_sequence
    }

    fn record_event(&mut self, event: HudEvent) {
        if self.events.len() == EVENT_RING_CAPACITY {
            self.events.pop_front();
        }
        if let Some(callback) = self.callback.as_ref() {
            callback.on_event(&event);
        }
        self.events.push_back(event);
    }

    fn mode_status(&self, error: String) -> UsbModeStatus {
        let (chip, limitation) = self.usb.role_details();
        UsbModeStatus {
            mode: self.usb.role().into(),
            chip,
            applied_elapsed_realtime_ms: self.usb_applied_at_ms,
            error: if error.is_empty() { limitation } else { error },
        }
    }
}

impl<U: UsbHardware, G: GpioCensusSource> HudApi for HudService<U, G> {
    fn get_status(&self) -> HudStatus {
        HudStatus {
            started_elapsed_realtime_ms: 0,
            mcu_connected: self.mcu_connected,
            frame_count: self.frame_count,
            valid_frames: self.valid_frames,
            invalid_frames: self.invalid_frames,
            last_event_sequence: self.next_sequence,
            acc_state: self.acc_state,
        }
    }

    fn subscribe(&mut self, callback: Box<dyn HudCallback>, now_ms: u64) {
        callback.on_acc_state_changed(&AccStateEvent {
            sequence: self.next_sequence,
            elapsed_realtime_ms: now_ms,
            state: self.acc_state,
            reason: "subscribe".into(),
        });
        self.callback = Some(callback);
    }

    fn unsubscribe(&mut self, identity: u64) {
        if self
            .callback
            .as_ref()
            .is_some_and(|callback| callback.identity() == identity)
        {
            self.callback = None;
        }
    }

    fn get_recent_events(&self, max_count: i32) -> Vec<HudEvent> {
        let count = if max_count <= 0 {
            self.events.len()
        } else {
            (max_count as usize).min(self.events.len())
        };
        self.events
            .iter()
            .skip(self.events.len().saturating_sub(count))
            .cloned()
            .collect()
    }

    fn get_usb_mode(&self) -> UsbModeStatus {
        self.mode_status(String::new())
    }

    fn set_usb_mode(&mut self, mode: UsbMode, now_ms: u64) -> UsbModeStatus {
        let result = match mode {
            UsbMode::Host => self.usb.set_role(true),
            UsbMode::Device => self.usb.set_role(false),
            UsbMode::Unknown => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "UNKNOWN is not an applicable USB role",
            )),
        };
        let error = result
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        if error.is_empty() {
            self.usb_applied_at_ms = now_ms;
        }
        let status = self.mode_status(error.clone());
        let sequence = self.allocate_sequence();
        self.record_event(HudEvent {
            sequence,
            elapsed_realtime_ms: now_ms,
            event_type: HudEventType::UsbMode,
            command: "set-usb-mode".into(),
            state: format!("{:?}", status.mode),
            reason: String::new(),
            error,
        });
        status
    }

    fn check_usb_hub(&mut self, now_ms: u64) -> UsbHubStatus {
        let (watchdog, error) = match self.usb.check_hub(now_ms) {
            Ok(status) => (status, String::new()),
            Err(error) => (WatchdogStatus::default(), error.to_string()),
        };
        let ports = self.get_hub_ports().unwrap_or_default();
        let status = UsbHubStatus {
            trigger: "api".into(),
            healthy: watchdog.healthy,
            pulses: watchdog.pulses,
            ports,
            budget_exhausted: watchdog.budget_exhausted,
            error: error.clone(),
        };
        let sequence = self.allocate_sequence();
        self.record_event(HudEvent {
            sequence,
            elapsed_realtime_ms: now_ms,
            event_type: HudEventType::UsbHub,
            command: "check-usb-hub".into(),
            state: if status.healthy { "healthy" } else { "missing" }.into(),
            reason: format!("pulses={}", status.pulses),
            error,
        });
        status
    }

    fn get_hub_ports(&mut self) -> io::Result<Vec<HubPortState>> {
        Ok(self
            .usb
            .hub_ports()?
            .into_iter()
            .map(HubPortState::from)
            .collect())
    }

    fn set_hub_port_enabled(&mut self, port: u32, enabled: bool) -> io::Result<HubPortState> {
        self.usb
            .set_hub_port_enabled(port, enabled)
            .map(HubPortState::from)
    }

    fn get_gpio_census(&mut self) -> io::Result<GpioCensus> {
        self.gpio.census()
    }
}

fn key_event(event: &Event, now_ms: u64, sequence: u64) -> Option<McuKeyEvent> {
    match event {
        Event::Button { code, state, name } => Some(McuKeyEvent {
            sequence,
            elapsed_realtime_ms: now_ms,
            raw_code: *code,
            name: format!(
                "raw-button-0x{code:02x} (device3-map=PROVISIONAL; device1-oracle={name})"
            ),
            action: match state {
                ButtonState::Press => McuKeyAction::Press,
                ButtonState::Release => McuKeyAction::Release,
                ButtonState::Other(_) => McuKeyAction::Unknown,
            },
            // Device 1's code table does not transfer to this five-button
            // panel. Keep policy neutral until Capture B confirms the map.
            android_key_code: 0,
            magnitude: 1,
        }),
        Event::Knob {
            code,
            magnitude,
            name,
        } => Some(McuKeyEvent {
            sequence,
            elapsed_realtime_ms: now_ms,
            raw_code: *code,
            name: format!(
                "raw-rotary-0x{code:02x} (device3-map=PROVISIONAL; device1-oracle={name})"
            ),
            action: if matches!(*code, 0x0d | 0x03) {
                McuKeyAction::RotateLeft
            } else {
                McuKeyAction::RotateRight
            },
            // Device 3 has no physical knobs; preserve the oracle event for
            // diagnostics without assigning Android policy to it.
            android_key_code: 0,
            magnitude: *magnitude,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpio::chardev::OutputLines;
    use crate::usb::{HubSysfs, Sleeper, UsbId};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Clone, Default)]
    struct GpioMock(Arc<Mutex<Vec<(u32, bool)>>>);

    impl OutputLines for GpioMock {
        fn drive(&mut self, offset: u32, high: bool) -> io::Result<()> {
            self.0.lock().unwrap().push((offset, high));
            Ok(())
        }
    }

    #[derive(Clone)]
    struct HubMock {
        ports: Arc<Mutex<Vec<HubPort>>>,
        writes: Arc<Mutex<Vec<(String, bool)>>>,
        healthy: bool,
    }

    impl HubSysfs for HubMock {
        fn ports(&mut self) -> io::Result<Vec<HubPort>> {
            Ok(self.ports.lock().unwrap().clone())
        }

        fn any_present(&mut self, _expected: &[UsbId]) -> io::Result<bool> {
            Ok(self.healthy)
        }

        fn set_disabled(&mut self, port: &HubPort, disabled: bool) -> io::Result<()> {
            self.writes
                .lock()
                .unwrap()
                .push((port.name.clone(), disabled));
            if let Some(stored) = self
                .ports
                .lock()
                .unwrap()
                .iter_mut()
                .find(|stored| stored.name == port.name)
            {
                stored.disabled = disabled;
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct SleepMock;

    impl Sleeper for SleepMock {
        fn sleep(&mut self, _duration: Duration) {}
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum CallbackEvent {
        Acc(AccState),
        Key(McuKeyAction, u8),
        Diagnostic(HudEventType),
    }

    struct CallbackMock {
        id: u64,
        events: Arc<Mutex<Vec<CallbackEvent>>>,
    }

    impl HudCallback for CallbackMock {
        fn identity(&self) -> u64 {
            self.id
        }

        fn on_acc_state_changed(&self, event: &AccStateEvent) {
            self.events
                .lock()
                .unwrap()
                .push(CallbackEvent::Acc(event.state));
        }

        fn on_mcu_key_event(&self, event: &McuKeyEvent) {
            self.events
                .lock()
                .unwrap()
                .push(CallbackEvent::Key(event.action, event.raw_code));
        }

        fn on_event(&self, event: &HudEvent) {
            self.events
                .lock()
                .unwrap()
                .push(CallbackEvent::Diagnostic(event.event_type));
        }
    }

    fn port(number: u32, device: Option<(&str, &str)>) -> HubPort {
        HubPort {
            name: format!("1-1-port{number}"),
            device: device.map(|(vendor, product)| UsbId {
                vendor: vendor.into(),
                product: product.into(),
            }),
            disabled: false,
        }
    }

    type Service = HudService<UsbOwner<GpioMock, HubMock, SleepMock>, InventoryGpioCensus>;

    type ServiceFixture = (
        Service,
        Arc<Mutex<Vec<(u32, bool)>>>,
        Arc<Mutex<Vec<(String, bool)>>>,
    );

    fn service() -> ServiceFixture {
        let gpio = GpioMock::default();
        let gpio_events = gpio.0.clone();
        let writes = Arc::new(Mutex::new(Vec::new()));
        let hub = HubMock {
            ports: Arc::new(Mutex::new(vec![
                port(1, Some(("0781", "5581"))),
                port(2, Some(("0bda", "8153"))),
            ])),
            writes: writes.clone(),
            healthy: true,
        };
        (
            HudService::new(UsbOwner::new(gpio, hub, SleepMock, 0), InventoryGpioCensus),
            gpio_events,
            writes,
        )
    }

    #[test]
    fn subscribe_gets_initial_and_confirmed_acc_events_with_diagnostics() {
        let (mut service, _, _) = service();
        let events = Arc::new(Mutex::new(Vec::new()));
        service.subscribe(
            Box::new(CallbackMock {
                id: 7,
                events: events.clone(),
            }),
            0,
        );
        service.record_mcu_event(&Event::AccOff, 100);
        service.publish_acc_state(AccState::OffGrace, "mcu-12-03", 100);
        assert_eq!(service.get_status().last_event_sequence, 1);
        assert_eq!(service.get_recent_events(0)[0].sequence, 1);
        assert_eq!(
            *events.lock().unwrap(),
            [
                CallbackEvent::Acc(AccState::Unknown),
                CallbackEvent::Acc(AccState::OffGrace),
                CallbackEvent::Diagnostic(HudEventType::Acc),
            ]
        );
        service.unsubscribe(8);
        service.record_mcu_event(&Event::AccOn, 200);
        service.publish_acc_state(AccState::On, "mcu-12-04", 200);
        assert!(events
            .lock()
            .unwrap()
            .contains(&CallbackEvent::Acc(AccState::On)));
        service.unsubscribe(7);
        let count = events.lock().unwrap().len();
        service.record_mcu_event(&Event::AccOff, 300);
        service.publish_acc_state(AccState::OffGrace, "mcu-12-03", 300);
        assert_eq!(events.lock().unwrap().len(), count);
    }

    #[test]
    fn acc_state_publication_does_not_run_usb_lifecycle() {
        let (mut service, gpio, writes) = service();
        service.publish_acc_state(AccState::OffGrace, "mcu-12-03", 0);
        service.publish_acc_state(AccState::FinalStage, "mcu-05-10", 25_000);
        service.publish_acc_state(AccState::Suspending, "timer", 25_500);
        service.publish_acc_state(AccState::On, "mcu-12-04", 26_000);
        assert!(writes.lock().unwrap().is_empty());
        assert!(gpio.lock().unwrap().is_empty());
        assert_eq!(service.get_status().acc_state, AccState::On);
    }

    #[test]
    fn usb_mode_and_port_calls_reach_gpio_and_sysfs_mocks() {
        let (mut service, gpio, writes) = service();
        assert_eq!(
            service.set_usb_mode(UsbMode::Device, 42).mode,
            UsbMode::Device
        );
        assert_eq!(*gpio.lock().unwrap(), [(85, true), (34, true)]);
        let changed = service.set_hub_port_enabled(2, false).unwrap();
        assert!(!changed.enabled);
        assert_eq!(
            writes.lock().unwrap().last().unwrap(),
            &("1-1-port2".into(), true)
        );
        assert_eq!(
            service.set_hub_port_enabled(1, false).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn button_and_knob_events_reach_the_semantic_callback() {
        let (mut service, _, _) = service();
        let events = Arc::new(Mutex::new(Vec::new()));
        service.subscribe(
            Box::new(CallbackMock {
                id: 1,
                events: events.clone(),
            }),
            0,
        );
        service.record_mcu_event(
            &Event::Button {
                code: 0x2a,
                state: ButtonState::Release,
                name: "right-knob-push/back",
            },
            10,
        );
        service.record_mcu_event(
            &Event::Knob {
                code: 0x0d,
                magnitude: 3,
                name: "left-knob-left/volume-down",
            },
            20,
        );
        let events = events.lock().unwrap();
        assert!(events.contains(&CallbackEvent::Key(McuKeyAction::Release, 0x2a)));
        assert!(events.contains(&CallbackEvent::Key(McuKeyAction::RotateLeft, 0x0d)));
    }

    #[test]
    fn all_ten_hud_methods_are_runnable_with_default_features() {
        let (mut service, _, _) = service();
        let events = Arc::new(Mutex::new(Vec::new()));
        assert_eq!(service.get_status().frame_count, 0);
        service.subscribe(Box::new(CallbackMock { id: 9, events }), 0);
        assert!(service.get_recent_events(5).is_empty());
        assert_eq!(service.get_usb_mode().mode, UsbMode::Unknown);
        assert_eq!(service.set_usb_mode(UsbMode::Host, 1).error, "");
        assert!(service.check_usb_hub(2).healthy);
        assert_eq!(service.get_hub_ports().unwrap().len(), 2);
        assert_eq!(
            service.set_hub_port_enabled(2, true).unwrap().port_number,
            2
        );
        assert!(!service.get_gpio_census().unwrap().lines.is_empty());
        service.unsubscribe(9);
    }
}
