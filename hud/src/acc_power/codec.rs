// SPDX-License-Identifier: Apache-2.0
//! Fixed BACPv1 wire encoding. No native structs or signed casts on the wire.

pub(crate) const LIMIT: u64 = i64::MAX as u64;
pub(crate) type Nonce = [u8; 16];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Kind {
    Hello = 1,
    Snapshot,
    Start,
    Grace,
    Prepare,
    Ready,
    FinalResult,
    Authorize,
    Resume,
    Cancel,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Packet {
    pub kind: Kind,
    pub phase: u8,
    pub session: Nonce,
    pub registration: Nonce,
    pub sequence: u64,
    pub generation: u64,
    pub deadline: u64,
}

impl Packet {
    pub fn hello() -> Self {
        Self {
            kind: Kind::Hello,
            phase: 0,
            session: [0; 16],
            registration: [0; 16],
            sequence: 0,
            generation: 0,
            deadline: 0,
        }
    }

    pub fn canonical(&self) -> bool {
        if self.sequence > LIMIT || self.generation > LIMIT || self.deadline > LIMIT {
            return false;
        }
        if self.kind == Kind::Hello {
            return *self == Self::hello();
        }
        if self.session == [0; 16] || self.registration == [0; 16] {
            return false;
        }
        let phase_ok = match self.kind {
            Kind::Snapshot => self.phase <= 4,
            Kind::FinalResult => matches!(self.phase, 1 | 2),
            _ => self.phase == 0,
        };
        phase_ok
            && match self.kind {
                Kind::Hello => false,
                Kind::Snapshot | Kind::Start => self.deadline == 0,
                Kind::Grace | Kind::Resume | Kind::Cancel => {
                    self.sequence != 0 && self.generation != 0 && self.deadline == 0
                }
                Kind::Prepare | Kind::Ready | Kind::Authorize => {
                    self.sequence != 0 && self.generation != 0 && self.deadline != 0
                }
                Kind::FinalResult => self.sequence != 0 && self.generation != 0,
                Kind::Close => self.sequence == 0 && self.generation == 0 && self.deadline == 0,
            }
    }

    pub fn encode(self) -> Option<[u8; 64]> {
        if !self.canonical() {
            return None;
        }
        let mut bytes = [0; 64];
        bytes[..4].copy_from_slice(b"BACP");
        bytes[4] = 1;
        bytes[5] = self.kind as u8;
        bytes[6] = self.phase;
        bytes[8..24].copy_from_slice(&self.session);
        bytes[24..40].copy_from_slice(&self.registration);
        bytes[40..48].copy_from_slice(&self.sequence.to_be_bytes());
        bytes[48..56].copy_from_slice(&self.generation.to_be_bytes());
        bytes[56..64].copy_from_slice(&self.deadline.to_be_bytes());
        Some(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 64 || &bytes[..4] != b"BACP" || bytes[4] != 1 || bytes[7] != 0 {
            return None;
        }
        let kind = match bytes[5] {
            1 => Kind::Hello,
            2 => Kind::Snapshot,
            3 => Kind::Start,
            4 => Kind::Grace,
            5 => Kind::Prepare,
            6 => Kind::Ready,
            7 => Kind::FinalResult,
            8 => Kind::Authorize,
            9 => Kind::Resume,
            10 => Kind::Cancel,
            11 => Kind::Close,
            _ => return None,
        };
        let packet = Self {
            kind,
            phase: bytes[6],
            session: bytes[8..24].try_into().ok()?,
            registration: bytes[24..40].try_into().ok()?,
            sequence: u64::from_be_bytes(bytes[40..48].try_into().ok()?),
            generation: u64::from_be_bytes(bytes[48..56].try_into().ok()?),
            deadline: u64::from_be_bytes(bytes[56..64].try_into().ok()?),
        };
        packet.canonical().then_some(packet)
    }

    pub fn same_envelope(&self, other: &Self) -> bool {
        self.session == other.session && self.registration == other.registration
    }

    pub fn references(&self, other: &Self) -> bool {
        self.same_envelope(other)
            && self.sequence == other.sequence
            && self.generation == other.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_twelve_golden_packets_round_trip_and_decode_fields() {
        let source = include_str!("../../tests/acc-power-golden.txt");
        let mut count = 0;
        for line in source
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
        {
            let (name, hex) = line.split_once(' ').unwrap();
            let bytes: Vec<_> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            let packet = Packet::decode(&bytes).unwrap();
            assert_eq!(packet.encode().unwrap().as_slice(), bytes, "{name}");
            if name == "prepare" {
                assert_eq!(
                    (
                        packet.kind,
                        packet.sequence,
                        packet.generation,
                        packet.deadline
                    ),
                    (Kind::Prepare, 102, 9, 123456)
                );
                assert_eq!(packet.session, std::array::from_fn(|i| (i + 1) as u8));
                assert_eq!(packet.registration, std::array::from_fn(|i| (i + 17) as u8));
            }
            count += 1;
        }
        assert_eq!(count, 12);
    }

    #[test]
    fn rejects_noncanonical_and_signed_overflow_without_prefix_acceptance() {
        let packet = Packet {
            kind: Kind::Prepare,
            session: [1; 16],
            registration: [2; 16],
            sequence: 1,
            generation: 1,
            deadline: 2000,
            ..Packet::hello()
        };
        let bytes = packet.encode().unwrap();
        for offset in [0, 4, 5, 6, 7, 40, 48, 56] {
            let mut bad = bytes;
            bad[offset] = 255;
            assert!(Packet::decode(&bad).is_none(), "offset {offset}");
        }
        assert!(Packet::decode(&bytes[..63]).is_none());
        assert!(Packet::decode(&[bytes.as_slice(), &[0]].concat()).is_none());
        assert!(Packet {
            sequence: LIMIT + 1,
            ..packet
        }
        .encode()
        .is_none());
        assert!(Packet {
            deadline: 0,
            ..packet
        }
        .encode()
        .is_none());
        assert!(Packet {
            session: [0; 16],
            ..packet
        }
        .encode()
        .is_none());
    }
}
