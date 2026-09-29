// SPDX-License-Identifier: Apache-2.0
//! Independent bounded physical-action fanout. No public trigger or replay path.
#![cfg_attr(not(feature = "binder"), allow(dead_code))]

use super::{ButtonService, ButtonSystemAction, PressAction};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub(crate) const GLOBAL_LIMIT: usize = 8;
pub(crate) const UID_LIMIT: usize = 2;

pub(crate) trait ActionSink: Send + Sync + 'static {
    fn send(&self, action: ButtonSystemAction) -> bool;
}

#[cfg(test)]
mod tests {
    use super::super::{ButtonBinding, ButtonSource};
    use super::*;
    use std::sync::mpsc;

    struct Sink(Box<dyn Fn(ButtonSystemAction) -> bool + Send + Sync>);
    impl ActionSink for Sink {
        fn send(&self, action: ButtonSystemAction) -> bool {
            (self.0)(action)
        }
    }
    type Registry = ActionRegistry<u32, Sink>;

    fn buttons() -> ButtonService {
        let mut service = ButtonService::new();
        service
            .set_bindings(vec![ButtonBinding {
                source: ButtonSource::Bezel,
                raw_code: 0,
                y_min: 80,
                y_max: 130,
                android_key_code: 0,
                label: "Power".into(),
                system_action: ButtonSystemAction::ShowGlobalActions,
            }])
            .unwrap();
        service
    }
    fn add(registry: &Registry, handle: u32, uid: u32, sink: Arc<Sink>) -> Token {
        let Reserve::New(token) = registry.reserve(handle, uid).unwrap() else {
            panic!("new handle")
        };
        assert!(registry.publish(token, sink));
        token
    }
    fn counting() -> (Arc<Sink>, mpsc::Receiver<ButtonSystemAction>) {
        let (tx, rx) = mpsc::channel();
        (
            Arc::new(Sink(Box::new(move |action| tx.send(action).is_ok()))),
            rx,
        )
    }
    fn drain(registry: &Registry) {
        for index in 0..GLOBAL_LIMIT {
            registry.dispatch_slot(index);
        }
    }

    #[test]
    fn independent_capabilities_coalesce_and_cannot_receive_old_generations() {
        let registry = Registry::default();
        let (a, rx_a) = counting();
        let (b, rx_b) = counting();
        let old_a = add(&registry, 1, 1000, a.clone());
        add(&registry, 2, 1000, b);
        assert_eq!(registry.reserve(1, 1000), Ok(Reserve::Existing));
        let mut service = buttons();
        registry.handle_press(&mut service, -30, 110, 1, true);
        registry.handle_press(&mut service, -30, 110, 2, true);
        drain(&registry);
        assert_eq!(rx_a.try_iter().count(), 1);
        assert_eq!(rx_b.try_iter().count(), 1);
        registry.handle_press(&mut service, -30, 110, 3, true);
        registry.unregister(&99);
        registry.unregister(&1);
        let new_a = add(&registry, 1, 1000, a);
        assert_ne!(old_a, new_a);
        registry.remove_token(old_a);
        drain(&registry);
        assert_eq!(rx_a.try_iter().count(), 0, "new A must not get old event");
        assert_eq!(rx_b.try_iter().count(), 1, "B survives exact A removal");
        registry.handle_press(&mut service, -30, 110, 4, true);
        drain(&registry);
        assert_eq!(
            rx_a.try_iter().count(),
            1,
            "stale death must not remove new A"
        );
        assert_eq!(rx_b.try_iter().count(), 1);
    }

    #[test]
    fn reservations_count_towards_quotas_and_death_before_publish_cannot_resurrect() {
        let registry = Registry::default();
        let (sink, _) = counting();
        let Reserve::New(dead) = registry.reserve(1, 1000).unwrap() else {
            panic!()
        };
        registry.remove_token(dead);
        assert!(!registry.publish(dead, sink.clone()));
        add(&registry, 1, 1000, sink.clone());
        add(&registry, 2, 1000, sink.clone());
        assert!(registry.reserve(3, 1000).is_err());
        for id in 3..=8 {
            add(&registry, id, 1000 + id, sink.clone());
        }
        assert!(registry.reserve(9, 2000).is_err());
        registry.unregister(&1);
        add(&registry, 9, 2000, sink);
    }

