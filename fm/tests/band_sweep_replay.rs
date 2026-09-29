// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Replay a recorded SNR/RSSI band sweep through the real seek logic.
//!
//! # Why this file exists
//!
//! D3-0117 wired the seek worker and flew PARTIAL: the thread runs, but "does
//! the walk stop on a station" was left to RF. That question cannot be settled
//! at the desk — but two questions underneath it can, and they were the ones
//! actually going wrong:
//!
//! 1. **Is the SNR the walk judges the SNR of the channel it just tuned?**
//!    D3-0117's `BandScan::poll` tuned and read register 0x02 in one call, with
//!    nothing in between. Device 1's walk — the one that finds stations on this
//!    hardware family — sleeps `radioSeekSettle = 100 ms` between exactly those
//!    two operations (`~/android/apps/mcu-toolbox/bridge/radio.go:40,473-486`).
//! 2. **When a walk finds nothing, does it say why?** It did not. Both a walk
//!    that measured 205 empty channels and a walk that never stepped ended as
//!    `on_seek_stopped(band_exhausted = true)` with nothing written anywhere.
//!
//! Both are answerable against a table of numbers, so they are answered here.
//!
//! # The bus model, and what part of it is a MODEL
//!
//! [`SweepBus`] is an [`I2cBus`] whose SNR/RSSI registers hold the values of
//! the last channel that was allowed to **settle**. `settle()` stands for the
//! caller waiting [`SEEK_SETTLE_MS`]; a walk that never calls it is D3-0117.
//!
//! The one-channel lag is a **model, not a measurement**. What the QN8035
//! really reports in the window between the RXREQ write and PLL lock is not in
//! this repository — it is either the previous channel's estimate (modelled
//! here) or a relock artefact near zero. *Both* make a no-settle walk wrong;
//! they differ only in HOW it looks, and the census line this change adds tells
//! them apart on the first flight. See the module tail for that reading.
//!
//! # Replacing the fixture with a real capture
//!
//! [`parse_sweep`] accepts the `hu-qn8035 --status` output line verbatim
//! (`apps/hu-bus-tools/src/hu-qn8035.c:186-191`) as well as a bare
//! `<MHz> <snr> <rssi>` triple, so a capture taken on device 3 can be pasted
//! into [`DEVICE_1_ANCHORS`] without reformatting.

use xyauto_fm::i2c::I2cBus;
use xyauto_fm::qn8035::{
    self, BandScan, ScanCensus, ScanOutcome, ScanStep, SeekDirection, SignalQuality, Tuner,
    SEEK_SETTLE_MS, SEEK_SNR_FLOOR,
};

/// The measured device-1 anchors, and nothing invented around them.
///
/// Every number below is quoted, not modelled:
///
/// * 97.50 → SNR 32, RSSI 91 — `radio.go:33-38` ("97.5 reads 32 ... RSSI
///   managed only 89 against 83"), `hu-qn8035.c:218-222` ("station snr 31-32"),
///   `~/android/handoff/x6-dsp-fm-userspace-prep.md` ("n 31-32", "88 vs 51").
/// * 97.40 / 97.60 → SNR 15 / 10 — the shoulders, `radio.go:34-35`
///   ("its neighbours at 10 and 15").
/// * everything else → SNR 6, RSSI 51 — "empty air at a flat 6", RSSI 51.
///
/// THIS IS DEVICE 1'S AERIAL IN DEVICE 1'S ROOM. It is a shape to test the
/// logic against, and it is explicitly NOT evidence about what device 3 hears.
/// The whole point of the census is that device 3 gets to answer for itself.
const DEVICE_1_ANCHORS: &str = "\
 97.40 MHz  snr= 15 rssi= 83
 97.50 MHz  snr= 32 rssi= 91
 97.60 MHz  snr= 10 rssi= 83
";

/// Empty air, per the same sources.
const EMPTY_SNR: u8 = 6;
/// Empty-air RSSI, per `x6-dsp-fm-userspace-prep.md` ("88 vs 51").
const EMPTY_RSSI: u8 = 51;

