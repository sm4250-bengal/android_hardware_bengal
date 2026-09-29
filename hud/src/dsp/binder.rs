// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! HUD-owned Binder adapter for the host-tested DSP readback and control core.
//!
//! # Why the V2 half is behind a feature
//!
//! The desk harness (`platform/scripts/hud-binder-desk-test.sh`) generates its
//! AIDL stub from the FROZEN `aidl_api/vendor.xyauto.dsp/<N>/` snapshot named by
//! the `-V<N>-rust` pin in `Android.bp`. Until root freezes V2 and moves that
//! pin, the only `IDsp` trait that exists anywhere — desk or Soong — is V1's
//! two read methods, and an `impl` carrying seven more would not compile in
//! either place. `dsp-v2` therefore gates exactly the V2 methods and their
//! imports, and root turns it on in the same change that bumps the pin. That
//! keeps `main` green on the current pin instead of red until a freeze lands.
//!
//! Nothing of consequence lives here: validation, the state gate, the model and
//! the exception mapping are all in [`super::controls`], which `cargo test`
//! covers. This file copies parcelable fields.

use super::controls::DspControls;
#[cfg(feature = "dsp-v3")]
use vendor_xyauto_dsp::aidl::vendor::xyauto::dsp::{
    DspEnhancementCapabilities::DspEnhancementCapabilities as AidlEnhancementCapabilities,
    DspEnhancementSnapshot::DspEnhancementSnapshot as AidlEnhancementSnapshot,
};
use super::guard::{AudioGuard, KernelTransport};
use super::readback::{LinuxParameterRamBus, DEFAULT_ADAPTER};
use binder::{BinderFeatures, ExceptionCode as BinderException, Interface, Status};
use std::sync::{Arc, Mutex, MutexGuard};
use vendor_xyauto_dsp::aidl::vendor::xyauto::dsp::{
    DspIdentity::DspIdentity as AidlDspIdentity,
    DspParameterBlock::DspParameterBlock as AidlDspParameterBlock,
    IDsp::{BnDsp, IDsp},
};
use xyauto_dsp::adau1701::{Adau1701, MonotonicClock};
use xyauto_dsp::service::{self as core, DspApi as _};

#[cfg(feature = "dsp-v2")]
use super::controls::{Capabilities, ControlError, Snapshot};
#[cfg(feature = "dsp-v2")]
use super::guard::DspOutput;
#[cfg(feature = "dsp-v2")]
use vendor_xyauto_dsp::aidl::vendor::xyauto::dsp::{
    DspControlCapabilities::DspControlCapabilities as AidlDspControlCapabilities,
    DspControlSnapshot::DspControlSnapshot as AidlDspControlSnapshot,
    DspEqBand::DspEqBand as AidlDspEqBand, DspOutput::DspOutput as AidlDspOutput,
    DspOutputState::DspOutputState as AidlDspOutputState,
};

/// Stable service-manager name reserved by the VINTF interface.
pub const DSP_SERVICE_NAME: &str = "vendor.xyauto.dsp.IDsp/default";

/// Register `IDsp/default` against the daemon's own audio-guard owner.
///
/// `audio_guard` is the daemon's SINGLE `/dev/hu-audio-guard` owner, created in
/// `binder.rs` and shared with `IAudioGuard`. IDsp does not open the guard and
/// must never be given a fresh one: the kernel allows exactly one opener and
/// runs its all-safe shutdown on close, so a second owner would be an audible
/// fault. Registration performs no DSP write of any kind.
///
/// Reuse the guard owner for kernel-muted reads. Missing kernel support still
/// yields an inert readback service; calls never open a raw bus or fall back
/// to a socket, raw register API or a second process. Guard injection is the
/// only registration entry point, so omitting the shared owner fails to build.
pub fn open_and_register_with_guard(
    audio_guard: Arc<Mutex<AudioGuard>>,
    controls: Arc<DspControls>,
) -> binder::Result<()> {
    let core = match LinuxParameterRamBus::open(Arc::clone(&audio_guard)) {
        Ok(bus) => core::DspService::live(Adau1701::new(bus, MonotonicClock::default())),
        Err(error) => core::DspService::unavailable(
            DEFAULT_ADAPTER,
            format!("NotReady/ADAU1701-adapter: {error}"),
        ),
    };
    register(
        Arc::new(Mutex::new(core)),
        controls,
    )
}

fn register(
    service: Arc<Mutex<core::DspService>>,
    controls: Arc<DspControls<KernelTransport>>,
) -> binder::Result<()> {
    let binder = BnDsp::new_binder(
        BinderDsp {
            core: service,
            controls,
        },
        BinderFeatures::default(),
    );
    Ok(binder::add_service(DSP_SERVICE_NAME, binder.as_binder())?)
}

struct BinderDsp {
    core: Arc<Mutex<core::DspService>>,
    // Held, but only reachable through the V2 methods. Keeping it constructed
    // in both configurations means the wiring — including the guard the splice
    // passes in — is compiled and type-checked before the freeze lands.
    #[cfg_attr(not(feature = "dsp-v2"), allow(dead_code))]
    controls: Arc<DspControls<KernelTransport>>,
}

