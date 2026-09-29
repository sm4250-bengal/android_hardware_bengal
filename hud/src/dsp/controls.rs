// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! The IDsp V2 control path: validation, the last-applied model, and the
//! exception mapping — all of it free of `#[cfg(feature = "binder")]`.
//!
//! This module exists in this shape for one reason: `dsp/binder.rs` cannot be
//! type-checked on a plain desk (it needs `libbinder_rs` and the Soong-generated
//! `vendor.xyauto.dsp-V*-rust` crate), so anything that can be got wrong must
//! live where `cargo test --offline` can reach it. What is left in the adapter
//! is parcelable field copying and nothing else.
//!
//! # What it will not do
//!
//! * Construction loads semantic intent but writes no hardware. The guard's
//!   activation boundary restores it before enabling the DAC/amplifier.
//! * No repaired or clamped values. An out-of-range request is refused; it is
//!   never quietly moved to the nearest legal value, because a slider that
//!   silently disagrees with the hardware is how a user loses trust in the
//!   readback.
//! * No write while the guard is unavailable or fault-latched — checked here,
//!   before the ioctl, so an unsupported or unsafe request never reaches the
//!   kernel at all.
//! * No model update on a refusal or a failure. The model is what this daemon
//!   successfully wrote, so a failed write must leave it exactly as it was.

use super::guard::{
    service_specific_errno, AudioGuard, DspOutput, Error, KernelTransport, Transport,
    EQ_BAND_COUNT, MAX_EQ_STEPS, MAX_OUTPUT_DELAY_STEPS, MAX_OUTPUT_GAIN_STEPS, MIN_EQ_STEPS,
    MIN_OUTPUT_GAIN_STEPS,
};
use std::sync::{Arc, Mutex};

mod persistence;

/// Device-encrypted, daemon-owned semantic settings; available before class hal.
pub const SETTINGS_PATH: &str = "/data/vendor/bengal-hud/dsp-settings-v1";

/// One output-gain step in millibels (hundredths of a decibel).
///
/// Re-derived from `xyauto_audio_guard_coefficients.h`'s `output_gain_coeff[401]`
/// rather than assumed: index 200 is exactly 5.23 unity (0 dB), the mean step
/// across all 400 intervals is 0.100343 dB, and the endpoints are -20.0687 and
/// +20.0687 dB. One step is therefore 0.1 dB, and 0.1 dB is 10 mB — not 100.
pub const GAIN_STEP_MILLIBELS: i32 = 10;

/// Lowest accepted delay request step.
pub const DELAY_MIN_STEPS: i32 = 0;

/// **0 = UNKNOWN.** The physical duration of one delay step is NOT proven on
/// device 3, and this constant exists to say so rather than to carry a number.
///
/// The hypothesis, recorded so it is neither lost nor mistaken for a result:
/// the guard writes `steps + 1` into the ADAU delay cell
/// (`xyauto_audio_guard.c:1016-1018`), the ADAU1701 delay block counts whole
/// samples, and this program's core rate is pinned at 48 kHz in
/// [`super::SAMPLE_RATE_HZ`] by reproducing the OEM's own captured 340 Hz
/// Butterworth high-pass to 5e-7; device 1's shipped UI captions the same OEM
/// control "N samples / N/48.0 ms". If all of that holds, one step is
/// 20.8333 us — which is not a whole number of microseconds either. Nothing
/// has confirmed it against device-3 hardware, so this stays 0.
pub const DELAY_STEP_MICROS: i32 = 0;

/// uAPI request steps per distinct SM4250 EQ filter.
///
/// The OEM encoder truncates `step / 2`, so consecutive pairs of request steps
/// write byte-identical coefficients.
pub const EQ_STEPS_PER_FILTER: i32 = 2;

/// Distinct filters per band across the 41 requestable steps.
///
/// Verified against the shipped table by counting unique 5-word rows: exactly
/// 21 in every band, with -1/0/+1 all the bypass row and only +/-20 unshared.
pub const EQ_DISTINCT_FILTERS: i32 = 21;

/// Millibels per EQ **filter** step — deliberately not per request step.
///
/// Derived by walking every distinct filter of every band for its response
/// peak: filter index `trunc(step/2)` runs -10..+10 and its peak gain is
/// 0.25 dB times that index, to within 0.00087 dB worst case over all four
/// bands and all 41 steps. One REQUEST step is therefore 12.5 mB, which is not
/// an integer — which is exactly why this counts filter steps, and why a UI
/// must divide by [`EQ_STEPS_PER_FILTER`] and mark the result approximate.
pub const EQ_STEP_MILLIBELS: i32 = 25;

/// The full span of the SM4250 EQ request range, in millibels.
///
/// +/-20 request steps is exactly +/-2.500 dB in the shipped `sm4250_eq_coeff`
/// table (10 filter steps x 25 mB). This is the number that makes "the EQ is
/// gentle" visible instead of a surprise: +/-20 is not +/-20 dB.
pub const EQ_FULL_SCALE_MILLIBELS: i32 = 250;

/// Centre frequency of each SM4250 EQ band, in hertz — DERIVED FROM
/// `sm4250_eq_coeff`, NOT MEASURED ON DEVICE 3.
///
/// Obtained from this repository's own coefficient table — response peaks at
/// 20.2 / 96.0 / 470.1 / 2281.5 Hz at +2.500 dB, fs = 48 kHz, biquad word order
/// `b0,b1,b2,-a1,-a2` settled by pole radius — and independently equal to
/// device 1's shipped labels. These are the shipped FILTERS' centres; what a
/// microphone in this car would report is a different question.
pub const EQ_BAND_CENTRE_HZ: [i32; 4] = [20, 96, 470, 2281];

/// `-EBUSY`: a source or FM-mix transition is still pending.
const EBUSY: i32 = 16;
/// `-EPIPE`: the DSP is unpowered or the IC60 program is not loaded.
const EPIPE: i32 = 32;
/// `-EUCLEAN`: the guard has latched a fault and refuses every operation.
const EUCLEAN: i32 = 117;

/// How a control refusal or failure is reported to Binder.
///
/// The variants are exactly the AIDL exception codes IDsp V2 documents, so the
/// adapter is a `match` with no policy of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlError {
    /// `EX_ILLEGAL_ARGUMENT`: unknown output or band, or a value out of range.
    IllegalArgument(String),
    /// `EX_ILLEGAL_STATE`: the guard cannot accept a write right now.
    IllegalState(String),
    /// `EX_UNSUPPORTED_OPERATION`: this build has no such control at all.
    Unsupported(String),
    /// `EX_SERVICE_SPECIFIC` with the negative errno of an unexpected ioctl
    /// failure, plus its text.
    ServiceSpecific(i32, String),
}

impl ControlError {
    /// The message a caller sees. Diagnostic text; never parse it.
    pub fn message(&self) -> &str {
        match self {
            Self::IllegalArgument(message)
            | Self::IllegalState(message)
            | Self::Unsupported(message)
            | Self::ServiceSpecific(_, message) => message,
        }
    }

    /// Map one guard error onto the AIDL contract.
    ///
    /// The three state errnos the kernel uses for "not now" become
    /// `EX_ILLEGAL_STATE` with the errno kept in the text, because a caller
    /// that retries after an ACC cycle wants a state, not a service-specific
    /// code it would have to special-case. Everything else keeps its errno.
    pub fn from_guard(error: Error) -> Self {
        match error {
            Error::InvalidArgument(message) => Self::IllegalArgument(message.to_owned()),
            Error::InvalidState(message) => Self::IllegalState(message.to_owned()),
            Error::NotReady(reason) => Self::IllegalState(reason),
            Error::DeviceAbsent(inner) => Self::IllegalState(format!("{inner}")),
            Error::Io(inner) => match inner.raw_os_error() {
                Some(EBUSY) => Self::IllegalState(format!(
                    "audio guard busy, a route transition is pending (errno {EBUSY} EBUSY)"
                )),
                Some(EPIPE) => Self::IllegalState(format!(
                    "audio guard has no loaded DSP program (errno {EPIPE} EPIPE)"
                )),
                Some(EUCLEAN) => Self::IllegalState(format!(
                    "audio guard has latched a fault (errno {EUCLEAN} EUCLEAN)"
                )),
                _ => {
                    let code = service_specific_errno(&inner);
                    Self::ServiceSpecific(code, format!("audio guard ioctl failed: {inner}"))
                }
            },
        }
    }
}

/// What this build's control path can do, and in which units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// Whether a write attempted right now would reach the chip. Live state.
    pub writable: bool,
    /// Number of logical outputs. Always 5.
    pub output_count: i32,
    /// Number of SM4250 EQ bands. Always 4.
    pub eq_band_count: i32,
    /// Lowest accepted delay request step. Always 0.
    pub delay_min_steps: i32,
    /// Highest accepted delay request step.
    pub delay_max_steps: i32,
    /// Microseconds per delay step, or 0 = unknown; see [`DELAY_STEP_MICROS`].
    pub delay_step_micros: i32,
    /// Lowest accepted output-gain step.
    pub gain_min_steps: i32,
    /// Highest accepted output-gain step.
    pub gain_max_steps: i32,
    /// Millibels per output-gain step.
    pub gain_step_millibels: i32,
    /// Lowest accepted EQ request step.
    pub eq_min_steps: i32,
    /// Highest accepted EQ request step.
    pub eq_max_steps: i32,
    /// Request steps per distinct filter. Always 2.
    pub eq_steps_per_filter: i32,
    /// Distinct filters per band. Always 21.
    pub eq_distinct_filters: i32,
    /// Millibels per EQ FILTER step; see [`EQ_STEP_MILLIBELS`].
    pub eq_step_millibels: i32,
    /// Millibels spanned by the full EQ request range.
    pub eq_full_scale_millibels: i32,
    /// Derived centre frequency of each band.
    pub eq_band_centre_hz: Vec<i32>,
    /// Whether the subwoofer has a delay cell. Always false.
    pub subwoofer_delay_supported: bool,
}

