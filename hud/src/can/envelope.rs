// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Streaming parser for the XYAUTO CAN-box default envelope (Raise family).
//!
//! Moved here from the desk-only crate `platform/hardware/xyauto/can` (2026-08-30)
//! with its tests; `tools/oem-can-decoder-agreement.py` still lifts the OEM
//! `CanbusMonitor.canbox_rx_uart()` out of the decompiled source and compares
//! accepted frames with this parser, so the two cannot drift apart unnoticed.
//!
//! Recovered envelope (`~/android/hardware/xyauto_can/evidence/CanbusMonitor.java:771-848`):
//!
//! ```text
//! 2E TYPE LEN DATA[LEN] CS      CS = ~(TYPE + LEN + ΣDATA) & 0xFF
//! ```
//!
//! Public corroboration: the same header/checksum family is documented for
//! Raise/RZC boxes (JunsunPSARemote README, `0x2E 0x01 0x02 0x01 0x01 0xDB`,
//! "0xFF − sum of the previous bytes excluding the header").
//!
//! Deliberate difference from the OEM loop: after accepting a frame the Java
//! code advances by the frame length **and** the loop increments once more, so
//! it misses a second frame whose header is immediately adjacent. This parser
//! accepts both; the agreement tool pins that divergence as a known one.

/// Header used by the OEM default parser.
pub const DEFAULT_HEADER: u8 = 0x2e;

/// Longest possible frame: two-byte header + type + length + 255 data + checksum.
pub const MAX_FRAME: usize = 260;

/// Source-linked plugin transport selector, independent of the physical baud.
/// ToolClass and CanbusSerial agree on these headers in both directions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Framing {
    #[default]
    Raise,
    Hiworld5aa5,
    HiworldAa55,
    XinpuAa55PackedRx,
}

impl Framing {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "raise-2e" => Some(Self::Raise),
            "hiworld-5aa5" => Some(Self::Hiworld5aa5),
            "hiworld-aa55" => Some(Self::HiworldAa55),
            "xinpu-aa55-packed-rx" => Some(Self::XinpuAa55PackedRx),
            _ => None,
        }
    }

    fn header(self) -> &'static [u8] {
        match self {
            Self::Raise => &[0x2e],
            Self::Hiworld5aa5 => &[0x5a, 0xa5],
            Self::HiworldAa55 | Self::XinpuAa55PackedRx => &[0xaa, 0x55],
        }
    }

    pub(crate) fn supports_tx(self) -> bool { self != Self::XinpuAa55PackedRx }

    fn checksum(self, message_type: u8, data: &[u8]) -> u8 {
        match self {
            Self::Raise => default_checksum(message_type, data.len() as u8, data),
            _ => data.iter().fold(message_type.wrapping_add(data.len() as u8), |s, b| s.wrapping_add(*b)).wrapping_sub(1),
        }
    }

    pub(crate) fn encode(self, message_type: u8, data: &[u8]) -> Result<Vec<u8>, String> {
        if !self.supports_tx() { return Err("selected framing is receive-only".into()); }
        if data.len() > 255 { return Err("payload exceeds envelope length byte".into()); }
        let mut raw = self.header().to_vec();
        match self {
            Self::Raise => raw.extend_from_slice(&[message_type, data.len() as u8]),
            _ => raw.extend_from_slice(&[data.len() as u8, message_type]),
        }
        raw.extend_from_slice(data);
        raw.push(self.checksum(message_type, data));
        Ok(raw)
    }
}

/// How much unparsed input the parser will hold before dropping it. A stream
/// that never contains a valid frame is a stream we are misreading (wrong baud,
/// wrong port); holding it forever turns a wiring fault into an OOM.
pub const MAX_PENDING: usize = 4096;

/// A checksum-valid or checksum-invalid candidate frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    /// Complete frame, including header and checksum.
    pub raw: Vec<u8>,
    /// OEM message type byte.
    pub message_type: u8,
    /// Payload bytes.
    pub data: Vec<u8>,
    /// Checksum carried by the frame.
    pub checksum: u8,
    /// Expected checksum, including the literal OEM packed-RX value -1 when
    /// its low-byte sum is zero. That value cannot match any received byte.
    pub expected_checksum: i16,
}

impl Frame {
    /// Whether the carried checksum matches the recovered OEM formula.
    pub fn checksum_ok(&self) -> bool {
        i16::from(self.checksum) == self.expected_checksum
    }
}

