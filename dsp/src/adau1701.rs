// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! ADAU1701 parameter-RAM model and read-only transport seam.

use std::fmt;
use std::time::Instant;

/// ADAU1701 seven-bit address measured on device 3 as client `3-0034`.
pub const I2C_ADDRESS: u16 = 0x34;
/// Parameter RAM contains 1024 four-byte words.
pub const PARAMETER_WORDS: usize = 1024;
/// Wire width of one parameter word.
pub const BYTES_PER_WORD: usize = 4;
/// One bounded full snapshot. The production kernel transport retains physical
/// mute across the request and splits it into at most 16 words per bus transfer.
pub const MAX_READ_WORDS: usize = PARAMETER_WORDS;
/// Minimum spacing between attempted reads.
pub const MIN_READ_INTERVAL_MS: u64 = 100;

/// The only transaction capability the V1 hardware core accepts.
///
/// Implementations receive an already-bounded parameter-word range. There is
/// no generic write method and no caller-provided subaddress byte buffer.
pub trait ParameterRamBus: Send {
    /// Transport-specific error.
    type Error: fmt::Display;

    /// Read raw transfer bytes for consecutive parameter words.
    fn read_parameter_words(
        &mut self,
        chip_address: u16,
        start_word: u16,
        output: &mut [u8],
    ) -> Result<(), Self::Error>;

    /// Resolved adapter node retained for status and bugreports.
    fn adapter_label(&self) -> String;
}

/// Injectable monotonic clock used by the transaction-rate boundary.
pub trait Clock: Send {
    /// Monotonic milliseconds in an arbitrary epoch.
    fn now_ms(&self) -> u64;
}

/// Production monotonic clock.
#[derive(Debug)]
pub struct MonotonicClock {
    origin: Instant,
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for MonotonicClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// Refusal or transport failure from one parameter-RAM read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadError {
    /// A zero-width, oversized or out-of-range request.
    InvalidRange(String),
    /// The previous attempted transfer is less than 100 ms old.
    RateLimited,
    /// The resolved bus transaction failed.
    Transport(String),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRange(reason) => write!(f, "invalid parameter-RAM range: {reason}"),
            Self::RateLimited => write!(
                f,
                "parameter-RAM read rate limited (minimum {MIN_READ_INTERVAL_MS} ms)"
            ),
            Self::Transport(reason) => write!(f, "parameter-RAM I2C read failed: {reason}"),
        }
    }
}

impl std::error::Error for ReadError {}

/// Live ADAU1701 reader with bounds and transaction cadence.
pub struct Adau1701<B, C> {
    bus: B,
    clock: C,
    last_attempt_ms: Option<u64>,
}

impl<B: ParameterRamBus, C: Clock> Adau1701<B, C> {
    /// Construct around one already-resolved adapter owner.
    pub fn new(bus: B, clock: C) -> Self {
        Self {
            bus,
            clock,
            last_attempt_ms: None,
        }
    }

    /// Resolved adapter identity.
    pub fn adapter_label(&self) -> String {
        self.bus.adapter_label()
    }

    /// Read consecutive parameter words and convert big-endian wire values.
    pub fn read_words(
        &mut self,
        start_word: u16,
        word_count: usize,
    ) -> Result<Vec<u32>, ReadError> {
        validate_range(start_word, word_count)?;
        let now_ms = self.clock.now_ms();
        if self
            .last_attempt_ms
            .is_some_and(|last| now_ms < last || now_ms.saturating_sub(last) < MIN_READ_INTERVAL_MS)
        {
            return Err(ReadError::RateLimited);
        }

        // Charge attempts, not successes: a failing ioctl still occupied the
        // shared adapter and may not be turned into a tight retry loop.
        self.last_attempt_ms = Some(now_ms);
        let mut bytes = vec![0u8; word_count * BYTES_PER_WORD];
        self.bus
            .read_parameter_words(I2C_ADDRESS, start_word, &mut bytes)
            .map_err(|error| ReadError::Transport(error.to_string()))?;

        Ok(bytes
            .chunks_exact(BYTES_PER_WORD)
            .map(|word| u32::from_be_bytes([word[0], word[1], word[2], word[3]]))
            .collect())
    }
}