/// Parse one sweep table into `(kHz, snr, rssi)` rows.
///
/// Accepts a `hu-qn8035 --status` line verbatim and a bare `MHz snr rssi`
/// triple, so a device-3 capture pastes in unedited. Blank lines and `#`
/// comments are skipped; anything else is a hard failure, because a silently
/// dropped row would weaken a test rather than fail it.
fn parse_sweep(table: &str) -> Vec<(u32, u8, u8)> {
    fn field(line: &str, key: &str) -> Option<u8> {
        let rest = line.split_once(key)?.1;
        let digits: String = rest
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    }

    table
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let megahertz: f64 = line
                .split_whitespace()
                .next()
                .and_then(|word| word.parse().ok())
                .unwrap_or_else(|| panic!("no leading frequency in sweep row {line:?}"));
            let khz = (megahertz * 1000.0).round() as u32;
            let (snr, rssi) = match (field(line, "snr="), field(line, "rssi=")) {
                (Some(snr), Some(rssi)) => (snr, rssi),
                _ => {
                    let mut columns = line.split_whitespace().skip(1);
                    let snr = columns
                        .next()
                        .and_then(|word| word.parse().ok())
                        .unwrap_or_else(|| panic!("no snr in sweep row {line:?}"));
                    let rssi = columns
                        .next()
                        .and_then(|word| word.parse().ok())
                        .unwrap_or_else(|| panic!("no rssi in sweep row {line:?}"));
                    (snr, rssi)
                }
            };
            (khz, snr, rssi)
        })
        .collect()
}

/// A whole-band sweep: the anchors where they were measured, empty air between.
fn anchored_band() -> Vec<(u32, u8, u8)> {
    let anchors = parse_sweep(DEVICE_1_ANCHORS);
    let mut band = Vec::new();
    let mut frequency = qn8035::MIN_KHZ;
    while frequency <= qn8035::MAX_KHZ {
        let row = anchors
            .iter()
            .find(|(khz, _, _)| *khz == frequency)
            .copied()
            .unwrap_or((frequency, EMPTY_SNR, EMPTY_RSSI));
        band.push(row);
        frequency += qn8035::STEP_KHZ;
    }
    band
}

/// Front-end state the harness has to reach while the tuner owns the bus.
struct SweepState {
    band: Vec<(u32, u8, u8)>,
    /// The channel word the last tune wrote — tuned, not yet settled.
    tuned_khz: u32,
    /// The channel whose numbers registers 0x02/0x03 currently report.
    settled_khz: u32,
    pending_word: u16,
    /// Every channel a read was answered for, in order.
    reported: Vec<u32>,
}

impl SweepState {
    /// Stand in for the caller waiting [`SEEK_SETTLE_MS`].
    fn settle(&mut self) {
        self.settled_khz = self.tuned_khz;
    }

    fn row(&self, khz: u32) -> (u32, u8, u8) {
        self.band
            .iter()
            .find(|(frequency, _, _)| *frequency == khz)
            .copied()
            .unwrap_or((khz, EMPTY_SNR, EMPTY_RSSI))
    }
}

/// An [`I2cBus`] that plays back a sweep, and only after a settle.
///
/// The shared handle is what lets the harness represent the wait: `Tuner` owns
/// the bus, and the settle is something that happens to the hardware while the
/// caller is not touching it.
#[derive(Clone)]
struct SweepBus(std::rc::Rc<std::cell::RefCell<SweepState>>);

impl SweepBus {
    fn new(band: Vec<(u32, u8, u8)>, resting_at: u32) -> Self {
        Self(std::rc::Rc::new(std::cell::RefCell::new(SweepState {
            band,
            tuned_khz: resting_at,
            settled_khz: resting_at,
            pending_word: qn8035::frequency_word(resting_at).unwrap(),
            reported: Vec::new(),
        })))
    }
}

impl I2cBus for SweepBus {
    type Error = &'static str;

    fn write(&mut self, address: u16, bytes: &[u8]) -> Result<(), Self::Error> {
        assert_eq!(address, qn8035::ADDRESS, "the walk addressed the wrong part");
        let mut state = self.0.borrow_mut();
        match bytes {
            [qn8035::reg::CHANNEL, low] => {
                state.pending_word = (state.pending_word & 0xff00) | u16::from(*low);
            }
            [qn8035::reg::CHANNEL_STEP, high] => {
                state.pending_word = (state.pending_word & 0x00ff) | (u16::from(*high & 0x03) << 8);
            }
            [qn8035::reg::SYSTEM1, _] => {
                // RXREQ latches the channel. The demodulator is now retuning;
                // 0x02/0x03 do NOT change until `settle()`.
                state.tuned_khz = qn8035::word_frequency(state.pending_word);
            }
            _ => {}
        }
        Ok(())
    }

    fn write_read(
        &mut self,
        address: u16,
        write: &[u8],
        read: &mut [u8],
    ) -> Result<(), Self::Error> {
        assert_eq!(address, qn8035::ADDRESS, "the walk addressed the wrong part");
        let mut state = self.0.borrow_mut();
        let settled = state.settled_khz;
        let (_, snr, rssi) = state.row(settled);
        match write {
            [qn8035::reg::STATUS1] => {
                // The AGC reports settled exactly when the harness has let the
                // channel settle, which is the whole modelled distinction.
                read[0] = if state.settled_khz == state.tuned_khz {
                    qn8035::status1::RX_AGC_SETTLED
                } else {
                    0x00
                };
            }
            [qn8035::reg::SNR] => {
                state.reported.push(settled);
                read[0] = snr;
            }
            [qn8035::reg::RSSI] => read[0] = rssi,
            _ => read.fill(0),
        }
        Ok(())
    }
}

