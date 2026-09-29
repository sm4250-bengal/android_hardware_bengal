// SPDX-License-Identifier: Apache-2.0
//! Native ACC release: restore FM, release the keyed audio hold, and recover
//! the guard when that release fails.
//!
//! # The failure this module exists for
//!
//! Every measured device-3 retained-sleep ACC cycle (D3-0137 `acc-user-2`,
//! D3-0138 `acc-user-1`, D3-0139 `acc-vulkan-user-1`) ended with the keyed
//! kernel RELEASE refused: after resume the guard read the shared master back
//! nonzero (`ACC hold lost DSP mute: master=057cba3c`), latched a fault and
//! returned EIO. Before this module the daemon then stayed locked for the rest
//! of the boot: the kernel hold was still active (`hold_blocks` answers EBUSY
//! to everything but ACC_HOLD/GET_STATUS/FAULT_SAFE), the guard kept its
//! `acc_lease` fence, and the preparation worker latched `failed_release` and
//! never prepared another cycle. FM had already been replayed onto a live
//! tuner, so the radio showed a station, scanned, and played nothing; every
//! DSP control was refused. Only closing the guard descriptor -- a reboot or
//! a daemon restart -- cleared it.
//!
//! # What recovery does, in order
//!
//! 1. Reconcile FM while its rails are still powered: the tuner is muted and
//!    put in standby by the FM service's own route-loss path, its replay
//!    intent is dropped, and its ACC fence is cleared. No automatic replay.
//! 2. FAULT_SAFE through the guard: the kernel's terminal safe state. It ends
//!    the hold, clears the latched fault and removes the FM rails.
//! 3. The boot activation ([`AudioGuard::ensure_ready`]): DSP power cycle,
//!    IC60 reload and verification, Android route committed behind the mute,
//!    DAC, amplifier. This is byte-for-byte the sequence every boot runs.
//!
//! A FAULT_SAFE failure keeps the fence: the physical state is unknown, so
//! nothing is allowed to unmute. Any other recovery failure leaves the guard
//! in the kernel's safe state with an explicit activation failure.
//!
//! FM playback does not come back by itself: the app still owns focus, gain
//! and its V2 lease. After recovery a new FM entry succeeds; before it, none
//! could.

use crate::dsp::guard::{AudioGuard, Transport};
use std::sync::Mutex;

/// The FM service half of a native ACC release, narrowed so the recovery
/// order is host-testable. Production implements it for the real FM service.
pub(crate) trait FmAccParticipant {
    /// Restore exactly this lease's saved tuner intent, then clear the fence.
    fn acc_release(&mut self, token: u64) -> Result<(), String>;
    /// The audio route under the tuner is going away; stop claiming it is live.
    fn audio_route_lost(&mut self, rails_live: bool) -> Result<(), String>;
    /// Tell the FM client its tuner is gone. Diagnostic; never fails.
    fn tuner_lost(&mut self, reason: &str);
}

/// What a release call ended in. Only `Failed` keeps the worker latched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReleaseOutcome {
    /// FM restore and the keyed audio release both succeeded.
    Released,
    /// The release failed and the guard was brought back by FAULT_SAFE plus a
    /// full activation. `cause` is the original failure; `notes` are FM-side
    /// reconciliation problems that did not stop the recovery.
    Recovered { cause: String, notes: Vec<String> },
    /// The release failed and so did the recovery. Outputs stay fenced/safe.
    Failed { cause: String, recovery: String, notes: Vec<String> },
}

impl ReleaseOutcome {
    /// Whether the preparation worker may treat this lease as released.
    pub(crate) fn released(&self) -> bool {
        !matches!(self, Self::Failed { .. })
    }
}

/// Reason given to the FM client when recovery removes its rails.
pub(crate) const TUNER_LOST_REASON: &str =
    "ACC wake: audio guard release failed; guard reset and DSP reloaded, start FM again";

/// A clean release also needs to wake a gain writer paused by the ACC fence.
pub(crate) const TUNER_RELEASED_REASON: &str =
    "ACC wake: audio hold released; refresh requested FM route";

