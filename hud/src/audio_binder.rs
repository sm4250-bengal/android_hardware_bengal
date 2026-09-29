// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Android-only Binder adapter for the daemon-owned physical audio guard.

use crate::callback_registry::{CallbackToken, GenerationRegistry, Publish};
use crate::dsp::guard::{
    service_specific_errno, validate_fm_transition, AudioGuard, Error, KernelTransport, Status,
    Transport,
};
use binder::binder_impl::IBinderInternal;
use binder::{
    BinderFeatures, DeathRecipient, ExceptionCode, IBinder, Interface, SpIBinder,
    Status as BinderStatus,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use vendor_xyauto_audio::aidl::vendor::xyauto::audio::{
    AudioGuardStatus::AudioGuardStatus as AidlAudioGuardStatus,
    IAudioGuard::{BnAudioGuard, IAudioGuard},
};

/// Narrow callback used to reconcile tuner/seek/RDS state around rail loss.
pub type FmCleanup = Arc<dyn Fn(bool) -> Result<(), String> + Send + Sync>;

/// Stable service-manager name declared with the daemon's VINTF fragment.
pub const AUDIO_GUARD_SERVICE_NAME: &str = "vendor.xyauto.audio.IAudioGuard/default";

/// Register the one typed physical-audio service.
pub fn register(guard: Arc<Mutex<AudioGuard>>, fm_cleanup: FmCleanup) -> binder::Result<()> {
    let leases = Arc::new(FmMixLeaseRegistry::new(Arc::clone(&guard), fm_cleanup));
    let service = BnAudioGuard::new_binder(
        BinderAudioGuard { guard, leases },
        BinderFeatures::default(),
    );
    Ok(binder::add_service(
        AUDIO_GUARD_SERVICE_NAME,
        service.as_binder(),
    )?)
}

struct BinderAudioGuard<T: Transport = KernelTransport> {
    guard: Arc<Mutex<AudioGuard<T>>>,
    leases: Arc<FmMixLeaseRegistry<T>>,
}

impl<T: Transport + Send + 'static> Interface for BinderAudioGuard<T> {}

impl<T: Transport + Send + 'static> BinderAudioGuard<T> {
    fn lock(&self) -> binder::Result<MutexGuard<'_, AudioGuard<T>>> {
        self.guard.lock().map_err(|_| lock_error())
    }
}

struct FmMixLeaseSlot {
    binder: SpIBinder,
    death: DeathRecipient,
}

#[derive(Default)]
struct FmMixLeaseState {
    current: GenerationRegistry<FmMixLeaseSlot>,
    linking: Vec<CallbackToken>,
    next_teardown: u64,
    teardown: Option<FmMixTeardown>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FmMixTeardown {
    id: u64,
    owner: Option<CallbackToken>,
    generation: Option<u64>,
}

impl FmMixLeaseState {
    fn reserve(&mut self, identity: u64) -> Option<CallbackToken> {
        if self.teardown.is_some() || self.current.current().is_some() {
            return None;
        }
        let token = self.current.reserve(identity);
        self.linking.push(token);
        Some(token)
    }

    fn finish_link(&mut self, token: CallbackToken) -> bool {
        let linked = self.linking.contains(&token);
        self.linking.retain(|candidate| *candidate != token);
        linked
    }

    fn died(&mut self, token: CallbackToken) -> Option<FmMixTeardown> {
        self.linking.retain(|candidate| *candidate != token);
        self.start_close(token)
    }

    fn start_close(&mut self, token: CallbackToken) -> Option<FmMixTeardown> {
        if self.teardown.is_some()
            || !self
                .current
                .current()
                .is_some_and(|entry| entry.token == token)
        {
            return None;
        }
        self.start_route(Some(token.generation))
    }

    fn start_route(&mut self, orphan_generation: Option<u64>) -> Option<FmMixTeardown> {
        if self.teardown.is_some() {
            return None;
        }
        self.next_teardown = self.next_teardown.wrapping_add(1).max(1);
        let owner = self.current.current().map(|entry| entry.token);
        let teardown = FmMixTeardown {
            id: self.next_teardown,
            owner,
            generation: owner.map(|token| token.generation).or(orphan_generation),
        };
        // A route/rail operation that races a link invalidates that unpublished
        // claim. The caller that owns the linked Binder will unlink it later.
        self.linking.clear();
        self.teardown = Some(teardown);
        Some(teardown)
    }