/// Run a whole walk. `honour_settle` false reproduces D3-0117.
fn replay(
    start: u32,
    direction: SeekDirection,
    honour_settle: bool,
) -> (Option<u32>, ScanCensus, Vec<u32>) {
    let bus = SweepBus::new(anchored_band(), start);
    let handle = bus.clone();
    let mut tuner = Tuner::new(bus);
    let mut scan = BandScan::new(start, direction).unwrap();
    // Two calls per channel, plus slack; a runaway loop must fail, not hang.
    let budget = 4 * ((qn8035::MAX_KHZ - qn8035::MIN_KHZ) / qn8035::STEP_KHZ + 1);
    for _ in 0..budget {
        let landed = match scan.step(&mut tuner).unwrap() {
            ScanStep::Settling(_, _) => {
                if honour_settle {
                    // The caller's SEEK_SETTLE_MS wait, with the lock dropped.
                    handle.0.borrow_mut().settle();
                }
                continue;
            }
            ScanStep::Decided(ScanOutcome::Continue) => continue,
            ScanStep::Decided(ScanOutcome::Complete(khz)) => Some(khz),
            ScanStep::Decided(ScanOutcome::Exhausted) => None,
        };
        let reported = handle.0.borrow().reported.clone();
        return (landed, scan.census(), reported);
    }
    panic!("the walk neither landed nor exhausted the band within its budget");
}

#[test]
fn the_fixture_parses_and_the_anchors_are_the_measured_device_1_numbers() {
    let anchors = parse_sweep(DEVICE_1_ANCHORS);
    assert_eq!(
        anchors,
        [(97_400, 15, 83), (97_500, 32, 91), (97_600, 10, 83)],
        "device-1 anchors: radio.go:33-38 and hu-qn8035.c:218-222"
    );
    // The bare-triple form a hand-written capture may use.
    assert_eq!(parse_sweep("# note\n\n98.7 21 90\n"), [(98_700, 21, 90)]);
}

#[test]
fn the_floor_sits_in_the_measured_gap_on_both_sides() {
    let anchors = parse_sweep(DEVICE_1_ANCHORS);
    let station = anchors.iter().map(|(_, snr, _)| *snr).max().unwrap();
    let shoulder = anchors
        .iter()
        .map(|(_, snr, _)| *snr)
        .filter(|snr| *snr != station)
        .max()
        .unwrap();
    assert!(
        shoulder < SEEK_SNR_FLOOR && SEEK_SNR_FLOOR < station,
        "SEEK_SNR_FLOOR {SEEK_SNR_FLOOR} must separate shoulder {shoulder} from station {station}"
    );
    // Compile-time: empty air must never be a station, whatever the floor
    // becomes. A retune that crosses 6 would make every channel a hit.
    const { assert!(EMPTY_SNR < SEEK_SNR_FLOOR) };
}

/// THE POSITIVE CONTROL: honour the settle and the walk lands on the station.
#[test]
fn a_walk_that_honours_the_settle_lands_on_the_station_in_the_sweep() {
    let (landed, census, _) = replay(90_000, SeekDirection::Up, true);
    assert_eq!(landed, Some(97_500), "the peak of the only cluster");
    assert_eq!(census.best, Some((97_500, 32)));
    assert_eq!(
        census.measured + census.skipped, 76,
        "90.1 up to 97.6: the walk steps one PAST the carrier to learn it was the peak"
    );
    assert_eq!(census.climbed, 0, "97.4 is under the floor, so 97.5 stands alone");
    assert_eq!(
        census.settled, census.measured,
        "every reading was taken after RXAGCSET; the settle was long enough"
    );
}

/// THE NEGATIVE CONTROL: D3-0117's shape, replayed. It does NOT find 97.5.
///
/// It stops one channel past where it started, on the SNR of the channel it
/// had *left* — visible in the census as a "best" that is not on the channel
/// the walk claims to have landed on.
#[test]
fn a_walk_that_skips_the_settle_judges_the_previous_channel() {
    let (landed, census, reported) = replay(97_500, SeekDirection::Up, false);
    // Every read returns the resting station's SNR 32, so every channel looks
    // like the starting broadcast's skirt and the walk never leaves
    // ClearingStart. Before the three-phase port this returned Some(97_600) --
    // a confident landing 100 kHz from where it began, on a reading that
    // belonged to the channel it had just left.
    assert_eq!(landed, None, "a walk that never settles never gets clear of itself");
    assert_eq!(
        census.cleared, 205,
        "with a stale reading every channel reads as the start station's skirt"
    );
    assert!(
        reported.iter().all(|khz| *khz == 97_500),
        "every read was answered for the resting channel: {reported:?}"
    );

    // And from a resting channel with no station, the same walk finds nothing
    // at all, however many channels it measures.
    let (landed, census, _) = replay(90_000, SeekDirection::Up, false);
    assert_eq!(landed, None, "no settle, no station, whatever the floor is");
    assert_eq!(census.best.map(|(_, snr)| snr), Some(EMPTY_SNR));
    assert_eq!(census.measured + census.skipped, 205, "the whole band, and all of it wrong");
}

