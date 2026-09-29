// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The Bengal device-3 project

//! The production QN8035 half of `IFmRadio`.
//!
//! HuRadio owns the two-phase source handover through the typed
//! `IAudioGuard` service:
//!
//! ```text
//! setFmPower(true) -> tune(while disabled) -> prepareFm -> commitFm
//!     -> IFmRadio.setEnabled(true)
//! ```
//!
//! The guard is therefore the only owner of the FM rails, ADAU gates and
//! analogue A=0/B=1 mux.  This backend deliberately owns only the QN8035
//! transaction sequence that follows the committed, muted route: reset, CID,
//! RX/RDS, prepared channel, then unmute.  It never opens a GPIO, MCU, DSP
//! node or legacy socket.

use crate::i2c::{self, adapter::FM_NEEDLES};
use std::fmt;
use std::io::Write as _;
use std::time::{Duration, Instant};
use std::path::Path;
use xyauto_fm::i2c::linux::LinuxI2c;
use xyauto_fm::qn8035::{
    self, RdsState, ScanOutcome, ScanStep, SeekDirection, SignalQuality, Tuner, TunerError,
};
use xyauto_fm::service::{EnableError, FmHardware, SeekStep};

/// Stable failure text when the packaged I2C path is not available.
const NOT_READY_PREFIX: &str = "NotReady/QN8035-backend";

/// The one real QN8035 backend used by the HUD's declared FM Binder service.
///
/// Opening the character device does not change hardware state.  All QN8035
/// writes happen only from `set_enabled(true)` after HuRadio has committed the
/// typed audio-guard handover.
pub struct ProductionFmBackend {
    adapter_label: String,
    tuner: Option<Tuner<LinuxI2c>>,
    unavailable: Option<String>,
    /// The queued band walk, owned beside the sole tuner exactly as
    /// `BandScan`'s own documentation requires. `None` = no seek in flight.
    scan: Option<qn8035::BandScan>,
    /// The VOLUME byte `enable()` last wrote, or `None` while disabled.
    ///
    /// A walk mutes the tuner and has to put back *exactly* what was there.
    /// Restoring a hard-coded "unmuted" instead would make a seek an
    /// un-mute for a radio something else had deliberately silenced.
    audible_volume: Option<u8>,
    /// The walk currently holds the tuner muted.
    muted_for_seek: bool,
    /// Wall-clock bound on the walk in flight.
    deadline: Option<Instant>,
    /// The candidate the scan just physically tuned. This is a cache of the
    /// `BandScan::step` result, not a register query: the service may publish
    /// it through frozen `getState()` while `seeking` remains true.
    seek_candidate: Option<u32>,
}

impl ProductionFmBackend {
    /// Resolve `i2c-fm` by identity and open its I2C_RDWR-only character node.
    ///
    /// The radio rail may still be off here; opening `/dev/i2c-N` is inert.
    /// A missing module/node stays a declared service with an actionable
    /// backend error instead of silently returning to the old placeholder.
    pub fn new(sysfs_root: &Path) -> Self {
        match open_tuner(sysfs_root) {
            Ok((label, tuner)) => Self {
                adapter_label: label,
                tuner: Some(tuner),
                unavailable: None,
                scan: None,
                    audible_volume: None,
                    muted_for_seek: false,
                    deadline: None,
                    seek_candidate: None,
            },
            Err(error) => {
                let message = format!("{NOT_READY_PREFIX}: {error}");
                Self {
                    adapter_label: message.clone(),
                    tuner: None,
                    unavailable: Some(message),
                    scan: None,
                    audible_volume: None,
                    muted_for_seek: false,
                    deadline: None,
                    seek_candidate: None,
                }
            }
        }
    }

    fn tuner(&mut self) -> Result<&mut Tuner<LinuxI2c>, BackendError> {
        self.tuner.as_mut().ok_or_else(|| {
            BackendError(self.unavailable.clone().unwrap_or_else(|| {
                format!("{NOT_READY_PREFIX}: tuner transport was not constructed")
            }))
        })
    }

