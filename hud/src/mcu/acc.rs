// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Pure ACC lifecycle ported from `hu-mcu-bridge`.
//!
//! Time and hardware are injected. This module never opens a tty, writes
//! sysfs, or requests suspend itself; the owner supplies [`Hooks`].

use super::{may_transmit, Event, TX_ACC_OFF_COMPLETE, TX_ACC_ON_ANSWER};

/// Stock's cancellable delay after the MCU sends `05 10`.
pub const FINAL_STAGE_DELAY_MS: u64 = 500;

/// Fallback guard for a PMIC key event when no completed-suspend proof exists.
pub const HARDWARE_WAKE_GUARD_MS: u64 = 30_000;

/// The five externally visible lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    /// No confirmed ACC transition yet.
    #[default]
    Idle,
    /// `12 03` accepted; panel blank, Android still awake.
    AccOffGrace,
    /// `05 10` accepted; cleanup started. The 500 ms callback is pending,
    /// or its host write failed and this cycle is waiting for ACC-on.
    FinalStage,
    /// Fixed `05 15` completely host-written and suspend requested.
    /// This does not establish UART drain, MCU receipt or actual suspend.
    Suspending,
    /// ACC-on accepted; the same boot is awake again.
    Resumed,
}

/// The only frames this machine can emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tx {
    /// Stock `sendACCOFFtoMCU()` completion.
    AccOffComplete,
    /// Stock `sendACCONANStoMCU()` acknowledgement.
    AccOnAnswer,
}

impl Tx {
    /// Exact bytes accepted by the crate-wide transmit gate.
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Tx::AccOffComplete => TX_ACC_OFF_COMPLETE,
            Tx::AccOnAnswer => TX_ACC_ON_ANSWER,
        }
    }
}

/// Result at the host write boundary, never proof of MCU receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostWriteResult {
    /// The complete frame was accepted by the host writer.
    Complete,
    /// The gate refused or the write failed; delivery may be partial.
    Failed,
}

/// Ordered control input for a future retained-standby owner, not TX diagnostics.
///
/// Generations identify accepted OFF cycles only within this Machine instance.
/// The production broker must supply a fresh session envelope on UART/daemon
/// replacement; neither this number nor a checksum establishes wire freshness.
/// These events carry no timestamp suitable for Android PowerManager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleEvent {
    /// One confirmed OFF begins a new grace period.
    Grace {
        /// Generation assigned to the accepted OFF transition.
        off_generation: u64,
    },
    /// One final-stage request starts preparation and the 500 ms delay.
    /// This is a request to prepare, not proof that policy work completed.
    Prepare {
        /// Generation of the OFF cycle being prepared.
        off_generation: u64,
    },
    /// The once-only final-frame host result, before any legacy sleep intent.
    FinalWrite {
        /// Generation of the OFF cycle that attempted completion.
        off_generation: u64,
        /// Host write outcome only; never MCU receipt or suspend proof.
        result: HostWriteResult,
    },
    /// End this OFF cycle: cancel pending/failed preparation or resume from
    /// Suspending. Always invalidate its work, even if no sleep occurred.
    /// Emitted after the prompt ACC-on answer attempt, before restore policy.
    Resume {
        /// The prior OFF generation, NOT the new resumed-state generation.
        off_generation: u64,
    },
    /// No more OFF cycles can be assigned safely. The machine rejects them
    /// until replaced; a broker must use a fresh session envelope, never wrap.
    GenerationExhausted,
}