/// The census is the tell. These are the four readings it has to keep apart.
#[test]
fn the_census_distinguishes_the_four_ways_a_scan_can_fail() {
    let never_ran = ScanCensus::default();
    assert_eq!(never_ran.measured, 0);
    assert_eq!(never_ran.best, None);
    assert!(
        never_ran.to_string().contains("measured=0"),
        "a scan that never stepped must be readable as such: {never_ran}"
    );

    // No RF at all: flat empty air across the band.
    let (_, dead, _) = replay(90_000, SeekDirection::Up, false);
    assert_eq!(dead.best.map(|(_, snr)| snr), Some(EMPTY_SNR));
    assert_eq!(dead.runner_up.map(|(_, snr)| snr), Some(EMPTY_SNR));

    // Real stations under a too-high floor: one clear peak, a distinct
    // runner-up, both below the floor. This is the reading that would justify
    // lowering SEEK_SNR_FLOOR -- and nothing else is.
    let mut under_floor = ScanCensus::default();
    for (khz, snr, rssi) in [(93_000, 18, 88), (97_500, 14, 85), (100_100, 6, 51)] {
        under_floor.record(
            khz,
            SignalQuality {
                snr,
                rssi,
                station_present: qn8035::station_present(snr),
            },
            true,
        );
    }
    assert_eq!(under_floor.best, Some((93_000, 18)));
    assert_eq!(under_floor.runner_up, Some((97_500, 14)));
    assert_eq!(under_floor.peak_rssi, Some((93_000, 88)));
    assert!(under_floor.best.unwrap().1 < SEEK_SNR_FLOOR);

    // A logic fault: something at or above the floor, yet the walk did not stop.
    let (landed, plenty, _) = replay(90_000, SeekDirection::Up, true);
    assert!(landed.is_some() && plenty.best.unwrap().1 >= SEEK_SNR_FLOOR);
}

/// One line, every field, no newline: the kmsg budget is 10 records per 5 s.
#[test]
fn the_census_renders_as_one_greppable_line() {
    let (_, census, _) = replay(90_000, SeekDirection::Up, true);
    let line = census.to_string();
    assert!(!line.contains('\n'), "one record, not three: {line}");
    for field in [
        "measured=4",
        "settled=4",
        "screened=74",
        "skipped=72",
        "promoted=2",
        "cleared=0",
        "climbed=0",
        &format!("floor={SEEK_SNR_FLOOR}"),
        &format!("settle_ms={SEEK_SETTLE_MS}"),
        "best=97500/32",
        "peak_rssi=97500/91",
    ] {
        assert!(line.contains(field), "{field:?} missing from {line:?}");
    }
    assert!(
        ScanCensus::default().to_string().contains("best=none"),
        "an unstepped walk must render its emptiness, not omit the field"
    );
}

// Synthetic signal shapes exercise centre selection, not physical RF accuracy.
fn replay_shape(band: &[(u32, u8, u8)], start: u32, direction: SeekDirection) -> u32 {
    let bus = SweepBus::new(band.to_vec(), start);
    let handle = bus.clone();
    let mut tuner = Tuner::new(bus);
    let mut scan = BandScan::new(start, direction).unwrap();
    for _ in 0..1000 {
        match scan.step(&mut tuner).unwrap() {
            ScanStep::Settling(_, _) => handle.0.borrow_mut().settle(),
            ScanStep::Decided(ScanOutcome::Continue) => (),
            ScanStep::Decided(ScanOutcome::Complete(khz)) => {
                assert_eq!(handle.0.borrow().tuned_khz, khz, "final hardware retune");
                return khz;
            }
            ScanStep::Decided(ScanOutcome::Exhausted) => panic!("expected a station"),
        }
    }
    panic!("walk exceeded bounded step count");
}

#[test]
fn flat_peak_centre_does_not_depend_on_scan_direction() {
    let band = [(90_200, 30, 90), (90_300, 30, 90), (90_400, 30, 90)];
    for (start, direction) in [(90_000, SeekDirection::Up), (90_600, SeekDirection::Down)] {
        assert_eq!(replay_shape(&band, start, direction), 90_300);
    }
}