/// Run the whole native release for `token`. Never holds both locks at once.
pub(crate) fn release_with_recovery<T: Transport, F: FmAccParticipant>(
    audio: &Mutex<AudioGuard<T>>,
    fm: &Mutex<F>,
    token: u64,
) -> ReleaseOutcome {
    let first = (|| -> Result<(), String> {
        fm.lock()
            .map_err(|_| "FM mutex poisoned".to_owned())?
            .acc_release(token)
            .map_err(|error| format!("FM restore: {error}"))?;
        audio
            .lock()
            .map_err(|_| "audio mutex poisoned".to_owned())?
            .acc_operation(token, 2)
            .map_err(|error| format!("audio release: {error}"))
    })();
    let cause = match first {
        Ok(()) => {
            // Outside the audio lock. A diagnostic hint, never permission to
            // play: the app still checks live intent, focus and route health.
            if let Ok(mut service) = fm.lock() {
                service.tuner_lost(TUNER_RELEASED_REASON);
            }
            return ReleaseOutcome::Released;
        }
        Err(cause) => cause,
    };

    let mut notes = Vec::new();
    // GET_STATUS is legal under an active hold; an unreadable status is
    // treated as "rails off" so the FM reset stays bus-free.
    let rails_live = match audio.lock() {
        Ok(mut guard) => guard.fm_rails_powered().unwrap_or(false),
        Err(_) => false,
    };
    match fm.lock() {
        Ok(mut service) => {
            if let Err(error) = service.audio_route_lost(rails_live) {
                notes.push(format!("FM route-lost: {error}"));
            }
            // After route loss the replay intent is gone, so this only clears
            // a fence a failed FM restore left behind. It never re-enables.
            if let Err(error) = service.acc_release(token) {
                notes.push(format!("FM fence: {error}"));
            }
        }
        Err(_) => notes.push("FM mutex poisoned".to_owned()),
    }
    let recovery = match audio.lock() {
        Ok(mut guard) => guard.recover_after_acc_fault().map_err(|error| error.to_string()),
        Err(_) => Err("audio mutex poisoned".to_owned()),
    };
    if let Ok(mut service) = fm.lock() {
        service.tuner_lost(TUNER_LOST_REASON);
    }
    match recovery {
        Ok(()) => ReleaseOutcome::Recovered { cause, notes },
        Err(recovery) => ReleaseOutcome::Failed { cause, recovery, notes },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::guard::{
        AccHold, DspOutput, DspUpdate, FmAttenuation, FmMixBase, FmMixTransition, RawStatus,
        SourceTransition, ABI_VERSION, FEATURE_ACC_HOLD,
    };
    use std::io;
    use std::sync::{Arc, Mutex};

    const EIO: i32 = 5;
    const EBUSY: i32 = 16;
    const EPIPE: i32 = 32;
    const ESTALE: i32 = 116;
    const EUCLEAN: i32 = 117;

    /// The subset of `xyauto_audio_guard.c` that decides this failure, as an
    /// executable model: `hold_blocks`, the sticky `hold_error`, `hold_fault`
    /// latching, FAULT_SAFE ending the hold, `all_safe` cutting DSP/FM power,
    /// DSP_ON's program load and the source/route bookkeeping.
    #[derive(Default)]
    struct KernelModel {
        status: RawStatus,
        hold_active: bool,
        hold_owner: u64,
        hold_generation: u64,
        hold_error: i32,
        /// `hold_confirm` reads the master back nonzero after resume.
        master_changed_across_suspend: bool,
        /// FAULT_SAFE itself errors (unknown physical state).
        fail_fault_safe: bool,
        /// DSP_ON errors (activation fails after FAULT_SAFE).
        fail_dsp_on: bool,
        calls: Vec<String>,
    }

    impl KernelModel {
        fn booted() -> Self {
            Self {
                status: RawStatus {
                    abi_version: ABI_VERSION,
                    dsp_powered: 1,
                    program_loaded: 1,
                    dac_enabled: 1,
                    route_ready: 1,
                    amplifier_enabled: 1,
                    source: 1,
                    reserved: [FEATURE_ACC_HOLD, 0, 0, 0, 0],
                    ..RawStatus::default()
                },
                ..Self::default()
            }
        }

        fn blocked(&self) -> io::Result<()> {
            if self.hold_active {
                Err(io::Error::from_raw_os_error(EBUSY))
            } else {
                Ok(())
            }
        }

        fn all_safe(&mut self) {
            self.status.dsp_powered = 0;
            self.status.program_loaded = 0;
            self.status.dac_enabled = 0;
            self.status.route_ready = 0;
            self.status.amplifier_enabled = 0;
            self.status.source = 0;
            self.status.fm_powered = 0;
            self.status.transition_pending = 0;
        }

        fn hold_fault(&mut self, error: i32) -> i32 {
            self.hold_error = error;
            self.status.fault_latched = 1;
            self.status.last_error = -error;
            error
        }
    }

    #[derive(Clone)]
    struct ModelTransport(Arc<Mutex<KernelModel>>);

    impl ModelTransport {
        fn with<R>(&self, f: impl FnOnce(&mut KernelModel) -> R) -> R {
            f(&mut self.0.lock().unwrap())
        }
    }

    impl Transport for ModelTransport {
        fn acc_hold(&mut self, mut request: AccHold) -> io::Result<AccHold> {
            self.with(|k| {
                k.calls.push(format!("acc:{}:{}", request.generation, request.operation));
                match request.operation {
                    1 if !k.hold_active => {
                        if k.hold_owner != 0
                            && (request.owner != k.hold_owner
                                || request.generation <= k.hold_generation)
                        {
                            return Err(io::Error::from_raw_os_error(ESTALE));
                        }
                        if k.status.fault_latched != 0 {
                            return Err(io::Error::from_raw_os_error(EUCLEAN));
                        }
                        if k.status.program_loaded == 0 {
                            return Err(io::Error::from_raw_os_error(EPIPE));
                        }
                        k.hold_owner = request.owner;
                        k.hold_generation = request.generation;
                        k.hold_active = true;
                        k.hold_error = 0;
                    }
                    _ if request.owner != k.hold_owner
                        || request.generation != k.hold_generation =>
                    {
                        return Err(io::Error::from_raw_os_error(ESTALE));
                    }
                    _ => {}
                }
                if request.operation == 2 {
                    if !k.hold_active {
                        return Err(io::Error::from_raw_os_error(ESTALE));
                    }
                    if k.hold_error != 0 {
                        return Err(io::Error::from_raw_os_error(k.hold_error));
                    }
                    if k.master_changed_across_suspend {
                        let error = k.hold_fault(EIO);
                        return Err(io::Error::from_raw_os_error(error));
                    }
                    k.hold_active = false;
                    request.held = 0;
                    return Ok(request);
                }
                request.held = u32::from(k.hold_error == 0);
                request.error = -k.hold_error;
                Ok(request)
            })
        }
        fn get_status(&mut self) -> io::Result<RawStatus> {
            self.with(|k| Ok(k.status))
        }
        fn fault_safe(&mut self) -> io::Result<()> {
            self.with(|k| {
                k.calls.push("fault-safe".into());
                if k.fail_fault_safe {
                    return Err(io::Error::from_raw_os_error(EIO));
                }
                k.all_safe();
                k.hold_active = false;
                k.hold_error = 0;
                k.status.fault_latched = 0;
                k.status.last_error = 0;
                Ok(())
            })
        }
        fn dsp_on(&mut self) -> io::Result<()> {
            self.with(|k| {
                k.calls.push("dsp-on".into());
                k.blocked()?;
                if k.status.fault_latched != 0 {
                    return Err(io::Error::from_raw_os_error(EUCLEAN));
                }
                if k.fail_dsp_on {
                    return Err(io::Error::from_raw_os_error(6));
                }
                k.status.dsp_powered = 1;
                k.status.program_loaded = 1;
                Ok(())
            })
        }
        fn dac_power(&mut self, enabled: bool) -> io::Result<()> {
            self.with(|k| {
                k.calls.push(format!("dac({enabled})"));
                k.blocked()?;
                k.status.dac_enabled = enabled.into();
                Ok(())
            })
        }
        fn amplifier_power(&mut self, enabled: bool) -> io::Result<()> {
            self.with(|k| {
                k.calls.push(format!("amp({enabled})"));
                k.blocked()?;
                k.status.amplifier_enabled = enabled.into();
                Ok(())
            })
        }
        fn source_transition(&mut self, request: SourceTransition) -> io::Result<()> {
            self.with(|k| {
                k.calls.push(format!("source({},{})", request.source, request.phase));
                k.blocked()?;
                if k.status.fault_latched != 0 {
                    return Err(io::Error::from_raw_os_error(EUCLEAN));
                }
                k.status.source = request.source;
                k.status.route_ready = u32::from(request.phase != 1);
                Ok(())
            })
        }
        fn dsp_update(&mut self, request: DspUpdate) -> io::Result<()> {
            self.with(|k| {
                k.calls.push(format!("dsp-update({})", request.operation));
                k.blocked()?;
                if k.status.fault_latched != 0 {
                    return Err(io::Error::from_raw_os_error(EUCLEAN));
                }
                Ok(())
            })
        }
        fn fm_power(&mut self, enabled: bool) -> io::Result<()> {
            self.with(|k| {
                k.calls.push(format!("fm-power({enabled})"));
                k.blocked()?;
                if enabled && k.status.fault_latched != 0 {
                    return Err(io::Error::from_raw_os_error(EUCLEAN));
                }
                k.status.fm_powered = enabled.into();
                Ok(())
            })
        }
        fn fm_mix_transition(&mut self, _request: FmMixTransition) -> io::Result<()> {
            self.with(|k| k.blocked())
        }
        fn fm_attenuation(&mut self, _request: FmAttenuation) -> io::Result<()> {
            self.with(|k| k.blocked())
        }
        // Merge resolution 2026-09-11: fm-level-control (IAudioGuard V3) added the
        // owner-scoped FM mix base ioctl; under a latched ACC hold the kernel answers
        // it EBUSY exactly like the attenuation, so the model does the same.
        fn fm_mix_base(&mut self, _request: FmMixBase) -> io::Result<()> {
            self.with(|k| k.blocked())
        }
    }

    /// Records the FM service calls in order; each can be told to fail.
    #[derive(Default)]
    struct FakeFm {
        calls: Vec<String>,
        fail_restore: bool,
        fail_route_lost: bool,
    }

    impl FmAccParticipant for FakeFm {
        fn acc_release(&mut self, token: u64) -> Result<(), String> {
            self.calls.push(format!("acc-release({token})"));
            if self.fail_restore {
                // Only the first attempt fails: after route loss there is no
                // replay left to fail, which is the FM service's real contract.
                self.fail_restore = false;
                return Err("tuner would not re-enable".into());
            }
            Ok(())
        }
        fn audio_route_lost(&mut self, rails_live: bool) -> Result<(), String> {
            self.calls.push(format!("route-lost({rails_live})"));
            if self.fail_route_lost {
                return Err("tuner disable failed".into());
            }
            Ok(())
        }
        fn tuner_lost(&mut self, reason: &str) {
            if reason == TUNER_RELEASED_REASON {
                self.calls.push("tuner-released".into());
            } else {
                self.calls.push(format!("tuner-lost({})", reason == TUNER_LOST_REASON));
            }
        }
    }

    struct Rig {
        kernel: ModelTransport,
        audio: Mutex<AudioGuard<ModelTransport>>,
        fm: Mutex<FakeFm>,
    }

    /// A booted guard with FM rails up, holding ACC lease `token`.
    fn held(token: u64) -> Rig {
        let kernel = ModelTransport(Arc::new(Mutex::new(KernelModel::booted())));
        kernel.with(|k| k.status.fm_powered = 1);
        let mut guard = AudioGuard::with_transport(kernel.clone());
        guard.acc_operation(token, 1).expect("hold");
        assert!(guard.acc_held());
        Rig { kernel, audio: Mutex::new(guard), fm: Mutex::new(FakeFm::default()) }
    }

    fn eq_write(rig: &Rig) -> Result<(), String> {
        rig.audio.lock().unwrap().set_eq_band_steps(1, 4).map_err(|e| e.to_string())
    }

    #[test]
    fn a_clean_release_needs_no_recovery_and_touches_nothing_else() {
        let rig = held(1);
        let outcome = release_with_recovery(&rig.audio, &rig.fm, 1);
        assert_eq!(outcome, ReleaseOutcome::Released);
        assert!(outcome.released());
        assert_eq!(rig.fm.lock().unwrap().calls, ["acc-release(1)", "tuner-released"]);
        assert!(!rig.kernel.with(|k| k.calls.iter().any(|c| c == "fault-safe")));
        assert!(eq_write(&rig).is_ok());
    }

    /// The measured car/bench failure, reproduced on the model without the
    /// recovery: after the refused RELEASE both the FM rail and an EQ write
    /// are refused, and a retried RELEASE can never succeed. This is the state
    /// only a reboot cleared before.
    #[test]
    fn without_recovery_a_refused_release_locks_fm_and_dsp_for_good() {
        let rig = held(1);
        rig.kernel.with(|k| k.master_changed_across_suspend = true);
        let mut guard = rig.audio.lock().unwrap();
        assert!(guard.acc_operation(1, 2).is_err());
        assert!(guard.acc_held(), "the daemon fence stays up");
        assert!(rig.kernel.with(|k| k.hold_active && k.status.fault_latched == 1));
        assert!(guard.set_fm_power(true).is_err());
        assert!(guard.set_eq_band_steps(1, 4).is_err());
        rig.kernel.with(|k| k.master_changed_across_suspend = false);
        assert!(guard.acc_operation(1, 2).is_err(), "the kernel hold error is sticky");
    }

    #[test]
    fn a_refused_audio_release_recovers_the_guard_and_unlocks_fm_and_dsp() {
        let rig = held(1);
        rig.kernel.with(|k| {
            k.master_changed_across_suspend = true;
            k.calls.clear();
        });
        let outcome = release_with_recovery(&rig.audio, &rig.fm, 1);
        let ReleaseOutcome::Recovered { cause, notes } = &outcome else {
            panic!("expected recovery, got {outcome:?}");
        };
        assert!(cause.starts_with("audio release:"), "{cause}");
        assert!(notes.is_empty(), "{notes:?}");
        assert!(outcome.released());
        // FM is reconciled while its rails are still up, before FAULT_SAFE.
        assert_eq!(
            rig.fm.lock().unwrap().calls,
            ["acc-release(1)", "route-lost(true)", "acc-release(1)", "tuner-lost(true)"]
        );
        // The boot activation, after the terminal safe state, and nothing else.
        assert_eq!(
            rig.kernel.with(|k| k.calls.clone()),
            [
                "acc:1:2",
                "fault-safe",
                "fault-safe",
                "dsp-on",
                "source(1,1)",
                "source(1,2)",
                "dac(true)",
                "amp(true)",
            ]
        );
        let mut guard = rig.audio.lock().unwrap();
        assert!(!guard.acc_held());
        assert!(rig.kernel.with(|k| !k.hold_active && k.status.fault_latched == 0));
        assert_eq!(guard.program_epoch(), 1);
        guard.set_fm_power(true).expect("a new FM entry can raise the rails");
        guard.set_eq_band_steps(1, 4).expect("DSP controls are writable again");
        guard.set_output_mute(DspOutput::FrontLeft, false).expect("outputs too");
    }

    #[test]
    fn a_later_acc_cycle_can_hold_and_release_after_a_recovery() {
        let rig = held(1);
        rig.kernel.with(|k| k.master_changed_across_suspend = true);
        assert!(release_with_recovery(&rig.audio, &rig.fm, 1).released());
        rig.kernel.with(|k| k.master_changed_across_suspend = false);
        rig.audio.lock().unwrap().acc_operation(2, 1).expect("next cycle holds");
        assert_eq!(release_with_recovery(&rig.audio, &rig.fm, 2), ReleaseOutcome::Released);
    }

    #[test]
    fn a_failed_fm_restore_is_recovered_too_and_the_fm_fence_is_cleared() {
        let rig = held(1);
        rig.fm.lock().unwrap().fail_restore = true;
        rig.kernel.with(|k| k.calls.clear());
        let outcome = release_with_recovery(&rig.audio, &rig.fm, 1);
        let ReleaseOutcome::Recovered { cause, .. } = &outcome else {
            panic!("expected recovery, got {outcome:?}");
        };
        assert!(cause.starts_with("FM restore:"), "{cause}");
        // The keyed release was never issued; FAULT_SAFE ended the hold.
        assert!(!rig.kernel.with(|k| k.calls.iter().any(|c| c == "acc:1:2")));
        assert_eq!(rig.fm.lock().unwrap().calls[1..3], ["route-lost(true)", "acc-release(1)"]);
        assert!(!rig.audio.lock().unwrap().acc_held());
        assert!(eq_write(&rig).is_ok());
    }

    /// A HOLD the kernel refused before reserving anything still set the
    /// daemon fence; the later RELEASE then sees no kernel hold (ESTALE). The
    /// same recovery must clear it instead of locking the boot.
    #[test]
    fn a_hold_refused_by_the_kernel_does_not_leave_a_permanent_fence() {
        let kernel = ModelTransport(Arc::new(Mutex::new(KernelModel::booted())));
        kernel.with(|k| k.status.program_loaded = 0);
        let mut guard = AudioGuard::with_transport(kernel.clone());
        assert!(guard.acc_operation(1, 1).is_err(), "EPIPE before reservation");
        assert!(guard.acc_held(), "the issued hold fences writers");
        let rig = Rig { kernel, audio: Mutex::new(guard), fm: Mutex::new(FakeFm::default()) };
        let outcome = release_with_recovery(&rig.audio, &rig.fm, 1);
        assert!(matches!(&outcome, ReleaseOutcome::Recovered { cause, .. } if cause.contains("audio release")));
        assert!(!rig.audio.lock().unwrap().acc_held());
        assert!(eq_write(&rig).is_ok());
    }

    #[test]
    fn if_fault_safe_fails_the_fence_stays_and_the_worker_stays_latched() {
        let rig = held(1);
        rig.kernel.with(|k| {
            k.master_changed_across_suspend = true;
            k.fail_fault_safe = true;
        });
        let outcome = release_with_recovery(&rig.audio, &rig.fm, 1);
        assert!(matches!(outcome, ReleaseOutcome::Failed { .. }), "{outcome:?}");
        assert!(!outcome.released());
        let mut guard = rig.audio.lock().unwrap();
        assert!(guard.acc_held(), "unknown physical state keeps the writer fence");
        assert!(guard.set_eq_band_steps(1, 4).is_err());
        assert!(!rig.kernel.with(|k| k.calls.iter().any(|c| c == "dsp-on")));
    }

    #[test]
    fn a_failed_reactivation_stays_safe_and_says_why() {
        let rig = held(1);
        rig.kernel.with(|k| {
            k.master_changed_across_suspend = true;
            k.fail_dsp_on = true;
        });
        let outcome = release_with_recovery(&rig.audio, &rig.fm, 1);
        let ReleaseOutcome::Failed { recovery, .. } = &outcome else {
            panic!("expected failure, got {outcome:?}");
        };
        assert!(recovery.contains("os error 6"), "{recovery}");
        let mut guard = rig.audio.lock().unwrap();
        let status = guard.status();
        assert!(!status.available);
        assert!(status.unavailable_reason.contains("activation-failed"));
        assert_eq!(rig.kernel.with(|k| k.status.amplifier_enabled), 0);
        assert!(guard.set_eq_band_steps(1, 4).is_err());
    }

    #[test]
    fn fm_reconciliation_problems_are_reported_but_do_not_block_recovery() {
        let rig = held(1);
        rig.kernel.with(|k| k.master_changed_across_suspend = true);
        rig.fm.lock().unwrap().fail_route_lost = true;
        let outcome = release_with_recovery(&rig.audio, &rig.fm, 1);
        let ReleaseOutcome::Recovered { notes, .. } = &outcome else {
            panic!("expected recovery, got {outcome:?}");
        };
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with("FM route-lost:"), "{notes:?}");
        assert!(eq_write(&rig).is_ok());
    }
}
