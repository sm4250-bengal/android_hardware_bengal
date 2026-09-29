// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! QN8035 register model, tune/seek policy and RDS programme-service parser.

use crate::i2c::I2cBus;

/// QN8035 7-bit address on the `i2c-fm` adapter.
pub const ADDRESS: u16 = 0x10;
/// Value expected from register [`reg::CHIP_ID`].
pub const CHIP_ID: u8 = 0x84;
/// Lowest supported FM channel.
pub const MIN_KHZ: u32 = 87_500;
/// Highest supported FM channel.
pub const MAX_KHZ: u32 = 108_000;
/// Product channel grid.
pub const STEP_KHZ: u32 = 100;

/// Device-1 measured station floor; empty air was 6 and a station 31-32.
///
/// MEASURED, not chosen: `radioSeekSNRFloor = 20` in
/// `~/android/apps/mcu-toolbox/bridge/radio.go:39`, the walk that actually
/// finds stations on this hardware family. The margin behind the number is
/// what makes it safe — on device 1, 97.5 MHz read SNR 32 with its immediate
/// neighbours at 10 and 15 and empty air at a flat 6, while RSSI managed only
/// 89 against 83 (`radio.go:33-38`, `apps/dsp-fm-tools/src/hu-qn8035.c:218-222`,
/// `~/android/handoff/x6-dsp-fm-userspace-prep.md`). 20 sits in a 5-point-wide
/// gap on both sides of THAT station.
///
/// ⟲ CORRECTED 2026-09-05: this comment used to end "…which is why the
/// shoulders never clear it", and that generalisation is NOT SUPPORTED.
/// The 10/15 neighbours are one station (97.5) on one aerial. A day later, on
/// the same hardware family, HuRadio recorded the opposite for a different
/// station — *"observed live 2026-08-24: 90.2/90.4/90.5 recorded around strong
/// 90.3, and a knob detent then stepped 0.1 inside one station's skirt"*
/// (`platform/packages/apps/HuRadio/…/domain/RadioController.kt:384-393`), and
/// `dedupeShoulders` exists to fold them. Device 1's own seek test states it
/// outright: *"This tuner's seek lands the 90.3 broadcast at 90.4"*
/// (`~/android/apps/fm-radio-app/…/SeekTest.kt:87-88`).
///
/// Strictly, those shoulders are only recorded as clearing **16**
/// (`RadioState.REAL_STATION_SNR`), not 20 — so the claim is unproven, not
/// disproved. THE SETTLING CHECK is the census `runner_up` field: two landings
/// within 200 kHz of each other, both at or above this floor, and the walk is
/// stopping on skirts. If that happens, device 1's peak-follow phase
/// (`radio.go:527-541` — walk on while the SNR rises, settle on the maximum)
/// has to be ported; this crate deliberately does not have it.
///
/// It is device 1's aerial and device 1's location. If a walk on THIS bench
/// reports every channel below the floor, [`ScanCensus`] says at what level it
/// failed, and that census — not this constant — is what a retune argues from.
///
/// ★ THIS IS ONE OF THREE STATION THRESHOLDS IN THE PRODUCT. They are not
/// duplicates of each other and only this one gates the walk:
///
/// | value | where | what it decides |
/// |---|---|---|
/// | **20** | here | where a seek STOPS. The only one on the bus path. |
/// | 20 | `bengal_hud::fm::SEEK_SNR_FLOOR` | nothing — a dead mirror. `hud` has no Cargo dependency on this crate, so it cannot reference the authority at the desk; `platform/scripts/fm-backend-desk-test.sh` T4 pins the two literals together. |
/// | 16 | `RadioState.REAL_STATION_SNR` (HuRadio) | whether a landing the daemon already accepted is RECORDED as a station (`RadioController.kt:450`). With this floor at 20 it can never fire; lower this below 16 and it comes alive. |
///
/// 16 and 20 come from the SAME measurement (31-32 station, 6 air) — two people
/// picking a midpoint, not a disagreement about the RF. `StationDirectory`
/// grades 30/22/16 as strong/good/weak for display only.
pub const SEEK_SNR_FLOOR: u8 = 20;

/// Milliseconds a freshly tuned channel needs before register 0x02 means it.
///
/// MEASURED ON DEVICE 1, and the one thing device 3's first seek worker did
/// not carry across: `radioSeekSettle = 100 * time.Millisecond`
/// (`~/android/apps/mcu-toolbox/bridge/radio.go:40`), whose `step()` tunes,
/// sleeps exactly this, and only then reads SNR (`radio.go:473-486`).
///
/// Register 0x02 is a running carrier-to-noise estimate over the demodulator
/// output, so it does not become the new channel's number the instant the
/// RXREQ write lands — the PLL has to lock and the estimator has to refill. A
/// read issued microseconds after the tune reports the channel the tuner was
/// on BEFORE, which during a walk is the previous, also-empty candidate. That
/// is a scan that can never stop, whatever the floor is set to.
///
/// This is a delay the SCAN OWNER must honour between [`ScanStep::Settling`]
/// and the next [`BandScan::step`]; `BandScan` deliberately does not sleep, so
/// the caller can release its lock across the wait.
///
/// Device-3 sampling on 2026-09-28 found 90.3 MHz climbing from SNR 18
/// at 314 ms to 25 at 624 ms and 32 at 1181 ms after UI tap completion.
/// These are not exact tune timestamps; 1200 ms is a conservative policy to
/// let the estimator converge, not a measured upper bound for every station.
/// RXAGCSET alone does not establish that SNR has converged.
///
/// Coupled to [`SEEK_DEADLINE_MS`] and HuRadio's `RealPorts.SEEK_TIMEOUT_MS`:
/// 205 channels x (1200 ms + 10 ms worker gap), plus 20 optional re-settles,
/// is about 272 s. The backend desk gate reads all three source constants and
/// requires this budget < daemon deadline (300 s) < app timeout (330 s).
/// An empty-band seek can therefore take several minutes; cancellation still
/// runs with the service lock released during each wait.
///
/// Do not confuse this with the OTHER 100/250 ms pair in the FM path: HuRadio's
/// `settleMs = 250` is the by-ear DEPOP gap around the source handover
/// (`RadioController.kt:92-111`, glitchy at 100, clean at 250). Different
/// mechanism, different measurement, same-looking numbers.
pub const SEEK_SETTLE_MS: u64 = 1_200;

/// Early screening delay used only after clearing the starting station.
/// Device-3 call-bracketed observations retain late station candidates with
/// a conservative early floor; this is a skip filter, never a station score.
/// The observed 90.3 MHz early SNR19–20 stays above11, while sampled weak
/// channels read8–10. This empirical policy is not a guarantee for every
/// antenna/location. ClearingStart and Climbing always get the full dwell.
pub const SEEK_SCREEN_MS: u64 = 400;
/// Early SNR at or above this gets the rest of the full scoring dwell.
/// An AGC-unready channel always gets that dwell regardless of early SNR.
pub const SEEK_SCREEN_SNR_FLOOR: u8 = 11;

/// Settle after the walk's final retune, before the audio comes back.
///
/// `radioTuneSettle = 120 * time.Millisecond` (`radio.go:48-50`), used by the
/// working LOS 23.2 walk in exactly one place: between the last tune and the
/// un-mute. Its comment is the reason it cannot be dropped — *"Whichever way
/// the walk ended, the tuner has just been retuned one last time. Let that
/// land before the level comes back, or the unmute reintroduces the very
/// transient the mute was there to hide."*
pub const TUNE_SETTLE_MS: u64 = 120;

/// Wall-clock bound on one walk, counted by the scan's owner.
///
/// The scan owner restores the starting channel and audio on expiry. Keep
/// this above the full-band dwell/re-settle budget and below HuRadio's wait,
/// so a slow bus produces a terminal callback before the app gives up.
/// These bounds move together; see [`SEEK_SETTLE_MS`].
pub const SEEK_DEADLINE_MS: u64 = 300_000;