/// One observable result from the stream parser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Item {
    /// Bytes skipped while hunting for a `0x2e` header.
    Discarded(Vec<u8>),
    /// A complete checksum-valid frame.
    Frame(Frame),
    /// A complete candidate with a bad checksum.
    BadChecksum(Frame),
    /// Bytes left when the caller declared the capture complete.
    Incomplete(Vec<u8>),
}

/// Incremental parser for one selected OEM envelope; the default is `0x2e`.
#[derive(Default)]
pub struct Parser {
    pending: Vec<u8>,
    framing: Framing,
}

impl Parser {
    /// Create an empty parser.
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with_framing(framing: Framing) -> Self {
        Self { pending: Vec::new(), framing }
    }

    /// Push one arbitrary read chunk and return every result now decidable.
    ///
    /// Unlike the OEM Java method, an incomplete frame is retained across
    /// reads. A bad candidate advances by one byte so an embedded real header
    /// can still be found.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Item> {
        self.pending.extend_from_slice(bytes);
        let mut items = Vec::new();

        loop {
            let signature = self.framing.header();
            let Some(header) = self.pending.iter().position(|byte| *byte == signature[0]) else {
                if !self.pending.is_empty() {
                    items.push(Item::Discarded(std::mem::take(&mut self.pending)));
                }
                break;
            };

            if header > 0 {
                items.push(Item::Discarded(self.pending.drain(..header).collect()));
            }

            if self.pending.len() < signature.len() { break; }
            if !self.pending.starts_with(signature) {
                items.push(Item::Discarded(self.pending.drain(..1).collect()));
                continue;
            }
            let packed = self.framing == Framing::XinpuAa55PackedRx;
            let data_start = if packed { 3 } else { signature.len() + 2 };
            if self.pending.len() < data_start {
                break;
            }

            // XinPu selector38406 packs TYPE into bits7..5 and payload LEN
            // into bits3..0. Bit4 is not part of LEN, but is checksummed.
            let data_len = usize::from(if packed { self.pending[2] & 0x0f } else { self.pending[2] });
            let frame_len = data_len + data_start + 1;
            if self.pending.len() < frame_len {
                if self.pending.len() > MAX_PENDING {
                    // Cannot be a frame we understand; drop the header byte and
                    // keep hunting rather than hoarding.
                    items.push(Item::Discarded(self.pending.drain(..1).collect()));
                    continue;
                }
                break;
            }

            let raw = self.pending[..frame_len].to_vec();
            let message_type = if packed { (raw[2] & 0xe0) >> 5 }
                else { raw[if self.framing == Framing::Raise { 1 } else { 3 }] };
            let data = raw[data_start..data_start + data_len].to_vec();
            let checksum = raw[frame_len - 1];
            let expected_checksum = if packed {
                // CanbusSerial.xp_rx_uart_aa55: (sum &255)-1, with NO mask
                // after subtraction. A zero sum is therefore always rejected.
                i16::from(data.iter().fold(raw[2], |sum, byte| sum.wrapping_add(*byte))) - 1
            } else { i16::from(self.framing.checksum(message_type, &data)) };
            let frame = Frame {
                raw,
                message_type,
                data,
                checksum,
                expected_checksum,
            };

            if frame.checksum_ok() {
                self.pending.drain(..frame_len);
                items.push(Item::Frame(frame));
            } else {
                self.pending.drain(..1);
                items.push(Item::BadChecksum(frame));
            }
        }

        items
    }

    /// Finish a finite capture and expose any incomplete tail.
    pub fn finish(&mut self) -> Vec<Item> {
        if self.pending.is_empty() {
            Vec::new()
        } else {
            vec![Item::Incomplete(std::mem::take(&mut self.pending))]
        }
    }

    /// Bytes held back waiting for the rest of a frame.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// Calculate the OEM default checksum.
pub fn default_checksum(message_type: u8, data_len: u8, data: &[u8]) -> u8 {
    let sum = data
        .iter()
        .fold(message_type.wrapping_add(data_len), |sum, byte| {
            sum.wrapping_add(*byte)
        });
    !sum
}

/// Build one complete, checksum-valid frame (tests and the desk feeder).
pub fn encode_frame(message_type: u8, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 4);
    out.push(DEFAULT_HEADER);
    out.push(message_type);
    out.push(data.len() as u8);
    out.extend_from_slice(data);
    out.push(default_checksum(message_type, data.len() as u8, data));
    out
}

/// Encode bytes as lowercase hexadecimal without separators.
pub fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

