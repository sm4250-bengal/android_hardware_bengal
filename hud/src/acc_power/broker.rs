// SPDX-License-Identifier: Apache-2.0
//! UART-local authority and fixed queues. No producer hook performs I/O.

use super::codec::{Kind, Nonce, Packet, LIMIT};
use crate::mcu::acc::{CycleEvent, HostWriteResult, State};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub(super) struct Ring<T: Copy, const N: usize> {
    slots: [Option<T>; N],
    head: usize,
    len: usize,
}

impl<T: Copy, const N: usize> Ring<T, N> {
    fn new() -> Self {
        Self {
            slots: [None; N],
            head: 0,
            len: 0,
        }
    }
    fn push(&mut self, value: T) -> bool {
        if self.len == N {
            return false;
        }
        self.slots[(self.head + self.len) % N] = Some(value);
        self.len += 1;
        true
    }
    fn front(&self) -> Option<T> {
        self.slots[self.head]
    }
    fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let value = self.slots[self.head].take();
        self.head = (self.head + 1) % N;
        self.len -= 1;
        value
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Record {
    pub epoch: u64,
    pub packet: Packet,
    pub native_token: u64,
}

#[derive(Clone, Copy)]
pub(super) enum Command {
    Register {
        epoch: u64,
        ticket: u64,
        nonce: Nonce,
    },
    Ready {
        epoch: u64,
        packet: Packet,
    },
    Cancel {
        epoch: u64,
        packet: Packet,
    },
}

struct Queues {
    events: Ring<Record, 32>,
    controls: Ring<Command, 8>,
}

/// Poison is out of band and monotonically covers old connection epochs.
/// A stale producer can never clear poison or poison a newer connection.
pub(super) struct Shared {
    pub native: Arc<super::participants::Preparation>,
    queues: Mutex<Queues>,
    pub active_epoch: AtomicU64,
    poison_through: AtomicU64,
    pub session_ticket: AtomicU64,
    pub session_alive: AtomicBool,
    /// Zero revokes authority, otherwise the current OFF generation.
    pub cycle: AtomicU64,
    pub step_active: AtomicBool,
    pub stop: AtomicBool,
}

impl Shared {
    #[cfg(test)]
    pub fn has_control(&self) -> bool {
        self.queues.lock().unwrap().controls.len != 0
    }
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            native: super::participants::Preparation::new(),
            queues: Mutex::new(Queues {
                events: Ring::new(),
                controls: Ring::new(),
            }),
            active_epoch: AtomicU64::new(0),
            poison_through: AtomicU64::new(0),
            session_ticket: AtomicU64::new(0),
            session_alive: AtomicBool::new(false),
            cycle: AtomicU64::new(0),
            step_active: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        })
    }

    pub fn poison(&self, epoch: u64) {
        self.poison_through.fetch_max(epoch, Ordering::AcqRel);
    }

    pub fn healthy(&self, epoch: u64, ticket: u64) -> bool {
        epoch != 0
            && self.active_epoch.load(Ordering::Acquire) == epoch
            && self.poison_through.load(Ordering::Acquire) < epoch
            && self.session_ticket.load(Ordering::Acquire) == ticket
            && self.session_alive.load(Ordering::Acquire)
            && !self.stop.load(Ordering::Acquire)
    }

    /// Worker only, after authenticated admission. No live owner replacement.
    pub fn claim(&self) -> Option<(u64, u64)> {
        let ticket = self.session_ticket.load(Ordering::Acquire);
        if ticket == 0 || !self.session_alive.load(Ordering::Acquire) {
            return None;
        }
        let epoch = self.active_epoch.load(Ordering::Acquire).checked_add(1)?;
        if epoch > LIMIT {
            return None;
        }
        let mut queues = self.queues.try_lock().ok()?;
        queues.events = Ring::new();
        queues.controls = Ring::new();
        self.cycle.store(0, Ordering::Release);
        self.active_epoch.store(epoch, Ordering::Release);
        Some((epoch, ticket))
    }

    pub fn command(&self, epoch: u64, command: Command) -> bool {
        let ok = self
            .queues
            .try_lock()
            .map(|mut q| q.controls.push(command))
            .unwrap_or(false);
        if !ok {
            self.poison(epoch);
        }
        ok
    }

    fn emit(&self, record: Record) {
        let ok = self
            .queues
            .try_lock()
            .map(|mut q| q.events.push(record))
            .unwrap_or(false);
        if !ok {
            self.poison(record.epoch);
        }
    }

    pub fn front(&self, epoch: u64) -> Option<Record> {
        match self.queues.try_lock() {
            Ok(q) => q.events.front(),
            Err(_) => {
                self.poison(epoch);
                None
            }
        }
    }

    /// Remove only the actual successfully sent/deliberately refused head.
    pub fn consume(&self, record: Record) {
        match self.queues.try_lock() {
            Ok(mut q) if q.events.front() == Some(record) => {
                q.events.pop();
            }
            _ => self.poison(record.epoch),
        }
    }
}

#[derive(Clone, Copy)]
struct Preparation {
    packet: Packet,
    ready: bool,
    final_seen: bool,
    cancelled: bool,
}

struct Registration {
    epoch: u64,
    nonce: Nonce,
    floor: u64,
    preparation: Option<Preparation>,
}

/// Owned solely by the UART thread and embedded in its PlatformHooks.
pub(crate) struct Producer {
    native_token: u64,
    native_ready: bool,
    shared: Arc<Shared>,
    ticket: u64,
    nonce: Nonce,
    sequence: u64,
    sample: u64,
    registration: Option<Registration>,
    valid: bool,
}