    fn enable(&mut self, prepared_frequency_khz: Option<u32>) -> Result<(), BackendError> {
        let frequency = prepared_frequency_khz.ok_or_else(|| {
            BackendError("FM enable requires a channel prepared while disabled".to_owned())
        })?;
        let tuner = self.tuner()?;

        // Keep the QN8035 itself silent until the selected channel has been
        // established.  The ADAU master is still muted by prepareFm at this
        // point; both boundaries stay closed through reset and PLL startup.
        tuner
            .write_register(qn8035::reg::VOLUME, 0x87)
            .map_err(BackendError::from)?;
        tuner
            .write_register(qn8035::reg::SYSTEM1, qn8035::system1::SOFTWARE_RESET)
            .map_err(BackendError::from)?;
        std::thread::sleep(std::time::Duration::from_millis(250));
        tuner.verify_chip_id().map_err(BackendError::from)?;
        tuner
            .write_register(qn8035::reg::SYSTEM1, qn8035::system1::RECEIVE_RDS)
            .map_err(BackendError::from)?;
        tuner.tune(frequency).map_err(BackendError::from)?;
        tuner
            .write_register(qn8035::reg::VOLUME, 0x07)
            .map_err(BackendError::from)?;
        // Remembered so a seek's mute can be undone with the same byte.
        self.audible_volume = Some(0x07);
        Ok(())
    }

    fn disable(&mut self) -> Result<(), BackendError> {
        // During HuRadio's exit the guard has already atomically returned the
        // ADAU to Android before this mute/standby edge.  Retain the descriptor
        // for the daemon lifetime; it is the one exclusive I2C owner.
        self.audible_volume = None;
        self.muted_for_seek = false;
        let tuner = self.tuner()?;
        tuner
            .write_register(qn8035::reg::VOLUME, 0x87)
            .map_err(BackendError::from)?;
        tuner
            .write_register(qn8035::reg::SYSTEM1, qn8035::system1::STANDBY)
            .map_err(BackendError::from)
    }
}

/// One `write_all` of one preformatted line: one kmsg record.
///
/// Deliberately NOT `binder::log_both`. This module is compiled on its own by
/// `platform/scripts/fm-backend-desk-test.sh`, and reaching into the Binder
/// adapter for a diagnostic would make the backend untypecheckable at the desk
/// — which is how a scan tell would quietly stop being verified. `logsink` has
/// no dependencies and carries the record format that stops one line from
/// becoming three records against a ten-per-five-seconds budget.
fn log_line(message: &str) {
    let line = crate::logsink::kmsg_line(false, message);
    let _ = std::io::stderr().write_all(line.as_bytes());
}

fn open_tuner(sysfs_root: &Path) -> Result<(String, Tuner<LinuxI2c>), BackendError> {
    let adapter = i2c::resolve(sysfs_root, FM_NEEDLES, None)
        .map_err(|error| BackendError(format!("cannot resolve i2c-fm: {error}")))?;
    let label = adapter.to_string();
    let bus = LinuxI2c::open(&adapter.path)
        .map_err(|error| BackendError(format!("cannot open {label}: {error}")))?;
    Ok((label, Tuner::new(bus)))
}

/// Backend errors become IFmRadio service-specific failures without exposing a
/// raw device API to HuRadio.
#[derive(Clone, Debug)]
pub struct BackendError(String);

