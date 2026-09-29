// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Binder-independent service facade mirroring `vendor.xyauto.fm.IFmRadio`.
//!
//! The generated Binder trait will be a thin adapter around this type in the
//! existing `bengal-hud` binary. Keeping generated parcelables out of this
//! crate lets the complete method/state contract run under host tests.

use crate::qn8035::{self, RdsState, SeekDirection};

/// What one [`FmService::poll_rds`] call did, so the worker can pace it.
///
/// RDS is read inside a bounded window that opens on every tune, enable and
/// seek completion and closes on a complete programme-service name, on the
/// budget, on a bus failure, or when a seek starts. The window is the device-2
/// budgeted loop (`radio.go:391-440`) with one change: the tuner mutex is
/// released between polls, so a name being read never queues a tune, a
/// `getState` or a seek behind it — the defect HuRadio's `RealPorts.kt:218-226`
/// records against the bridge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RdsProgress {
    /// No window is open: disabled, seeking, nothing tuned, budget spent, or
    /// the last poll failed. Sleep the idle interval.
    Idle,
    /// Polled and nothing complete yet. Poll again after [`qn8035::RDS_POLL_MS`].
    Polling,
    /// A programme-service name was published and the window closed.
    Published,
}

/// One open RDS window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RdsWindow {
    /// The tune generation this window was armed for. A window from an older
    /// generation can never publish into the current station.
    generation: u64,
    /// Absolute deadline in the caller's clock, set by the FIRST poll so the
    /// budget starts when the worker looks, not when a Binder thread armed it.
    deadline_ms: Option<u64>,
    /// Polls made in this window (diagnostic).
    polls: u32,
}

/// AIDL-shaped `TunerState`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TunerState {
    /// `setEnabled(true)` completed.
    pub enabled: bool,
    /// Current frequency, or zero before the first tune.
    pub frequency_khz: u32,
    /// A seek worker owns the tuner.
    pub seeking: bool,
    /// Resolved path and adapter name for bugreports.
    pub i2c_adapter: String,
}

/// AIDL-shaped `SignalQuality`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SignalQuality {
    /// Raw register 0x02.
    pub snr: u8,
    /// Raw register 0x03.
    pub rssi: u8,
    /// Shared SNR-floor decision.
    pub station_present: bool,
}

impl From<qn8035::SignalQuality> for SignalQuality {
    fn from(value: qn8035::SignalQuality) -> Self {
        Self {
            snr: value.snr,
            rssi: value.rssi,
            station_present: value.station_present,
        }
    }
}

/// AIDL-shaped `RdsInfo`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RdsInfo {
    /// Programme Identification, or zero before sync.
    pub programme_identification: u16,
    /// Empty until all four segments have arrived.
    pub programme_service: String,
    /// Current decoder synchronization state.
    pub synchronized: bool,
}

impl From<RdsState> for RdsInfo {
    fn from(value: RdsState) -> Self {
        Self {
            programme_identification: value.programme_identification,
            programme_service: value.programme_service,
            synchronized: value.synchronized,
        }
    }
}

/// Binder exception family consumed by the Android-only generated adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExceptionCode {
    /// Maps to `EX_ILLEGAL_ARGUMENT`.
    IllegalArgument,
    /// Maps to `EX_ILLEGAL_STATE`.
    IllegalState,
    /// Maps to `EX_SERVICE_SPECIFIC` for an adapter/transport failure.
    ServiceSpecific,
}

/// Service-layer error independent of libbinder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceError {
    /// Binder exception family.
    pub code: ExceptionCode,
    /// Stable human-readable failure.
    pub message: String,
}

impl ServiceError {
    fn hardware(error: impl std::fmt::Display) -> Self {
        Self {
            code: ExceptionCode::ServiceSpecific,
            message: error.to_string(),
        }
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ServiceError {}

/// Failures specific to the composite enable sequence.
#[derive(Debug)]
pub enum EnableError<E> {
    /// MCU, tuner, GPIO or DSP transport failed.
    Hardware(E),
    /// The live identity read did not return [`qn8035::CHIP_ID`].
    WrongChipId(u8),
}

impl<E: std::fmt::Display> std::fmt::Display for EnableError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hardware(error) => error.fmt(f),
            Self::WrongChipId(found) => write!(
                f,
                "chip id 0x{found:02x}, expected QN8035 0x{:02x}",
                qn8035::CHIP_ID
            ),
        }
    }
}

/// Composite hardware port implemented by `bengal-hud`.
///
/// `set_enabled` owns QN8035 reset/CID/RX/tune/unmute only after HuRadio has
/// committed the typed audio-guard source handover. This crate intentionally
/// cannot perform the guard's rail, ADAU, or analogue-mux operation.
pub trait FmHardware {
    /// Hardware/backend failure.
    type Error: std::fmt::Display;

    /// Resolved adapter label cached into `TunerState`.
    fn adapter_label(&self) -> String;
    /// Run the complete idempotent source/power transition.
    ///
    /// On enable, `prepared_frequency_khz` is the most recent channel accepted
    /// while disabled. The implementation applies it after tuner power-up and
    /// before routing the mux/un-muting, preserving HuRadio's prepare/commit
    /// handover. It is `None` on disable or when no channel was prepared.
    fn set_enabled(
        &mut self,
        enabled: bool,
        prepared_frequency_khz: Option<u32>,
    ) -> Result<(), EnableError<Self::Error>>;
    /// Read register 0x06 live.
    fn chip_id(&mut self) -> Result<u8, Self::Error>;
    /// Tune on the validated product grid.
    fn tune(&mut self, frequency_khz: u32) -> Result<(), Self::Error>;
    /// Read signal registers live.
    fn signal_quality(&mut self) -> Result<qn8035::SignalQuality, Self::Error>;
    /// Say, WITHOUT touching the tuner, whether the physical facility exists.
    ///
    /// Real backends use the default. A deliberately unavailable backend (the
    /// Phase-4c placeholder) returns its stable reason here, so a disabled-
    /// state refusal names "not ready" instead of a generic state error. Never
    /// a bus access: this is consulted while the tuner rail may be off.
    fn check_available(&self) -> Result<(), Self::Error> {
        Ok(())
    }
    /// Check whether a seek may be requested without touching the tuner.
    ///
    /// Defaults to [`Self::check_available`], preserving the state machine's
    /// bus-free disabled rejection.
    fn check_seek_available(&self) -> Result<(), Self::Error> {
        self.check_available()
    }
    /// Queue an asynchronous seek on the HUD worker.
    ///
    /// `from_khz` is the channel the walk must NOT recheck. The tuner itself
    /// is stateless about frequency -- only this service tracks it -- so the
    /// starting point is passed rather than read back off the chip.
    fn start_seek(&mut self, direction: SeekDirection, from_khz: u32) -> Result<(), Self::Error>;
    /// Request cancellation of the current seek.
    fn stop_seek(&mut self) -> Result<(), Self::Error>;
    /// Advance a queued seek by exactly ONE half-channel, without blocking.
    ///
    /// This is the step the worker drives. `BandScan::step` performs either
    /// the three-write tune or the SNR/RSSI measurement — never both — so one
    /// call is one bus burst and a 205-channel walk never sits inside a Binder
    /// transaction, and never judges a channel the front end has not settled
    /// on. The returned [`SeekStep`] tells the worker how long to wait before
    /// calling again, which is the entire reason it is not a `bool`.
    ///
    /// Defaulted to "nothing queued" so a backend without a tuner — the
    /// placeholder, and every test double — is unaffected by this addition.
    fn poll_seek(&mut self) -> Result<SeekStep, Self::Error> {
        Ok(SeekStep::Idle)
    }
    /// The candidate most recently tuned by an in-flight seek, if the backend
    /// has one. This is a cache of work it already performed, never a second
    /// I2C read. The frozen `getState()` surface exposes it while `seeking` is
    /// true; terminal state remains owned by [`FmService::complete_seek`].
    fn seek_candidate(&self) -> Option<u32> {
        None
    }
    /// One RDS poll: `STATUS2`, and the block bytes only behind an accepting
    /// status. `Ok(Some)` exactly when a programme-service name completed.
    ///
    /// Defaulted to "nothing" so the placeholder and every test double are
    /// unaffected — the same shape `poll_seek` had before the worker existed.
    fn poll_rds(&mut self) -> Result<Option<RdsState>, Self::Error> {
        Ok(None)
    }

    /// Forget every backend state that assumes tuner power is present.
    /// This hook is bus-free and therefore remains safe after a rail loss.
    fn power_lost(&mut self) {}
}