/// Extra [`SEEK_SETTLE_MS`] waits granted to ONE channel whose `RXAGCSET` is
/// still clear when the walk comes to read it.
///
/// MEASURED ON DEVICE 3, D3-0119 (2026-09-05, `boot1-after-actions`): of 220
/// channel reads across an 18-seek autoscan, 212 had `RXAGCSET` set at the
/// 100 ms mark and **8 did not** (3.6 %) — and two of those eight were the ONLY
/// reading of their walk, so the walk LANDED on a number the chip had flagged
/// as not yet valid (`measured=1 settled=0`, 97.9 MHz and 98.0 MHz). The same
/// mechanism can equally read a real station LOW and skip it.
///
/// Retain this extra wait for channels whose AGC is explicitly not ready.
/// The later 90.3 MHz measurements also show slow SNR convergence after AGC
/// readiness, which is why scoring requires the full dwell even after a
/// ready early screen. Only weak Searching channels can skip that score.
/// A chip that never settles degrades to reading as-is after the cap, never
/// to a hung walk.
pub const SEEK_RESETTLE_PER_CHANNEL: u8 = 2;

/// Ceiling on re-settles across ONE walk, so a pathological board (every
/// channel unsettled) stays bounded by `205 x settle + this x settle` and the
/// [`SEEK_DEADLINE_MS`] is a backstop, not the expected ending.
pub const SEEK_RESETTLE_PER_WALK: u32 = 20;

/// Milliseconds between RDS polls inside an open window.
///
/// Device 2's bridge — the LOS 23.2 / 6.18-ACK unit on which "FM search was
/// working well with RDS" (user, 2026-09-05) — slept exactly this between
/// polls of `STATUS2` (`~/android/apps/mcu-toolbox/bridge/radio.go:409,423,
/// 433`). Each poll is one register read, plus the eight RDS bytes only when
/// the status accepts the group, so ≈33 polls/s is ≈33–66 I2C transactions/s
/// on `i2c-fm` with audio live. That ran on device 2; it is a STARTING POINT
/// for device 3, not proof that this adapter and this audio path tolerate it
/// — the next flight row is "is a station audibly clean while its name is
/// being read".
pub const RDS_POLL_MS: u64 = 30;

/// How long a window stays open after a tune before the walk gives up on a
/// programme-service name.
///
/// Device 2's default `radio_station_name` budget, `api.go:323`
/// (`budget := 4 * time.Second`, capped at 10 s). RDS group 0A carries two PS
/// characters per group; four distinct segments are needed and the standard
/// repeats them about once a second, so a complete PS typically needs ≥ 1 s
/// and a weak signal several. The app's own 300 ms wait (`RadioController.kt:
/// 16`) therefore almost never sees the name itself; the daemon keeps reading
/// for this long and publishes through `onRdsUpdate` when it completes.
pub const RDS_WINDOW_MS: u64 = 4_000;

/// Registers used by this service.
pub mod reg {
    /// Reset, receive request, RDS enable and standby.
    pub const SYSTEM1: u8 = 0x00;
    /// Signal-to-noise estimate.
    pub const SNR: u8 = 0x02;
    /// Received signal strength.
    pub const RSSI: u8 = 0x03;
    /// CCA/AGC/FSM status. Read to check that a reading has settled.
    pub const STATUS1: u8 = 0x04;
    /// Chip identity.
    pub const CHIP_ID: u8 = 0x06;
    /// Channel word low byte.
    pub const CHANNEL: u8 = 0x07;
    /// Channel word high two bits.
    pub const CHANNEL_STEP: u8 = 0x0a;
    /// First of eight RDS block bytes.
    pub const RDS_FIRST: u8 = 0x0b;
    /// RDS synchronization and block errors.
    pub const STATUS2: u8 = 0x13;
    /// Audio gain and mute.
    pub const VOLUME: u8 = 0x14;
}

/// Values written to `SYSTEM1`.
pub mod system1 {
    /// Software reset.
    pub const SOFTWARE_RESET: u8 = 0x80;
    /// Standby.
    pub const STANDBY: u8 = 0x20;
    /// Channel-register selection, receive request and RDS enable.
    pub const RECEIVE_RDS: u8 = 0x19;
}

/// `STATUS1` masks.
///
/// AGC readiness from the datasheet. The walk gives an unready channel
/// bounded extra waits and records the final bit in its census. A ready bit
/// does not establish that the SNR estimate has converged; see the later
/// held-station measurements documented on [`SEEK_SETTLE_MS`].
pub mod status1 {
    /// AGC settled: 0 = not settled, 1 = settled. QN8035 Rev 0.08 p.22.
    pub const RX_AGC_SETTLED: u8 = 0x04;
}

/// `STATUS2` masks.
pub mod status2 {
    /// Per-group block error count.
    pub const BLOCK_ERROR: u8 = 0x0f;
    /// RDS decoder synchronized.
    pub const RDS_SYNC: u8 = 0x10;
    /// RDS data updated.
    pub const RDS_UPDATED: u8 = 0x80;
}

/// One deterministic register write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisterWrite {
    /// Register number.
    pub register: u8,
    /// Value to write.
    pub value: u8,
}

/// Frequency/model validation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelError {
    /// Frequency is outside 87.5-108.0 MHz.
    OutOfBand(u32),
    /// Frequency is not on the 100 kHz product grid.
    OffGrid(u32),
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfBand(khz) => write!(f, "{khz} kHz is outside {MIN_KHZ}..={MAX_KHZ}"),
            Self::OffGrid(khz) => write!(f, "{khz} kHz is not on the {STEP_KHZ} kHz grid"),
        }
    }
}

impl std::error::Error for ModelError {}

/// Convert a product-grid frequency to the chip's 50 kHz word from 60 MHz.
pub fn frequency_word(frequency_khz: u32) -> Result<u16, ModelError> {
    validate_frequency(frequency_khz)?;
    Ok(((frequency_khz - 60_000) / 50) as u16)
}

/// Convert a QN8035 channel word back to kHz.
pub const fn word_frequency(word: u16) -> u32 {
    60_000 + word as u32 * 50
}

/// Validate the AIDL tune contract.
pub fn validate_frequency(frequency_khz: u32) -> Result<(), ModelError> {
    if !(MIN_KHZ..=MAX_KHZ).contains(&frequency_khz) {
        return Err(ModelError::OutOfBand(frequency_khz));
    }
    if frequency_khz % STEP_KHZ != 0 {
        return Err(ModelError::OffGrid(frequency_khz));
    }
    Ok(())
}

/// Register writes used to tune, in wire order.
pub fn tune_sequence(frequency_khz: u32) -> Result<[RegisterWrite; 3], ModelError> {
    let word = frequency_word(frequency_khz)?;
    Ok([
        RegisterWrite {
            register: reg::CHANNEL,
            value: word as u8,
        },
        RegisterWrite {
            register: reg::CHANNEL_STEP,
            value: ((word >> 8) & 0x03) as u8,
        },
        RegisterWrite {
            register: reg::SYSTEM1,
            value: system1::RECEIVE_RDS,
        },
    ])
}

/// Direction of a band walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeekDirection {
    /// Increasing frequency, wrapping at the top.
    Up,
    /// Decreasing frequency, wrapping at the bottom.
    Down,
}

/// Return every candidate after `start`, in seek order, ending before `start`.
pub fn seek_channels(start: u32, direction: SeekDirection) -> Result<Vec<u32>, ModelError> {
    validate_frequency(start)?;
    let channel_count = ((MAX_KHZ - MIN_KHZ) / STEP_KHZ) as usize + 1;
    let mut channels = Vec::with_capacity(channel_count - 1);
    let mut cursor = start;
    for _ in 1..channel_count {
        cursor = match direction {
            SeekDirection::Up if cursor == MAX_KHZ => MIN_KHZ,
            SeekDirection::Up => cursor + STEP_KHZ,
            SeekDirection::Down if cursor == MIN_KHZ => MAX_KHZ,
            SeekDirection::Down => cursor - STEP_KHZ,
        };
        channels.push(cursor);
    }
    Ok(channels)
}

/// Outcome of one cooperative band-scan step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanOutcome {
    /// This channel did not clear the SNR floor; schedule another step.
    Continue,
    /// A station was found, and THE TUNER IS ON IT.
    ///
    /// The frequency is the walk's PEAK, not the last channel measured — the
    /// walk steps past a station to find out where its maximum was (see
    /// [`WalkPhase::Climbing`]), so it always overshoots. [`BandScan::step`]
    /// retunes onto the peak before returning this, exactly as the working LOS
    /// 23.2 walk does (`radio.go:602` `r.tuner.tune(found)`), so a caller
    /// cannot forget and leave the chip one channel off the station it just
    /// reported.
    Complete(u32),
    /// Every other product-grid channel was checked once.
    ///
    /// THE TUNER IS BACK ON THE CHANNEL THE SEEK STARTED FROM: a walk that
    /// gives up must not strand the chip wherever it stopped, or `getState()`
    /// reports the old station while the hardware sits on noise
    /// (`radio.go:596` *"Put the tuner back where it started rather than
    /// leaving it stranded wherever the walk gave up."*).
    Exhausted,
}

