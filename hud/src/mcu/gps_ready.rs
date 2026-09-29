// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Bounded OEM GPS-ready boot/wake notifications for the sole UART owner.
//!
//! Authentic device-3 NewkeypadMonitor.java:4846,4864 sends a notification
//! during guarded initialization and queues one repeat 1000 ms later; :8977
//! cancels that repeat on ACC-off. This is not an antenna-power or fix claim.
//! The scheduler survives UART replacement; opening an fd is not a new wake.

use super::acc::State;

/// OEM delay before enqueuing the second notification; owner polling adds jitter.
pub const REPEAT_DELAY_MS: u64 = 1_000;

/// Why a bounded notification pair was admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reason {
    /// Startup answer succeeded and a valid version was received without OFF.
    /// This is initialization evidence, not a confirmed ACC-on transition.
    BootInit,
    /// The ACC machine accepted an actual wake after an OFF cycle.
    Wake,
}

impl Reason {
    /// Stable log label; neither label claims an MCU acknowledgement.
    pub fn label(self) -> &'static str {
        match self {
            Self::BootInit => "boot-init",
            Self::Wake => "wake",
        }
    }
}

/// One notification attempt. A write error cancels the pair, never retries it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Notification {
    /// One or two within this initialization pair.
    pub sequence: u8,
    /// Boot initialization or a confirmed wake transition.
    pub reason: Reason,
}

#[derive(Clone, Copy)]
struct Observation {
    connection: u64,
    generation: u64,
    state: State,
}

struct Pending {
    connection: u64,
    generation: u64,
    deadline_ms: u64,
    reason: Reason,
}

/// Process-lifetime scheduling state, kept outside the UART reopen loop.
#[derive(Default)]
pub struct Scheduler {
    initialized: bool,
    off_seen: bool,
    previous: Option<Observation>,
    pending: Option<Pending>,
}

impl Scheduler {
    /// Cancel a pending second write and invalidate the old session identity.
    /// Preserve initialization and any known OFF so reconnect cannot flood
    /// notifications, but a confirmed wake after a known OFF can initialize.
    pub fn disconnected(&mut self) {
        self.pending = None;
        self.previous = None;
    }