/// What one [`FmService::poll_seek`] call did, and therefore how to pace.
///
/// The worker's cadence is a correctness property, not a comfort one: a walk
/// that steps faster than [`qn8035::SEEK_SETTLE_MS`] reads register 0x02
/// before the channel it just tuned owns it. Making the phase visible here is
/// what stops that from being a comment somebody can forget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeekProgress {
    /// No seek is in flight. Sleep long; an idle radio costs nothing.
    Idle,
    /// A channel is settling. Sleep the requested milliseconds before stepping.
    Settling(u64),
    /// A channel was judged. Step again as soon as convenient.
    Stepping,
}

/// What one [`FmHardware::poll_seek`] call did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeekStep {
    /// Nothing was queued; the tuner was not touched.
    Idle,
    /// Wait the requested milliseconds before the next measurement.
    Settling(u64),
    /// A channel was measured and rejected; the walk continues.
    Continue,
    /// The walk ended: `Some(khz)` landed on a station, `None` exhausted the band.
    Finished(Option<u32>),
}

/// Stable reason exposed until Phase 4c has supplied the audio guard.
///
/// The FM power/mux transaction is unsafe to begin without the guard: a tuner
/// brought up while the Android I2S branch remains connected would make an
/// uncontrolled mixed audio path.  This is deliberately a backend, rather
/// than an omitted Binder registration, so Android can distinguish a declared
/// service that is not ready from a missing or blocked service.
pub const PHASE_4C_AUDIO_GUARD_UNAVAILABLE: &str = "NotReady/Phase-4c-audio-guard-unavailable";

/// Production backend until the Phase-4c audio guard is available.
///
/// It owns no descriptors and has no constructors that open I2C, GPIO, the
/// MCU UART, or the DSP. Registering the AIDL service with this type is thus
/// electrically inert; only a future replacement may perform the composite
/// MCU → tuner → mux → unmute operation.
#[derive(Debug, Default)]
pub struct Phase4cAudioGuardUnavailable;

/// The precise non-ready error returned by physical FM operations.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NotReady;

impl std::fmt::Display for NotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(PHASE_4C_AUDIO_GUARD_UNAVAILABLE)
    }
}

impl std::error::Error for NotReady {}

impl FmHardware for Phase4cAudioGuardUnavailable {
    type Error = NotReady;

    fn adapter_label(&self) -> String {
        PHASE_4C_AUDIO_GUARD_UNAVAILABLE.to_owned()
    }

    fn set_enabled(
        &mut self,
        _enabled: bool,
        _prepared_frequency_khz: Option<u32>,
    ) -> Result<(), EnableError<Self::Error>> {
        Err(EnableError::Hardware(NotReady))
    }

    fn chip_id(&mut self) -> Result<u8, Self::Error> {
        Err(NotReady)
    }

    fn tune(&mut self, _frequency_khz: u32) -> Result<(), Self::Error> {
        Err(NotReady)
    }

    fn signal_quality(&mut self) -> Result<qn8035::SignalQuality, Self::Error> {
        Err(NotReady)
    }

    fn check_available(&self) -> Result<(), Self::Error> {
        Err(NotReady)
    }

    fn start_seek(&mut self, _direction: SeekDirection, _from_khz: u32) -> Result<(), Self::Error> {
        Err(NotReady)
    }

    fn stop_seek(&mut self) -> Result<(), Self::Error> {
        Err(NotReady)
    }
}

/// Binder callback shape without a Binder dependency.
pub trait FmRadioCallback: Send + Sync {
    /// Seek landed on a station.
    fn on_seek_complete(&self, frequency_khz: u32);
    /// Seek stopped by the caller or exhausted the band.
    fn on_seek_stopped(&self, band_exhausted: bool);
    /// Newly complete programme service or new PI.
    fn on_rds_update(&self, rds: &RdsInfo);
    /// Tuner became unavailable.
    fn on_tuner_lost(&self, reason: &str);
}

/// Binder-independent shape of all nine `IFmRadio` methods.
///
/// The Android-only adapter is generic over this boundary and shares the
/// concrete implementation with the daemon worker. Host tests call that same
/// implementation directly without generated AIDL or `libbinder_rs`.
pub trait FmApi: Send {
    /// `IFmRadio.setEnabled`.
    fn set_enabled(&mut self, enabled: bool) -> Result<(), ServiceError>;
    /// `IFmRadio.getState`.
    fn get_state(&self) -> TunerState;
    /// `IFmRadio.getChipId`; refused by state while disabled, see
    /// [`FmService::get_chip_id`].
    fn get_chip_id(&mut self) -> Result<u8, ServiceError>;
    /// `IFmRadio.tune`.
    fn tune(&mut self, frequency_khz: u32) -> Result<(), ServiceError>;
    /// `IFmRadio.getSignalQuality`; refused by state while disabled, see
    /// [`FmService::get_signal_quality`].
    fn get_signal_quality(&mut self) -> Result<SignalQuality, ServiceError>;
    /// `IFmRadio.startSeek`.
    fn start_seek(&mut self, upward: bool) -> Result<(), ServiceError>;
    /// `IFmRadio.stopSeek`.
    fn stop_seek(&mut self) -> Result<(), ServiceError>;
    /// `IFmRadio.getRdsInfo`.
    fn get_rds_info(&self) -> RdsInfo;
    /// `IFmRadio.setCallback`.
    fn set_callback(&mut self, callback: Option<Box<dyn FmRadioCallback>>);
}

/// Host-testable implementation behind `IFmRadio/default`.
pub struct FmService<H> {
    hardware: H,
    state: TunerState,
    rds: RdsInfo,
    callback: Option<Box<dyn FmRadioCallback>>,
    /// Bumped on every successful tune, enable, and seek completion —
    /// INCLUDING a retune to the same frequency, which the chip experiences
    /// as a new station (its assembler resets) even though the number did
    /// not change. A frequency comparison would miss that.
    tune_generation: u64,
    /// The open RDS window, if any.
    rds_window: Option<RdsWindow>,
    /// The station that was playing when the current seek began. While a walk
    /// is in flight `state.frequency_khz` follows physical candidates; every
    /// non-landing terminal path restores this exact start value.
    seek_start_khz: Option<u32>,
    // Only HUD's native ACC worker creates this lease. A user stop while held
    // clears replay intent; rejected tune/enable calls never change the snapshot.
    #[cfg(any(test, feature = "acc-power-channel"))]
    acc_hold: Option<(u64, bool)>,
    #[cfg(any(test, feature = "acc-power-channel"))]
    acc_parked: bool,
}

impl<H: FmHardware> FmService<H> {
    /// Create a disabled service and cache the resolved adapter description.
    pub fn new(hardware: H) -> Self {
        let adapter = hardware.adapter_label();
        Self {
            hardware,
            state: TunerState {
                i2c_adapter: adapter,
                ..TunerState::default()
            },
            rds: RdsInfo::default(),
            callback: None,
            tune_generation: 0,
            rds_window: None,
            seek_start_khz: None,
            #[cfg(any(test, feature = "acc-power-channel"))]
            acc_hold: None,
            #[cfg(any(test, feature = "acc-power-channel"))]
            acc_parked: false,
        }
    }

    /// Open (or re-open) the RDS window for the station the chip is on now.
    fn arm_rds(&mut self) {
        self.tune_generation += 1;
        self.rds_window = Some(RdsWindow {
            generation: self.tune_generation,
            deadline_ms: None,
            polls: 0,
        });
    }

    /// The current tune generation. Diagnostic; never touches hardware.
    pub fn rds_generation(&self) -> u64 {
        self.tune_generation
    }

    /// Map `IFmRadio.setEnabled`.
    pub fn set_enabled(&mut self, enabled: bool) -> Result<(), ServiceError> {
        #[cfg(any(test, feature = "acc-power-channel"))]
        if let Some((_, replay)) = &mut self.acc_hold {
            if !enabled { *replay = false; return Ok(()); }
            return Err(Self::acc_busy());
        }
        if self.state.enabled == enabled {
            return Ok(());
        }
        let prepared_frequency =
            (enabled && self.state.frequency_khz != 0).then_some(self.state.frequency_khz);
        match self.hardware.set_enabled(enabled, prepared_frequency) {
            Ok(()) => {}
            Err(EnableError::Hardware(error)) => return Err(self.hardware_error(error)),
            Err(EnableError::WrongChipId(found)) => {
                let error = ServiceError {
                    code: ExceptionCode::IllegalState,
                    message: EnableError::<H::Error>::WrongChipId(found).to_string(),
                };
                self.notify_tuner_lost(&error.message);
                return Err(error);
            }
        }
        self.state.enabled = enabled;
        if enabled {
            // The prepared channel was applied inside the enable sequence, so
            // the chip is on a station: start reading its name.
            if self.state.frequency_khz != 0 {
                self.arm_rds();
            }
        } else {
            self.state.seeking = false;
            self.seek_start_khz = None;
            self.rds = RdsInfo::default();
            self.rds_window = None;
        }
        Ok(())
    }