/// Which leg of the three-phase walk the scan is in.
///
/// Ported from the working LOS 23.2 seek (`radio.go:490-545`). D3-0117
/// implemented only the middle leg, which is why it could stop on the skirt of
/// the station it started on — see [`ClearingStart`] — and on the shoulder of
/// the one it found — see [`Climbing`].
///
/// [`ClearingStart`]: WalkPhase::ClearingStart
/// [`Climbing`]: WalkPhase::Climbing
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalkPhase {
    /// Stepping off the broadcast the seek started on.
    ///
    /// A channel at or above the floor here is the CURRENT station's skirt,
    /// not a find: measured on this hardware family, a strong carrier reads
    /// above the station threshold 0.1 and 0.2 MHz out
    /// (`RadioController.kt:384-393`, 90.2/90.4/90.5 around 90.3). Without
    /// this leg a seek from a strong station lands 100 kHz away, on itself.
    ClearingStart,
    /// Past the start station; looking for the first channel over the floor.
    Searching,
    /// On a candidate, following it while the SNR still rises.
    ///
    /// *"A station is a local maximum above the floor, not merely a loud
    /// frequency … Stopping on the first sample above the floor would land on
    /// a shoulder and sound off-station"* (`radio.go:517-521`).
    Climbing,
}

/// What one call to [`BandScan::step`] did, and how long to wait after it.
///
/// Tuning and scoring are separated by caller-owned waits. Searching may
/// reject weak channels after [`SEEK_SCREEN_MS`]; candidates receive the full
/// [`SEEK_SETTLE_MS`] before scoring. The caller releases its mutex throughout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanStep {
    /// This channel is now tuned and is settling.
    ///
    /// Contains (frequency in kHz, requested wait in milliseconds). The caller
    /// drops its lock and pays this wait before measuring the same frequency.
    Settling(u32, u64),
    /// The settled channel was measured and the walk decided.
    Decided(ScanOutcome),
}

/// A readable summary of one band walk.
///
/// This exists because a walk that found nothing and a walk that never ran
/// were indistinguishable from outside the daemon: both ended as
/// `on_seek_stopped(band_exhausted = true)` with nothing written anywhere. The
/// census turns "scanning does not work" into a number — and into WHICH
/// number, which is the part that says what to change:
///
/// | census                          | reading                                   |
/// |---------------------------------|-------------------------------------------|
/// | `measured=0`                    | the walk never stepped — worker or queue  |
/// | best SNR ~6 across the band     | no RF at all: aerial, rail, or mux        |
/// | best SNR 10-19, several peaks   | check settling against held-station SNR  |
/// | best SNR >= 20 and still no stop| a logic fault, not a threshold            |
/// | `settled` below `measured`      | [`SEEK_SETTLE_MS`] is too short           |
///
/// It is deliberately a summary, not a per-channel trace: the daemon's
/// kmsg budget is ten records per five seconds per fd, so a per-step line
/// would evict the summary it exists to deliver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanCensus {
    /// Channels scored after the full dwell, excluding early skips.
    pub measured: u32,
    /// Channels consumed stepping off the station the seek STARTED on.
    ///
    /// A direct measurement of the open shoulder question: every one of these
    /// is a channel adjacent to the starting broadcast that read at or above
    /// the floor. `cleared=0` means that station's skirt stayed under the
    /// floor here; `cleared>0` means it did not, and a walk without
    /// [`WalkPhase::ClearingStart`] would have "found" the station it was
    /// already on.
    pub cleared: u32,
    /// Channels walked while following a candidate upward.
    ///
    /// The other half of the same measurement. `climbed>0` means the first
    /// channel over the floor was NOT the peak — i.e. a walk without
    /// [`WalkPhase::Climbing`] would have landed on a shoulder, off-station.
    pub climbed: u32,
    /// Of those, how many reported `RXAGCSET` at read time.
    ///
    /// This counts the FINAL read after any extra waits. A low count shows
    /// AGC was not ready; equality does not prove SNR had converged. Compare
    /// held-station measurements before changing the floor.
    pub settled: u32,
    /// Extra settles the walk waited because `RXAGCSET` was still clear.
    ///
    /// Each one costs [`SEEK_SETTLE_MS`]. Zero means AGC was ready at each
    /// read, not that each channel's SNR estimate had finished converging.
    pub resettled: u32,
    /// Searching channels screened before the full scoring dwell.
    pub screened: u32,
    /// Screened channels rejected early; excluded from full-dwell scores.
    pub skipped: u32,
    /// Screened channels granted the rest of the full scoring dwell.
    pub promoted: u32,
    /// The floor that was in force for this walk.
    pub floor: u8,
    /// Highest SNR seen, with its channel.
    pub best: Option<(u32, u8)>,
    /// Second-highest SNR seen, with its channel.
    ///
    /// Present so a single hot channel can be told apart from a band that is
    /// uniformly warm — one peak well above a flat floor is a station under
    /// the threshold; a flat band is no signal.
    pub runner_up: Option<(u32, u8)>,
    /// Highest RSSI seen, with its channel.
    ///
    /// RSSI does not decide anything here (it is why the device-1 seek missed
    /// one time in eight), but a high RSSI with a floor-level SNR is the
    /// signature of a front end that hears noise, which SNR alone cannot say.
    pub peak_rssi: Option<(u32, u8)>,
}

impl Default for ScanCensus {
    fn default() -> Self {
        Self {
            measured: 0,
            cleared: 0,
            climbed: 0,
            settled: 0,
            resettled: 0,
            screened: 0,
            skipped: 0,
            promoted: 0,
            floor: SEEK_SNR_FLOOR,
            best: None,
            runner_up: None,
            peak_rssi: None,
        }
    }
}

impl ScanCensus {
    /// Fold one measured channel into the summary.
    pub fn record(&mut self, frequency_khz: u32, quality: SignalQuality, agc_settled: bool) {
        self.measured += 1;
        self.settled += u32::from(agc_settled);
        match self.best {
            Some((_, best_snr)) if quality.snr <= best_snr => {
                if self.runner_up.is_none_or(|(_, snr)| quality.snr > snr) {
                    self.runner_up = Some((frequency_khz, quality.snr));
                }
            }
            Some(previous_best) => {
                self.runner_up = Some(previous_best);
                self.best = Some((frequency_khz, quality.snr));
            }
            None => self.best = Some((frequency_khz, quality.snr)),
        }
        if self.peak_rssi.is_none_or(|(_, rssi)| quality.rssi > rssi) {
            self.peak_rssi = Some((frequency_khz, quality.rssi));
        }
    }
}

/// One line, fixed field order, greppable: this is the whole point of the type.
impl std::fmt::Display for ScanCensus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn pair(label: &str, value: Option<(u32, u8)>) -> String {
            match value {
                Some((khz, level)) => format!("{label}={khz}/{level}"),
                None => format!("{label}=none"),
            }
        }
        write!(
            f,
            "measured={} cleared={} climbed={} settled={} resettled={} screened={} skipped={} promoted={} floor={} settle_ms={} screen_ms={} screen_floor={} {} {} {}",
            self.measured,
            self.cleared,
            self.climbed,
            self.settled,
            self.resettled,
            self.screened,
            self.skipped,
            self.promoted,
            self.floor,
            SEEK_SETTLE_MS,
            SEEK_SCREEN_MS,
            SEEK_SCREEN_SNR_FLOOR,
            pair("best", self.best),
            pair("runner_up", self.runner_up),
            pair("peak_rssi", self.peak_rssi),
        )
    }
}

/// Cancellable seek state machine with caller-owned per-step waits.
///
/// The daemon worker owns this beside the sole [`Tuner`]. One [`step`] does
/// either the three-write tune or the two-read SNR/RSSI measurement through
/// [`I2cBus`] — never both — so a slow 205-channel walk never blocks a Binder
/// transaction, with a caller-owned dwell before each channel is judged.
///
/// [`step`]: BandScan::step
#[derive(Debug)]
pub struct BandScan {
    channels: Vec<u32>,
    start: u32,
    next: usize,
    /// The channel at `next` has been tuned and is settling.
    settling: bool,
    /// The next read is an early Searching-only screen, never a peak score.
    screening: bool,
    /// Extra settles already granted to the channel at `next`.
    resettles_here: u8,
    finished: bool,
    phase: WalkPhase,
    /// Best channel of the cluster currently being climbed.
    peak: Option<(u32, u8)>,
    /// Last adjacent channel tied with that maximum. Separate equal peaks
    /// retain the first peak; a dip or band wrap must not become a plateau.
    peak_end: u32,
    census: ScanCensus,
}