impl Interface for BinderDsp {}

impl BinderDsp {
    fn lock(&self) -> binder::Result<MutexGuard<'_, core::DspService>> {
        self.core.lock().map_err(|_| {
            Status::new_exception_str(
                BinderException::ILLEGAL_STATE,
                Some("DSP core mutex poisoned"),
            )
        })
    }
}

impl IDsp for BinderDsp {
    #[cfg(feature = "dsp-v4")]
    fn getSharedOutputGainDb(&self) -> binder::Result<i32> {
        self.controls.shared_output_gain_db().map_err(control_error)
    }

    #[cfg(feature = "dsp-v4")]
    fn setSharedOutputGainDb(&self, gain_db: i32) -> binder::Result<()> {
        self.controls.set_shared_output_gain_db(gain_db).map_err(control_error)
    }

    fn getIdentity(&self) -> binder::Result<AidlDspIdentity> {
        Ok(aidl_identity(self.lock()?.get_identity()))
    }

    fn readParameterRam(
        &self,
        start_word: i32,
        word_count: i32,
    ) -> binder::Result<AidlDspParameterBlock> {
        self.lock()?
            .read_parameter_ram(start_word, word_count)
            .map(aidl_block)
            .map_err(service_error)
    }

    #[cfg(feature = "dsp-v2")]
    fn getControlCapabilities(&self) -> binder::Result<AidlDspControlCapabilities> {
        Ok(aidl_capabilities(self.controls.capabilities()))
    }

    #[cfg(feature = "dsp-v2")]
    fn getControlSnapshot(&self) -> binder::Result<AidlDspControlSnapshot> {
        self.controls
            .snapshot()
            .map(aidl_snapshot)
            .map_err(control_error)
    }

    #[cfg(feature = "dsp-v2")]
    fn setOutputMute(&self, output: AidlDspOutput, muted: bool) -> binder::Result<()> {
        self.controls
            .set_output_mute(control_output(output)?, muted)
            .map_err(control_error)
    }

    #[cfg(feature = "dsp-v2")]
    fn setOutputPhaseInverted(
        &self,
        output: AidlDspOutput,
        inverted: bool,
    ) -> binder::Result<()> {
        self.controls
            .set_output_phase_inverted(control_output(output)?, inverted)
            .map_err(control_error)
    }

    #[cfg(feature = "dsp-v2")]
    fn setOutputDelaySteps(&self, output: AidlDspOutput, steps: i32) -> binder::Result<()> {
        self.controls
            .set_output_delay_steps(control_output(output)?, steps)
            .map_err(control_error)
    }

    #[cfg(feature = "dsp-v2")]
    fn setOutputGainSteps(&self, output: AidlDspOutput, steps: i32) -> binder::Result<()> {
        self.controls
            .set_output_gain_steps(control_output(output)?, steps)
            .map_err(control_error)
    }

    #[cfg(feature = "dsp-v2")]
    fn setEqBandSteps(&self, band: i32, steps: i32) -> binder::Result<()> {
        self.controls
            .set_eq_band_steps(band, steps)
            .map_err(control_error)
    }

    #[cfg(feature = "dsp-v3")]
    fn getEnhancementCapabilities(&self) -> binder::Result<AidlEnhancementCapabilities> {
        let caps = self.controls.enhancement_capabilities();
        Ok(AidlEnhancementCapabilities {
            supported: caps.supported, writable: caps.writable,
            minLevel: -32, maxLevel: 32,
            bassFrequencyHz: 100, trebleFrequencyHz: 5000, sampleRateHz: 48000,
            volumeIndexMin: 0, volumeIndexMax: 63,
            disabledRestoresProgramDefault: true,
        })
    }

    #[cfg(feature = "dsp-v3")]
    fn getEnhancementSnapshot(&self) -> binder::Result<AidlEnhancementSnapshot> {
        let s = self.controls.enhancement_snapshot().map_err(control_error)?;
        Ok(AidlEnhancementSnapshot {
            bassApplied: s.bass_applied, bassLevel: s.bass_level,
            trebleApplied: s.treble_applied, trebleLevel: s.treble_level,
            loudnessApplied: s.loudness_applied, loudnessEnabled: s.loudness_enabled,
            volumeIndexKnown: s.volume_index_known, volumeIndex: s.volume_index,
            loudnessAppliedVolumeIndex: s.loudness_applied_volume_index,
            generation: s.generation as i64, lastError: s.last_error,
        })
    }

    #[cfg(feature = "dsp-v3")]
    fn setBassLevel(&self, level: i32) -> binder::Result<()> {
        self.controls.set_bass_level(level).map_err(control_error)
    }

    #[cfg(feature = "dsp-v3")]
    fn setTrebleLevel(&self, level: i32) -> binder::Result<()> {
        self.controls.set_treble_level(level).map_err(control_error)
    }

