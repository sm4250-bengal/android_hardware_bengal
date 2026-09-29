// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Typed owner for the `xyauto_audio_guard` kernel ABI.
//!
//! `bengal-hud` opens the guard once and retains that descriptor for its
//! process lifetime.  The kernel deliberately allows only one opener: closing
//! it calls `all_safe()`, so opening a fresh descriptor for every Binder call
//! would turn harmless control traffic into an audible shutdown/restart edge.
//! This module consequently exposes only reviewed semantic operations and no
//! raw GPIO, I2C, DSP-address, coefficient, socket, or file-descriptor API.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::raw::{c_int, c_ulong};
use std::path::Path;

/// The miscdevice name supplied by `xyauto_audio_guard`.
pub const DEVICE_PATH: &str = "/dev/hu-audio-guard";
/// The only supported kernel uAPI revision.
pub const ABI_VERSION: u32 = 5;

/// Additive ABI5 transient ACC hold capability; never inferred from ABI alone.
#[cfg(any(test, feature = "acc-power-channel"))]
pub const FEATURE_ACC_HOLD: u32 = 1 << 1;
/// Fixed 32-byte keyed operation, shared with hu_audio_acc_hold.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg(any(test, feature = "acc-power-channel"))]
pub struct AccHold {
    /// Sole-open-FD daemon owner identity.
    pub owner: u64,
    /// Monotonic physical lease generation, retained across logical OFF transfer.
    pub generation: u64,
    /// HOLD=1, RELEASE=2, QUERY=3.
    pub operation: u32,
    /// Output: exact pair has a confirmed mute.
    pub held: u32,
    /// Output: latched fault; zero is required for readiness.
    pub error: i32,
    /// Input/output must be zero.
    pub reserved: u32,
}
/// Fixed _IOWR(0xbc, 0x0f, 32), no architecture-sized fields.
#[cfg(any(test, feature = "acc-power-channel"))]
pub const ACC_HOLD: c_ulong = ioc(IOC_READ | IOC_WRITE, 0x0f, 32);

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const IOC_MAGIC: u32 = 0xbc;

const fn ioc(direction: u32, number: u32, size: u32) -> c_ulong {
    ((direction << IOC_DIRSHIFT)
        | (IOC_MAGIC << IOC_TYPESHIFT)
        | (number << IOC_NRSHIFT)
        | (size << IOC_SIZESHIFT)) as c_ulong
}

/// Exact `struct hu_audio_guard_status` layout from the kernel uAPI.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawStatus {
    /// Kernel uAPI revision.
    pub abi_version: u32,
    /// DSP rail and control sequence completed.
    pub dsp_powered: u32,
    /// The guarded IC60 program was successfully verified.
    pub program_loaded: u32,
    /// FM rails are enabled.
    pub fm_powered: u32,
    /// External DAC is enabled.
    pub dac_enabled: u32,
    /// A source transition completed.
    pub route_ready: u32,
    /// Amplifier rail is enabled.
    pub amplifier_enabled: u32,
    /// `hu_audio_source` value.
    pub source: u32,
    /// A hardware failure is latched in the guard.
    pub fault_latched: u32,
    /// Last kernel-side operation error.
    pub last_error: i32,
    /// A successful PREPARE awaits a matching COMMIT.
    pub transition_pending: u32,
    /// Reserved kernel ABI space; must be retained exactly.
    pub reserved: [u32; 5],
}

/// Exact `struct hu_audio_source_transition` layout from the kernel uAPI.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceTransition {
    /// Destination source.
    pub source: u32,
    /// Reviewed OEM master-volume index, 0 through 61.
    pub master_volume_index: u32,
    /// Reviewed FM mix level, 0 through 100.
    pub fm_mix_level: u32,
    /// Two-phase source-transition operation.
    pub phase: u32,
}

/// Exact `struct hu_audio_dsp_update` layout from the kernel uAPI.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DspUpdate {
    /// `HU_AUDIO_DSP_MASTER_VOLUME` for this narrow service.
    pub operation: u32,
    /// `HU_AUDIO_DSP_CHANNEL_NONE` for a global master update.
    pub channel: u32,
    /// Zero for a master update.
    pub band: u32,
    /// Reviewed master index, 0 through 61.
    pub value: i32,
    /// Reserved kernel ABI space; always zero.
    pub reserved: [u32; 4],
}

/// Exact `struct hu_audio_fm_mix_transition` layout from the kernel uAPI.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FmMixTransition {
    /// Private nonzero Binder-owner generation.
    pub generation: u64,
    /// Prepare or commit; never a raw DSP operation.
    pub phase: u32,
    /// Must remain zero.
    pub reserved: u32,
}

/// Exact `struct hu_audio_fm_attenuation` layout from the kernel uAPI.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FmAttenuation {
    /// Exact generation returned by the current owner lease.
    pub generation: u64,
    /// 0..9600 mB; the upper endpoint is exact digital mute.
    pub attenuation_millibels: u32,
    /// Must remain zero.
    pub reserved: u32,
}

/// Exact `struct hu_audio_fm_mix_base` layout from the kernel uAPI.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FmMixBase {
    /// Exact generation returned by the current owner lease.
    pub generation: u64,
    /// OEM radio-mix level 0..100; 100 is the factory +14 dB power-up trim.
    pub level: u32,
    /// Must remain zero.
    pub reserved: u32,
}

const STATUS_SIZE: u32 = std::mem::size_of::<RawStatus>() as u32;
const TRANSITION_SIZE: u32 = std::mem::size_of::<SourceTransition>() as u32;
const UPDATE_SIZE: u32 = std::mem::size_of::<DspUpdate>() as u32;
const FM_MIX_TRANSITION_SIZE: u32 = std::mem::size_of::<FmMixTransition>() as u32;
const FM_ATTENUATION_SIZE: u32 = std::mem::size_of::<FmAttenuation>() as u32;
const FM_MIX_BASE_SIZE: u32 = std::mem::size_of::<FmMixBase>() as u32;

/// `HU_AUDIO_GUARD_GET_STATUS`.
pub const GET_STATUS: c_ulong = ioc(IOC_READ, 0x00, STATUS_SIZE);
/// `HU_AUDIO_GUARD_DSP_ON`.
pub const DSP_ON: c_ulong = ioc(0, 0x01, 0);
/// `HU_AUDIO_GUARD_DAC_ON`.
pub const DAC_ON: c_ulong = ioc(0, 0x05, 0);
/// `HU_AUDIO_GUARD_AMP_ON`.
pub const AMP_ON: c_ulong = ioc(0, 0x08, 0);
/// `HU_AUDIO_GUARD_FAULT_SAFE`.
pub const FAULT_SAFE: c_ulong = ioc(0, 0x0a, 0);
/// `HU_AUDIO_GUARD_FM_ON`.
pub const FM_ON: c_ulong = ioc(0, 0x03, 0);
/// `HU_AUDIO_GUARD_FM_OFF`.
pub const FM_OFF: c_ulong = ioc(0, 0x04, 0);
/// `HU_AUDIO_GUARD_SOURCE_TRANSITION`.
pub const SOURCE_TRANSITION: c_ulong = ioc(IOC_WRITE, 0x0b, TRANSITION_SIZE);
/// `HU_AUDIO_GUARD_DSP_UPDATE`.
pub const DSP_UPDATE: c_ulong = ioc(IOC_WRITE, 0x0c, UPDATE_SIZE);
/// `HU_AUDIO_GUARD_FM_MIX_TRANSITION`.
pub const FM_MIX_TRANSITION: c_ulong = ioc(IOC_WRITE, 0x0d, FM_MIX_TRANSITION_SIZE);
/// `HU_AUDIO_GUARD_FM_ATTENUATION`.
pub const FM_ATTENUATION: c_ulong = ioc(IOC_WRITE, 0x0e, FM_ATTENUATION_SIZE);
/// `HU_AUDIO_GUARD_FM_MIX_BASE` (ABI-5 additive, FEATURE_FM_MIX_BASE).
pub const FM_MIX_BASE: c_ulong = ioc(IOC_WRITE, 0x10, FM_MIX_BASE_SIZE);

const SOURCE_ANDROID: u32 = 1;
const SOURCE_FM: u32 = 2;
const SOURCE_FM_MIX: u32 = 3;
const PHASE_PREPARE: u32 = 1;
const PHASE_COMMIT: u32 = 2;
const PHASE_ATOMIC: u32 = 0;
const DSP_OPERATION_OUTPUT_MUTE: u32 = 1;
const DSP_OPERATION_OUTPUT_PHASE: u32 = 2;
const DSP_OPERATION_OUTPUT_DELAY: u32 = 3;
const DSP_OPERATION_OUTPUT_GAIN: u32 = 4;
const DSP_OPERATION_SM4250_EQ: u32 = 5;
const DSP_OPERATION_MASTER_VOLUME: u32 = 6;
const DSP_OPERATION_BASS_LEVEL: u32 = 8;
const DSP_OPERATION_TREBLE_LEVEL: u32 = 9;
const DSP_OPERATION_LOUDNESS: u32 = 10;
/// Additive ABI-5 bit in status.reserved[0]; zero on older guards.
pub const FEATURE_ENHANCEMENTS: u32 = 1;
/// Additive ABI-5 bit: the guard accepts `FM_MIX_BASE` for the mixed owner.
pub const FEATURE_FM_MIX_BASE: u32 = 1 << 2;
/// Shared output gain in status.reserved[1] as dB + 20.
pub const FEATURE_SHARED_GAIN: u32 = 1 << 3;
/// Kernel serializes physical mute + bounded RAM read + restore.
pub const FEATURE_MUTED_READ: u32 = 1 << 4;

