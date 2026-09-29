// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Typed, source-only OEM status requests for XinPu Volkswagen PQ.
//!
//! This module builds a six-entry allowlist of complete Raise `0x2e` UART
//! envelopes and applies a bounded queue policy.  It deliberately owns no
//! file descriptor and has no `Write`/UART dependency: [`RequestPolicy::poll`]
//! only returns bytes to a future, separately reviewed transport owner.
//!
//! The allowlist is for user `2009002` only.  It excludes acknowledgements,
//! the `0x81 01` session switch, settings writes, metadata, calibration,
//! MCU broadcasts, and DSP broadcasts.  The latter are distinct destinations
//! in the OEM catalogue and must not be smuggled into this status-request
//! backend.

use std::collections::VecDeque;

#[cfg(test)]
use super::envelope::{encode_frame, Item, Parser};

/// The only model/user ID accepted by this request slice.
pub const XINPU_VW_PQ_USER: u32 = 2_009_002;
/// Minimum interval between emitted non-ACK status requests.
pub const MIN_REQUEST_INTERVAL_MS: u64 = 250;
/// Pending requests expire after this interval, preventing stale replay.
pub const REQUEST_TTL_MS: u64 = 5_000;
/// Default queue bound.
pub const DEFAULT_QUEUE_CAPACITY: usize = 8;

const CAR_INFO_VEHICLE: &[u8] = &[0x2e, 0x90, 0x02, 0x41, 0x02, 0x2a];
const CAR_INFO_ALERTS: &[u8] = &[0x2e, 0x90, 0x02, 0x41, 0x03, 0x29];
const TYRES: &[u8] = &[0x2e, 0x90, 0x02, 0x37, 0x00, 0x36];
const AMPLIFIER: &[u8] = &[0x2e, 0x90, 0x02, 0x27, 0x00, 0x46];
const SETTINGS: &[u8] = &[0x2e, 0x90, 0x03, 0x42, 0x00, 0x00, 0x2a];
const MEDIA_BOX: &[u8] = &[0x2e, 0x90, 0x02, 0x29, 0x00, 0x44];

const REPLY_CAR_INFO_VEHICLE: &[ReplySelector] = &[ReplySelector::new(0x41, Some(0x02))];
const REPLY_CAR_INFO_ALERTS: &[ReplySelector] = &[ReplySelector::new(0x41, Some(0x03))];
const REPLY_TYRES: &[ReplySelector] = &[ReplySelector::new(0x37, None)];
const REPLY_AMPLIFIER: &[ReplySelector] = &[ReplySelector::new(0x27, None)];
const REPLY_SETTINGS: &[ReplySelector] = &[
    ReplySelector::new(0x25, None),
    ReplySelector::new(0x28, None),
];
const REPLY_MEDIA_BOX: &[ReplySelector] = &[
    ReplySelector::new(0x29, None),
    ReplySelector::new(0x2a, None),
];

/// One allowlisted typed OEM status request.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RequestKind {
    /// Request `0x41/02` RPM, speed, voltage, odometer and fuel.
    CarInfoVehicle,
    /// Request `0x41/03` low-fuel/low-battery alerts.
    CarInfoAlerts,
    /// Request `0x37` tyre-pressure status.
    Tyres,
    /// Request `0x27` amplifier status.
    Amplifier,
    /// Request `0x42` car-settings readback.
    Settings,
    /// Request `0x29` Media Box status.
    MediaBox,
}

impl RequestKind {
    /// Return the complete, checksummed UART frame for this request.
    pub fn frame(self) -> &'static [u8] {
        match self {
            Self::CarInfoVehicle => CAR_INFO_VEHICLE,
            Self::CarInfoAlerts => CAR_INFO_ALERTS,
            Self::Tyres => TYRES,
            Self::Amplifier => AMPLIFIER,
            Self::Settings => SETTINGS,
            Self::MediaBox => MEDIA_BOX,
        }
    }

    /// Return the expected box reply selector(s).
    pub fn expected_replies(self) -> &'static [ReplySelector] {
        match self {
            Self::CarInfoVehicle => REPLY_CAR_INFO_VEHICLE,
            Self::CarInfoAlerts => REPLY_CAR_INFO_ALERTS,
            Self::Tyres => REPLY_TYRES,
            Self::Amplifier => REPLY_AMPLIFIER,
            Self::Settings => REPLY_SETTINGS,
            Self::MediaBox => REPLY_MEDIA_BOX,
        }
    }
}

/// A box reply identified by UART TYPE and optionally its first DATA byte.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReplySelector {
    /// UART TYPE byte.
    pub message_type: u8,
    /// First DATA byte, when the response has a meaningful subtype.
    pub subtype: Option<u8>,
}

