// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Android-only `libbinder_rs` adapter for the host-tested FM service.
//!
//! Soong alone enables this module. The daemon supplies a concrete
//! [`crate::service::FmApi`] backed by its sole QN8035/I2C/MCU/DSP/GPIO owner;
//! this adapter only translates generated parcelables, callbacks and errors.

use crate::service as core;
use binder::{BinderFeatures, ExceptionCode as BinderException, Interface, Status, Strong};
use std::sync::{Arc, Mutex, MutexGuard};
use vendor_xyauto_fm::aidl::vendor::xyauto::fm::{
    IFmRadio::{BnFmRadio, IFmRadio},
    IFmRadioCallback::IFmRadioCallback,
    RdsInfo::RdsInfo as AidlRdsInfo,
    SignalQuality::SignalQuality as AidlSignalQuality,
    TunerState::TunerState as AidlTunerState,
};

/// Stable service-manager name from the VINTF fragment.
pub const FM_SERVICE_NAME: &str = "vendor.xyauto.fm.IFmRadio/default";

/// Register `IFmRadio/default` before the owning daemon joins its thread pool.
///
/// The caller retains another [`Arc`] so its seek/RDS worker can call
/// `FmService` completion hooks on the exact instance Binder serves.
pub fn register<S>(service: Arc<Mutex<S>>) -> binder::Result<()>
where
    S: core::FmApi + 'static,
{
    let binder = BnFmRadio::new_binder(BinderFm { core: service }, BinderFeatures::default());
    // `?` and not a bare tail expression: add_service() returns
    // Result<(), StatusCode> while this function returns binder::Result<()> =
    // Result<(), Status>. The `?` converts StatusCode into Status through the
    // From impl binder provides, and Ok() rewraps the success arm.
    //
    // Caught by the FIRST build that ever compiled this crate (2026-08-31):
    //   error[E0308]: mismatched types  hardware/xyauto/fm/src/binder.rs:33:5
    //   expected `Result<(), Status>`, found `Result<(), StatusCode>`
    // The two error types are one word apart in the docs and the mistake is
    // invisible to every desk check we had, because none of them compiled Rust.
    Ok(binder::add_service(FM_SERVICE_NAME, binder.as_binder())?)
}

struct BinderFm<S> {
    core: Arc<Mutex<S>>,
}

impl<S> Interface for BinderFm<S> where S: core::FmApi + 'static {}

impl<S> BinderFm<S> {
    fn lock(&self) -> binder::Result<MutexGuard<'_, S>> {
        self.core.lock().map_err(|_| {
            Status::new_exception_str(
                BinderException::ILLEGAL_STATE,
                Some("FM core mutex poisoned"),
            )
        })
    }
}

impl<S> IFmRadio for BinderFm<S>
where
    S: core::FmApi + 'static,
{
    fn setEnabled(&self, enabled: bool) -> binder::Result<()> {
        self.lock()?.set_enabled(enabled).map_err(service_error)
    }

    fn getState(&self) -> binder::Result<AidlTunerState> {
        Ok(aidl_state(self.lock()?.get_state()))
    }

    fn getChipId(&self) -> binder::Result<i32> {
        self.lock()?
            .get_chip_id()
            .map(i32::from)
            .map_err(service_error)
    }

    fn tune(&self, frequency_khz: i32) -> binder::Result<()> {
        let frequency_khz = u32::try_from(frequency_khz).map_err(|_| {
            service_error(core::ServiceError {
                code: core::ExceptionCode::IllegalArgument,
                message: "frequency must be non-negative".into(),
            })
        })?;
        self.lock()?.tune(frequency_khz).map_err(service_error)
    }

    fn getSignalQuality(&self) -> binder::Result<AidlSignalQuality> {
        self.lock()?
            .get_signal_quality()
            .map(aidl_signal)
            .map_err(service_error)
    }

    fn startSeek(&self, upward: bool) -> binder::Result<()> {
        self.lock()?.start_seek(upward).map_err(service_error)
    }

    fn stopSeek(&self) -> binder::Result<()> {
        self.lock()?.stop_seek().map_err(service_error)
    }

    fn getRdsInfo(&self) -> binder::Result<AidlRdsInfo> {
        Ok(aidl_rds(self.lock()?.get_rds_info()))
    }

    fn setCallback(&self, callback: Option<&Strong<dyn IFmRadioCallback>>) -> binder::Result<()> {
        self.lock()?.set_callback(
            callback.cloned().map(|callback| {
                Box::new(BinderCallback(callback)) as Box<dyn core::FmRadioCallback>
            }),
        );
        Ok(())
    }
}

struct BinderCallback(Strong<dyn IFmRadioCallback>);

impl core::FmRadioCallback for BinderCallback {
    fn on_seek_complete(&self, frequency_khz: u32) {
        let _ = self
            .0
            .onSeekComplete(i32::try_from(frequency_khz).unwrap_or(i32::MAX));
    }

    fn on_seek_stopped(&self, band_exhausted: bool) {
        let _ = self.0.onSeekStopped(band_exhausted);
    }

    fn on_rds_update(&self, rds: &core::RdsInfo) {
        let _ = self.0.onRdsUpdate(&aidl_rds(rds.clone()));
    }

    fn on_tuner_lost(&self, reason: &str) {
        let _ = self.0.onTunerLost(reason);
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

fn aidl_state(state: core::TunerState) -> AidlTunerState {
    AidlTunerState {
        enabled: state.enabled,
        frequencyKHz: i32::try_from(state.frequency_khz).unwrap_or(i32::MAX),
        seeking: state.seeking,
        i2cAdapter: state.i2c_adapter,
    }
}

fn aidl_signal(signal: core::SignalQuality) -> AidlSignalQuality {
    AidlSignalQuality {
        snr: i32::from(signal.snr),
        rssi: i32::from(signal.rssi),
        stationPresent: signal.station_present,
    }
}

fn aidl_rds(rds: core::RdsInfo) -> AidlRdsInfo {
    AidlRdsInfo {
        programmeIdentification: i32::from(rds.programme_identification),
        programmeService: rds.programme_service,
        synchronised: rds.synchronized,
    }
}
