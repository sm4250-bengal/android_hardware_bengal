// SPDX-License-Identifier: Apache-2.0
//! Same-session PMIC wake evidence feeds the existing MCU owner, never a
//! second UART writer or a new Android sleep/wake policy.

use crate::mcu::acc::{State, SuspendProof};
use crate::mcu::serial::{AccSession, PlatformHooks, SessionError};
use std::io::{self, Read, Write};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts { pub success: u64, pub fail: u64 }

pub(crate) trait Source {
    fn counts(&mut self) -> io::Result<Counts>;
    fn reason(&mut self) -> io::Result<String>;
}

pub(crate) struct Sysfs;
impl Source for Sysfs {
    fn counts(&mut self) -> io::Result<Counts> {
        fn count(name: &str) -> io::Result<u64> {
            let text = bounded(&format!("/sys/power/suspend_stats/{name}"))?;
            let text = text.strip_suffix('\n').unwrap_or(&text);
            if text.is_empty() || !text.bytes().all(|c| c.is_ascii_digit()) {
                return Err(io::Error::other("invalid suspend counter"));
            }
            text.parse().map_err(io::Error::other)
        }
        Ok(Counts { success: count("success")?, fail: count("fail")? })
    }
    fn reason(&mut self) -> io::Result<String> {
        bounded("/sys/kernel/wakeup_reasons/last_resume_reason")
    }
}

fn bounded(path: &str) -> io::Result<String> {
    let mut text = String::new();
    std::fs::File::open(path)?.take(257).read_to_string(&mut text)?;
    if text.len() > 256 { return Err(io::Error::other("oversized wake status")); }
    Ok(text)
}

// Accept only the observed PMIC IRQ name. IRQ numbers may change with probe
// order. Abort/unknown, additional simultaneous sources and lookalikes fail.
fn pmic_reason(text: &str) -> bool {
    let fields: Vec<_> = text.split_ascii_whitespace().collect();
    fields.len() == 2 && fields[0].parse::<u32>().is_ok()
        && fields[1] == "pm8941_pwrkey"
}

#[derive(Debug)]
pub(crate) enum Observation {
    None,
    Resumed { success: u64 },
    Unavailable(String),
}

pub(crate) struct Monitor<S> {
    source: S,
    baseline: Option<(u64, Counts)>,
    warned: bool,
}

impl<S: Source> Monitor<S> {
    pub(crate) fn new(source: S) -> Self {
        Self { source, baseline: None, warned: false }
    }

    pub(crate) fn step<T: Read + Write, P: PlatformHooks>(
        &mut self, session: &mut AccSession<T, P>, now_ms: u64,
    ) -> Result<Observation, SessionError> {
        let generation = session.generation();
        match session.state() {
            State::Idle | State::Resumed => {
                self.baseline = None;
                return Ok(Observation::None);
            }
            State::AccOffGrace | State::FinalStage => {
                // Capture BEFORE suspend is authorized, not on the first tick
                // after wake. A new UART session gets a new Monitor as well.
                if self.baseline.map(|v| v.0) != Some(generation) {
                    match self.source.counts() {
                        Ok(counts) => {
                            self.baseline = Some((generation, counts));
                            self.warned = false;
                        }
                        Err(error) => return Ok(self.unavailable(error)),
                    }
                }
                return Ok(Observation::None);
            }
            State::Suspending => {}
        }
        let Some((owner, baseline)) = self.baseline else { return Ok(Observation::None); };
        if owner != generation { return Ok(Observation::None); }
        // Kernel writes the reason during resume and increments success after
        // thaw. Bracket the reason with BOTH counters; mixed/failed observations
        // and old PMIC reasons can never authorize a new cycle's cleanup.
        let sample = (|| -> io::Result<(Counts, bool)> {
            let before = self.source.counts()?;
            if before == baseline { return Ok((before, false)); }
            let pmic = pmic_reason(&self.source.reason()?);
            if self.source.counts()? != before {
                return Err(io::Error::other("wake counters changed during sample"));
            }
            Ok((before, pmic))
        })();
        let (counts, pmic) = match sample {
            Ok(sample) => { self.warned = false; sample }
            Err(error) => return Ok(self.unavailable(error)),
        };
        if counts.success < baseline.success || counts.fail < baseline.fail {
            // Counter reset/wrap cannot establish freshness for this cycle.
            self.baseline = None;
            return Ok(self.unavailable(io::Error::other("wake counter regressed")));
        }
        self.baseline = Some((owner, counts));
        if counts.success <= baseline.success || counts.fail != baseline.fail || !pmic {
            return Ok(Observation::None);
        }
        let token = session.wake_token();
        if session.hardware_wake(token, SuspendProof::Completed, now_ms)? {
            self.baseline = None;
            Ok(Observation::Resumed { success: counts.success })
        } else { Ok(Observation::None) }
    }

