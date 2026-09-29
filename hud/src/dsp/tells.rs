// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! The two FM diagnostic tells, and the ordering that makes them mean
//! something.
//!
//! A flight reader can already see that HuRadio ASKED for an FM mix commit or
//! an attenuation. What no log line said until now is whether the guard ioctl
//! that actually moves the analogue chain RETURNED. These two lines close
//! exactly that gap and nothing else:
//!
//! ```text
//! fm mix commit done generation=<u64>
//! fm attenuation applied generation=<u64> millibels=<u32>
//! ```
//!
//! Three properties they must keep, all of them enforced by the helpers here
//! rather than by whoever calls them next:
//!
//! 1. **They are API completions, not DSP readback.** The ioctl returned 0.
//!    That says the kernel accepted the request and finished its safeload; it
//!    does not say what the ADAU1701 now holds, and it never says which cone
//!    moved. `readParameterRam` remains the only independent view.
//! 2. **They are emitted only after success.** A failed ioctl keeps its
//!    existing error line and produces no tell, so a tell in the log is proof
//!    of completion rather than of an attempt.
//! 3. **They are emitted OUTSIDE the guard lock.** The guard mutex serialises
//!    every physical audio operation in this daemon; a `write()` to
//!    `/dev/kmsg` inside it would put a syscall of unbounded latency into the
//!    hardware-critical section that FM mute/restore ordering depends on.
//!    [`commit_fm_mix`] and [`set_fm_attenuation`] therefore drop the lock
//!    before they call the sink, and a test proves it by refusing to emit
//!    while the mutex is still held.
//!
//! One `write_all` is one kernel record; see [`crate::logsink`] for why that
//! matters on this unit. There is no per-poll logging here: `setFmAttenuation`
//! is called on user volume changes, and the FM mix commit happens once per
//! route.

use super::guard::{AudioGuard, Error, Transport};
use std::sync::Mutex;

/// Where a tell goes. A trait so the ordering can be tested at the desk.
pub trait TellSink {
    /// Emit one complete line as one record. Must never panic.
    fn emit(&self, line: &str);
}

/// The production sink: one `write_all` of one preformatted `/dev/kmsg` line.
pub struct KmsgTellSink;

impl TellSink for KmsgTellSink {
    fn emit(&self, line: &str) {
        use std::io::Write as _;

        let record = crate::logsink::kmsg_line(false, line);
        let _ = std::io::stderr().write_all(record.as_bytes());
    }
}

/// A guard operation that could not be completed.
#[derive(Debug)]
pub enum TellError {
    /// The shared guard mutex was poisoned by a panicking holder.
    Poisoned,
    /// The guard refused or the ioctl failed. Callers keep their own mapping.
    Guard(Error),
}

/// Exact text of the FM mix commit tell.
pub fn fm_mix_commit_line(generation: u64) -> String {
    format!("fm mix commit done generation={generation}")
}

/// Exact text of the FM attenuation tell.
pub fn fm_attenuation_line(generation: u64, millibels: u32) -> String {
    format!("fm attenuation applied generation={generation} millibels={millibels}")
}

/// Exact text of the FM trim-base tell (OEM radio-mix level, not dB).
pub fn fm_mix_base_line(generation: u64, level: u32) -> String {
    format!("fm mix base applied generation={generation} level={level}")
}

/// Commit the mixed FM route, then tell the log it completed.
///
/// The lock is scoped so it is released before `sink.emit` runs.
pub fn commit_fm_mix<T: Transport>(
    guard: &Mutex<AudioGuard<T>>,
    generation: u64,
    sink: &dyn TellSink,
) -> Result<(), TellError> {
    let result = {
        let mut owner = guard.lock().map_err(|_| TellError::Poisoned)?;
        owner.commit_fm_mix(generation)
    };
    result.map_err(TellError::Guard)?;
    sink.emit(&fm_mix_commit_line(generation));
    Ok(())
}

/// Apply one FM attenuation, then tell the log it completed.
///
/// `attenuation_millibels` is reported as the unsigned value the guard
/// accepted; the cast is safe because the guard has already refused anything
/// outside 0..=9600 by the time the tell runs.
pub fn set_fm_attenuation<T: Transport>(
    guard: &Mutex<AudioGuard<T>>,
    generation: u64,
    attenuation_millibels: i32,
    sink: &dyn TellSink,
) -> Result<(), TellError> {
    let result = {
        let mut owner = guard.lock().map_err(|_| TellError::Poisoned)?;
        owner.set_fm_attenuation(generation, attenuation_millibels)
    };
    result.map_err(TellError::Guard)?;
    sink.emit(&fm_attenuation_line(
        generation,
        attenuation_millibels.max(0) as u32,
    ));
    Ok(())
}

