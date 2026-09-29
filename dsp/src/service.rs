// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Binder-independent `IDsp` service core.

use crate::adau1701::{
    validate_range, Adau1701, Clock, ParameterRamBus, ReadError, BYTES_PER_WORD, I2C_ADDRESS,
    MAX_READ_WORDS, MIN_READ_INTERVAL_MS, PARAMETER_WORDS,
};

/// Core form of the AIDL identity parcelable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DspIdentity {
    /// The adapter descriptor is owned and reads may be attempted.
    pub available: bool,
    /// Stable reason when unavailable.
    pub unavailable_reason: String,
    /// Board-qualified configured part.
    pub chip_model: String,
    /// Resolved adapter node or expected node when unavailable.
    pub i2c_adapter: String,
    /// Seven-bit address.
    pub i2c_address: u16,
    /// Parameter RAM word count.
    pub parameter_word_count: usize,
    /// Transfer bytes per word.
    pub parameter_word_bytes: usize,
    /// Maximum words per call.
    pub max_read_words: usize,
    /// Minimum spacing between attempted calls.
    pub min_read_interval_ms: u64,
}

/// Core form of one AIDL readback block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DspParameterBlock {
    /// First word address.
    pub start_word: u16,
    /// Consecutive raw word values.
    pub words: Vec<u32>,
}

/// Binder exception family selected by the Android adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExceptionCode {
    /// Invalid start/count request.
    IllegalArgument,
    /// Valid call made before the next safe read time.
    IllegalState,
    /// Adapter or transfer failure.
    ServiceSpecific,
}

/// Stable service error independent of Binder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceError {
    /// Binder exception family.
    pub code: ExceptionCode,
    /// Human-readable failure.
    pub message: String,
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ServiceError {}

/// Object-safe core consumed by the Binder adapter.
pub trait DspApi: Send {
    /// `IDsp.getIdentity`.
    fn get_identity(&self) -> DspIdentity;
    /// `IDsp.readParameterRam`.
    fn read_parameter_ram(
        &mut self,
        start_word: i32,
        word_count: i32,
    ) -> Result<DspParameterBlock, ServiceError>;
}

trait ReadBackend: Send {
    fn read_words(&mut self, start_word: u16, word_count: usize) -> Result<Vec<u32>, ReadError>;
}

impl<B: ParameterRamBus, C: Clock> ReadBackend for Adau1701<B, C> {
    fn read_words(&mut self, start_word: u16, word_count: usize) -> Result<Vec<u32>, ReadError> {
        self.read_words(start_word, word_count)
    }
}

/// Host-testable implementation behind `IDsp/default`.
pub struct DspService {
    backend: Option<Box<dyn ReadBackend>>,
    identity: DspIdentity,
}

impl DspService {
    /// Construct a live service around one resolved adapter owner.
    pub fn live<B, C>(reader: Adau1701<B, C>) -> Self
    where
        B: ParameterRamBus + 'static,
        C: Clock + 'static,
    {
        let adapter = reader.adapter_label();
        Self {
            backend: Some(Box::new(reader)),
            identity: identity(true, String::new(), adapter),
        }
    }

    /// Construct a declared but electrically inert service.
    pub fn unavailable(expected_adapter: impl Into<String>, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            backend: None,
            identity: identity(false, reason, expected_adapter.into()),
        }
    }
}

impl DspApi for DspService {
    fn get_identity(&self) -> DspIdentity {
        self.identity.clone()
    }

    fn read_parameter_ram(
        &mut self,
        start_word: i32,
        word_count: i32,
    ) -> Result<DspParameterBlock, ServiceError> {
        let start_word = u16::try_from(start_word).map_err(|_| ServiceError {
            code: ExceptionCode::IllegalArgument,
            message: format!("startWord {start_word} is outside parameter RAM"),
        })?;
        let word_count = usize::try_from(word_count).map_err(|_| ServiceError {
            code: ExceptionCode::IllegalArgument,
            message: format!("wordCount {word_count} is negative"),
        })?;
        validate_range(start_word, word_count).map_err(service_error)?;
        let backend = self.backend.as_mut().ok_or_else(|| ServiceError {
            code: ExceptionCode::ServiceSpecific,
            message: self.identity.unavailable_reason.clone(),
        })?;
        let words = backend
            .read_words(start_word, word_count)
            .map_err(service_error)?;
        Ok(DspParameterBlock { start_word, words })
    }
}

