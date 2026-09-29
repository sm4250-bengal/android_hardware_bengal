// SPDX-License-Identifier: Apache-2.0
//! Native preparation runs on one owned worker, never inside a UART hook.
//! Tokens below are local lease identities, NOT MCU wire/session identities.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

/// All methods run on the preparation worker. A failed hold may have changed
/// hardware: release must still reconcile that exact owner's uncertain state.
pub(crate) trait Owner: Send + 'static {
    fn hold(&mut self, token: u64) -> bool;
    fn confirmed(&mut self, token: u64) -> bool;
    fn release(&mut self, token: u64) -> bool;
    fn arm(&mut self, _token: u64, _deadline_ms: u64) -> bool {
        false
    }
    fn disarm(&mut self, _token: u64) -> bool {
        true
    }
}

/// Root-owned platform implementation. Called only on the preparation worker
/// after UART acceptance of READY, never GRACE. Arm must validate current
/// backend/provider state and the absolute BOOTTIME deadline; no mem entry.
/// Disarm restores only this token's selection, including a failed/late arm.
pub(crate) trait DeepOwner: Send + 'static {
    fn arm(&mut self, token: u64, deadline_ms: u64) -> bool;
    fn disarm(&mut self, token: u64) -> bool;
}

pub(super) struct Preparation {
    pub validity: Arc<AtomicBool>,
    next: AtomicU64,
    desired: AtomicU64,
    completed: AtomicU64,
    arm_request: AtomicU64,
    arm_deadline: AtomicU64,
    armed: AtomicU64,
    stop: AtomicBool,
    wake_epoch: AtomicU64,
}

impl Preparation {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            validity: Arc::new(AtomicBool::new(true)),
            next: AtomicU64::new(0),
            desired: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            arm_request: AtomicU64::new(0),
            arm_deadline: AtomicU64::new(0),
            armed: AtomicU64::new(0),
            wake_epoch: AtomicU64::new(0),
        })
    }

    /// Sole UART caller; fixed atomic operations, no wake syscall or allocation.
    pub fn begin(&self) -> u64 {
        let Some(token) = self
            .next
            .load(Ordering::Relaxed)
            .checked_add(1)
            .filter(|v| *v <= i64::MAX as u64)
        else {
            return 0;
        };
        self.next.store(token, Ordering::Relaxed);
        self.completed.store(0, Ordering::Release);
        self.desired.store(token, Ordering::Release);
        token
    }

    pub fn cancel(&self, token: u64) {
        let _ = self
            .desired
            .compare_exchange(token, 0, Ordering::AcqRel, Ordering::Acquire);
    }

    pub fn ready(&self, token: u64) -> bool {
        self.held_ready(token) && self.armed.load(Ordering::Acquire) == token
    }

    fn held_ready(&self, token: u64) -> bool {
        token != 0
            && self.completed.load(Ordering::Acquire) == token
            && self.validity.load(Ordering::Acquire)
            && self.desired.load(Ordering::Acquire) == token
    }

    /// UART checkpoint only, after exact authenticated READY validation.
    pub fn request_arm(&self, token: u64, deadline_ms: u64) {
        if token != 0 && self.desired.load(Ordering::Acquire) == token {
            self.arm_deadline.store(deadline_ms, Ordering::Release);
            self.arm_request.store(token, Ordering::Release);
        }
    }

    fn complete(&self, token: u64, success: bool) {
        if success && self.desired.load(Ordering::Acquire) == token {
            self.completed.store(token, Ordering::Release);
        }
    }

    #[cfg(test)]
    pub(super) fn test_complete_current(&self) {
        let token = self.desired.load(Ordering::Acquire);
        self.complete(token, true);
        self.armed.store(token, Ordering::Release);
    }
    #[cfg(test)]
    pub(super) fn test_arm_requested(&self) -> bool {
        let token = self.desired.load(Ordering::Acquire);
        token != 0 && self.arm_request.load(Ordering::Acquire) == token
    }
}