#[repr(C)]
struct ParameterRead {
    start_word: u32,
    word_count: u32,
    data: [u8; 4096],
}
const PARAMETER_READ: c_ulong = ioc(IOC_READ | IOC_WRITE, 0x11, 4104);
const CHANNEL_NONE: u32 = 0;
const ANDROID_MASTER_INDEX: u32 = 61;
const ANDROID_FM_MIX_LEVEL: u32 = 0;
const MAX_MASTER_INDEX: u32 = 61;
const MAX_FM_MIX_LEVEL: u32 = 100;
const MAX_FM_ATTENUATION_MILLIBELS: u32 = 9600;

/// Highest accepted output delay, in uAPI REQUEST STEPS.
///
/// `xyauto_audio_guard.c:1009-1015` rejects `value < 0 || value > 145`. What a
/// step is worth physically is NOT proven on device 3 — the guard writes
/// `value + 1` into the delay cell, which is consistent with whole samples at
/// the pinned 48 kHz core rate, but that reading is a hypothesis and is
/// recorded as one in `controls::DELAY_STEP_MICROS`, which reports 0 = unknown.
pub const MAX_OUTPUT_DELAY_STEPS: i32 = 145;
/// Lowest accepted output gain step; index 0 of `output_gain_coeff[401]`.
pub const MIN_OUTPUT_GAIN_STEPS: i32 = -200;
/// Highest accepted output gain step; index 400 of `output_gain_coeff[401]`.
pub const MAX_OUTPUT_GAIN_STEPS: i32 = 200;
/// Number of SM4250 EQ bands in the shipped CpuType-1 program.
pub const EQ_BAND_COUNT: i32 = 4;
/// Lowest accepted SM4250 EQ uAPI request step. A step, not decibels.
pub const MIN_EQ_STEPS: i32 = -20;
/// Highest accepted SM4250 EQ uAPI request step. A step, not decibels.
pub const MAX_EQ_STEPS: i32 = 20;

/// The five logical outputs of the recovered IC60 program.
///
/// Mirrors `enum hu_audio_dsp_channel` (`xyauto_audio_guard.h:59-66`). The
/// discriminants are the kernel's channel numbers so no translation table can
/// drift; the guard maps each to its own internal slot index, which is the
/// order the program's cells are laid out in and NOT this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DspOutput {
    /// Guard output index 0.
    FrontLeft,
    /// Guard output index 3.
    FrontRight,
    /// Guard output index 1.
    RearLeft,
    /// Guard output index 4.
    RearRight,
    /// Guard output index 2. Has no delay cell.
    Subwoofer,
}

impl DspOutput {
    /// Every output, in kernel channel order. Snapshot rows use this order.
    pub const ALL: [Self; 5] = [
        Self::FrontLeft,
        Self::FrontRight,
        Self::RearLeft,
        Self::RearRight,
        Self::Subwoofer,
    ];

    /// The kernel `hu_audio_dsp_channel` value.
    pub fn raw(self) -> u32 {
        match self {
            Self::FrontLeft => 1,
            Self::FrontRight => 2,
            Self::RearLeft => 3,
            Self::RearRight => 4,
            Self::Subwoofer => 5,
        }
    }

    /// Parse an AIDL `DspOutput` value; `None` for anything else.
    pub fn from_raw(value: i32) -> Option<Self> {
        match value {
            1 => Some(Self::FrontLeft),
            2 => Some(Self::FrontRight),
            3 => Some(Self::RearLeft),
            4 => Some(Self::RearRight),
            5 => Some(Self::Subwoofer),
            _ => None,
        }
    }

    /// Whether the program has a delay cell for this output.
    ///
    /// The subwoofer does not: `output_index()` maps it to index 2 and
    /// `HU_AUDIO_DSP_OUTPUT_DELAY` rejects exactly that index. Callers refuse
    /// before the ioctl rather than relying on the kernel's `-EINVAL`.
    pub fn has_delay_cell(self) -> bool {
        !matches!(self, Self::Subwoofer)
    }

    /// Stable lowercase name for diagnostics and log tells.
    pub fn name(self) -> &'static str {
        match self {
            Self::FrontLeft => "front-left",
            Self::FrontRight => "front-right",
            Self::RearLeft => "rear-left",
            Self::RearRight => "rear-right",
            Self::Subwoofer => "subwoofer",
        }
    }
}

extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

/// The stable unavailable reason used by the Binder adapter and host tests.
pub const NOT_READY_ABSENT: &str = "NotReady/Phase-4c-audio-guard-device-absent";
/// The stable unavailable reason when the node exists but IC60 is not loaded.
pub const NOT_READY_PROGRAM: &str = "NotReady/Phase-4c-audio-guard-program-not-loaded";

/// A semantic source, never an analogue mux state or raw DSP address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Android I2S. The kernel requires unity master index 61 and FM mix 0.
    Android,
    /// QN8035 FM through the guarded ADC path.
    Fm,
}

impl Source {
    fn raw(self) -> u32 {
        match self {
            Self::Android => SOURCE_ANDROID,
            Self::Fm => SOURCE_FM,
        }
    }
}

/// A guarded source-transition phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// One guarded fallback that needs no pending PREPARE.
    Atomic,
    /// Program the destination under the still-muted output boundary.
    Prepare,
    /// Release exactly the prior successful preparation.
    Commit,
}

impl Phase {
    fn raw(self) -> u32 {
        match self {
            Self::Atomic => PHASE_ATOMIC,
            Self::Prepare => PHASE_PREPARE,
            Self::Commit => PHASE_COMMIT,
        }
    }
}

/// A complete availability report. `available=false` distinguishes an absent
/// miscdevice from a present guard whose program has not been loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// Whether a programmed, supported guard can accept a physical operation.
    pub available: bool,
    /// Stable NotReady/mismatch explanation when unavailable.
    pub unavailable_reason: String,
    /// Kernel status, if the node was opened and queried successfully.
    pub raw: Option<RawStatus>,
}

impl Status {
    fn absent(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            unavailable_reason: reason.into(),
            raw: None,
        }
    }

    fn from_raw(raw: RawStatus) -> Self {
        if raw.abi_version != ABI_VERSION {
            return Self {
                available: false,
                unavailable_reason: format!(
                    "NotReady/Phase-4c-audio-guard-abi-mismatch-found-{}-expected-{}",
                    raw.abi_version, ABI_VERSION
                ),
                raw: Some(raw),
            };
        }
        if raw.program_loaded == 0 {
            return Self {
                available: false,
                unavailable_reason: NOT_READY_PROGRAM.into(),
                raw: Some(raw),
            };
        }
        Self {
            available: true,
            unavailable_reason: String::new(),
            raw: Some(raw),
        }
    }
}

/// Errors are semantic and stable; raw errno is retained only as context.
#[derive(Debug)]
pub enum Error {
    /// The node did not exist or could not be opened at daemon start.
    DeviceAbsent(io::Error),
    /// A caller supplied an index outside the kernel's reviewed range.
    InvalidArgument(&'static str),
    /// The operation did not belong to the current mixed-playback lease.
    InvalidState(&'static str),
    /// Guard is absent, has a different ABI, or lacks a successfully loaded program.
    NotReady(String),
    /// One typed ioctl failed.
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeviceAbsent(error) => write!(f, "{NOT_READY_ABSENT}: {error}"),
            Self::InvalidArgument(message) => f.write_str(message),
            Self::InvalidState(message) => f.write_str(message),
            Self::NotReady(reason) => f.write_str(reason),
            Self::Io(error) => write!(f, "audio guard ioctl failed: {error}"),
        }
    }
}

impl std::error::Error for Error {}

/// Exact semantic response for a validated FM owner blocked by our ACC fence.
/// Not a successful/deferred write and not an arbitrary transport EBUSY.
pub const FM_ACC_SUSPENDED: &str = "FM gain suspended by ACC hold";

impl Error {
    /// Whether this refused gain write leaves the owner's playback request valid.
    /// In particular, an arbitrary ioctl EBUSY or an unknown state is fatal.
    pub fn preserves_fm_lease(&self) -> bool {
        matches!(self, Self::InvalidArgument(_))
            || matches!(self, Self::InvalidState(reason) if *reason == FM_ACC_SUSPENDED)
    }
}