impl BandScan {
    /// Create a full wraparound walk that never rechecks `start`.
    pub fn new(start: u32, direction: SeekDirection) -> Result<Self, ModelError> {
        Ok(Self {
            channels: seek_channels(start, direction)?,
            start,
            next: 0,
            settling: false,
            screening: false,
            resettles_here: 0,
            finished: false,
            phase: WalkPhase::ClearingStart,
            peak: None,
            peak_end: start,
            census: ScanCensus::default(),
        })
    }

    /// What the walk has seen so far. Never touches hardware.
    pub fn census(&self) -> ScanCensus {
        self.census
    }

    /// Which leg the walk is on. Never touches hardware.
    pub fn phase(&self) -> WalkPhase {
        self.phase
    }

    /// The channel the seek started from, for a caller that must undo the walk.
    pub fn start(&self) -> u32 {
        self.start
    }

    /// Put the tuner back where the seek started.
    ///
    /// For the owner to call when it abandons a walk that [`step`] will never
    /// finish — a cancel, or an expired deadline. A walk that ends inside
    /// `step` restores itself; this is the same guarantee for the paths that
    /// do not.
    ///
    /// [`step`]: BandScan::step
    pub fn restore<B: I2cBus>(&self, tuner: &mut Tuner<B>) -> Result<(), TunerError<B::Error>> {
        tuner.tune(self.start)
    }

    /// Advance the walk: tune, screen, or fully score what was tuned.
    ///
    /// Returns [`ScanStep::Settling`] with the exact wait needed before the
    /// next read, or [`ScanStep::Decided`] after consuming the channel. A
    /// promoted screen requests only the remainder and does not re-tune.
    ///
    /// A failed I2C transaction does not consume the candidate OR its phase,
    /// so a retry re-issues exactly the operation that failed: the owner can
    /// report tuner loss or retry by policy without skipping a channel and
    /// without judging one it never tuned.
    pub fn step<B: I2cBus>(
        &mut self,
        tuner: &mut Tuner<B>,
    ) -> Result<ScanStep, TunerError<B::Error>> {
        if self.finished {
            return Ok(ScanStep::Decided(ScanOutcome::Exhausted));
        }
        let Some(&frequency) = self.channels.get(self.next) else {
            self.finished = true;
            return Ok(ScanStep::Decided(ScanOutcome::Exhausted));
        };
        if !self.settling {
            tuner.tune(frequency)?;
            self.settling = true;
            self.screening = self.phase == WalkPhase::Searching;
            let wait_ms = if self.screening { SEEK_SCREEN_MS } else { SEEK_SETTLE_MS };
            return Ok(ScanStep::Settling(frequency, wait_ms));
        }
        // Both reads are idempotent, so a failure in either simply repeats
        // them on the next call without re-tuning or consuming the candidate.
        let agc_settled = tuner.agc_settled()?;
        if self.screening {
            // Only a valid, low early SNR can skip. Do not use an early
            // station-like number for peak scoring or for clearing a skirt.
            // Complete every fallible read before consuming the screen, so
            // a bus retry stays on this frequency without re-tuning.
            let skip = agc_settled
                && tuner.signal_quality()?.snr < SEEK_SCREEN_SNR_FLOOR;
            let last_channel = self.next + 1 == self.channels.len();
            // A failed terminal restore must not consume this final channel.
            // The caller can retry or abort according to its bus-error policy.
            if skip && last_channel {
                tuner.tune(self.start)?;
            }
            self.screening = false;
            self.census.screened += 1;
            if skip {
                self.census.skipped += 1;
                self.settling = false;
                self.next += 1;
                if last_channel {
                    self.finished = true;
                    return Ok(ScanStep::Decided(ScanOutcome::Exhausted));
                }
                return Ok(ScanStep::Decided(ScanOutcome::Continue));
            }
            self.census.promoted += 1;
            return Ok(ScanStep::Settling(frequency, SEEK_SETTLE_MS - SEEK_SCREEN_MS));
        }
        if !agc_settled
            && self.resettles_here < SEEK_RESETTLE_PER_CHANNEL
            && self.census.resettled < SEEK_RESETTLE_PER_WALK
        {
            // The chip says this number is not ready. Do not judge a channel
            // on it: stay tuned here, ask the caller for one more settle, and
            // read again. Nothing is consumed -- `settling` stays true and
            // `next` does not move -- so the retry is the same channel.
            self.resettles_here += 1;
            self.census.resettled += 1;
            return Ok(ScanStep::Settling(frequency, SEEK_SETTLE_MS));
        }
        let quality = tuner.signal_quality()?;
        self.settling = false;
        self.resettles_here = 0;
        self.next += 1;
        self.census.record(frequency, quality, agc_settled);
        let over_floor = quality.station_present;
        let last_channel = self.next == self.channels.len();

        // The three legs, in device 1's order (`radio.go:490-545`). Only the
        // middle one existed before; the walk could therefore stop on the
        // skirt of the station it started from, and on the shoulder of the
        // one it found.
        match self.phase {
            WalkPhase::ClearingStart => {
                if over_floor {
                    // Still inside the starting broadcast's skirt.
                    self.census.cleared += 1;
                } else {
                    self.phase = WalkPhase::Searching;
                }
            }
            WalkPhase::Searching => {
                if over_floor {
                    self.peak = Some((frequency, quality.snr));
                    self.peak_end = frequency;
                    self.phase = WalkPhase::Climbing;
                }
            }
            WalkPhase::Climbing => {
                if over_floor {
                    self.census.climbed += 1;
                    if self.peak.is_none_or(|(_, snr)| quality.snr > snr) {
                        self.peak = Some((frequency, quality.snr));
                        self.peak_end = frequency;
                    } else if self.peak.is_some_and(|(_, snr)| quality.snr == snr)
                        && frequency.abs_diff(self.peak_end) == STEP_KHZ
                    {
                        self.peak_end = frequency;
                    }
                } else {
                    // The cluster has fallen away: its maximum was the station.
                    return self.land(tuner);
                }
            }
        }

        if !last_channel {
            return Ok(ScanStep::Decided(ScanOutcome::Continue));
        }
        // The band ran out. Mid-climb that is still a find: the peak so far is
        // the best this walk will ever know about.
        if self.phase == WalkPhase::Climbing {
            return self.land(tuner);
        }
        self.finished = true;
        // Do not stitch the tuner up wherever the walk stopped.
        tuner.tune(self.start)?;
        Ok(ScanStep::Decided(ScanOutcome::Exhausted))
    }

    /// Finish on the peak, leaving the tuner ON it.
    ///
    /// The walk always overshoots — it only knows the maximum once the signal
    /// falls away — so the chip is one or more channels past the station. The
    /// working LOS 23.2 seek retunes for exactly this reason
    /// (`radio.go:602`), and doing it here rather than in the caller is what
    /// makes [`ScanOutcome::Complete`]'s "the tuner is on it" a guarantee.
    fn land<B: I2cBus>(
        &mut self,
        tuner: &mut Tuner<B>,
    ) -> Result<ScanStep, TunerError<B::Error>> {
        let (first, _) = self.peak.expect("Climbing implies a peak");
        // Equal integer SNR maxima cannot identify the exact carrier. Use
        // the centre of one contiguous plateau instead of its approach-side
        // edge. For an even plateau the midpoint lies between grid channels;
        // choose the lower one consistently in either scan direction.
        let low = first.min(self.peak_end);
        let frequency = low + (first.abs_diff(self.peak_end) / STEP_KHZ / 2) * STEP_KHZ;
        tuner.tune(frequency)?;
        self.finished = true;
        Ok(ScanStep::Decided(ScanOutcome::Complete(frequency)))
    }
}

/// Whether a measured SNR is a station candidate.
pub const fn station_present(snr: u8) -> bool {
    snr >= SEEK_SNR_FLOOR
}

/// Raw signal registers and the shared station judgment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignalQuality {
    /// QN8035 SNR register.
    pub snr: u8,
    /// QN8035 RSSI register.
    pub rssi: u8,
    /// `snr >= SEEK_SNR_FLOOR`.
    pub station_present: bool,
}