impl Capabilities {
    /// Build the capability report for a given live writability.
    pub fn new(writable: bool) -> Self {
        Self {
            writable,
            output_count: DspOutput::ALL.len() as i32,
            eq_band_count: EQ_BAND_COUNT,
            delay_min_steps: DELAY_MIN_STEPS,
            delay_max_steps: MAX_OUTPUT_DELAY_STEPS,
            delay_step_micros: DELAY_STEP_MICROS,
            gain_min_steps: MIN_OUTPUT_GAIN_STEPS,
            gain_max_steps: MAX_OUTPUT_GAIN_STEPS,
            gain_step_millibels: GAIN_STEP_MILLIBELS,
            eq_min_steps: MIN_EQ_STEPS,
            eq_max_steps: MAX_EQ_STEPS,
            eq_steps_per_filter: EQ_STEPS_PER_FILTER,
            eq_distinct_filters: EQ_DISTINCT_FILTERS,
            eq_step_millibels: EQ_STEP_MILLIBELS,
            eq_full_scale_millibels: EQ_FULL_SCALE_MILLIBELS,
            eq_band_centre_hz: EQ_BAND_CENTRE_HZ.to_vec(),
            subwoofer_delay_supported: DspOutput::Subwoofer.has_delay_cell(),
        }
    }
}

/// The `delaySteps` value reported for an output that has no delay cell.
pub const DELAY_UNSUPPORTED: i32 = -1;

/// One logical output's last successfully applied state, PER CONTROL.
///
/// Each `*_applied` flag is set by a successful ioctl for THAT control alone.
/// A successful mute leaves `phase_applied`, `delay_applied` and `gain_applied`
/// false, so the values beside them stay visibly meaningless — which is what
/// stops a caller replaying a remembered mute from dragging three inert
/// defaults onto the hardware with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputState {
    /// Which output this row describes.
    pub output: DspOutput,
    /// True once a mute write for this output succeeded this boot.
    pub mute_applied: bool,
    /// Last applied mute; meaningless unless `mute_applied`.
    pub muted: bool,
    /// True once a phase write for this output succeeded this boot.
    pub phase_applied: bool,
    /// Last applied polarity inversion; meaningless unless `phase_applied`.
    pub phase_inverted: bool,
    /// True once a delay write succeeded this boot. Always false for the
    /// subwoofer, which has no delay cell to write.
    pub delay_applied: bool,
    /// Last applied delay in request steps, or [`DELAY_UNSUPPORTED`].
    pub delay_steps: i32,
    /// True once a gain write for this output succeeded this boot.
    pub gain_applied: bool,
    /// Last applied gain in request steps; meaningless unless `gain_applied`.
    pub gain_steps: i32,
}

/// One SM4250 EQ band's last successfully applied request step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EqBandState {
    /// Band index 0..3.
    pub band: i32,
    /// False until this daemon wrote this band this boot.
    pub applied: bool,
    /// Last applied uAPI request step, -20..20. Not decibels, and two steps
    /// per distinct filter.
    pub steps: i32,
}

/// The daemon's complete last-applied model. Never a chip readback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// One row per output, in [`DspOutput::ALL`] order.
    pub outputs: Vec<OutputState>,
    /// One row per band, in band order.
    pub eq: Vec<EqBandState>,
    /// Increments once per successfully applied write, and never otherwise.
    pub generation: u64,
    /// The last refusal or failure; empty after a success or before any write.
    pub last_error: String,
}

#[derive(Clone, Debug)]
struct ControlModel {
    outputs: Vec<OutputState>,
    eq: Vec<EqBandState>,
    generation: u64,
    last_error: String,
    enhancements: EnhancementSnapshot,
    /// The guard program epoch this model was last reconciled against.
    program_epoch: Option<u64>,
    persistence_error: String,
}

/// Recorded as `last_error` when a program reload invalidated applied values.
pub const PROGRAM_RELOADED: &str = "the DSP program was reloaded (ACC recovery or daemon \
restart); earlier control writes are no longer applied";

impl ControlModel {
    fn new() -> Self {
        Self {
            outputs: DspOutput::ALL
                .iter()
                .map(|output| OutputState {
                    output: *output,
                    mute_applied: false,
                    muted: false,
                    phase_applied: false,
                    phase_inverted: false,
                    delay_applied: false,
                    delay_steps: if output.has_delay_cell() {
                        0
                    } else {
                        DELAY_UNSUPPORTED
                    },
                    gain_applied: false,
                    gain_steps: 0,
                })
                .collect(),
            eq: (0..EQ_BAND_COUNT)
                .map(|band| EqBandState {
                    band,
                    applied: false,
                    steps: 0,
                })
                .collect(),
            generation: 0,
            last_error: String::new(),
            enhancements: EnhancementSnapshot::default(),
            program_epoch: None,
            persistence_error: String::new(),
        }
    }

    /// Adopt the guard's program epoch. A change after anything was applied
    /// means the chip now holds the program defaults, not this model.
    fn observe_program_epoch(&mut self, epoch: u64) {
        if self.program_epoch.is_some_and(|seen| seen != epoch) && self.generation != 0 {
            self.program_reloaded();
        }
        self.program_epoch = Some(epoch);
    }

    /// Forget every applied value; keep the policy input and the generation
    /// (it still counts successful writes) and record why.
    fn program_reloaded(&mut self) {
        let generation = self.generation;
        let volume_index_known = self.enhancements.volume_index_known;
        let volume_index = self.enhancements.volume_index;
        let persistence_error = self.persistence_error.clone();
        *self = Self::new();
        self.generation = generation;
        self.enhancements.volume_index_known = volume_index_known;
        self.enhancements.volume_index = volume_index;
        self.persistence_error = persistence_error;
        self.last_error = PROGRAM_RELOADED.to_owned();
    }

    fn row(&mut self, output: DspOutput) -> &mut OutputState {
        let index = DspOutput::ALL
            .iter()
            .position(|candidate| *candidate == output)
            .expect("DspOutput::ALL contains every output");
        &mut self.outputs[index]
    }

    /// Apply one successful write. This is the ONLY place `generation` moves,
    /// and each arm touches exactly ONE value and ONE validity flag.
    fn record(&mut self, change: Change) {
        match change {
            Change::Mute(output, muted) => {
                let row = self.row(output);
                row.mute_applied = true;
                row.muted = muted;
            }
            Change::Phase(output, inverted) => {
                let row = self.row(output);
                row.phase_applied = true;
                row.phase_inverted = inverted;
            }
            Change::Delay(output, steps) => {
                let row = self.row(output);
                row.delay_applied = true;
                row.delay_steps = steps;
            }
            Change::Gain(output, steps) => {
                let row = self.row(output);
                row.gain_applied = true;
                row.gain_steps = steps;
            }
            Change::Eq(band, steps) => {
                let row = &mut self.eq[band as usize];
                row.applied = true;
                row.steps = steps;
            }
            Change::Bass(level) => {
                self.enhancements.bass_applied = true;
                self.enhancements.bass_level = level;
            }
            Change::Treble(level) => {
                self.enhancements.treble_applied = true;
                self.enhancements.treble_level = level;
            }
            Change::Loudness(index) => {
                self.enhancements.loudness_applied = true;
                self.enhancements.loudness_enabled = index.is_some();
                self.enhancements.loudness_applied_volume_index = index.unwrap_or(-1);
            }
        }
        self.generation += 1;
        self.last_error.clear();
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            outputs: self.outputs.clone(),
            eq: self.eq.clone(),
            generation: self.generation,
            last_error: self.error_text(),
        }
    }

    fn error_text(&self) -> String {
        if self.persistence_error.is_empty() { self.last_error.clone() }
        else { self.persistence_error.clone() }
    }
}

/// A hook run in the window between a successful ioctl and its publication.
///
/// Only tests install one — it is the seam the two-writer ordering regression
/// needs, because that window is exactly where the publication race lived. It
/// is compiled unconditionally rather than behind `#[cfg(test)]` so the code
/// path the regression exercises is byte-for-byte the code that ships; the
/// production cost is one uncontended lock and a `None` check per successful
/// write, and nothing installs a hook.
type PublishBarrier = Arc<dyn Fn() + Send + Sync>;

/// One semantic change, recorded only after its ioctl returned success.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Mute(DspOutput, bool),
    Phase(DspOutput, bool),
    Delay(DspOutput, i32),
    Gain(DspOutput, i32),
    Eq(i32, i32),
    Bass(i32),
    Treble(i32),
    Loudness(Option<i32>),
}

impl Change {
    fn apply_to<T: Transport>(self, guard: &mut AudioGuard<T>) -> Result<(), Error> {
        match self {
            Self::Mute(o, v) => guard.set_output_mute(o, v),
            Self::Phase(o, v) => guard.set_output_phase_inverted(o, v),
            Self::Delay(o, v) => guard.set_output_delay_steps(o, v),
            Self::Gain(o, v) => guard.set_output_gain_steps(o, v),
            Self::Eq(b, v) => guard.set_eq_band_steps(b, v),
            Self::Bass(v) => guard.set_bass_level(v),
            Self::Treble(v) => guard.set_treble_level(v),
            Self::Loudness(v) => guard.set_loudness(v),
        }
    }
}

/// Fixed, compiled enhancement support, separate from live writability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnhancementCapabilities {
    /// Guard advertises the additive semantic operations; false on old ABI-5 guards.
    pub supported: bool,
    /// Current guard state permits writes; may change after this read.
    pub writable: bool,
}

/// Applied hardware model plus a separately labelled policy input. No chip readback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnhancementSnapshot {
    /// Whether this daemon successfully wrote the bass shelf this boot.
    pub bass_applied: bool,
    /// Last applied OEM bass request level, -32..32, not dB.
    pub bass_level: i32,
    /// Whether this daemon successfully wrote the treble shelf this boot.
    pub treble_applied: bool,
    /// Last applied OEM treble request level, -32..32, not dB.
    pub treble_level: i32,
    /// Whether a loudness mode write succeeded this boot.
    pub loudness_applied: bool,
    /// Last successfully applied mode: false means program default, not hard bypass.
    pub loudness_enabled: bool,
    /// True after a valid policy notification; not a hardware-applied claim.
    pub volume_index_known: bool,
    /// Latest valid policy input, or -1 when unknown.
    pub volume_index: i32,
    /// Basis of the last successful linked write, or -1 for program default/unapplied.
    pub loudness_applied_volume_index: i32,
    /// Shared successful DSP-write counter; notifications alone do not increment it.
    pub generation: u64,
    /// Shared last refusal/failure text; diagnostic only.
    pub last_error: String,
}

impl Default for EnhancementSnapshot {
    fn default() -> Self {
        Self {
            bass_applied: false, bass_level: 0,
            treble_applied: false, treble_level: 0,
            loudness_applied: false, loudness_enabled: false,
            volume_index_known: false, volume_index: -1,
            loudness_applied_volume_index: -1,
            generation: 0, last_error: String::new(),
        }
    }
}