impl Producer {
    /// Entropy must be obtained by the caller before constructing AccSession.
    pub(super) fn new(shared: Arc<Shared>, nonce: Nonce) -> Option<Self> {
        shared.session_alive.store(false, Ordering::Release);
        let ticket = shared
            .session_ticket
            .load(Ordering::Acquire)
            .checked_add(1)?;
        if nonce == [0; 16] || ticket > LIMIT {
            return None;
        }
        shared.session_ticket.store(ticket, Ordering::Release);
        shared.session_alive.store(true, Ordering::Release);
        Some(Self {
            native_token: 0,
            native_ready: false,
            shared,
            ticket,
            nonce,
            sequence: 0,
            sample: 0,
            registration: None,
            valid: true,
        })
    }

    fn healthy(&self) -> bool {
        self.valid
            && self
                .registration
                .as_ref()
                .map(|r| self.shared.healthy(r.epoch, self.ticket))
                .unwrap_or(false)
    }

    pub(crate) fn invalidate(&mut self) {
        self.shared.native.cancel(self.native_token);
        self.valid = false;
        if let Some(r) = &self.registration {
            self.shared.poison(r.epoch);
        }
        if self.shared.session_ticket.load(Ordering::Acquire) == self.ticket {
            self.shared.session_alive.store(false, Ordering::Release);
            self.shared.step_active.store(false, Ordering::Release);
        }
    }

    fn poison_registration(&self) {
        if let Some(r) = &self.registration {
            self.shared.poison(r.epoch);
        }
    }

    /// Call immediately before each serial read/advance. The caller samples
    /// CLOCK_BOOTTIME outside all hooks; snapshots use the actual Machine.
    pub(crate) fn checkpoint(&mut self, state: State, generation: u64, now: u64) {
        if !self.valid {
            return;
        }
        if now > LIMIT || now < self.sample || generation > LIMIT {
            self.invalidate();
            return;
        }
        self.sample = now;
        if !self.healthy() {
            self.shared.native.cancel(self.native_token);
        }
        self.native_ready = self.shared.native.ready(self.native_token);
        self.shared.step_active.store(true, Ordering::Release);
        let mut commands = [None; 8];
        let epoch = self.shared.active_epoch.load(Ordering::Acquire);
        let drained = match self.shared.queues.try_lock() {
            Ok(mut q) => {
                for command in &mut commands {
                    *command = q.controls.pop();
                }
                true
            }
            Err(_) => false,
        };
        if !drained {
            self.shared.poison(epoch);
            return;
        }
        for command in commands.into_iter().flatten() {
            match command {
                Command::Register {
                    epoch,
                    ticket,
                    nonce,
                } => {
                    if ticket != self.ticket
                        || !self.shared.healthy(epoch, ticket)
                        || nonce == [0; 16]
                    {
                        self.shared.poison(epoch);
                        continue;
                    }
                    self.registration = Some(Registration {
                        epoch,
                        nonce,
                        floor: generation,
                        preparation: None,
                    });
                    self.shared.cycle.store(0, Ordering::Release);
                    let phase = match state {
                        State::Idle => 0,
                        State::Resumed => 1,
                        State::AccOffGrace => 2,
                        State::FinalStage => 3,
                        State::Suspending => 4,
                    };
                    self.publish(Kind::Snapshot, phase, generation, 0);
                }
                Command::Ready { epoch, packet } => {
                    if !self.healthy() {
                        continue;
                    }
                    let r = self.registration.as_mut().unwrap();
                    let valid = r.epoch == epoch
                        && r.preparation
                            .as_mut()
                            .map(|p| {
                                if !packet.references(&p.packet)
                                    || packet.deadline != p.packet.deadline
                                {
                                    return false;
                                }
                                // An authenticated CANCEL may revoke out of band after
                                // READY was queued but before this checkpoint. Consume
                                // that exact READY inertly; cancellation is not poison.
                                if self.shared.cycle.load(Ordering::Acquire) == 0 {
                                    return true;
                                }
                                if p.ready
                                    || p.final_seen
                                    || p.cancelled
                                    || now >= p.packet.deadline
                                    || self.shared.cycle.load(Ordering::Acquire)
                                        != packet.generation
                                {
                                    return false;
                                }
                                p.ready = true;
                                self.shared.native.request_arm(self.native_token, p.packet.deadline);
                                true
                            })
                            .unwrap_or(false);
                    if !valid {
                        self.shared.poison(epoch);
                    }
                }
                Command::Cancel { epoch, packet } => {
                    if !self.healthy() {
                        continue;
                    }
                    let r = self.registration.as_mut().unwrap();
                    if r.epoch == epoch {
                        if let Some(p) = r
                            .preparation
                            .as_mut()
                            .filter(|p| packet.references(&p.packet))
                        {
                            p.cancelled = true;
                            p.ready = false;
                            self.shared.native.cancel(self.native_token);
                        }
                        // This command was already authenticated and revoked
                        // atomically by the worker. A UART step can have moved
                        // to a newer OFF before it is drained; do not touch it.
                    }
                }
            }
        }
    }

    /// Publish the completed serial step before the caller notifies the worker.
    /// Failure invalidates authority before any queued authorization is sent.
    pub(crate) fn after_step(&mut self, ok: bool) {
        if !ok {
            self.invalidate();
        }
        self.shared.step_active.store(false, Ordering::Release);
    }