/// Validate one public parameter-RAM request without touching hardware.
pub fn validate_range(start_word: u16, word_count: usize) -> Result<(), ReadError> {
    if word_count == 0 {
        return Err(ReadError::InvalidRange("word count is zero".into()));
    }
    if word_count > MAX_READ_WORDS {
        return Err(ReadError::InvalidRange(format!(
            "{word_count} words exceeds maximum {MAX_READ_WORDS}"
        )));
    }
    if usize::from(start_word)
        .checked_add(word_count)
        .is_none_or(|end| end > PARAMETER_WORDS)
    {
        return Err(ReadError::InvalidRange(format!(
            "start {start_word} plus count {word_count} exceeds {PARAMETER_WORDS} words"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct FakeClock(Arc<Mutex<u64>>);

    impl FakeClock {
        fn set(&self, now: u64) {
            *self.0.lock().unwrap() = now;
        }
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            *self.0.lock().unwrap()
        }
    }

    #[derive(Debug, Default)]
    struct FakeBus {
        calls: Vec<(u16, u16, usize)>,
        fail: bool,
    }

    impl ParameterRamBus for FakeBus {
        type Error = &'static str;

        fn read_parameter_words(
            &mut self,
            chip_address: u16,
            start_word: u16,
            output: &mut [u8],
        ) -> Result<(), Self::Error> {
            self.calls.push((chip_address, start_word, output.len()));
            if self.fail {
                return Err("fixture failure");
            }
            for (index, byte) in output.iter_mut().enumerate() {
                *byte = index as u8;
            }
            Ok(())
        }

        fn adapter_label(&self) -> String {
            "/dev/fake-i2c".into()
        }
    }

    #[test]
    fn bounds_reject_without_touching_the_bus() {
        let mut reader = Adau1701::new(FakeBus::default(), FakeClock::default());
        assert!(matches!(
            reader.read_words(0, 0),
            Err(ReadError::InvalidRange(_))
        ));
        assert!(matches!(
            reader.read_words(0, MAX_READ_WORDS + 1),
            Err(ReadError::InvalidRange(_))
        ));
        assert!(matches!(
            reader.read_words(1023, 2),
            Err(ReadError::InvalidRange(_))
        ));
        assert!(reader.bus.calls.is_empty());
    }

    #[test]
    fn read_uses_fixed_address_and_decodes_big_endian_words() {
        let clock = FakeClock::default();
        clock.set(1_000);
        let mut reader = Adau1701::new(FakeBus::default(), clock);
        let words = reader.read_words(0x0008, 2).unwrap();
        assert_eq!(reader.bus.calls, vec![(0x34, 0x0008, 8)]);
        assert_eq!(words, vec![0x0001_0203, 0x0405_0607]);
    }

    #[test]
    fn failed_attempt_is_charged_and_clock_rollback_is_refused() {
        let clock = FakeClock::default();
        clock.set(1_000);
        let mut reader = Adau1701::new(
            FakeBus {
                fail: true,
                ..FakeBus::default()
            },
            clock.clone(),
        );
        assert!(matches!(
            reader.read_words(0, 1),
            Err(ReadError::Transport(_))
        ));
        assert_eq!(reader.bus.calls.len(), 1);

        clock.set(1_099);
        assert_eq!(reader.read_words(0, 1), Err(ReadError::RateLimited));
        clock.set(900);
        assert_eq!(reader.read_words(0, 1), Err(ReadError::RateLimited));
        assert_eq!(reader.bus.calls.len(), 1);

        clock.set(1_100);
        assert!(matches!(
            reader.read_words(0, 1),
            Err(ReadError::Transport(_))
        ));
        assert_eq!(reader.bus.calls.len(), 2);
    }
}