/// One accepted RDS group-0 programme-service segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProgrammeServiceSegment {
    /// Programme Identification from block A.
    pub pi: u16,
    /// Segment number 0..3.
    pub segment: u8,
    /// Two display bytes from block D.
    pub characters: [u8; 2],
}

/// Parse one QN8035 RDS snapshot.
///
/// The group is rejected unless `RDSSYNC` is set and `blkerr` is zero. Group
/// 0A and 0B both carry programme-service characters in block D.
pub fn programme_service_segment(status: u8, blocks: [u8; 8]) -> Option<ProgrammeServiceSegment> {
    if status & status2::RDS_SYNC == 0 || status & status2::BLOCK_ERROR != 0 {
        return None;
    }
    let block_b = u16::from_be_bytes([blocks[2], blocks[3]]);
    let group_type = (block_b >> 12) & 0x0f;
    if group_type != 0 {
        return None;
    }
    let characters = [blocks[6], blocks[7]];
    if !characters
        .iter()
        .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
    {
        return None;
    }
    Some(ProgrammeServiceSegment {
        pi: u16::from_be_bytes([blocks[0], blocks[1]]),
        segment: (block_b & 0x03) as u8,
        characters,
    })
}

/// Complete, stable RDS state exposed to the service layer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RdsState {
    /// Latest Programme Identification, or zero before sync.
    pub programme_identification: u16,
    /// Empty until all four PS segments for one PI have arrived.
    pub programme_service: String,
    /// Whether RDS is currently synchronized.
    pub synchronized: bool,
}

/// Stateful four-segment assembler reset on every tune or loss of sync.
#[derive(Debug, Default)]
pub struct RdsAssembler {
    pi: Option<u16>,
    bytes: [u8; 8],
    segments: u8,
    last_published: Option<(u16, [u8; 8])>,
    state: RdsState,
}

impl RdsAssembler {
    /// Discard all state after a tune.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Current publishable state.
    pub fn state(&self) -> &RdsState {
        &self.state
    }

    /// Ingest one status/block snapshot; return a newly completed PS once.
    pub fn ingest(&mut self, status: u8, blocks: [u8; 8]) -> Option<RdsState> {
        if status & status2::RDS_SYNC == 0 {
            self.reset();
            return None;
        }
        self.state.synchronized = true;
        let segment = programme_service_segment(status, blocks)?;
        if self.pi != Some(segment.pi) {
            self.pi = Some(segment.pi);
            self.bytes = [b' '; 8];
            self.segments = 0;
            self.state.programme_identification = segment.pi;
            self.state.programme_service.clear();
        }
        let offset = segment.segment as usize * 2;
        self.bytes[offset..offset + 2].copy_from_slice(&segment.characters);
        self.segments |= 1 << segment.segment;
        if self.segments != 0x0f {
            return None;
        }
        let publication = (segment.pi, self.bytes);
        if self.last_published == Some(publication) {
            return None;
        }
        self.last_published = Some(publication);
        self.state.programme_service =
            String::from_utf8(self.bytes.to_vec()).expect("accepted RDS characters are ASCII");
        Some(self.state.clone())
    }
}

/// I2C-backed tuner primitive used below the HUD orchestration layer.
#[derive(Debug)]
pub struct Tuner<B> {
    bus: B,
    rds: RdsAssembler,
}

impl<B> Tuner<B> {
    /// Wrap an already resolved/opened `i2c-fm` adapter.
    pub fn new(bus: B) -> Self {
        Self {
            bus,
            rds: RdsAssembler::default(),
        }
    }

    /// Recover the bus for shutdown or tests.
    pub fn into_inner(self) -> B {
        self.bus
    }
}

/// Tuner transaction failure.
#[derive(Debug)]
pub enum TunerError<E> {
    /// Frequency/model validation failed before bus access.
    Model(ModelError),
    /// Adapter transaction failed.
    Bus(E),
    /// Register 0x06 did not identify a QN8035.
    WrongChipId(u8),
}

impl<E: std::fmt::Display> std::fmt::Display for TunerError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Model(error) => error.fmt(f),
            Self::Bus(error) => write!(f, "I2C transaction: {error}"),
            Self::WrongChipId(found) => {
                write!(f, "chip id 0x{found:02x}, expected 0x{CHIP_ID:02x}")
            }
        }
    }
}

impl<E: std::fmt::Debug + std::fmt::Display> std::error::Error for TunerError<E> {}

impl<B: I2cBus> Tuner<B> {
    /// Read one register using a combined transaction.
    pub fn read_register(&mut self, register: u8) -> Result<u8, TunerError<B::Error>> {
        let mut value = [0];
        self.bus
            .write_read(ADDRESS, &[register], &mut value)
            .map_err(TunerError::Bus)?;
        Ok(value[0])
    }

    /// Write one register.
    pub fn write_register(&mut self, register: u8, value: u8) -> Result<(), TunerError<B::Error>> {
        self.bus
            .write(ADDRESS, &[register, value])
            .map_err(TunerError::Bus)
    }

    /// Read and verify the live chip identity.
    pub fn verify_chip_id(&mut self) -> Result<u8, TunerError<B::Error>> {
        let found = self.read_register(reg::CHIP_ID)?;
        if found == CHIP_ID {
            Ok(found)
        } else {
            Err(TunerError::WrongChipId(found))
        }
    }

    /// Apply the pure tune sequence and reset RDS assembly.
    pub fn tune(&mut self, frequency_khz: u32) -> Result<(), TunerError<B::Error>> {
        let sequence = tune_sequence(frequency_khz).map_err(TunerError::Model)?;
        for write in sequence {
            self.write_register(write.register, write.value)?;
        }
        self.rds.reset();
        Ok(())
    }

    /// Whether the AGC reports itself settled on the current channel.
    ///
    /// One byte, used for bounded re-settles. This is not a guarantee that
    /// the SNR estimator has finished converging.
    pub fn agc_settled(&mut self) -> Result<bool, TunerError<B::Error>> {
        Ok(self.read_register(reg::STATUS1)? & status1::RX_AGC_SETTLED != 0)
    }

    /// Read SNR and RSSI and apply the single shared threshold.
    pub fn signal_quality(&mut self) -> Result<SignalQuality, TunerError<B::Error>> {
        let snr = self.read_register(reg::SNR)?;
        let rssi = self.read_register(reg::RSSI)?;
        Ok(SignalQuality {
            snr,
            rssi,
            station_present: station_present(snr),
        })
    }

    /// Read `STATUS2` plus blocks A-D and update the PS assembler.
    ///
    /// One `STATUS2` read per call; the eight block bytes are read **only**
    /// when the status says the group is usable (`RDSSYNC` set, `blkerr`
    /// zero). That is the working device-2 loop (`radio.go:404-415`): a
    /// rejected status costs one transaction, not two, and — unlike feeding
    /// an unsynced status straight into the assembler — it does NOT discard
    /// the segments already collected, so a momentary sync drop mid-name is
    /// a skipped poll rather than a restart.
    pub fn poll_rds(&mut self) -> Result<Option<RdsState>, TunerError<B::Error>> {
        let status = self.read_register(reg::STATUS2)?;
        if status & status2::RDS_SYNC == 0 || status & status2::BLOCK_ERROR != 0 {
            return Ok(None);
        }
        let mut blocks = [0; 8];
        self.bus
            .write_read(ADDRESS, &[reg::RDS_FIRST], &mut blocks)
            .map_err(TunerError::Bus)?;
        Ok(self.rds.ingest(status, blocks))
    }