    #[cfg(feature = "dsp-v3")]
    fn setLoudnessEnabled(&self, enabled: bool) -> binder::Result<()> {
        self.controls.set_loudness_enabled(enabled).map_err(control_error)
    }

    #[cfg(feature = "dsp-v3")]
    fn notifyVolumeIndex(&self, index: i32) -> binder::Result<()> {
        self.controls.notify_volume_index(index).map_err(control_error)
    }
}

fn aidl_identity(value: core::DspIdentity) -> AidlDspIdentity {
    AidlDspIdentity {
        available: value.available,
        unavailableReason: value.unavailable_reason,
        chipModel: value.chip_model,
        i2cAdapter: value.i2c_adapter,
        i2cAddress: i32::from(value.i2c_address),
        parameterWordCount: i32::try_from(value.parameter_word_count).unwrap_or(i32::MAX),
        parameterWordBytes: i32::try_from(value.parameter_word_bytes).unwrap_or(i32::MAX),
        maxReadWords: i32::try_from(value.max_read_words).unwrap_or(i32::MAX),
        minReadIntervalMs: i32::try_from(value.min_read_interval_ms).unwrap_or(i32::MAX),
    }
}

fn aidl_block(value: core::DspParameterBlock) -> AidlDspParameterBlock {
    AidlDspParameterBlock {
        startWord: i32::from(value.start_word),
        words: value.words.into_iter().map(|word| word as i32).collect(),
    }
}

fn service_error(error: core::ServiceError) -> Status {
    match error.code {
        core::ExceptionCode::IllegalArgument => {
            Status::new_exception_str(BinderException::ILLEGAL_ARGUMENT, Some(error.message))
        }
        core::ExceptionCode::IllegalState => {
            Status::new_exception_str(BinderException::ILLEGAL_STATE, Some(error.message))
        }
        core::ExceptionCode::ServiceSpecific => {
            Status::new_service_specific_error_str(-1, Some(error.message))
        }
    }
}

/// Translate one AIDL output value, refusing anything outside the enum.
#[cfg(feature = "dsp-v2")]
fn control_output(value: AidlDspOutput) -> binder::Result<DspOutput> {
    DspOutput::from_raw(value.0).ok_or_else(|| {
        Status::new_exception_str(
            BinderException::ILLEGAL_ARGUMENT,
            Some(format!("unknown DspOutput value {}", value.0)),
        )
    })
}

/// The whole exception policy lives in [`ControlError`]; this is the `match`.
#[cfg(feature = "dsp-v2")]
fn control_error(error: ControlError) -> Status {
    match error {
        ControlError::IllegalArgument(message) => {
            Status::new_exception_str(BinderException::ILLEGAL_ARGUMENT, Some(message))
        }
        ControlError::IllegalState(message) => {
            Status::new_exception_str(BinderException::ILLEGAL_STATE, Some(message))
        }
        ControlError::Unsupported(message) => {
            Status::new_exception_str(BinderException::UNSUPPORTED_OPERATION, Some(message))
        }
        ControlError::ServiceSpecific(code, message) => {
            Status::new_service_specific_error_str(code, Some(message))
        }
    }
}

#[cfg(feature = "dsp-v2")]
fn aidl_capabilities(value: Capabilities) -> AidlDspControlCapabilities {
    AidlDspControlCapabilities {
        writable: value.writable,
        outputCount: value.output_count,
        eqBandCount: value.eq_band_count,
        delayMinSteps: value.delay_min_steps,
        delayMaxSteps: value.delay_max_steps,
        delayStepMicros: value.delay_step_micros,
        gainMinSteps: value.gain_min_steps,
        gainMaxSteps: value.gain_max_steps,
        gainStepMillibels: value.gain_step_millibels,
        eqMinSteps: value.eq_min_steps,
        eqMaxSteps: value.eq_max_steps,
        eqStepsPerFilter: value.eq_steps_per_filter,
        eqDistinctFilters: value.eq_distinct_filters,
        eqStepMillibels: value.eq_step_millibels,
        eqFullScaleMillibels: value.eq_full_scale_millibels,
        eqBandCentreHz: value.eq_band_centre_hz,
        subwooferDelaySupported: value.subwoofer_delay_supported,
    }
}

#[cfg(feature = "dsp-v2")]
fn aidl_snapshot(value: Snapshot) -> AidlDspControlSnapshot {
    AidlDspControlSnapshot {
        outputs: value
            .outputs
            .into_iter()
            .map(|row| AidlDspOutputState {
                output: AidlDspOutput(row.output.raw() as i32),
                muteApplied: row.mute_applied,
                muted: row.muted,
                phaseApplied: row.phase_applied,
                phaseInverted: row.phase_inverted,
                delayApplied: row.delay_applied,
                delaySteps: row.delay_steps,
                gainApplied: row.gain_applied,
                gainSteps: row.gain_steps,
            })
            .collect(),
        eq: value
            .eq
            .into_iter()
            .map(|row| AidlDspEqBand {
                band: row.band,
                applied: row.applied,
                steps: row.steps,
            })
            .collect(),
        generation: value.generation as i64,
        lastError: value.last_error,
    }
}