    /// Reconcile a lost audio-owner lease before its FM rails are removed.
    ///
    /// A normal disable is attempted first so a powered tuner reaches mute and
    /// standby. Whatever that I/O returns, the following state invalidation is
    /// bus-free: workers immediately stop seek/RDS work and the public state
    /// cannot continue claiming an enabled tuner behind dead rails. The last
    /// prepared frequency is deliberately retained for the next clean enable.
    pub fn audio_route_lost(&mut self, rails_live: bool) -> Result<(), ServiceError> {
        #[cfg(any(test, feature = "acc-power-channel"))]
        if let Some((_, replay)) = &mut self.acc_hold { *replay = false; }
        let disable = if rails_live && self.state.enabled {
            match self.hardware.set_enabled(false, None) {
                Ok(()) => Ok(()),
                Err(EnableError::Hardware(error)) => Err(ServiceError::hardware(error)),
                Err(EnableError::WrongChipId(found)) => Err(ServiceError {
                    code: ExceptionCode::IllegalState,
                    message: EnableError::<H::Error>::WrongChipId(found).to_string(),
                }),
            }
        } else {
            Ok(())
        };
        self.hardware.power_lost();
        self.state.enabled = false;
        self.state.seeking = false;
        self.seek_start_khz = None;
        self.rds = RdsInfo::default();
        self.rds_window = None;
        if let Err(error) = &disable {
            self.notify_tuner_lost(&error.message);
        }
        disable
    }

    /// Map `IFmRadio.getState`; this never touches hardware.
    pub fn get_state(&self) -> TunerState {
        self.state.clone()
    }

    /// A live-register query while the radio is disabled: refused BEFORE any
    /// bus access, and never reported as a lost tuner.
    ///
    /// The QN8035's rail (TLMM84 fmic + TLMM96 fm-power) belongs to the kernel
    /// audio guard, which HuRadio drops on every FM exit; while this service is
    /// disabled the chip is therefore EXPECTED to be unpowered and to NACK.
    /// Live on D3-0119 (2026-09-05 17:22:24-17:23:28) one quality read and
    /// nine connect-time identity probes on the unpowered chip each produced
    /// `ENXIO`, each was broadcast as `onTunerLost`, and the app answered every
    /// one by dropping its proxy and probing again. Nothing was lost: the
    /// radio was off. A deliberately unavailable backend still names its own
    /// stable reason (`check_available`), bus-free.
    fn refuse_while_disabled(&self, what: &str) -> Option<ServiceError> {
        if self.state.enabled {
            return None;
        }
        Some(match self.hardware.check_available() {
            Err(error) => ServiceError::hardware(error),
            Ok(()) => ServiceError {
                code: ExceptionCode::IllegalState,
                message: format!(
                    "{what} requires an enabled radio: the tuner is unpowered while disabled"
                ),
            },
        })
    }

    /// Map `IFmRadio.getChipId`; wrong identity is `EX_ILLEGAL_STATE`.
    ///
    /// Refused by state while disabled (no bus access, no `onTunerLost`).
    /// Identity is still verified on EVERY enable inside the backend's power
    /// sequence (`EnableError::WrongChipId`), so nothing goes unchecked: the
    /// frozen AIDL comment calling this "callable before anything is powered"
    /// described the daemon's own sequence, not the guard's rail.
    pub fn get_chip_id(&mut self) -> Result<u8, ServiceError> {
        if let Some(refusal) = self.refuse_while_disabled("chip identity") {
            return Err(refusal);
        }
        let found = match self.hardware.chip_id() {
            Ok(found) => found,
            Err(error) => return Err(self.hardware_error(error)),
        };
        if found != qn8035::CHIP_ID {
            let error = ServiceError {
                code: ExceptionCode::IllegalState,
                message: format!(
                    "chip id 0x{found:02x}, expected QN8035 0x{:02x}",
                    qn8035::CHIP_ID
                ),
            };
            self.notify_tuner_lost(&error.message);
            return Err(error);
        }
        Ok(found)
    }

    /// Map `IFmRadio.tune`.
    pub fn tune(&mut self, frequency_khz: u32) -> Result<(), ServiceError> {
        #[cfg(any(test, feature = "acc-power-channel"))]
        if self.acc_hold.is_some() { return Err(Self::acc_busy()); }
        qn8035::validate_frequency(frequency_khz).map_err(|error| ServiceError {
            code: ExceptionCode::IllegalArgument,
            message: error.to_string(),
        })?;
        if self.state.seeking {
            return Err(ServiceError {
                code: ExceptionCode::IllegalState,
                message: "cannot tune while seek is running".to_owned(),
            });
        }
        if self.state.enabled {
            if let Err(error) = self.hardware.tune(frequency_khz) {
                return Err(self.hardware_error(error));
            }
        }
        self.state.frequency_khz = frequency_khz;
        self.rds = RdsInfo::default();
        if self.state.enabled {
            // Same frequency or not: the chip was retuned and its assembler
            // reset, so this is a new station as far as RDS is concerned.
            self.arm_rds();
        }
        Ok(())
    }

    /// Map `IFmRadio.getSignalQuality`.
    pub fn get_signal_quality(&mut self) -> Result<SignalQuality, ServiceError> {
        if let Some(refusal) = self.refuse_while_disabled("signal quality") {
            return Err(refusal);
        }
        match self.hardware.signal_quality() {
            Ok(quality) => Ok(quality.into()),
            Err(error) => Err(self.hardware_error(error)),
        }
    }

    /// Map `IFmRadio.startSeek`; completion arrives through [`Self::complete_seek`].
    pub fn start_seek(&mut self, upward: bool) -> Result<(), ServiceError> {
        #[cfg(any(test, feature = "acc-power-channel"))]
        if self.acc_hold.is_some() { return Err(Self::acc_busy()); }
        if let Err(error) = self.hardware.check_seek_available() {
            return Err(self.hardware_error(error));
        }
        if self.state.seeking {
            return Err(ServiceError {
                code: ExceptionCode::IllegalState,
                message: "seek already running".to_owned(),
            });
        }
        if !self.state.enabled || self.state.frequency_khz == 0 {
            return Err(ServiceError {
                code: ExceptionCode::IllegalState,
                message: "seek requires an enabled, tuned radio".to_owned(),
            });
        }
        let direction = if upward {
            SeekDirection::Up
        } else {
            SeekDirection::Down
        };
        if let Err(error) = self.hardware.start_seek(direction, self.state.frequency_khz) {
            return Err(self.hardware_error(error));
        }
        self.seek_start_khz = Some(self.state.frequency_khz);
        self.state.seeking = true;
        // The walk retunes the chip on every channel, so no RDS is read while
        // it runs -- that is `poll_rds`'s `seeking` guard, deliberately the ONE
        // mechanism (a second, closing the window here, masked a mutant of the
        // first). Every way a seek ends re-arms the window, so the one left
        // open here is always replaced before it could be polled again.
        Ok(())
    }

    /// Drive a queued seek by one channel and finish it when it lands.
    ///
    /// The counterpart to `complete_seek`, and the reason that hook existed
    /// with nothing calling it: the worker calls THIS on an interval, and it
    /// calls `complete_seek` itself on a terminal outcome, so the callback
    /// contract has exactly one implementation rather than one per caller.
    ///
    /// Returns what the step did, so the worker knows both whether to keep
    /// stepping AND how long to wait first — a channel that has just been
    /// tuned needs [`qn8035::SEEK_SETTLE_MS`] before it can be judged, and
    /// that wait belongs out here where no lock is held. Bus loss ends the
    /// seek as `band exhausted` and reports the tuner lost, rather than
    /// spinning on a dead adapter.
    pub fn poll_seek(&mut self) -> SeekProgress {
        if !self.state.seeking {
            return SeekProgress::Idle;
        }
        match self.hardware.poll_seek() {
            Ok(SeekStep::Idle) => SeekProgress::Idle,
            Ok(SeekStep::Settling(wait_ms)) => {
                // The backend has tuned a candidate or requested its remaining
                // dwell. Consume only its cache without another register read;
                // complete_seek below owns terminal truth.
                if let Some(candidate) = self.hardware.seek_candidate() {
                    self.state.frequency_khz = candidate;
                }
                SeekProgress::Settling(wait_ms)
            }
            Ok(SeekStep::Continue) => SeekProgress::Stepping,
            Ok(SeekStep::Finished(outcome)) => {
                self.complete_seek(outcome);
                SeekProgress::Idle
            }
            Err(error) => {
                let message = error.to_string();
                self.complete_seek(None);
                self.notify_tuner_lost(&message);
                SeekProgress::Idle
            }
        }
    }