    fn finish_teardown(&mut self, teardown: FmMixTeardown) -> Option<FmMixLeaseSlot> {
        if self.teardown != Some(teardown) {
            return None;
        }
        self.teardown = None;
        teardown.owner.and_then(|token| {
            self.current
                .remove_if_current(token)
                .map(|entry| entry.value)
        })
    }
}

/// Private playback-owner slot. Binder link/unlink calls occur outside locks.
struct FmMixLeaseRegistry<T: Transport> {
    next_identity: AtomicU64,
    state: Mutex<FmMixLeaseState>,
    guard: Weak<Mutex<AudioGuard<T>>>,
    fm_cleanup: FmCleanup,
}

impl<T: Transport + Send + 'static> FmMixLeaseRegistry<T> {
    fn new(guard: Arc<Mutex<AudioGuard<T>>>, fm_cleanup: FmCleanup) -> Self {
        Self {
            next_identity: AtomicU64::new(1),
            state: Mutex::new(FmMixLeaseState::default()),
            guard: Arc::downgrade(&guard),
            fm_cleanup,
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, FmMixLeaseState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn cancel_link(&self, token: CallbackToken) {
        self.lock_state()
            .linking
            .retain(|candidate| *candidate != token);
    }

    fn on_death(&self, token: CallbackToken) {
        let teardown = {
            let mut state = self.lock_state();
            state.died(token)
        };
        let Some(teardown) = teardown else {
            return;
        };
        if let Err(error) = self.close_started(teardown) {
            eprintln!("audio guard owner-death close failed: {error}");
        }
    }

    fn cleanup_fm_branch(&self, generation: Option<u64>) -> Result<(), String> {
        let mut failures = Vec::new();
        let mut rails_live = false;
        if let Some(guard) = self.guard.upgrade() {
            #[cfg(feature = "acc-power-channel")]
            {
                let mut audio = guard.lock().unwrap_or_else(|p| p.into_inner());
                if audio.acc_held() {
                    // The keyed kernel hold already closes every output. Retire
                    // this user's replay intent through the real FM owner now;
                    // defer route/rail changes until native keyed release.
                    audio.acc_defer_fm_cleanup();
                    let powered = audio.fm_rails_powered().map_err(|e| e.to_string())?;
                    drop(audio);
                    return (self.fm_cleanup)(powered);
                }
            }
            {
                let mut guard = guard
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(generation) = generation {
                    if let Err(error) = guard.silence_fm_mix(generation) {
                        failures.push(format!("silence: {error}"));
                    }
                }
                match guard.fm_rails_powered() {
                    Ok(powered) => rails_live = powered,
                    Err(error) => failures.push(format!("rail status: {error}")),
                }
            }
            if let Err(error) = (self.fm_cleanup)(rails_live) {
                failures.push(format!("tuner cleanup: {error}"));
            }
            if let Err(error) = guard
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .finish_fm_mix_release()
            {
                failures.push(format!("rail off: {error}"));
            }
        } else {
            failures.push("audio guard owner disappeared".to_owned());
            if let Err(error) = (self.fm_cleanup)(false) {
                failures.push(format!("bus-free tuner cleanup: {error}"));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    fn close_started(&self, teardown: FmMixTeardown) -> Result<(), String> {
        let result = self.cleanup_fm_branch(teardown.generation);
        let retired = self.lock_state().finish_teardown(teardown);
        Self::unlink_slot(retired);
        result
    }

    fn close(&self, token: CallbackToken) -> Result<(), String> {
        let Some(teardown) = self.lock_state().start_close(token) else {
            return Ok(());
        };
        self.close_started(teardown)
    }

    fn current_token(&self) -> Option<CallbackToken> {
        self.lock_state().current.current().map(|entry| entry.token)
    }

    fn close_generation(&self, generation: u64) -> Result<(), String> {
        match self
            .current_token()
            .filter(|token| token.generation == generation)
        {
            Some(token) => self.close(token),
            None => Ok(()),
        }
    }

    fn rail_off_without_owner(&self) -> Result<(), String> {
        let teardown = self
            .lock_state()
            .start_route(None)
            .ok_or_else(|| "an audio route teardown is already active".to_owned())?;
        self.close_started(teardown)?;
        #[cfg(feature = "acc-power-channel")]
        {
            // Explicit setFmPower(false) promises a physical result, unlike
            // internal lease retirement. An ACC hold may defer that rail edge.
            let guard = self.guard.upgrade().ok_or("audio guard owner disappeared")?;
            if guard.lock().map_err(|_| "audio guard mutex poisoned")?
                .fm_rails_powered().map_err(|e| e.to_string())? {
                return Err("ACC hold deferred physical FM rail-off".into());
            }
        }
        Ok(())
    }

    fn fenced_guard_operation<F>(&self, operation: F) -> binder::Result<()>
    where
        F: FnOnce(&mut AudioGuard<T>) -> Result<(), Error>,
    {
        let teardown = self.lock_state().start_route(None).ok_or_else(|| {
            BinderStatus::new_exception_str(
                ExceptionCode::ILLEGAL_STATE,
                Some("an audio route teardown is already active"),
            )
        })?;
        let cleanup = if teardown.owner.is_some() {
            self.cleanup_fm_branch(teardown.generation)
        } else {
            Ok(())
        };
        if let Err(error) = cleanup {
            let retired = self.lock_state().finish_teardown(teardown);
            Self::unlink_slot(retired);
            return Err(BinderStatus::new_exception_str(
                ExceptionCode::ILLEGAL_STATE,
                Some(format!("cleanup: {error}")),
            ));
        }
        let operation_result = match self.guard.upgrade() {
            Some(guard) => operation(
                &mut guard
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            ),
            None => Err(Error::InvalidState("audio guard owner disappeared")),
        };
        let retired = self.lock_state().finish_teardown(teardown);
        Self::unlink_slot(retired);
        operation_result.map_err(binder_error)
    }

    /// Run a legacy PREPARE as one fenced transaction. A failed mixed-owner
    /// cleanup stops before the destination call, so an error can never leave
    /// a newly published PREPARE behind. A failed FM destination is returned
    /// to the already stable Android route with its temporary rail removed.
    fn fenced_prepare_operation<F>(&self, rearm_fm: bool, operation: F) -> binder::Result<()>
    where
        F: FnOnce(&mut AudioGuard<T>) -> Result<(), Error>,
    {
        let teardown = self.lock_state().start_route(None).ok_or_else(|| {
            BinderStatus::new_exception_str(
                ExceptionCode::ILLEGAL_STATE,
                Some("an audio route teardown is already active"),
            )
        })?;
        let had_mixed_owner = teardown.owner.is_some();
        let cleanup = if had_mixed_owner {
            self.cleanup_fm_branch(teardown.generation)
        } else {
            Ok(())
        };
        if let Err(error) = cleanup {
            let retired = self.lock_state().finish_teardown(teardown);
            Self::unlink_slot(retired);
            return Err(BinderStatus::new_exception_str(
                ExceptionCode::ILLEGAL_STATE,
                Some(format!("cleanup: {error}")),
            ));
        }
        let mut recovery_failures = Vec::new();
        let operation_result = match self.guard.upgrade() {
            Some(guard) => {
                let mut guard = guard
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let result = if rearm_fm && had_mixed_owner {
                    guard
                        .set_fm_power(true)
                        .and_then(|()| operation(&mut guard))
                } else {
                    operation(&mut guard)
                };
                #[cfg(feature = "acc-power-channel")]
                let recover = !guard.acc_held();
                #[cfg(not(feature = "acc-power-channel"))]
                let recover = true;
                if result.is_err() && recover {
                    if let Err(error) = guard.transition_to_android() {
                        recovery_failures.push(format!("rollback route: {error}"));
                        match guard.force_fault_safe() {
                            Ok(()) => recovery_failures
                                .push("forced fail-safe after rollback failure".to_owned()),
                            Err(error) => {
                                recovery_failures.push(format!("forced fail-safe: {error}"));
                            }
                        }
                    }
                    if rearm_fm {
                        if let Err(error) = guard.finish_fm_mix_release() {
                            recovery_failures.push(format!("rollback rail off: {error}"));
                        }
                    }
                }
                result
            }
            None => Err(Error::InvalidState("audio guard owner disappeared")),
        };
        let retired = self.lock_state().finish_teardown(teardown);
        Self::unlink_slot(retired);

        if let Err(error) = operation_result {
            return Err(binder_error_with_diagnostics(error, &recovery_failures));
        }
        Ok(())
    }

    fn unlink_slot(slot: Option<FmMixLeaseSlot>) {
        let Some(mut slot) = slot else {
            return;
        };
        if slot.binder.is_binder_alive() {
            if let Err(error) = slot.binder.unlink_to_death(&mut slot.death) {
                eprintln!("audio guard owner unlink failed: {error:?}");
            }
        }
    }

    fn claim_and_prepare(
        self: &Arc<Self>,
        owner: &SpIBinder,
        guard: &Arc<Mutex<AudioGuard<T>>>,
    ) -> binder::Result<()> {
        let token = loop {
            let mut state = self.lock_state();
            if state.teardown.is_some() {
                return Err(BinderStatus::new_exception_str(
                    ExceptionCode::ILLEGAL_STATE,
                    Some("an audio route teardown is active"),
                ));
            }
            let active = guard
                .lock()
                .map_err(|_| lock_error())?
                .active_fm_mix_generation();
            match (state.current.current().map(|entry| entry.token), active) {
                (Some(current), Ok(Some(generation))) if current.generation == generation => {
                    return Err(BinderStatus::new_exception_str(
                        ExceptionCode::ILLEGAL_STATE,
                        Some("an FM mix owner is already active"),
                    ));
                }
                (current, Ok(active_generation))
                    if current.is_some() || active_generation.is_some() =>
                {
                    let teardown = state
                        .start_route(active_generation)
                        .expect("teardown was checked above");
                    drop(state);
                    self.close_started(teardown).map_err(|error| {
                        BinderStatus::new_exception_str(ExceptionCode::ILLEGAL_STATE, Some(error))
                    })?;
                }
                (Some(_), Err(error)) => {
                    let teardown = state.start_route(None).expect("teardown was checked above");
                    drop(state);
                    if let Err(cleanup) = self.close_started(teardown) {
                        eprintln!("audio guard stale-owner cleanup failed: {cleanup}");
                    }
                    return Err(binder_error(error));
                }
                (None, Err(error)) => return Err(binder_error(error)),
                (None, Ok(None)) => {
                    let identity = self.next_identity.fetch_add(1, Ordering::Relaxed);
                    break state
                        .reserve(identity)
                        .expect("claim state changed while locked");
                }
                _ => unreachable!("FM mix claim state was exhaustively classified"),
            }
        };
        let leases = Arc::downgrade(self);
        let mut death = DeathRecipient::new(move || {
            if let Some(leases) = Weak::upgrade(&leases) {
                leases.on_death(token);
            }
        });
        let mut owner_binder = owner.clone();
        if let Err(error) = owner_binder.link_to_death(&mut death) {
            self.cancel_link(token);
            return Err(error.into());
        }
        let candidate = FmMixLeaseSlot {
            binder: owner_binder,
            death,
        };

        // Publication and local PREPARE form one registry critical section.
        let (result, dispose, cleanup) = {
            let mut state = self.lock_state();
            let linked = state.finish_link(token);
            if !linked || state.teardown.is_some() || state.current.current().is_some() {
                (
                    Err(Error::InvalidState("FM mix owner lost its claim")),
                    Some(candidate),
                    None,
                )
            } else {
                match state.current.publish(token, candidate) {
                    Publish::Published(None) => {
                        let result = guard
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .prepare_fm_mix(token.generation);
                        if result.is_err() {
                            let teardown = state.start_close(token);
                            (result, None, teardown)
                        } else {
                            (Ok(()), None, None)
                        }
                    }
                    Publish::Published(Some(_)) => unreachable!("active FM lease was replaced"),
                    Publish::Superseded(candidate) => (
                        Err(Error::InvalidState("FM mix owner lost its claim")),
                        Some(candidate.value),
                        None,
                    ),
                }
            }
        };
        Self::unlink_slot(dispose);
        if let Some(teardown) = cleanup {
            if let Err(error) = self.close_started(teardown) {
                eprintln!("audio guard failed-PREPARE cleanup failed: {error}");
            }
        }
        result.map_err(binder_error)
    }

    fn current_generation(&self, owner: &SpIBinder) -> binder::Result<u64> {
        let state = self.lock_state();
        state
            .current
            .current()
            .filter(|_| state.teardown.is_none())
            .filter(|entry| entry.value.binder == *owner)
            .map(|entry| entry.token.generation)
            .ok_or_else(|| {
                BinderStatus::new_exception_str(
                    ExceptionCode::ILLEGAL_STATE,
                    Some("FM mix owner is not current"),
                )
            })
    }
}

/// Execute generation/death ordering controls with the desk Binder fixture.
#[cfg(feature = "binder-desk")]
pub fn audio_binder_desk_semantic_controls() {
    use std::sync::Condvar;
    use std::thread;

    #[cfg(feature = "acc-power-channel")]
    {
        let (guard, leases, shared) = lease_desk_active_mix(false, false);
        shared.lock().unwrap().status.reserved[0] |= crate::dsp::guard::FEATURE_ACC_HOLD;
        guard.lock().unwrap().acc_operation(1, 1).unwrap();
        let adapter = BinderAudioGuard { guard: guard.clone(), leases: leases.clone() };
        let owner = leases.lock_state().current.current().unwrap().value.binder.clone();
        let original_token = leases.current_token();
        adapter.setFmAttenuation(&owner, 9600).expect_err("held gain is not applied");
        assert_eq!(leases.current_token(), original_token,
                   "ACC-held gain refusal must preserve the live FM request");
        adapter.setFmMixBaseLevel(&owner, 80).expect_err("held base is not applied");
        assert_eq!(leases.current_token(), original_token);
        adapter.setFmPower(false).expect_err("deferred physical rail-off must not return success");
        assert_eq!(shared.lock().unwrap().status.fm_powered, 1);
        leases.cleanup_fm_branch(None).expect("logical lease retirement remains valid");
        guard.lock().unwrap().acc_operation(1, 2).unwrap();
        assert_eq!(shared.lock().unwrap().status.fm_powered, 0);
        adapter.setFmPower(false).expect("actual rail-off remains successful");
    }

    let endpoint = crate::Endpoint::new();
    let binder = endpoint.binder();
    let mut state = FmMixLeaseState::default();

    // A transport-level Binder link failure crosses into the AIDL Status
    // family, but must still cancel the unpublished lease reservation first.
    let (link_transport, _link_state) = LeaseDeskTransport::ready();
    let link_guard = Arc::new(Mutex::new(AudioGuard::with_transport(link_transport)));
    let link_registry = Arc::new(FmMixLeaseRegistry::new(
        Arc::clone(&link_guard),
        Arc::new(|_| Ok(())),
    ));
    let failed_endpoint = crate::Endpoint::new();
    failed_endpoint.fail_next_link();
    link_registry
        .claim_and_prepare(&failed_endpoint.binder(), &link_guard)
        .expect_err("configured owner link failure");
    let state_after_link = link_registry.lock_state();
    assert!(state_after_link.linking.is_empty());
    assert!(state_after_link.current.current().is_none());
    assert!(state_after_link.teardown.is_none());
    assert_eq!(failed_endpoint.links(), 1);
    assert_eq!(failed_endpoint.live_deaths(), 0);
    drop(state_after_link);

    let during_link = state.reserve(1).expect("initial claim");
    assert!(state.died(during_link).is_none());
    assert!(!state.finish_link(during_link));
    assert!(state.current.current().is_none());

    let a = state.reserve(2).expect("owner A claim");
    assert!(state.finish_link(a));
    let slot_a = FmMixLeaseSlot {
        binder: binder.clone(),
        death: DeathRecipient::new(|| {}),
    };
    assert!(matches!(
        state.current.publish(a, slot_a),
        Publish::Published(None)
    ));
    let close_a = state.died(a).expect("owner A close");
    assert!(state.reserve(3).is_none(), "claim escaped CLOSING fence");
    assert!(state.finish_teardown(close_a).is_some());

    let b = state.reserve(3).expect("owner B claim");
    assert!(state.finish_link(b));
    let slot_b = FmMixLeaseSlot {
        binder,
        death: DeathRecipient::new(|| {}),
    };
    assert!(matches!(
        state.current.publish(b, slot_b),
        Publish::Published(None)
    ));
    assert!(
        state.died(a).is_none(),
        "late death from generation A started closing B"
    );
    assert_eq!(state.current.current().map(|entry| entry.token), Some(b));

    let close_b = state.start_route(None).expect("route close B");
    assert!(
        state.reserve(4).is_none(),
        "B replacement escaped route fence"
    );
    assert!(state.finish_teardown(close_b).is_some());
    let c = state.reserve(4).expect("owner C claim after route");
    assert!(state.finish_link(c));
    assert!(
        state.finish_teardown(close_b).is_none(),
        "late A/B teardown retired the new generation"
    );

    // Exercise the actual registry adapter, not only its state container: a
    // no-owner final route operation holds the fence for the entire closure.
    let guard = Arc::new(Mutex::new(AudioGuard::<KernelTransport>::unavailable(
        "desk",
    )));
    let registry = Arc::new(FmMixLeaseRegistry::new(
        Arc::clone(&guard),
        Arc::new(|_| Ok(())),
    ));
    let latch = Arc::new((Mutex::new((false, false)), Condvar::new()));
    let worker_registry = Arc::clone(&registry);
    let worker_latch = Arc::clone(&latch);
    let worker = thread::spawn(move || {
        worker_registry.fenced_guard_operation(|_| {
            let (lock, ready) = &*worker_latch;
            let mut flags = lock.lock().expect("latch lock");
            flags.0 = true;
            ready.notify_all();
            while !flags.1 {
                flags = ready.wait(flags).expect("latch wait");
            }
            Ok(())
        })
    });
    {
        let (lock, ready) = &*latch;
        let mut flags = lock.lock().expect("latch lock");
        while !flags.0 {
            flags = ready.wait(flags).expect("latch wait");
        }
        assert!(
            registry.lock_state().reserve(5).is_none(),
            "claim published before the final legacy transition completed"
        );
        flags.1 = true;
        ready.notify_all();
    }
    assert!(worker.join().expect("route worker").is_ok());
    let after_route = registry
        .lock_state()
        .reserve(5)
        .expect("claim after final route");
    registry.cancel_link(after_route);

    // The no-owner rail-off path uses the same fence while tuner cleanup and
    // the final FM_OFF are outstanding; otherwise PREPARE could publish in
    // the middle and immediately lose its freshly enabled rails.
    let rail_latch = Arc::new((Mutex::new((false, false)), Condvar::new()));
    let cleanup_latch = Arc::clone(&rail_latch);
    let rail_guard = Arc::new(Mutex::new(AudioGuard::<KernelTransport>::unavailable(
        "desk",
    )));
    let rail_registry = Arc::new(FmMixLeaseRegistry::new(
        Arc::clone(&rail_guard),
        Arc::new(move |_| {
            let (lock, ready) = &*cleanup_latch;
            let mut flags = lock.lock().expect("rail latch lock");
            flags.0 = true;
            ready.notify_all();
            while !flags.1 {
                flags = ready.wait(flags).expect("rail latch wait");
            }
            Ok(())
        }),
    ));
    let worker_registry = Arc::clone(&rail_registry);
    let rail_worker = thread::spawn(move || worker_registry.rail_off_without_owner());
    {
        let (lock, ready) = &*rail_latch;
        let mut flags = lock.lock().expect("rail latch lock");
        while !flags.0 {
            flags = ready.wait(flags).expect("rail latch wait");
        }
        assert!(
            rail_registry.lock_state().reserve(6).is_none(),
            "claim published during a no-owner FM rail-off"
        );
        flags.1 = true;
        ready.notify_all();
    }
    assert!(
        rail_worker.join().expect("rail worker").is_err(),
        "unavailable desk guard unexpectedly completed FM_OFF"
    );
    let after_rail_off = rail_registry
        .lock_state()
        .reserve(6)
        .expect("claim after rail-off fence");
    rail_registry.cancel_link(after_rail_off);

    // Invalid legacy FM values are pure validation failures. They must be
    // rejected before the route fence, whether legacy FM currently has no V2
    // owner or a live mixed owner would otherwise be torn down.
    let invalid_fm = [(-1, 27), (62, 27), (0, -1), (0, 101)];
    for (master_index, fm_mix_level) in invalid_fm {
        let (transport, shared) = LeaseDeskTransport::ready();
        {
            let mut state = shared.lock().expect("desk state lock");
            state.status.source = 2;
            state.status.transition_pending = 0;
            state.calls.clear();
        }
        let guard = Arc::new(Mutex::new(AudioGuard::with_transport(transport)));
        let registry = Arc::new(FmMixLeaseRegistry::new(
            Arc::clone(&guard),
            Arc::new(|_| Ok(())),
        ));
        let adapter = BinderAudioGuard {
            guard,
            leases: Arc::clone(&registry),
        };
        let before = shared.lock().expect("desk state lock").status;
        let error = adapter
            .prepareFm(master_index, fm_mix_level)
            .expect_err("invalid no-owner FM PREPARE");
        assert_eq!(error.exception_code(), ExceptionCode::ILLEGAL_ARGUMENT);
        let state = shared.lock().expect("desk state lock");
        assert_eq!(state.status, before);
        assert!(
            state.calls.is_empty(),
            "invalid call touched guard: {:?}",
            state.calls
        );
        assert!(registry.current_token().is_none());
    }
    for (master_index, fm_mix_level) in invalid_fm {
        let (guard, registry, shared) = lease_desk_active_mix(false, false);
        let adapter = BinderAudioGuard {
            guard,
            leases: Arc::clone(&registry),
        };
        let owner_before = registry
            .current_token()
            .expect("mixed owner before invalid call");
        let state = shared.lock().expect("desk state lock");
        let status_before = state.status;
        let calls_before = state.calls.len();
        drop(state);
        let error = adapter
            .prepareFm(master_index, fm_mix_level)
            .expect_err("invalid mixed-owner FM PREPARE");
        assert_eq!(error.exception_code(), ExceptionCode::ILLEGAL_ARGUMENT);
        let state = shared.lock().expect("desk state lock");
        assert_eq!(state.status, status_before);
        assert_eq!(
            state.calls.len(),
            calls_before,
            "invalid mixed-owner call touched hardware"
        );
        assert_eq!(registry.current_token(), Some(owner_before));
    }

    // End-to-end through the real guard, registry and FmService state
    // machines. The legacy app enters in this order: rails on, tuner PREPARE
    // (tune), DSP PREPARE, DSP COMMIT, tuner COMMIT (enable). During a V2-to-V1
    // handoff the cleanup between the two PREPARE calls must preserve the
    // newly tuned frequency so tuner COMMIT can reinitialize the powered-down
    // backend at that exact station.
    let (transport, shared) = LeaseDeskTransport::ready();
    let mixed_guard = Arc::new(Mutex::new(AudioGuard::with_transport(transport)));
    let fm = Arc::new(Mutex::new(xyauto_fm::service::FmService::new(
        LeaseDeskFmHardware {
            shared: Arc::clone(&shared),
        },
    )));
    fm.lock()
        .expect("FM service lock")
        .tune(97_500)
        .expect("initial tuner prepare");
    fm.lock()
        .expect("FM service lock")
        .set_enabled(true)
        .expect("initial tuner commit");
    let cleanup_fm = Arc::clone(&fm);
    let mixed_registry = Arc::new(FmMixLeaseRegistry::new(
        Arc::clone(&mixed_guard),
        Arc::new(move |rails_live| {
            cleanup_fm
                .lock()
                .map_err(|_| "FM service mutex poisoned".to_owned())?
                .audio_route_lost(rails_live)
                .map_err(|error| error.to_string())
        }),
    ));
    let mixed_owner = crate::Endpoint::new().binder();
    mixed_registry
        .claim_and_prepare(&mixed_owner, &mixed_guard)
        .expect("mixed prepare");
    let mixed_generation = mixed_registry
        .current_generation(&mixed_owner)
        .expect("mixed generation");
    mixed_guard
        .lock()
        .expect("mixed guard lock")
        .commit_fm_mix(mixed_generation)
        .expect("mixed commit");

    // Exact V1 prefix before its DSP PREPARE: the rail call is idempotent and
    // the enabled service applies tune immediately, updating its remembered
    // frequency before mixed-owner cleanup resets the backend.
    mixed_guard
        .lock()
        .expect("mixed guard lock")
        .set_fm_power(true)
        .expect("V1 caller raised FM rails");
    fm.lock()
        .expect("FM service lock")
        .tune(99_900)
        .expect("V1 tuner prepare");
    assert!(
        mixed_registry
            .fenced_prepare_operation(true, |guard| guard.prepare_fm(0, 27))
            .is_ok(),
        "V2-to-V1 FM handoff lost its rail prerequisite: {:?}",
        shared.lock().expect("desk state lock").calls
    );
    let after_cleanup = fm.lock().expect("FM service lock").get_state();
    assert!(!after_cleanup.enabled);
    assert_eq!(after_cleanup.frequency_khz, 99_900);
    mixed_guard
        .lock()
        .expect("mixed guard lock")
        .commit_fm(0, 27)
        .expect("V1 DSP commit");
    fm.lock()
        .expect("FM service lock")
        .set_enabled(true)
        .expect("V1 tuner commit");
    let after_commit = fm.lock().expect("FM service lock").get_state();
    assert!(after_commit.enabled);
    assert_eq!(after_commit.frequency_khz, 99_900);
    let shared_state = shared.lock().expect("desk state lock");
    assert_eq!(shared_state.status.fm_powered, 1);
    assert_eq!(shared_state.status.source, 2);
    assert_eq!(shared_state.status.transition_pending, 0);
    assert_eq!(shared_state.status.route_ready, 1);
    let calls = &shared_state.calls;
    let rail_off = calls
        .iter()
        .position(|call| call == "fm-power(false)")
        .unwrap();
    let tuner_prepare = calls
        .iter()
        .position(|call| call == "tuner-tune(99900)")
        .unwrap();
    let tuner_cleanup = calls
        .iter()
        .position(|call| call == "tuner-enabled(false,None)")
        .unwrap();
    let tuner_reset = calls
        .iter()
        .position(|call| call == "tuner-power-lost")
        .unwrap();
    let rail_on = calls
        .iter()
        .rposition(|call| call == "fm-power(true)")
        .unwrap();
    let fm_prepare = calls
        .iter()
        .rposition(|call| call == "source(source=2,phase=1)")
        .unwrap();
    let fm_commit = calls
        .iter()
        .rposition(|call| call == "source(source=2,phase=2)")
        .unwrap();
    let tuner_commit = calls
        .iter()
        .rposition(|call| call == "tuner-enabled(true,Some(99900))")
        .unwrap();
    assert!(
        tuner_prepare < tuner_cleanup
            && tuner_cleanup < tuner_reset
            && tuner_reset < rail_off
            && rail_off < rail_on
            && rail_on < fm_prepare
            && fm_prepare < fm_commit
            && fm_commit < tuner_commit
    );
    drop(shared_state);
    drop(mixed_guard);

    // A cleanup failure short-circuits even a destination PREPARE that would
    // succeed. It therefore cannot publish a pending route while returning an
    // error, and the cleanup error remains the primary diagnostic.
    let (_guard, registry, shared) = lease_desk_active_mix(true, false);
    let error = registry
        .fenced_prepare_operation(false, AudioGuard::prepare_android)
        .expect_err("cleanup failure must fail the transaction");
    assert!(error.to_string().contains("cleanup: rail off"));
    let shared = shared.lock().expect("desk state lock");
    assert_eq!(shared.status.transition_pending, 0);
    assert_eq!(
        shared.calls.last().map(String::as_str),
        Some("fm-power(false)")
    );
    assert!(!shared
        .calls
        .iter()
        .any(|call| call == "source(source=1,phase=1)"));
    drop(shared);

    // The same fail-fast boundary applies when the destination transport is
    // configured to fail. It is not invoked, so it cannot mask cleanup or
    // publish anything before the lease generation is retired.
    let (_guard, registry, shared) = lease_desk_active_mix(true, true);
    let error = registry
        .fenced_prepare_operation(false, AudioGuard::prepare_android)
        .expect_err("cleanup must stop before the configured destination failure");
    assert!(error.to_string().contains("cleanup: rail off"));
    let shared = shared.lock().expect("desk state lock");
    assert_eq!(shared.status.transition_pending, 0);
    assert_eq!(
        shared.calls.last().map(String::as_str),
        Some("fm-power(false)")
    );
    assert!(!shared
        .calls
        .iter()
        .any(|call| call == "source(source=1,phase=1)"));
    drop(shared);

    // If cleanup succeeds but the destination PREPARE fails, retain its raw
    // errno and finish the synchronous rollback before releasing the fence.
    let (_guard, registry, shared) = lease_desk_active_mix(false, true);
    let error = registry
        .fenced_prepare_operation(false, AudioGuard::prepare_android)
        .expect_err("destination failure must fail the transaction");
    assert_eq!(error.service_specific_error(), -5);
    assert!(error.to_string().contains("audio guard ioctl failed"));
    let shared = shared.lock().expect("desk state lock");
    assert_eq!(shared.status.transition_pending, 0);
    assert_eq!(
        shared.calls.last().map(String::as_str),
        Some("source(source=1,phase=0)")
    );
    drop(shared);

    // A failed atomic rollback escalates to the guard's complete fail-safe;
    // the returned text distinguishes the destination, rollback and forced
    // recovery instead of implying that the atomic route succeeded.
    let (_guard, registry, shared) = lease_desk_active_mix(false, true);
    let fail_rollback = Arc::clone(&shared);
    let error = registry
        .fenced_prepare_operation(false, move |guard| {
            fail_rollback
                .lock()
                .expect("desk state lock")
                .fail_android_atomic = true;
            guard.prepare_android()
        })
        .expect_err("destination and rollback failure must force safe");
    let diagnostic = error.to_string();
    assert_eq!(error.service_specific_error(), -5);
    assert!(diagnostic.contains("audio guard ioctl failed"));
    assert!(diagnostic.contains("rollback route"));
    assert!(diagnostic.contains("forced fail-safe after rollback failure"));
    let shared = shared.lock().expect("desk state lock");
    assert_eq!(shared.status.fm_powered, 0);
    assert_eq!(shared.status.source, 1);
    assert_eq!(shared.status.transition_pending, 0);
    assert_eq!(shared.calls.last().map(String::as_str), Some("fault-safe"));
}

/// Map a guarded-operation-plus-tell result back onto the existing contract.
///
/// A poisoned mutex is the same `lock_error()` the direct `self.lock()?` path
/// produced, and a guard error keeps `binder_error`'s mapping, so adding the
/// tell changed no caller-visible status.
fn tell_error(error: crate::dsp::tells::TellError) -> BinderStatus {
    match error {
        crate::dsp::tells::TellError::Poisoned => lock_error(),
        crate::dsp::tells::TellError::Guard(inner) => binder_error(inner),
    }
}

fn lock_error() -> BinderStatus {
    BinderStatus::new_exception_str(
        ExceptionCode::ILLEGAL_STATE,
        Some("audio guard owner mutex poisoned"),
    )
}

impl<T: Transport + Send + 'static> IAudioGuard for BinderAudioGuard<T> {
    fn getStatus(&self) -> binder::Result<AidlAudioGuardStatus> {
        Ok(aidl_status(self.lock()?.status()))
    }

    fn setMasterVolumeIndex(&self, master_index: i32) -> binder::Result<()> {
        self.lock()?
            .set_master_index(master_index)
            .map_err(binder_error)
    }

    fn setFmPower(&self, enabled: bool) -> binder::Result<()> {
        if enabled {
            self.lock()?.set_fm_power(true).map_err(binder_error)
        } else {
            self.leases.rail_off_without_owner().map_err(|error| {
                BinderStatus::new_exception_str(ExceptionCode::ILLEGAL_STATE, Some(error))
            })
        }
    }

    fn prepareAndroid(&self) -> binder::Result<()> {
        self.leases
            .fenced_prepare_operation(false, AudioGuard::prepare_android)
    }

    fn commitAndroid(&self) -> binder::Result<()> {
        self.lock()?.commit_android().map_err(binder_error)
    }

    fn transitionToAndroid(&self) -> binder::Result<()> {
        self.leases
            .fenced_guard_operation(AudioGuard::transition_to_android)
    }

    fn prepareFm(&self, master_index: i32, fm_mix_level: i32) -> binder::Result<()> {
        validate_fm_transition(master_index, fm_mix_level).map_err(binder_error)?;
        self.leases
            .fenced_prepare_operation(true, |guard| guard.prepare_fm(master_index, fm_mix_level))
    }

    fn commitFm(&self, master_index: i32, fm_mix_level: i32) -> binder::Result<()> {
        self.lock()?
            .commit_fm(master_index, fm_mix_level)
            .map_err(binder_error)
    }

    fn prepareFmMix(&self, owner_token: &SpIBinder) -> binder::Result<()> {
        self.leases.claim_and_prepare(owner_token, &self.guard)
    }

    fn commitFmMix(&self, owner_token: &SpIBinder) -> binder::Result<()> {
        let generation = self.leases.current_generation(owner_token)?;
        // The helper takes the guard lock, issues the ioctl, RELEASES the lock
        // and only then writes one kmsg record saying the commit completed.
        // Ordering and after-success-only are asserted in dsp::tells' tests.
        let result = crate::dsp::tells::commit_fm_mix(
            &self.guard,
            generation,
            &crate::dsp::tells::KmsgTellSink,
        )
        .map_err(tell_error);
        if result.is_err() {
            if let Err(error) = self.leases.close_generation(generation) {
                eprintln!("audio guard commit failure cleanup failed: {error}");
            }
        }
        result
    }

    fn setFmAttenuation(
        &self,
        owner_token: &SpIBinder,
        attenuation_millibels: i32,
    ) -> binder::Result<()> {
        let generation = self.leases.current_generation(owner_token)?;
        // As commitFmMix: the tell is written after the ioctl returns success
        // and outside the guard lock. A refused range or a failed ioctl keeps
        // its existing behaviour and produces no line.
        let operation = crate::dsp::tells::set_fm_attenuation(
            &self.guard,
            generation,
            attenuation_millibels,
            &crate::dsp::tells::KmsgTellSink,
        );
        let preserve_lease = matches!(
            &operation,
            Err(crate::dsp::tells::TellError::Guard(error)) if error.preserves_fm_lease()
        );
        if !preserve_lease && operation.is_err() {
            if let Err(cleanup) = self.leases.close_generation(generation) {
                eprintln!("audio guard attenuation failure cleanup failed: {cleanup}");
            }
        }
        operation.map_err(tell_error)
    }

    fn setFmMixBaseLevel(&self, owner_token: &SpIBinder, level: i32) -> binder::Result<()> {
        let generation = self.leases.current_generation(owner_token)?;
        // As setFmAttenuation: bounded (0..100, the OEM radio-mix domain)
        // before any state, exact owner generation, tell only after success.
        let operation = crate::dsp::tells::set_fm_mix_base_level(
            &self.guard,
            generation,
            level,
            &crate::dsp::tells::KmsgTellSink,
        );
        let preserve_lease = matches!(
            &operation,
            Err(crate::dsp::tells::TellError::Guard(error)) if error.preserves_fm_lease()
        );
        if !preserve_lease && operation.is_err() {
            if let Err(cleanup) = self.leases.close_generation(generation) {
                eprintln!("audio guard base failure cleanup failed: {cleanup}");
            }
        }
        operation.map_err(tell_error)
    }
}

fn binder_error_with_diagnostics(error: Error, diagnostics: &[String]) -> BinderStatus {
    let message = if diagnostics.is_empty() {
        error.to_string()
    } else {
        format!("{}; {}", error, diagnostics.join("; "))
    };
    match error {
        Error::InvalidArgument(_) => {
            BinderStatus::new_exception_str(ExceptionCode::ILLEGAL_ARGUMENT, Some(message))
        }
        Error::InvalidState(_) | Error::DeviceAbsent(_) | Error::NotReady(_) => {
            BinderStatus::new_exception_str(ExceptionCode::ILLEGAL_STATE, Some(message))
        }
        Error::Io(error) => BinderStatus::new_service_specific_error_str(
            service_specific_errno(&error),
            Some(message),
        ),
    }
}

fn binder_error(error: Error) -> BinderStatus {
    binder_error_with_diagnostics(error, &[])
}

fn aidl_status(status: Status) -> AidlAudioGuardStatus {
    let raw = status.raw.unwrap_or_default();
    AidlAudioGuardStatus {
        available: status.available,
        unavailableReason: status.unavailable_reason,
        abiVersion: i32::try_from(raw.abi_version).unwrap_or(i32::MAX),
        dspPowered: raw.dsp_powered != 0,
        programLoaded: raw.program_loaded != 0,
        fmPowered: raw.fm_powered != 0,
        dacEnabled: raw.dac_enabled != 0,
        routeReady: raw.route_ready != 0,
        amplifierEnabled: raw.amplifier_enabled != 0,
        source: i32::try_from(raw.source).unwrap_or(i32::MAX),
        faultLatched: raw.fault_latched != 0,
        lastError: raw.last_error,
        transitionPending: raw.transition_pending != 0,
    }
}

#[cfg(feature = "binder-desk")]
#[derive(Default)]
struct LeaseDeskState {
    status: crate::dsp::guard::RawStatus,
    calls: Vec<String>,
    fail_fm_off: bool,
    fail_android_prepare: bool,
    fail_android_atomic: bool,
}

#[cfg(feature = "binder-desk")]
struct LeaseDeskTransport {
    shared: Arc<Mutex<LeaseDeskState>>,
}

#[cfg(feature = "binder-desk")]
impl LeaseDeskTransport {
    fn ready() -> (Self, Arc<Mutex<LeaseDeskState>>) {
        let shared = Arc::new(Mutex::new(LeaseDeskState {
            status: crate::dsp::guard::RawStatus {
                abi_version: crate::dsp::guard::ABI_VERSION,
                dsp_powered: 1,
                program_loaded: 1,
                fm_powered: 1,
                dac_enabled: 1,
                route_ready: 1,
                amplifier_enabled: 1,
                source: 1,
                ..Default::default()
            },
            ..Default::default()
        }));
        (
            Self {
                shared: Arc::clone(&shared),
            },
            shared,
        )
    }

    fn simple_call(&mut self, call: impl Into<String>) -> std::io::Result<()> {
        self.shared
            .lock()
            .expect("desk transport lock")
            .calls
            .push(call.into());
        Ok(())
    }
}

#[cfg(feature = "binder-desk")]
impl Transport for LeaseDeskTransport {
    #[cfg(feature = "acc-power-channel")]
    fn acc_hold(&mut self, mut request: crate::dsp::guard::AccHold) -> std::io::Result<crate::dsp::guard::AccHold> {
        request.held = u32::from(request.operation != 2);
        Ok(request)
    }
    fn get_status(&mut self) -> std::io::Result<crate::dsp::guard::RawStatus> {
        Ok(self.shared.lock().expect("desk transport lock").status)
    }

    fn fault_safe(&mut self) -> std::io::Result<()> {
        let mut shared = self.shared.lock().expect("desk transport lock");
        shared.calls.push("fault-safe".to_owned());
        shared.status.fm_powered = 0;
        shared.status.source = 1;
        shared.status.transition_pending = 0;
        shared.status.route_ready = 0;
        Ok(())
    }

    fn dsp_on(&mut self) -> std::io::Result<()> {
        self.simple_call("dsp-on")
    }

    fn dac_power(&mut self, enabled: bool) -> std::io::Result<()> {
        self.simple_call(format!("dac-power({enabled})"))
    }

    fn amplifier_power(&mut self, enabled: bool) -> std::io::Result<()> {
        self.simple_call(format!("amp-power({enabled})"))
    }

    fn source_transition(
        &mut self,
        request: crate::dsp::guard::SourceTransition,
    ) -> std::io::Result<()> {
        let mut shared = self.shared.lock().expect("desk transport lock");
        shared.calls.push(format!(
            "source(source={},phase={})",
            request.source, request.phase
        ));
        if request.source == 2 && shared.status.fm_powered == 0 {
            return Err(std::io::Error::from_raw_os_error(32));
        }
        if request.source == 1 && request.phase == 1 && shared.fail_android_prepare {
            return Err(std::io::Error::from_raw_os_error(5));
        }
        if request.source == 1 && request.phase == 0 && shared.fail_android_atomic {
            return Err(std::io::Error::from_raw_os_error(5));
        }
        shared.status.source = request.source;
        shared.status.transition_pending = u32::from(request.phase == 1);
        shared.status.route_ready = u32::from(request.phase != 1);
        Ok(())
    }

    fn dsp_update(&mut self, _request: crate::dsp::guard::DspUpdate) -> std::io::Result<()> {
        self.simple_call("dsp-update")
    }

    fn master_volume(&mut self, _request: crate::dsp::guard::DspUpdate) -> std::io::Result<()> {
        self.simple_call("master")
    }

    fn fm_power(&mut self, enabled: bool) -> std::io::Result<()> {
        let mut shared = self.shared.lock().expect("desk transport lock");
        shared.calls.push(format!("fm-power({enabled})"));
        if !enabled && shared.fail_fm_off {
            return Err(std::io::Error::from_raw_os_error(5));
        }
        shared.status.fm_powered = u32::from(enabled);
        Ok(())
    }

    fn fm_mix_transition(
        &mut self,
        request: crate::dsp::guard::FmMixTransition,
    ) -> std::io::Result<()> {
        let mut shared = self.shared.lock().expect("desk transport lock");
        shared
            .calls
            .push(format!("fm-mix(phase={})", request.phase));
        shared.status.source = 3;
        shared.status.transition_pending = u32::from(request.phase == 1);
        shared.status.route_ready = u32::from(request.phase != 1);
        Ok(())
    }

    fn fm_attenuation(
        &mut self,
        _request: crate::dsp::guard::FmAttenuation,
    ) -> std::io::Result<()> {
        self.simple_call("fm-attenuation")
    }

    fn fm_mix_base(&mut self, _request: crate::dsp::guard::FmMixBase) -> std::io::Result<()> {
        self.simple_call("fm-mix-base")
    }
}

#[cfg(feature = "binder-desk")]
struct LeaseDeskFmHardware {
    shared: Arc<Mutex<LeaseDeskState>>,
}

#[cfg(feature = "binder-desk")]
impl xyauto_fm::service::FmHardware for LeaseDeskFmHardware {
    type Error = &'static str;

    fn adapter_label(&self) -> String {
        "desk-fm".to_owned()
    }

    fn set_enabled(
        &mut self,
        enabled: bool,
        prepared_frequency_khz: Option<u32>,
    ) -> Result<(), xyauto_fm::service::EnableError<Self::Error>> {
        self.shared
            .lock()
            .expect("desk state lock")
            .calls
            .push(format!(
                "tuner-enabled({enabled},{prepared_frequency_khz:?})"
            ));
        Ok(())
    }

    fn chip_id(&mut self) -> Result<u8, Self::Error> {
        Ok(xyauto_fm::qn8035::CHIP_ID)
    }

    fn tune(&mut self, frequency_khz: u32) -> Result<(), Self::Error> {
        self.shared
            .lock()
            .expect("desk state lock")
            .calls
            .push(format!("tuner-tune({frequency_khz})"));
        Ok(())
    }

    fn signal_quality(&mut self) -> Result<xyauto_fm::qn8035::SignalQuality, Self::Error> {
        Ok(xyauto_fm::qn8035::SignalQuality {
            snr: 31,
            rssi: 88,
            station_present: true,
        })
    }

    fn start_seek(
        &mut self,
        _direction: xyauto_fm::qn8035::SeekDirection,
        _from_khz: u32,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn stop_seek(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn power_lost(&mut self) {
        self.shared
            .lock()
            .expect("desk state lock")
            .calls
            .push("tuner-power-lost".to_owned());
    }
}

#[cfg(feature = "binder-desk")]
type LeaseDeskFixture = (
    Arc<Mutex<AudioGuard<LeaseDeskTransport>>>,
    Arc<FmMixLeaseRegistry<LeaseDeskTransport>>,
    Arc<Mutex<LeaseDeskState>>,
);

#[cfg(feature = "binder-desk")]
fn lease_desk_active_mix(fail_fm_off: bool, fail_android_prepare: bool) -> LeaseDeskFixture {
    let (transport, shared) = LeaseDeskTransport::ready();
    {
        let mut state = shared.lock().expect("desk state lock");
        state.fail_fm_off = fail_fm_off;
        state.fail_android_prepare = fail_android_prepare;
    }
    let guard = Arc::new(Mutex::new(AudioGuard::with_transport(transport)));
    guard
        .lock()
        .expect("mixed guard lock")
        .set_fm_power(true)
        .expect("V1 caller raised FM rails");
    let cleanup_state = Arc::clone(&shared);
    let registry = Arc::new(FmMixLeaseRegistry::new(
        Arc::clone(&guard),
        Arc::new(move |rails_live| {
            cleanup_state
                .lock()
                .expect("desk state lock")
                .calls
                .push(format!("tuner-cleanup({rails_live})"));
            Ok(())
        }),
    ));
    let owner = crate::Endpoint::new().binder();
    registry
        .claim_and_prepare(&owner, &guard)
        .expect("mixed prepare");
    let generation = registry
        .current_generation(&owner)
        .expect("mixed generation");
    guard
        .lock()
        .expect("mixed guard lock")
        .commit_fm_mix(generation)
        .expect("mixed commit");
    (guard, registry, shared)
}