/// Select the owner's FM trim base, then tell the log it completed.
///
/// Same shape as the attenuation: the guard refuses anything outside 0..=100
/// before the ioctl, the lock is released before the emit, and a failed or
/// refused write produces no line.
pub fn set_fm_mix_base_level<T: Transport>(
    guard: &Mutex<AudioGuard<T>>,
    generation: u64,
    level: i32,
    sink: &dyn TellSink,
) -> Result<(), TellError> {
    let result = {
        let mut owner = guard.lock().map_err(|_| TellError::Poisoned)?;
        owner.set_fm_mix_base_level(generation, level)
    };
    result.map_err(TellError::Guard)?;
    sink.emit(&fm_mix_base_line(generation, level.max(0) as u32));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::guard::{
        DspUpdate, FmAttenuation, FmMixBase, FmMixTransition, RawStatus, SourceTransition,
        ABI_VERSION,
    };
    use std::io;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeTransport {
        status: RawStatus,
        fail_with: Option<i32>,
    }

    impl FakeTransport {
        fn ready() -> Self {
            Self {
                status: RawStatus {
                    abi_version: ABI_VERSION,
                    dsp_powered: 1,
                    program_loaded: 1,
                    fm_powered: 1,
                    ..RawStatus::default()
                },
                fail_with: None,
            }
        }
    }

    /// Shared so a test can arm a failure after the guard already owns it.
    type Shared = Arc<Mutex<FakeTransport>>;

    impl Transport for Shared {
        fn get_status(&mut self) -> io::Result<RawStatus> {
            Ok(self.lock().expect("fake lock").status)
        }
        fn fault_safe(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn dsp_on(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn dac_power(&mut self, _enabled: bool) -> io::Result<()> {
            Ok(())
        }
        fn amplifier_power(&mut self, _enabled: bool) -> io::Result<()> {
            Ok(())
        }
        fn source_transition(&mut self, _request: SourceTransition) -> io::Result<()> {
            Ok(())
        }
        fn dsp_update(&mut self, _request: DspUpdate) -> io::Result<()> {
            Ok(())
        }
        fn fm_power(&mut self, _enabled: bool) -> io::Result<()> {
            Ok(())
        }
        fn fm_mix_transition(&mut self, _request: FmMixTransition) -> io::Result<()> {
            match self.lock().expect("fake lock").fail_with {
                Some(errno) => Err(io::Error::from_raw_os_error(errno)),
                None => Ok(()),
            }
        }
        fn fm_attenuation(&mut self, _request: FmAttenuation) -> io::Result<()> {
            match self.lock().expect("fake lock").fail_with {
                Some(errno) => Err(io::Error::from_raw_os_error(errno)),
                None => Ok(()),
            }
        }
        fn fm_mix_base(&mut self, _request: FmMixBase) -> io::Result<()> {
            match self.lock().expect("fake lock").fail_with {
                Some(errno) => Err(io::Error::from_raw_os_error(errno)),
                None => Ok(()),
            }
        }
    }

    /// Records emitted lines, and — the point of the whole harness — records
    /// whether the guard mutex was free at the moment of the call.
    #[derive(Default)]
    struct RecordingSink {
        lines: Mutex<Vec<String>>,
        lock_was_free: Mutex<Vec<bool>>,
        guard: Mutex<Option<Arc<Mutex<AudioGuard<Shared>>>>>,
    }

    impl RecordingSink {
        fn watching(guard: &Arc<Mutex<AudioGuard<Shared>>>) -> Self {
            Self {
                guard: Mutex::new(Some(Arc::clone(guard))),
                ..Self::default()
            }
        }

        fn lines(&self) -> Vec<String> {
            self.lines.lock().expect("sink lock").clone()
        }

        fn every_emit_was_outside_the_lock(&self) -> bool {
            let observed = self.lock_was_free.lock().expect("sink lock");
            !observed.is_empty() && observed.iter().all(|free| *free)
        }
    }

    impl TellSink for RecordingSink {
        fn emit(&self, line: &str) {
            let free = self
                .guard
                .lock()
                .expect("sink lock")
                .as_ref()
                .map(|guard| guard.try_lock().is_ok())
                .unwrap_or(true);
            self.lock_was_free.lock().expect("sink lock").push(free);
            self.lines.lock().expect("sink lock").push(line.to_owned());
        }
    }

    fn armed() -> (Arc<Mutex<AudioGuard<Shared>>>, Shared) {
        let shared: Shared = Arc::new(Mutex::new(FakeTransport::ready()));
        let guard = Arc::new(Mutex::new(AudioGuard::with_transport(Arc::clone(&shared))));
        guard
            .lock()
            .expect("guard lock")
            .prepare_fm_mix(7)
            .expect("prepare a mixed lease so commit/attenuation are legal");
        (guard, shared)
    }

    #[test]
    fn the_commit_tell_has_the_exact_agreed_text() {
        assert_eq!(
            fm_mix_commit_line(42),
            "fm mix commit done generation=42",
            "the flight reader matches this string; changing it needs a new reader branch"
        );
    }

    #[test]
    fn the_attenuation_tell_has_the_exact_agreed_text() {
        assert_eq!(
            fm_attenuation_line(42, 1200),
            "fm attenuation applied generation=42 millibels=1200"
        );
    }

    #[test]
    fn a_successful_commit_emits_one_tell_outside_the_lock() {
        let (guard, _shared) = armed();
        let sink = RecordingSink::watching(&guard);
        commit_fm_mix(&guard, 7, &sink).expect("commit");
        assert_eq!(sink.lines(), vec!["fm mix commit done generation=7"]);
        assert!(
            sink.every_emit_was_outside_the_lock(),
            "a kmsg write inside the guard lock would sit in the FM mute/restore critical section"
        );
    }

    #[test]
    fn a_successful_attenuation_emits_one_tell_outside_the_lock() {
        let (guard, _shared) = armed();
        let sink = RecordingSink::watching(&guard);
        set_fm_attenuation(&guard, 7, 1600, &sink).expect("attenuation");
        assert_eq!(
            sink.lines(),
            vec!["fm attenuation applied generation=7 millibels=1600"]
        );
        assert!(sink.every_emit_was_outside_the_lock());
    }

    #[test]
    fn a_failed_commit_emits_no_tell() {
        let (guard, shared) = armed();
        shared.lock().expect("fake lock").fail_with = Some(32);
        let sink = RecordingSink::watching(&guard);
        let error = commit_fm_mix(&guard, 7, &sink).expect_err("the ioctl failed");
        assert!(matches!(error, TellError::Guard(_)));
        assert!(
            sink.lines().is_empty(),
            "a tell must prove completion, never an attempt"
        );
    }

    #[test]
    fn a_failed_attenuation_emits_no_tell() {
        let (guard, shared) = armed();
        shared.lock().expect("fake lock").fail_with = Some(5);
        let sink = RecordingSink::watching(&guard);
        assert!(set_fm_attenuation(&guard, 7, 800, &sink).is_err());
        assert!(sink.lines().is_empty());
    }

    #[test]
    fn a_refused_attenuation_range_emits_no_tell() {
        let (guard, _shared) = armed();
        let sink = RecordingSink::watching(&guard);
        assert!(set_fm_attenuation(&guard, 7, 9601, &sink).is_err());
        assert!(set_fm_attenuation(&guard, 7, -1, &sink).is_err());
        assert!(sink.lines().is_empty());
    }

    #[test]
    fn a_wrong_generation_emits_no_tell() {
        let (guard, _shared) = armed();
        let sink = RecordingSink::watching(&guard);
        assert!(commit_fm_mix(&guard, 8, &sink).is_err());
        assert!(set_fm_attenuation(&guard, 8, 100, &sink).is_err());
        assert!(set_fm_mix_base_level(&guard, 8, 100, &sink).is_err());
        assert!(sink.lines().is_empty());
    }

    #[test]
    fn the_base_tell_has_the_exact_agreed_text() {
        assert_eq!(
            fm_mix_base_line(42, 100),
            "fm mix base applied generation=42 level=100"
        );
    }

    #[test]
    fn a_successful_base_emits_one_tell_outside_the_lock() {
        let (guard, _shared) = armed();
        let sink = RecordingSink::watching(&guard);
        set_fm_mix_base_level(&guard, 7, 100, &sink).expect("base");
        assert_eq!(sink.lines(), vec!["fm mix base applied generation=7 level=100"]);
        assert!(sink.every_emit_was_outside_the_lock());
    }

    #[test]
    fn a_failed_or_refused_base_emits_no_tell() {
        let (guard, shared) = armed();
        let sink = RecordingSink::watching(&guard);
        assert!(set_fm_mix_base_level(&guard, 7, 101, &sink).is_err());
        assert!(set_fm_mix_base_level(&guard, 7, -1, &sink).is_err());
        shared.lock().expect("fake lock").fail_with = Some(32);
        assert!(set_fm_mix_base_level(&guard, 7, 100, &sink).is_err());
        assert!(sink.lines().is_empty());
    }
}