/// Side effects owned by the eventual vendor service/PID 1 integration.
///
/// Implementations record their own failures. Policy hooks remain best-effort;
/// a failed final-frame host write prevents the suspend request.
pub trait Hooks {
    /// Handle ordered control locally without waiting, Binder or other I/O.
    /// A production consumer must preserve delivery or invalidate its session
    /// on queue failure; this must not share a lossy diagnostic mailbox.
    /// Inert until an owner explicitly implements the control contract.
    fn on_cycle_event(&mut self, _event: CycleEvent) {}
    /// Blank the panel but keep Android awake during the MCU grace period.
    fn enter_acc_off_grace(&mut self);
    /// Stop radio-producing subsystems before rail removal.
    fn quiesce_radio(&mut self);
    /// Detach/unmount the stick sink before USB is quiesced.
    fn detach_stick_sink(&mut self);
    /// Quiesce USB and its wake sources.
    fn quiesce_usb(&mut self);
    /// Write a fixed, gated MCU frame and report whether the whole host write
    /// completed. Implementations retain the original error for their owner.
    fn transmit(&mut self, frame: Tx) -> HostWriteResult;
    /// Ask Android SystemSuspend to enter the selected retained state.
    fn request_suspend(&mut self);
    /// Restore post-wake USB/radio/panel policy after the MCU answer.
    fn resume(&mut self);
}

/// Proof accompanying a PMIC KPDPWR event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspendProof {
    /// `/sys/power/suspend_stats/success` advanced across this cycle.
    Completed,
    /// No counter proof; apply the 30 s fallback guard.
    Unavailable,
}

/// Generation captured before an asynchronous PMIC wake handler runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeToken(u64);

/// Deterministic ACC state machine.
#[derive(Debug, Default)]
pub struct Machine {
    state: State,
    generation: u64,
    exhaustion_reported: bool,
    acc_on_candidate_count: u8,
    final_deadline_ms: Option<u64>,
    final_generation: Option<u64>,
    suspending_since_ms: Option<u64>,
    pending_reopen_answer: Option<u64>,
}

impl Machine {
    /// New machine in [`State::Idle`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Current lifecycle state.
    pub fn state(&self) -> State {
        self.state
    }

    /// Current transition generation within this Machine only. Resume advances
    /// it; CycleEvent::Resume carries the prior OFF generation instead.
    /// New instances require a fresh broker envelope. OFF admission stops before
    /// rollover, reserving one generation for the last admitted cycle's resume.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Token for a possibly asynchronous PMIC wake observation.
    pub fn wake_token(&self) -> WakeToken {
        WakeToken(self.generation)
    }

    /// Consume one decoded, checksum-valid MCU event.
    pub fn on_event<H: Hooks>(&mut self, event: &Event, now_ms: u64, hooks: &mut H) {
        match event {
            Event::AccOff => self.on_acc_off(hooks),
            Event::AccOn => self.on_acc_on(hooks),
            Event::AccWake => {
                if self.is_off_cycle() {
                    self.confirm_acc_on(hooks);
                }
            }
            Event::PowerKey { abstract_key, .. } if *abstract_key == super::POWER_KEY_SHUTDOWN => {
                self.on_final_stage(now_ms, hooks)
            }
            _ => {}
        }
    }

    /// Run the due 500 ms completion callback, if its generation is current.
    pub fn advance<H: Hooks>(&mut self, now_ms: u64, hooks: &mut H) {
        let Some(deadline) = self.final_deadline_ms else {
            return;
        };
        if now_ms < deadline
            || self.state != State::FinalStage
            || self.final_generation != Some(self.generation)
        {
            return;
        }
        self.final_deadline_ms = None;
        self.final_generation = None;
        // Consume the callback before attempting TX: an incomplete write must
        // not be retried by later ticks or duplicate OFF/final-stage frames.
        // Keep FinalStage so ACC-on can still cancel and restore this cycle.
        let result = self.emit(Tx::AccOffComplete, hooks);
        hooks.on_cycle_event(CycleEvent::FinalWrite {
            off_generation: self.generation,
            result,
        });
        if result != HostWriteResult::Complete {
            return;
        }
        self.state = State::Suspending;
        self.suspending_since_ms = Some(now_ms);
        hooks.request_suspend();
    }