fn identity(available: bool, unavailable_reason: String, adapter: String) -> DspIdentity {
    DspIdentity {
        available,
        unavailable_reason,
        chip_model: "ADAU1701".into(),
        i2c_adapter: adapter,
        i2c_address: I2C_ADDRESS,
        parameter_word_count: PARAMETER_WORDS,
        parameter_word_bytes: BYTES_PER_WORD,
        max_read_words: MAX_READ_WORDS,
        min_read_interval_ms: MIN_READ_INTERVAL_MS,
    }
}

fn service_error(error: ReadError) -> ServiceError {
    let code = match error {
        ReadError::InvalidRange(_) => ExceptionCode::IllegalArgument,
        ReadError::RateLimited => ExceptionCode::IllegalState,
        ReadError::Transport(_) => ExceptionCode::ServiceSpecific,
    };
    ServiceError {
        code,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FixedClock;

    impl Clock for FixedClock {
        fn now_ms(&self) -> u64 {
            1_000
        }
    }

    #[derive(Clone, Default)]
    struct FakeBus(Arc<Mutex<Vec<(u16, u16, usize)>>>);

    impl ParameterRamBus for FakeBus {
        type Error = &'static str;

        fn read_parameter_words(
            &mut self,
            address: u16,
            start: u16,
            output: &mut [u8],
        ) -> Result<(), Self::Error> {
            self.0.lock().unwrap().push((address, start, output.len()));
            output.copy_from_slice(&[0x01, 0x09, 0x74, 0xf1]);
            Ok(())
        }

        fn adapter_label(&self) -> String {
            "/dev/i2c-test".into()
        }
    }

    #[test]
    fn identity_is_explicit_about_capability_and_adapter() {
        let service = DspService::live(Adau1701::new(FakeBus::default(), FixedClock));
        let got = service.get_identity();
        assert!(got.available);
        assert_eq!(got.chip_model, "ADAU1701");
        assert_eq!(got.i2c_adapter, "/dev/i2c-test");
        assert_eq!(got.i2c_address, 0x34);
        assert_eq!(got.max_read_words, 1024);
        assert_eq!(got.min_read_interval_ms, 100);
    }

    #[test]
    fn service_returns_exact_parameter_word() {
        let bus = FakeBus::default();
        let calls = Arc::clone(&bus.0);
        let mut service = DspService::live(Adau1701::new(bus, FixedClock));
        let block = service.read_parameter_ram(0x0008, 1).unwrap();
        assert_eq!(block.start_word, 0x0008);
        assert_eq!(block.words, vec![0x0109_74f1]);
        assert_eq!(*calls.lock().unwrap(), vec![(0x34, 0x0008, 4)]);
    }

    #[test]
    fn negative_and_oversized_aidl_inputs_are_illegal_arguments() {
        let mut service = DspService::live(Adau1701::new(FakeBus::default(), FixedClock));
        assert_eq!(
            service.read_parameter_ram(-1, 1).unwrap_err().code,
            ExceptionCode::IllegalArgument
        );
        assert_eq!(
            service.read_parameter_ram(0, 1025).unwrap_err().code,
            ExceptionCode::IllegalArgument
        );
    }

    #[test]
    fn unavailable_service_is_visible_but_electrically_inert() {
        let mut service = DspService::unavailable("/dev/i2c-3", "NotReady/i2c-3-absent");
        assert!(!service.get_identity().available);
        assert_eq!(
            service.read_parameter_ram(1023, 2).unwrap_err().code,
            ExceptionCode::IllegalArgument,
            "argument validation must not depend on hardware availability"
        );
        let error = service.read_parameter_ram(0, 1).unwrap_err();
        assert_eq!(error.code, ExceptionCode::ServiceSpecific);
        assert_eq!(error.message, "NotReady/i2c-3-absent");
    }
}