/// The IDsp V2 control surface over the daemon's single audio-guard owner.
///
/// It holds the SAME `Arc<Mutex<AudioGuard>>` the HUD created and IAudioGuard
/// uses. There is deliberately no second `/dev/hu-audio-guard` opener and no
/// raw I2C or GPIO path: the kernel allows one opener, and closing it runs the
/// all-safe shutdown, so a second owner would be an audible fault, not a
/// tidiness question.
pub struct DspControls<T: Transport = KernelTransport> {
    /// Serialises whole control operations — state check, ioctl, publication —
    /// so that hardware order and model order cannot diverge. Outermost lock;
    /// see [`DspControls::apply`] for the order and the race it prevents.
    transaction: Mutex<()>,
    guard: Arc<Mutex<AudioGuard<T>>>,
    model: Arc<Mutex<ControlModel>>,
    persistence: Option<Arc<Mutex<persistence::Store>>>,
    publish_barrier: Mutex<Option<PublishBarrier>>,
}

impl<T: Transport> DspControls<T> {
    /// Construct the production persistence/restart seam.
    pub fn with_persistence(guard: Arc<Mutex<AudioGuard<T>>>, path: std::path::PathBuf) -> Self {
        let mut controls = Self::new(guard);
        let loaded = persistence::Store::load(path);
        if let Some(error) = &loaded.load_error {
            controls.model.lock().expect("new model mutex").persistence_error = error.clone();
        }
        let store = Arc::new(Mutex::new(loaded));
        let model = Arc::clone(&controls.model);
        let restore_store = Arc::clone(&store);
        let hook = Arc::new(move |guard: &mut AudioGuard<T>, restore: bool| {
            let mut model = model.lock().map_err(|_| Error::InvalidState("DSP model poisoned"))?;
            if !restore {
                model.program_reloaded();
                model.program_epoch = Some(guard.program_epoch());
                model.enhancements.volume_index_known = false;
                model.enhancements.volume_index = -1;
                return Ok(());
            }
            let store = restore_store.lock().map_err(|_| Error::InvalidState("DSP settings poisoned"))?;
            if let Some(error) = &store.load_error {
                model.persistence_error = error.clone();
                return Err(Error::NotReady(format!("DSP settings restore refused: {error}")));
            }
            for change in store.settings.changes() {
                // Android owns the current media index. Never resurrect a
                // previous-boot index or claim enabled loudness was applied.
                if matches!(change, Change::Loudness(Some(_))) { continue; }
                if let Err(error) = change.apply_to(guard) {
                    model.last_error = format!("DSP settings restore failed: {error}");
                    return Err(error);
                }
                model.record(change);
            }
            model.program_epoch = Some(guard.program_epoch().wrapping_add(1));
            if store.settings.loudness_enabled() {
                model.last_error = "saved loudness pending fresh volume policy".into();
            } else {
                model.last_error.clear();
            }
            Ok(())
        });
        match controls.guard.lock() {
            Ok(mut guard) => guard.install_program_settings(hook),
            Err(_) => {
                controls.model.lock().expect("new model mutex").persistence_error =
                    "audio guard mutex poisoned; DSP settings hook unavailable".into();
            }
        }
        controls.persistence = Some(store);
        controls
    }

    /// Wrap the daemon's existing guard owner. Performs no hardware operation.
    pub fn new(guard: Arc<Mutex<AudioGuard<T>>>) -> Self {
        Self {
            transaction: Mutex::new(()),
            guard,
            model: Arc::new(Mutex::new(ControlModel::new())),
            persistence: None,
            publish_barrier: Mutex::new(None),
        }
    }

    /// Install the test-only parked-writer hook. See [`PublishBarrier`].
    #[cfg(test)]
    fn set_publish_barrier(&self, hook: PublishBarrier) {
        *self
            .publish_barrier
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(hook);
    }

    /// Read the common output calibration without causing a RAM transaction.
    pub fn shared_output_gain_db(&self) -> Result<i32, ControlError> {
        self.guard()?.shared_output_gain_db().map_err(ControlError::from_guard)
    }

    /// Single-owner calibration. Kernel serializes mute/write/restore with ACC.
    pub fn set_shared_output_gain_db(&self, db: i32) -> Result<(), ControlError> {
        self.guard()?.set_shared_output_gain_db(db).map_err(ControlError::from_guard)
    }

    /// Report capabilities, consulting guard status for `writable` only.
    pub fn capabilities(&self) -> Capabilities {
        Capabilities::new(self.writable())
    }

    /// The last-applied model. Touches no hardware.
    pub fn snapshot(&self) -> Result<Snapshot, ControlError> {
        self.observe_program_epoch_nonblocking();
        Ok(self.model()?.snapshot())
    }

    /// Reconcile the model with a program reload without queueing behind an
    /// ioctl: a contended guard is skipped, and the next write or capability
    /// read (both take the guard) reconciles instead.
    fn observe_program_epoch_nonblocking(&self) {
        let guard = match self.guard.try_lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        if let Ok(mut model) = self.model.lock() {
            model.observe_program_epoch(guard.program_epoch());
        }
    }

    /// Inspect the actual kernel feature bit, never infer support from userspace version.
    pub fn enhancement_capabilities(&self) -> EnhancementCapabilities {
        let status = self.guard().ok().map(|mut guard| guard.status());
        let supported = status.as_ref().and_then(|status| status.raw)
            .is_some_and(|raw| raw.abi_version == super::guard::ABI_VERSION
                && raw.reserved[0] & super::guard::FEATURE_ENHANCEMENTS != 0);
        EnhancementCapabilities { supported, writable: supported && self.writable() }
    }

    /// Last applied enhancements and latest policy input, explicitly distinct facts.
    pub fn enhancement_snapshot(&self) -> Result<EnhancementSnapshot, ControlError> {
        self.observe_program_epoch_nonblocking();
        let model = self.model()?;
        let mut result = model.enhancements.clone();
        result.generation = model.generation;
        result.last_error = model.error_text();
        Ok(result)
    }

    /// Apply one fixed low shelf; kernel and transport both validate the level.
    pub fn set_bass_level(&self, level: i32) -> Result<(), ControlError> {
        if !(-32..=32).contains(&level) {
            return Err(self.refuse(ControlError::IllegalArgument("bass level must be -32..32".into())));
        }
        self.apply(Change::Bass(level), |guard| guard.set_bass_level(level))
    }

    /// Apply one fixed high shelf; kernel and transport both validate the level.
    pub fn set_treble_level(&self, level: i32) -> Result<(), ControlError> {
        if !(-32..=32).contains(&level) {
            return Err(self.refuse(ControlError::IllegalArgument("treble level must be -32..32".into())));
        }
        self.apply(Change::Treble(level), |guard| guard.set_treble_level(level))
    }

    /// Explicit user mode change. Enabling without an actual policy basis is refused.
    pub fn set_loudness_enabled(&self, enabled: bool) -> Result<(), ControlError> {
        let _transaction = self.transaction();
        let basis = {
            let model = self.model()?;
            if enabled && !model.enhancements.volume_index_known {
                drop(model);
                return Err(self.refuse(ControlError::IllegalState("volume index is unknown".into())));
            }
            enabled.then_some(model.enhancements.volume_index)
        };
        self.apply_locked(Change::Loudness(basis), |guard| guard.set_loudness(basis))
    }

    /// The sole volume-policy runner supplies normalized media indices. A valid
    /// notification is an input fact, not an applied value. Disabled notifications
    /// never touch the guard; an enabled failed write leaves applied fields intact.
    pub fn notify_volume_index(&self, index: i32) -> Result<(), ControlError> {
        let _transaction = self.transaction();
        if !(0..=63).contains(&index) {
            return Err(self.refuse(ControlError::IllegalArgument("volume index must be 0..63".into())));
        }
        let mut write = {
            let mut model = self.model()?;
            let state = &mut model.enhancements;
            state.volume_index_known = true;
            state.volume_index = index;
            state.loudness_enabled
        };
        if let Some(store) = &self.persistence {
            write |= store.lock().map_err(|_| ControlError::IllegalState("DSP settings poisoned".into()))?
                .settings.loudness_enabled();
        }
        if write {
            self.apply_locked(Change::Loudness(Some(index)), |guard| guard.set_loudness(Some(index)))
        } else {
            Ok(())
        }
    }

    /// Mute or unmute one logical output.
    pub fn set_output_mute(
        &self,
        output: DspOutput,
        muted: bool,
    ) -> Result<(), ControlError> {
        self.apply(Change::Mute(output, muted), |guard| {
            guard.set_output_mute(output, muted)
        })
    }

    /// Invert or restore one logical output's polarity.
    pub fn set_output_phase_inverted(
        &self,
        output: DspOutput,
        inverted: bool,
    ) -> Result<(), ControlError> {
        self.apply(Change::Phase(output, inverted), |guard| {
            guard.set_output_phase_inverted(output, inverted)
        })
    }

    /// Set one logical output's delay, 0..=145 uAPI REQUEST STEPS.
    ///
    /// The physical duration of a step is not proven on device 3; see
    /// [`DELAY_STEP_MICROS`], which reports 0 = unknown and carries the
    /// one-sample-at-48-kHz reading as a hypothesis only.
    ///
    /// The subwoofer is refused here, before the guard is even locked: it has
    /// no delay cell, and `EX_UNSUPPORTED_OPERATION` is a different answer
    /// from "that number is out of range".
    pub fn set_output_delay_steps(
        &self,
        output: DspOutput,
        steps: i32,
    ) -> Result<(), ControlError> {
        if !output.has_delay_cell() {
            return Err(self.refuse(ControlError::Unsupported(format!(
                "{} has no delay cell in the shipped program",
                output.name()
            ))));
        }
        if !(0..=MAX_OUTPUT_DELAY_STEPS).contains(&steps) {
            return Err(self.refuse(ControlError::IllegalArgument(format!(
                "delay must be 0..{MAX_OUTPUT_DELAY_STEPS} steps, got {steps}"
            ))));
        }
        self.apply(Change::Delay(output, steps), |guard| {
            guard.set_output_delay_steps(output, steps)
        })
    }

    /// Set one logical output's gain, -200..=200 steps of 0.1 dB.
    pub fn set_output_gain_steps(
        &self,
        output: DspOutput,
        steps: i32,
    ) -> Result<(), ControlError> {
        if !(MIN_OUTPUT_GAIN_STEPS..=MAX_OUTPUT_GAIN_STEPS).contains(&steps) {
            return Err(self.refuse(ControlError::IllegalArgument(format!(
                "gain must be {MIN_OUTPUT_GAIN_STEPS}..{MAX_OUTPUT_GAIN_STEPS} steps, got {steps}"
            ))));
        }
        self.apply(Change::Gain(output, steps), |guard| {
            guard.set_output_gain_steps(output, steps)
        })
    }