impl ReplySelector {
    /// Construct a reply selector.
    pub const fn new(message_type: u8, subtype: Option<u8>) -> Self {
        Self {
            message_type,
            subtype,
        }
    }

    /// Match a parsed envelope's type and data prefix.
    pub fn matches(self, message_type: u8, data: &[u8]) -> bool {
        message_type == self.message_type
            && self
                .subtype
                .map_or(true, |subtype| data.first() == Some(&subtype))
    }
}

/// A request ready for a future transport owner to send.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutboundRequest {
    /// Typed request kind.
    pub kind: RequestKind,
    /// Session generation at which it was queued.
    pub session: u64,
    /// Complete checksummed UART bytes.
    pub frame: &'static [u8],
}

/// Why a request could not be queued.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestError {
    /// The caller has not explicitly enabled requests.
    Disabled,
    /// ACC is not confirmed on.
    AccOff,
    /// SE5 identity has not been positively proven.
    Se5Unproven,
    /// The selected profile is absent or is not user 2009002.
    WrongModel,
    /// The bounded queue is full of different, unexpired requests.
    QueueFull,
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Disabled => "disabled",
            Self::AccOff => "ACC-off",
            Self::Se5Unproven => "SE5 identity unproven",
            Self::WrongModel => "wrong profile",
            Self::QueueFull => "queue full",
        })
    }
}

/// Result of adding a request to the bounded queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnqueueResult {
    /// A new request occupied one queue slot.
    Queued,
    /// An existing same-kind request was replaced/coalesced.
    Coalesced,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Pending {
    kind: RequestKind,
    expires_at_ms: u64,
    session: u64,
}

/// Gate and bounded queue for future typed status-request transport.
///
/// All gates default closed.  Importing this module, constructing a policy,
/// or setting a profile never enables or emits a request.  A caller must
/// explicitly set enabled, ACC-on, positively proven SE5, and user 2009002,
/// then enqueue a request.  State transitions cancel pending work; reconnect
/// additionally advances [`session`](Self::session), so stale work cannot be
/// replayed into a replacement UART session.
#[derive(Debug)]
pub struct RequestPolicy {
    enabled: bool,
    acc_on: bool,
    se5_proven: bool,
    profile_user: Option<u32>,
    session: u64,
    queue_capacity: usize,
    queue: VecDeque<Pending>,
    last_sent_ms: Option<u64>,
}

impl Default for RequestPolicy {
    fn default() -> Self {
        Self::new(DEFAULT_QUEUE_CAPACITY)
    }
}

impl RequestPolicy {
    /// Construct a policy with a finite queue capacity.
    pub fn new(queue_capacity: usize) -> Self {
        Self {
            enabled: false,
            acc_on: false,
            se5_proven: false,
            profile_user: None,
            session: 0,
            queue_capacity,
            queue: VecDeque::new(),
            last_sent_ms: None,
        }
    }