    /// Confirm a UART-independent KPDPWR/MPM wake if its snapshot is current.
    pub fn on_hardware_wake<H: Hooks>(
        &mut self,
        token: WakeToken,
        proof: SuspendProof,
        now_ms: u64,
        hooks: &mut H,
    ) -> bool {
        if token.0 != self.generation || self.state != State::Suspending {
            return false;
        }
        let immediate = proof == SuspendProof::Completed;
        let guard_elapsed = self
            .suspending_since_ms
            .map(|at| now_ms.saturating_sub(at) >= HARDWARE_WAKE_GUARD_MS)
            .unwrap_or(false);
        if !immediate && !guard_elapsed {
            return false;
        }
        self.confirm_acc_on(hooks);
        true
    }

    /// Repeat `05 17` once on the deliberately reopened UART session.
    pub fn on_uart_reopened<H: Hooks>(&mut self, hooks: &mut H) -> bool {
        if self.pending_reopen_answer != Some(self.generation) || self.state != State::Resumed {
            return false;
        }
        self.pending_reopen_answer = None;
        self.emit(Tx::AccOnAnswer, hooks);
        true
    }

    fn is_off_cycle(&self) -> bool {
        matches!(
            self.state,
            State::AccOffGrace | State::FinalStage | State::Suspending
        )
    }

    fn on_acc_off<H: Hooks>(&mut self, hooks: &mut H) {
        if self.is_off_cycle() {
            return;
        }
        // Reserve the next increment for ACC-on, which must always be able to
        // end an admitted cycle and send 05 17 without aliasing a generation.
        if self.generation >= u64::MAX - 1 {
            if !self.exhaustion_reported {
                self.exhaustion_reported = true;
                hooks.on_cycle_event(CycleEvent::GenerationExhausted);
            }
            return;
        }
        self.generation += 1;
        self.state = State::AccOffGrace;
        self.acc_on_candidate_count = 0;
        self.final_deadline_ms = None;
        self.final_generation = None;
        self.suspending_since_ms = None;
        self.pending_reopen_answer = None;
        hooks.on_cycle_event(CycleEvent::Grace { off_generation: self.generation });
        hooks.enter_acc_off_grace();
    }

    fn on_final_stage<H: Hooks>(&mut self, now_ms: u64, hooks: &mut H) {
        if self.state != State::AccOffGrace {
            return;
        }
        self.state = State::FinalStage;
        self.final_generation = Some(self.generation);
        self.final_deadline_ms = Some(now_ms.saturating_add(FINAL_STAGE_DELAY_MS));
        hooks.on_cycle_event(CycleEvent::Prepare { off_generation: self.generation });
        hooks.quiesce_radio();
        hooks.detach_stick_sink();
        hooks.quiesce_usb();
    }

    fn on_acc_on<H: Hooks>(&mut self, hooks: &mut H) {
        if self.is_off_cycle() {
            self.confirm_acc_on(hooks);
            return;
        }
        if self.state == State::Resumed {
            return;
        }
        self.acc_on_candidate_count = self.acc_on_candidate_count.saturating_add(1);
        if self.acc_on_candidate_count < 2 {
            return;
        }
        // Startup ON allocates no OFF cycle. Saturation cannot admit another
        // cycle: on_acc_off reserves room for both of its transitions.
        self.generation = self.generation.saturating_add(1);
        self.state = State::Resumed;
        self.acc_on_candidate_count = 0;
        hooks.resume();
    }

    fn confirm_acc_on<H: Hooks>(&mut self, hooks: &mut H) {
        let off_generation = self.generation;
        self.generation = self.generation.checked_add(1)
            .expect("OFF admission reserves a resume generation");
        self.state = State::Resumed;
        self.acc_on_candidate_count = 0;
        self.final_deadline_ms = None;
        self.final_generation = None;
        self.suspending_since_ms = None;
        self.pending_reopen_answer = Some(self.generation);
        self.emit(Tx::AccOnAnswer, hooks);
        hooks.on_cycle_event(CycleEvent::Resume { off_generation });
        hooks.resume();
    }