    /// Worker hook for a finished/exhausted seek.
    pub fn complete_seek(&mut self, frequency_khz: Option<u32>) {
        self.state.seeking = false;
        let seek_start = self.seek_start_khz.take();
        match frequency_khz {
            Some(frequency) => {
                self.state.frequency_khz = frequency;
                self.rds = RdsInfo::default();
                if let Some(callback) = &self.callback {
                    callback.on_seek_complete(frequency);
                }
            }
            None => {
                if let Some(start) = seek_start {
                    self.state.frequency_khz = start;
                }
                if let Some(callback) = &self.callback {
                    callback.on_seek_stopped(true);
                }
            }
        }
        // Landed: a new station. Exhausted: the walk put the chip back on the
        // start channel (a retune, assembler reset) — same station, so the old
        // name is kept, but its window must be re-opened to be re-confirmed.
        self.arm_rds();
    }

    /// Map `IFmRadio.stopSeek`; a no-op when idle.
    pub fn stop_seek(&mut self) -> Result<(), ServiceError> {
        if !self.state.seeking {
            return Ok(());
        }
        if let Err(error) = self.hardware.stop_seek() {
            return Err(self.hardware_error(error));
        }
        self.state.seeking = false;
        if let Some(start) = self.seek_start_khz.take() {
            self.state.frequency_khz = start;
        }
        if let Some(callback) = &self.callback {
            callback.on_seek_stopped(false);
        }
        // The backend put the chip back where the seek started (a retune).
        self.arm_rds();
        Ok(())
    }

    #[cfg(any(test, feature = "acc-power-channel"))]
    fn acc_busy() -> ServiceError {
        ServiceError { code: ExceptionCode::IllegalState, message: "ACC preparation owns FM".into() }
    }

    /// Native owner only, called AFTER the shared audio guard confirms all-output
    /// mute. The same service mutex fences Binder calls and the seek worker.
    #[cfg(any(test, feature = "acc-power-channel"))]
    pub fn acc_hold(&mut self, token: u64) -> Result<(), ServiceError> {
        if token == 0 { return Err(Self::acc_busy()); }
        if let Some((held, _)) = self.acc_hold {
            return if held == token { Ok(()) } else { Err(Self::acc_busy()) };
        }
        let replay = self.state.enabled;
        // stop_seek restores the exact pre-seek channel behind the guard mute.
        // Fence publication is under the caller's same service mutex.
        self.stop_seek()?;
        self.acc_hold = Some((token, replay));
        self.acc_parked = false;
        if replay {
            if let Err(error) = self.hardware.set_enabled(false, None) {
                self.acc_hold = Some((token, false));
                return Err(ServiceError::hardware(error));
            }
        }
        self.state.enabled = false;
        self.state.seeking = false;
        self.rds_window = None;
        self.acc_parked = true;
        Ok(())
    }

    /// Release only this lease. A stop/death/route loss during the hold forbids
    /// automatic replay. Audio remains guarded until this restoration returns.
    #[cfg(any(test, feature = "acc-power-channel"))]
    pub fn acc_release(&mut self, token: u64) -> Result<(), ServiceError> {
        let Some((held, replay)) = self.acc_hold else { return Ok(()); };
        if held != token { return Err(Self::acc_busy()); }
        if !replay && !self.acc_parked {
            // A failed park may have changed the tuner. A later user stop must
            // not turn uncertain park into a successful release/unmute.
            self.hardware.set_enabled(false, None).map_err(ServiceError::hardware)?;
            self.state.enabled = false;
            self.state.seeking = false;
            self.rds_window = None;
        }
        if replay {
            let frequency = (self.state.frequency_khz != 0).then_some(self.state.frequency_khz);
            if let Err(error) = self.hardware.set_enabled(true, frequency) {
                self.acc_hold = Some((token, false));
                return Err(ServiceError::hardware(error));
            }
            self.state.enabled = true;
            self.arm_rds();
        }
        self.acc_hold = None;
        Ok(())
    }

    /// Map `IFmRadio.getRdsInfo`; cached and bus-free.
    pub fn get_rds_info(&self) -> RdsInfo {
        self.rds.clone()
    }

    /// Drive the open RDS window by one poll, if there is one to drive.
    ///
    /// `now_ms` is the caller's monotonic clock; the window's deadline is set
    /// from the FIRST poll, so a budget of [`qn8035::RDS_WINDOW_MS`] starts
    /// when the worker looks, not when a Binder thread armed it. Fake time in
    /// tests is a number.
    ///
    /// Never touches the bus when there is nothing to read: disabled, seeking,
    /// no window, or a spent budget all return [`RdsProgress::Idle`] before any
    /// hardware call. A bus failure closes the window rather than retrying —
    /// the next tune re-opens one, and a dead adapter surfaces there.
    pub fn poll_rds(&mut self, now_ms: u64) -> RdsProgress {
        if !self.state.enabled || self.state.seeking {
            return RdsProgress::Idle;
        }
        let Some(window) = self.rds_window.as_mut() else {
            return RdsProgress::Idle;
        };
        let deadline = *window
            .deadline_ms
            .get_or_insert(now_ms.saturating_add(qn8035::RDS_WINDOW_MS));
        if now_ms >= deadline {
            self.rds_window = None;
            return RdsProgress::Idle;
        }
        window.polls += 1;
        let generation = window.generation;
        match self.hardware.poll_rds() {
            Ok(None) => RdsProgress::Polling,
            Ok(Some(state)) => {
                // Structural guard: a publication may only land in the window
                // it was read for. Under the service mutex nothing can retune
                // between the read and here, so this cannot fire today; it is
                // what keeps that true if the locking ever changes.
                if self.rds_window.map(|w| w.generation) != Some(generation) {
                    return RdsProgress::Idle;
                }
                self.rds_window = None;
                self.publish_rds(state);
                RdsProgress::Published
            }
            Err(_) => {
                self.rds_window = None;
                RdsProgress::Idle
            }
        }
    }

    /// Worker hook for a newly completed RDS state.
    pub fn publish_rds(&mut self, rds: RdsState) {
        self.rds = rds.into();
        if let Some(callback) = &self.callback {
            callback.on_rds_update(&self.rds);
        }
    }

    /// Worker hook for a lost tuner.
    pub fn tuner_lost(&self, reason: &str) {
        if let Some(callback) = &self.callback {
            callback.on_tuner_lost(reason);
        }
    }

    /// Map `IFmRadio.setCallback`; replacing the previous single client.
    pub fn set_callback(&mut self, callback: Option<Box<dyn FmRadioCallback>>) {
        self.callback = callback;
    }

    /// Recover the backend for integration tests or daemon shutdown.
    pub fn into_inner(self) -> H {
        self.hardware
    }

    fn hardware_error(&self, error: impl std::fmt::Display) -> ServiceError {
        let error = ServiceError::hardware(error);
        self.notify_tuner_lost(&error.message);
        error
    }

    fn notify_tuner_lost(&self, reason: &str) {
        if let Some(callback) = &self.callback {
            callback.on_tuner_lost(reason);
        }
    }
}

impl<H: FmHardware + Send> FmApi for FmService<H> {
    fn set_enabled(&mut self, enabled: bool) -> Result<(), ServiceError> {
        FmService::set_enabled(self, enabled)
    }

    fn get_state(&self) -> TunerState {
        FmService::get_state(self)
    }

    fn get_chip_id(&mut self) -> Result<u8, ServiceError> {
        FmService::get_chip_id(self)
    }

    fn tune(&mut self, frequency_khz: u32) -> Result<(), ServiceError> {
        FmService::tune(self, frequency_khz)
    }

    fn get_signal_quality(&mut self) -> Result<SignalQuality, ServiceError> {
        FmService::get_signal_quality(self)
    }

    fn start_seek(&mut self, upward: bool) -> Result<(), ServiceError> {
        FmService::start_seek(self, upward)
    }

    fn stop_seek(&mut self) -> Result<(), ServiceError> {
        FmService::stop_seek(self)
    }