    fn packet(&self, kind: Kind, phase: u8, generation: u64, deadline: u64) -> Packet {
        Packet {
            kind,
            phase,
            session: self.nonce,
            registration: self.registration.as_ref().unwrap().nonce,
            sequence: self.sequence,
            generation,
            deadline,
        }
    }

    fn publish(&self, kind: Kind, phase: u8, generation: u64, deadline: u64) {
        if !self.healthy() {
            return;
        }
        let r = self.registration.as_ref().unwrap();
        let packet = self.packet(kind, phase, generation, deadline);
        if !packet.canonical() {
            self.shared.poison(r.epoch);
            return;
        }
        self.shared.emit(Record {
            epoch: r.epoch,
            packet,
            native_token: self.native_token,
        });
    }

    /// Fixed-state, bounded try-enqueue only. No clocks, entropy, allocations,
    /// wakeup syscalls, logging or blocking locks may be added to this method.
    pub(crate) fn event(&mut self, event: CycleEvent) {
        if !self.valid {
            return;
        }
        let generation = match event {
            CycleEvent::Grace { off_generation }
            | CycleEvent::Prepare { off_generation }
            | CycleEvent::FinalWrite { off_generation, .. }
            | CycleEvent::Resume { off_generation } => off_generation,
            CycleEvent::GenerationExhausted => {
                self.invalidate();
                return;
            }
        };
        if generation == 0 || generation > LIMIT || self.sequence == LIMIT {
            self.invalidate();
            return;
        }
        self.sequence += 1;
        if !self.healthy() {
            return;
        }
        match event {
            CycleEvent::Grace { .. } => {
                self.shared.native.cancel(self.native_token);
                self.native_token = self.shared.native.begin();
                self.native_ready = false;
                self.shared.cycle.store(generation, Ordering::Release);
                self.registration.as_mut().unwrap().preparation = None;
                self.publish(Kind::Grace, 0, generation, 0);
            }
            CycleEvent::Prepare { .. } => {
                let Some(deadline) = self.sample.checked_add(2000).filter(|d| *d <= LIMIT) else {
                    self.poison_registration();
                    return;
                };
                let packet = self.packet(Kind::Prepare, 0, generation, deadline);
                let r = self.registration.as_mut().unwrap();
                if generation > r.floor {
                    r.preparation = Some(Preparation {
                        packet,
                        ready: false,
                        final_seen: false,
                        cancelled: false,
                    });
                }
                self.publish(Kind::Prepare, 0, generation, deadline);
            }
            CycleEvent::FinalWrite { result, .. } => {
                let mut deadline = 0;
                let mut authorize = false;
                if let Some(p) = self.registration.as_mut().unwrap().preparation.as_mut() {
                    if p.packet.generation == generation {
                        deadline = p.packet.deadline;
                        authorize = p.ready
                            && self.native_ready
                            && self.shared.native.ready(self.native_token)
                            && !p.cancelled
                            && !p.final_seen
                            && result == HostWriteResult::Complete
                            && self.sample < deadline
                            && self.shared.cycle.load(Ordering::Acquire) == generation;
                        p.final_seen = true;
                    }
                }
                self.publish(
                    Kind::FinalResult,
                    if result == HostWriteResult::Complete {
                        1
                    } else {
                        2
                    },
                    generation,
                    deadline,
                );
                if authorize {
                    self.publish(Kind::Authorize, 0, generation, deadline);
                }
            }
            CycleEvent::Resume { .. } => {
                self.shared.native.cancel(self.native_token);
                self.native_ready = false;
                // Revocation is independent of space/availability in the ring.
                let _ = self.shared.cycle.compare_exchange(
                    generation,
                    0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                if let Some(p) = self.registration.as_mut().unwrap().preparation.as_mut() {
                    if p.packet.generation == generation {
                        p.cancelled = true;
                        p.ready = false;
                    }
                }
                self.publish(Kind::Resume, 0, generation, 0);
            }
            CycleEvent::GenerationExhausted => unreachable!(),
        }
    }
}

impl Drop for Producer {
    fn drop(&mut self) {
        self.invalidate();
    }
}

/// Single-worker protocol state. Endpoint identity is retained with its FD by
/// the transport, in addition to these nonces and monotonic local epoch.
pub(super) struct Peer {
    pub epoch: u64,
    pub ticket: u64,
    pub registration: Nonce,
    pub handshake_deadline: u64,
    pub snapshot: Option<Packet>,
    pub started: bool,
    hello: bool,
    next_event: u64,
    pub preparation: Option<Packet>,
    ready_received: bool,
    cancelled: bool,
    last_final: Option<Packet>,
}

impl Peer {
    pub fn new(shared: &Shared, registration: Nonce, now: u64) -> Option<Self> {
        if registration == [0; 16] {
            return None;
        }
        let handshake_deadline = now.checked_add(2000).filter(|d| *d <= LIMIT)?;
        let (epoch, ticket) = shared.claim()?;
        Some(Self {
            epoch,
            ticket,
            registration,
            handshake_deadline,
            snapshot: None,
            started: false,
            hello: false,
            next_event: 0,
            preparation: None,
            ready_received: false,
            cancelled: false,
            last_final: None,
        })
    }