#[test]
fn even_flat_peak_uses_lower_grid_centre_in_both_directions() {
    let band = [(90_200, 30, 90), (90_300, 30, 90)];
    for (start, direction) in [(90_000, SeekDirection::Up), (90_500, SeekDirection::Down)] {
        assert_eq!(replay_shape(&band, start, direction), 90_200);
    }
}

#[test]
fn distinct_equal_peaks_are_not_averaged_across_a_dip() {
    let band = [(90_200, 30, 90), (90_300, 25, 90), (90_400, 30, 90)];
    assert_eq!(replay_shape(&band, 90_000, SeekDirection::Up), 90_200);
    assert_eq!(replay_shape(&band, 90_600, SeekDirection::Down), 90_400);
}

#[test]
fn stronger_unique_peak_replaces_a_flat_peak() {
    let band = [(90_200, 30, 90), (90_300, 30, 90), (90_400, 35, 90), (90_500, 25, 90)];
    for (start, direction) in [(90_000, SeekDirection::Up), (90_700, SeekDirection::Down)] {
        assert_eq!(replay_shape(&band, start, direction), 90_400);
    }
}

#[test]
fn flat_peak_at_each_band_edge_stays_on_grid() {
    for (band, start, direction, expected) in [
        ([(107_800, 30, 90), (107_900, 30, 90), (108_000, 30, 90)], 107_600, SeekDirection::Up, 107_900),
        ([(87_500, 30, 90), (87_600, 30, 90), (87_700, 30, 90)], 87_900, SeekDirection::Down, 87_600),
    ] {
        assert_eq!(replay_shape(&band, start, direction), expected);
    }
}

#[test]
fn equal_peaks_across_band_wrap_are_not_a_single_plateau() {
    let band = [(108_000, 30, 90), (87_500, 30, 90)];
    assert_eq!(replay_shape(&band, 107_800, SeekDirection::Up), 108_000);
    assert_eq!(replay_shape(&band, 87_700, SeekDirection::Down), 87_500);
}

// Device-3 90.3 MHz samples from fm-fine-samples.jsonl in
// kernel/out/.tmp/car-failures-20260928-lead: (ms, SNR)
// (314,18), (624,25), (904,29), (1181,32), (1464,32).
// Times were measured AFTER UI tap completion, not at the RXREQ write.
// This fixture conservatively holds the previous sample between timestamps
// and holds the first sample before 314 ms. That early value is a MODEL,
// not a measured 100 ms reading; other frequencies are synthetic empty air.
// RXAGCSET is deliberately already set: it does not certify SNR convergence.
#[derive(Clone)]
struct TimedSweepBus {
    bus: SweepBus,
    age_ms: std::rc::Rc<std::cell::Cell<u64>>,
}

impl TimedSweepBus {
    fn new(start: u32) -> Self {
        Self {
            bus: SweepBus::new(Vec::new(), start),
            age_ms: std::rc::Rc::new(std::cell::Cell::new(0)),
        }
    }
}

impl I2cBus for TimedSweepBus {
    type Error = &'static str;

    fn write(&mut self, address: u16, bytes: &[u8]) -> Result<(), Self::Error> {
        self.bus.write(address, bytes)?;
        if matches!(bytes, [qn8035::reg::SYSTEM1, _]) {
            self.age_ms.set(0);
            self.bus.0.borrow_mut().settle();
        }
        Ok(())
    }

    fn write_read(
        &mut self,
        address: u16,
        write: &[u8],
        read: &mut [u8],
    ) -> Result<(), Self::Error> {
        self.bus.write_read(address, write, read)?;
        if write == [qn8035::reg::SNR] && self.bus.0.borrow().tuned_khz == 90_300 {
            read[0] = match self.age_ms.get() {
                0..624 => 18,
                624..904 => 25,
                904..1181 => 29,
                _ => 32,
            };
        }
        Ok(())
    }
}

fn replay_slow_station(settle_ms: u64, start: u32, direction: SeekDirection) -> Option<u32> {
    let bus = TimedSweepBus::new(start);
    let handle = bus.clone();
    let mut tuner = Tuner::new(bus);
    let mut scan = BandScan::new(start, direction).unwrap();
    for _ in 0..1000 {
        let landed = match scan.step(&mut tuner).unwrap() {
            ScanStep::Settling(_, wait_ms) => {
                let paid_ms = wait_ms * settle_ms / SEEK_SETTLE_MS;
                handle.age_ms.set(handle.age_ms.get() + paid_ms);
                continue;
            }
            ScanStep::Decided(ScanOutcome::Continue) => continue,
            ScanStep::Decided(ScanOutcome::Complete(khz)) => Some(khz),
            ScanStep::Decided(ScanOutcome::Exhausted) => None,
        };
        let census = scan.census();
        assert_eq!(census.measured, census.settled, "AGC ready throughout");
        assert_eq!(census.resettled, 0, "AGC cannot request the missing wait");
        assert_eq!(handle.bus.0.borrow().tuned_khz, landed.unwrap_or(start));
        return landed;
    }
    panic!("timed walk exceeded bounded step count");
}