    fn unavailable(&mut self, error: io::Error) -> Observation {
        if self.warned { return Observation::None; }
        self.warned = true;
        Observation::Unavailable(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct Io { input: io::Cursor<Vec<u8>>, output: Vec<u8> }
    impl Read for Io {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> { self.input.read(bytes) }
    }
    impl Write for Io {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(bytes); Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    #[derive(Default)]
    struct Hooks { cycles: Vec<crate::mcu::acc::CycleEvent> }
    impl PlatformHooks for Hooks {
        fn on_cycle_event(&mut self, event: crate::mcu::acc::CycleEvent) { self.cycles.push(event); }
        fn enter_acc_off_grace(&mut self) -> io::Result<()> { Ok(()) }
        fn quiesce_radio(&mut self) -> io::Result<()> { Ok(()) }
        fn detach_stick_sink(&mut self) -> io::Result<()> { Ok(()) }
        fn quiesce_usb(&mut self) -> io::Result<()> { Ok(()) }
        fn resume(&mut self) -> io::Result<()> { Ok(()) }
        fn publish_acc_state(&mut self, _: State, _: &'static str, _: u64) -> io::Result<()> { Ok(()) }
        fn request_sleep(&mut self, _: u64) -> io::Result<()> { Ok(()) }
    }

    #[derive(Default)]
    struct Data { counts: Counts, reason: String, error: bool, racing: bool }
    #[derive(Clone, Default)]
    struct Fake(Rc<RefCell<Data>>);
    impl Source for Fake {
        fn counts(&mut self) -> io::Result<Counts> {
            let data = self.0.borrow();
            if data.error { Err(io::Error::other("denied")) } else { Ok(data.counts) }
        }
        fn reason(&mut self) -> io::Result<String> {
            let mut data = self.0.borrow_mut();
            if data.racing { data.counts.success += 1; }
            Ok(data.reason.clone())
        }
    }

    fn off() -> AccSession<Io, Hooks> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[0x5a, 0xa5, 3, 0x12, 3, 0xeb]);
        bytes.extend_from_slice(&[0x5a, 0xa5, 3, 5, 0x10, 0xeb]);
        let mut session = AccSession::new(Io { input: io::Cursor::new(bytes), output: Vec::new() }, Hooks::default());
        session.read_once(0).unwrap();
        assert_eq!(session.state(), State::FinalStage);
        session
    }

    #[test]
    fn completed_pmic_resume_without_uart_ends_the_actual_off_cycle() {
        let source = Fake::default();
        let mut monitor = Monitor::new(source.clone());
        let mut session = off();
        monitor.step(&mut session, 0).unwrap();
        session.advance(500).unwrap();
        source.0.borrow_mut().counts.success = 1;
        source.0.borrow_mut().reason = "20 pm8941_pwrkey\n".into();
        monitor.step(&mut session, 600).unwrap();
        assert_eq!(session.state(), State::Resumed);
        assert_eq!(session.generation(), 2);
        assert_eq!(session.platform_mut().cycles.last(),
            Some(&crate::mcu::acc::CycleEvent::Resume { off_generation: 1 }));
        let count = session.platform_mut().cycles.len();
        monitor.step(&mut session, 700).unwrap();
        assert_eq!(session.platform_mut().cycles.len(), count);
    }

    #[test]
    fn stale_failed_foreign_or_ambiguous_wake_does_not_resume() {
        for (success, fail, reason) in [
            (0, 0, "20 pm8941_pwrkey\n"),
            (1, 1, "20 pm8941_pwrkey\n"),
            (1, 0, "21 pm8xxx_rtc_alarm\n"),
            (1, 0, "45 dwc3\n"),
            (1, 0, "20 pm8941_pwrkey\n21 pm8xxx_rtc_alarm\n"),
            (1, 0, "Abort: pending wakeup"),
            (1, 0, "-1 pm8941_pwrkey"),
            (1, 0, "20 pm8941_pwrkey_fake"),
            (1, 0, ""),
        ] {
            let source = Fake::default();
            let mut monitor = Monitor::new(source.clone());
            let mut session = off();
            monitor.step(&mut session, 0).unwrap();
            session.advance(500).unwrap();
            source.0.borrow_mut().counts = Counts { success, fail };
            source.0.borrow_mut().reason = reason.into();
            assert!(matches!(monitor.step(&mut session, 600).unwrap(), Observation::None));
            assert_eq!(session.state(), State::Suspending, "{reason}");
        }
    }

    #[test]
    fn no_baseline_after_suspend_and_no_reuse_of_previous_cycle() {
        let source = Fake::default();
        source.0.borrow_mut().counts.success = 1;
        source.0.borrow_mut().reason = "20 pm8941_pwrkey\n".into();
        let mut session = off();
        session.advance(500).unwrap();
        let mut monitor = Monitor::new(source.clone());
        monitor.step(&mut session, 600).unwrap();
        assert_eq!(session.state(), State::Suspending);
        // Fresh session/cycle starts AFTER that same successful wake.
        let mut session = off();
        let mut monitor = Monitor::new(source);
        monitor.step(&mut session, 0).unwrap();
        session.advance(500).unwrap();
        monitor.step(&mut session, 600).unwrap();
        assert_eq!(session.state(), State::Suspending);
    }

    #[test]
    fn status_failure_or_race_is_not_a_wake_and_valid_retry_succeeds() {
        for race in [false, true] {
            let source = Fake::default();
            let mut monitor = Monitor::new(source.clone());
            let mut session = off();
            monitor.step(&mut session, 0).unwrap();
            session.advance(500).unwrap();
            source.0.borrow_mut().counts.success = 1;
            source.0.borrow_mut().reason = "20 pm8941_pwrkey\n".into();
            source.0.borrow_mut().error = !race;
            source.0.borrow_mut().racing = race;
            assert!(matches!(monitor.step(&mut session, 600).unwrap(), Observation::Unavailable(_)));
            assert_eq!(session.state(), State::Suspending);
            source.0.borrow_mut().error = false;
            source.0.borrow_mut().racing = false;
            monitor.step(&mut session, 700).unwrap();
            assert_eq!(session.state(), State::Resumed);
        }
    }
}