    #[test]
    fn physical_dispatch_respects_learning_off_and_never_replays_recent_events() {
        let registry = Registry::default();
        let (sink, rx) = counting();
        add(&registry, 1, 1000, sink);
        let mut service = buttons();
        service.set_learn_mode(true, 0);
        registry.handle_press(&mut service, -30, 110, 10, true);
        service.set_learn_mode(false, 20);
        registry.handle_press(&mut service, -30, 110, 30, false);
        registry.handle_press(&mut service, 30, 110, 40, true);
        registry.handle_press(&mut service, -30, 500, 50, true);
        assert!(!service.recent(0).is_empty());
        drain(&registry);
        assert_eq!(rx.try_iter().count(), 0);
        registry.handle_press(&mut service, -30, 110, 60, true);
        drain(&registry);
        assert_eq!(rx.try_iter().count(), 1);
        service.recent(0);
        drain(&registry);
        assert_eq!(rx.try_iter().count(), 0);
    }

    #[test]
    fn blocked_reentrant_sink_cannot_delay_other_listener_or_reader() {
        let registry = Arc::new(Registry::default());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let weak = Arc::downgrade(&registry);
        let a = Arc::new(Sink(Box::new(move |_| {
            // A transaction may reenter HUD while all dispatch locks are free.
            weak.upgrade().unwrap().unregister(&999);
            entered_tx.send(std::thread::current().id()).unwrap();
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
            true
        })));
        let (b, rx_b) = counting();
        add(&registry, 1, 1000, a);
        add(&registry, 2, 1000, b);
        registry.start().unwrap();
        let mut service = buttons();
        registry.handle_press(&mut service, -30, 110, 1, true);
        assert_ne!(
            entered_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            std::thread::current().id()
        );
        assert_eq!(
            rx_b.recv_timeout(Duration::from_secs(1)).unwrap(),
            ButtonSystemAction::ShowGlobalActions
        );
        let started = std::time::Instant::now();
        registry.handle_press(&mut service, -30, 110, 2, true);
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "reader must not wait for A"
        );
        release_tx.send(()).unwrap();
        assert!(
            entered_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "inflight SHOW coalesced"
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Token(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Reserve {
    Existing,
    New(Token),
}

struct Entry<H, V> {
    handle: H,
    uid: u32,
    token: Token,
    value: Option<Arc<V>>,
}

struct State<H, V> {
    next: u64,
    entries: [Option<Entry<H, V>>; GLOBAL_LIMIT],
    pending: [Option<Token>; GLOBAL_LIMIT],
    busy: [bool; GLOBAL_LIMIT],
}

pub(crate) struct ActionRegistry<H, V> {
    state: Arc<Mutex<State<H, V>>>,
    changed: Arc<Condvar>,
}

impl<H, V> Default for ActionRegistry<H, V> {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                next: 1,
                entries: std::array::from_fn(|_| None),
                pending: [None; GLOBAL_LIMIT],
                busy: [false; GLOBAL_LIMIT],
            })),
            changed: Arc::new(Condvar::new()),
        }
    }
}