    fn emit<H: Hooks>(&self, frame: Tx, hooks: &mut H) -> HostWriteResult {
        debug_assert!(may_transmit(frame.bytes()).is_ok());
        hooks.transmit(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcu::acc_go_fixtures::{FixtureStep, GO_FIXTURES};
    use crate::mcu::{FrameRefused, Item, Parser, DENY};

    #[derive(Default)]
    struct Recorder {
        calls: Vec<&'static str>,
        tx: Vec<Tx>,
        fail_final_write: bool,
        cycle_events: Vec<CycleEvent>,
    }

    impl Hooks for Recorder {
        fn on_cycle_event(&mut self, event: CycleEvent) {
            self.cycle_events.push(event);
        }

        fn enter_acc_off_grace(&mut self) {
            self.calls.push("grace");
        }
        fn quiesce_radio(&mut self) {
            self.calls.push("radio");
        }
        fn detach_stick_sink(&mut self) {
            self.calls.push("stick-detach");
        }
        fn quiesce_usb(&mut self) {
            self.calls.push("usb");
        }
        fn transmit(&mut self, frame: Tx) -> HostWriteResult {
            self.tx.push(frame);
            self.calls.push(match frame {
                Tx::AccOffComplete => "tx-05-15",
                Tx::AccOnAnswer => "tx-05-17",
            });
            if self.fail_final_write && frame == Tx::AccOffComplete {
                HostWriteResult::Failed
            } else {
                HostWriteResult::Complete
            }
        }
        fn request_suspend(&mut self) {
            self.calls.push("suspend");
        }
        fn resume(&mut self) {
            self.calls.push("resume");
        }
    }

    fn event(frame: &[u8]) -> Event {
        let mut parser = Parser::new();
        match parser.push(frame).pop().unwrap() {
            Item::Frame(frame) => frame.event(),
            item => panic!("fixture is not a valid frame: {:?}", item),
        }
    }

    #[test]
    fn go_frame_sequences_emit_identical_tx_bytes_under_the_same_timing_rules() {
        for fixture in GO_FIXTURES {
            let mut machine = Machine::new();
            let mut recorder = Recorder::default();
            for step in fixture.steps {
                match step {
                    FixtureStep::Frame { at_ms, bytes } => {
                        machine.on_event(&event(bytes), *at_ms, &mut recorder)
                    }
                    FixtureStep::Advance { at_ms } => machine.advance(*at_ms, &mut recorder),
                    FixtureStep::UartReopened => {
                        machine.on_uart_reopened(&mut recorder);
                    }
                }
            }
            let got: Vec<&[u8]> = recorder.tx.iter().map(|tx| tx.bytes()).collect();
            assert_eq!(got, fixture.expected_tx, "fixture {}", fixture.name);
        }
    }

    #[test]
    fn final_stage_callback_order_keeps_the_sink_before_usb_and_tx_before_suspend() {
        let mut machine = Machine::new();
        let mut recorder = Recorder::default();
        machine.on_event(&Event::AccOff, 0, &mut recorder);
        machine.on_event(
            &Event::PowerKey {
                sub: 0x10,
                abstract_key: super::super::POWER_KEY_SHUTDOWN,
            },
            25_000,
            &mut recorder,
        );
        machine.advance(25_500, &mut recorder);
        assert_eq!(
            recorder.calls,
            [
                "grace",
                "radio",
                "stick-detach",
                "usb",
                "tx-05-15",
                "suspend"
            ]
        );
        assert_eq!(machine.state(), State::Suspending);
    }

    #[test]
    fn failed_final_write_is_disarmed_until_acc_on_or_legacy_wake() {
        for wake in [Event::AccOn, Event::AccWake] {
            let mut machine = Machine::new();
            let mut recorder = Recorder { fail_final_write: true, ..Default::default() };
            let final_stage = Event::PowerKey {
                sub: 0x10,
                abstract_key: super::super::POWER_KEY_SHUTDOWN,
            };
            machine.on_event(&Event::AccOff, 0, &mut recorder);
            machine.on_event(&final_stage, 25_000, &mut recorder);
            machine.advance(25_499, &mut recorder);
            assert!(recorder.tx.is_empty());
            machine.advance(25_500, &mut recorder);
            assert_eq!(machine.state(), State::FinalStage);
            assert_eq!(recorder.calls, ["grace", "radio", "stick-detach", "usb", "tx-05-15"]);
            assert_eq!(machine.final_deadline_ms, None);
            assert_eq!(machine.suspending_since_ms, None);

            // Repairing the writer cannot silently retry the consumed callback.
            recorder.fail_final_write = false;
            machine.on_event(&Event::AccOff, 26_000, &mut recorder);
            machine.on_event(&final_stage, 26_000, &mut recorder);
            machine.advance(60_000, &mut recorder);
            assert_eq!(recorder.tx, [Tx::AccOffComplete]);
            assert!(!machine.on_hardware_wake(machine.wake_token(), SuspendProof::Completed, 60_000, &mut recorder));
            machine.on_event(&wake, 60_001, &mut recorder);
            machine.advance(60_002, &mut recorder);
            assert_eq!(machine.state(), State::Resumed);
            assert_eq!(recorder.tx, [Tx::AccOffComplete, Tx::AccOnAnswer]);
            assert_eq!(&recorder.calls[5..], ["tx-05-17", "resume"]);
        }
    }

    #[test]
    fn generation_limit_reserves_resume_and_rejects_new_off_without_aliasing() {
        for fail_final_write in [false, true] {
            let mut machine = Machine { generation: u64::MAX - 2, ..Default::default() };
            let mut recorder = Recorder { fail_final_write, ..Default::default() };
            let final_stage = Event::PowerKey {
                sub: 0x10,
                abstract_key: super::super::POWER_KEY_SHUTDOWN,
            };
            let off_generation = u64::MAX - 1;
            machine.on_event(&Event::AccOff, 0, &mut recorder);
            machine.on_event(&final_stage, 25_000, &mut recorder);
            machine.advance(25_500, &mut recorder);
            machine.on_event(&Event::AccOn, 30_000, &mut recorder);
            assert_eq!(machine.generation(), u64::MAX);
            assert_eq!(machine.state(), State::Resumed);
            assert_eq!(recorder.tx, [Tx::AccOffComplete, Tx::AccOnAnswer]);
            assert_eq!(recorder.cycle_events, [
                CycleEvent::Grace { off_generation },
                CycleEvent::Prepare { off_generation },
                CycleEvent::FinalWrite { off_generation, result: if fail_final_write { HostWriteResult::Failed } else { HostWriteResult::Complete } },
                CycleEvent::Resume { off_generation },
            ]);
            let calls = recorder.calls.clone();
            for _ in 0..2 {
                machine.on_event(&Event::AccOff, 31_000, &mut recorder);
                machine.on_event(&final_stage, 31_000, &mut recorder);
                machine.advance(31_500, &mut recorder);
            }
            assert_eq!(machine.state(), State::Resumed);
            assert_eq!(machine.generation(), u64::MAX);
            assert_eq!(recorder.calls, calls);
            assert_eq!(recorder.tx, [Tx::AccOffComplete, Tx::AccOnAnswer]);
            assert_eq!(recorder.cycle_events.len(), 5);
            assert_eq!(recorder.cycle_events[4], CycleEvent::GenerationExhausted);
            assert!(machine.on_uart_reopened(&mut recorder));
            assert!(!machine.on_uart_reopened(&mut recorder));
            assert_eq!(recorder.tx, [Tx::AccOffComplete, Tx::AccOnAnswer, Tx::AccOnAnswer]);
            assert_eq!(recorder.cycle_events.len(), 5);
        }
    }

    #[test]
    fn acc_on_cancels_a_pending_final_stage() {
        let mut machine = Machine::new();
        let mut recorder = Recorder::default();
        machine.on_event(&Event::AccOff, 0, &mut recorder);
        machine.on_event(
            &Event::PowerKey {
                sub: 0x10,
                abstract_key: super::super::POWER_KEY_SHUTDOWN,
            },
            25_000,
            &mut recorder,
        );
        machine.on_event(&Event::AccOn, 25_499, &mut recorder);
        machine.advance(25_500, &mut recorder);
        assert_eq!(recorder.tx, [Tx::AccOnAnswer]);
        assert_eq!(machine.state(), State::Resumed);
    }

    #[test]
    fn pmic_wake_revalidates_generation_and_rejects_the_shutdown_stage() {
        let mut machine = Machine::new();
        let mut recorder = Recorder::default();
        machine.on_event(&Event::AccOff, 0, &mut recorder);
        let stale = machine.wake_token();
        assert!(!machine.on_hardware_wake(stale, SuspendProof::Completed, 1, &mut recorder));
        machine.on_event(
            &Event::PowerKey {
                sub: 0x10,
                abstract_key: super::super::POWER_KEY_SHUTDOWN,
            },
            25_000,
            &mut recorder,
        );
        machine.advance(25_500, &mut recorder);
        machine.on_event(&Event::AccOn, 25_501, &mut recorder);
        machine.on_event(&Event::AccOff, 25_502, &mut recorder);
        assert!(!machine.on_hardware_wake(stale, SuspendProof::Completed, 25_503, &mut recorder));
    }

    #[test]
    fn completed_suspend_proof_bypasses_the_fallback_guard() {
        let mut machine = Machine::new();
        let mut recorder = Recorder::default();
        machine.on_event(&Event::AccOff, 0, &mut recorder);
        machine.on_event(
            &Event::PowerKey {
                sub: 0x10,
                abstract_key: super::super::POWER_KEY_SHUTDOWN,
            },
            25_000,
            &mut recorder,
        );
        machine.advance(25_500, &mut recorder);
        let token = machine.wake_token();
        assert!(machine.on_hardware_wake(token, SuspendProof::Completed, 25_501, &mut recorder));
        assert_eq!(machine.state(), State::Resumed);
    }

    #[test]
    fn pmic_wake_without_suspend_proof_waits_for_the_full_guard() {
        let mut machine = Machine::new();
        let mut recorder = Recorder::default();
        machine.on_event(&Event::AccOff, 0, &mut recorder);
        machine.on_event(
            &Event::PowerKey {
                sub: 0x10,
                abstract_key: super::super::POWER_KEY_SHUTDOWN,
            },
            25_000,
            &mut recorder,
        );
        machine.advance(25_500, &mut recorder);
        let token = machine.wake_token();
        assert!(!machine.on_hardware_wake(token, SuspendProof::Unavailable, 55_499, &mut recorder));
        assert!(machine.on_hardware_wake(token, SuspendProof::Unavailable, 55_500, &mut recorder));
    }

    #[test]
    fn the_machine_can_only_emit_allowed_non_denied_variants() {
        for tx in [Tx::AccOffComplete, Tx::AccOnAnswer] {
            assert!(may_transmit(tx.bytes()).is_ok());
            let cmd = tx.bytes()[3];
            let sub = tx.bytes()[4];
            assert!(!DENY
                .iter()
                .any(|(deny_cmd, deny_sub, _)| cmd == *deny_cmd
                    && (*deny_sub == 0 || sub == *deny_sub)));
        }
        assert!(matches!(
            may_transmit(&[0x5a, 0xa5, 0x03, 0x05, 0x16, 0xe5]),
            Err(FrameRefused::Denied(_))
        ));
    }
}