    /// Latest stable RDS state; never accesses hardware.
    pub fn rds_state(&self) -> &RdsState {
        self.rds.state()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Eq, PartialEq)]
    enum Operation {
        Write(u16, Vec<u8>),
        WriteRead(u16, Vec<u8>, usize),
    }

    #[derive(Default)]
    struct MockI2c {
        operations: Vec<Operation>,
        reads: Vec<Vec<u8>>,
        error: Option<&'static str>,
    }

    impl I2cBus for MockI2c {
        type Error = &'static str;

        fn write(&mut self, address: u16, bytes: &[u8]) -> Result<(), Self::Error> {
            self.operations
                .push(Operation::Write(address, bytes.to_vec()));
            if let Some(error) = self.error {
                return Err(error);
            }
            Ok(())
        }

        fn write_read(
            &mut self,
            address: u16,
            write: &[u8],
            read: &mut [u8],
        ) -> Result<(), Self::Error> {
            self.operations
                .push(Operation::WriteRead(address, write.to_vec(), read.len()));
            if let Some(error) = self.error {
                return Err(error);
            }
            let response = self.reads.remove(0);
            read.copy_from_slice(&response);
            Ok(())
        }
    }

    #[test]
    fn tune_uses_the_trait_and_carries_the_address_on_every_write() {
        let mut tuner = Tuner::new(MockI2c::default());
        tuner.tune(97_500).unwrap();
        let bus = tuner.into_inner();
        assert_eq!(
            bus.operations,
            vec![
                Operation::Write(ADDRESS, vec![0x07, 0xee]),
                Operation::Write(ADDRESS, vec![0x0a, 0x02]),
                Operation::Write(ADDRESS, vec![0x00, 0x19]),
            ]
        );
    }

    #[test]
    fn signal_quality_uses_snr_not_rssi() {
        let bus = MockI2c {
            reads: vec![vec![6], vec![99]],
            ..MockI2c::default()
        };
        let mut tuner = Tuner::new(bus);
        let quality = tuner.signal_quality().unwrap();
        assert!(!quality.station_present);
        assert_eq!(quality.rssi, 99);
    }

    #[test]
    fn seek_walks_once_and_wraps() {
        let up = seek_channels(MAX_KHZ, SeekDirection::Up).unwrap();
        assert_eq!(up.first(), Some(&MIN_KHZ));
        assert_eq!(up.last(), Some(&(MAX_KHZ - STEP_KHZ)));
        assert_eq!(up.len(), 205);

        let down = seek_channels(MIN_KHZ, SeekDirection::Down).unwrap();
        assert_eq!(down.first(), Some(&MAX_KHZ));
        assert_eq!(down.last(), Some(&(MIN_KHZ + STEP_KHZ)));
    }

    /// Drive a whole walk against a scripted SNR-per-channel table.
    ///
    /// Every channel costs three reads (STATUS1, SNR, RSSI) and two calls, so
    /// hand-rolling these in each test buried the behaviour under bookkeeping.
    fn walk(start: u32, snrs: &[u8]) -> (ScanOutcome, ScanCensus, Vec<Operation>) {
        let mut reads = Vec::new();
        let mut phase = WalkPhase::ClearingStart;
        for snr in snrs {
            // Searching candidates have an early screen; retained candidates
            // then read the same static fixture after the remaining dwell.
            let samples = if phase == WalkPhase::Searching && *snr >= SEEK_SCREEN_SNR_FLOOR { 2 } else { 1 };
            for _ in 0..samples {
                reads.push(vec![status1::RX_AGC_SETTLED]);
                reads.push(vec![*snr]);
                reads.push(vec![80]);
            }
            if phase == WalkPhase::ClearingStart && *snr < SEEK_SNR_FLOOR {
                phase = WalkPhase::Searching;
            } else if phase == WalkPhase::Searching && *snr >= SEEK_SNR_FLOOR {
                phase = WalkPhase::Climbing;
            }
        }
        let mut tuner = Tuner::new(MockI2c {
            reads,
            ..MockI2c::default()
        });
        let mut scan = BandScan::new(start, SeekDirection::Up).unwrap();
        for _ in 0..(snrs.len() * 3) {
            match scan.step(&mut tuner).unwrap() {
                ScanStep::Settling(_, _) | ScanStep::Decided(ScanOutcome::Continue) => {}
                ScanStep::Decided(outcome) => {
                    let census = scan.census();
                    return (outcome, census, tuner.into_inner().operations);
                }
            }
        }
        panic!("the scripted walk neither landed nor exhausted");
    }

    #[test]
    fn band_scan_steps_one_channel_at_a_time_and_stops_on_snr() {
        let settled = vec![status1::RX_AGC_SETTLED];
        let bus = MockI2c {
            reads: vec![
                settled.clone(),
                vec![6],
                vec![99],
                settled.clone(),
                vec![31],
                vec![88],
                settled.clone(),
                vec![31],
                vec![88],
                settled,
                vec![6],
                vec![88],
            ],
            ..MockI2c::default()
        };
        let mut tuner = Tuner::new(bus);
        let mut scan = BandScan::new(97_500, SeekDirection::Up).unwrap();
        // 97.6 is empty: the walk is now clear of the starting station.
        assert_eq!(scan.step(&mut tuner).unwrap(), ScanStep::Settling(97_600, SEEK_SETTLE_MS));
        assert_eq!(
            scan.step(&mut tuner).unwrap(),
            ScanStep::Decided(ScanOutcome::Continue)
        );
        // 97.7 clears the floor -- but a candidate is not yet a landing.
        assert_eq!(scan.step(&mut tuner).unwrap(), ScanStep::Settling(97_700, SEEK_SCREEN_MS));
        assert_eq!(scan.step(&mut tuner).unwrap(), ScanStep::Settling(97_700, SEEK_SETTLE_MS - SEEK_SCREEN_MS));
        assert_eq!(
            scan.step(&mut tuner).unwrap(),
            ScanStep::Decided(ScanOutcome::Continue),
            "the first channel over the floor is a candidate, not a station"
        );
        // 97.8 falls away, so 97.7 was the peak.
        assert_eq!(scan.step(&mut tuner).unwrap(), ScanStep::Settling(97_800, SEEK_SETTLE_MS));
        assert_eq!(
            scan.step(&mut tuner).unwrap(),
            ScanStep::Decided(ScanOutcome::Complete(97_700))
        );
    }

    /// THE SHOULDER FIX, and the reason `ClearingStart` exists.
    ///
    /// A seek from a strong 90.3 whose +0.1 skirt also clears the floor must
    /// not "find" 90.4 — that is the station the listener is already on.
    /// Measured shape: `RadioController.kt:384-393`, 2026-08-24.
    #[test]
    fn a_seek_from_a_strong_station_does_not_land_on_its_own_skirt() {
        //            90.4  90.5 90.6 90.7
        let snrs = [26u8, 6, 22, 6];
        let (outcome, census, _) = walk(90_300, &snrs);
        assert_eq!(
            outcome,
            ScanOutcome::Complete(90_600),
            "90.4 is the starting station's shoulder, not the next broadcast"
        );
        assert_eq!(census.cleared, 1, "one channel of skirt was stepped over");
    }

    /// THE PEAK-FOLLOW FIX. 90.2/90.3/90.4 is one broadcast; the walk must
    /// settle on its maximum, not on whichever edge it met first.
    #[test]
    fn the_walk_settles_on_the_peak_of_a_cluster_not_its_first_edge() {
        //           90.1 90.2  90.3  90.4  90.5
        let snrs = [6u8, 24, 34, 26, 6];
        let (outcome, census, operations) = walk(90_000, &snrs);
        assert_eq!(
            outcome,
            ScanOutcome::Complete(90_300),
            "24 cleared the floor first, but 34 is the carrier"
        );
        assert_eq!(census.climbed, 2, "two channels were followed upward");

        // …and the chip is ON the peak, not past it.
        let last_tune = operations
            .iter()
            .rev()
            .find_map(|operation| match operation {
                Operation::Write(_, bytes) if bytes[0] == reg::CHANNEL => Some(bytes[1]),
                _ => None,
            })
            .expect("the walk must end with a tune");
        assert_eq!(
            last_tune,
            (frequency_word(90_300).unwrap() & 0xff) as u8,
            "ScanOutcome::Complete promises the tuner is on the reported channel"
        );
    }

    /// A walk that gives up must not strand the chip where it stopped.
    #[test]
    fn an_exhausted_walk_puts_the_tuner_back_where_the_seek_started() {
        let snrs = [6u8; 205];
        let (outcome, census, operations) = walk(90_000, &snrs);
        assert_eq!(outcome, ScanOutcome::Exhausted);
        assert_eq!(census.measured + census.skipped, 205);
        let last_tune = operations
            .iter()
            .rev()
            .find_map(|operation| match operation {
                Operation::Write(_, bytes) if bytes[0] == reg::CHANNEL => Some(bytes[1]),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            last_tune,
            (frequency_word(90_000).unwrap() & 0xff) as u8,
            "an exhausted walk leaves the tuner on the channel it started from"
        );
    }

    /// The same guarantee for the paths that never reach a terminal `step`.
    #[test]
    fn restore_returns_the_tuner_to_the_start_channel() {
        let mut tuner = Tuner::new(MockI2c::default());
        let scan = BandScan::new(95_700, SeekDirection::Down).unwrap();
        assert_eq!(scan.start(), 95_700);
        scan.restore(&mut tuner).unwrap();
        assert_eq!(
            tuner.into_inner().operations[0],
            Operation::Write(ADDRESS, vec![reg::CHANNEL, 0xca]),
            "95.7 MHz => word 714 = 0x02ca => low byte 0xca"
        );
    }

    /// The regression this whole change exists for.
    ///
    /// D3-0117's walk tuned and read SNR inside ONE call, so register 0x02 was
    /// sampled microseconds after the RXREQ write and reported the channel
    /// before. The structural proof that the settle is now the caller's is
    /// this: the step that tunes performs the three writes and NO read, and
    /// the reads only happen on a later call the caller is free to delay.
    #[test]
    fn the_tuning_step_never_reads_signal_quality_in_the_same_call() {
        let bus = MockI2c {
            reads: vec![vec![status1::RX_AGC_SETTLED], vec![32], vec![91]],
            ..MockI2c::default()
        };
        let mut tuner = Tuner::new(bus);
        let mut scan = BandScan::new(97_500, SeekDirection::Up).unwrap();

        assert_eq!(scan.step(&mut tuner).unwrap(), ScanStep::Settling(97_600, SEEK_SETTLE_MS));
        assert_eq!(
            tuner.bus.operations,
            [
                Operation::Write(ADDRESS, vec![0x07, 0xf0]),
                Operation::Write(ADDRESS, vec![0x0a, 0x02]),
                Operation::Write(ADDRESS, vec![0x00, 0x19]),
            ],
            "the settling step must tune and stop, leaving the wait to the caller"
        );

        scan.step(&mut tuner).unwrap();
        assert_eq!(
            &tuner.bus.operations[3..],
            [
                Operation::WriteRead(ADDRESS, vec![reg::STATUS1], 1),
                Operation::WriteRead(ADDRESS, vec![reg::SNR], 1),
                Operation::WriteRead(ADDRESS, vec![reg::RSSI], 1),
            ],
            "the measuring step must read the channel the previous call tuned"
        );
        assert_eq!(
            scan.census().settled,
            1,
            "the walk must record that the AGC said it had settled"
        );
    }

    /// The settle self-check: a channel read before `RXAGCSET` is counted.
    #[test]
    fn a_reading_taken_before_the_agc_settled_is_visible_in_the_census() {
        // A channel that NEVER settles: the walk grants it its two extra
        // settles, then reads it anyway and records the shortfall honestly.
        let bus = MockI2c {
            reads: vec![vec![0x00], vec![0x00], vec![0x00], vec![6], vec![51]],
            ..MockI2c::default()
        };
        let mut tuner = Tuner::new(bus);
        let mut scan = BandScan::new(97_500, SeekDirection::Up).unwrap();
        assert_eq!(scan.step(&mut tuner).unwrap(), ScanStep::Settling(97_600, SEEK_SETTLE_MS));
        assert_eq!(scan.step(&mut tuner).unwrap(), ScanStep::Settling(97_600, SEEK_SETTLE_MS), "1st re-settle");
        assert_eq!(scan.step(&mut tuner).unwrap(), ScanStep::Settling(97_600, SEEK_SETTLE_MS), "2nd re-settle");
        assert_eq!(
            scan.step(&mut tuner).unwrap(),
            ScanStep::Decided(ScanOutcome::Continue),
            "cap reached: read it as it is"
        );
        let census = scan.census();
        assert_eq!((census.measured, census.settled, census.resettled), (1, 0, 2));
        assert!(
            census
                .to_string()
                .contains("measured=1 cleared=0 climbed=0 settled=0 resettled=2"),
            "the shortfall must be readable in the one line: {census}"
        );
    }

    /// THE D3-0119 FAULT, seeks 12 and 13: `measured=1 settled=0` landings at
    /// 97.9 and 98.0 MHz -- the walk stopped on a number the chip had flagged
    /// as not yet valid. Here the unsettled first read says "station"; the
    /// settled second read says "air". Before this change the walk landed.
    #[test]
    fn an_unsettled_station_grade_reading_is_not_a_landing() {
        let bus = MockI2c {
            reads: vec![
                vec![0x00],                     // STATUS1: not settled
                vec![status1::RX_AGC_SETTLED],  // STATUS1 on the retry: settled
                vec![6],                        // SNR: empty air after all
                vec![51],
            ],
            ..MockI2c::default()
        };
        let mut tuner = Tuner::new(bus);
        let mut scan = BandScan::new(97_800, SeekDirection::Up).unwrap();
        assert_eq!(scan.step(&mut tuner).unwrap(), ScanStep::Settling(97_900, SEEK_SETTLE_MS));
        assert_eq!(
            scan.step(&mut tuner).unwrap(),
            ScanStep::Settling(97_900, SEEK_SETTLE_MS),
            "not settled: wait again instead of judging"
        );
        assert_eq!(
            scan.step(&mut tuner).unwrap(),
            ScanStep::Decided(ScanOutcome::Continue),
            "the settled read is air, so no landing"
        );
        let census = scan.census();
        assert_eq!((census.measured, census.settled, census.resettled), (1, 1, 1));
        // And the SNR register was read exactly once: the unsettled pass never
        // consumed a reading it was not going to trust.
        let snr_reads = tuner
            .bus
            .operations
            .iter()
            .filter(|op| matches!(op, Operation::WriteRead(_, w, _) if w[0] == reg::SNR))
            .count();
        assert_eq!(snr_reads, 1);
    }

    /// The mirror failure, which the census could not even see: an unsettled
    /// read that comes back LOW on a real station makes the walk skip it.
    /// Here 90.2 is the station; its first read is unsettled, and only the
    /// settled retry says 32. The walk must land on it.
    #[test]
    fn an_unsettled_low_reading_does_not_skip_a_station() {
        let bus = MockI2c {
            reads: vec![
                vec![status1::RX_AGC_SETTLED], vec![6],  vec![51], // 90.1: air, leaves ClearingStart
                vec![0x00],                                        // 90.2: NOT settled -> re-settle
                vec![status1::RX_AGC_SETTLED], vec![32], vec![91], // 90.2 retry: station
                vec![status1::RX_AGC_SETTLED], vec![6],  vec![51], // 90.3: air -> land on 90.2
            ],
            ..MockI2c::default()
        };
        let mut tuner = Tuner::new(bus);
        let mut scan = BandScan::new(90_000, SeekDirection::Up).unwrap();
        let mut outcome = None;
        for _ in 0..12 {
            if let ScanStep::Decided(ScanOutcome::Complete(khz)) = scan.step(&mut tuner).unwrap() {
                outcome = Some(khz);
                break;
            }
        }
        assert_eq!(outcome, Some(90_200), "the station an unsettled read would have skipped");
        let census = scan.census();
        assert_eq!(census.promoted, 1, "unready screen gets full dwell before AGC retries");
        assert_eq!((census.measured, census.settled, census.resettled), (3, 3, 0));
    }

    /// A board where nothing ever settles must degrade to the old behaviour
    /// after the per-walk cap, never hang and never burn the deadline alone.
    #[test]
    fn the_per_walk_resettle_cap_bounds_a_board_that_never_settles() {
        let channels = 30u32;
        let capped_after = SEEK_RESETTLE_PER_WALK / u32::from(SEEK_RESETTLE_PER_CHANNEL);
        let mut reads = Vec::new();
        for channel in 0..channels {
            // Before the per-walk cap a channel costs three STATUS1 reads (two
            // re-settles, then the final unsettled read); after it, exactly
            // one. Scripting the stream to match is what keeps every read
            // landing on the register it was meant for.
            if channel > 0 { reads.push(vec![0x00]); } // unready early screen
            let status_reads = if channel < capped_after { 3 } else { 1 };
            for _ in 0..status_reads {
                reads.push(vec![0x00]);
            }
            reads.push(vec![6]);
            reads.push(vec![51]);
        }
        let mut tuner = Tuner::new(MockI2c { reads, ..MockI2c::default() });
        let mut scan = BandScan::new(90_000, SeekDirection::Up).unwrap();
        let mut settling_steps = 0u32;
        let mut measured = 0u32;
        while measured < channels {
            match scan.step(&mut tuner).unwrap() {
                ScanStep::Settling(_, _) => settling_steps += 1,
                ScanStep::Decided(ScanOutcome::Continue) => measured += 1,
                other => panic!("unexpected {other:?}"),
            }
        }
        let census = scan.census();
        assert_eq!(
            census.resettled, SEEK_RESETTLE_PER_WALK,
            "re-settles stop at the per-walk cap"
        );
        // 30 channels x 1 mandatory settle + 20 re-settles = 50 settling steps.
        assert_eq!(settling_steps, channels + (channels - 1) + SEEK_RESETTLE_PER_WALK);
        assert_eq!(census.settled, 0, "none of them ever settled; the census says so");
    }

    #[test]
    fn nodev_or_nack_does_not_advance_the_band_scan() {
        for error in ["ENODEV", "EREMOTEIO: NACK"] {
            let bus = MockI2c {
                error: Some(error),
                ..MockI2c::default()
            };
            let mut tuner = Tuner::new(bus);
            let mut scan = BandScan::new(97_500, SeekDirection::Up).unwrap();
            for _ in 0..2 {
                assert!(scan
                    .step(&mut tuner)
                    .unwrap_err()
                    .to_string()
                    .contains(error));
            }
            let bus = tuner.into_inner();
            assert_eq!(
                bus.operations,
                [
                    Operation::Write(ADDRESS, vec![0x07, 0xf0]),
                    Operation::Write(ADDRESS, vec![0x07, 0xf0]),
                ]
            );
        }
    }

    /// A bus failure in the MEASURING phase must not silently re-tune.
    ///
    /// Re-tuning would restart the settle the caller has already paid for and,
    /// worse, would make a retry loop able to walk the band without ever
    /// reading it.
    #[test]
    fn a_failed_measurement_retries_the_read_and_never_re_tunes() {
        // Deliberately a BELOW-floor channel: a landing retunes onto the
        // peak, which would make "exactly one tune" the wrong assertion for
        // the wrong reason. This test is about the retry, not the landing.
        let bus = MockI2c {
            reads: vec![vec![status1::RX_AGC_SETTLED], vec![6], vec![51]],
            ..MockI2c::default()
        };
        let mut tuner = Tuner::new(bus);
        let mut scan = BandScan::new(97_500, SeekDirection::Up).unwrap();
        assert_eq!(scan.step(&mut tuner).unwrap(), ScanStep::Settling(97_600, SEEK_SETTLE_MS));

        tuner.bus.error = Some("EREMOTEIO: NACK");
        assert!(scan.step(&mut tuner).is_err());
        tuner.bus.error = None;

        assert_eq!(
            scan.step(&mut tuner).unwrap(),
            ScanStep::Decided(ScanOutcome::Continue)
        );
        let writes = tuner
            .bus
            .operations
            .iter()
            .filter(|operation| matches!(operation, Operation::Write(..)))
            .count();
        assert_eq!(writes, 3, "exactly one tune, however often the read fails");
    }

    /// The device-2 loop reads the eight RDS bytes only behind an accepting
    /// status. The port read them unconditionally — two transactions per poll
    /// on a bit-banged bus, and an unsynced status fed to the assembler threw
    /// away every segment collected so far.
    #[test]
    fn poll_rds_reads_the_blocks_only_when_status_accepts_the_group() {
        let group = [0x84, 0x12, 0x00, 0x00, 0, 0, b'T', b'E'];
        let bus = MockI2c {
            reads: vec![
                vec![0x00],                          // no sync
                vec![status2::RDS_SYNC | 0x01],      // synced, one block errored
                vec![status2::RDS_SYNC],             // accepted
                group.to_vec(),
            ],
            ..MockI2c::default()
        };
        let mut tuner = Tuner::new(bus);
        assert_eq!(tuner.poll_rds().unwrap(), None);
        assert_eq!(tuner.poll_rds().unwrap(), None);
        assert_eq!(tuner.poll_rds().unwrap(), None, "one segment is not a name");
        let block_reads = tuner
            .bus
            .operations
            .iter()
            .filter(|op| matches!(op, Operation::WriteRead(_, w, 8) if w[0] == reg::RDS_FIRST))
            .count();
        assert_eq!(block_reads, 1, "the blocks were read once, behind the one accepting status");
        assert_eq!(tuner.bus.operations.len(), 4, "three status reads + one block read");
    }

    /// A rejected poll must not restart the assembly: device 2 kept `have[]`
    /// across skipped polls, and a real station drops sync for a group now
    /// and then.
    #[test]
    fn a_rejected_poll_keeps_the_segments_already_collected() {
        let seg = |s: u8, a: u8, b: u8| vec![0x84, 0x12, 0x00, s, 0, 0, a, b];
        let bus = MockI2c {
            reads: vec![
                vec![status2::RDS_SYNC], seg(0, b'T', b'E'),
                vec![status2::RDS_SYNC], seg(1, b'S', b'T'),
                vec![0x00],                                  // sync drop: skipped
                vec![status2::RDS_SYNC], seg(2, b'F', b'M'),
                vec![status2::RDS_SYNC], seg(3, b' ', b' '),
            ],
            ..MockI2c::default()
        };
        let mut tuner = Tuner::new(bus);
        let mut published = None;
        for _ in 0..5 {
            if let Some(state) = tuner.poll_rds().unwrap() {
                published = Some(state.programme_service);
            }
        }
        assert_eq!(published.as_deref(), Some("TESTFM  "));
    }

    /// A tune resets the assembler, so segments from the station just left can
    /// never complete a name on the station just tuned. This is the
    /// hardware-layer half of "never publish old-generation PS into a new
    /// station"; the service layer adds a generation guard on top.
    #[test]
    fn a_tune_discards_the_previous_stations_partial_name() {
        let seg = |s: u8, a: u8, b: u8| vec![0x84, 0x12, 0x00, s, 0, 0, a, b];
        let bus = MockI2c {
            reads: vec![
                vec![status2::RDS_SYNC], seg(0, b'O', b'L'),
                vec![status2::RDS_SYNC], seg(1, b'D', b' '),
                vec![status2::RDS_SYNC], seg(2, b'P', b'S'),
                // -- tune happens here --
                vec![status2::RDS_SYNC], seg(3, b'!', b'!'),  // the 4th of the OLD name
                vec![status2::RDS_SYNC], seg(0, b'N', b'E'),
                vec![status2::RDS_SYNC], seg(1, b'W', b' '),
                vec![status2::RDS_SYNC], seg(2, b'P', b'S'),
            ],
            ..MockI2c::default()
        };
        let mut tuner = Tuner::new(bus);
        for _ in 0..3 {
            assert_eq!(tuner.poll_rds().unwrap(), None);
        }
        tuner.tune(97_700).unwrap();
        assert_eq!(
            tuner.poll_rds().unwrap(),
            None,
            "the fourth segment of the OLD station must not complete a name"
        );
        for _ in 0..2 {
            assert_eq!(tuner.poll_rds().unwrap(), None);
        }
        let done = tuner.poll_rds().unwrap();
        assert_eq!(
            done.map(|s| s.programme_service).as_deref(),
            Some("NEW PS!!"),
            "assembly restarted cleanly on the new station"
        );
    }

    #[test]
    fn rds_requires_sync_and_zero_block_errors() {
        let blocks = [0x84, 0x12, 0x00, 0x00, 0, 0, b'T', b'E'];
        assert!(programme_service_segment(0, blocks).is_none());
        assert!(programme_service_segment(status2::RDS_SYNC | 1, blocks).is_none());
        assert!(programme_service_segment(status2::RDS_SYNC, blocks).is_some());
    }

    #[test]
    fn rds_publishes_only_after_four_segments_for_one_pi() {
        let mut assembler = RdsAssembler::default();
        let pairs = [*b"TE", *b"ST", *b"FM", *b"  "];
        for (segment, pair) in pairs.into_iter().enumerate() {
            let blocks = [0x84, 0x12, 0x00, segment as u8, 0, 0, pair[0], pair[1]];
            let update = assembler.ingest(status2::RDS_SYNC, blocks);
            if segment < 3 {
                assert!(update.is_none());
                assert!(assembler.state().programme_service.is_empty());
            } else {
                assert_eq!(update.unwrap().programme_service, "TESTFM  ");
            }
        }
    }

    #[test]
    fn a_pi_change_discards_partial_programme_service() {
        let mut assembler = RdsAssembler::default();
        let first = [0x84, 0x12, 0, 0, 0, 0, b'O', b'L'];
        let next = [0x12, 0x34, 0, 1, 0, 0, b'E', b'W'];
        assembler.ingest(status2::RDS_SYNC, first);
        assembler.ingest(status2::RDS_SYNC, next);
        assert_eq!(assembler.state().programme_identification, 0x1234);
        assert!(assembler.state().programme_service.is_empty());
    }
}