    fn get_rds_info(&self) -> RdsInfo {
        FmService::get_rds_info(self)
    }

    fn set_callback(&mut self, callback: Option<Box<dyn FmRadioCallback>>) {
        FmService::set_callback(self, callback);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn failed_fm_park_and_failed_restore_never_report_released() {
        let mut s = enabled_at(97_500, MockHardware::default());
        s.hardware.fail_on = Some("set_enabled");
        assert!(s.acc_hold(1).is_err());
        s.set_enabled(false).unwrap();
        assert!(s.acc_release(1).is_err());
        assert!(s.acc_hold.is_some());
        assert!(s.set_enabled(true).is_err());
        s.hardware.fail_on = None;
        s.acc_release(1).unwrap();
        assert!(!s.get_state().enabled);
        s.set_enabled(true).unwrap();
        s.acc_hold(2).unwrap();
        s.hardware.fail_on = Some("set_enabled");
        assert!(s.acc_release(2).is_err());
        assert!(s.acc_hold.is_some());
    }
    #[test]
    fn acc_hold_fences_mutations_and_stop_revokes_only_owned_replay() {
        let mut s = enabled_at(97_500, MockHardware::default());
        s.acc_hold(1).unwrap();
        assert!(!s.get_state().enabled);
        assert!(s.set_enabled(true).is_err());
        assert!(s.tune(99_500).is_err());
        assert!(s.start_seek(true).is_err());
        assert_eq!(s.get_state().frequency_khz, 97_500);
        assert!(s.acc_release(2).is_err());
        s.set_enabled(false).unwrap(); // actual user stop, not overwritten on resume
        s.acc_release(1).unwrap();
        assert!(!s.get_state().enabled);
        s.set_enabled(true).unwrap();
        s.acc_hold(3).unwrap();
        s.acc_release(3).unwrap();
        assert!(s.get_state().enabled);
        assert_eq!(s.get_state().frequency_khz, 97_500);
    }
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Default)]
    struct MockHardware {
        calls: Vec<String>,
        chip_id: u8,
        wrong_chip_on_enable: Option<u8>,
        fail_on: Option<&'static str>,
        failure: &'static str,
        frequency_khz: Option<u32>,
        /// Scripted `poll_seek` steps, popped from the END (so a test lists
        /// them in reverse order of delivery). Empty = "band exhausted".
        seek_steps: Vec<SeekStep>,
        /// Candidate cache for each scripted settling step, popped from END.
        seek_candidate_steps: Vec<Option<u32>>,
        /// Scripted `poll_rds` results, popped from the END. Empty = nothing.
        rds_steps: Vec<Option<RdsState>>,
    }

    impl MockHardware {
        fn fail(&self, operation: &str) -> Result<(), &'static str> {
            if self.fail_on == Some(operation) {
                Err(self.failure)
            } else {
                Ok(())
            }
        }
    }

    impl FmHardware for MockHardware {
        type Error = &'static str;

        fn adapter_label(&self) -> String {
            "/dev/i2c-1 (i2c-fm)".to_owned()
        }

        fn set_enabled(
            &mut self,
            enabled: bool,
            prepared_frequency_khz: Option<u32>,
        ) -> Result<(), EnableError<Self::Error>> {
            self.calls
                .push(format!("set_enabled({enabled},{prepared_frequency_khz:?})"));
            self.fail("set_enabled").map_err(EnableError::Hardware)?;
            match self.wrong_chip_on_enable {
                Some(found) if enabled => Err(EnableError::WrongChipId(found)),
                _ => {
                    if enabled {
                        self.frequency_khz = prepared_frequency_khz;
                    }
                    Ok(())
                }
            }
        }

        fn chip_id(&mut self) -> Result<u8, Self::Error> {
            self.calls.push("chip_id".to_owned());
            self.fail("chip_id")?;
            Ok(self.chip_id)
        }

        fn tune(&mut self, frequency_khz: u32) -> Result<(), Self::Error> {
            self.calls.push(format!("tune({frequency_khz})"));
            self.fail("tune")?;
            self.frequency_khz = Some(frequency_khz);
            Ok(())
        }

        fn signal_quality(&mut self) -> Result<qn8035::SignalQuality, Self::Error> {
            self.calls.push("signal_quality".to_owned());
            self.fail("signal_quality")?;
            Ok(qn8035::SignalQuality {
                snr: 31,
                rssi: 88,
                station_present: true,
            })
        }

        fn start_seek(&mut self, direction: SeekDirection, _from_khz: u32) -> Result<(), Self::Error> {
            self.calls.push(format!("seek({direction:?})"));
            self.fail("start_seek")?;
            Ok(())
        }

        fn stop_seek(&mut self) -> Result<(), Self::Error> {
            self.calls.push("stop_seek".to_owned());
            self.fail("stop_seek")?;
            Ok(())
        }

        fn poll_seek(&mut self) -> Result<SeekStep, Self::Error> {
            self.calls.push("poll_seek".to_owned());
            self.fail("poll_seek")?;
            let step = self.seek_steps.pop().unwrap_or(SeekStep::Finished(None));
            if matches!(step, SeekStep::Settling(_)) {
                if let Some(candidate) = self.seek_candidate_steps.pop() {
                    self.frequency_khz = candidate;
                }
            }
            Ok(step)
        }

        fn seek_candidate(&self) -> Option<u32> {
            self.frequency_khz
        }

        fn poll_rds(&mut self) -> Result<Option<RdsState>, Self::Error> {
            self.calls.push("poll_rds".to_owned());
            self.fail("poll_rds")?;
            Ok(self.rds_steps.pop().unwrap_or(None))
        }

        fn power_lost(&mut self) {
            self.calls.push("power_lost".to_owned());
        }
    }

    fn ps(name: &str) -> RdsState {
        RdsState {
            programme_identification: 0x8412,
            programme_service: name.to_owned(),
            synchronized: true,
        }
    }

    fn rds_polls(service: &FmService<MockHardware>) -> usize {
        service
            .hardware
            .calls
            .iter()
            .filter(|c| c.as_str() == "poll_rds")
            .count()
    }

    fn enabled_at(khz: u32, hardware: MockHardware) -> FmService<MockHardware> {
        let mut service = FmService::new(hardware);
        service.tune(khz).unwrap();
        service.set_enabled(true).unwrap();
        service
    }

    #[test]
    fn rds_window_arms_on_enable_polls_at_the_cadence_and_publishes_once() {
        let events = Arc::new(Mutex::new(Vec::new()));
        // Popped from the end: two empty polls, then the complete name.
        let hardware = MockHardware {
            rds_steps: vec![Some(ps("TESTFM  ")), None, None],
            ..MockHardware::default()
        };
        let mut service = enabled_at(97_500, hardware);
        service.set_callback(Some(Box::new(RecordingCallback(events.clone()))));

        assert_eq!(service.poll_rds(0), RdsProgress::Polling);
        assert_eq!(service.poll_rds(30), RdsProgress::Polling);
        assert_eq!(service.poll_rds(60), RdsProgress::Published);
        assert_eq!(*events.lock().unwrap(), vec!["rds:TESTFM  "]);
        assert_eq!(service.get_rds_info().programme_service, "TESTFM  ");

        // Closed: no further bus traffic for this station.
        assert_eq!(service.poll_rds(90), RdsProgress::Idle);
        assert_eq!(service.poll_rds(5_000), RdsProgress::Idle);
        assert_eq!(rds_polls(&service), 3, "exactly the three polls that were needed");
    }

    #[test]
    fn rds_window_closes_at_its_budget_and_then_leaves_the_bus_alone() {
        let mut service = enabled_at(97_500, MockHardware::default());
        assert_eq!(service.poll_rds(1_000), RdsProgress::Polling, "budget starts at the first poll");
        assert_eq!(
            service.poll_rds(1_000 + qn8035::RDS_WINDOW_MS - 1),
            RdsProgress::Polling
        );
        assert_eq!(
            service.poll_rds(1_000 + qn8035::RDS_WINDOW_MS),
            RdsProgress::Idle,
            "the budget is inclusive of its own deadline"
        );
        assert_eq!(service.poll_rds(1_000 + qn8035::RDS_WINDOW_MS + 30), RdsProgress::Idle);
        assert_eq!(rds_polls(&service), 2, "no poll at or after the deadline");
        assert_eq!(service.get_rds_info(), RdsInfo::default(), "nothing invented");
    }

    #[test]
    fn rds_is_never_polled_while_disabled_or_seeking() {
        let mut service = FmService::new(MockHardware::default());
        service.tune(97_500).unwrap(); // a prepare, while disabled
        assert_eq!(service.poll_rds(0), RdsProgress::Idle);
        assert_eq!(rds_polls(&service), 0, "disabled: no bus traffic");

        service.set_enabled(true).unwrap();
        assert_eq!(service.poll_rds(0), RdsProgress::Polling, "enable armed it");

        service.start_seek(true).unwrap();
        assert_eq!(service.poll_rds(30), RdsProgress::Idle, "a walk closes the window");
        assert_eq!(rds_polls(&service), 1);

        service.complete_seek(Some(90_300));
        assert_eq!(service.poll_rds(60), RdsProgress::Polling, "a landing re-arms it");
    }

    /// The requirement that a frequency comparison cannot meet: the chip
    /// experiences a retune to the same number as a new station (its
    /// assembler resets), so the window must re-open.
    #[test]
    fn a_retune_to_the_same_frequency_rearms_the_window() {
        let mut service = enabled_at(97_500, MockHardware::default());
        let first = service.rds_generation();
        assert_eq!(service.poll_rds(0), RdsProgress::Polling);
        assert_eq!(service.poll_rds(qn8035::RDS_WINDOW_MS), RdsProgress::Idle, "spent");

        service.tune(97_500).unwrap(); // same frequency
        assert_eq!(service.rds_generation(), first + 1);
        assert_eq!(
            service.poll_rds(qn8035::RDS_WINDOW_MS + 30),
            RdsProgress::Polling,
            "re-armed although the frequency did not change"
        );
    }

    #[test]
    fn seek_outcomes_rearm_and_only_a_landing_clears_the_old_name() {
        let hardware = MockHardware {
            rds_steps: vec![Some(ps("OLDNAME "))],
            ..MockHardware::default()
        };
        let mut service = enabled_at(97_500, hardware);
        assert_eq!(service.poll_rds(0), RdsProgress::Published);
        assert_eq!(service.get_rds_info().programme_service, "OLDNAME ");

        // Exhausted walk: the chip is back on 97.5 — the name is still true,
        // the window re-opens to re-confirm it.
        service.start_seek(true).unwrap();
        service.complete_seek(None);
        assert_eq!(service.get_rds_info().programme_service, "OLDNAME ");
        assert_eq!(service.poll_rds(100), RdsProgress::Polling);

        // A landing is a new station: the name goes, the window re-opens.
        service.start_seek(true).unwrap();
        service.complete_seek(Some(90_300));
        assert_eq!(service.get_rds_info(), RdsInfo::default());
        assert_eq!(service.poll_rds(200), RdsProgress::Polling);

        // stopSeek restores the start channel (a retune): re-armed, name kept.
        service.start_seek(true).unwrap();
        service.stop_seek().unwrap();
        assert_eq!(service.poll_rds(300), RdsProgress::Polling);
    }

    #[test]
    fn a_bus_failure_during_rds_closes_the_window_without_a_retry_storm() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let hardware = MockHardware {
            fail_on: Some("poll_rds"),
            failure: "EREMOTEIO: NACK",
            ..MockHardware::default()
        };
        let mut service = enabled_at(97_500, hardware);
        service.set_callback(Some(Box::new(RecordingCallback(events.clone()))));
        assert_eq!(service.poll_rds(0), RdsProgress::Idle, "closed on the failure");
        assert_eq!(service.poll_rds(30), RdsProgress::Idle);
        assert_eq!(service.poll_rds(60), RdsProgress::Idle);
        assert_eq!(rds_polls(&service), 1, "one failed poll, then quiet");
        assert!(
            events.lock().unwrap().is_empty(),
            "an RDS read failure is not a lost tuner: {:?}",
            events.lock().unwrap()
        );
        // ...and the next tune opens a fresh window.
        service.tune(97_700).unwrap();
        assert_eq!(service.poll_rds(90), RdsProgress::Idle, "still failing, closed again");
        assert_eq!(rds_polls(&service), 2);
    }

    #[test]
    fn disabling_closes_the_window_and_drops_the_name() {
        let hardware = MockHardware {
            rds_steps: vec![Some(ps("TESTFM  "))],
            ..MockHardware::default()
        };
        let mut service = enabled_at(97_500, hardware);
        assert_eq!(service.poll_rds(0), RdsProgress::Published);
        service.set_enabled(false).unwrap();
        assert_eq!(service.get_rds_info(), RdsInfo::default());
        assert_eq!(service.poll_rds(30), RdsProgress::Idle);
        assert_eq!(rds_polls(&service), 1);
    }

    struct RecordingCallback(Arc<Mutex<Vec<String>>>);

    impl FmRadioCallback for RecordingCallback {
        fn on_seek_complete(&self, frequency_khz: u32) {
            self.0
                .lock()
                .unwrap()
                .push(format!("complete:{frequency_khz}"));
        }

        fn on_seek_stopped(&self, band_exhausted: bool) {
            self.0
                .lock()
                .unwrap()
                .push(format!("stopped:{band_exhausted}"));
        }

        fn on_rds_update(&self, rds: &RdsInfo) {
            self.0
                .lock()
                .unwrap()
                .push(format!("rds:{}", rds.programme_service));
        }

        fn on_tuner_lost(&self, reason: &str) {
            self.0.lock().unwrap().push(format!("lost:{reason}"));
        }
    }

    #[test]
    fn maps_state_chip_tune_and_quality_methods() {
        let hardware = MockHardware {
            chip_id: qn8035::CHIP_ID,
            ..MockHardware::default()
        };
        let mut service = FmService::new(hardware);
        assert_eq!(service.get_state().i2c_adapter, "/dev/i2c-1 (i2c-fm)");
        service.set_enabled(true).unwrap();
        assert_eq!(service.get_chip_id().unwrap(), qn8035::CHIP_ID);
        service.tune(97_500).unwrap();
        assert_eq!(service.get_state().frequency_khz, 97_500);
        assert!(service.get_signal_quality().unwrap().station_present);
    }

    #[test]
    fn tune_while_disabled_is_prepared_and_enable_commits_that_frequency() {
        let mut service = FmService::new(MockHardware::default());
        service.tune(97_500).unwrap();
        assert_eq!(service.get_state().frequency_khz, 97_500);
        assert!(
            service.hardware.calls.is_empty(),
            "prepare must not touch hardware"
        );

        service.set_enabled(true).unwrap();
        assert_eq!(service.hardware.frequency_khz, Some(97_500));
        assert_eq!(service.hardware.calls, ["set_enabled(true,Some(97500))"]);
    }

    #[test]
    fn tune_while_enabled_is_applied_immediately() {
        let mut service = FmService::new(MockHardware::default());
        service.set_enabled(true).unwrap();
        service.tune(99_900).unwrap();
        assert_eq!(service.hardware.frequency_khz, Some(99_900));
        assert!(service.hardware.calls.contains(&"tune(99900)".to_owned()));
    }

    #[test]
    fn audio_owner_loss_disables_and_clears_worker_state_before_rail_loss() {
        let mut service = FmService::new(MockHardware::default());
        service.tune(97_500).unwrap();
        service.set_enabled(true).unwrap();
        service.start_seek(true).unwrap();

        service.audio_route_lost(true).unwrap();

        assert_eq!(service.get_state().frequency_khz, 97_500);
        assert!(!service.get_state().enabled);
        assert!(!service.get_state().seeking);
        assert_eq!(service.get_rds_info(), RdsInfo::default());
        assert_eq!(
            &service.hardware.calls[2..],
            ["set_enabled(false,None)", "power_lost"]
        );
        assert_eq!(service.poll_seek(), SeekProgress::Idle);
        assert_eq!(service.poll_rds(0), RdsProgress::Idle);
    }

    #[test]
    fn failed_owner_loss_disable_still_invalidates_state_without_more_bus_io() {
        let mut service = FmService::new(MockHardware::default());
        service.set_enabled(true).unwrap();
        service.state.seeking = true;
        service.rds_window = Some(RdsWindow {
            generation: 1,
            deadline_ms: None,
            polls: 0,
        });
        service.hardware.fail_on = Some("set_enabled");
        service.hardware.failure = "EREMOTEIO: NACK";

        let error = service.audio_route_lost(true).unwrap_err();

        assert_eq!(error.code, ExceptionCode::ServiceSpecific);
        assert!(!service.get_state().enabled);
        assert!(!service.get_state().seeking);
        assert!(service.rds_window.is_none());
        assert_eq!(
            &service.hardware.calls[1..],
            ["set_enabled(false,None)", "power_lost"]
        );
    }

    #[test]
    fn already_unpowered_owner_loss_is_strictly_bus_free() {
        let mut service = FmService::new(MockHardware::default());
        service.state.enabled = true;
        service.state.seeking = true;

        service.audio_route_lost(false).unwrap();

        assert_eq!(service.hardware.calls, ["power_lost"]);
        assert!(!service.get_state().enabled);
        assert!(!service.get_state().seeking);
    }

    #[test]
    fn invalid_tune_is_rejected_before_hardware() {
        let mut service = FmService::new(MockHardware::default());
        let error = service.tune(87_525).unwrap_err();
        assert_eq!(error.code, ExceptionCode::IllegalArgument);
        assert!(service.into_inner().calls.is_empty());
    }

    #[test]
    fn nodev_and_nack_are_service_specific_and_notify_the_client() {
        for (operation, failure) in [("chip_id", "ENODEV"), ("tune", "EREMOTEIO: NACK")] {
            let events = Arc::new(Mutex::new(Vec::new()));
            let hardware = MockHardware {
                fail_on: Some(operation),
                failure,
                ..MockHardware::default()
            };
            let mut service = FmService::new(hardware);
            service.set_callback(Some(Box::new(RecordingCallback(events.clone()))));
            service.set_enabled(true).unwrap();
            let error = if operation == "chip_id" {
                service.get_chip_id().unwrap_err()
            } else {
                service.tune(97_500).unwrap_err()
            };
            assert_eq!(error.code, ExceptionCode::ServiceSpecific);
            assert!(error.message.contains(failure));
            assert_eq!(*events.lock().unwrap(), [format!("lost:{failure}")]);
        }
    }

    #[test]
    fn signal_quality_nack_is_reported() {
        let hardware = MockHardware {
            fail_on: Some("signal_quality"),
            failure: "EREMOTEIO: NACK",
            ..MockHardware::default()
        };
        let mut service = FmService::new(hardware);
        service.set_enabled(true).unwrap();
        let error = service.get_signal_quality().unwrap_err();
        assert_eq!(error.code, ExceptionCode::ServiceSpecific);
    }

    #[test]
    fn enable_wrong_identity_maps_to_illegal_state() {
        let hardware = MockHardware {
            wrong_chip_on_enable: Some(0),
            ..MockHardware::default()
        };
        let mut service = FmService::new(hardware);
        let error = service.set_enabled(true).unwrap_err();
        assert_eq!(error.code, ExceptionCode::IllegalState);
        assert!(!service.get_state().enabled);
    }

    #[test]
    fn seek_is_async_and_callbacks_map_both_outcomes() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut service = FmService::new(MockHardware::default());
        service.set_callback(Some(Box::new(RecordingCallback(events.clone()))));
        service.tune(97_500).unwrap();
        service.set_enabled(true).unwrap();
        service.start_seek(true).unwrap();
        assert!(service.get_state().seeking);
        service.complete_seek(Some(90_300));
        service.start_seek(false).unwrap();
        service.complete_seek(None);
        assert_eq!(
            *events.lock().unwrap(),
            vec!["complete:90300", "stopped:true"]
        );
    }

    /// The bug this whole worker exists to fix, planted so it cannot return.
    ///
    /// `FmHardware::poll_seek` is DEFAULTED to "nothing queued". A backend that
    /// accepts `startSeek` but never implements the step therefore leaves the
    /// service seeking for ever and fires no callback — which is exactly what
    /// shipped: the HUD backend returned NotReady from `start_seek`, and had it
    /// instead returned Ok without a worker, this is the state the user would
    /// have seen. Scanning is "not working" in precisely this shape.
    #[test]
    fn a_backend_that_never_steps_leaves_the_seek_hanging() {
        struct NeverSteps(MockHardware);
        impl FmHardware for NeverSteps {
            type Error = &'static str;
            fn adapter_label(&self) -> String {
                self.0.adapter_label()
            }
            fn set_enabled(
                &mut self,
                enabled: bool,
                prepared: Option<u32>,
            ) -> Result<(), EnableError<Self::Error>> {
                self.0.set_enabled(enabled, prepared)
            }
            fn chip_id(&mut self) -> Result<u8, Self::Error> {
                self.0.chip_id()
            }
            fn tune(&mut self, frequency_khz: u32) -> Result<(), Self::Error> {
                self.0.tune(frequency_khz)
            }
            fn signal_quality(&mut self) -> Result<qn8035::SignalQuality, Self::Error> {
                self.0.signal_quality()
            }
            fn start_seek(&mut self, d: SeekDirection, f: u32) -> Result<(), Self::Error> {
                self.0.start_seek(d, f)
            }
            fn stop_seek(&mut self) -> Result<(), Self::Error> {
                self.0.stop_seek()
            }
            // poll_seek deliberately NOT implemented -- the default applies.
        }

        let events = Arc::new(Mutex::new(Vec::new()));
        let mut service = FmService::new(NeverSteps(MockHardware::default()));
        service.set_callback(Some(Box::new(RecordingCallback(events.clone()))));
        service.tune(97_500).unwrap();
        service.set_enabled(true).unwrap();
        service.start_seek(true).unwrap();
        for _ in 0..50 {
            assert_eq!(
                service.poll_seek(),
                SeekProgress::Idle,
                "the default step never advances the walk"
            );
        }
        assert!(service.get_state().seeking, "still seeking after 50 steps");
        assert!(
            events.lock().unwrap().is_empty(),
            "a hung seek fires no callback -- the user sees a scan that never ends"
        );
    }

    #[test]
    fn poll_seek_is_a_no_op_when_no_seek_is_in_flight() {
        let mut service = FmService::new(MockHardware::default());
        service.tune(97_500).unwrap();
        service.set_enabled(true).unwrap();
        // It must return early WITHOUT touching the bus: poll_seek on an idle
        // service reaching the hardware would tune a channel nobody asked for.
        assert_eq!(
            service.poll_seek(),
            SeekProgress::Idle,
            "idle must report no seek in flight"
        );
    }

    #[test]
    fn poll_seek_preserves_each_requested_wait() {
        let hardware = MockHardware {
            seek_steps: vec![SeekStep::Settling(925), SeekStep::Settling(275)],
            seek_candidate_steps: vec![Some(90_100), Some(90_100)],
            ..MockHardware::default()
        };
        let mut service = FmService::new(hardware);
        service.tune(90_000).unwrap();
        service.set_enabled(true).unwrap();
        service.start_seek(true).unwrap();
        assert_eq!(service.poll_seek(), SeekProgress::Settling(275));
        assert_eq!(service.get_state().frequency_khz, 90_100);
        assert_eq!(service.poll_seek(), SeekProgress::Settling(925));
        assert_eq!(service.get_state().frequency_khz, 90_100);
        assert!(service.get_state().seeking);
    }

    #[test]
    fn poll_seek_walks_one_channel_per_call_and_completes_on_a_station() {
        let events = Arc::new(Mutex::new(Vec::new()));
        // Popped from the end: two Continue steps, then a station at 90.3.
        let hardware = MockHardware {
            seek_steps: vec![
                SeekStep::Finished(Some(90_300)),
                SeekStep::Continue,
                SeekStep::Settling(qn8035::SEEK_SETTLE_MS),
                SeekStep::Continue,
                SeekStep::Settling(qn8035::SEEK_SETTLE_MS),
            ],
            ..MockHardware::default()
        };
        let mut service = FmService::new(hardware);
        service.set_callback(Some(Box::new(RecordingCallback(events.clone()))));
        service.tune(97_500).unwrap();
        service.set_enabled(true).unwrap();
        service.start_seek(true).unwrap();

        // Two calls per channel: tune, wait SEEK_SETTLE_MS, then judge.
        assert_eq!(service.poll_seek(), SeekProgress::Settling(qn8035::SEEK_SETTLE_MS), "tune 1");
        assert_eq!(service.poll_seek(), SeekProgress::Stepping, "judge 1");
        assert_eq!(service.poll_seek(), SeekProgress::Settling(qn8035::SEEK_SETTLE_MS), "tune 2");
        assert_eq!(service.poll_seek(), SeekProgress::Stepping, "judge 2");
        assert!(events.lock().unwrap().is_empty(), "no callback while walking");
        assert_eq!(service.poll_seek(), SeekProgress::Idle, "third channel lands");

        assert_eq!(*events.lock().unwrap(), vec!["complete:90300"]);
        assert!(!service.get_state().seeking);
        assert_eq!(
            service.get_state().frequency_khz,
            90_300,
            "a landed seek adopts the found channel"
        );
    }

    #[test]
    fn poll_seek_publishes_the_inflight_candidate_and_cancel_restores_the_start() {
        // `getState()` is the already-frozen, bus-free progress channel. The
        // actual front end is on this candidate while it settles; keeping the
        // old completed station here makes a scan indicator lie for the whole
        // walk. A later cancel must publish the restored start instead.
        let hardware = MockHardware {
            seek_steps: vec![SeekStep::Settling(qn8035::SEEK_SETTLE_MS)],
            seek_candidate_steps: vec![Some(88_000)],
            ..MockHardware::default()
        };
        let mut service = FmService::new(hardware);
        service.tune(97_500).unwrap();
        service.set_enabled(true).unwrap();
        service.start_seek(true).unwrap();

        assert_eq!(service.poll_seek(), SeekProgress::Settling(qn8035::SEEK_SETTLE_MS));
        assert!(service.get_state().seeking);
        assert_eq!(
            service.get_state().frequency_khz,
            88_000,
            "the in-flight candidate, not the last completed station, is public state"
        );

        service.stop_seek().unwrap();
        assert!(!service.get_state().seeking);
        assert_eq!(service.get_state().frequency_khz, 97_500);
    }

    #[test]
    fn poll_seek_reports_an_exhausted_band_through_the_stopped_callback() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let hardware = MockHardware {
            seek_steps: vec![SeekStep::Finished(None)],
            ..MockHardware::default()
        };
        let mut service = FmService::new(hardware);
        service.set_callback(Some(Box::new(RecordingCallback(events.clone()))));
        service.tune(97_500).unwrap();
        service.set_enabled(true).unwrap();
        service.start_seek(true).unwrap();
        assert_eq!(service.poll_seek(), SeekProgress::Idle);
        assert_eq!(*events.lock().unwrap(), vec!["stopped:true"]);
        assert_eq!(
            service.get_state().frequency_khz,
            97_500,
            "an exhausted band leaves the original channel alone"
        );
    }

    #[test]
    fn poll_seek_ends_the_seek_and_reports_tuner_lost_on_bus_failure() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let hardware = MockHardware {
            fail_on: Some("poll_seek"),
            failure: "ENXIO",
            ..MockHardware::default()
        };
        let mut service = FmService::new(hardware);
        service.set_callback(Some(Box::new(RecordingCallback(events.clone()))));
        service.tune(97_500).unwrap();
        service.set_enabled(true).unwrap();
        service.start_seek(true).unwrap();
        assert_eq!(
            service.poll_seek(),
            SeekProgress::Idle,
            "bus loss must end the walk, not spin"
        );
        assert!(!service.get_state().seeking);
        let seen = events.lock().unwrap().clone();
        assert!(seen.contains(&"stopped:true".to_owned()), "got {seen:?}");
        assert!(
            seen.iter().any(|e| e.contains("ENXIO")),
            "the tuner-lost reason must reach the client: {seen:?}"
        );
    }

    #[test]
    fn second_seek_is_illegal_state_and_stop_is_idempotent() {
        let mut service = FmService::new(MockHardware::default());
        service.tune(97_500).unwrap();
        service.set_enabled(true).unwrap();
        service.hardware.calls.clear();
        service.start_seek(true).unwrap();
        assert_eq!(
            service.start_seek(true).unwrap_err().code,
            ExceptionCode::IllegalState
        );
        service.stop_seek().unwrap();
        service.stop_seek().unwrap();
        assert_eq!(service.into_inner().calls, vec!["seek(Up)", "stop_seek"]);
    }

    #[test]
    fn rds_and_tuner_lost_reach_the_callback() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut service = FmService::new(MockHardware::default());
        service.set_callback(Some(Box::new(RecordingCallback(events.clone()))));
        service.publish_rds(RdsState {
            programme_identification: 0x8412,
            programme_service: "TESTFM  ".to_owned(),
            synchronized: true,
        });
        service.tuner_lost("ENXIO");
        assert_eq!(*events.lock().unwrap(), vec!["rds:TESTFM  ", "lost:ENXIO"]);
    }

    #[test]
    fn all_nine_fm_methods_are_runnable_with_default_features() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut service = FmService::new(MockHardware {
            chip_id: qn8035::CHIP_ID,
            ..MockHardware::default()
        });
        let api: &mut dyn FmApi = &mut service;

        api.set_callback(Some(Box::new(RecordingCallback(events))));
        assert!(!api.get_state().enabled);
        api.tune(97_500).unwrap();
        api.set_enabled(true).unwrap();
        assert_eq!(api.get_chip_id().unwrap(), qn8035::CHIP_ID);
        assert!(api.get_signal_quality().unwrap().station_present);
        api.start_seek(true).unwrap();
        api.stop_seek().unwrap();
        assert_eq!(api.get_rds_info(), RdsInfo::default());
    }

    #[test]
    fn phase_4c_backend_registers_without_hardware_and_reports_not_ready() {
        let mut service = FmService::new(Phase4cAudioGuardUnavailable);

        // Construction/getState are Binder registration operations: they must
        // be possible before any I2C/GPIO/MCU/DSP facility exists.
        assert_eq!(
            service.get_state().i2c_adapter,
            PHASE_4C_AUDIO_GUARD_UNAVAILABLE
        );
        assert_eq!(service.get_rds_info(), RdsInfo::default());

        // A disabled tune is the documented prepare-only half, so it remains
        // cached without attempting a physical operation.
        service.tune(97_500).unwrap();
        assert_eq!(service.get_state().frequency_khz, 97_500);

        for error in [
            service.get_chip_id().unwrap_err(),
            service.set_enabled(true).unwrap_err(),
            service.get_signal_quality().unwrap_err(),
            service.start_seek(true).unwrap_err(),
        ] {
            assert_eq!(error.code, ExceptionCode::ServiceSpecific);
            assert_eq!(error.message, PHASE_4C_AUDIO_GUARD_UNAVAILABLE);
        }
        assert!(!service.get_state().enabled);
    }

    #[test]
    fn disabled_queries_are_refused_by_state_without_bus_access_or_tuner_lost() {
        // The unpowered chip would NACK every read: the mock proves no read
        // happens by failing them AND by recording them.
        for operation in ["chip_id", "signal_quality"] {
            let events = Arc::new(Mutex::new(Vec::new()));
            let hardware = MockHardware {
                fail_on: Some(operation),
                failure: "ENXIO: No such device or address",
                ..MockHardware::default()
            };
            let mut service = FmService::new(hardware);
            service.set_callback(Some(Box::new(RecordingCallback(events.clone()))));
            // A prepared channel does not make the radio enabled.
            service.tune(94_900).unwrap();

            let chip = service.get_chip_id().unwrap_err();
            let quality = service.get_signal_quality().unwrap_err();

            for error in [&chip, &quality] {
                assert_eq!(error.code, ExceptionCode::IllegalState, "{operation}");
                assert!(
                    error.message.contains("requires an enabled radio"),
                    "{}",
                    error.message
                );
            }
            assert!(
                events.lock().unwrap().is_empty(),
                "an expected NACK is not a lost tuner"
            );
            assert!(
                service.into_inner().calls.is_empty(),
                "zero bus calls while disabled"
            );
        }
    }

    #[test]
    fn enabled_query_failures_still_reach_the_client_as_tuner_lost() {
        for operation in ["chip_id", "signal_quality"] {
            let events = Arc::new(Mutex::new(Vec::new()));
            let hardware = MockHardware {
                fail_on: Some(operation),
                failure: "EREMOTEIO: NACK",
                ..MockHardware::default()
            };
            let mut service = FmService::new(hardware);
            service.set_callback(Some(Box::new(RecordingCallback(events.clone()))));
            service.set_enabled(true).unwrap();
            let error = if operation == "chip_id" {
                service.get_chip_id().unwrap_err()
            } else {
                service.get_signal_quality().unwrap_err()
            };
            assert_eq!(error.code, ExceptionCode::ServiceSpecific);
            assert_eq!(*events.lock().unwrap(), ["lost:EREMOTEIO: NACK"]);
            assert_eq!(
                service.into_inner().calls,
                ["set_enabled(true,None)", operation]
            );
        }
    }

    #[test]
    fn disabled_no_ops_remain_bus_free() {
        let mut service = FmService::new(MockHardware::default());

        service.set_enabled(false).unwrap();
        service.stop_seek().unwrap();
        service.tune(97_500).unwrap();

        assert!(
            service.into_inner().calls.is_empty(),
            "disabled no-ops and prepare must not touch hardware"
        );
    }
}