#[test]
fn old_100_ms_policy_misses_the_slow_station_despite_agc_ready() {
    for (start, direction) in [(90_000, SeekDirection::Up), (90_600, SeekDirection::Down)] {
        assert_eq!(replay_slow_station(100, start, direction), None);
    }
}

#[test]
fn production_settle_finds_the_slow_station_in_both_directions() {
    for (start, direction) in [(90_000, SeekDirection::Up), (90_600, SeekDirection::Down)] {
        assert_eq!(replay_slow_station(SEEK_SETTLE_MS, start, direction), Some(90_300));
    }
}

#[test]
fn a_slow_station_walk_can_be_cancelled_during_the_wait() {
    let bus = TimedSweepBus::new(90_000);
    let handle = bus.clone();
    let mut tuner = Tuner::new(bus);
    let mut scan = BandScan::new(90_000, SeekDirection::Up).unwrap();
    assert_eq!(scan.step(&mut tuner).unwrap(), ScanStep::Settling(90_100, SEEK_SETTLE_MS));
    scan.restore(&mut tuner).unwrap();
    assert_eq!(handle.bus.0.borrow().tuned_khz, 90_000);
    assert_eq!(handle.age_ms.get(), 0, "restore does not wait for the dwell");
}

#[test]
fn empty_band_requests_under_two_minutes_of_dwell() {
    let bus = SweepBus::new(Vec::new(), 90_000);
    let handle = bus.clone();
    let mut tuner = Tuner::new(bus);
    let mut scan = BandScan::new(90_000, SeekDirection::Up).unwrap();
    let mut total_wait_ms = 0;
    for _ in 0..1000 {
        match scan.step(&mut tuner).unwrap() {
            ScanStep::Settling(_, wait_ms) => {
                total_wait_ms += wait_ms;
                handle.0.borrow_mut().settle();
            }
            ScanStep::Decided(ScanOutcome::Continue) => (),
            ScanStep::Decided(ScanOutcome::Exhausted) => {
                assert!(total_wait_ms < 120_000, "empty air requested {total_wait_ms} ms");
                assert_eq!(total_wait_ms, SEEK_SETTLE_MS + 204 * qn8035::SEEK_SCREEN_MS);
                assert_eq!(handle.0.borrow().tuned_khz, 90_000, "last early skip must restore");
                return;
            }
            ScanStep::Decided(ScanOutcome::Complete(_)) => panic!("empty air is not a station"),
        }
    }
    panic!("empty walk did not terminate");
}

// Representative native Binder observations, collected through the sole HAL
// owner during the old 1200 ms walk. Source: lead/precise-groups.json in
// kernel/out/.tmp/fm-adaptive-20260928-lead. At 90.3 the early SNR19 sample
// is bracketed314.70–371.77ms and20 at370.50–427.65ms, late31–32. Empty
// 104.0 has early10 at327.54–386.92/381.77–443.42ms, late7. Use the lower
// candidate and higher empty reading conservatively. The piecewise extension
// to unmeasured ages is a MODEL, not a claim of exact400ms/1200ms hardware.
#[derive(Clone)]
struct AdaptiveBus {
    bus: SweepBus,
    state: std::rc::Rc<std::cell::RefCell<AdaptiveState>>,
}

struct AdaptiveState {
    age_ms: u64,
    curves: Vec<(u32, u8, u8)>,
    default_curve: (u8, u8),
    never_agc_ready: bool,
    fail_reg: Option<u8>,
    fail_write_reg: Option<u8>,
    tunes: Vec<u32>,
    reads: Vec<(u32, u8, u64)>,
}

impl AdaptiveBus {
    fn new(start: u32, curves: Vec<(u32, u8, u8)>, default_curve: (u8, u8)) -> Self {
        Self {
            bus: SweepBus::new(Vec::new(), start),
            state: std::rc::Rc::new(std::cell::RefCell::new(AdaptiveState {
                age_ms: 0, curves, default_curve, never_agc_ready: false,
                fail_reg: None, fail_write_reg: None, tunes: Vec::new(), reads: Vec::new(),
            })),
        }
    }

    fn wait(&self, ms: u64) {
        self.state.borrow_mut().age_ms += ms;
    }

    fn tuned(&self) -> u32 { self.bus.0.borrow().tuned_khz }
}