impl<H: Eq + Send + 'static, V: ActionSink> ActionRegistry<H, V> {
    /// Reserve quota before linking. A death or unregister can retire the
    /// reservation, so publication cannot resurrect a dead pending listener.
    pub(crate) fn reserve(&self, handle: H, uid: u32) -> Result<Reserve, &'static str> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // Concurrent duplicate waits for the original link result, bounded so
        // a stalled link cannot consume every Binder pool thread indefinitely.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match state.entries.iter().flatten().find(|e| e.handle == handle) {
                Some(entry) if entry.value.is_some() => return Ok(Reserve::Existing),
                Some(_) => {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        return Err("listener registration still pending");
                    }
                    state = self
                        .changed
                        .wait_timeout(state, remaining)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
                None => break,
            }
        }
        if state
            .entries
            .iter()
            .flatten()
            .filter(|e| e.uid == uid)
            .count()
            >= UID_LIMIT
        {
            return Err("button action per-UID quota exhausted");
        }
        let index = state
            .entries
            .iter()
            .position(Option::is_none)
            .ok_or("button action global quota exhausted")?;
        let token = Token(state.next);
        state.next = state
            .next
            .checked_add(1)
            .ok_or("button action generation exhausted")?;
        state.entries[index] = Some(Entry {
            handle,
            uid,
            token,
            value: None,
        });
        Ok(Reserve::New(token))
    }

    pub(crate) fn publish(&self, token: Token, value: Arc<V>) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let result = if let Some(entry) = state
            .entries
            .iter_mut()
            .flatten()
            .find(|e| e.token == token)
        {
            entry.value = Some(value);
            true
        } else {
            false
        };
        self.changed.notify_all();
        result
    }

    pub(crate) fn remove_token(&self, token: Token) {
        let removed = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state
                .entries
                .iter_mut()
                .find(|e| e.as_ref().is_some_and(|e| e.token == token))
                .and_then(Option::take)
        };
        self.changed.notify_all();
        drop(removed); // Binder unlink/drop is always outside the registry lock.
    }

    pub(crate) fn unregister(&self, handle: &H) {
        let removed = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state
                .entries
                .iter_mut()
                .find(|e| e.as_ref().is_some_and(|e| &e.handle == handle))
                .and_then(Option::take)
        };
        self.changed.notify_all();
        drop(removed);
    }

    /// Called with the button lock held. Match and listener snapshot share
    /// the registry lock; a newly registered callback cannot receive old work.
    pub(crate) fn handle_press(
        &self,
        buttons: &mut ButtonService,
        x: i32,
        y: i32,
        now: i64,
        enabled: bool,
    ) -> PressAction {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let action = super::handle_press(buttons, x, y, now, 0, enabled);
        if action.system_action == ButtonSystemAction::ShowGlobalActions {
            for index in 0..GLOBAL_LIMIT {
                if !state.busy[index] {
                    if let Some(entry) = &state.entries[index] {
                        if entry.value.is_some() {
                            state.pending[index] = Some(entry.token);
                        }
                    }
                }
            }
            self.changed.notify_all();
        }
        action
    }

    /// Taking the callback here linearizes dispatch against unregister. An
    /// already taken transaction may finish; queued stale tokens are skipped.
    fn dispatch_slot(&self, index: usize) -> bool {
        let work = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(token) = state.pending[index].take() else {
                return false;
            };
            let value = state.entries[index]
                .as_ref()
                .filter(|e| e.token == token)
                .and_then(|e| e.value.clone());
            value.map(|value| {
                state.busy[index] = true;
                (token, value)
            })
        };
        let Some((token, value)) = work else {
            return false;
        };
        let success = value.send(ButtonSystemAction::ShowGlobalActions);
        {
            self.state.lock().unwrap_or_else(|e| e.into_inner()).busy[index] = false;
        }
        if !success {
            self.remove_token(token);
        }
        true
    }

    /// Fixed worker pool: a slow listener never occupies another live
    /// listener's worker, nor causes unbounded threads across re-registration.
    pub(crate) fn start(self: &Arc<Self>) -> std::io::Result<()> {
        for index in 0..GLOBAL_LIMIT {
            let weak = Arc::downgrade(self);
            let state = Arc::clone(&self.state);
            let changed = Arc::clone(&self.changed);
            std::thread::Builder::new()
                .name(format!("hud-action-{index}"))
                .spawn(move || {
                    while let Some(registry) = weak.upgrade() {
                        let dispatched = registry.dispatch_slot(index);
                        drop(registry); // Sleeping workers do not keep the registry alive.
                        if !dispatched {
                            let guard = state.lock().unwrap_or_else(|e| e.into_inner());
                            if guard.pending[index].is_none() {
                                drop(
                                    changed
                                        .wait_timeout(guard, Duration::from_millis(100))
                                        .unwrap_or_else(|e| e.into_inner()),
                                );
                            }
                        }
                    }
                })?;
        }
        Ok(())
    }

    #[cfg(feature = "binder-desk")]
    pub(crate) fn drain_for_desk(&self) {
        for index in 0..GLOBAL_LIMIT {
            self.dispatch_slot(index);
        }
    }
}