    /// Explicitly arm or disarm request scheduling. Disarming cancels work.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.cancel_pending();
        }
    }

    /// Update confirmed ACC state. ACC-off cancels work.
    pub fn set_acc_on(&mut self, acc_on: bool) {
        self.acc_on = acc_on;
        if !acc_on {
            self.cancel_pending();
        }
    }

    /// Update positive SE5 identity. Losing identity cancels work.
    pub fn set_se5_proven(&mut self, se5_proven: bool) {
        self.se5_proven = se5_proven;
        if !se5_proven {
            self.cancel_pending();
        }
    }

    /// Select a profile user. Changing it cancels work; only 2009002 queues.
    pub fn set_profile_user(&mut self, profile_user: Option<u32>) {
        if self.profile_user != profile_user {
            self.profile_user = profile_user;
            self.cancel_pending();
            self.session = self.session.wrapping_add(1);
        }
    }

    /// Replace the UART session and discard all requests from the old one.
    pub fn reconnect(&mut self) {
        self.session = self.session.wrapping_add(1);
        self.cancel_pending();
    }

    /// Current session generation.
    pub fn session(&self) -> u64 {
        self.session
    }

    /// Number of pending, unexpired requests.
    pub fn pending_len(&mut self, now_ms: u64) -> usize {
        self.expire(now_ms);
        self.queue.len()
    }

    /// Queue a typed request if all explicit prerequisites are true.
    pub fn enqueue(
        &mut self,
        kind: RequestKind,
        now_ms: u64,
    ) -> Result<EnqueueResult, RequestError> {
        self.check_gates()?;
        self.expire(now_ms);
        let expires_at_ms = now_ms.saturating_add(REQUEST_TTL_MS);
        if let Some(existing) = self.queue.iter_mut().find(|pending| pending.kind == kind) {
            existing.expires_at_ms = expires_at_ms;
            existing.session = self.session;
            return Ok(EnqueueResult::Coalesced);
        }
        if self.queue.len() >= self.queue_capacity {
            return Err(RequestError::QueueFull);
        }
        self.queue.push_back(Pending {
            kind,
            expires_at_ms,
            session: self.session,
        });
        Ok(EnqueueResult::Queued)
    }

    /// Queue a batch atomically, preserving the FIFO order supplied by the
    /// caller.  The refresh owner uses this to ensure a six-request refresh
    /// is either wholly queued or not queued at all.
    pub fn enqueue_batch(
        &mut self,
        kinds: &[RequestKind],
        now_ms: u64,
    ) -> Result<(), RequestError> {
        self.check_gates()?;
        self.expire(now_ms);
        let new_count = kinds
            .iter()
            .filter(|kind| !self.queue.iter().any(|pending| pending.kind == **kind))
            .count();
        if self.queue.len().saturating_add(new_count) > self.queue_capacity {
            return Err(RequestError::QueueFull);
        }
        for kind in kinds {
            let expires_at_ms = now_ms.saturating_add(REQUEST_TTL_MS);
            if let Some(existing) = self.queue.iter_mut().find(|pending| pending.kind == *kind) {
                existing.expires_at_ms = expires_at_ms;
                existing.session = self.session;
            } else {
                self.queue.push_back(Pending {
                    kind: *kind,
                    expires_at_ms,
                    session: self.session,
                });
            }
        }
        Ok(())
    }

    /// Cancel all pending requests without changing gates or session.
    pub fn cancel_pending(&mut self) {
        self.queue.clear();
    }

    /// Return and remove one due request, respecting the 250 ms rate limit.
    ///
    /// This does not write bytes.  The caller remains responsible for the
    /// separate transport decision and for any response/ACK handling.
    pub fn poll(&mut self, now_ms: u64) -> Option<OutboundRequest> {
        self.expire(now_ms);
        if self.check_gates().is_err() {
            return None;
        }
        if self
            .last_sent_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < MIN_REQUEST_INTERVAL_MS)
        {
            return None;
        }
        while let Some(pending) = self.queue.pop_front() {
            if pending.session != self.session || pending.expires_at_ms <= now_ms {
                continue;
            }
            self.last_sent_ms = Some(now_ms);
            return Some(OutboundRequest {
                kind: pending.kind,
                session: self.session,
                frame: pending.kind.frame(),
            });
        }
        None
    }

    fn check_gates(&self) -> Result<(), RequestError> {
        if !self.enabled {
            return Err(RequestError::Disabled);
        }
        if !self.acc_on {
            return Err(RequestError::AccOff);
        }
        if !self.se5_proven {
            return Err(RequestError::Se5Unproven);
        }
        if self.profile_user != Some(XINPU_VW_PQ_USER) {
            return Err(RequestError::WrongModel);
        }
        Ok(())
    }

    fn expire(&mut self, now_ms: u64) {
        self.queue
            .retain(|pending| pending.session == self.session && pending.expires_at_ms > now_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armed(capacity: usize) -> RequestPolicy {
        let mut policy = RequestPolicy::new(capacity);
        policy.set_profile_user(Some(XINPU_VW_PQ_USER));
        policy.set_se5_proven(true);
        policy.set_acc_on(true);
        policy.set_enabled(true);
        policy
    }

    #[test]
    fn exact_frames_roundtrip_through_oem_parser() {
        let kinds = [
            RequestKind::CarInfoVehicle,
            RequestKind::CarInfoAlerts,
            RequestKind::Tyres,
            RequestKind::Amplifier,
            RequestKind::Settings,
            RequestKind::MediaBox,
        ];
        for kind in kinds {
            let mut parser = Parser::new();
            let items = parser.push(kind.frame());
            assert!(matches!(&items[..], [Item::Frame(frame)] if frame.checksum_ok()));
            let frame = match &items[0] {
                Item::Frame(frame) => frame,
                _ => unreachable!(),
            };
            assert_eq!(frame.raw, kind.frame());
            assert!(
                kind.expected_replies()
                    .iter()
                    .any(|reply| reply.matches(frame.message_type, &frame.data))
                    == false
            );
        }
        assert_eq!(
            RequestKind::CarInfoVehicle.frame(),
            &[0x2e, 0x90, 0x02, 0x41, 0x02, 0x2a]
        );
        assert_eq!(
            RequestKind::CarInfoAlerts.frame(),
            &[0x2e, 0x90, 0x02, 0x41, 0x03, 0x29]
        );
        assert_eq!(
            RequestKind::Tyres.frame(),
            &[0x2e, 0x90, 0x02, 0x37, 0x00, 0x36]
        );
        assert_eq!(
            RequestKind::Amplifier.frame(),
            &[0x2e, 0x90, 0x02, 0x27, 0x00, 0x46]
        );
        assert_eq!(
            RequestKind::Settings.frame(),
            &[0x2e, 0x90, 0x03, 0x42, 0x00, 0x00, 0x2a]
        );
        assert_eq!(
            RequestKind::MediaBox.frame(),
            &[0x2e, 0x90, 0x02, 0x29, 0x00, 0x44]
        );
    }

    #[test]
    fn expected_reply_selectors_match_subtypes() {
        assert!(ReplySelector::new(0x41, Some(2)).matches(0x41, &[2, 0]));
        assert!(!ReplySelector::new(0x41, Some(2)).matches(0x41, &[3, 0]));
        assert!(RequestKind::Settings
            .expected_replies()
            .contains(&ReplySelector::new(0x25, None)));
        assert!(RequestKind::MediaBox
            .expected_replies()
            .contains(&ReplySelector::new(0x2a, None)));
    }

    #[test]
    fn gates_deny_without_all_positive_prerequisites() {
        let mut policy = RequestPolicy::default();
        assert_eq!(
            policy.enqueue(RequestKind::Tyres, 0),
            Err(RequestError::Disabled)
        );
        policy.set_enabled(true);
        assert_eq!(
            policy.enqueue(RequestKind::Tyres, 0),
            Err(RequestError::AccOff)
        );
        policy.set_acc_on(true);
        assert_eq!(
            policy.enqueue(RequestKind::Tyres, 0),
            Err(RequestError::Se5Unproven)
        );
        policy.set_se5_proven(true);
        assert_eq!(
            policy.enqueue(RequestKind::Tyres, 0),
            Err(RequestError::WrongModel)
        );
        policy.set_profile_user(Some(2009001));
        assert_eq!(
            policy.enqueue(RequestKind::Tyres, 0),
            Err(RequestError::WrongModel)
        );
    }

    #[test]
    fn queue_coalesces_bounds_expires_and_rate_limits() {
        let mut policy = armed(2);
        assert_eq!(
            policy.enqueue(RequestKind::Tyres, 0),
            Ok(EnqueueResult::Queued)
        );
        assert_eq!(
            policy.enqueue(RequestKind::Tyres, 1),
            Ok(EnqueueResult::Coalesced)
        );
        assert_eq!(
            policy.enqueue(RequestKind::Amplifier, 2),
            Ok(EnqueueResult::Queued)
        );
        assert_eq!(
            policy.enqueue(RequestKind::Settings, 3),
            Err(RequestError::QueueFull)
        );
        let first = policy.poll(1).expect("first request");
        assert_eq!(first.kind, RequestKind::Tyres);
        assert!(
            policy.poll(200).is_none(),
            "non-ACK requests need 250 ms spacing"
        );
        assert_eq!(
            policy.poll(251).expect("second request").kind,
            RequestKind::Amplifier
        );
        assert_eq!(
            policy.enqueue(RequestKind::Settings, 300),
            Ok(EnqueueResult::Queued)
        );
        assert_eq!(policy.pending_len(5_300), 0, "stale requests expire");
    }

    #[test]
    fn acc_disable_and_session_change_cancel_without_replay() {
        let mut policy = armed(8);
        policy.enqueue(RequestKind::Tyres, 0).unwrap();
        policy.set_acc_on(false);
        assert_eq!(policy.pending_len(0), 0);
        policy.set_acc_on(true);
        policy.enqueue(RequestKind::Tyres, 1).unwrap();
        policy.set_enabled(false);
        assert!(policy.poll(1).is_none());
        policy.set_enabled(true);
        policy.enqueue(RequestKind::Tyres, 2).unwrap();
        let old_session = policy.session();
        policy.reconnect();
        assert_ne!(policy.session(), old_session);
        assert!(policy.poll(3).is_none());
    }

    #[test]
    fn generated_request_uses_existing_checksum_encoder() {
        for kind in [RequestKind::CarInfoVehicle, RequestKind::Settings] {
            let expected = encode_frame(kind.frame()[1], &kind.frame()[3..kind.frame().len() - 1]);
            assert_eq!(expected, kind.frame());
        }
    }
}
