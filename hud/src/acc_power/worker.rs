// SPDX-License-Identifier: Apache-2.0
//! One persistent I/O worker, one pending/active offered socket. No listener.

use super::broker::{Peer, Producer, Shared};
use super::codec::{Kind, LIMIT};
use super::transport::{self, PeerIdentity, PollFd, POLLIN, POLLOUT, POLL_FAILURE};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

// Deterministic test interleaving at the actual check/front boundary. Local to
// the calling test thread, never present in production or other worker tests.
#[cfg(test)]
thread_local! {
    static AFTER_IDLE_STEP_CHECK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

struct Offer {
    fd: OwnedFd,
    identity: PeerIdentity,
    accepted_ms: u64,
}
struct Slot {
    busy: AtomicBool,
    pending: Mutex<Option<Offer>>,
}

/// A fixed lifetime worker shared by the standalone Binder bootstrap and UART.
/// Construction is property gated by the caller; no hardware/sleep policy here.
pub(crate) struct Channel {
    participants: Option<super::participants::Worker>,
    shared: Arc<Shared>,
    wake: Arc<OwnedFd>,
    slot: Arc<Slot>,
    thread: Option<JoinHandle<()>>,
}

impl Channel {
    pub fn new() -> io::Result<Self> {
        let shared = Shared::new();
        let wake = Arc::new(transport::wake_fd()?);
        let slot = Arc::new(Slot {
            busy: AtomicBool::new(false),
            pending: Mutex::new(None),
        });
        let (s, w, q) = (shared.clone(), wake.clone(), slot.clone());
        let thread = thread::Builder::new()
            .name("acc-power-io".into())
            .spawn(move || run(s, w, q))?;
        Ok(Self {
            participants: None,
            shared,
            wake,
            slot,
            thread: Some(thread),
        })
    }

    pub(crate) fn start_participants(&mut self, owner: impl super::participants::Owner) -> io::Result<()> {
        self.participants = Some(super::participants::Worker::start(self.shared.native.clone(), owner)?);
        Ok(())
    }

    pub(crate) fn native_validity(&self) -> Arc<AtomicBool> {
        self.shared.native.validity.clone()
    }

    /// Called inside the actual Binder transaction. All validation is finite,
    /// local FD work; failed offers cannot evict or replace a reserved owner.
    /// The borrowed PFD remains valid until its checked duplicate is published.
    pub fn offer(&self, fd: &OwnedFd, calling_uid: u32) -> bool {
        self.offer_with(fd, calling_uid, transport::authenticate)
    }

    fn offer_with(
        &self,
        fd: &OwnedFd,
        calling_uid: u32,
        authenticate: impl FnOnce(i32, u32) -> io::Result<PeerIdentity>,
    ) -> bool {
        if self.shared.stop.load(Ordering::Acquire)
            || self
                .slot
                .busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return false;
        }
        let offer = (|| {
            let identity = authenticate(fd.as_raw_fd(), calling_uid)?;
            if !identity.valid() {
                return Err(io::Error::other("invalid retained identity"));
            }
            let accepted_ms = transport::boottime_ms()?;
            if accepted_ms
                .checked_add(2000)
                .filter(|v| *v <= LIMIT)
                .is_none()
            {
                return Err(io::Error::other("deadline overflow"));
            }
            let fd = fd.try_clone()?;
            Ok::<_, io::Error>(Offer {
                fd,
                identity,
                accepted_ms,
            })
        })();
        let published = match offer {
            Ok(offer) => match self.slot.pending.try_lock() {
                Ok(mut pending)
                    if pending.is_none() && !self.shared.stop.load(Ordering::Acquire) =>
                {
                    *pending = Some(offer);
                    true
                }
                _ => false,
            },
            Err(_) => false,
        };
        if !published {
            self.slot.busy.store(false, Ordering::Release);
        } else {
            self.notify();
        }
        published
    }

    /// Generate a fresh session identity before constructing EACH AccSession.
    /// Failure disables this channel session, without changing legacy UART I/O.
    pub fn new_uart(&self) -> Option<Producer> {
        self.shared.session_alive.store(false, Ordering::Release);
        self.notify();
        let nonce = transport::random_nonce().ok()?;
        Producer::new(self.shared.clone(), nonce)
    }

    /// Only after serial returns (or other non-hook invalidation/admission).
    pub fn notify(&self) {
        if let Some(worker) = &self.participants { worker.notify(); }
        transport::notify(self.wake.as_raw_fd());
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.notify();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Connection {
    fd: OwnedFd,
    // Retain complete authenticated identity for this exact owned descriptor.
    _identity: PeerIdentity,
    peer: Peer,
    stalled_until: Option<u64>,
}

impl Connection {
    fn invalidate(&self, shared: &Shared) {
        shared.poison(self.peer.epoch);
        transport::disconnect(&self.fd);
    }

    fn dispatch(&mut self, shared: &Shared, now: u64) -> bool {
        if !shared.healthy(self.peer.epoch, self.peer.ticket)
            || (!self.peer.started && now >= self.peer.handshake_deadline)
            || self.stalled_until.map(|d| now >= d).unwrap_or(false)
        {
            return false;
        }
        // Bound inbound processing; malformed/flooded controls never monopolize
        // the worker or displace another owner. Every FD-bearing record rejects.
        for _ in 0..8 {
            match transport::receive(self.fd.as_raw_fd()) {
                Ok(Some(packet)) if self.peer.receive(shared, packet, now) => {}
                Ok(None) => break,
                _ => return false,
            }
        }
        for _ in 0..32 {
            if !shared.healthy(self.peer.epoch, self.peer.ticket) {
                return false;
            }
            if shared.step_active.load(Ordering::Acquire) {
                break;
            }
            #[cfg(test)]
            AFTER_IDLE_STEP_CHECK.with(|slot| {
                if let Some(action) = slot.borrow_mut().take() {
                    action();
                }
            });
            // This check is not a per-record commit fence: a new step may
            // enqueue its truthful FINAL_RESULT before front() locks the queue.
            // AUTHORIZE follows that result. Acquiring the queue mutex imports
            // the producing step's prior true store, so the next loop check
            // holds its AUTHORIZE until after_step publishes completion (or
            // failure poisons the connection). Keep the check on EVERY record.
            let Some(record) = shared.front(self.peer.epoch) else {
                break;
            };
            if self.peer.snapshot.is_some() && !self.peer.started {
                break;
            }
            // Cancellation/expiry only suppresses this cycle's optional
            // AUTHORIZE. It does not discard sequenced facts or poison owner.
            if record.packet.kind == Kind::Authorize
                && (now >= record.packet.deadline
                    || shared.cycle.load(Ordering::Acquire) != record.packet.generation)
            {
                shared.consume(record);
                continue;
            }
            // Sample immediately before every send, not just dispatch entry.
            let Ok(fresh) = transport::boottime_ms() else {
                return false;
            };
            if record.packet.kind == Kind::Authorize && fresh >= record.packet.deadline {
                shared.consume(record);
                continue;
            }
            if !self.peer.outbound_valid(shared, record, fresh) {
                return false;
            }
            match transport::transmit(self.fd.as_raw_fd(), record.packet) {
                Ok(true) => {
                    self.stalled_until = None;
                    if !self.peer.sent(record.packet) {
                        return false;
                    }
                    shared.consume(record);
                }
                Ok(false) => {
                    if self.stalled_until.is_none() {
                        self.stalled_until = fresh.checked_add(2000).filter(|d| *d <= LIMIT);
                        if self.stalled_until.is_none() {
                            return false;
                        }
                    }
                    break;
                }
                Err(_) => return false,
            }
        }
        shared.healthy(self.peer.epoch, self.peer.ticket)
    }
}

fn run(shared: Arc<Shared>, wake: Arc<OwnedFd>, slot: Arc<Slot>) {
    let mut connection: Option<Connection> = None;
    while !shared.stop.load(Ordering::Acquire) {
        if connection.is_none() {
            // Binder offer holds the slot reservation until this worker drops
            // the complete connection. There is no per-offer spawned worker.
            let offer = slot.pending.try_lock().ok().and_then(|mut p| p.take());
            if let Some(offer) = offer {
                let peer = transport::random_nonce()
                    .ok()
                    .and_then(|nonce| Peer::new(&shared, nonce, offer.accepted_ms));
                if let Some(peer) = peer {
                    connection = Some(Connection {
                        fd: offer.fd,
                        _identity: offer.identity,
                        peer,
                        stalled_until: None,
                    });
                } else {
                    transport::disconnect(&offer.fd);
                    slot.busy.store(false, Ordering::Release);
                }
            }
        }
        let Ok(now) = transport::boottime_ms() else {
            break;
        };
        if let Some(c) = connection.as_mut() {
            if !c.dispatch(&shared, now) {
                c.invalidate(&shared);
                connection = None;
                slot.busy.store(false, Ordering::Release);
            }
        }
        let mut fds = [
            PollFd {
                fd: wake.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            },
            PollFd {
                fd: -1,
                events: 0,
                revents: 0,
            },
        ];
        let mut timeout = -1;
        if let Some(c) = &connection {
            let outgoing = shared.front(c.peer.epoch).is_some()
                && !shared.step_active.load(Ordering::Acquire)
                && (c.peer.snapshot.is_none() || c.peer.started);
            fds[1] = PollFd {
                fd: c.fd.as_raw_fd(),
                events: POLLIN | if outgoing { POLLOUT } else { 0 },
                revents: 0,
            };
            let pending_deadline = if !c.peer.started {
                Some(c.peer.handshake_deadline)
            } else {
                c.stalled_until
                    .or(c.peer.preparation.map(|p| p.deadline).filter(|d| *d > now))
            };
            if let Some(deadline) = pending_deadline {
                timeout = deadline.saturating_sub(now).min(50) as i32;
            }
            if !shared.healthy(c.peer.epoch, c.peer.ticket) {
                timeout = 0;
            }
        }
        if transport::wait(&mut fds, timeout).is_err() {
            break;
        }
        if fds[0].revents & POLLIN != 0 {
            transport::drain_wake(wake.as_raw_fd());
        }
        if fds[1].revents & POLL_FAILURE != 0 {
            if let Some(c) = connection.take() {
                c.invalidate(&shared);
            }
            slot.busy.store(false, Ordering::Release);
        }
    }
    shared.stop.store(true, Ordering::Release);
    if let Some(c) = connection {
        c.invalidate(&shared);
    }
    if let Ok(mut pending) = slot.pending.lock() {
        if let Some(offer) = pending.take() {
            transport::disconnect(&offer.fd);
        }
    }
    slot.busy.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::super::codec::Packet;
    use super::super::transport::tests::{injected_auth, pair};
    use super::*;
    use crate::mcu::acc::{CycleEvent, HostWriteResult, State};
    use std::time::{Duration, Instant};

    fn until(mut condition: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(Instant::now() < end, "worker did not make bounded progress");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn packet(fd: &OwnedFd) -> Packet {
        let mut result = None;
        until(|| {
            result = transport::receive(fd.as_raw_fd()).unwrap();
            result.is_some()
        });
        result.unwrap()
    }
    #[test]
    fn one_worker_real_socketpair_handshake_cycle_cancel_death_and_uart_reopen() {
        let channel = Channel::new().unwrap();
        let worker_id = channel.thread.as_ref().unwrap().thread().id();
        let mut producer = channel.new_uart().unwrap();
        let (endpoint, client) = pair();
        assert!(!channel.offer_with(&endpoint, 10000, |_, _| Err(io::Error::other("deny"))));
        assert!(!channel.slot.busy.load(Ordering::Acquire));
        assert!(channel.offer_with(&endpoint, 10000, injected_auth));
        assert!(!channel.offer_with(&endpoint, 10000, injected_auth));
        drop(endpoint); // worker owns its independent duplicate
        assert!(transport::transmit(client.as_raw_fd(), Packet::hello()).unwrap());
        until(|| channel.shared.has_control());
        producer.checkpoint(State::Idle, 0, transport::boottime_ms().unwrap());
        producer.after_step(true);
        channel.notify();
        let snapshot = packet(&client);
        assert_eq!(
            (snapshot.kind, snapshot.phase, snapshot.generation),
            (Kind::Snapshot, 0, 0)
        );
        assert!(transport::transmit(
            client.as_raw_fd(),
            Packet {
                kind: Kind::Start,
                phase: 0,
                ..snapshot
            }
        )
        .unwrap());
        producer.checkpoint(State::Idle, 0, transport::boottime_ms().unwrap());
        producer.event(CycleEvent::Grace { off_generation: 1 });
        producer.event(CycleEvent::Prepare { off_generation: 1 });
        producer.after_step(true);
        channel.notify();
        assert_eq!(packet(&client).kind, Kind::Grace);
        let prepare = packet(&client);
        assert_eq!(prepare.kind, Kind::Prepare);
        assert!(transport::transmit(
            client.as_raw_fd(),
            Packet {
                kind: Kind::Ready,
                ..prepare
            }
        )
        .unwrap());
        until(|| channel.shared.has_control());
        channel.shared.native.test_complete_current(); // explicit successful native fixture
        producer.checkpoint(State::FinalStage, 1, transport::boottime_ms().unwrap());
        // A socket READY arrived before this explicit UART checkpoint.
        producer.event(CycleEvent::FinalWrite {
            off_generation: 1,
            result: HostWriteResult::Complete,
        });
        // Worker must not leak even queued authority until serial returns.
        channel.notify();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(transport::receive(client.as_raw_fd()).unwrap(), None);
        producer.after_step(true);
        channel.notify();
        let final_result = packet(&client);
        assert_eq!(final_result.kind, Kind::FinalResult);
        assert_eq!(packet(&client).kind, Kind::Authorize);
        assert!(transport::transmit(
            client.as_raw_fd(),
            Packet {
                kind: Kind::Cancel,
                deadline: 0,
                ..prepare
            }
        )
        .unwrap());
        until(|| channel.shared.cycle.load(Ordering::Acquire) == 0);
        assert!(channel.slot.busy.load(Ordering::Acquire)); // cycle revoke != poison
        let session = snapshot.session;
        let old_ticket = channel.shared.session_ticket.load(Ordering::Acquire);
        drop(client);
        until(|| !channel.slot.busy.load(Ordering::Acquire));
        let mut replacement = channel.new_uart().unwrap();
        drop(producer); // stale producer cannot invalidate the new UART
        assert!(channel.shared.session_alive.load(Ordering::Acquire));
        assert_ne!(
            old_ticket,
            channel.shared.session_ticket.load(Ordering::Acquire)
        );
        let (endpoint, client) = pair();
        let old_epoch = channel.shared.active_epoch.load(Ordering::Acquire);
        assert!(channel.offer_with(&endpoint, 10000, injected_auth));
        assert!(transport::transmit(client.as_raw_fd(), Packet::hello()).unwrap());
        until(|| {
            channel.shared.active_epoch.load(Ordering::Acquire) != old_epoch
                && channel.shared.has_control()
        });
        replacement.checkpoint(State::Idle, 0, transport::boottime_ms().unwrap());
        replacement.after_step(true);
        channel.notify();
        assert_ne!(packet(&client).session, session);
        assert_eq!(channel.thread.as_ref().unwrap().thread().id(), worker_id);
        drop(channel); // eventfd interrupts idle poll and joins this one worker
        assert!(transport::receive(client.as_raw_fd()).is_err());
    }
    #[test]
    fn false_step_check_may_release_final_fact_but_not_uncommitted_authorize() {
        use std::cell::RefCell;
        use std::rc::Rc;
        for success in [false, true] {
            let shared = Shared::new();
            let mut producer = Producer::new(shared.clone(), [1; 16]).unwrap();
            let (fd, client) = pair();
            let now = transport::boottime_ms().unwrap();
            let peer = Peer::new(&shared, [2; 16], now).unwrap();
            let mut connection = Connection {
                _identity: injected_auth(fd.as_raw_fd(), 10000).unwrap(),
                fd,
                peer,
                stalled_until: None,
            };
            assert!(connection.peer.receive(&shared, Packet::hello(), now));
            producer.checkpoint(State::Idle, 0, now);
            producer.after_step(true);
            assert!(connection.dispatch(&shared, now));
            let snapshot = packet(&client);
            assert!(connection.peer.receive(
                &shared,
                Packet {
                    kind: Kind::Start,
                    phase: 0,
                    ..snapshot
                },
                now
            ));
            producer.checkpoint(State::Idle, 0, now);
            producer.event(CycleEvent::Grace { off_generation: 1 });
            producer.event(CycleEvent::Prepare { off_generation: 1 });
            producer.after_step(true);
            assert!(connection.dispatch(&shared, now));
            assert_eq!(packet(&client).kind, Kind::Grace);
            let prepare = packet(&client);
            assert!(connection.peer.receive(
                &shared,
                Packet {
                    kind: Kind::Ready,
                    ..prepare
                },
                now
            ));
            let producer = Rc::new(RefCell::new(producer));
            shared.native.test_complete_current(); // exact native completion before checkpoint
            let producing = producer.clone();
            let during = shared.clone();
            AFTER_IDLE_STEP_CHECK.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    let mut producer = producing.borrow_mut();
                    // Exact interleaving: worker saw false, then the UART begins
                    // and publishes a final result before worker front() locks.
                    producer.checkpoint(State::FinalStage, 1, now);
                    producer.event(CycleEvent::FinalWrite {
                        off_generation: 1,
                        result: HostWriteResult::Complete,
                    });
                    assert!(during.step_active.load(Ordering::Acquire));
                }))
            });
            assert!(connection.dispatch(&shared, now));
            let final_result = packet(&client);
            assert_eq!(
                (final_result.kind, final_result.phase),
                (Kind::FinalResult, 1)
            );
            assert_eq!(
                transport::receive(client.as_raw_fd()).unwrap(),
                None,
                "same-step AUTHORIZE must wait for after_step"
            );
            let next = shared.front(connection.peer.epoch).unwrap();
            assert_eq!(next.packet.kind, Kind::Authorize);
            producer.borrow_mut().after_step(success);
            assert_eq!(connection.dispatch(&shared, now), success);
            if success {
                let authorize = packet(&client);
                assert_eq!(authorize.kind, Kind::Authorize);
                assert!(authorize.references(&final_result));
            } else {
                assert_eq!(transport::receive(client.as_raw_fd()).unwrap(), None);
            }
        }
    }

    #[test]
    fn stalled_peer_cannot_block_producer_and_has_absolute_send_deadline() {
        let shared = Shared::new();
        let mut producer = Producer::new(shared.clone(), [1; 16]).unwrap();
        let (fd, client) = pair();
        // Saturate the actual kernel send queue without a reader.
        let mut filled = 0;
        while transport::transmit(fd.as_raw_fd(), Packet::hello()).unwrap() {
            filled += 1;
            assert!(filled < 100000);
        }
        assert!(filled > 0);
        let now = transport::boottime_ms().unwrap();
        let peer = Peer::new(&shared, [2; 16], now).unwrap();
        let mut connection = Connection {
            _identity: injected_auth(fd.as_raw_fd(), 10000).unwrap(),
            fd,
            peer,
            stalled_until: None,
        };
        assert!(connection.peer.receive(&shared, Packet::hello(), now));
        producer.checkpoint(State::Idle, 0, now);
        producer.after_step(true);
        assert!(connection.dispatch(&shared, now));
        let deadline = connection.stalled_until.unwrap();
        assert!(!connection.dispatch(&shared, deadline)); // expiry, before poison
        let started = Instant::now();
        for generation in 1..=33 {
            producer.event(CycleEvent::Grace {
                off_generation: generation,
            });
        }
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(!shared.healthy(connection.peer.epoch, connection.peer.ticket));
        assert!(!connection.dispatch(&shared, deadline));
        drop(client);
    }
}