/// Decode hexadecimal text. ASCII whitespace, `:`, `-`, and `_` separators
/// are accepted; every other character must be a hex digit.
pub fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    let compact: String = text
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace() && !matches!(ch, ':' | '-' | '_'))
        .collect();
    if compact.len() % 2 != 0 {
        return Err("hex input has an odd number of digits".to_owned());
    }

    compact
        .as_bytes()
        .chunks_exact(2)
        .enumerate()
        .map(|(index, pair)| {
            let high = hex_nibble(pair[0])
                .ok_or_else(|| format!("invalid hex digit at compact offset {}", index * 2))?;
            let low = hex_nibble(pair[1])
                .ok_or_else(|| format!("invalid hex digit at compact offset {}", index * 2 + 1))?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic protocol vectors only. No byte sequence in this module is
    // represented as a capture from the user's vehicle or CAN box.

    #[test]
    fn checksum_matches_the_recovered_complement_formula() {
        assert_eq!(default_checksum(0x41, 2, &[0x01, 0x11]), 0xaa);
        assert_eq!(default_checksum(0x10, 0, &[]), 0xef);
    }

    #[test]
    fn complete_frame_is_decoded() {
        let items = Parser::new().push(&[0x2e, 0x41, 0x02, 0x01, 0x11, 0xaa]);
        assert_eq!(items.len(), 1);
        let Item::Frame(frame) = &items[0] else {
            panic!("not a valid frame: {items:?}");
        };
        assert_eq!(frame.message_type, 0x41);
        assert_eq!(frame.data, [0x01, 0x11]);
    }

    #[test]
    fn every_split_boundary_is_reassembled() {
        let frame = [0x2e, 0x41, 0x02, 0x01, 0x11, 0xaa];
        for split in 0..=frame.len() {
            let mut parser = Parser::new();
            let mut items = parser.push(&frame[..split]);
            items.extend(parser.push(&frame[split..]));
            assert!(
                matches!(items.as_slice(), [Item::Frame(_)]),
                "split={split}"
            );
        }
    }

    #[test]
    fn noise_and_bad_checksum_remain_observable() {
        let input = [
            0x99, 0x2e, 0x20, 0x02, 0x06, 0x01, 0xd7, 0x2e, 0x10, 0x00, 0xef,
        ];
        let items = Parser::new().push(&input);
        assert!(items
            .iter()
            .any(|item| matches!(item, Item::BadChecksum(_))));
        assert!(items.iter().any(|item| matches!(item, Item::Frame(_))));
        assert!(items.iter().any(|item| matches!(item, Item::Discarded(_))));
    }

    #[test]
    fn two_contiguous_frames_are_both_accepted_unlike_the_oem_loop() {
        let mut bytes = encode_frame(0x10, &[]);
        bytes.extend(encode_frame(0x7f, &[0x00]));
        let items = Parser::new().push(&bytes);
        assert_eq!(
            items.iter().filter(|i| matches!(i, Item::Frame(_))).count(),
            2
        );
    }

    #[test]
    fn incomplete_tail_is_reported_only_at_finish() {
        let mut parser = Parser::new();
        assert!(parser.push(&[0x2e, 0x41, 0x02, 0x01]).is_empty());
        assert_eq!(parser.pending_len(), 4);
        assert_eq!(
            parser.finish(),
            [Item::Incomplete(vec![0x2e, 0x41, 0x02, 0x01])]
        );
    }

    #[test]
    fn a_header_followed_by_endless_garbage_does_not_hoard() {
        // 0x2e then LEN=0xff and never enough bytes: the parser must not keep
        // more than MAX_PENDING waiting for a frame that never comes.
        let mut parser = Parser::new();
        let mut items = parser.push(&[0x2e, 0x41, 0xff]);
        for _ in 0..(MAX_PENDING / 8) {
            items.extend(parser.push(&[0x2d; 8]));
        }
        assert!(parser.pending_len() <= MAX_PENDING + 8);
        assert!(items.iter().any(|i| matches!(i, Item::Discarded(_))));
    }

    #[test]
    fn encode_frame_round_trips() {
        let frame = encode_frame(0x41, &[0x02, 0x0b, 0xb8]);
        assert_eq!(frame[0], DEFAULT_HEADER);
        let items = Parser::new().push(&frame);
        let Item::Frame(decoded) = &items[0] else {
            panic!("{items:?}")
        };
        assert_eq!(decoded.data, [0x02, 0x0b, 0xb8]);
        assert!(decoded.checksum_ok());
    }

    #[test]
    fn hiworld_known_vectors_split_cleanly_and_keep_families_isolated() {
        for (framing, header) in [(Framing::Hiworld5aa5, [0x5a, 0xa5]), (Framing::HiworldAa55, [0xaa, 0x55])] {
            // Independent OEM formula: LEN2 + TYPE0xa0 + DATA0,255 -1 =0xa0.
            let raw = [header[0], header[1], 2, 0xa0, 0, 255, 0xa0];
            assert_eq!(framing.encode(0xa0, &[0, 255]).unwrap(), raw);
            for split in 0..=raw.len() {
                let mut parser = Parser::with_framing(framing);
                let mut results = parser.push(&raw[..split]);
                results.extend(parser.push(&raw[split..]));
                assert!(matches!(results.as_slice(), [Item::Frame(frame)] if frame.message_type == 0xa0 && frame.data == [0, 255]));
            }
            let mut parser = Parser::with_framing(framing);
            assert!(!parser.push(&encode_frame(0xa0, &[0, 255])).iter().any(|i| matches!(i, Item::Frame(_))));
            let mut bad = raw;
            bad[6] ^= 1;
            assert!(parser.push(&bad).iter().any(|i| matches!(i, Item::BadChecksum(_))));
            let mut adjacent = raw.to_vec();
            adjacent.extend_from_slice(&raw);
            assert_eq!(parser.push(&adjacent).iter().filter(|i| matches!(i, Item::Frame(_))).count(), 2);
            assert!(framing.encode(1, &[0; 256]).is_err());
        }
    }

    #[test]
    fn xinpu_packed_rx_matches_source_layout_at_every_split() {
        // Synthetic source vectors, not measured traffic: Audi2001001 key
        // frame has6payload bytes, type0, key at payload2; sum6+7-1=12.
        let key = [0xaa, 0x55, 6, 0, 0, 7, 0, 0, 0, 12];
        for split in 0..=key.len() {
            let mut parser = Parser::with_framing(Framing::XinpuAa55PackedRx);
            let mut items = parser.push(&key[..split]);
            items.extend(parser.push(&key[split..]));
            assert!(matches!(items.as_slice(), [Item::Frame(f)] if f.message_type == 0 && f.data == [0, 0, 7, 0, 0, 0]));
        }
        let mut parser = Parser::with_framing(Framing::XinpuAa55PackedRx);
        // Bit4 contributes to checksum but not length: type5,length1.
        let items = parser.push(&[0xaa, 0x55, 0xb1, 0x22, 0xd2]);
        assert!(matches!(items.as_slice(), [Item::Frame(f)] if f.message_type == 5 && f.data == [0x22]));
        let mut longest = vec![0xaa, 0x55, 0xef];
        longest.extend_from_slice(&[0; 15]);
        longest.push(0xee);
        assert!(matches!(parser.push(&longest).as_slice(), [Item::Frame(f)] if f.message_type == 7 && f.data.len() == 15));
        let mut adjacent = key.to_vec();
        adjacent.extend_from_slice(&key);
        assert_eq!(parser.push(&adjacent).iter().filter(|i| matches!(i, Item::Frame(_))).count(), 2);
    }

    #[test]
    fn xinpu_packed_zero_sum_is_rejected_literally_and_no_tx_exists() {
        let mut parser = Parser::with_framing(Framing::XinpuAa55PackedRx);
        let items = parser.push(&[0xaa, 0x55, 1, 255, 255]);
        assert!(matches!(&items[0], Item::BadChecksum(f) if f.expected_checksum == -1 && !f.checksum_ok()));
        assert!(!items.iter().any(|i| matches!(i, Item::Frame(_))));
        let items = parser.push(&[0xaa, 0x55, 1, 1, 0]); // Expected1, received0.
        assert!(matches!(&items[0], Item::BadChecksum(f) if f.expected_checksum == 1));
        assert!(parser.push(&[0x55, 0xaa, 1, 1, 1]).iter().all(|i| !matches!(i, Item::Frame(_))));
        parser.finish();
        assert!(matches!(parser.push(&[0xaa, 0x55, 1, 1, 1]).as_slice(), [Item::Frame(_)]));
        for message_type in 0..=255 {
            assert!(Framing::XinpuAa55PackedRx.encode(message_type, &[]).is_err());
            assert!(Framing::XinpuAa55PackedRx.encode(message_type, &[1]).is_err());
        }
    }

    #[test]
    fn hex_codec_accepts_separators_and_rejects_non_hex() {
        assert_eq!(
            decode_hex("2e:41 02-01_11 aa").unwrap(),
            [0x2e, 0x41, 2, 1, 0x11, 0xaa]
        );
        assert!(decode_hex("2e4").is_err());
        assert!(decode_hex("2egg").is_err());
        assert_eq!(encode_hex(&[0x2e, 0xff, 0x00]), "2eff00");
    }
}