pub(super) struct Worker {
    state: Arc<Preparation>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn start(state: Arc<Preparation>, mut owner: impl Owner) -> std::io::Result<Self> {
        let s = state.clone();
        let thread = thread::Builder::new()
            .name("acc-prepare".into())
            .spawn(move || {
                let mut work = Work::default();
                while !s.stop.load(Ordering::Acquire) {
                    let epoch = s.wake_epoch.load(Ordering::Acquire);
                    work.step(&s, &mut owner);
                    // Blocking owner internals may consume a thread park token.
                    // Do not lose cancellation/shutdown that arrived during I/O.
                    if s.stop.load(Ordering::Acquire) {
                        break;
                    }
                    if s.wake_epoch.load(Ordering::Acquire) != epoch {
                        continue;
                    }
                    thread::park();
                }
                s.desired.store(0, Ordering::Release);
                work.step(&s, &mut owner);
            })?;
        Ok(Self {
            state,
            thread: Some(thread),
        })
    }
    /// Called after a UART checkpoint or serial step, never inside MCU hooks.
    pub fn notify(&self) {
        self.state.wake_epoch.fetch_add(1, Ordering::Release);
        if let Some(thread) = &self.thread {
            thread.thread().unpark();
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        self.notify();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Default)]
struct Work {
    held: Option<u64>,
    prepared: bool,
    attempted: u64,
    failed_release: bool,
    armed: Option<u64>,
    attempted_arm: u64,
}
impl Work {
    fn step(&mut self, state: &Preparation, owner: &mut impl Owner) {
        let desired = state.desired.load(Ordering::Acquire);
        if self.failed_release {
            return;
        } // uncertain restoration never grants later readiness
        if self.armed.is_some_and(|token| token != desired) {
            let token = self.armed.take().unwrap();
            state.armed.store(0, Ordering::Release);
            if !owner.disarm(token) {
                self.failed_release = true;
                return;
            }
        }
        if desired == 0 {
            if let Some(held) = self.held.take() {
                self.failed_release = !owner.release(held);
            }
            self.attempted = 0;
        } else if desired != self.attempted {
            self.attempted = desired;
            let success = if let Some(held) = self.held {
                // Consecutive OFFs reuse the held physical boundary, never a
                // late old-cycle unmute after preparing a newer cycle.
                self.prepared && owner.confirmed(held)
            } else {
                self.held = Some(desired); // issued state, even on failure
                owner.hold(desired)
            };
            self.prepared = success;
            state.complete(desired, success);
        }
        if desired != 0
            && self.attempted_arm != desired
            && state.held_ready(desired)
            && state.arm_request.load(Ordering::Acquire) == desired
        {
            self.attempted_arm = desired;
            self.armed = Some(desired); // issued arm may mutate even if it fails
            let success = owner.arm(desired, state.arm_deadline.load(Ordering::Acquire));
            if success && state.held_ready(desired) {
                state.armed.store(desired, Ordering::Release);
            } else {
                state.armed.store(0, Ordering::Release);
                self.armed = None;
                if !owner.disarm(desired) {
                    self.failed_release = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        calls: Vec<(&'static str, u64)>,
        fail: bool,
        cancel_on_arm: Option<Arc<Preparation>>,
        disarm_fail: bool,
    }
    impl Owner for Fake {
        fn hold(&mut self, t: u64) -> bool {
            self.calls.push(("hold", t));
            !self.fail
        }
        fn confirmed(&mut self, t: u64) -> bool {
            self.calls.push(("query", t));
            !self.fail
        }
        fn release(&mut self, t: u64) -> bool {
            self.calls.push(("release", t));
            !self.fail
        }
        fn arm(&mut self, t: u64, _: u64) -> bool {
            self.calls.push(("arm", t));
            if let Some(s) = &self.cancel_on_arm {
                s.cancel(t);
            }
            !self.fail
        }
        fn disarm(&mut self, t: u64) -> bool {
            self.calls.push(("disarm", t));
            !self.fail && !self.disarm_fail
        }
    }
    #[test]
    fn grace_never_arms_and_cancel_disarms_before_audio_restore() {
        let s = Preparation::new();
        let mut w = Work::default();
        let mut o = Fake::default();
        let t = s.begin();
        w.step(&s, &mut o);
        assert_eq!(o.calls, [("hold", t)]);
        assert!(!s.ready(t));
        s.request_arm(t, 2000);
        w.step(&s, &mut o);
        assert!(s.ready(t));
        s.cancel(t);
        w.step(&s, &mut o);
        assert_eq!(
            o.calls,
            [("hold", t), ("arm", t), ("disarm", t), ("release", t)]
        );
    }
    #[test]
    fn late_arm_is_rolled_back_and_failed_disarm_never_releases_audio() {
        for fails in [false, true] {
            let s = Preparation::new();
            let mut w = Work::default();
            let mut o = Fake {
                cancel_on_arm: Some(s.clone()),
                disarm_fail: fails,
                ..Fake::default()
            };
            let t = s.begin();
            s.request_arm(t, 2000);
            w.step(&s, &mut o);
            assert!(!s.ready(t));
            assert_eq!(o.calls.last(), Some(&("disarm", t)));
            w.step(&s, &mut o);
            assert_eq!(o.calls.iter().any(|(op, _)| *op == "release"), !fails);
            if fails {
                let next = s.begin();
                s.request_arm(next, 3000);
                w.step(&s, &mut o);
                assert!(!s.ready(next));
            }
        }
    }
    #[test]
    fn blocked_owner_never_blocks_uart_cancel_and_late_success_is_discarded() {
        use std::sync::mpsc;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        struct Blocked {
            entered: mpsc::Sender<()>,
            resume: mpsc::Receiver<()>,
        }
        impl Owner for Blocked {
            fn hold(&mut self, _: u64) -> bool {
                self.entered.send(()).unwrap();
                self.resume.recv().unwrap();
                true
            }
            fn confirmed(&mut self, _: u64) -> bool {
                true
            }
            fn release(&mut self, _: u64) -> bool {
                true
            }
        }
        let s = Preparation::new();
        let w = Worker::start(
            s.clone(),
            Blocked {
                entered: entered_tx,
                resume: resume_rx,
            },
        )
        .unwrap();
        let token = s.begin();
        w.notify();
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        s.cancel(token); // executes while the physical owner is blocked
        assert!(!s.ready(token));
        resume_tx.send(()).unwrap();
        drop(w); // joins only in owner shutdown, never on UART callback
        assert!(!s.ready(token));
    }
    #[test]
    fn late_completion_and_old_cancel_cannot_ready_or_release_new_cycle() {
        let s = Preparation::new();
        let old = s.begin();
        s.cancel(old);
        let new = s.begin();
        s.complete(old, true);
        s.cancel(old);
        assert!(!s.ready(old));
        assert!(!s.ready(new));
        s.complete(new, true);
        assert!(s.held_ready(new));
        assert!(!s.ready(new));
    }
    #[test]
    fn newer_off_retains_exact_owned_hold_and_restores_once() {
        let s = Preparation::new();
        let mut w = Work::default();
        let mut o = Fake::default();
        let first = s.begin();
        w.step(&s, &mut o);
        assert!(s.held_ready(first));
        s.cancel(first);
        let second = s.begin();
        w.step(&s, &mut o);
        assert!(s.held_ready(second));
        assert_eq!(o.calls, [("hold", first), ("query", first)]);
        s.cancel(first);
        w.step(&s, &mut o);
        assert!(s.held_ready(second));
        s.cancel(second);
        w.step(&s, &mut o);
        w.step(&s, &mut o);
        assert_eq!(o.calls.last(), Some(&("release", first)));
        assert_eq!(o.calls.len(), 3);
    }
    #[test]
    fn failed_issued_hold_never_ready_or_retried_and_still_has_cleanup_owner() {
        let s = Preparation::new();
        let mut w = Work::default();
        let mut o = Fake {
            fail: true,
            ..Fake::default()
        };
        let t = s.begin();
        w.step(&s, &mut o);
        w.step(&s, &mut o);
        assert!(!s.ready(t));
        assert_eq!(o.calls, [("hold", t)]);
        s.cancel(t);
        w.step(&s, &mut o);
        assert_eq!(o.calls.last(), Some(&("release", t)));
        let new = s.begin();
        w.step(&s, &mut o);
        assert!(!s.ready(new));
    }
}