/// Validate the public legacy-FM value pair without consulting or mutating
/// guard state. The Binder adapter uses this before beginning lease teardown;
/// [`AudioGuard::transition`] repeats the same shared check at the transport
/// boundary so internal callers cannot bypass it.
pub(crate) fn validate_fm_transition(master_index: i32, fm_mix_level: i32) -> Result<(), Error> {
    if !(0..=MAX_MASTER_INDEX as i32).contains(&master_index) {
        return Err(Error::InvalidArgument("master index must be 0..61"));
    }
    if !(0..=MAX_FM_MIX_LEVEL as i32).contains(&fm_mix_level) {
        return Err(Error::InvalidArgument("FM mix level must be 0..100"));
    }
    Ok(())
}

/// Narrow ioctl transport, supplied by a fake in host tests.
pub trait Transport {
    /// Read-only snapshot under the kernel's physical output mute.
    fn parameter_read(&mut self, _start: u16, _output: &mut [u8]) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(95))
    }
    /// Additive ACC operation; legacy fixtures/kernels explicitly lack support.
    #[cfg(any(test, feature = "acc-power-channel"))]
    fn acc_hold(&mut self, _request: AccHold) -> io::Result<AccHold> {
        Err(io::Error::from_raw_os_error(95))
    }
    /// Fetch the complete uAPI status structure.
    fn get_status(&mut self) -> io::Result<RawStatus>;
    /// Put every audio output into the kernel's guarded safe state.
    fn fault_safe(&mut self) -> io::Result<()>;
    /// Power the DSP and request/load the pinned guard firmware.
    fn dsp_on(&mut self) -> io::Result<()>;
    /// Enable the external DAC after a committed route.
    fn dac_power(&mut self, enabled: bool) -> io::Result<()>;
    /// Enable the amplifier after DAC and route are ready.
    fn amplifier_power(&mut self, enabled: bool) -> io::Result<()>;
    /// Send an already-validated source transition.
    fn source_transition(&mut self, request: SourceTransition) -> io::Result<()>;
    /// Issue one already-validated `HU_AUDIO_GUARD_DSP_UPDATE`.
    ///
    /// This is the single carrier for every semantic DSP control the kernel
    /// accepts. Callers must have validated the operation, channel, band and
    /// value first: the kernel's `-EINVAL` is a backstop, never the range
    /// check, because an out-of-range request that reached the driver has
    /// already told us the userspace model is wrong.
    fn dsp_update(&mut self, request: DspUpdate) -> io::Result<()>;
    /// Set a bounded master index.
    ///
    /// Provided, and deliberately not overridden by the production transport:
    /// master volume is one `DSP_UPDATE` like the others, and keeping it a
    /// distinct trait method only lets a test fake tell the two apart.
    fn master_volume(&mut self, request: DspUpdate) -> io::Result<()> {
        self.dsp_update(request)
    }
    /// Turn FM rail sequencing on or off.
    fn fm_power(&mut self, enabled: bool) -> io::Result<()>;
    /// Prepare or commit a generation-keyed mixed route.
    fn fm_mix_transition(&mut self, request: FmMixTransition) -> io::Result<()>;
    /// Write one semantic FM attenuation value for the current generation.
    fn fm_attenuation(&mut self, request: FmAttenuation) -> io::Result<()>;
    /// Select the owner's FM trim base (OEM radio-mix level) for the current generation.
    fn fm_mix_base(&mut self, request: FmMixBase) -> io::Result<()>;
}

/// Production kernel transport. The `File` is intentionally retained by the
/// enclosing [`AudioGuard`] for the daemon lifetime.
pub struct KernelTransport {
    file: File,
}