    pub fn receive(&mut self, shared: &Shared, packet: Packet, now: u64) -> bool {
        if !packet.canonical() || !shared.healthy(self.epoch, self.ticket) || now > LIMIT {
            return false;
        }
        if !self.started && now >= self.handshake_deadline {
            return false;
        }
        if !self.hello {
            if packet != Packet::hello() {
                return false;
            }
            self.hello = true;
            return shared.command(
                self.epoch,
                Command::Register {
                    epoch: self.epoch,
                    ticket: self.ticket,
                    nonce: self.registration,
                },
            );
        }
        let Some(snapshot) = self.snapshot else {
            return false;
        };
        if !packet.same_envelope(&snapshot) {
            return false;
        }
        if packet.kind == Kind::Close {
            shared.poison(self.epoch);
            return false;
        }
        if !self.started {
            if packet.kind != Kind::Start || !packet.references(&snapshot) {
                return false;
            }
            self.started = true;
            return true;
        }
        let Some(preparation) = self.preparation else {
            return false;
        };
        if !packet.references(&preparation) {
            return false;
        }
        match packet.kind {
            Kind::Ready
                if !self.ready_received
                    && !self.cancelled
                    && packet.deadline == preparation.deadline
                    && now < preparation.deadline =>
            {
                self.ready_received = true;
                shared.command(
                    self.epoch,
                    Command::Ready {
                        epoch: self.epoch,
                        packet,
                    },
                )
            }
            Kind::Cancel => {
                // Exact repeated cancellation is idempotent and need not consume
                // another control slot. Do not poison an otherwise valid peer.
                let _ = shared.cycle.compare_exchange(
                    packet.generation,
                    0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                if self.cancelled {
                    return true;
                }
                self.cancelled = true;
                shared.command(
                    self.epoch,
                    Command::Cancel {
                        epoch: self.epoch,
                        packet,
                    },
                )
            }
            _ => false,
        }
    }

    /// Validate just before send using a fresh worker BOOTTIME sample. No later
    /// READY can upgrade a FinalResult: only the UART can enqueue Authorize.
    pub fn outbound_valid(&self, shared: &Shared, record: Record, now: u64) -> bool {
        if record.epoch != self.epoch || !shared.healthy(self.epoch, self.ticket) || now > LIMIT {
            return false;
        }
        let p = record.packet;
        if p.registration != self.registration {
            return false;
        }
        if self.snapshot.is_none() {
            return self.hello && p.kind == Kind::Snapshot && now < self.handshake_deadline;
        }
        if !self.started || !p.same_envelope(&self.snapshot.unwrap()) {
            return false;
        }
        if p.kind == Kind::Authorize {
            return self
                .last_final
                .map(|f| {
                    f.phase == 1 && p.references(&f) && p.deadline == f.deadline && p.deadline != 0
                })
                .unwrap_or(false)
                && !self.cancelled
                && shared.native.ready(record.native_token)
                && now < p.deadline
                && shared.cycle.load(Ordering::Acquire) == p.generation;
        }
        matches!(
            p.kind,
            Kind::Grace | Kind::Prepare | Kind::FinalResult | Kind::Resume
        ) && p.sequence == self.next_event
    }

    /// Only after an atomic seqpacket send succeeds; never consume on EAGAIN.
    pub fn sent(&mut self, packet: Packet) -> bool {
        match packet.kind {
            Kind::Snapshot => {
                self.snapshot = Some(packet);
                let Some(next) = packet.sequence.checked_add(1).filter(|n| *n <= LIMIT) else {
                    return false;
                };
                self.next_event = next;
            }
            Kind::Authorize => {
                self.last_final = None;
            }
            _ => {
                let Some(next) = packet.sequence.checked_add(1).filter(|n| *n <= LIMIT) else {
                    return false;
                };
                self.next_event = next;
                match packet.kind {
                    Kind::Prepare if packet.generation > self.snapshot.unwrap().generation => {
                        self.preparation = Some(packet);
                        self.ready_received = false;
                        self.cancelled = false;
                        self.last_final = None;
                    }
                    Kind::FinalResult => self.last_final = Some(packet),
                    Kind::Resume => {
                        self.cancelled = true;
                        self.last_final = None;
                    }
                    _ => self.last_final = None,
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcu::acc_go_fixtures::{ACC_OFF, ACC_ON, FINAL_STAGE};
    use crate::mcu::serial::{AccSession, PlatformHooks};
    use std::cell::RefCell;
    use std::io::{self, Cursor, Read, Write};
    use std::rc::Rc;

    #[derive(Default)]
    struct IoState {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
        fail: bool,
        during_write: Option<Box<dyn FnMut()>>,
    }
    struct TestIo(Rc<RefCell<IoState>>);
    impl Read for TestIo {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.0.borrow_mut().input.read(bytes)
        }
    }
    impl Write for TestIo {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let mut state = self.0.borrow_mut();
            if let Some(callback) = state.during_write.as_mut() {
                callback();
            }
            if state.fail {
                return Err(io::Error::from_raw_os_error(5));
            }
            state.output.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!("no MCU flush")
        }
    }
    struct Policy {
        producer: Producer,
        sleeps: usize,
    }
    impl PlatformHooks for Policy {
        fn on_cycle_event(&mut self, event: CycleEvent) {
            self.producer.event(event);
        }
        fn enter_acc_off_grace(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn quiesce_radio(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn detach_stick_sink(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn quiesce_usb(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn resume(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn request_sleep(&mut self, _: u64) -> io::Result<()> {
            self.sleeps += 1;
            Ok(())
        }
    }
    struct Harness {
        auto_native: bool,
        shared: Arc<Shared>,
        peer: Rc<RefCell<Peer>>,
        io: Rc<RefCell<IoState>>,
        session: AccSession<TestIo, Policy>,
    }
    impl Harness {
        fn new() -> Self {
            let shared = Shared::new();
            let producer = Producer::new(shared.clone(), [1; 16]).unwrap();
            let peer = Rc::new(RefCell::new(Peer::new(&shared, [2; 16], 0).unwrap()));
            let io = Rc::new(RefCell::new(IoState::default()));
            let session = AccSession::new(
                TestIo(io.clone()),
                Policy {
                    producer,
                    sleeps: 0,
                },
            );
            Self {
                auto_native: true,
                shared,
                peer,
                io,
                session,
            }
        }
        fn checkpoint(&mut self, now: u64) {
            if self.auto_native { self.shared.native.test_complete_current(); }
            let (state, generation) = (self.session.state(), self.session.generation());
            self.session
                .platform_mut()
                .producer
                .checkpoint(state, generation, now);
        }
        fn step(&mut self, now: u64, bytes: &[u8]) -> bool {
            self.checkpoint(now);
            self.io.borrow_mut().input = Cursor::new(bytes.to_vec());
            let ok = self.session.read_once(now).is_ok();
            self.session.platform_mut().producer.after_step(ok);
            ok
        }
        fn take(&mut self, now: u64) -> Vec<Packet> {
            let mut packets = Vec::new();
            loop {
                let epoch = self.peer.borrow().epoch;
                let Some(record) = self.shared.front(epoch) else {
                    break;
                };
                assert!(
                    self.peer.borrow().outbound_valid(&self.shared, record, now),
                    "{:?}",
                    record.packet
                );
                assert!(self.peer.borrow_mut().sent(record.packet));
                self.shared.consume(record);
                packets.push(record.packet);
            }
            packets
        }
        fn hello(&mut self, now: u64) -> Packet {
            assert!(self
                .peer
                .borrow_mut()
                .receive(&self.shared, Packet::hello(), now));
            self.checkpoint(now);
            self.session.platform_mut().producer.after_step(true);
            let record = self.shared.front(self.peer.borrow().epoch).unwrap();
            assert_eq!(record.packet.kind, Kind::Snapshot);
            assert!(self.peer.borrow_mut().sent(record.packet));
            self.shared.consume(record);
            record.packet
        }
        fn start(&mut self, snapshot: Packet, now: u64) {
            assert!(self.peer.borrow_mut().receive(
                &self.shared,
                Packet {
                    kind: Kind::Start,
                    phase: 0,
                    ..snapshot
                },
                now
            ));
        }
        fn prepare(&mut self) -> Packet {
            let snapshot = self.hello(0);
            self.start(snapshot, 0);
            assert!(self.step(10, ACC_OFF));
            assert!(self.step(100, FINAL_STAGE));
            let packets = self.take(100);
            assert_eq!(
                packets.iter().map(|p| p.kind).collect::<Vec<_>>(),
                [Kind::Grace, Kind::Prepare]
            );
            packets[1]
        }
    }

    #[test]
    fn quiet_pty_ready_arms_before_once_only_final_without_more_mcu_bytes() {
        use crate::acc_power::participants::{Owner, Worker};
        use crate::mcu::serial::test_support::{pty_pair_with_name, read_all_available};
        use std::time::{Duration, Instant};
        struct ReadyOwner;
        impl Owner for ReadyOwner {
            fn hold(&mut self, _: u64) -> bool { true }
            fn confirmed(&mut self, _: u64) -> bool { true }
            fn release(&mut self, _: u64) -> bool { true }
            fn arm(&mut self, _: u64, _: u64) -> bool {
                std::thread::sleep(Duration::from_millis(5));
                true
            }
        }
        let shared = Shared::new();
        let mut producer = Producer::new(shared.clone(), [1; 16]).unwrap();
        let mut peer = Peer::new(&shared, [2; 16], 0).unwrap();
        assert!(peer.receive(&shared, Packet::hello(), 0));
        producer.checkpoint(State::Idle, 0, 0);
        producer.after_step(true);
        let snapshot = shared.front(peer.epoch).unwrap();
        assert!(peer.sent(snapshot.packet));
        shared.consume(snapshot);
        assert!(peer.receive(&shared, Packet { kind: Kind::Start, phase: 0, ..snapshot.packet }, 0));
        let worker = Worker::start(shared.native.clone(), ReadyOwner).unwrap();
        let (mut master, path) = pty_pair_with_name();
        let mut session = AccSession::open_path(std::path::Path::new(&path), Policy { producer, sleeps: 0 }).unwrap();
        let started = Instant::now();
        // Same selected-channel ordering as the production reader: bounded wait,
        // fresh checkpoint, immediate worker wake, then read OR timer advance.
        let tick = |session: &mut AccSession<std::fs::File, Policy>| {
            let readable = session.wait_acc_input().unwrap();
            let now = started.elapsed().as_millis() as u64;
            let (state, generation) = (session.state(), session.generation());
            session.platform_mut().producer.checkpoint(state, generation, now);
            worker.notify();
            if readable { session.read_once(now).unwrap(); }
            else { session.advance(now).unwrap(); }
            session.platform_mut().producer.after_step(true);
            worker.notify();
            now
        };
        master.write_all(ACC_OFF).unwrap();
        tick(&mut session);
        master.write_all(FINAL_STAGE).unwrap();
        let final_start = tick(&mut session);
        let mut prepare = None;
        while let Some(record) = shared.front(peer.epoch) {
            if record.packet.kind == Kind::Prepare { prepare = Some(record.packet); }
            assert!(peer.outbound_valid(&shared, record, final_start));
            assert!(peer.sent(record.packet));
            shared.consume(record);
        }
        assert!(read_all_available(&mut master).is_empty());
        // No UART input after PREPARE. READY and native completion are separate
        // worker/control activity, not accidental MCU chatter keeping read live.
        while started.elapsed().as_millis() < 100 { tick(&mut session); }
        assert!(peer.receive(&shared, Packet { kind: Kind::Ready, ..prepare.unwrap() }, started.elapsed().as_millis() as u64));
        let mut final_time = 0;
        while session.state() != State::Suspending && started.elapsed().as_millis() < 900 {
            final_time = tick(&mut session);
        }
        assert_eq!(session.state(), State::Suspending);
        assert!((500..900).contains(&(final_time - final_start)));
        let mut kinds = Vec::new();
        while let Some(record) = shared.front(peer.epoch) {
            assert!(peer.outbound_valid(&shared, record, final_time));
            assert!(peer.sent(record.packet));
            shared.consume(record);
            kinds.push(record.packet.kind);
        }
        assert_eq!(kinds, [Kind::FinalResult, Kind::Authorize]);
        assert_eq!(read_all_available(&mut master), crate::mcu::TX_ACC_OFF_COMPLETE);
        tick(&mut session);
        assert!(read_all_available(&mut master).is_empty());
        assert!(shared.front(peer.epoch).is_none());
        assert_eq!(session.platform_mut().sleeps, 1);
        drop(master);
        assert!(session.wait_acc_input().is_err()); // HUP does not become a read/tick success
    }

    #[test]
    fn only_uart_accepted_ready_requests_arm_and_later_checkpoint_qualifies_it() {
        let mut h = Harness::new(); h.auto_native = false;
        let prepare = h.prepare();
        assert!(!h.shared.native.test_arm_requested());
        assert!(h.peer.borrow_mut().receive(&h.shared, Packet { kind: Kind::Ready, ..prepare }, 400));
        assert!(!h.shared.native.test_arm_requested()); // socket receipt isn't acceptance
        assert!(h.step(450, &[]));
        assert!(h.shared.native.test_arm_requested());
        assert!(!h.session.platform_mut().producer.native_ready);
        h.shared.native.test_complete_current(); // explicit worker proof fixture
        assert!(!h.session.platform_mut().producer.native_ready);
        assert!(h.step(600, &[]));
        assert_eq!(h.take(601).iter().map(|p| p.kind).collect::<Vec<_>>(),
            [Kind::FinalResult, Kind::Authorize]);
    }

    #[test]
    fn native_missing_late_and_during_write_cannot_authorize_host_complete() {
        for moment in 0..3 {
            let mut h = Harness::new();
            h.auto_native = false;
            let prepare = h.prepare();
            assert!(h.peer.borrow_mut().receive(&h.shared, Packet { kind: Kind::Ready, ..prepare }, 599));
            if moment == 1 {
                let shared = h.shared.clone();
                h.io.borrow_mut().during_write = Some(Box::new(move || shared.native.test_complete_current()));
            }
            assert!(h.step(600, &[]));
            if moment == 2 { h.shared.native.test_complete_current(); }
            let packets = h.take(601);
            assert_eq!(packets.len(), 1);
            assert_eq!((packets[0].kind, packets[0].phase), (Kind::FinalResult, 1));
            assert_eq!(h.session.platform_mut().sleeps, 1); // legacy intent unchanged
        }
    }

    #[test]
    fn native_revocation_during_write_keeps_host_fact_but_refuses_authority() {
        let mut h = Harness::new();
        let prepare = h.prepare();
        assert!(h.peer.borrow_mut().receive(&h.shared, Packet { kind: Kind::Ready, ..prepare }, 599));
        let shared = h.shared.clone();
        h.io.borrow_mut().during_write = Some(Box::new(move || {
            shared.native.validity.store(false, Ordering::Release);
        }));
        assert!(h.step(600, &[]));
        assert_eq!(h.take(601).len(), 1);
    }

    #[test]
    fn ready_before_step_authorizes_but_during_write_or_after_does_not() {
        for moment in 0..3 {
            let mut h = Harness::new();
            let prepare = h.prepare();
            let ready = Packet {
                kind: Kind::Ready,
                ..prepare
            };
            if moment == 0 {
                assert!(h.peer.borrow_mut().receive(&h.shared, ready, 599));
            }
            if moment == 1 {
                let (peer, shared) = (h.peer.clone(), h.shared.clone());
                h.io.borrow_mut().during_write = Some(Box::new(move || {
                    assert!(peer.borrow_mut().receive(&shared, ready, 600));
                }));
            }
            assert!(h.step(600, &[]));
            if moment == 2 {
                assert!(h.peer.borrow_mut().receive(&h.shared, ready, 601));
            }
            let packets = h.take(601);
            assert_eq!(packets[0].kind, Kind::FinalResult);
            assert_eq!(packets[0].phase, 1);
            assert_eq!(packets.len(), if moment == 0 { 2 } else { 1 });
            if moment == 0 {
                assert_eq!(packets[1].kind, Kind::Authorize);
                assert!(packets[1].references(&packets[0]));
            }
            assert_eq!(h.session.platform_mut().sleeps, 1); // legacy seam unchanged
        }
    }

    #[test]
    fn snapshot_fence_keeps_start_race_and_historical_off_observational() {
        let mut h = Harness::new();
        assert!(h.step(10, ACC_OFF)); // before registration: sequence floor1, gen1
        let snapshot = h.hello(20);
        assert_eq!(
            (snapshot.phase, snapshot.sequence, snapshot.generation),
            (2, 1, 1)
        );
        assert!(h.step(30, FINAL_STAGE)); // queues while START is pending
        let record = h.shared.front(h.peer.borrow().epoch).unwrap();
        assert!(!h.peer.borrow().outbound_valid(&h.shared, record, 30));
        h.start(snapshot, 40);
        let prepare = h.take(40)[0];
        assert!(h.peer.borrow().preparation.is_none());
        assert!(!h.peer.borrow_mut().receive(
            &h.shared,
            Packet {
                kind: Kind::Ready,
                ..prepare
            },
            41
        ));
        assert!(h.step(530, &[]));
        let final_packet = h.take(530)[0];
        assert_eq!(
            (final_packet.kind, final_packet.deadline),
            (Kind::FinalResult, 0)
        );
        assert!(h.step(600, ACC_ON));
        assert_eq!(h.take(600)[0].generation, 1);
        assert!(h.step(700, ACC_OFF));
        assert!(h.step(800, FINAL_STAGE));
        let packets = h.take(800);
        assert_eq!(packets[1].generation, 3);
        assert!(h.peer.borrow().preparation.is_some());
    }

    #[test]
    fn cancel_is_immediate_idempotent_and_distinct_from_connection_poison() {
        let mut h = Harness::new();
        let prepare = h.prepare();
        assert!(h.peer.borrow_mut().receive(
            &h.shared,
            Packet {
                kind: Kind::Ready,
                ..prepare
            },
            599
        ));
        assert!(h.step(600, &[])); // final+authorize are queued, not sent
        let cancel = Packet {
            kind: Kind::Cancel,
            deadline: 0,
            ..prepare
        };
        assert!(h.peer.borrow_mut().receive(&h.shared, cancel, 601));
        assert!(h.peer.borrow_mut().receive(&h.shared, cancel, 601));
        assert_eq!(h.shared.cycle.load(Ordering::Acquire), 0);
        let peer = h.peer.borrow();
        assert!(h.shared.healthy(peer.epoch, peer.ticket));
        drop(peer);
        let final_record = h.shared.front(h.peer.borrow().epoch).unwrap();
        assert!(h.peer.borrow_mut().sent(final_record.packet));
        h.shared.consume(final_record);
        let authorize = h.shared.front(h.peer.borrow().epoch).unwrap();
        assert_eq!(authorize.packet.kind, Kind::Authorize);
        assert!(!h.peer.borrow().outbound_valid(&h.shared, authorize, 601));
        h.shared.consume(authorize); // worker suppresses only optional authority
        assert!(h.step(602, ACC_ON));
        assert_eq!(h.take(602)[0].kind, Kind::Resume);
        assert!(h.step(700, ACC_OFF));
        assert!(h.step(800, FINAL_STAGE));
        let next = h.take(800)[1];
        assert!(!h.peer.borrow_mut().receive(&h.shared, cancel, 801));
        assert_eq!(h.shared.cycle.load(Ordering::Acquire), next.generation);
    }

    #[test]
    fn queued_ready_then_cancel_before_checkpoint_preserves_connection_not_authority() {
        let mut h = Harness::new();
        let prepare = h.prepare();
        assert!(h.peer.borrow_mut().receive(
            &h.shared,
            Packet {
                kind: Kind::Ready,
                ..prepare
            },
            599
        ));
        assert!(h.peer.borrow_mut().receive(
            &h.shared,
            Packet {
                kind: Kind::Cancel,
                deadline: 0,
                ..prepare
            },
            599
        ));
        assert!(h.step(600, &[]));
        let packets = h.take(600);
        assert_eq!(
            packets.iter().map(|p| p.kind).collect::<Vec<_>>(),
            [Kind::FinalResult]
        );
        let peer = h.peer.borrow();
        assert!(h.shared.healthy(peer.epoch, peer.ticket));
    }

    #[test]
    fn on_499_failure_deadline_and_reopen_fail_closed() {
        let mut h = Harness::new();
        let prepare = h.prepare();
        assert!(h.step(599, ACC_ON));
        assert!(h.step(600, &[]));
        assert_eq!(
            h.take(600).iter().map(|p| p.kind).collect::<Vec<_>>(),
            [Kind::Resume]
        );
        assert_eq!(h.session.platform_mut().sleeps, 0);
        assert!(!h.peer.borrow_mut().receive(
            &h.shared,
            Packet {
                kind: Kind::Ready,
                ..prepare
            },
            2100
        ));

        let mut h = Harness::new();
        let prepare = h.prepare();
        assert!(h.peer.borrow_mut().receive(
            &h.shared,
            Packet {
                kind: Kind::Ready,
                ..prepare
            },
            599
        ));
        h.io.borrow_mut().fail = true;
        assert!(!h.step(600, &[]));
        assert_eq!(h.session.platform_mut().sleeps, 0);
        let record = h.shared.front(h.peer.borrow().epoch).unwrap();
        assert_eq!(
            (record.packet.kind, record.packet.phase),
            (Kind::FinalResult, 2)
        );
        assert!(!h.peer.borrow().outbound_valid(&h.shared, record, 600)); // reader error poisons
        let old = (h.peer.borrow().epoch, h.peer.borrow().ticket);
        let _replacement = Producer::new(h.shared.clone(), [3; 16]).unwrap();
        assert!(!h.shared.healthy(old.0, old.1));
    }

    #[test]
    fn fixed_rings_contention_and_signed_exhaustion_poison_out_of_band() {
        let mut h = Harness::new();
        let snapshot = h.hello(0);
        h.start(snapshot, 0);
        for generation in 1..=33 {
            h.session.platform_mut().producer.event(CycleEvent::Grace {
                off_generation: generation,
            });
        }
        let peer = h.peer.borrow();
        assert!(!h.shared.healthy(peer.epoch, peer.ticket));
        assert_eq!(h.shared.queues.lock().unwrap().events.len, 32);
        drop(peer);
        let mut h = Harness::new();
        let _ = h.hello(0);
        let shared = h.shared.clone();
        let held = shared.queues.lock().unwrap();
        h.session
            .platform_mut()
            .producer
            .event(CycleEvent::Grace { off_generation: 1 });
        assert!(!shared.healthy(h.peer.borrow().epoch, h.peer.borrow().ticket));
        drop(held);
        let mut h = Harness::new();
        let _ = h.hello(0);
        let epoch = h.peer.borrow().epoch;
        for _ in 0..8 {
            assert!(h.shared.command(
                epoch,
                Command::Register {
                    epoch,
                    ticket: 1,
                    nonce: [2; 16]
                }
            ));
        }
        assert!(!h.shared.command(
            epoch,
            Command::Register {
                epoch,
                ticket: 1,
                nonce: [2; 16]
            }
        ));
        assert!(!h.shared.healthy(epoch, 1));
        let mut h = Harness::new();
        let _ = h.hello(0);
        h.session.platform_mut().producer.sequence = LIMIT;
        h.session
            .platform_mut()
            .producer
            .event(CycleEvent::Grace { off_generation: 1 });
        assert!(!h.shared.session_alive.load(Ordering::Acquire));
    }

    #[test]
    fn cold_start_duplicates_and_pmic_tokens_use_only_actual_off_cycles() {
        use crate::mcu::acc::SuspendProof;
        let mut h = Harness::new();
        let snapshot = h.hello(0);
        h.start(snapshot, 0);
        assert!(h.step(1, &[ACC_ON, ACC_ON, FINAL_STAGE].concat()));
        assert!(h.step(501, &[]));
        assert!(h.take(501).is_empty());
        assert_eq!(h.session.platform_mut().sleeps, 0);
        assert!(h.io.borrow().output.is_empty());
        assert!(h.step(1000, &[ACC_OFF, ACC_OFF, FINAL_STAGE, FINAL_STAGE].concat()));
        let token = h.session.wake_token();
        assert!(!h
            .session
            .hardware_wake(token, SuspendProof::Completed, 1499)
            .unwrap());
        assert!(h.step(1500, &[]));
        assert!(h.step(1600, FINAL_STAGE));
        let packets = h.take(1600);
        assert_eq!(
            packets.iter().map(|p| p.kind).collect::<Vec<_>>(),
            [Kind::Grace, Kind::Prepare, Kind::FinalResult]
        );
        let first = packets[0].generation;
        assert!(h
            .session
            .hardware_wake(token, SuspendProof::Completed, 1601)
            .unwrap());
        assert_eq!(h.take(1601)[0].generation, first);
        assert!(h.step(2000, &[ACC_OFF, FINAL_STAGE].concat()));
        assert!(h.step(2500, &[]));
        let second = h.take(2500)[0].generation;
        assert!(second > first);
        assert!(!h
            .session
            .hardware_wake(token, SuspendProof::Completed, 30000)
            .unwrap());
        assert!(h.take(30000).is_empty());
        assert!(h
            .session
            .hardware_wake(h.session.wake_token(), SuspendProof::Completed, 30001)
            .unwrap());
        assert_eq!(h.take(30001)[0].generation, second);
    }

    #[test]
    fn mismatched_start_nonces_sequence_and_expired_authorize_deny() {
        let mut h = Harness::new();
        let snapshot = h.hello(0);
        for bad in [
            Packet {
                kind: Kind::Start,
                session: [4; 16],
                ..snapshot
            },
            Packet {
                kind: Kind::Start,
                registration: [4; 16],
                ..snapshot
            },
            Packet {
                kind: Kind::Start,
                sequence: 1,
                ..snapshot
            },
        ] {
            assert!(!h.peer.borrow_mut().receive(&h.shared, bad, 1));
        }
        h.start(snapshot, 1);
        assert!(!h.peer.borrow_mut().receive(
            &h.shared,
            Packet {
                kind: Kind::Start,
                ..snapshot
            },
            2
        ));
        let mut h = Harness::new();
        let prepare = h.prepare();
        assert!(h.peer.borrow_mut().receive(
            &h.shared,
            Packet {
                kind: Kind::Ready,
                ..prepare
            },
            599
        ));
        assert!(h.step(600, &[]));
        let final_record = h.shared.front(h.peer.borrow().epoch).unwrap();
        let mut gap = final_record;
        gap.packet.sequence += 1;
        assert!(!h.peer.borrow().outbound_valid(&h.shared, gap, 600));
        h.peer.borrow_mut().sent(final_record.packet);
        h.shared.consume(final_record);
        let authorize = h.shared.front(h.peer.borrow().epoch).unwrap();
        assert!(!h
            .peer
            .borrow()
            .outbound_valid(&h.shared, authorize, prepare.deadline));
    }
}