impl<E: fmt::Display> From<TunerError<E>> for BackendError {
    fn from(error: TunerError<E>) -> Self {
        Self(error.to_string())
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BackendError {}

impl FmHardware for ProductionFmBackend {
    type Error = BackendError;

    fn adapter_label(&self) -> String {
        self.adapter_label.clone()
    }

    fn set_enabled(
        &mut self,
        enabled: bool,
        prepared_frequency_khz: Option<u32>,
    ) -> Result<(), EnableError<Self::Error>> {
        if enabled {
            self.enable(prepared_frequency_khz)
                .map_err(EnableError::Hardware)
        } else {
            self.disable().map_err(EnableError::Hardware)
        }
    }

    fn chip_id(&mut self) -> Result<u8, Self::Error> {
        self.tuner()?.verify_chip_id().map_err(BackendError::from)
    }

    fn tune(&mut self, frequency_khz: u32) -> Result<(), Self::Error> {
        self.tuner()?
            .tune(frequency_khz)
            .map_err(BackendError::from)
    }

    fn signal_quality(&mut self) -> Result<SignalQuality, Self::Error> {
        self.tuner()?.signal_quality().map_err(BackendError::from)
    }

    /// Queue the walk; do NOT perform it here.
    ///
    /// This runs on a Binder thread, so it only builds the state machine and
    /// returns. The channel list is computed from the CURRENT frequency, so a
    /// seek never rechecks the station it started on.
    fn start_seek(&mut self, direction: SeekDirection, from_khz: u32) -> Result<(), Self::Error> {
        // Fail here, on the Binder thread, if the channel is off-grid: the
        // worker has no caller to report a rejection to.
        let scan = qn8035::BandScan::new(from_khz, direction)
            .map_err(|error| BackendError(format!("seek rejected: {error}")))?;

        // MUTE THE TUNER FOR THE WHOLE WALK -- restored from the working LOS
        // 23.2 seek, which device 3's port dropped. `radio.go:566-570`: "A
        // seek retunes the front end every 100 ms across up to 205 channels.
        // Left audible that is a sweep of noise bursts through the speakers."
        // HuRadio holds the DSP master at zero for a SCAN, but nothing covers
        // a bare Seek press, and at the 100 ms settle each of 205 channels is
        // now dwelt on rather than flicked past.
        //
        // 0x87 is 0x07 | MUTE_EN, the same byte `disable()` writes. Only mute
        // if the radio is actually audible: muting a disabled tuner would
        // leave `muted_for_seek` promising a restore that has nothing to
        // restore to.
        if self.audible_volume.is_some() {
            self.tuner()?
                .write_register(qn8035::reg::VOLUME, 0x87)
                .map_err(BackendError::from)?;
            self.muted_for_seek = true;
        }
        self.deadline =
            Some(Instant::now() + Duration::from_millis(qn8035::SEEK_DEADLINE_MS));
        self.seek_candidate = None;
        self.scan = Some(scan);
        // Half of the tell. Its absence says startSeek never reached the
        // backend; a start with no matching result says the WORKER is dead,
        // which is a different fault from a walk that found nothing. Two
        // records per seek against a ten-per-five-seconds budget, and a walk
        // takes twenty seconds -- a per-channel line would evict the summary.
        log_line(&format!(
            "fm seek start from={from_khz} direction={direction:?} muted={}",
            if self.muted_for_seek { "yes" } else { "no" }
        ));
        Ok(())
    }

    fn stop_seek(&mut self) -> Result<(), Self::Error> {
        let Some(scan) = self.scan.take() else {
            return Ok(());
        };
        // A cancelled walk ends nowhere in particular, so the model's own
        // restore does not run. Put the chip back on the caller's channel
        // before the audio returns, or `stopSeek` leaves it on noise.
        let restored = {
            let tuner = self.tuner()?;
            scan.restore(tuner)
        };
        Self::report(&scan, "stopped");
        let unmuted = self.end_of_walk();
        restored.map_err(BackendError::from)?;
        unmuted?;
        self.seek_candidate = None;
        Ok(())
    }

    /// One RDS poll. The service decides WHEN (window open, not seeking); this
    /// only does the bus work and logs the bounded events.
    ///
    /// Logging is per PUBLICATION and per FAILURE, never per poll: a window is
    /// up to ~130 polls and the daemon's kmsg budget is ten records per five
    /// seconds. The assembler publishes a (PI, PS) pair once, and the service
    /// closes the window on the first failure, so both lines are bounded by
    /// the number of tunes.
    fn poll_rds(&mut self) -> Result<Option<RdsState>, Self::Error> {
        let published = match self.tuner()?.poll_rds() {
            Ok(published) => published,
            Err(error) => {
                log_line(&format!("fm rds window closed: {error}"));
                return Err(BackendError::from(error));
            }
        };
        if let Some(state) = &published {
            log_line(&format!(
                "fm rds published pi=0x{:04x} ps=\"{}\"",
                state.programme_identification, state.programme_service
            ));
        }
        Ok(published)
    }

    /// One half-channel per call — the whole point of the cooperative design.
    ///
    /// Tuning and measuring are separate calls so [`qn8035::SEEK_SETTLE_MS`]
    /// passes with this backend's lock released. The worker honours that; see
    /// `spawn_fm_seek_worker`.
    fn poll_seek(&mut self) -> Result<SeekStep, Self::Error> {
        let Some(mut scan) = self.scan.take() else {
            return Ok(SeekStep::Idle);
        };

        // Bound even a full-dwell walk or a slow bus before HuRadio times out.
        // Adaptive screening saves typical time, not the worst-case budget.
        if self.deadline.is_some_and(|at| Instant::now() >= at) {
            let restored = {
                let tuner = self.tuner()?;
                scan.restore(tuner)
            };
            Self::report(&scan, "deadline");
            let unmuted = self.end_of_walk();
            restored.map_err(BackendError::from)?;
            unmuted?;
            self.seek_candidate = None;
            return Ok(SeekStep::Finished(None));
        }

        let step = {
            let tuner = self.tuner()?;
            scan.step(tuner)
        };
        let step = match step {
            Ok(step) => step,
            Err(error) => {
                // The service ends the seek and reports the tuner lost, so the
                // walk is over either way -- but it is over with a census, and
                // that census says how far it got before the bus went.
                Self::report(&scan, "lost");
                // The audio must come back even when the bus does not.
                let _ = self.end_of_walk();
                self.seek_candidate = None;
                return Err(BackendError::from(error));
            }
        };
        match step {
            // Neither phase is terminal, so the walk goes back in the box.
            ScanStep::Settling(frequency, wait_ms) => {
                // The candidate is already tuned. A promoted screen repeats
                // the same frequency while requesting the remaining dwell.
                // Cache it for getState without another I2C read or retune.
                self.seek_candidate = Some(frequency);
                self.scan = Some(scan);
                Ok(SeekStep::Settling(wait_ms))
            }
            ScanStep::Decided(ScanOutcome::Continue) => {
                self.scan = Some(scan);
                Ok(SeekStep::Continue)
            }
            ScanStep::Decided(ScanOutcome::Complete(frequency)) => {
                Self::report(&scan, "landed");
                self.end_of_walk()?;
                self.seek_candidate = Some(frequency);
                Ok(SeekStep::Finished(Some(frequency)))
            }
            ScanStep::Decided(ScanOutcome::Exhausted) => {
                Self::report(&scan, "exhausted");
                self.end_of_walk()?;
                self.seek_candidate = None;
                Ok(SeekStep::Finished(None))
            }
        }
    }

    fn power_lost(&mut self) {
        /* Bus-free: the guard may already have removed the tuner rails. */
        self.scan = None;
        self.audible_volume = None;
        self.muted_for_seek = false;
        self.deadline = None;
        self.seek_candidate = None;
    }

    fn seek_candidate(&self) -> Option<u32> {
        self.seek_candidate
    }
}

impl ProductionFmBackend {
    /// The other half of the tell: one line, at every terminal outcome.
    ///
    /// SUCCESS IS LOGGED TOO, deliberately. D3-0117's worker logged only on
    /// failure, so a working seek was silent and the thread name was the only
    /// evidence it had run at all. A tell that fires on one branch answers
    /// half a question.
    /// Disarm the deadline and give the tuner its voice back.
    ///
    /// The settle before the un-mute is not padding. `radio.go:585-588`:
    /// *"Whichever way the walk ended, the tuner has just been retuned one
    /// last time. Let that land before the level comes back, or the unmute
    /// reintroduces the very transient the mute was there to hide."*
    /// It is [`qn8035::TUNE_SETTLE_MS`] once per seek, on the worker thread —
    /// not per channel, which is why it can be an inline sleep here while the
    /// per-channel settle deliberately is not.
    fn end_of_walk(&mut self) -> Result<(), BackendError> {
        self.deadline = None;
        if !self.muted_for_seek {
            return Ok(());
        }
        self.muted_for_seek = false;
        let Some(volume) = self.audible_volume else {
            return Ok(());
        };
        std::thread::sleep(Duration::from_millis(qn8035::TUNE_SETTLE_MS));
        self.tuner()?
            .write_register(qn8035::reg::VOLUME, volume)
            .map_err(BackendError::from)
    }

    fn report(scan: &qn8035::BandScan, outcome: &str) {
        log_line(&format!(
            "fm seek result outcome={outcome} {}",
            scan.census()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_documents_the_guard_owned_switch_boundary() {
        let source = include_str!("backend.rs");
        let prepare = source.find("prepareFm -> commitFm").unwrap();
        let enable = source.find("IFmRadio.setEnabled(true)").unwrap();
        assert!(prepare < enable);
        assert!(source.contains("only owner of the FM rails, ADAU gates"));
        assert!(source.contains("never opens a GPIO, MCU, DSP"));
    }

    #[test]
    fn no_legacy_socket_or_raw_huradio_device_path_exists() {
        let source = include_str!("backend.rs");
        assert!(!source.contains("/dev/socket/hu-dsp"));
        assert!(!source.contains("/dev/hu-audio-guard"));
    }
}