    /// Set one SM4250 EQ band's uAPI request step, -20..=20. Not decibels.
    pub fn set_eq_band_steps(&self, band: i32, steps: i32) -> Result<(), ControlError> {
        if !(0..EQ_BAND_COUNT).contains(&band) {
            return Err(self.refuse(ControlError::IllegalArgument(format!(
                "EQ band must be 0..{}, got {band}",
                EQ_BAND_COUNT - 1
            ))));
        }
        if !(MIN_EQ_STEPS..=MAX_EQ_STEPS).contains(&steps) {
            return Err(self.refuse(ControlError::IllegalArgument(format!(
                "EQ steps must be {MIN_EQ_STEPS}..{MAX_EQ_STEPS}, got {steps}"
            ))));
        }
        self.apply(Change::Eq(band, steps), |guard| {
            guard.set_eq_band_steps(band, steps)
        })
    }

    /// Whether a write attempted now would reach the chip.
    fn writable(&self) -> bool {
        self.state().is_ok()
    }

    /// The one state gate. Returns the refusal a write would produce, if any.
    ///
    /// It reads guard status rather than trusting a cached value, because
    /// `fault_latched` and `transition_pending` are set by the kernel behind
    /// this daemon's back — a rail fault or an FM route change moves them
    /// without any Binder call.
    fn state(&self) -> Result<(), ControlError> {
        let mut guard = self.guard()?;
        let status = guard.status();
        let epoch = guard.program_epoch();
        if let Ok(mut model) = self.model.lock() {
            model.observe_program_epoch(epoch);
        }
        if !status.available {
            return Err(ControlError::IllegalState(status.unavailable_reason));
        }
        let Some(raw) = status.raw else {
            return Err(ControlError::IllegalState(
                "audio guard reported no status".to_owned(),
            ));
        };
        if raw.fault_latched != 0 {
            return Err(ControlError::IllegalState(
                "audio guard has latched a fault; controls are refused until it is cleared"
                    .to_owned(),
            ));
        }
        if raw.transition_pending != 0 {
            return Err(ControlError::IllegalState(
                "an audio source transition is pending; controls are refused until it settles"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// One control operation, start to finish, as ONE ordered transaction.
    ///
    /// # The race this serialisation exists to prevent
    ///
    /// The guard lock alone orders the ioctls; it does NOT order the model
    /// publications that follow them, because the lock is released before the
    /// model is touched. Two concurrent Binder setters A then B could reach
    /// the hardware in order A, B and publish in order B, A — leaving the
    /// model claiming A is the latest value while the chip holds B. Every
    /// consequence of that is user-visible: the wrong `*Applied` value, a
    /// generation whose order contradicts the hardware, and an app that saves
    /// A and later replays it over B.
    ///
    /// So the state check, the single ioctl, and the publication are all held
    /// under one DSP-local transaction mutex. Hardware order and publication
    /// order are then the same order by construction, not by timing.
    ///
    /// # Lock order
    ///
    /// `transaction` → `guard` → `model` → optional settings store.
    ///
    /// The transaction mutex is DSP-local and is the outermost of the three.
    /// The guard is never held while acquiring the transaction — nothing else
    /// in this daemon knows the transaction exists — so there is no cycle with
    /// the FM/audio adapter that shares the guard. Keeping the guard through
    /// publication/save also prevents a recovery from replaying stale intent
    /// after the ioctl but before its successful change is persisted. Readers (`snapshot`,
    /// `capabilities`) deliberately do NOT take the transaction: a reader must
    /// not queue behind an ioctl, and seeing the model either side of a write
    /// is exactly what a caller polling for a change wants.
    fn apply<F>(&self, change: Change, operation: F) -> Result<(), ControlError>
    where
        F: FnOnce(&mut AudioGuard<T>) -> Result<(), Error>,
    {
        let _transaction = self.transaction();
        self.apply_locked(change, operation)
    }

    // Caller holds transaction; used by volume notifications and mode changes
    // so selecting a basis and applying it cannot race each other.
    fn apply_locked<F>(&self, change: Change, operation: F) -> Result<(), ControlError>
    where
        F: FnOnce(&mut AudioGuard<T>) -> Result<(), Error>,
    {
        if let Err(error) = self.state() {
            return Err(self.refuse(error));
        }
        let mut guard = self.guard()?;
        let epoch = guard.program_epoch();
        let result = operation(&mut guard);
        match result {
            Ok(()) => {
                self.publish_barrier();
                let mut model = self.model()?;
                model.observe_program_epoch(epoch);
                model.record(change);
                if let Some(store) = &self.persistence {
                    let saved = store.lock()
                        .map_err(|_| "DSP settings mutex poisoned".to_owned())
                        .and_then(|mut store| store.record_and_save(change));
                    match saved {
                        Ok(()) => model.persistence_error.clear(),
                        Err(error) => {
                            model.persistence_error = error.clone();
                            // Hardware succeeded: retain applied values and
                            // generation, while exposing failed durability.
                            return Err(ControlError::IllegalState(error));
                        }
                    }
                }
                Ok(())
            }
            // A refusal cannot reorder a success: it happens under the same
            // transaction, so it is ordered against every other operation too.
            Err(error) => Err(self.refuse(ControlError::from_guard(error))),
        }
    }

    /// Take the transaction mutex for one complete control operation.
    ///
    /// A poisoned transaction is recovered rather than propagated: the mutex
    /// guards ordering, not data, so there is no invariant a panicking holder
    /// could have broken inside it — and refusing to recover would disable
    /// every DSP control for the life of the process. (This crate builds with
    /// `panic = "abort"` in both profiles, so it should not arise at all.)
    fn transaction(&self) -> std::sync::MutexGuard<'_, ()> {
        self.transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Test-only seam, always compiled: run the parked-writer hook, if one is
    /// installed, in the window between a successful ioctl and its
    /// publication. Production installs none, so this is one uncontended lock
    /// and a `None` check per successful write — and, more to the point, the
    /// code the regression exercises is the code that ships.
    fn publish_barrier(&self) {
        // Cloned out and the lock dropped before the hook runs: a hook that
        // blocks must not also hold this mutex.
        let hook = self
            .publish_barrier
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Record a refusal without touching any applied value or the generation.
    fn refuse(&self, error: ControlError) -> ControlError {
        if let Ok(mut model) = self.model.lock() {
            model.last_error = error.message().to_owned();
        }
        error
    }

    fn guard(&self) -> Result<std::sync::MutexGuard<'_, AudioGuard<T>>, ControlError> {
        self.guard
            .lock()
            .map_err(|_| ControlError::IllegalState("audio guard mutex poisoned".to_owned()))
    }

    fn model(&self) -> Result<std::sync::MutexGuard<'_, ControlModel>, ControlError> {
        self.model
            .lock()
            .map_err(|_| ControlError::IllegalState("DSP control model mutex poisoned".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::guard::{DspUpdate, FmAttenuation, FmMixBase, FmMixTransition, RawStatus, SourceTransition, ABI_VERSION};
    use std::io;
    use std::sync::{Arc, Mutex};

    /// Records every `DSP_UPDATE` and can fail one with a chosen errno.
    #[derive(Default)]
    struct FakeTransport {
        status: RawStatus,
        updates: Vec<DspUpdate>,
        fail_with: Option<i32>,
        events: Vec<String>,
        fail_update_at: Option<usize>,
        fail_amp: bool,
    }

    impl FakeTransport {
        fn ready() -> Self {
            Self {
                status: RawStatus {
                    abi_version: ABI_VERSION,
                    dsp_powered: 1,
                    program_loaded: 1,
                    ..RawStatus::default()
                },
                ..Self::default()
            }
        }
    }

    impl Transport for Arc<Mutex<FakeTransport>> {
        fn get_status(&mut self) -> io::Result<RawStatus> {
            Ok(self.lock().expect("fake lock").status)
        }
        fn fault_safe(&mut self) -> io::Result<()> {
            let mut state = self.lock().unwrap();
            state.events.push("safe".into());
            state.status.amplifier_enabled = 0;
            state.status.dac_enabled = 0;
            Ok(())
        }
        fn dsp_on(&mut self) -> io::Result<()> {
            self.lock().unwrap().events.push("program".into());
            Ok(())
        }
        fn dac_power(&mut self, enabled: bool) -> io::Result<()> {
            let mut state = self.lock().unwrap();
            state.events.push(format!("dac:{enabled}"));
            state.status.dac_enabled = u32::from(enabled);
            Ok(())
        }
        fn amplifier_power(&mut self, enabled: bool) -> io::Result<()> {
            let mut state = self.lock().unwrap();
            state.events.push(format!("amp:{enabled}"));
            // A reply can be lost after the rail changed.
            state.status.amplifier_enabled = u32::from(enabled);
            if state.fail_amp { return Err(io::Error::from_raw_os_error(5)); }
            Ok(())
        }
        fn source_transition(&mut self, _request: SourceTransition) -> io::Result<()> {
            self.lock().unwrap().events.push("route".into());
            Ok(())
        }
        fn dsp_update(&mut self, request: DspUpdate) -> io::Result<()> {
            let mut shared = self.lock().expect("fake lock");
            shared.updates.push(request);
            shared.events.push(format!("update:{}:{}:{}", request.operation, request.channel, request.value));
            if shared.fail_update_at == Some(shared.updates.len()) {
                return Err(io::Error::from_raw_os_error(5));
            }
            match shared.fail_with {
                Some(errno) => Err(io::Error::from_raw_os_error(errno)),
                None => Ok(()),
            }
        }
        fn fm_power(&mut self, _enabled: bool) -> io::Result<()> {
            Ok(())
        }
        fn fm_mix_transition(&mut self, _request: FmMixTransition) -> io::Result<()> {
            Ok(())
        }
        fn fm_attenuation(&mut self, _request: FmAttenuation) -> io::Result<()> {
            Ok(())
        }
        fn fm_mix_base(&mut self, _request: FmMixBase) -> io::Result<()> {
            Ok(())
        }
    }

    type Fixture = (
        DspControls<Arc<Mutex<FakeTransport>>>,
        Arc<Mutex<FakeTransport>>,
    );

    fn fixture() -> Fixture {
        let shared = Arc::new(Mutex::new(FakeTransport::ready()));
        let guard = Arc::new(Mutex::new(AudioGuard::with_transport(Arc::clone(&shared))));
        (DspControls::new(guard), shared)
    }

    fn updates(shared: &Arc<Mutex<FakeTransport>>) -> Vec<DspUpdate> {
        shared.lock().expect("fake lock").updates.clone()
    }

    fn enhancement_fixture() -> Fixture {
        let result = fixture();
        result.1.lock().unwrap().status.reserved[0] = super::super::guard::FEATURE_ENHANCEMENTS;
        result
    }

    #[test]
    fn persisted_settings_survive_real_guard_restart_before_amplifier_enable() {
        let root = std::env::var_os("BENGAL_DESK_TMP").expect("exclusive scratch required");
        let directory = std::path::PathBuf::from(root).join("restart-regression");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("dsp-settings-v1");
        let _ = std::fs::remove_file(&path);
        let shared = Arc::new(Mutex::new(FakeTransport::ready()));
        let guard = Arc::new(Mutex::new(AudioGuard::with_transport(shared.clone())));
        let controls = DspControls::with_persistence(guard, path.clone());
        controls.set_output_gain_steps(DspOutput::FrontLeft, -100).unwrap();
        controls.set_output_mute(DspOutput::Subwoofer, true).unwrap();
        controls.set_eq_band_steps(1, -16).unwrap();
        drop(controls);
        let restarted = Arc::new(Mutex::new(FakeTransport::ready()));
        let guard = Arc::new(Mutex::new(AudioGuard::with_transport(restarted.clone())));
        let controls = DspControls::with_persistence(guard.clone(), path);
        guard.lock().unwrap().ensure_ready().unwrap();
        let snapshot = controls.snapshot().unwrap();
        assert!(snapshot.outputs[0].gain_applied, "restart lost saved output gain");
        assert_eq!(snapshot.outputs[0].gain_steps, -100);
        assert!(snapshot.outputs[4].mute_applied && snapshot.outputs[4].muted);
        assert!(snapshot.eq[1].applied);
        assert_eq!(snapshot.eq[1].steps, -16);
        assert!(!snapshot.outputs[1].gain_applied, "never adopt unknown defaults");
        assert_eq!(snapshot.generation, 3);
        assert_eq!(updates(&restarted).len(), 3);
        let events = restarted.lock().unwrap().events.clone();
        assert_eq!(&events[..2], ["safe", "program"]);
        assert!(events[2..5].iter().all(|e| e.starts_with("update:")));
        assert_eq!(&events[5..], ["route", "route", "dac:true", "amp:true"]);
    }

    pub(super) fn settings_path(name: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::var_os("BENGAL_DESK_TMP").expect("exclusive scratch required");
        let path = std::path::PathBuf::from(root).join(format!("settings-{name}-{}-{}",
            std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::create_dir(&path).unwrap();
        path.join("settings")
    }

    fn persistent_fixture(path: std::path::PathBuf) -> Fixture {
        let shared = Arc::new(Mutex::new(FakeTransport::ready()));
        shared.lock().unwrap().status.reserved[0] = super::super::guard::FEATURE_ENHANCEMENTS;
        let guard = Arc::new(Mutex::new(AudioGuard::with_transport(shared.clone())));
        (DspControls::with_persistence(guard, path), shared)
    }

    #[test]
    fn persistence_recovery_restores_all_supported_settings_and_uses_fresh_loudness_basis() {
        let path = settings_path("all");
        let (controls, _) = persistent_fixture(path.clone());
        for output in DspOutput::ALL {
            controls.set_output_gain_steps(output, -100).unwrap();
            controls.set_output_mute(output, output == DspOutput::Subwoofer).unwrap();
            controls.set_output_phase_inverted(output, true).unwrap();
            if output.has_delay_cell() { controls.set_output_delay_steps(output, 145).unwrap(); }
        }
        for band in 0..4 { controls.set_eq_band_steps(band, -16).unwrap(); }
        controls.set_bass_level(-32).unwrap();
        controls.set_treble_level(4).unwrap();
        controls.notify_volume_index(25).unwrap();
        controls.set_loudness_enabled(true).unwrap();
        let before = controls.snapshot().unwrap();
        drop(controls);
        let (controls, shared) = persistent_fixture(path.clone());
        controls.guard.lock().unwrap().ensure_ready().unwrap();
        let after = controls.snapshot().unwrap();
        assert_eq!(after.outputs, before.outputs);
        assert_eq!(after.eq, before.eq);
        let enhancement = controls.enhancement_snapshot().unwrap();
        assert!(enhancement.bass_applied && enhancement.treble_applied);
        assert!(!enhancement.loudness_applied && !enhancement.volume_index_known);
        assert!(enhancement.last_error.contains("pending fresh volume"));
        assert!(!updates(&shared).iter().any(|r| r.operation == 10));
        let saved = std::fs::read(&path).unwrap();
        controls.notify_volume_index(7).unwrap();
        assert_eq!(updates(&shared).last().unwrap().value, 7);
        assert!(controls.enhancement_snapshot().unwrap().loudness_enabled);
        assert_eq!(std::fs::read(&path).unwrap(), saved, "media notifications do not persist policy volume");
        let generation = controls.snapshot().unwrap().generation;
        controls.guard.lock().unwrap().recover_after_acc_fault().unwrap();
        assert_eq!(controls.snapshot().unwrap().generation, generation + 25);
        assert_eq!(controls.snapshot().unwrap().outputs, before.outputs);
        assert!(!controls.enhancement_snapshot().unwrap().volume_index_known);
        controls.notify_volume_index(9).unwrap();
        assert_eq!(updates(&shared).last().unwrap().value, 9);
        controls.set_loudness_enabled(false).unwrap();
        drop(controls);
        let (controls, shared) = persistent_fixture(path);
        controls.guard.lock().unwrap().ensure_ready().unwrap();
        assert_eq!(updates(&shared).last().unwrap().value, -1);
        assert!(controls.enhancement_snapshot().unwrap().loudness_applied);
        let count = updates(&shared).len();
        controls.notify_volume_index(23).unwrap();
        assert_eq!(updates(&shared).len(), count);
    }

    #[test]
    fn persistence_save_failure_reports_applied_state_and_retries_without_losing_prior_intent() {
        let path = settings_path("save-failure");
        let (controls, shared) = persistent_fixture(path.clone());
        controls.set_output_gain_steps(DspOutput::FrontLeft, -100).unwrap();
        let saved = std::fs::read(&path).unwrap();
        // Exhaust exactly the production store's bounded create-new slots.
        for slot in 0..8 {
            std::fs::write(path.parent().unwrap().join(format!(".dsp-settings-{}-{slot}.tmp", std::process::id())), b"occupied").unwrap();
        }
        assert!(controls.set_eq_band_steps(1, -16).is_err());
        let snapshot = controls.snapshot().unwrap();
        assert!(snapshot.eq[1].applied);
        assert_eq!(snapshot.generation, 2);
        assert!(snapshot.last_error.contains("applied but settings save failed"));
        assert_eq!(std::fs::read(&path).unwrap(), saved);
        shared.lock().unwrap().fail_with = Some(5);
        assert!(controls.set_output_gain_steps(DspOutput::FrontLeft, 50).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), saved);
        shared.lock().unwrap().fail_with = None;
        std::fs::remove_file(path.parent().unwrap().join(format!(".dsp-settings-{}-0.tmp", std::process::id()))).unwrap();
        controls.set_eq_band_steps(1, -16).unwrap();
        assert!(controls.snapshot().unwrap().last_error.is_empty());
        drop(controls);
        let (controls, _) = persistent_fixture(path);
        controls.guard.lock().unwrap().ensure_ready().unwrap();
        assert_eq!(controls.snapshot().unwrap().outputs[0].gain_steps, -100);
        assert_eq!(controls.snapshot().unwrap().eq[1].steps, -16);
    }

    #[test]
    fn persistence_restore_failures_never_enable_outputs_or_claim_partial_state() {
        let path = settings_path("restore-failure");
        let (controls, _) = persistent_fixture(path.clone());
        controls.set_output_mute(DspOutput::Subwoofer, true).unwrap();
        controls.set_output_gain_steps(DspOutput::FrontLeft, -100).unwrap();
        controls.set_eq_band_steps(0, -20).unwrap();
        drop(controls);
        let saved = std::fs::read(&path).unwrap();
        for failure in 1..=4 {
            let (controls, shared) = persistent_fixture(path.clone());
            shared.lock().unwrap().fail_update_at = Some(failure);
            shared.lock().unwrap().fail_amp = failure == 4;
            assert!(controls.guard.lock().unwrap().ensure_ready().is_err());
            let state = shared.lock().unwrap();
            assert_eq!(state.status.amplifier_enabled, 0);
            assert_eq!(state.status.dac_enabled, 0);
            assert_eq!(state.events.last().unwrap(), "safe");
            if failure < 4 { assert!(!state.events.iter().any(|e| e == "amp:true")); }
            drop(state);
            assert!(!controls.capabilities().writable);
            assert!(controls.snapshot().unwrap().outputs.iter().all(|o| !o.gain_applied && !o.mute_applied));
            assert!(controls.snapshot().unwrap().eq.iter().all(|e| !e.applied));
            assert_eq!(std::fs::read(&path).unwrap(), saved);
        }
    }

    #[test]
    fn persistence_corrupt_file_refuses_activation_without_partial_replay() {
        let path = settings_path("corrupt");
        let (controls, _) = persistent_fixture(path.clone());
        controls.set_output_gain_steps(DspOutput::FrontLeft, -100).unwrap();
        drop(controls);
        let mut corrupt = std::fs::read(&path).unwrap();
        corrupt[0] = b'X';
        std::fs::write(&path, &corrupt).unwrap();
        let (controls, shared) = persistent_fixture(path.clone());
        assert!(controls.guard.lock().unwrap().ensure_ready().is_err());
        assert!(updates(&shared).is_empty());
        assert_eq!(shared.lock().unwrap().events, ["safe", "program", "safe"]);
        assert!(controls.snapshot().unwrap().last_error.contains("invalid DSP settings"));
        assert_eq!(std::fs::read(&path).unwrap(), corrupt);
        assert!(controls.set_output_gain_steps(DspOutput::FrontLeft, 0).is_err());
    }

    #[test]
    fn enhancements_old_guard_has_no_capability_and_no_update() {
        let (controls, shared) = fixture();
        assert!(!controls.enhancement_capabilities().supported);
        assert!(controls.set_bass_level(0).is_err());
        assert!(controls.set_treble_level(0).is_err());
        assert!(controls.set_loudness_enabled(false).is_err());
        assert!(updates(&shared).is_empty());
        assert_eq!(controls.enhancement_snapshot().unwrap().generation, 0);
    }

    #[test]
    fn enhancements_shelves_have_exact_bounded_semantic_requests() {
        let (controls, shared) = enhancement_fixture();
        assert!(controls.enhancement_capabilities().writable);
        for level in [-32, 0, 32] {
            controls.set_bass_level(level).unwrap();
            controls.set_treble_level(level).unwrap();
        }
        for level in [i32::MIN, -33, 33, i32::MAX] {
            assert!(controls.set_bass_level(level).is_err());
            assert!(controls.set_treble_level(level).is_err());
        }
        let got = updates(&shared);
        assert_eq!(got.len(), 6);
        for (request, (op, value)) in got.iter().zip([(8,-32),(9,-32),(8,0),(9,0),(8,32),(9,32)]) {
            assert_eq!((request.operation, request.value), (op, value));
            assert_eq!((request.channel, request.band, request.reserved), (0, 0, [0; 4]));
        }
        let state = controls.enhancement_snapshot().unwrap();
        assert!(state.bass_applied && state.treble_applied);
        assert!(!state.loudness_applied);
        assert_eq!(state.generation, 6);
        assert!(controls.snapshot().unwrap().outputs.iter().all(|row| !row.gain_applied));
    }

    #[test]
    fn enhancements_volume_notifications_are_inert_until_explicit_enable() {
        let (controls, shared) = enhancement_fixture();
        assert!(controls.set_loudness_enabled(true).is_err());
        assert!(!controls.enhancement_snapshot().unwrap().volume_index_known);
        for index in 0..=63 { controls.notify_volume_index(index).unwrap(); }
        assert!(updates(&shared).is_empty());
        assert_eq!(controls.enhancement_snapshot().unwrap().generation, 0);
        controls.set_loudness_enabled(true).unwrap();
        controls.notify_volume_index(14).unwrap();
        controls.set_loudness_enabled(false).unwrap();
        controls.notify_volume_index(0).unwrap();
        let got = updates(&shared);
        assert_eq!(got.iter().map(|r| (r.operation, r.value)).collect::<Vec<_>>(),
                   vec![(10,63),(10,14),(10,-1)]);
        let state = controls.enhancement_snapshot().unwrap();
        assert!(state.loudness_applied && !state.loudness_enabled && state.volume_index_known);
        assert_eq!(state.volume_index, 0);
        assert_eq!(state.loudness_applied_volume_index, -1);
        assert_eq!(state.generation, 3);
    }

    #[test]
    fn enhancements_bad_volume_leaves_basis_and_chip_unchanged() {
        let (controls, shared) = enhancement_fixture();
        controls.notify_volume_index(20).unwrap();
        controls.set_loudness_enabled(true).unwrap();
        for index in [-1,64,i32::MIN,i32::MAX] { assert!(controls.notify_volume_index(index).is_err()); }
        let state = controls.enhancement_snapshot().unwrap();
        assert_eq!((state.volume_index,state.loudness_applied_volume_index,state.generation), (20,20,1));
        assert_eq!(updates(&shared).len(), 1);
    }

    #[test]
    fn enhancements_failed_write_keeps_applied_fields_but_labels_new_input() {
        let (controls, shared) = enhancement_fixture();
        controls.notify_volume_index(20).unwrap();
        controls.set_loudness_enabled(true).unwrap();
        shared.lock().unwrap().fail_with = Some(5);
        assert!(controls.notify_volume_index(30).is_err());
        assert!(controls.set_loudness_enabled(false).is_err());
        assert!(controls.set_bass_level(32).is_err());
        assert!(controls.set_treble_level(-32).is_err());
        let state = controls.enhancement_snapshot().unwrap();
        assert_eq!((state.volume_index,state.loudness_applied_volume_index,state.generation), (30,20,1));
        assert!(state.loudness_enabled && !state.bass_applied && !state.treble_applied);
        assert!(!state.last_error.is_empty());
        shared.lock().unwrap().fail_with = None;
        controls.notify_volume_index(30).unwrap();
        let state = controls.enhancement_snapshot().unwrap();
        assert_eq!((state.loudness_applied_volume_index,state.generation), (30,2));
        assert!(state.last_error.is_empty());
    }

    #[test]
    fn enhancements_route_fault_and_unloaded_state_refuse_writes() {
        let (controls, shared) = enhancement_fixture();
        controls.notify_volume_index(10).unwrap();
        for case in 0..3 {
            {
                let mut fake = shared.lock().unwrap();
                fake.status.transition_pending = u32::from(case == 0);
                fake.status.fault_latched = u32::from(case == 1);
                fake.status.program_loaded = u32::from(case != 2);
            }
            assert!(!controls.enhancement_capabilities().writable);
            assert!(controls.set_bass_level(0).is_err());
            assert!(controls.set_treble_level(0).is_err());
            assert!(controls.set_loudness_enabled(true).is_err());
        }
        assert!(updates(&shared).is_empty());
    }

    #[test]
    fn enhancements_restart_does_not_restore_mode_or_write_on_notification() {
        let (controls, shared) = enhancement_fixture();
        controls.notify_volume_index(40).unwrap();
        controls.set_loudness_enabled(true).unwrap();
        let fresh = DspControls::new(Arc::new(Mutex::new(AudioGuard::with_transport(Arc::clone(&shared)))));
        fresh.notify_volume_index(40).unwrap();
        let state = fresh.enhancement_snapshot().unwrap();
        assert!(!state.loudness_applied && !state.loudness_enabled);
        assert_eq!(state.generation, 0);
        assert_eq!(updates(&shared).len(), 1);
    }

    // ---- ranges: every boundary, both sides -------------------------------

    #[test]
    fn delay_accepts_both_endpoints_and_refuses_just_outside() {
        let (controls, shared) = fixture();
        assert!(controls
            .set_output_delay_steps(DspOutput::FrontLeft, 0)
            .is_ok());
        assert!(controls
            .set_output_delay_steps(DspOutput::FrontLeft, MAX_OUTPUT_DELAY_STEPS)
            .is_ok());
        assert_eq!(updates(&shared).len(), 2);

        for bad in [-1, MAX_OUTPUT_DELAY_STEPS + 1] {
            let error = controls
                .set_output_delay_steps(DspOutput::FrontLeft, bad)
                .expect_err("out of range must be refused");
            assert!(matches!(error, ControlError::IllegalArgument(_)), "{error:?}");
        }
        // The refusals never became ioctls.
        assert_eq!(updates(&shared).len(), 2);
    }

    #[test]
    fn gain_accepts_both_endpoints_and_refuses_just_outside() {
        let (controls, shared) = fixture();
        assert!(controls
            .set_output_gain_steps(DspOutput::RearRight, MIN_OUTPUT_GAIN_STEPS)
            .is_ok());
        assert!(controls
            .set_output_gain_steps(DspOutput::RearRight, MAX_OUTPUT_GAIN_STEPS)
            .is_ok());
        for bad in [MIN_OUTPUT_GAIN_STEPS - 1, MAX_OUTPUT_GAIN_STEPS + 1] {
            assert!(matches!(
                controls.set_output_gain_steps(DspOutput::RearRight, bad),
                Err(ControlError::IllegalArgument(_))
            ));
        }
        assert_eq!(updates(&shared).len(), 2);
    }

    #[test]
    fn eq_accepts_both_endpoints_and_refuses_just_outside() {
        let (controls, shared) = fixture();
        assert!(controls.set_eq_band_steps(0, MIN_EQ_STEPS).is_ok());
        assert!(controls
            .set_eq_band_steps(EQ_BAND_COUNT - 1, MAX_EQ_STEPS)
            .is_ok());
        for (band, steps) in [
            (0, MIN_EQ_STEPS - 1),
            (0, MAX_EQ_STEPS + 1),
            (-1, 0),
            (EQ_BAND_COUNT, 0),
        ] {
            assert!(
                matches!(
                    controls.set_eq_band_steps(band, steps),
                    Err(ControlError::IllegalArgument(_))
                ),
                "band {band} steps {steps} was not refused"
            );
        }
        assert_eq!(updates(&shared).len(), 2);
    }

    #[test]
    fn the_eq_request_carries_channel_none_and_the_band() {
        let (controls, shared) = fixture();
        controls.set_eq_band_steps(2, -7).expect("EQ write");
        let update = updates(&shared).pop().expect("one update");
        assert_eq!(update.operation, 5, "HU_AUDIO_DSP_SM4250_EQ");
        assert_eq!(update.channel, 0, "the kernel rejects any other channel");
        assert_eq!(update.band, 2);
        assert_eq!(update.value, -7);
        assert_eq!(update.reserved, [0; 4]);
    }

    #[test]
    fn each_output_maps_to_its_kernel_channel_number() {
        let (controls, shared) = fixture();
        for output in DspOutput::ALL {
            controls.set_output_mute(output, true).expect("mute");
        }
        let channels: Vec<u32> = updates(&shared).iter().map(|update| update.channel).collect();
        assert_eq!(channels, vec![1, 2, 3, 4, 5]);
    }

    // ---- the subwoofer has no delay cell ----------------------------------

    #[test]
    fn subwoofer_delay_is_refused_before_the_transport_is_touched() {
        let (controls, shared) = fixture();
        let error = controls
            .set_output_delay_steps(DspOutput::Subwoofer, 10)
            .expect_err("the subwoofer has no delay cell");
        assert!(matches!(error, ControlError::Unsupported(_)), "{error:?}");
        assert!(
            updates(&shared).is_empty(),
            "the guard must not see an ioctl the program cannot serve"
        );
    }

    #[test]
    fn the_subwoofer_row_reports_delay_unsupported_forever() {
        let (controls, _shared) = fixture();
        controls
            .set_output_mute(DspOutput::Subwoofer, true)
            .expect("mute is supported for the subwoofer");
        let snapshot = controls.snapshot().expect("snapshot");
        let row = snapshot
            .outputs
            .iter()
            .find(|row| row.output == DspOutput::Subwoofer)
            .expect("subwoofer row");
        assert!(row.mute_applied);
        // The one control it does not have stays visibly unknown, forever.
        assert!(!row.delay_applied);
        assert_eq!(row.delay_steps, DELAY_UNSUPPORTED);
    }

    // ---- state gating -----------------------------------------------------

    #[test]
    fn a_fault_latched_guard_refuses_every_write_without_an_ioctl() {
        let (controls, shared) = fixture();
        shared.lock().expect("fake lock").status.fault_latched = 1;
        assert!(matches!(
            controls.set_output_mute(DspOutput::FrontLeft, true),
            Err(ControlError::IllegalState(_))
        ));
        assert!(updates(&shared).is_empty());
        assert!(!controls.capabilities().writable);
    }

    #[test]
    fn a_pending_transition_refuses_every_write_without_an_ioctl() {
        let (controls, shared) = fixture();
        shared.lock().expect("fake lock").status.transition_pending = 1;
        assert!(matches!(
            controls.set_output_gain_steps(DspOutput::FrontRight, 10),
            Err(ControlError::IllegalState(_))
        ));
        assert!(updates(&shared).is_empty());
    }

    #[test]
    fn an_unloaded_program_refuses_every_write_without_an_ioctl() {
        let (controls, shared) = fixture();
        shared.lock().expect("fake lock").status.program_loaded = 0;
        assert!(matches!(
            controls.set_eq_band_steps(1, 3),
            Err(ControlError::IllegalState(_))
        ));
        assert!(updates(&shared).is_empty());
    }

    #[test]
    fn an_absent_guard_is_inert_and_not_writable() {
        let controls: DspControls<Arc<Mutex<FakeTransport>>> =
            DspControls::new(Arc::new(Mutex::new(AudioGuard::unavailable("absent"))));
        assert!(!controls.capabilities().writable);
        assert!(matches!(
            controls.set_output_mute(DspOutput::FrontLeft, true),
            Err(ControlError::IllegalState(_))
        ));
        assert_eq!(controls.snapshot().expect("snapshot").generation, 0);
    }

    // ---- the model moves only on success ----------------------------------

    #[test]
    fn a_fresh_model_has_applied_false_everywhere_and_generation_zero() {
        let (controls, _shared) = fixture();
        let snapshot = controls.snapshot().expect("snapshot");
        assert_eq!(snapshot.generation, 0);
        assert!(snapshot.last_error.is_empty());
        assert!(snapshot.outputs.iter().all(|row| !row.mute_applied
            && !row.phase_applied
            && !row.delay_applied
            && !row.gain_applied));
        assert!(snapshot.eq.iter().all(|row| !row.applied));
        assert_eq!(snapshot.outputs.len(), 5);
        assert_eq!(snapshot.eq.len(), 4);
    }

    #[test]
    fn a_successful_write_sets_applied_and_advances_the_generation_by_one() {
        let (controls, _shared) = fixture();
        controls
            .set_output_gain_steps(DspOutput::RearLeft, -35)
            .expect("gain write");
        let snapshot = controls.snapshot().expect("snapshot");
        assert_eq!(snapshot.generation, 1);
        let row = snapshot
            .outputs
            .iter()
            .find(|row| row.output == DspOutput::RearLeft)
            .expect("rear-left row");
        assert!(row.gain_applied);
        assert_eq!(row.gain_steps, -35);
        // The OTHER controls of the SAME output stay unknown. This is the
        // whole point of per-control flags: a caller replaying saved settings
        // must not treat this row's default mute/phase/delay as known.
        assert!(!row.mute_applied);
        assert!(!row.phase_applied);
        assert!(!row.delay_applied);
        // And no other output moved at all.
        assert!(snapshot
            .outputs
            .iter()
            .filter(|row| row.output != DspOutput::RearLeft)
            .all(|row| !row.gain_applied));
    }

    #[test]
    fn each_control_carries_its_own_validity_flag() {
        let (controls, _shared) = fixture();
        controls
            .set_output_mute(DspOutput::FrontLeft, true)
            .expect("mute");
        controls
            .set_output_phase_inverted(DspOutput::FrontLeft, true)
            .expect("phase");

        let snapshot = controls.snapshot().expect("snapshot");
        assert_eq!(snapshot.generation, 2, "one per successful write");
        let row = snapshot
            .outputs
            .iter()
            .find(|row| row.output == DspOutput::FrontLeft)
            .expect("front-left row");
        assert!(row.mute_applied && row.muted);
        assert!(row.phase_applied && row.phase_inverted);
        // The two controls NOT written stay unknown even though their sibling
        // controls on the same output are now known. A caller replaying saved
        // settings skips these; one flag per output would have replayed the
        // daemon's inert 0 dB and 0-step defaults onto the hardware.
        assert!(!row.delay_applied, "delay was never written");
        assert!(!row.gain_applied, "gain was never written");
        assert_eq!(row.delay_steps, 0);
        assert_eq!(row.gain_steps, 0);
    }

    #[test]
    fn a_failed_ioctl_leaves_the_model_and_generation_untouched() {
        let (controls, shared) = fixture();
        controls
            .set_output_mute(DspOutput::FrontLeft, true)
            .expect("first write");
        let before = controls.snapshot().expect("snapshot");

        shared.lock().expect("fake lock").fail_with = Some(EUCLEAN);
        let error = controls
            .set_output_mute(DspOutput::FrontLeft, false)
            .expect_err("the ioctl failed");
        assert!(matches!(error, ControlError::IllegalState(_)));

        let after = controls.snapshot().expect("snapshot");
        assert_eq!(after.generation, before.generation);
        assert_eq!(after.outputs, before.outputs);
        assert!(
            after
                .outputs
                .iter()
                .find(|row| row.output == DspOutput::FrontLeft)
                .expect("front-left row")
                .muted,
            "the failed unmute must not appear as applied"
        );
        assert!(!after.last_error.is_empty());
    }

    #[test]
    fn a_refused_range_records_the_error_without_moving_the_generation() {
        let (controls, _shared) = fixture();
        let _ = controls.set_output_gain_steps(DspOutput::FrontLeft, 9999);
        let snapshot = controls.snapshot().expect("snapshot");
        assert_eq!(snapshot.generation, 0);
        assert!(snapshot.last_error.contains("9999"), "{:?}", snapshot.last_error);
    }

    #[test]
    fn a_later_success_clears_the_recorded_error() {
        let (controls, _shared) = fixture();
        let _ = controls.set_eq_band_steps(9, 0);
        assert!(!controls.snapshot().expect("snapshot").last_error.is_empty());
        controls.set_eq_band_steps(0, 5).expect("EQ write");
        assert!(controls.snapshot().expect("snapshot").last_error.is_empty());
    }

    // ---- a program reload (ACC recovery) invalidates the model -------------

    fn reload(controls: &DspControls<Arc<Mutex<FakeTransport>>>) {
        controls.guard.lock().expect("guard lock").ensure_ready().expect("activation");
    }

    #[test]
    fn a_program_reload_forgets_every_applied_value_and_says_why() {
        let (controls, shared) = enhancement_fixture();
        assert!(controls.capabilities().writable);
        controls.set_eq_band_steps(2, 6).expect("EQ write");
        controls.set_output_gain_steps(DspOutput::Subwoofer, -40).expect("gain write");
        controls.set_bass_level(-32).expect("bass write");
        controls.notify_volume_index(30).expect("policy input");
        let before = controls.snapshot().expect("snapshot");
        assert_eq!(before.generation, 3);

        reload(&controls);
        let after = controls.snapshot().expect("snapshot");
        assert_eq!(after.generation, 3, "the generation still counts writes only");
        assert_eq!(after.last_error, PROGRAM_RELOADED);
        assert!(after.eq.iter().all(|row| !row.applied));
        assert!(after.outputs.iter().all(|row| !row.gain_applied && !row.mute_applied));
        let enhancements = controls.enhancement_snapshot().expect("enhancements");
        assert!(!enhancements.bass_applied);
        assert!(enhancements.volume_index_known, "a policy input is not a chip value");
        assert_eq!(enhancements.volume_index, 30);

        // The next write lands normally and clears the note.
        let calls_before = updates(&shared).len();
        controls.set_eq_band_steps(2, 6).expect("EQ write after reload");
        assert_eq!(updates(&shared).len(), calls_before + 1);
        let replayed = controls.snapshot().expect("snapshot");
        assert!(replayed.last_error.is_empty());
        assert!(replayed.eq[2].applied);
        assert_eq!(replayed.generation, 4);
    }

    #[test]
    fn a_reload_before_any_write_is_silent() {
        let (controls, _shared) = fixture();
        assert!(controls.capabilities().writable);
        reload(&controls);
        let snapshot = controls.snapshot().expect("snapshot");
        assert!(snapshot.last_error.is_empty());
        assert_eq!(snapshot.generation, 0);
    }

    #[test]
    fn the_capability_read_alone_reconciles_a_reload() {
        let (controls, _shared) = fixture();
        controls.set_output_mute(DspOutput::FrontLeft, true).expect("mute");
        reload(&controls);
        // Hold the guard so the snapshot's non-blocking path must skip it; the
        // capability read then reconciles through the state gate.
        let held = controls.guard.lock().expect("guard lock");
        assert!(controls.snapshot().expect("snapshot").outputs[0].mute_applied);
        drop(held);
        assert!(controls.capabilities().writable);
        assert!(!controls.snapshot().expect("snapshot").outputs[0].mute_applied);
    }

    // ---- errno mapping ----------------------------------------------------

    #[test]
    fn the_three_state_errnos_become_illegal_state_with_their_errno_in_the_text() {
        for (errno, needle) in [(EBUSY, "EBUSY"), (EPIPE, "EPIPE"), (EUCLEAN, "EUCLEAN")] {
            let (controls, shared) = fixture();
            shared.lock().expect("fake lock").fail_with = Some(errno);
            let error = controls
                .set_output_phase_inverted(DspOutput::FrontLeft, true)
                .expect_err("errno must surface");
            match error {
                ControlError::IllegalState(message) => {
                    assert!(message.contains(needle), "{message}");
                    assert!(message.contains(&errno.to_string()), "{message}");
                }
                other => panic!("errno {errno} mapped to {other:?}"),
            }
        }
    }

    #[test]
    fn any_other_errno_becomes_service_specific_with_its_negative_code() {
        let (controls, shared) = fixture();
        shared.lock().expect("fake lock").fail_with = Some(5); // EIO
        let error = controls
            .set_output_mute(DspOutput::FrontLeft, true)
            .expect_err("EIO must surface");
        match error {
            ControlError::ServiceSpecific(code, _) => assert_eq!(code, -5),
            other => panic!("EIO mapped to {other:?}"),
        }
    }

    // ---- capabilities are pinned, not guessed -----------------------------

    #[test]
    fn capabilities_report_the_units_derived_from_the_coefficient_tables() {
        let (controls, _shared) = fixture();
        let capabilities = controls.capabilities();
        assert!(capabilities.writable);
        assert_eq!(capabilities.output_count, 5);
        assert_eq!(capabilities.eq_band_count, 4);
        assert_eq!(capabilities.delay_min_steps, 0);
        assert_eq!(capabilities.delay_max_steps, 145);
        // UNPROVEN on device 3, and reported as unknown rather than guessed.
        assert_eq!(capabilities.delay_step_micros, 0);
        assert!(!capabilities.subwoofer_delay_supported);
        assert_eq!(capabilities.gain_min_steps, -200);
        assert_eq!(capabilities.gain_max_steps, 200);
        // 0.1 dB per step is 10 millibels. 100 would be a whole decibel.
        assert_eq!(capabilities.gain_step_millibels, 10);
        assert_eq!(capabilities.eq_min_steps, -20);
        assert_eq!(capabilities.eq_max_steps, 20);
        assert_eq!(capabilities.eq_steps_per_filter, 2);
        assert_eq!(capabilities.eq_distinct_filters, 21);
        // Per FILTER step, not per request step: one request step is 12.5 mB.
        assert_eq!(capabilities.eq_step_millibels, 25);
        // +/-20 request steps span +/-2.500 dB, not +/-20 dB.
        assert_eq!(capabilities.eq_full_scale_millibels, 250);
        assert_eq!(capabilities.eq_band_centre_hz, vec![20, 96, 470, 2281]);
    }

    #[test]
    fn the_full_gain_range_is_about_twenty_decibels_either_way() {
        // A guard against someone "fixing" gain_step_millibels to 100: that
        // would claim this DSP swings +/-200 dB.
        let capabilities = Capabilities::new(true);
        let span_millibels = capabilities.gain_max_steps * capabilities.gain_step_millibels;
        assert_eq!(span_millibels, 2000, "+20.0 dB, in hundredths of a decibel");
    }

    // ---- ordering: hardware order IS publication order --------------------

    /// Two writers, deterministically interleaved at the exact window the race
    /// lived in: writer A is parked AFTER its ioctl returned and BEFORE it
    /// publishes, and writer B is released into that window.
    ///
    /// The discriminator is load-independent, not a timing guess. Both writers
    /// set the SAME control, so the model can hold only one of them:
    ///
    /// * serialised (correct): B cannot start while A holds the transaction,
    ///   so the ioctls are A then B and the publications are A then B. The
    ///   model ends at B's value.
    /// * unserialised (the bug): A's ioctl has already happened when it parks,
    ///   B then runs to completion and publishes B, and A finally publishes A.
    ///   The ioctls are still A then B — but the model ends at A's value.
    ///
    /// So `gain_steps == B` passes only if the transaction ordered them, and
    /// `gain_steps == A` is precisely the reversed publication. Neither
    /// outcome depends on how fast either thread runs.
    ///
    /// The PRIMARY assertion is stronger still and does not involve a second
    /// thread at all: while writer A sits in the window, it observes its own
    /// `transaction.try_lock()` and must get `WouldBlock`. `std::sync::Mutex`
    /// is not reentrant, so a `WouldBlock` from the holding thread is a direct
    /// statement that the lock is held across the ioctl-to-publication window
    /// — true or false regardless of what any scheduler does. (The bounded
    /// wait on writer B below is kept as corroboration, not as the verdict:
    /// root is right that a `recv_timeout` alone can pass the old code simply
    /// because B never got scheduled.)
    #[test]
    fn a_second_writer_cannot_publish_inside_the_first_writers_window() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;

        const A_VALUE: i32 = -10;
        const B_VALUE: i32 = 20;

        let (controls, shared) = fixture();
        let controls = Arc::new(controls);

        // Channels, not barriers: a partner that dies must never hang the
        // test thread on a rendezvous that can no longer complete.
        let (parked_tx, parked_rx) = mpsc::channel::<bool>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let hook_calls = Arc::new(AtomicUsize::new(0));

        let hook_count = Arc::clone(&hook_calls);
        let hook_controls = Arc::downgrade(&controls);
        // The hook must be Sync; mpsc endpoints are not, so they travel inside
        // a Mutex. Only the parking writer ever touches them.
        let endpoints = Mutex::new((parked_tx, release_rx));
        controls.set_publish_barrier(Arc::new(move || {
            // Only the FIRST successful write parks; B must run through.
            if hook_count.fetch_add(1, Ordering::SeqCst) == 0 {
                let controls = hook_controls.upgrade().expect("controls still alive");
                // The observation, made ON the thread that is mid-transaction.
                // Reported rather than asserted here, so a failure surfaces on
                // the test thread instead of poisoning a worker.
                let held = matches!(
                    controls.transaction.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                );
                let endpoints = endpoints.lock().expect("hook endpoints");
                let _ = endpoints.0.send(held);
                let _ = endpoints.1.recv();
            }
        }));

        let writer_a = {
            let controls = Arc::clone(&controls);
            std::thread::spawn(move || {
                controls
                    .set_output_gain_steps(DspOutput::FrontLeft, A_VALUE)
                    .expect("writer A");
            })
        };

        // A is now inside the window: its ioctl has returned, its model
        // publication has not happened.
        let transaction_held = parked_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("writer A never reached the publication window");
        assert_eq!(
            updates(&shared).len(),
            1,
            "A's ioctl must already have reached the transport"
        );
        assert_eq!(
            controls.snapshot().expect("snapshot").generation,
            0,
            "A must not have published yet"
        );

        let (b_done_tx, b_done_rx) = mpsc::channel();
        let writer_b = {
            let controls = Arc::clone(&controls);
            std::thread::spawn(move || {
                controls
                    .set_output_gain_steps(DspOutput::FrontRight, B_VALUE)
                    .expect("writer B");
                let _ = b_done_tx.send(());
            })
        };

        // Corroboration only — see the note above. Recorded, not asserted on
        // its own, because "B did not finish in 250 ms" is also what an
        // unscheduled thread looks like.
        let b_finished_inside_the_window = b_done_rx
            .recv_timeout(std::time::Duration::from_millis(250))
            .is_ok();

        // Everything is released before any assertion, so no thread can be
        // left parked by a panic.
        let _ = release_tx.send(());
        writer_a.join().expect("writer A thread");
        writer_b.join().expect("writer B thread");

        // THE schedule-independent negative: a Mutex is not reentrant, so the
        // holding thread's own try_lock is WouldBlock exactly when the lock is
        // held. Dropping the transaction from apply() makes this false on
        // every run, on every machine.
        assert!(
            transaction_held,
            "the ioctl-to-publication window did not run under the transaction: \
             the writer's own transaction.try_lock() succeeded"
        );
        assert!(
            !b_finished_inside_the_window,
            "writer B completed while writer A still held the transaction"
        );

        // Hardware order: A then B, in both worlds.
        let issued: Vec<i32> = updates(&shared).iter().map(|update| update.value).collect();
        assert_eq!(issued, vec![A_VALUE, B_VALUE], "ioctl order");

        // Publication order: A then B. Under the bug the model would show A's
        // value on FRONT_LEFT published last and B's generation ahead of it.
        let snapshot = controls.snapshot().expect("snapshot");
        assert_eq!(snapshot.generation, 2);
        let front_left = snapshot
            .outputs
            .iter()
            .find(|row| row.output == DspOutput::FrontLeft)
            .expect("front-left row");
        let front_right = snapshot
            .outputs
            .iter()
            .find(|row| row.output == DspOutput::FrontRight)
            .expect("front-right row");
        assert_eq!(front_left.gain_steps, A_VALUE);
        assert_eq!(front_right.gain_steps, B_VALUE);
        assert_eq!(hook_calls.load(Ordering::SeqCst), 2, "both writes hooked");
    }

    /// The same construction on ONE control, where the reversed publication
    /// leaves a value the hardware does not hold. This is the assertion the
    /// "drop the transaction lock" mutant fails on value, not on timing.
    #[test]
    fn the_model_ends_on_the_value_the_hardware_received_last() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;

        const FIRST: i32 = -10;
        const LAST: i32 = 20;

        let (controls, shared) = fixture();
        let controls = Arc::new(controls);
        let (parked_tx, parked_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let calls = Arc::new(AtomicUsize::new(0));

        let hook_calls = Arc::clone(&calls);
        let endpoints = Mutex::new((parked_tx, release_rx));
        controls.set_publish_barrier(Arc::new(move || {
            if hook_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let endpoints = endpoints.lock().expect("hook endpoints");
                let _ = endpoints.0.send(());
                let _ = endpoints.1.recv();
            }
        }));

        let first = {
            let controls = Arc::clone(&controls);
            std::thread::spawn(move || {
                controls
                    .set_output_gain_steps(DspOutput::RearLeft, FIRST)
                    .expect("first writer");
            })
        };
        parked_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the first writer never reached the publication window");

        let (done_tx, done_rx) = mpsc::channel();
        let last = {
            let controls = Arc::clone(&controls);
            std::thread::spawn(move || {
                controls
                    .set_output_gain_steps(DspOutput::RearLeft, LAST)
                    .expect("second writer");
                let _ = done_tx.send(());
            })
        };
        // Bounded wait, deliberately NOT asserted on: it is a synchronisation
        // point, not the verdict. Serialised, it times out because the second
        // writer is blocked; unserialised, it returns because that writer has
        // already published LAST — and then releasing the first writer makes
        // it publish FIRST on top, which is the reversal this test catches by
        // VALUE below. Either way the test proceeds, so no timing decides it.
        let _ = done_rx.recv_timeout(std::time::Duration::from_millis(250));
        let _ = release_tx.send(());
        first.join().expect("first thread");
        last.join().expect("second thread");

        let issued: Vec<i32> = updates(&shared).iter().map(|update| update.value).collect();
        assert_eq!(issued, vec![FIRST, LAST], "the chip saw FIRST then LAST");
        let snapshot = controls.snapshot().expect("snapshot");
        let row = snapshot
            .outputs
            .iter()
            .find(|row| row.output == DspOutput::RearLeft)
            .expect("rear-left row");
        assert_eq!(
            row.gain_steps, LAST,
            "the model must end on what the chip actually holds"
        );
        assert_eq!(snapshot.generation, 2);
    }

    // ---- no unrequested traffic -------------------------------------------

    #[test]
    fn constructing_and_reading_never_writes_to_the_chip() {
        let (controls, shared) = fixture();
        let _ = controls.capabilities();
        let _ = controls.snapshot();
        let _ = controls.capabilities();
        assert!(
            updates(&shared).is_empty(),
            "no startup, reconnect or default-render write burst"
        );
    }
}
