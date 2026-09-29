// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Generation-safe single-callback state for the frozen HUD V1 API.
//!
//! V1 deliberately has one callback slot.  The Binder adapter must therefore
//! distinguish a late death/delivery failure for an old client from the
//! current replacement.  This module is Binder-free so those races stay
//! directly host-testable.

/// Opaque identity of one published callback generation.
///
/// `identity` names the subscription attempt and `generation` orders attempts
/// that overlap while a remote Binder `link_to_death` call is in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallbackToken {
    /// Per-subscribe identity used by the core's V1 unsubscribe slot.
    pub identity: u64,
    /// Monotonic publish order, including concurrent link attempts.
    pub generation: u64,
}

/// One value together with the token that owns it.
#[derive(Debug)]
pub struct Registration<T> {
    /// Exact owner token for `value`.
    pub token: CallbackToken,
    /// Caller-owned registration payload.
    pub value: T,
}

/// Result of publishing a linked callback.
#[derive(Debug)]
pub enum Publish<T> {
    /// The candidate became current; this is the replaced registration, if any.
    Published(Option<Registration<T>>),
    /// A newer subscribe attempt won while this candidate was linking.
    /// The caller must dispose of this un-published value outside its lock.
    Superseded(Registration<T>),
}

/// Tiny state machine for the V1 single callback slot.
///
/// The owner supplies synchronization.  Keeping this type lock-free by itself
/// makes it possible to prove the replacement/death ordering without Binder,
/// and forces the adapter to keep Binder calls outside its registry mutex.
#[derive(Debug)]
pub struct GenerationRegistry<T> {
    next_generation: u64,
    current: Option<Registration<T>>,
}

impl<T> Default for GenerationRegistry<T> {
    fn default() -> Self {
        Self {
            next_generation: 1,
            current: None,
        }
    }
}

impl<T> GenerationRegistry<T> {
    /// Reserve a monotonically ordered token before the caller links death.
    pub fn reserve(&mut self, identity: u64) -> CallbackToken {
        let token = CallbackToken {
            identity,
            generation: self.next_generation,
        };
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        token
    }

    /// Publish a candidate only when no newer reservation has become current.
    pub fn publish(&mut self, token: CallbackToken, value: T) -> Publish<T> {
        let candidate = Registration { token, value };
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.token.generation > token.generation)
        {
            return Publish::Superseded(candidate);
        }
        Publish::Published(self.current.replace(candidate))
    }

    /// Remove the current registration only when the exact token still owns it.
    pub fn remove_if_current(&mut self, token: CallbackToken) -> Option<Registration<T>> {
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.token == token)
        {
            self.current.take()
        } else {
            None
        }
    }

    /// Inspect the current value only when it is still owned by `token`.
    pub fn get_if_current(&self, token: CallbackToken) -> Option<&T> {
        self.current
            .as_ref()
            .filter(|current| current.token == token)
            .map(|current| &current.value)
    }

    /// Perform a local publication only while `token` remains current.
    ///
    /// The Binder adapter calls this while holding its registry mutex, then
    /// updates the core slot inside `publish`. The closure must stay local: it
    /// must not call a remote Binder or any other potentially re-entrant API.
    pub fn publish_if_current<F: FnOnce()>(&self, token: CallbackToken, publish: F) -> bool {
        if self.get_if_current(token).is_none() {
            return false;
        }
        publish();
        true
    }

    /// Inspect the currently published registration.
    pub fn current(&self) -> Option<&Registration<T>> {
        self.current.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_death_cannot_clear_a_replacement() {
        let mut registry = GenerationRegistry::default();
        let a = registry.reserve(41);
        assert!(matches!(registry.publish(a, "A"), Publish::Published(None)));
        let b = registry.reserve(42);
        assert!(matches!(
            registry.publish(b, "B"),
            Publish::Published(Some(_))
        ));

        assert!(registry.remove_if_current(a).is_none());
        assert_eq!(registry.get_if_current(b), Some(&"B"));
    }

    #[test]
    fn stale_failed_delivery_cannot_clear_a_same_identity_replacement() {
        let mut registry = GenerationRegistry::default();
        let a = registry.reserve(7);
        let _ = registry.publish(a, "first");
        let b = registry.reserve(7);
        let _ = registry.publish(b, "replacement");

        assert!(registry.remove_if_current(a).is_none());
        assert_eq!(registry.get_if_current(b), Some(&"replacement"));
        assert_eq!(
            registry.remove_if_current(b).map(|entry| entry.value),
            Some("replacement")
        );
    }

    #[test]
    fn later_subscribe_wins_when_an_earlier_link_returns_late() {
        let mut registry = GenerationRegistry::default();
        let slow_a = registry.reserve(10);
        let fast_b = registry.reserve(11);
        assert!(matches!(
            registry.publish(fast_b, "B"),
            Publish::Published(None)
        ));

        match registry.publish(slow_a, "A") {
            Publish::Superseded(entry) => assert_eq!(entry.value, "A"),
            Publish::Published(_) => panic!("older linked callback displaced newer callback"),
        }
        assert_eq!(registry.get_if_current(fast_b), Some(&"B"));
    }

    #[test]
    fn reordered_core_publication_cannot_restore_the_old_callback() {
        let mut registry = GenerationRegistry::default();
        let a = registry.reserve(100);
        let _ = registry.publish(a, "A");
        let b = registry.reserve(101);
        let _ = registry.publish(b, "B");
        let mut core_callback = "none";

        // A finished Binder linking first but was delayed before it could
        // publish the core callback. B is already current, so A is forbidden
        // from restoring itself into the V1 core slot.
        assert!(!registry.publish_if_current(a, || core_callback = "A"));
        assert_eq!(core_callback, "none");
        assert!(registry.publish_if_current(b, || core_callback = "B"));
        assert_eq!(core_callback, "B");
    }
}