impl KernelTransport {
    /// Open one exclusive guard descriptor. The kernel handles electrical
    /// safety on close; callers must keep this object alive rather than reopen.
    pub fn open(path: &Path) -> io::Result<Self> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map(|file| Self { file })
    }

    fn request(&self, request: c_ulong, argument: *mut std::ffi::c_void) -> io::Result<()> {
        // SAFETY: `argument` is either null for _IO or points to the exact
        // repr(C) uAPI structure for the duration of this synchronous ioctl.
        let result = unsafe { ioctl(self.file.as_raw_fd(), request, argument) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl Transport for KernelTransport {
    fn parameter_read(&mut self, start: u16, output: &mut [u8]) -> io::Result<()> {
        if output.is_empty() || output.len() % 4 != 0 ||
            output.len() > 4096 || usize::from(start) + output.len() / 4 > 1024 {
            return Err(io::Error::from_raw_os_error(22));
        }
        let mut read = Box::new(ParameterRead {
            start_word: u32::from(start), word_count: (output.len() / 4) as u32, data: [0; 4096],
        });
        self.request(PARAMETER_READ, (&mut *read as *mut ParameterRead).cast())?;
        output.copy_from_slice(&read.data[..output.len()]);
        Ok(())
    }
    #[cfg(any(test, feature = "acc-power-channel"))]
    fn acc_hold(&mut self, mut request: AccHold) -> io::Result<AccHold> {
        self.request(ACC_HOLD, (&mut request as *mut AccHold).cast())?;
        Ok(request)
    }
    fn get_status(&mut self) -> io::Result<RawStatus> {
        let mut status = RawStatus::default();
        self.request(GET_STATUS, (&mut status as *mut RawStatus).cast())?;
        Ok(status)
    }

    fn fault_safe(&mut self) -> io::Result<()> {
        self.request(FAULT_SAFE, std::ptr::null_mut())
    }

    fn dsp_on(&mut self) -> io::Result<()> {
        self.request(DSP_ON, std::ptr::null_mut())
    }

    fn dac_power(&mut self, enabled: bool) -> io::Result<()> {
        self.request(
            if enabled { DAC_ON } else { ioc(0, 0x06, 0) },
            std::ptr::null_mut(),
        )
    }

    fn amplifier_power(&mut self, enabled: bool) -> io::Result<()> {
        self.request(
            if enabled { AMP_ON } else { ioc(0, 0x09, 0) },
            std::ptr::null_mut(),
        )
    }

    fn source_transition(&mut self, mut request: SourceTransition) -> io::Result<()> {
        self.request(
            SOURCE_TRANSITION,
            (&mut request as *mut SourceTransition).cast(),
        )
    }

    fn dsp_update(&mut self, mut request: DspUpdate) -> io::Result<()> {
        self.request(DSP_UPDATE, (&mut request as *mut DspUpdate).cast())
    }

    fn fm_power(&mut self, enabled: bool) -> io::Result<()> {
        self.request(if enabled { FM_ON } else { FM_OFF }, std::ptr::null_mut())
    }

    fn fm_mix_transition(&mut self, mut request: FmMixTransition) -> io::Result<()> {
        self.request(
            FM_MIX_TRANSITION,
            (&mut request as *mut FmMixTransition).cast(),
        )
    }

    fn fm_attenuation(&mut self, mut request: FmAttenuation) -> io::Result<()> {
        self.request(FM_ATTENUATION, (&mut request as *mut FmAttenuation).cast())
    }

    fn fm_mix_base(&mut self, mut request: FmMixBase) -> io::Result<()> {
        self.request(FM_MIX_BASE, (&mut request as *mut FmMixBase).cast())
    }
}

/// The sole daemon owner of the audio guard. `None` represents an absent node
/// and still provides a registered Binder service that fails inertly.
pub struct AudioGuard<T = KernelTransport> {
    #[cfg(any(test, feature = "acc-power-channel"))]
    acc_lease: Option<u64>,
    #[cfg(any(test, feature = "acc-power-channel"))]
    acc_fm_cleanup_pending: bool,
    #[cfg(any(test, feature = "acc-power-channel"))]
    acc_validity: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    transport: Option<T>,
    absent_reason: String,
    activation_failure: Option<String>,
    fm_mix_generation: Option<u64>,
    /// Successful [`AudioGuard::ensure_ready`] runs. Each one reloads the IC60
    /// program, so every control value written before it is gone from the chip.
    program_epoch: u64,
    // Installed once before activation. false invalidates applied claims;
    // true restores with DAC/amp off. Never takes the DSP transaction/guard lock.
    program_settings: Option<ProgramSettings<T>>,
}

pub(crate) type ProgramSettings<T> = std::sync::Arc<
    dyn Fn(&mut AudioGuard<T>, bool) -> Result<(), Error> + Send + Sync,
>;

impl AudioGuard<KernelTransport> {
    /// Open the guard once as `bengal-hud` starts. Failure is recorded rather
    /// than retried from Binder calls, preserving both single-owner semantics
    /// and an electrically inert unavailable service.
    pub fn open_default() -> Self {
        Self::open_path(Path::new(DEVICE_PATH))
    }

    /// Open one explicit path; public for a tightly scoped daemon integration.
    pub fn open_path(path: &Path) -> Self {
        match KernelTransport::open(path) {
            Ok(transport) => Self::with_transport(transport),
            Err(error) => Self {
                #[cfg(any(test, feature = "acc-power-channel"))]
                acc_validity: None,
                #[cfg(any(test, feature = "acc-power-channel"))]
                acc_lease: None,
                #[cfg(any(test, feature = "acc-power-channel"))]
                acc_fm_cleanup_pending: false,
                transport: None,
                absent_reason: format!("{NOT_READY_ABSENT}: {error}"),
                activation_failure: None,
                fm_mix_generation: None,
                program_epoch: 0,
                program_settings: None,
            },
        }
    }
}

impl<T: Transport> AudioGuard<T> {
    /// Construct a present test/integration owner around one transport.
    pub fn with_transport(transport: T) -> Self {
        Self {
            transport: Some(transport),
            #[cfg(any(test, feature = "acc-power-channel"))]
            acc_validity: None,
            #[cfg(any(test, feature = "acc-power-channel"))]
            acc_lease: None,
            #[cfg(any(test, feature = "acc-power-channel"))]
            acc_fm_cleanup_pending: false,
            absent_reason: NOT_READY_ABSENT.into(),
            activation_failure: None,
            fm_mix_generation: None,
            program_epoch: 0,
            program_settings: None,
        }
    }

    /// Construct an unavailable owner without opening anything.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            transport: None,
            #[cfg(any(test, feature = "acc-power-channel"))]
            acc_validity: None,
            #[cfg(any(test, feature = "acc-power-channel"))]
            acc_lease: None,
            #[cfg(any(test, feature = "acc-power-channel"))]
            acc_fm_cleanup_pending: false,
            absent_reason: reason.into(),
            activation_failure: None,
            fm_mix_generation: None,
            program_epoch: 0,
            program_settings: None,
        }
    }

    /// Read status. This is the only operation legal when the node is absent.
    pub fn status(&mut self) -> Status {
        let Some(transport) = self.transport.as_mut() else {
            return Status::absent(self.absent_reason.clone());
        };
        match transport.get_status() {
            Ok(raw) => {
                let mut status = Status::from_raw(raw);
                if let Some(reason) = &self.activation_failure {
                    status.available = false;
                    status.unavailable_reason = reason.clone();
                }
                status
            }
            Err(error) => Status::absent(format!("{NOT_READY_ABSENT}: {error}")),
        }
    }

    fn ready(&mut self) -> Result<RawStatus, Error> {
        #[cfg(any(test, feature = "acc-power-channel"))]
        if self.acc_lease.is_some() {
            return Err(Error::Io(io::Error::from_raw_os_error(16)));
        }
        let Some(transport) = self.transport.as_mut() else {
            return Err(Error::NotReady(self.absent_reason.clone()));
        };
        let raw = transport.get_status().map_err(Error::Io)?;
        let status = Status::from_raw(raw);
        if status.available {
            Ok(raw)
        } else {
            Err(Error::NotReady(status.unavailable_reason))
        }
    }

    fn transport(&mut self) -> Result<&mut T, Error> {
        self.transport
            .as_mut()
            .ok_or_else(|| Error::NotReady(self.absent_reason.clone()))
    }

    /// Perform the one reviewed physical bring-up, internally to `bengal-hud`.
    ///
    /// Firmware presence is intentionally not treated as program readiness:
    /// the kernel only requests and verifies the approved IC60 image while
    /// handling `DSP_ON`.  This method is called asynchronously after Binder
    /// registration, and may be called again after an ACC-on/ADSP park.  No
    /// Binder method exposes these raw power operations.
    #[cfg_attr(not(feature = "binder"), allow(dead_code))]
    pub(crate) fn ensure_ready(&mut self) -> Result<(), Error> {
        #[cfg(any(test, feature = "acc-power-channel"))]
        self.acc_revoke();
        let result = self.settings_boundary(false).and_then(|()| self.ensure_ready_inner());
        match &result {
            Ok(()) => {
                self.activation_failure = None;
                self.program_epoch = self.program_epoch.wrapping_add(1);
            }
            Err(error) => {
                let _ = self.settings_boundary(false);
                // A failed bring-up is never allowed to leave an intermediate
                // rail state. Preserve the original errno for the Binder caller
                // while making the best-effort safe operation invisible to it.
                if let Some(transport) = self.transport.as_mut() {
                    let _ = transport.fault_safe();
                }
                self.activation_failure = Some(format!(
                    "NotReady/Phase-4c-audio-guard-activation-failed: {error}"
                ));
            }
        }
        result
    }

    pub(crate) fn install_program_settings(&mut self, settings: ProgramSettings<T>) {
        self.program_settings = Some(settings);
    }

    fn settings_boundary(&mut self, restore: bool) -> Result<(), Error> {
        match self.program_settings.clone() {
            Some(settings) => settings(self, restore),
            None => Ok(()),
        }
    }

    /// How many times [`Self::ensure_ready`] has reloaded the IC60 program.
    ///
    /// A change means every semantic control written before it is no longer
    /// on the chip: the program's own defaults are. Pure bookkeeping, no I/O.
    pub fn program_epoch(&self) -> u64 {
        self.program_epoch
    }

    /// End a failed or uncertain ACC hold and re-run the boot activation.
    ///
    /// # Why (car test 2026-09-10; D3-0137/0138/0139 ACC captures)
    ///
    /// A retained-sleep ACC cycle ends with a keyed kernel RELEASE. On every
    /// measured device-3 cycle that RELEASE failed: after resume the guard read
    /// the shared master back nonzero (`ACC hold lost DSP mute:
    /// master=057cba3c`, D3-0139 `logs/acc-vulkan-user-1/dmesg.txt:2422`),
    /// latched a fault and returned EIO. From then on the kernel refuses
    /// everything except ACC_HOLD/GET_STATUS/FAULT_SAFE while its hold stays
    /// active (`hold_blocks`), and this daemon keeps `acc_lease`, so every FM
    /// route change and every DSP control is refused until the descriptor is
    /// closed -- a reboot or a daemon restart. The tuner keeps answering on
    /// its own bus, which is why the radio looks alive and plays nothing.
    ///
    /// Recovery is exactly what a daemon restart does: FAULT_SAFE (the
    /// kernel's terminal safe state, which also ends the hold and clears the
    /// latched fault), then [`Self::ensure_ready`], which power-cycles the DSP,
    /// reloads and verifies the IC60 program and commits the Android route
    /// behind the mute. The OEM stack runs the same power-init and program
    /// download on every ACC-on (device-2 stock capture, family prior art).
    ///
    /// If FAULT_SAFE itself fails the physical state is still unknown, so the
    /// lease and its writer fence are kept and that error is returned.
    #[cfg(any(test, feature = "acc-power-channel"))]
    pub(crate) fn recover_after_acc_fault(&mut self) -> Result<(), Error> {
        self.acc_revoke();
        self.transport()?.fault_safe().map_err(Error::Io)?;
        self.acc_lease = None;
        self.acc_fm_cleanup_pending = false;
        self.fm_mix_generation = None;
        self.ensure_ready()
    }

    fn ensure_ready_inner(&mut self) -> Result<(), Error> {
        self.fm_mix_generation = None;
        let transport = self.transport()?;
        // This ordering is the kernel state-machine contract. In particular,
        // DSP_ON performs request_firmware/load_program; the file merely being
        // installed cannot set program_loaded.
        transport.fault_safe().map_err(Error::Io)?;
        transport.dsp_on().map_err(Error::Io)?;
        let raw = transport.get_status().map_err(Error::Io)?;
        let status = Status::from_raw(raw);
        if !status.available {
            return Err(Error::NotReady(status.unavailable_reason));
        }
        // DSP_ON loaded IC60; FAULT_SAFE still keeps DAC/amp off. Replay only
        // reviewed semantic controls before committing an audible route.
        self.settings_boundary(true)?;
        let transport = self.transport()?;
        transport
            .source_transition(SourceTransition {
                source: SOURCE_ANDROID,
                master_volume_index: ANDROID_MASTER_INDEX,
                fm_mix_level: ANDROID_FM_MIX_LEVEL,
                phase: PHASE_PREPARE,
            })
            .map_err(Error::Io)?;
        transport
            .source_transition(SourceTransition {
                source: SOURCE_ANDROID,
                master_volume_index: ANDROID_MASTER_INDEX,
                fm_mix_level: ANDROID_FM_MIX_LEVEL,
                phase: PHASE_COMMIT,
            })
            .map_err(Error::Io)?;
        transport.dac_power(true).map_err(Error::Io)?;
        transport.amplifier_power(true).map_err(Error::Io)
    }

    /// Kernel-muted RAM read. Never falls back to an unmuted raw adapter.
    pub fn read_parameter_words(&mut self, start: u16, output: &mut [u8]) -> Result<(), Error> {
        let raw = self.transport()?.get_status().map_err(Error::Io)?;
        if raw.abi_version != ABI_VERSION || raw.reserved[0] & FEATURE_MUTED_READ == 0 {
            return Err(Error::Io(io::Error::from_raw_os_error(95)));
        }
        self.transport()?.parameter_read(start, output).map_err(Error::Io)
    }

    /// Configured calibration, not an independent parameter-RAM readback.
    pub fn shared_output_gain_db(&mut self) -> Result<i32, Error> {
        let raw = self.transport()?.get_status().map_err(Error::Io)?;
        if raw.abi_version != ABI_VERSION || raw.reserved[0] & FEATURE_SHARED_GAIN == 0 {
            return Err(Error::Io(io::Error::from_raw_os_error(95)));
        }
        if raw.reserved[1] > 20 {
            return Err(Error::InvalidState("invalid shared output gain status"));
        }
        Ok(raw.reserved[1] as i32 - 20)
    }

    /// Dedicated calibration operation; the legacy media-volume no-op stays intact.
    pub fn set_shared_output_gain_db(&mut self, db: i32) -> Result<(), Error> {
        if !(-20..=0).contains(&db) {
            return Err(Error::InvalidArgument("shared output gain must be -20..0 dB"));
        }
        self.shared_output_gain_db()?; // explicit kernel feature, not AIDL-version inference
        self.ready()?;
        self.transport()?.dsp_update(DspUpdate {
            operation: 11, channel: CHANNEL_NONE, band: 0, value: db, reserved: [0; 4],
        }).map_err(Error::Io)
    }

    /// Set only the reviewed global ADAU master index, 0 through 61.
    pub fn set_master_index(&mut self, index: i32) -> Result<(), Error> {
        if !(0..=MAX_MASTER_INDEX as i32).contains(&index) {
            return Err(Error::InvalidArgument("master index must be 0..61"));
        }
        self.ready()?;
        self.transport()?
            .master_volume(DspUpdate {
                operation: DSP_OPERATION_MASTER_VOLUME,
                channel: CHANNEL_NONE,
                band: 0,
                value: index,
                reserved: [0; 4],
            })
            .map_err(Error::Io)
    }

    /// Mute or unmute one logical output. One ioctl, one semantic change.
    pub fn set_output_mute(&mut self, output: DspOutput, muted: bool) -> Result<(), Error> {
        self.output_update(
            DSP_OPERATION_OUTPUT_MUTE,
            output,
            i32::from(muted),
        )
    }

    /// Invert or restore one logical output's polarity.
    pub fn set_output_phase_inverted(
        &mut self,
        output: DspOutput,
        inverted: bool,
    ) -> Result<(), Error> {
        self.output_update(
            DSP_OPERATION_OUTPUT_PHASE,
            output,
            i32::from(inverted),
        )
    }

    /// Set one logical output's delay, 0..=145 uAPI request steps.
    ///
    /// Refused for the subwoofer before any ioctl is issued: that output has
    /// no delay cell at all, and letting the kernel answer `-EINVAL` would
    /// make an unsupported control indistinguishable from a bad value.
    pub fn set_output_delay_steps(
        &mut self,
        output: DspOutput,
        steps: i32,
    ) -> Result<(), Error> {
        if !output.has_delay_cell() {
            return Err(Error::InvalidArgument(
                "the subwoofer output has no delay cell",
            ));
        }
        if !(0..=MAX_OUTPUT_DELAY_STEPS).contains(&steps) {
            return Err(Error::InvalidArgument("output delay must be 0..145 steps"));
        }
        self.output_update(DSP_OPERATION_OUTPUT_DELAY, output, steps)
    }

    /// Set one logical output's gain, -200..=200 steps of 0.1 dB.
    pub fn set_output_gain_steps(
        &mut self,
        output: DspOutput,
        steps: i32,
    ) -> Result<(), Error> {
        if !(MIN_OUTPUT_GAIN_STEPS..=MAX_OUTPUT_GAIN_STEPS).contains(&steps) {
            return Err(Error::InvalidArgument(
                "output gain must be -200..200 steps",
            ));
        }
        self.output_update(DSP_OPERATION_OUTPUT_GAIN, output, steps)
    }

    /// Set one SM4250 EQ band's uAPI request step, -20..=20 (steps, not dB).
    ///
    /// The band is a global filter, so the request carries channel NONE — the
    /// kernel rejects anything else for this operation.
    pub fn set_eq_band_steps(&mut self, band: i32, steps: i32) -> Result<(), Error> {
        if !(0..EQ_BAND_COUNT).contains(&band) {
            return Err(Error::InvalidArgument("EQ band must be 0..3"));
        }
        if !(MIN_EQ_STEPS..=MAX_EQ_STEPS).contains(&steps) {
            return Err(Error::InvalidArgument("EQ steps must be -20..20"));
        }
        self.ready()?;
        self.transport()?
            .dsp_update(DspUpdate {
                operation: DSP_OPERATION_SM4250_EQ,
                channel: CHANNEL_NONE,
                band: band as u32,
                value: steps,
                reserved: [0; 4],
            })
            .map_err(Error::Io)
    }

    /// Fixed 100 Hz low shelf, OEM request levels (not decibels).
    pub fn set_bass_level(&mut self, level: i32) -> Result<(), Error> {
        if !(-32..=32).contains(&level) {
            return Err(Error::InvalidArgument("bass level must be -32..32"));
        }
        self.enhancement_update(DSP_OPERATION_BASS_LEVEL, level)
    }

    /// Fixed 5000 Hz high shelf, OEM request levels (not decibels).
    pub fn set_treble_level(&mut self, level: i32) -> Result<(), Error> {
        if !(-32..=32).contains(&level) {
            return Err(Error::InvalidArgument("treble level must be -32..32"));
        }
        self.enhancement_update(DSP_OPERATION_TREBLE_LEVEL, level)
    }

    /// None restores the IC60 program default; Some is the normalized 0..63 basis.
    pub fn set_loudness(&mut self, volume_index: Option<i32>) -> Result<(), Error> {
        if volume_index.is_some_and(|index| !(0..=63).contains(&index)) {
            return Err(Error::InvalidArgument("volume index must be 0..63"));
        }
        self.enhancement_update(DSP_OPERATION_LOUDNESS, volume_index.unwrap_or(-1))
    }

    fn enhancement_update(&mut self, operation: u32, value: i32) -> Result<(), Error> {
        let raw = self.ready()?;
        if raw.reserved[0] & FEATURE_ENHANCEMENTS == 0 {
            return Err(Error::InvalidState("guard does not support enhancements"));
        }
        self.transport()?.dsp_update(DspUpdate {
            operation,
            channel: CHANNEL_NONE,
            band: 0,
            value,
            reserved: [0; 4],
        }).map_err(Error::Io)
    }

    /// Shared tail for the four per-output controls: check readiness, then
    /// issue exactly one `DSP_UPDATE`. Ranges are checked by the callers, so
    /// this never repairs or clamps a value it was handed.
    fn output_update(
        &mut self,
        operation: u32,
        output: DspOutput,
        value: i32,
    ) -> Result<(), Error> {
        self.ready()?;
        self.transport()?
            .dsp_update(DspUpdate {
                operation,
                channel: output.raw(),
                band: 0,
                value,
                reserved: [0; 4],
            })
            .map_err(Error::Io)
    }

    /// Power or depower FM rails through the guard, never direct GPIO.
    pub fn set_fm_power(&mut self, enabled: bool) -> Result<(), Error> {
        self.ready()?;
        let result = self.transport()?.fm_power(enabled).map_err(Error::Io);
        if !enabled {
            self.fm_mix_generation = None;
        }
        result
    }

    /// Force the complete kernel-owned audio path safe after a failed
    /// userspace recovery operation. This remains private to the HUD daemon;
    /// no Binder method exposes raw guard power controls.
    #[cfg_attr(not(feature = "binder"), allow(dead_code))]
    pub(crate) fn force_fault_safe(&mut self) -> Result<(), Error> {
        #[cfg(any(test, feature = "acc-power-channel"))]
        self.acc_revoke();
        self.fm_mix_generation = None;
        self.transport()?.fault_safe().map_err(Error::Io)
    }

    /// Share revocation with the native authorization gate; no new device owner.
    #[cfg(any(test, feature = "acc-power-channel"))]
    pub(crate) fn attach_acc_validity(&mut self, valid: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.acc_validity = Some(valid);
        self.acc_revoke();
    }

    #[cfg(any(test, feature = "acc-power-channel"))]
    fn acc_revoke(&self) {
        if let Some(valid) = &self.acc_validity {
            valid.store(false, std::sync::atomic::Ordering::Release);
        }
    }

    /// An issued hold, including uncertain acquisition, fences normal writers.
    #[cfg(any(test, feature = "acc-power-channel"))]
    pub(crate) fn acc_held(&self) -> bool { self.acc_lease.is_some() }

    /// Retire a stopped/dead FM client's route after the ACC physical hold.
    #[cfg(any(test, feature = "acc-power-channel"))]
    pub(crate) fn acc_defer_fm_cleanup(&mut self) { self.acc_fm_cleanup_pending = true; }

    /// The fixed physical owner is this sole FD's daemon (1); `generation` is
    /// an increasing local lease token, distinct from the MCU cycle generation.
    #[cfg(any(test, feature = "acc-power-channel"))]
    pub(crate) fn acc_operation(&mut self, generation: u64, operation: u32) -> Result<(), Error> {
        if self.acc_lease.is_some_and(|held| held != generation) {
            return Err(Error::InvalidState("ACC physical lease is not current"));
        }
        self.acc_revoke();
        if generation == 0 || !(1..=3).contains(&operation) {
            return Err(Error::InvalidArgument("invalid ACC hold operation"));
        }
        let raw = self.transport()?.get_status().map_err(Error::Io)?;
        if raw.abi_version != ABI_VERSION || raw.reserved[0] & FEATURE_ACC_HOLD == 0 {
            return Err(Error::NotReady("ACC transient hold unsupported".into()));
        }
        if operation == 1 { self.acc_lease = Some(generation); }
        let request = AccHold { owner: 1, generation, operation, ..AccHold::default() };
        let response = self.transport()?.acc_hold(request).map_err(Error::Io)?;
        if response.owner != 1 || response.generation != generation
            || response.operation != operation || response.reserved != 0 || response.error != 0
            || response.held != u32::from(operation != 2) {
            return Err(Error::InvalidState("ACC hold not confirmed"));
        }
        if operation != 2 {
            if let Some(valid) = &self.acc_validity {
                valid.store(true, std::sync::atomic::Ordering::Release);
            }
        } else {
            self.acc_lease = None;
            if self.acc_fm_cleanup_pending {
                self.transition_to_android()?;
                self.finish_fm_mix_release()?;
                self.acc_fm_cleanup_pending = false;
            }
        }
        Ok(())
    }

    /// Execute exactly one PREPARE or matching COMMIT source operation.
    pub fn transition(
        &mut self,
        source: Source,
        phase: Phase,
        master_index: i32,
        fm_mix_level: i32,
    ) -> Result<(), Error> {
        validate_fm_transition(master_index, fm_mix_level)?;
        if source == Source::Android
            && (master_index as u32 != ANDROID_MASTER_INDEX
                || fm_mix_level as u32 != ANDROID_FM_MIX_LEVEL)
        {
            return Err(Error::InvalidArgument(
                "Android transition requires master index 61 and FM mix level 0",
            ));
        }
        self.ready()?;
        let result = self
            .transport()?
            .source_transition(SourceTransition {
                source: source.raw(),
                master_volume_index: master_index as u32,
                fm_mix_level: fm_mix_level as u32,
                phase: phase.raw(),
            })
            .map_err(Error::Io);
        if phase != Phase::Commit {
            self.fm_mix_generation = None;
        }
        result
    }

    /// Android always uses the guard-required unity/zero request.
    pub fn prepare_android(&mut self) -> Result<(), Error> {
        self.transition(
            Source::Android,
            Phase::Prepare,
            ANDROID_MASTER_INDEX as i32,
            ANDROID_FM_MIX_LEVEL as i32,
        )
    }

    /// Commit exactly the prior Android PREPARE.
    pub fn commit_android(&mut self) -> Result<(), Error> {
        self.transition(
            Source::Android,
            Phase::Commit,
            ANDROID_MASTER_INDEX as i32,
            ANDROID_FM_MIX_LEVEL as i32,
        )
    }

    /// Atomically restore Android when a PREPARE may have failed or been
    /// abandoned. This must not be emulated by COMMIT: the kernel returns
    /// `-EPIPE` for COMMIT without its exact pending PREPARE.
    pub fn transition_to_android(&mut self) -> Result<(), Error> {
        self.transition(
            Source::Android,
            Phase::Atomic,
            ANDROID_MASTER_INDEX as i32,
            ANDROID_FM_MIX_LEVEL as i32,
        )
    }

    /// Prepare FM under the existing physical mute. FM power must already be
    /// enabled; the kernel enforces that dependency with `-EPIPE` too.
    pub fn prepare_fm(&mut self, master_index: i32, fm_mix_level: i32) -> Result<(), Error> {
        self.transition(Source::Fm, Phase::Prepare, master_index, fm_mix_level)
    }

    /// Commit exactly the prior FM PREPARE.
    pub fn commit_fm(&mut self, master_index: i32, fm_mix_level: i32) -> Result<(), Error> {
        self.transition(Source::Fm, Phase::Commit, master_index, fm_mix_level)
    }

    /// Refresh the local owner generation against the kernel's route state.
    /// Suspend or an internal legacy transition can invalidate the kernel
    /// generation without making a Binder call through this adapter.
    pub fn active_fm_mix_generation(&mut self) -> Result<Option<u64>, Error> {
        let raw = self.ready()?;
        if raw.source != SOURCE_FM_MIX || raw.fm_powered == 0 || raw.fault_latched != 0 {
            self.fm_mix_generation = None;
        }
        Ok(self.fm_mix_generation)
    }

    /// Whether the kernel still reports live FM rails. This is used only to
    /// choose a powered tuner shutdown versus strict bus-free invalidation.
    pub fn fm_rails_powered(&mut self) -> Result<bool, Error> {
        let raw = self.transport()?.get_status().map_err(Error::Io)?;
        if raw.abi_version != ABI_VERSION {
            return Err(Error::NotReady(format!(
                "NotReady/Phase-4c-audio-guard-abi-mismatch-found-{}-expected-{}",
                raw.abi_version, ABI_VERSION
            )));
        }
        Ok(raw.fm_powered != 0)
    }

    /// Prepare mixed Android/FM playback under exact digital mute.
    pub fn prepare_fm_mix(&mut self, generation: u64) -> Result<(), Error> {
        if generation == 0 {
            return Err(Error::InvalidArgument("FM mix generation must be nonzero"));
        }
        self.ready()?;
        if self.fm_mix_generation.is_some() {
            return Err(Error::InvalidState("an FM mix owner is already active"));
        }
        self.transport()?
            .fm_mix_transition(FmMixTransition {
                generation,
                phase: PHASE_PREPARE,
                reserved: 0,
            })
            .map_err(Error::Io)?;
        self.fm_mix_generation = Some(generation);
        Ok(())
    }

    /// Commit only the exact owner's muted preparation.
    pub fn commit_fm_mix(&mut self, generation: u64) -> Result<(), Error> {
        if self.fm_mix_generation != Some(generation) {
            return Err(Error::InvalidState("FM mix owner is not current"));
        }
        self.ready()?;
        self.transport()?
            .fm_mix_transition(FmMixTransition {
                generation,
                phase: PHASE_COMMIT,
                reserved: 0,
            })
            .map_err(Error::Io)
    }

    /// Apply one bounded, no-louder-rounded analogue FM attenuation update.
    pub fn set_fm_attenuation(
        &mut self,
        generation: u64,
        attenuation_millibels: i32,
    ) -> Result<(), Error> {
        if !(0..=MAX_FM_ATTENUATION_MILLIBELS as i32).contains(&attenuation_millibels) {
            return Err(Error::InvalidArgument(
                "FM attenuation must be 0..9600 millibels",
            ));
        }
        if self.fm_mix_generation != Some(generation) {
            return Err(Error::InvalidState("FM mix owner is not current"));
        }
        #[cfg(any(test, feature = "acc-power-channel"))]
        if self.acc_held() {
            return Err(Error::InvalidState(FM_ACC_SUSPENDED));
        }
        self.ready()?;
        self.transport()?
            .fm_attenuation(FmAttenuation {
                generation,
                attenuation_millibels: attenuation_millibels as u32,
                reserved: 0,
            })
            .map_err(Error::Io)
    }

    /// Select the owner's FM trim base as an OEM radio-mix level 0..100.
    ///
    /// The kernel maps the level to its reviewed `fm_coeff[]` word, refuses
    /// anything above level 100 (the factory +14 dB power-up trim), accepts
    /// it only after COMMIT for the exact generation, and re-applies the
    /// owner's last attenuation on top of it.  No coefficient crosses here.
    pub fn set_fm_mix_base_level(&mut self, generation: u64, level: i32) -> Result<(), Error> {
        if !(0..=MAX_FM_MIX_LEVEL as i32).contains(&level) {
            return Err(Error::InvalidArgument("FM mix base level must be 0..100"));
        }
        if self.fm_mix_generation != Some(generation) {
            return Err(Error::InvalidState("FM mix owner is not current"));
        }
        #[cfg(any(test, feature = "acc-power-channel"))]
        if self.acc_held() {
            return Err(Error::InvalidState(FM_ACC_SUSPENDED));
        }
        self.ready()?;
        self.transport()?
            .fm_mix_base(FmMixBase {
                generation,
                level: level as u32,
                reserved: 0,
            })
            .map_err(Error::Io)
    }

    /// Immediately close the mixed branch while leaving tuner rails live for
    /// its bounded mute/standby cleanup.
    pub fn silence_fm_mix(&mut self, generation: u64) -> Result<bool, Error> {
        if self.active_fm_mix_generation()? != Some(generation) {
            return Ok(false);
        }
        self.fm_mix_generation = None;
        self.transition_to_android()?;
        Ok(true)
    }

    /// Remove FM rails after the tuner state has been reconciled.
    pub fn finish_fm_mix_release(&mut self) -> Result<(), Error> {
        #[cfg(any(test, feature = "acc-power-channel"))]
        if self.acc_held() {
            self.acc_defer_fm_cleanup();
            return Err(Error::Io(io::Error::from_raw_os_error(16)));
        }
        self.fm_mix_generation = None;
        self.transport()?.fm_power(false).map_err(Error::Io)
    }

    /// Close only the current mixed branch; never power-cycle DSP/DAC/amp.
    pub fn release_fm_mix(&mut self, generation: u64) -> Result<(), Error> {
        let silence = self.silence_fm_mix(generation);
        let rails = self.finish_fm_mix_release();
        silence.map(|_| ()).and(rails)
    }
}