impl I2cBus for AdaptiveBus {
    type Error = &'static str;
    fn write(&mut self, address: u16, bytes: &[u8]) -> Result<(), Self::Error> {
        if self.state.borrow().fail_write_reg == Some(bytes[0]) {
            self.state.borrow_mut().fail_write_reg = None;
            return Err("EREMOTEIO: restore NACK");
        }
        self.bus.write(address, bytes)?;
        if matches!(bytes, [qn8035::reg::SYSTEM1, _]) {
            self.bus.0.borrow_mut().settle();
            let mut state = self.state.borrow_mut();
            state.age_ms = 0;
            state.tunes.push(self.tuned());
        }
        Ok(())
    }
    fn write_read(&mut self, address: u16, write: &[u8], read: &mut [u8]) -> Result<(), Self::Error> {
        let mut state = self.state.borrow_mut();
        let frequency = self.tuned();
        let age_ms = state.age_ms;
        state.reads.push((frequency, write[0], age_ms));
        if state.fail_reg == Some(write[0]) {
            state.fail_reg = None;
            return Err("EREMOTEIO: NACK");
        }
        self.bus.write_read(address, write, read)?;
        if write == [qn8035::reg::STATUS1] && state.never_agc_ready {
            read[0] = 0;
        }
        if write == [qn8035::reg::SNR] {
            let (early, late) = state.curves.iter()
                .find(|(khz, _, _)| *khz == frequency)
                .map(|(_, early, late)| (*early, *late))
                .unwrap_or(state.default_curve);
            read[0] = if age_ms < SEEK_SETTLE_MS { early } else { late };
        }
        Ok(())
    }
}

fn adaptive_walk(bus: AdaptiveBus, start: u32, direction: SeekDirection)
    -> (Option<u32>, ScanCensus, u64) {
    let handle = bus.clone();
    let mut tuner = Tuner::new(bus);
    let mut scan = BandScan::new(start, direction).unwrap();
    let mut total_ms = 0;
    for _ in 0..1000 {
        match scan.step(&mut tuner).unwrap() {
            ScanStep::Settling(_, ms) => { handle.wait(ms); total_ms += ms; }
            ScanStep::Decided(ScanOutcome::Continue) => (),
            ScanStep::Decided(outcome) => {
                let landed = match outcome { ScanOutcome::Complete(khz) => Some(khz), _ => None };
                assert_eq!(handle.tuned(), landed.unwrap_or(start));
                return (landed, scan.census(), total_ms);
            }
        }
    }
    panic!("adaptive walk exceeded step bound");
}

#[test]
fn native_early_late_samples_keep_903_and_skip_weak_air_both_directions() {
    for (start, direction) in [(90_000, SeekDirection::Up), (90_600, SeekDirection::Down)] {
        let bus = AdaptiveBus::new(start, vec![(90_300, 19, 31)], (10, 7));
        let handle = bus.clone();
        let (landed, census, _) = adaptive_walk(bus, start, direction);
        assert_eq!(landed, Some(90_300));
        assert_eq!((census.screened, census.skipped, census.promoted), (2, 1, 1));
        assert_eq!(census.best, Some((90_300, 31)), "early19 is never a peak score");
        let reads: Vec<_> = handle.state.borrow().reads.iter()
            .filter(|(khz, reg, _)| *khz == 90_300 && *reg == qn8035::reg::SNR).copied().collect();
        assert_eq!(reads, [(90_300, qn8035::reg::SNR, qn8035::SEEK_SCREEN_MS),
                          (90_300, qn8035::reg::SNR, SEEK_SETTLE_MS)]);
        assert_eq!(handle.state.borrow().tunes.iter().filter(|khz| **khz == 90_300).count(), 2,
                   "one candidate tune and terminal landing; promotion must not retune");
    }
}

#[test]
fn screening_never_clears_a_starting_skirt_or_ends_a_peak_early() {
    // Synthetic early6/late30 skirt and early6/late34 peak specifically
    // reject extending the fast screen into either protected phase.
    let bus = AdaptiveBus::new(90_000, vec![(90_100, 6, 30), (90_300, 19, 28),
                                        (90_400, 6, 34)], (10, 7));
    let handle = bus.clone();
    let (landed, census, _) = adaptive_walk(bus, 90_000, SeekDirection::Up);
    assert_eq!(landed, Some(90_400));
    assert_eq!(census.cleared, 1);
    for frequency in [90_100, 90_200, 90_400, 90_500] {
        assert!(handle.state.borrow().reads.iter()
            .filter(|(khz, reg, _)| *khz == frequency && *reg == qn8035::reg::SNR)
            .all(|(_, _, age)| *age >= SEEK_SETTLE_MS));
    }
}

