// SPDX-License-Identifier: Apache-2.0
//! BACPv1 fixed control channel. Physical sleep policy is not owned here.
// Production entry points are called by the Android-only Binder adapter.
#![cfg_attr(not(feature = "binder"), allow(dead_code))]

mod broker;
mod codec;
mod transport;
mod worker;
pub(crate) mod participants;
pub(crate) mod release;
pub(crate) mod deep_owner;
pub(crate) mod wake;

#[cfg(feature = "binder")]
pub(crate) use broker::Producer;
#[cfg(feature = "binder")]
pub(crate) use transport::boottime_ms;
#[cfg(feature = "binder")]
pub(crate) use worker::Channel;

#[cfg(feature = "binder")]
pub(crate) mod bootstrap;

/// Exercise the actual Binder ReaderHooks without exporting broker internals.
#[cfg(feature = "binder-desk")]
pub(crate) fn reader_hook_control(exercise: impl FnOnce(Producer) -> Producer) {
    use crate::mcu::acc::State;
    use codec::{Kind, Packet};
    let shared = broker::Shared::new();
    let mut producer = Producer::new(shared.clone(), [1; 16]).unwrap();
    let mut peer = broker::Peer::new(&shared, [2; 16], 0).unwrap();
    assert!(peer.receive(&shared, Packet::hello(), 0));
    producer.checkpoint(State::Idle, 0, 0);
    producer.after_step(true);
    let snapshot = shared.front(peer.epoch).unwrap();
    assert!(peer.sent(snapshot.packet));
    shared.consume(snapshot);
    assert!(peer.receive(
        &shared,
        Packet {
            kind: Kind::Start,
            phase: 0,
            ..snapshot.packet
        },
        0
    ));
    let _producer = exercise(producer);
    for expected in [Kind::Grace, Kind::Prepare, Kind::FinalResult] {
        let record = shared.front(peer.epoch).unwrap();
        assert_eq!(record.packet.kind, expected);
        assert!(peer.outbound_valid(&shared, record, 600));
        assert!(peer.sent(record.packet));
        shared.consume(record);
    }
    assert!(shared.front(peer.epoch).is_none()); // no READY, no AUTHORIZE
}