/// Convert a raw kernel errno into the service-specific code the Binder
/// contract returns. Kernel errors are positive in `raw_os_error`; the AIDL
/// convention is their stable negative counterpart. Unknown/non-errno I/O
/// remains `-1` without pretending to be a known kernel failure.
pub fn service_specific_errno(error: &io::Error) -> i32 {
    match error.raw_os_error() {
        Some(errno) if errno > 0 => -errno,
        Some(errno) if errno < 0 => errno,
        _ => -1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn acc_hold_fixed_abi_requires_capability_confirmation_and_revokes_on_fault_safe() {
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
        assert_eq!(std::mem::size_of::<AccHold>(), 32);
        assert_eq!(ACC_HOLD, 0xc020bc0f);
        let mut guard = AudioGuard::with_transport(FakeTransport::ready());
        let valid = Arc::new(AtomicBool::new(true));
        guard.attach_acc_validity(valid.clone());
        assert!(!valid.load(Ordering::Acquire));
        assert!(guard.acc_operation(1, 1).is_err()); // no feature, no pretend hold
        guard.transport.as_mut().unwrap().status.reserved[0] |= FEATURE_ACC_HOLD;
        guard.acc_operation(1, 1).unwrap();
        assert!(guard.acc_held());
        assert!(guard.set_output_mute(DspOutput::FrontLeft, false).is_err());
        assert!(valid.load(Ordering::Acquire));
        guard.acc_operation(1, 3).unwrap();
        guard.force_fault_safe().unwrap();
        assert!(!valid.load(Ordering::Acquire));
        guard.transport.as_mut().unwrap().fail_acc = true;
        assert!(guard.acc_operation(1, 1).is_err());
        assert!(!valid.load(Ordering::Acquire));
        guard.transport.as_mut().unwrap().fail_acc = false;
        guard.acc_defer_fm_cleanup();
        guard.acc_operation(1, 2).unwrap();
        assert!(!guard.acc_held());
        assert!(!valid.load(Ordering::Acquire));
    }

    #[derive(Default)]
    struct FakeTransport {
        fail_acc: bool,
        status: RawStatus,
        calls: Vec<String>,
        statuses: VecDeque<io::Result<RawStatus>>,
        fail_transition: bool,
        fail_dsp_on: bool,
        fail_dsp_update: Option<i32>,
    }

    impl FakeTransport {
        fn ready() -> Self {
            Self {
                status: RawStatus {
                    abi_version: ABI_VERSION,
                    dsp_powered: 1,
                    program_loaded: 1,
                    ..RawStatus::default()
                },
                ..Self::default()
            }
        }
    }

    impl Transport for FakeTransport {
        fn acc_hold(&mut self, mut request: AccHold) -> io::Result<AccHold> {
            self.calls.push(format!("acc:{}:{}:{}", request.owner, request.generation, request.operation));
            request.held = u32::from(request.operation != 2 && !self.fail_acc);
            request.error = if self.fail_acc { -5 } else { 0 };
            Ok(request)
        }
        fn get_status(&mut self) -> io::Result<RawStatus> {
            self.statuses.pop_front().unwrap_or_else(|| Ok(self.status))
        }

        fn fault_safe(&mut self) -> io::Result<()> {
            self.calls.push("fault-safe".into());
            Ok(())
        }

        fn dsp_on(&mut self) -> io::Result<()> {
            self.calls.push("dsp-on".into());
            if self.fail_dsp_on {
                return Err(io::Error::from_raw_os_error(6));
            }
            self.status.dsp_powered = 1;
            self.status.program_loaded = 1;
            Ok(())
        }

        fn dac_power(&mut self, enabled: bool) -> io::Result<()> {
            self.calls.push(format!("dac-power({enabled})"));
            self.status.dac_enabled = enabled.into();
            Ok(())
        }

        fn amplifier_power(&mut self, enabled: bool) -> io::Result<()> {
            self.calls.push(format!("amp-power({enabled})"));
            self.status.amplifier_enabled = enabled.into();
            Ok(())
        }

        fn source_transition(&mut self, request: SourceTransition) -> io::Result<()> {
            self.calls.push(format!(
                "transition(source={},master={},mix={},phase={})",
                request.source, request.master_volume_index, request.fm_mix_level, request.phase
            ));
            if self.fail_transition {
                Err(io::Error::from_raw_os_error(32))
            } else {
                Ok(())
            }
        }

        fn dsp_update(&mut self, request: DspUpdate) -> io::Result<()> {
            self.calls.push(format!(
                "dsp-update(operation={},channel={},band={},value={},reserved={:?})",
                request.operation, request.channel, request.band, request.value, request.reserved
            ));
            if let Some(errno) = self.fail_dsp_update {
                return Err(io::Error::from_raw_os_error(errno));
            }
            Ok(())
        }

        // Overridden so master traffic stays distinguishable from control
        // traffic in `calls`; the production transport does NOT override it.
        fn master_volume(&mut self, request: DspUpdate) -> io::Result<()> {
            self.calls.push(format!(
                "master(operation={},channel={},band={},value={},reserved={:?})",
                request.operation, request.channel, request.band, request.value, request.reserved
            ));
            Ok(())
        }

        fn fm_power(&mut self, enabled: bool) -> io::Result<()> {
            self.calls.push(format!("fm-power({enabled})"));
            self.status.fm_powered = enabled.into();
            Ok(())
        }

        fn fm_mix_transition(&mut self, request: FmMixTransition) -> io::Result<()> {
            self.calls.push(format!(
                "fm-mix(generation={},phase={},reserved={})",
                request.generation, request.phase, request.reserved
            ));
            if request.phase == PHASE_PREPARE {
                self.status.source = SOURCE_FM_MIX;
                self.status.transition_pending = 1;
            } else {
                self.status.transition_pending = 0;
                self.status.route_ready = 1;
            }
            Ok(())
        }

        fn fm_attenuation(&mut self, request: FmAttenuation) -> io::Result<()> {
            self.calls.push(format!(
                "fm-attenuation(generation={},millibels={},reserved={})",
                request.generation, request.attenuation_millibels, request.reserved
            ));
            Ok(())
        }

        fn fm_mix_base(&mut self, request: FmMixBase) -> io::Result<()> {
            self.calls.push(format!(
                "fm-mix-base(generation={},level={},reserved={})",
                request.generation, request.level, request.reserved
            ));
            Ok(())
        }
    }

    #[test]
    fn exact_kernel_uapi_layout_and_ioctl_numbers_are_frozen() {
        assert_eq!(std::mem::size_of::<RawStatus>(), 64);
        assert_eq!(std::mem::size_of::<SourceTransition>(), 16);
        assert_eq!(std::mem::size_of::<DspUpdate>(), 32);
        assert_eq!(std::mem::size_of::<FmMixTransition>(), 16);
        assert_eq!(std::mem::size_of::<FmAttenuation>(), 16);
        assert_eq!(std::mem::size_of::<FmMixBase>(), 16);
        assert_eq!(FM_MIX_BASE, 0x4010_bc10);
        assert_eq!(FEATURE_FM_MIX_BASE, 4);
        assert_eq!(GET_STATUS, 0x8040_bc00);
        assert_eq!(DSP_ON, 0x0000_bc01);
        assert_eq!(DAC_ON, 0x0000_bc05);
        assert_eq!(AMP_ON, 0x0000_bc08);
        assert_eq!(FAULT_SAFE, 0x0000_bc0a);
        assert_eq!(FM_ON, 0x0000_bc03);
        assert_eq!(FM_OFF, 0x0000_bc04);
        assert_eq!(SOURCE_TRANSITION, 0x4010_bc0b);
        assert_eq!(DSP_UPDATE, 0x4020_bc0c);
        assert_eq!(FM_MIX_TRANSITION, 0x4010_bc0d);
        assert_eq!(FM_ATTENUATION, 0x4010_bc0e);
        assert_eq!(Phase::Atomic.raw(), 0);
    }

    #[test]
    fn absent_node_reports_distinct_not_ready_and_never_touches_transport() {
        let mut guard = AudioGuard::<FakeTransport>::unavailable(NOT_READY_ABSENT);
        let status = guard.status();
        assert!(!status.available);
        assert_eq!(status.unavailable_reason, NOT_READY_ABSENT);
        assert!(status.raw.is_none());
        assert!(
            matches!(guard.set_fm_power(true), Err(Error::NotReady(reason)) if reason == NOT_READY_ABSENT)
        );
    }

    #[test]
    fn present_but_unprogrammed_guard_is_inert_and_distinct() {
        let mut transport = FakeTransport::ready();
        transport.status.program_loaded = 0;
        let mut guard = AudioGuard::with_transport(transport);
        let status = guard.status();
        assert!(!status.available);
        assert_eq!(status.unavailable_reason, NOT_READY_PROGRAM);
        assert_eq!(status.raw.unwrap().abi_version, ABI_VERSION);
        assert!(
            matches!(guard.prepare_android(), Err(Error::NotReady(reason)) if reason == NOT_READY_PROGRAM)
        );
        assert!(guard.transport.unwrap().calls.is_empty());
    }

    #[test]
    fn each_android_half_uses_the_forced_kernel_request_not_atomic() {
        let mut guard = AudioGuard::with_transport(FakeTransport::ready());
        guard.prepare_android().unwrap();
        guard.commit_android().unwrap();
        assert_eq!(
            guard.transport.unwrap().calls,
            [
                "transition(source=1,master=61,mix=0,phase=1)",
                "transition(source=1,master=61,mix=0,phase=2)",
            ]
        );
    }

    #[test]
    fn atomic_android_fallback_is_one_request_and_never_a_stray_commit() {
        let mut guard = AudioGuard::with_transport(FakeTransport::ready());
        guard.transition_to_android().unwrap();
        assert_eq!(
            guard.transport.unwrap().calls,
            ["transition(source=1,master=61,mix=0,phase=0)"]
        );
    }

    #[test]
    fn mixed_route_is_generation_keyed_and_attenuation_is_semantic() {
        let mut transport = FakeTransport::ready();
        transport.status.fm_powered = 1;
        let mut guard = AudioGuard::with_transport(transport);

        guard.prepare_fm_mix(17).unwrap();
        assert!(matches!(
            guard.commit_fm_mix(18),
            Err(Error::InvalidState("FM mix owner is not current"))
        ));
        guard.commit_fm_mix(17).unwrap();
        guard.set_fm_attenuation(17, 1_301).unwrap();
        assert!(matches!(
            guard.set_fm_attenuation(17, 9_601),
            Err(Error::InvalidArgument(_))
        ));
        // The owner's trim base rides the same lease: range first, then
        // exact generation, then the ioctl. Level 100 is the OEM ceiling.
        assert!(matches!(
            guard.set_fm_mix_base_level(17, 101),
            Err(Error::InvalidArgument(_))
        ));
        assert!(matches!(
            guard.set_fm_mix_base_level(17, -1),
            Err(Error::InvalidArgument(_))
        ));
        assert!(matches!(
            guard.set_fm_mix_base_level(18, 100),
            Err(Error::InvalidState("FM mix owner is not current"))
        ));
        guard.set_fm_mix_base_level(17, 100).unwrap();
        assert_eq!(
            guard.transport.as_ref().unwrap().calls,
            [
                "fm-mix(generation=17,phase=1,reserved=0)",
                "fm-mix(generation=17,phase=2,reserved=0)",
                "fm-attenuation(generation=17,millibels=1301,reserved=0)",
                "fm-mix-base(generation=17,level=100,reserved=0)",
            ]
        );
    }

    #[test]
    fn owner_release_closes_branch_without_global_power_cycle() {
        let mut transport = FakeTransport::ready();
        transport.status.fm_powered = 1;
        let mut guard = AudioGuard::with_transport(transport);

        guard.prepare_fm_mix(21).unwrap();
        guard.commit_fm_mix(21).unwrap();
        guard.release_fm_mix(21).unwrap();
        assert_eq!(
            &guard.transport.as_ref().unwrap().calls[2..],
            [
                "transition(source=1,master=61,mix=0,phase=0)",
                "fm-power(false)",
            ]
        );
        assert!(!guard
            .transport
            .as_ref()
            .unwrap()
            .calls
            .iter()
            .any(|call| matches!(
                call.as_str(),
                "fault-safe" | "dsp-on" | "dac-power(false)" | "amp-power(false)"
            )));
    }

    #[test]
    fn fm_power_precedes_a_typed_prepare_and_commit_preserves_every_field() {
        let mut guard = AudioGuard::with_transport(FakeTransport::ready());
        guard.set_fm_power(true).unwrap();
        guard.prepare_fm(42, 17).unwrap();
        guard.commit_fm(42, 17).unwrap();
        assert_eq!(
            guard.transport.unwrap().calls,
            [
                "fm-power(true)",
                "transition(source=2,master=42,mix=17,phase=1)",
                "transition(source=2,master=42,mix=17,phase=2)",
            ]
        );
    }

    #[test]
    fn bounded_master_update_has_the_exact_reviewed_dsp_shape() {
        let mut guard = AudioGuard::with_transport(FakeTransport::ready());
        guard.set_master_index(37).unwrap();
        assert_eq!(
            guard.transport.unwrap().calls,
            ["master(operation=6,channel=0,band=0,value=37,reserved=[0, 0, 0, 0])"]
        );
    }

    #[test]
    fn invalid_values_are_refused_before_any_kernel_operation() {
        let mut guard = AudioGuard::with_transport(FakeTransport::ready());
        for result in [
            guard.set_master_index(-1),
            guard.set_master_index(62),
            guard.prepare_fm(5, 101),
            guard.transition(Source::Android, Phase::Prepare, 60, 0),
        ] {
            assert!(matches!(result, Err(Error::InvalidArgument(_))));
        }
        assert!(guard.transport.unwrap().calls.is_empty());
    }

    #[test]
    fn ioctl_failure_is_propagated_and_not_rewritten_as_success() {
        let mut transport = FakeTransport::ready();
        transport.fail_transition = true;
        let mut guard = AudioGuard::with_transport(transport);
        assert!(
            matches!(guard.prepare_android(), Err(Error::Io(error)) if error.raw_os_error() == Some(32))
        );
    }

    #[test]
    fn ensure_ready_uses_the_complete_guarded_activation_order() {
        let mut transport = FakeTransport::ready();
        transport.status.program_loaded = 0;
        let mut guard = AudioGuard::with_transport(transport);
        guard.ensure_ready().unwrap();
        assert_eq!(
            guard.transport.unwrap().calls,
            [
                "fault-safe",
                "dsp-on",
                "transition(source=1,master=61,mix=0,phase=1)",
                "transition(source=1,master=61,mix=0,phase=2)",
                "dac-power(true)",
                "amp-power(true)",
            ]
        );
    }

    #[test]
    fn activation_failure_returns_raw_errno_and_leaves_visible_safe_state() {
        let mut transport = FakeTransport::ready();
        transport.fail_dsp_on = true;
        let mut guard = AudioGuard::with_transport(transport);
        assert!(
            matches!(guard.ensure_ready(), Err(Error::Io(error)) if error.raw_os_error() == Some(6))
        );
        let status = guard.status();
        assert!(!status.available);
        assert!(status.unavailable_reason.contains("activation-failed"));
        assert_eq!(
            guard.transport.unwrap().calls,
            ["fault-safe", "dsp-on", "fault-safe"]
        );
    }

    #[test]
    fn only_a_successful_activation_advances_the_program_epoch() {
        let mut guard = AudioGuard::with_transport(FakeTransport::ready());
        assert_eq!(guard.program_epoch(), 0);
        guard.ensure_ready().unwrap();
        assert_eq!(guard.program_epoch(), 1);
        guard.transport.as_mut().unwrap().fail_dsp_on = true;
        assert!(guard.ensure_ready().is_err());
        assert_eq!(guard.program_epoch(), 1, "a failed reload changed nothing on the chip we know of");
        guard.transport.as_mut().unwrap().fail_dsp_on = false;
        guard.recover_after_acc_fault().unwrap();
        assert_eq!(guard.program_epoch(), 2);
    }

    #[test]
    fn errno_mapping_keeps_distinct_kernel_errors() {
        assert_eq!(service_specific_errno(&io::Error::from_raw_os_error(6)), -6);
        assert_eq!(
            service_specific_errno(&io::Error::from_raw_os_error(32)),
            -32
        );
        assert_eq!(
            service_specific_errno(&io::Error::other("host failure")),
            -1
        );
    }
}