    /// Observe the ACC owner's current state, then consume at most one due TX.
    /// `boot_ready` means the connection's fixed startup answer succeeded AND
    /// its parser received a checksum-valid, decoded version response. It does
    /// not alter the ACC machine's Idle state or synthesize an ACC-on event.
    pub fn poll(
        &mut self,
        connection: u64,
        generation: u64,
        state: State,
        boot_ready: bool,
        now_ms: u64,
    ) -> Option<Notification> {
        let previous = self.previous.replace(Observation {
            connection,
            generation,
            state,
        });
        if matches!(
            state,
            State::AccOffGrace | State::FinalStage | State::Suspending
        ) {
            self.off_seen = true;
            self.pending = None;
            return None;
        }
        if self.pending.as_ref().is_some_and(|pending| {
            pending.connection != connection || pending.generation != generation
        }) {
            self.pending = None;
        }
        // A changed Resumed generation in one session proves a new lifecycle.
        // Idle→Resumed +1 is startup confirmation only; +2 or more includes
        // OFF→ON even when both valid frames arrived in a single read chunk.
        let new_wake = state == State::Resumed
            && (self.off_seen
                || previous.is_some_and(|old| {
                    old.connection == connection
                        && old.generation != generation
                        && (old.state == State::Resumed
                            || generation.saturating_sub(old.generation) >= 2)
                }));
        let boot_init = !self.initialized && !self.off_seen && boot_ready;
        if new_wake || boot_init {
            let reason = if new_wake {
                Reason::Wake
            } else {
                Reason::BootInit
            };
            self.initialized = true;
            self.off_seen = false;
            self.pending = now_ms
                .checked_add(REPEAT_DELAY_MS)
                .map(|deadline_ms| Pending {
                    connection,
                    generation,
                    deadline_ms,
                    reason,
                });
            return Some(Notification {
                sequence: 1,
                reason,
            });
        }
        if self.off_seen {
            return None;
        } // reconnect Idle cannot establish wake
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| now_ms >= pending.deadline_ms)
        {
            let pending = self.pending.take().expect("due pending notification");
            return Some(Notification {
                sequence: 2,
                reason: pending.reason,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_needs_version_and_startup_answer_then_exactly_one_delayed_repeat() {
        let mut scheduler = Scheduler::default();
        assert_eq!(scheduler.poll(1, 0, State::Idle, false, 0), None);
        assert_eq!(
            scheduler.poll(1, 0, State::Idle, true, 100),
            Some(Notification {
                sequence: 1,
                reason: Reason::BootInit
            })
        );
        assert_eq!(scheduler.poll(1, 0, State::Idle, true, 1099), None);
        assert_eq!(
            scheduler
                .poll(1, 0, State::Idle, true, 1100)
                .unwrap()
                .sequence,
            2
        );
        assert_eq!(scheduler.poll(1, 0, State::Idle, true, 5000), None);
    }

    #[test]
    fn off_cancels_repeat_and_confirmed_wake_starts_one_new_pair() {
        for off in [State::AccOffGrace, State::FinalStage, State::Suspending] {
            let mut scheduler = Scheduler::default();
            assert!(scheduler.poll(1, 0, State::Idle, true, 0).is_some());
            assert_eq!(scheduler.poll(1, 1, off, true, 1000), None);
            assert_eq!(scheduler.poll(1, 1, off, true, 3000), None);
            assert_eq!(
                scheduler.poll(1, 2, State::Resumed, true, 3100),
                Some(Notification {
                    sequence: 1,
                    reason: Reason::Wake
                })
            );
            assert_eq!(scheduler.poll(1, 2, State::Resumed, true, 4099), None);
            assert_eq!(
                scheduler
                    .poll(1, 2, State::Resumed, true, 4100)
                    .unwrap()
                    .sequence,
                2
            );
            assert_eq!(scheduler.poll(1, 2, State::Resumed, true, 9000), None);
        }
    }

    #[test]
    fn reconnect_cancels_and_does_not_replay_boot_or_startup_confirmation() {
        let mut scheduler = Scheduler::default();
        assert!(scheduler.poll(1, 0, State::Idle, true, 0).is_some());
        scheduler.disconnected();
        for connection in 2..20 {
            assert_eq!(scheduler.poll(connection, 0, State::Idle, true, 2000), None);
            assert_eq!(
                scheduler.poll(connection, 1, State::Resumed, true, 3000),
                None
            );
            scheduler.disconnected();
        }
        // Owner seeds a new session before its first read, so a whole OFF/ON
        // cycle in that read is still distinguishable from mere fd reopen.
        assert_eq!(scheduler.poll(20, 0, State::Idle, false, 4000), None);
        assert_eq!(
            scheduler
                .poll(20, 2, State::Resumed, true, 4100)
                .unwrap()
                .reason,
            Reason::Wake
        );
    }

    #[test]
    fn known_off_survives_disconnect_but_needs_confirmed_resumed_state() {
        let mut scheduler = Scheduler::default();
        assert_eq!(scheduler.poll(1, 1, State::AccOffGrace, true, 0), None);
        scheduler.disconnected();
        assert_eq!(scheduler.poll(2, 0, State::Idle, true, 2000), None);
        assert_eq!(
            scheduler
                .poll(2, 1, State::Resumed, true, 2100)
                .unwrap()
                .reason,
            Reason::Wake
        );
    }

    #[test]
    fn generation_change_cancels_and_batched_off_on_still_starts_a_wake_pair() {
        let mut scheduler = Scheduler::default();
        assert!(scheduler.poll(1, 0, State::Idle, true, 0).is_some());
        // Startup two-ON confirmation changes generation, but isn't a wake.
        assert_eq!(scheduler.poll(1, 1, State::Resumed, true, 1000), None);
        assert_eq!(scheduler.poll(1, 1, State::Resumed, true, 2000), None);
        // OFF and ON can be parsed in one read, jumping two generations.
        assert_eq!(
            scheduler
                .poll(1, 3, State::Resumed, true, 3000)
                .unwrap()
                .reason,
            Reason::Wake
        );
        assert_eq!(
            scheduler
                .poll(1, 3, State::Resumed, true, 4000)
                .unwrap()
                .sequence,
            2
        );
        let mut startup = Scheduler::default();
        assert_eq!(startup.poll(1, 0, State::Idle, false, 0), None);
        assert_eq!(
            startup
                .poll(1, 2, State::Resumed, true, 100)
                .unwrap()
                .reason,
            Reason::Wake
        );
    }

    #[test]
    fn clock_overflow_does_not_schedule_an_immediate_repeat() {
        let mut scheduler = Scheduler::default();
        assert_eq!(
            scheduler
                .poll(1, 0, State::Idle, true, u64::MAX)
                .unwrap()
                .sequence,
            1
        );
        assert_eq!(scheduler.poll(1, 0, State::Idle, true, u64::MAX), None);
    }
}