#[test]
fn threshold_boundary_and_unready_agc_keep_the_full_budget() {
    for never_agc_ready in [false, true] {
        let bus = AdaptiveBus::new(90_000, Vec::new(), (11, 7));
        bus.state.borrow_mut().never_agc_ready = never_agc_ready;
        let (_, census, total_ms) = adaptive_walk(bus, 90_000, SeekDirection::Up);
        assert_eq!((census.measured, census.screened, census.skipped, census.promoted), (205, 204, 0, 204));
        let extra = if never_agc_ready { qn8035::SEEK_RESETTLE_PER_WALK } else { 0 };
        assert_eq!(census.resettled, extra);
        assert_eq!(total_ms, (205 + u64::from(extra)) * SEEK_SETTLE_MS);
        assert!(total_ms + 205 * 10 < qn8035::SEEK_DEADLINE_MS);
    }
    let bus = AdaptiveBus::new(90_000, Vec::new(), (10, 7));
    bus.state.borrow_mut().never_agc_ready = true;
    let (_, census, _) = adaptive_walk(bus, 90_000, SeekDirection::Up);
    assert_eq!(census.skipped, 0, "low SNR is not a skip with AGC unready");
}

#[test]
fn read_errors_leave_early_and_promoted_measurements_retryable_without_retune() {
    for promoted in [false, true] {
        for reg in [qn8035::reg::STATUS1, qn8035::reg::SNR, qn8035::reg::RSSI] {
            let bus = AdaptiveBus::new(90_000, Vec::new(), (11, 7));
            let handle = bus.clone();
            let mut tuner = Tuner::new(bus);
            let mut scan = BandScan::new(90_000, SeekDirection::Up).unwrap();
            for _ in 0..3 { // tune/read ClearingStart, then tune Searching
                if let ScanStep::Settling(_, ms) = scan.step(&mut tuner).unwrap() { handle.wait(ms); }
            }
            if promoted {
                match scan.step(&mut tuner).unwrap() {
                    ScanStep::Settling(90_200, ms) => handle.wait(ms),
                    other => panic!("expected promoted wait, got {other:?}"),
                }
            }
            let census = scan.census();
            let tunes = handle.state.borrow().tunes.clone();
            handle.state.borrow_mut().fail_reg = Some(reg);
            assert!(scan.step(&mut tuner).is_err());
            assert_eq!(scan.census(), census);
            assert_eq!(handle.tuned(), 90_200);
            let retry = scan.step(&mut tuner).unwrap();
            if promoted {
                assert_eq!(retry, ScanStep::Decided(ScanOutcome::Continue));
            } else {
                assert_eq!(retry, ScanStep::Settling(90_200, SEEK_SETTLE_MS - qn8035::SEEK_SCREEN_MS));
            }
            assert_eq!(handle.state.borrow().tunes, tunes);
        }
    }
}

#[test]
fn cancel_restores_immediately_during_full_screen_and_promoted_waits() {
    for wait_index in 0..4 {
        let bus = AdaptiveBus::new(90_000, vec![(90_200, 19, 31)], (10, 7));
        let handle = bus.clone();
        let mut tuner = Tuner::new(bus);
        let mut scan = BandScan::new(90_000, SeekDirection::Up).unwrap();
        let mut waits = 0;
        loop {
            match scan.step(&mut tuner).unwrap() {
                ScanStep::Settling(_, ms) => {
                    if waits == wait_index { break; }
                    handle.wait(ms);
                    waits += 1;
                }
                ScanStep::Decided(ScanOutcome::Continue) => (),
                other => panic!("ended before wait{wait_index}: {other:?}"),
            }
        }
        // Owner drops the walk after restore. No further scan step is allowed.
        scan.restore(&mut tuner).unwrap();
        assert_eq!(handle.tuned(), 90_000);
        assert_eq!(handle.state.borrow().age_ms, 0);
    }
}

#[test]
fn final_early_skip_restore_failure_does_not_consume_the_channel() {
    for reg in [qn8035::reg::CHANNEL, qn8035::reg::CHANNEL_STEP, qn8035::reg::SYSTEM1] {
        let bus = AdaptiveBus::new(90_000, Vec::new(), (10, 7));
        let handle = bus.clone();
        let mut tuner = Tuner::new(bus);
        let mut scan = BandScan::new(90_000, SeekDirection::Up).unwrap();
        loop {
            match scan.step(&mut tuner).unwrap() {
                ScanStep::Settling(khz, ms) => {
                    handle.wait(ms);
                    if khz == 89_900 { break; }
                }
                ScanStep::Decided(ScanOutcome::Continue) => (),
                other => panic!("ended before the last channel: {other:?}"),
            }
        }
        let census = scan.census();
        handle.state.borrow_mut().fail_write_reg = Some(reg);
        assert!(scan.step(&mut tuner).is_err());
        assert_eq!(scan.census(), census);
        assert_eq!(handle.tuned(), 89_900);
        assert_eq!(scan.step(&mut tuner).unwrap(), ScanStep::Decided(ScanOutcome::Exhausted));
        assert_eq!(handle.tuned(), 90_000);
        assert_eq!(scan.census().skipped, census.skipped + 1);
    }
}
