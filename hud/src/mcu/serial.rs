// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! SE0 tty ownership and the ACC codec/machine runtime.
//!
//! This is the daemon's only MCU write path. [`TxWriter::send`] applies
//! [`super::may_transmit`] immediately before `write_all`; callers cannot get
//! at the owned tty. RX bytes go through [`Parser`](super::Parser), and only
//! checksum-valid frames reach [`Machine`](super::acc::Machine).

use super::acc::{CycleEvent, Hooks, Machine, State, SuspendProof, Tx, WakeToken};
use super::{
    may_transmit, resolve_port, Frame, FrameRefused, Item, Parser, SE0_MMIO, TX_ACC_ON_ANSWER,
    TX_VERSION_QUERY, TX_GPS_READY,
};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// `O_NOCTTY`: never let a tty we open become the controlling terminal.
/// Shared with [`crate::can::port`], which opens its port read-only.
pub(crate) const O_NOCTTY: i32 = 0x0100;
const TCGETS: u64 = 0x5401;
const TCSETS: u64 = 0x5402;
const NCCS: usize = 19;
const VTIME: usize = 5;
const VMIN: usize = 6;

const CBAUD: u32 = 0x0000_100f;
const CSIZE: u32 = 0x0000_0030;
const CS8: u32 = 0x0000_0030;
const CSTOPB: u32 = 0x0000_0040;
const CREAD: u32 = 0x0000_0080;
const PARENB: u32 = 0x0000_0100;
const PARODD: u32 = 0x0000_0200;
const CLOCAL: u32 = 0x0000_0800;
const B115200: u32 = 0x0000_1002;
const CRTSCTS: u32 = 0x8000_0000;

/// Linux UAPI `struct termios` (`asm-generic/termbits.h`).
///
/// Both the device's aarch64 ABI and the desk's x86_64 ABI use 19 control
/// bytes for the `TCGETS`/`TCSETS` ioctl structure. This is deliberately the
/// kernel structure, not libc's larger userspace wrapper.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct KernelTermios {
    iflag: u32,
    oflag: u32,
    cflag: u32,
    lflag: u32,
    line: u8,
    cc: [u8; NCCS],
}

// Declared locally to keep the desk crate dependency-free. Soong links libc.
extern "C" {
    fn ioctl(fd: RawFd, request: u64, ...) -> i32;
}

/// Configure one already-open tty exactly like the measured Go bridge.
///
/// `~/android/apps/mcu-toolbox/bridge/main.go:2337-2354` clears input/output/
/// local processing, selects 115200 raw 8N1 with receiver/local mode enabled,
/// and uses `VMIN=0`, `VTIME=10` so a silent MCU wakes the read loop once per
/// second. Hardware flow control is explicitly cleared because SE0 is two-wire
/// (TLMM 2/3), not RTS/CTS.
fn configure_uart(fd: RawFd) -> io::Result<()> {
    configure_raw_8n1(fd, B115200)
}

/// The same raw-8N1 setup at an arbitrary `CBAUD` code: `VMIN=0`, `VTIME=10`,
/// no flow control, no processing. The MCU passes `B115200`; the CAN decoder
/// box port ([`crate::can::port`]) passes the profile's rate (38400 on the
/// XinPu box). One function so the two ports cannot drift in anything but the
/// baud.
pub(crate) fn configure_raw_8n1(fd: RawFd, cbaud: u32) -> io::Result<()> {
    let mut termios = KernelTermios::default();
    // SAFETY: `termios` is the kernel UAPI layout and remains live and writable
    // for the duration of TCGETS.
    if unsafe { ioctl(fd, TCGETS, &mut termios as *mut KernelTermios) } < 0 {
        return Err(io::Error::last_os_error());
    }

    termios.iflag = 0;
    termios.oflag = 0;
    termios.lflag = 0;
    termios.cflag &= !(CBAUD | CSIZE | CSTOPB | PARENB | PARODD | CRTSCTS);
    termios.cflag |= (cbaud & CBAUD) | CS8 | CREAD | CLOCAL;
    termios.cc[VMIN] = 0;
    termios.cc[VTIME] = 10;

    // SAFETY: same UAPI layout; TCSETS reads the structure synchronously.
    if unsafe { ioctl(fd, TCSETS, &termios as *const KernelTermios) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Test-only pty helpers shared by the MCU and CAN port tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::fs::File;
    use std::io::{self, Read};
    use std::os::fd::{AsRawFd, FromRawFd};

    const O_RDWR: i32 = 0x0002;
    const O_CLOEXEC: i32 = 0x0008_0000;
    const O_NONBLOCK: i32 = 0x0800;
    const F_GETFL: i32 = 3;
    const F_SETFL: i32 = 4;

    extern "C" {
        fn posix_openpt(flags: i32) -> i32;
        fn grantpt(fd: i32) -> i32;
        fn unlockpt(fd: i32) -> i32;
        fn ptsname_r(fd: i32, buf: *mut u8, buflen: usize) -> i32;
        fn fcntl(fd: i32, command: i32, ...) -> i32;
    }

    /// A pty master and the slave's path (the slave is NOT opened here, so a
    /// test can open it read-only, read-write, or not at all).
    pub(crate) fn pty_pair_with_name() -> (File, String) {
        // SAFETY: owned fds and a writable path buffer; the master fd is
        // transferred exactly once into File.
        unsafe {
            let master_fd = posix_openpt(O_RDWR | O_CLOEXEC | super::O_NOCTTY);
            assert!(master_fd >= 0, "posix_openpt: {}", io::Error::last_os_error());
            assert_eq!(grantpt(master_fd), 0, "grantpt: {}", io::Error::last_os_error());
            assert_eq!(unlockpt(master_fd), 0, "unlockpt: {}", io::Error::last_os_error());
            let mut path = [0u8; 128];
            assert_eq!(
                ptsname_r(master_fd, path.as_mut_ptr(), path.len()),
                0,
                "ptsname_r: {}",
                io::Error::last_os_error()
            );
            let end = path.iter().position(|byte| *byte == 0).unwrap();
            let slave = std::str::from_utf8(&path[..end]).unwrap().to_owned();
            (File::from_raw_fd(master_fd), slave)
        }
    }

    /// Everything readable right now on `file`, without blocking.
    pub(crate) fn read_all_available(file: &mut File) -> Vec<u8> {
        // SAFETY: fcntl reads/updates flags on this live fd.
        unsafe {
            let flags = fcntl(file.as_raw_fd(), F_GETFL);
            assert!(flags >= 0);
            assert_eq!(fcntl(file.as_raw_fd(), F_SETFL, flags | O_NONBLOCK), 0);
        }
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                // EIO on a pty master = the slave side hung up; pending bytes
                // were already returned above, so there is nothing more.
                Err(e) if e.raw_os_error() == Some(5) => break,
                Err(e) => panic!("read: {e}"),
            }
        }
        out
    }
}

fn open_configured(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NOCTTY)
        .open(path)?;
    configure_uart(file.as_raw_fd())?;
    Ok(file)
}

/// Checksum verdict attached to an observed complete receive frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameValidity {
    /// The checksum matched and the frame reached the ACC/event decoders.
    Valid,
    /// The frame boundary was complete but its checksum did not match.
    BadChecksum,
}

/// Host-side evidence only: neither success nor failure establishes MCU receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccTxPhase {
    /// Entered the gated send operation; no write has been attempted yet.
    Attempted,
    /// The gate refused the frame; zero UART writes.
    Refused,
    /// `write_all` returned success, not a drain or an MCU acknowledgement.
    WriteComplete,
    /// `write_all` failed; partial delivery is unknown.
    WriteError {
        /// Original host I/O error classification.
        kind: io::ErrorKind,
        /// Original OS error when available, not an invented byte count.
        errno: Option<i32>,
    },
}

/// Deferred observation of an existing runtime ACC lifecycle send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccTxObservation {
    /// One of the two fixed lifecycle frames, never caller-supplied raw bytes.
    pub tx: Tx,
    /// Host boundary observed.
    pub phase: AccTxPhase,
    /// Event-loop timestamp, possibly sampled before a blocking read; NOT TX time.
    pub loop_ms: u64,
}

impl std::fmt::Display for AccTxObservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (name, payload) = match self.tx {
            Tx::AccOffComplete => ("acc-off-complete", "05-15"),
            Tx::AccOnAnswer => ("acc-on-answer", "05-17"),
        };
        write!(f, "mcu-acc-tx name={name} payload={payload} loop_ms={} deferred=1 ", self.loop_ms)?;
        match self.phase {
            AccTxPhase::Attempted => write!(f, "phase=attempted boundary=before-gate delivery=unknown"),
            AccTxPhase::Refused => write!(f, "phase=refused uart_writes=0"),
            AccTxPhase::WriteComplete => write!(f, "phase=write-complete boundary=write_all-return mcu_ack=unobserved"),
            AccTxPhase::WriteError { kind, errno } => write!(f, "phase=write-error kind={kind:?} errno={errno:?} delivery=unknown"),
        }
    }
}

/// Platform policy callbacks that surround the fixed ACC lifecycle frames.
pub trait PlatformHooks {
    /// Ordered non-diagnostic cycle control. Called synchronously from the
    /// Machine, independently of observe_acc_tx, before legacy sleep intent.
    /// Keep this local and nonblocking; no Binder, filesystem or waiting I/O.
    /// A connected owner must preserve delivery or invalidate its session on
    /// queue failure. It must add a fresh envelope on session replacement and
    /// refuse an exhausted generation space. Default is deliberately inert.
    fn on_cycle_event(&mut self, _event: CycleEvent) {}
    /// Best-effort, local, nonblocking observation only. Never do logging I/O
    /// here: this runs inside the serial operation. A dropped observation must
    /// not change protocol errors, lifecycle policy, or control-message delivery.
    /// Even a pre-gate observation may only be logged later and can be lost on
    /// power loss; its absence never proves that no send was attempted.
    fn observe_acc_tx(&mut self, _observation: AccTxObservation) {}
    /// Blank/mark the ACC-off grace period while Android remains awake.
    fn enter_acc_off_grace(&mut self) -> io::Result<()>;
    /// Stop radio-producing subsystems before peripheral-rail removal.
    fn quiesce_radio(&mut self) -> io::Result<()>;
    /// Detach the log/stick sink before USB teardown.
    fn detach_stick_sink(&mut self) -> io::Result<()>;
    /// Quiesce USB and its wake sources.
    fn quiesce_usb(&mut self) -> io::Result<()>;
    /// Restore policy after the ACC-on answer.
    fn resume(&mut self) -> io::Result<()>;
    /// Queue a confirmed public state transition for the Android-facing owner.
    /// This callback must be local and nonblocking: Binder delivery happens
    /// only after the UART session and HUD core locks have been released.
    fn publish_acc_state(
        &mut self,
        _state: State,
        _reason: &'static str,
        _now_ms: u64,
    ) -> io::Result<()> {
        Ok(())
    }
    /// Queue the final-stage Android sleep request. The future system-side
    /// consumer owns PowerManager/SystemSuspend; the MCU reader never does.
    fn request_sleep(&mut self, _now_ms: u64) -> io::Result<()> {
        Ok(())
    }
    /// Observe the event-loop timestamp before a lifecycle callback.
    fn observe_time(&mut self, _now_ms: u64) {}
    /// Run platform work due at this event-loop timestamp.
    fn run_due(&mut self, _now_ms: u64) -> io::Result<()> {
        Ok(())
    }
}

/// Intentionally inert hooks until the Android owners are wired.
///
/// The serial/ACC path is real; the surrounding policy remains a visible
/// no-op rather than guessing at Android integration. Each method cites the
/// bridge operation it must eventually rework.
#[derive(Debug, Default)]
pub struct BridgeHooksTodo;

impl PlatformHooks for BridgeHooksTodo {
    fn enter_acc_off_grace(&mut self) -> io::Result<()> {
        // TODO(D18): bridge/main.go:358-372 blanks the panel but stays awake.
        Ok(())
    }

    fn quiesce_radio(&mut self) -> io::Result<()> {
        // TODO(D18): bridge/main.go:474-500 stops radio/Bluetooth producers.
        Ok(())
    }

    fn detach_stick_sink(&mut self) -> io::Result<()> {
        // TODO(D18): bridge/main.go:1402-1405 assumes its stick mirror is done;
        // device 3 needs an explicit vold/unmount acknowledgement here.
        Ok(())
    }

    fn quiesce_usb(&mut self) -> io::Result<()> {
        // TODO(D18): bridge/main.go:501-520 performs the USB/hub transition.
        Ok(())
    }

    fn resume(&mut self) -> io::Result<()> {
        // TODO(D18): bridge/main.go:1485-1505 restores host policy first.
        Ok(())
    }
}

/// An error from the sole transmit site or a surrounding hook.
#[derive(Debug)]
pub enum SessionError {
    /// Tty or platform I/O failed.
    Io(io::Error),
    /// The deny/allow gate refused a frame before any write.
    Refused(FrameRefused),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Io(error) => write!(f, "MCU session I/O: {}", error),
            SessionError::Refused(reason) => write!(f, "MCU transmit refused: {}", reason),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<io::Error> for SessionError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

struct TxWriter<T> {
    io: T,
}

impl<T: Read + Write> TxWriter<T> {
    fn new(io: T) -> Self {
        Self { io }
    }

    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.io.read(bytes)
    }

    /// The one and only MCU `write_all` in the daemon core.
    fn send(&mut self, frame: &[u8]) -> Result<(), SessionError> {
        self.send_observed(frame, |_| {})
    }

    fn send_observed(
        &mut self,
        frame: &[u8],
        mut observe: impl FnMut(AccTxPhase),
    ) -> Result<(), SessionError> {
        observe(AccTxPhase::Attempted);
        if let Err(reason) = may_transmit(frame) {
            observe(AccTxPhase::Refused);
            return Err(SessionError::Refused(reason));
        }
        let result = self.io.write_all(frame);
        observe(match &result {
            Ok(()) => AccTxPhase::WriteComplete,
            Err(error) => AccTxPhase::WriteError {
                kind: error.kind(),
                errno: error.raw_os_error(),
            },
        });
        result.map_err(SessionError::Io)
    }
}

struct RuntimeHooks<T, P> {
    writer: TxWriter<T>,
    platform: P,
    now_ms: u64,
    first_error: Option<SessionError>,
}

impl<T, P> RuntimeHooks<T, P> {
    fn record(&mut self, result: Result<(), SessionError>) {
        if self.first_error.is_none() {
            self.first_error = result.err();
        }
    }

    fn finish(&mut self) -> Result<(), SessionError> {
        match self.first_error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn observe_time(&mut self, now_ms: u64)
    where
        P: PlatformHooks,
    {
        self.now_ms = now_ms;
        self.platform.observe_time(now_ms);
    }
}

impl<T: Read + Write, P: PlatformHooks> Hooks for RuntimeHooks<T, P> {
    fn on_cycle_event(&mut self, event: CycleEvent) {
        self.platform.on_cycle_event(event);
    }

    fn enter_acc_off_grace(&mut self) {
        let result = self
            .platform
            .enter_acc_off_grace()
            .map_err(SessionError::Io);
        self.record(result);
    }

    fn quiesce_radio(&mut self) {
        let result = self.platform.quiesce_radio().map_err(SessionError::Io);
        self.record(result);
    }

    fn detach_stick_sink(&mut self) {
        let result = self.platform.detach_stick_sink().map_err(SessionError::Io);
        self.record(result);
    }

    fn quiesce_usb(&mut self) {
        let result = self.platform.quiesce_usb().map_err(SessionError::Io);
        self.record(result);
    }

    fn transmit(&mut self, frame: Tx) -> super::acc::HostWriteResult {
        let platform = &mut self.platform;
        let loop_ms = self.now_ms;
        let result = self.writer.send_observed(frame.bytes(), |phase| {
            platform.observe_acc_tx(AccTxObservation { tx: frame, phase, loop_ms });
        });
        let host_write = if result.is_ok() {
            super::acc::HostWriteResult::Complete
        } else {
            super::acc::HostWriteResult::Failed
        };
        self.record(result);
        host_write
    }

    fn request_suspend(&mut self) {
        let result = self
            .platform
            .request_sleep(self.now_ms)
            .map_err(SessionError::Io);
        self.record(result);
    }

    fn resume(&mut self) {
        let result = self.platform.resume().map_err(SessionError::Io);
        self.record(result);
    }
}

/// One owner for the SE0 stream, codec, ACC machine and gated transmitter.
pub struct AccSession<T, P> {
    parser: Parser,
    machine: Machine,
    hooks: RuntimeHooks<T, P>,
    diagnostics: super::diagnostics::Snapshot,
}

impl<T: Read + Write, P: PlatformHooks> AccSession<T, P> {
    /// Build a session around an already-open duplex stream.
    pub fn new(io: T, platform: P) -> Self {
        Self {
            parser: Parser::new(),
            machine: Machine::new(),
            diagnostics: super::diagnostics::Snapshot::default(),
            hooks: RuntimeHooks {
                writer: TxWriter::new(io),
                platform,
                now_ms: 0,
                first_error: None,
            },
        }
    }

    /// Fixed-size metadata from this owner; no raw bytes or key codes.
    #[cfg(any(test, feature = "binder"))]
    pub(crate) fn diagnostics(&self) -> super::diagnostics::Snapshot {
        self.diagnostics
    }

    /// Identify a new stream session; counters and records belong to this open.
    #[cfg(any(test, feature = "binder"))]
    pub(crate) fn set_diagnostic_session(&mut self, session: u64) {
        self.diagnostics.session = session;
    }

    /// Current confirmed ACC lifecycle state.
    pub fn state(&self) -> State {
        self.machine.state()
    }

    /// Current transition generation within this session only. Resume advances
    /// it; use CycleEvent's off_generation to cancel the previous OFF cycle.
    /// A new AccSession requires a fresh broker envelope, not number comparison.
    pub fn generation(&self) -> u64 {
        self.machine.generation()
    }

    /// UART owner's policy state, for pre-step checkpoints and post-step
    /// notification. This exposes neither the writer nor another MCU machine.
    #[cfg(any(test, feature = "acc-power-channel"))]
    pub fn platform_mut(&mut self) -> &mut P {
        &mut self.hooks.platform
    }

    /// Send the fixed stock version query through the sole gated TX site.
    pub fn send_version_query(&mut self) -> Result<(), SessionError> {
        self.hooks.writer.send(TX_VERSION_QUERY)
    }

    /// Send the fixed OEM GPS-ready notification through the sole TX gate.
    /// Success proves a complete host write, not MCU receipt or GPS reception.
    pub fn send_gps_ready(&mut self) -> Result<(), SessionError> {
        self.hooks.writer.send(TX_GPS_READY)
    }

    /// Send the fixed startup ACC-on answer through the sole gated TX site.
    ///
    /// The runtime waits the oracle's 750 ms after [`Self::send_version_query`]
    /// before calling this, so activation and acknowledgement are not one UART
    /// burst. No caller can supply bytes to either method.
    pub fn send_startup_acc_on_answer(&mut self) -> Result<(), SessionError> {
        self.hooks.writer.send(TX_ACC_ON_ANSWER)
    }

    fn publish_state_change(&mut self, before: State, reason: &'static str) {
        let after = self.machine.state();
        if before != after {
            let result = self
                .hooks
                .platform
                .publish_acc_state(after, reason, self.hooks.now_ms)
                .map_err(SessionError::Io);
            self.hooks.record(result);
        }
    }

    /// Read one tty chunk, decode all complete valid frames and advance time.
    pub fn read_once(&mut self, now_ms: u64) -> Result<usize, SessionError> {
        self.read_once_observed(now_ms, |_, _| {})
    }

    /// Read one tty chunk and synchronously observe every complete frame.
    ///
    /// Both valid and bad-checksum frames are reported before the valid frame
    /// advances the ACC machine. This is the capture seam used by the Android
    /// reader thread to log raw bytes and decoded meaning without adding a
    /// second parser or a second UART owner.
    pub fn read_once_observed<F>(
        &mut self,
        now_ms: u64,
        mut observe: F,
    ) -> Result<usize, SessionError>
    where
        F: FnMut(&Frame, FrameValidity),
    {
        self.hooks.observe_time(now_ms);
        let mut bytes = [0u8; 512];
        let read = self.hooks.writer.read(&mut bytes);
        self.diagnostics.read(now_ms, &read);
        let count = read?;
        if count != 0 {
            self.ingest(&bytes[..count], now_ms, &mut observe);
        }
        self.diagnostics.parser(self.parser.stats(), self.parser.pending_len());
        let before = self.machine.state();
        self.machine.advance(now_ms, &mut self.hooks);
        self.publish_state_change(before, "timer");
        self.hooks
            .platform
            .run_due(now_ms)
            .map_err(SessionError::Io)?;
        self.hooks.finish()?;
        Ok(count)
    }

    /// Run a due ACC final-stage callback without waiting for another read.
    pub fn advance(&mut self, now_ms: u64) -> Result<(), SessionError> {
        self.hooks.observe_time(now_ms);
        let before = self.machine.state();
        self.machine.advance(now_ms, &mut self.hooks);
        self.publish_state_change(before, "timer");
        self.hooks
            .platform
            .run_due(now_ms)
            .map_err(SessionError::Io)?;
        self.hooks.finish()
    }

    /// Repeat the ACC-on answer once after a deliberate tty reopen.
    pub fn uart_reopened(&mut self) -> Result<bool, SessionError> {
        let emitted = self.machine.on_uart_reopened(&mut self.hooks);
        self.hooks.finish()?;
        Ok(emitted)
    }

    /// Snapshot a generation before an asynchronous KPDPWR handler runs.
    pub fn wake_token(&self) -> WakeToken {
        self.machine.wake_token()
    }

    /// Revalidate and apply an asynchronous hardware wake.
    pub fn hardware_wake(
        &mut self,
        token: WakeToken,
        proof: SuspendProof,
        now_ms: u64,
    ) -> Result<bool, SessionError> {
        self.hooks.observe_time(now_ms);
        let before = self.machine.state();
        let accepted = self
            .machine
            .on_hardware_wake(token, proof, now_ms, &mut self.hooks);
        self.publish_state_change(before, "hardware-wake");
        self.hooks
            .platform
            .run_due(now_ms)
            .map_err(SessionError::Io)?;
        self.hooks.finish()?;
        Ok(accepted)
    }

    fn ingest<F>(&mut self, bytes: &[u8], now_ms: u64, observe: &mut F)
    where
        F: FnMut(&Frame, FrameValidity),
    {
        for item in self.parser.push(bytes) {
            match item {
                Item::Frame(frame) => {
                    self.diagnostics.frame(&frame, true, now_ms);
                    observe(&frame, FrameValidity::Valid);
                    let event = frame.event();
                    let before = self.machine.state();
                    self.machine.on_event(&event, now_ms, &mut self.hooks);
                    self.publish_state_change(before, event_reason(&event));
                }
                Item::BadChecksum(frame) => {
                    self.diagnostics.frame(&frame, false, now_ms);
                    observe(&frame, FrameValidity::BadChecksum);
                }
                Item::Discarded { .. } => {}
            }
        }
    }
}

fn event_reason(event: &super::Event) -> &'static str {
    match event {
        super::Event::AccOff => "mcu-12-03",
        super::Event::AccOn => "mcu-12-04",
        super::Event::AccWake => "mcu-12-0d",
        super::Event::PowerKey { abstract_key, .. }
            if *abstract_key == super::POWER_KEY_SHUTDOWN =>
        {
            "mcu-05-10"
        }
        _ => "mcu-event",
    }
}

impl<P: PlatformHooks> AccSession<File, P> {
    /// Enabled ACC channel cadence: wait at most 25ms before the caller samples
    /// fresh clocks/checkpoints. On a quiet UART call `advance`, NOT a VTIME
    /// read. The unchanged VMIN0/VTIME10 read is used only after POLLIN, by this
    /// sole reader, so an otherwise silent peer cannot swallow the 500ms window.
    #[cfg(any(test, feature = "acc-power-channel"))]
    pub fn wait_acc_input(&self) -> io::Result<bool> {
        #[repr(C)]
        struct PollFd {
            fd: i32,
            events: i16,
            revents: i16,
        }
        extern "C" {
            fn poll(fds: *mut PollFd, count: usize, timeout: i32) -> i32;
        }
        let mut fd = PollFd {
            fd: self.hooks.writer.io.as_raw_fd(),
            events: 1, // POLLIN
            revents: 0,
        };
        // SAFETY: one live owned descriptor and a writable C pollfd for this call.
        let result = unsafe { poll(&mut fd, 1, 25) };
        if result < 0 {
            let error = io::Error::last_os_error();
            // EINTR ends this wait: the outer loop ticks using fresh clocks,
            // then starts a new bounded wait. Never retry with a stale sample.
            return if error.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(error)
            };
        }
        if fd.revents & (8 | 16 | 32) != 0 { // POLLERR | POLLHUP | POLLNVAL
            return Err(io::Error::other("MCU poll error/hangup/invalid descriptor"));
        }
        Ok(fd.revents & 1 != 0)
    }

    /// Open an explicitly selected MCU tty and configure 115200 raw 8N1.
    pub fn open_path(path: &Path, platform: P) -> io::Result<Self> {
        Ok(Self::new(open_configured(path)?, platform))
    }

    /// Resolve SE0 by MMIO, open it O_RDWR|O_NOCTTY and configure bridge-equal
    /// 115200 raw 8N1 framing before constructing the session.
    pub fn open_se0(sysfs_root: &Path, platform: P) -> io::Result<Self> {
        let path = resolve_port(sysfs_root, SE0_MMIO)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no tty fronts MCU SE0 MMIO 0x{}", SE0_MMIO),
            )
        })?;
        Ok(Self::new(open_configured(&path)?, platform))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcu::acc_go_fixtures::{FixtureStep, GO_FIXTURES};
    use crate::mcu::{checksum, DENY, TX_ACC_OFF_COMPLETE};
    use std::os::fd::FromRawFd;

    const O_RDWR: i32 = 0x0002;
    const O_CLOEXEC: i32 = 0x0008_0000;
    const O_NONBLOCK: i32 = 0x0800;
    const F_GETFL: i32 = 3;
    const F_SETFL: i32 = 4;

    extern "C" {
        fn posix_openpt(flags: i32) -> i32;
        fn grantpt(fd: i32) -> i32;
        fn unlockpt(fd: i32) -> i32;
        fn ptsname_r(fd: i32, buf: *mut u8, buflen: usize) -> i32;
        fn fcntl(fd: i32, command: i32, ...) -> i32;
    }

    fn pty_pair() -> (File, File) {
        // SAFETY: all calls use owned fds and a writable path buffer; each
        // successful raw fd is transferred exactly once into File.
        unsafe {
            let master_fd = posix_openpt(O_RDWR | O_NOCTTY | O_CLOEXEC);
            assert!(
                master_fd >= 0,
                "posix_openpt: {}",
                io::Error::last_os_error()
            );
            assert_eq!(
                grantpt(master_fd),
                0,
                "grantpt: {}",
                io::Error::last_os_error()
            );
            assert_eq!(
                unlockpt(master_fd),
                0,
                "unlockpt: {}",
                io::Error::last_os_error()
            );
            let mut path = [0u8; 128];
            assert_eq!(
                ptsname_r(master_fd, path.as_mut_ptr(), path.len()),
                0,
                "ptsname_r: {}",
                io::Error::last_os_error()
            );
            let end = path.iter().position(|byte| *byte == 0).unwrap();
            let slave_path = std::str::from_utf8(&path[..end]).unwrap();
            let slave = open_configured(Path::new(slave_path)).unwrap();
            (File::from_raw_fd(master_fd), slave)
        }
    }

    fn set_nonblocking(file: &File) {
        // SAFETY: fcntl reads/updates flags on this live fd.
        unsafe {
            let flags = fcntl(file.as_raw_fd(), F_GETFL);
            assert!(flags >= 0);
            assert_eq!(fcntl(file.as_raw_fd(), F_SETFL, flags | O_NONBLOCK), 0);
        }
    }

    fn read_all_available(file: &mut File) -> Vec<u8> {
        set_nonblocking(file);
        let mut out = Vec::new();
        let mut buf = [0u8; 128];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(count) => out.extend_from_slice(&buf[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("read pty master: {}", error),
            }
        }
        out
    }

    #[derive(Default)]
    struct RecordingPlatform {
        events: Vec<String>,
        cycle_events: Vec<CycleEvent>,
        observations: Vec<AccTxObservation>,
        trace: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
        discard_evidence: bool,
        trace_control: bool,
    }

    impl PlatformHooks for RecordingPlatform {
        fn on_cycle_event(&mut self, event: CycleEvent) {
            self.cycle_events.push(event);
            if self.trace_control {
                self.trace.borrow_mut().push(format!("control:{event:?}"));
            }
        }

        fn observe_acc_tx(&mut self, observation: AccTxObservation) {
            if !self.discard_evidence {
                self.trace.borrow_mut().push(format!("{:?}", observation.phase));
                self.observations.push(observation);
            }
        }

        fn enter_acc_off_grace(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn quiesce_radio(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn detach_stick_sink(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn quiesce_usb(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn resume(&mut self) -> io::Result<()> {
            self.trace.borrow_mut().push("resume".into());
            Ok(())
        }

        fn publish_acc_state(
            &mut self,
            state: State,
            reason: &'static str,
            now_ms: u64,
        ) -> io::Result<()> {
            self.events
                .push(format!("state:{state:?}:{reason}:{now_ms}"));
            self.trace.borrow_mut().push(format!("state:{state:?}"));
            Ok(())
        }

        fn request_sleep(&mut self, now_ms: u64) -> io::Result<()> {
            self.events.push(format!("sleep:{now_ms}"));
            self.trace.borrow_mut().push("sleep".into());
            Ok(())
        }
    }

    #[derive(Default)]
    struct TraceIo {
        input: io::Cursor<Vec<u8>>,
        written: Vec<u8>,
        trace: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
        fail_after: Option<usize>,
        write_zero: bool,
    }

    impl Read for TraceIo {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> { self.input.read(bytes) }
    }

    impl Write for TraceIo {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.trace.borrow_mut().push("write".into());
            let count = self.fail_after.map_or(bytes.len(), |n| n.saturating_sub(self.written.len()).min(bytes.len()));
            if count == 0 {
                return if self.write_zero { Ok(0) } else { Err(io::Error::from_raw_os_error(5)) };
            }
            self.written.extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> { panic!("must not add UART flush/drain") }
    }

    #[test]
    fn cycle_control_tags_real_parser_events_even_when_tx_diagnostics_are_dropped() {
        use crate::mcu::acc::HostWriteResult;
        use crate::mcu::acc_go_fixtures::{ACC_OFF, ACC_ON, FINAL_STAGE};
        let platform = RecordingPlatform { discard_evidence: true, trace_control: true, ..Default::default() };
        let io = TraceIo { trace: platform.trace.clone(), ..Default::default() };
        let mut session = AccSession::new(io, platform);
        // Duplicate OFF/final inputs must not duplicate control events.
        session.hooks.writer.io.input = io::Cursor::new([ACC_OFF, ACC_OFF].concat());
        session.read_once(0).unwrap();
        let off_generation = session.generation();
        session.hooks.writer.io.input = io::Cursor::new([FINAL_STAGE, FINAL_STAGE].concat());
        session.read_once(25_000).unwrap();
        session.advance(25_499).unwrap();
        assert_eq!(session.hooks.platform.cycle_events, [
            CycleEvent::Grace { off_generation },
            CycleEvent::Prepare { off_generation },
        ]);
        session.advance(25_500).unwrap();
        session.advance(25_501).unwrap();
        session.hooks.writer.io.input = io::Cursor::new([ACC_ON, ACC_ON].concat());
        session.read_once(30_000).unwrap();
        assert_eq!(session.generation(), off_generation + 1);
        assert_eq!(session.hooks.platform.cycle_events, [
            CycleEvent::Grace { off_generation },
            CycleEvent::Prepare { off_generation },
            CycleEvent::FinalWrite { off_generation, result: HostWriteResult::Complete },
            CycleEvent::Resume { off_generation },
        ]);
        assert!(session.hooks.platform.observations.is_empty());
        assert_eq!(session.hooks.writer.io.written, [TX_ACC_OFF_COMPLETE, TX_ACC_ON_ANSWER].concat());
        assert_eq!(*session.hooks.platform.trace.borrow(), [
            "control:Grace { off_generation: 1 }", "state:AccOffGrace",
            "control:Prepare { off_generation: 1 }", "state:FinalStage",
            "write", "control:FinalWrite { off_generation: 1, result: Complete }",
            "sleep", "state:Suspending",
            "write", "control:Resume { off_generation: 1 }", "resume", "state:Resumed",
        ]);
    }

    #[test]
    fn cycle_control_startup_and_pmic_wake_do_not_invent_or_end_another_cycle() {
        use crate::mcu::acc::HostWriteResult;
        use crate::mcu::acc_go_fixtures::{ACC_OFF, ACC_ON, FINAL_STAGE};
        let mut session = AccSession::new(TraceIo::default(), RecordingPlatform::default());
        let startup_token = session.wake_token();
        session.hooks.writer.io.input = io::Cursor::new([ACC_ON, ACC_ON, FINAL_STAGE].concat());
        session.read_once(0).unwrap();
        session.advance(500).unwrap();
        assert_eq!(session.state(), State::Resumed);
        assert!(session.hooks.platform.cycle_events.is_empty());
        assert!(session.hooks.writer.io.written.is_empty());
        assert!(!session.hardware_wake(startup_token, SuspendProof::Completed, 501).unwrap());

        session.hooks.writer.io.input = io::Cursor::new([ACC_OFF, FINAL_STAGE].concat());
        session.read_once(1_000).unwrap();
        let off_generation = session.generation();
        let first_off_token = session.wake_token();
        assert!(!session.hardware_wake(first_off_token, SuspendProof::Completed, 1_499).unwrap());
        session.advance(1_500).unwrap();
        assert!(session.hardware_wake(first_off_token, SuspendProof::Completed, 1_501).unwrap());
        assert_eq!(session.hooks.platform.cycle_events, [
            CycleEvent::Grace { off_generation },
            CycleEvent::Prepare { off_generation },
            CycleEvent::FinalWrite { off_generation, result: HostWriteResult::Complete },
            CycleEvent::Resume { off_generation },
        ]);
        assert_eq!(session.generation(), off_generation + 1);
        // A later Suspending state must not make the first cycle's token valid.
        session.hooks.writer.io.input = io::Cursor::new([ACC_OFF, FINAL_STAGE].concat());
        session.read_once(2_000).unwrap();
        let second_off_generation = session.generation();
        session.advance(2_500).unwrap();
        let before = session.hooks.platform.cycle_events.clone();
        assert!(!session.hardware_wake(first_off_token, SuspendProof::Completed, 40_000).unwrap());
        assert_eq!(session.hooks.platform.cycle_events, before);
        assert!(session.hardware_wake(session.wake_token(), SuspendProof::Completed, 40_001).unwrap());
        assert_eq!(session.hooks.platform.cycle_events.last(), Some(&CycleEvent::Resume { off_generation: second_off_generation }));
    }

    #[test]
    fn failed_final_control_survives_dropped_diagnostics_and_cancels_exact_cycle() {
        use crate::mcu::acc::HostWriteResult;
        use crate::mcu::acc_go_fixtures::{ACC_OFF, ACC_ON, FINAL_STAGE};
        let platform = RecordingPlatform { discard_evidence: true, ..Default::default() };
        let io = TraceIo { fail_after: Some(2), ..Default::default() };
        let mut session = AccSession::new(io, platform);
        session.hooks.writer.io.input = io::Cursor::new([ACC_OFF, FINAL_STAGE].concat());
        session.read_once(100).unwrap();
        // Exercise the actual read loop's due-callback path on an empty read.
        assert!(matches!(session.read_once(600), Err(SessionError::Io(error)) if error.raw_os_error() == Some(5)));
        session.advance(601).unwrap();
        assert_eq!(session.state(), State::FinalStage);
        session.hooks.writer.io.fail_after = None;
        session.hooks.writer.io.input = io::Cursor::new(ACC_ON.to_vec());
        session.read_once(602).unwrap();
        assert_eq!(session.hooks.platform.cycle_events, [
            CycleEvent::Grace { off_generation: 1 },
            CycleEvent::Prepare { off_generation: 1 },
            CycleEvent::FinalWrite { off_generation: 1, result: HostWriteResult::Failed },
            CycleEvent::Resume { off_generation: 1 },
        ]);
        assert!(session.hooks.platform.observations.is_empty());
        assert!(!session.hooks.platform.events.iter().any(|event| event.starts_with("sleep:")));
        assert_eq!(session.generation(), 2);
    }

    #[test]
    fn runtime_tx_evidence_preserves_bytes_deadline_and_control_order() {
        use crate::mcu::acc_go_fixtures::{ACC_OFF, ACC_ON, FINAL_STAGE};
        let platform = RecordingPlatform::default();
        let io = TraceIo { trace: platform.trace.clone(), ..Default::default() };
        let mut session = AccSession::new(io, platform);
        session.hooks.writer.io.input = io::Cursor::new(ACC_OFF.to_vec());
        session.read_once(0).unwrap();
        session.hooks.writer.io.input = io::Cursor::new(FINAL_STAGE.to_vec());
        session.read_once(25_000).unwrap();
        session.hooks.platform.trace.borrow_mut().clear();
        session.advance(25_499).unwrap();
        assert!(session.hooks.platform.observations.is_empty());
        session.advance(25_500).unwrap();
        assert_eq!(*session.hooks.platform.trace.borrow(), ["Attempted", "write", "WriteComplete", "sleep", "state:Suspending"]);
        session.hooks.platform.trace.borrow_mut().clear();
        session.hooks.writer.io.input = io::Cursor::new(ACC_ON.to_vec());
        session.read_once(30_000).unwrap();
        assert_eq!(*session.hooks.platform.trace.borrow(), ["Attempted", "write", "WriteComplete", "resume", "state:Resumed"]);
        assert_eq!(session.hooks.writer.io.written, [TX_ACC_OFF_COMPLETE, TX_ACC_ON_ANSWER].concat());
        assert_eq!(session.hooks.platform.observations[0], AccTxObservation { tx: Tx::AccOffComplete, phase: AccTxPhase::Attempted, loop_ms: 25_500 });
        assert_eq!(session.hooks.platform.observations[1].to_string(), "mcu-acc-tx name=acc-off-complete payload=05-15 loop_ms=25500 deferred=1 phase=write-complete boundary=write_all-return mcu_ack=unobserved");
        assert_eq!(session.hooks.platform.observations[2].loop_ms, 30_000);
    }

    #[test]
    fn gps_ready_uses_the_fixed_gated_writer_and_reports_partial_failure() {
        let mut session = AccSession::new(TraceIo::default(), RecordingPlatform::default());
        session.send_gps_ready().unwrap();
        assert_eq!(session.hooks.writer.io.written, [0x5a, 0xa5, 0x02, 0x64, 0x9c]);
        for prefix in [0, 2] {
            let io = TraceIo { fail_after: Some(prefix), ..Default::default() };
            let mut failed = AccSession::new(io, RecordingPlatform::default());
            assert!(failed.send_gps_ready().is_err());
            assert_eq!(failed.hooks.writer.io.written, TX_GPS_READY[..prefix]);
        }
    }

    #[test]
    fn parsed_acc_lifecycle_drives_gps_ready_without_early_or_off_writes() {
        use crate::mcu::acc_go_fixtures::{ACC_OFF, ACC_ON};
        use crate::mcu::gps_ready::Scheduler;
        let mut session = AccSession::new(TraceIo::default(), RecordingPlatform::default());
        let mut scheduler = Scheduler::default();
        let tick = |session: &mut AccSession<TraceIo, RecordingPlatform>, scheduler: &mut Scheduler,
                    now_ms: u64, ready: bool| {
            if scheduler.poll(1, session.generation(), session.state(), ready, now_ms).is_some() {
                session.send_gps_ready().unwrap();
            }
        };
        tick(&mut session, &mut scheduler, 0, false);
        session.send_startup_acc_on_answer().unwrap();
        tick(&mut session, &mut scheduler, 100, true);
        tick(&mut session, &mut scheduler, 1099, true);
        assert_eq!(session.hooks.writer.io.written, [TX_ACC_ON_ANSWER, TX_GPS_READY].concat());
        // OFF at the repeat deadline is parsed before servicing the schedule.
        session.hooks.writer.io.input = io::Cursor::new(ACC_OFF.to_vec());
        session.read_once(1100).unwrap();
        tick(&mut session, &mut scheduler, 1100, true);
        assert_eq!(session.hooks.writer.io.written, [TX_ACC_ON_ANSWER, TX_GPS_READY].concat());
        session.hooks.writer.io.input = io::Cursor::new(ACC_ON.to_vec());
        session.read_once(2000).unwrap();
        tick(&mut session, &mut scheduler, 2000, true);
        tick(&mut session, &mut scheduler, 2999, true);
        tick(&mut session, &mut scheduler, 3000, true);
        tick(&mut session, &mut scheduler, 9000, true);
        assert_eq!(session.hooks.writer.io.written,
            [TX_ACC_ON_ANSWER, TX_GPS_READY, TX_ACC_ON_ANSWER, TX_GPS_READY, TX_GPS_READY].concat());
    }

    #[test]
    fn observed_gate_refusal_has_zero_writes_and_original_error() {
        let mut writer = TxWriter::new(TraceIo::default());
        for (command, sub, _) in DENY {
            let payload = [*command, *sub];
            let frame = [0x5a, 0xa5, 0x03, payload[0], payload[1], checksum(&payload)];
            let mut phases = Vec::new();
            let result = writer.send_observed(&frame, |phase| phases.push(phase));
            assert!(matches!(result, Err(SessionError::Refused(ref reason)) if Some(reason) == may_transmit(&frame).err().as_ref()));
            assert_eq!(phases, [AccTxPhase::Attempted, AccTxPhase::Refused]);
        }
        assert!(writer.io.written.is_empty());
        assert!(writer.io.trace.borrow().is_empty());
    }

    #[test]
    fn incomplete_final_write_never_queues_sleep_or_retries_and_acc_on_recovers() {
        use crate::mcu::acc_go_fixtures::{ACC_OFF, ACC_ON, FINAL_STAGE};
        for (prefix, write_zero) in [(0, false), (2, false), (0, true), (2, true)] {
            let platform = RecordingPlatform::default();
            let io = TraceIo { trace: platform.trace.clone(), fail_after: Some(prefix), write_zero, ..Default::default() };
            let mut session = AccSession::new(io, platform);
            session.hooks.writer.io.input = io::Cursor::new(ACC_OFF.to_vec());
            session.read_once(0).unwrap();
            session.hooks.writer.io.input = io::Cursor::new(FINAL_STAGE.to_vec());
            session.read_once(25_000).unwrap();
            let error = session.advance(25_500).unwrap_err();
            let (kind, errno) = if write_zero {
                (io::ErrorKind::WriteZero, None)
            } else {
                (io::Error::from_raw_os_error(5).kind(), Some(5))
            };
            assert!(matches!(error, SessionError::Io(error) if error.kind() == kind && error.raw_os_error() == errno));
            assert_eq!(session.hooks.writer.io.written, TX_ACC_OFF_COMPLETE[..prefix]);
            assert_eq!(session.state(), State::FinalStage);
            assert_eq!(session.hooks.platform.cycle_events, [
                CycleEvent::Grace { off_generation: 1 },
                CycleEvent::Prepare { off_generation: 1 },
                CycleEvent::FinalWrite { off_generation: 1, result: super::super::acc::HostWriteResult::Failed },
            ]);
            assert_eq!(session.hooks.platform.events, ["state:AccOffGrace:mcu-12-03:0", "state:FinalStage:mcu-05-10:25000"]);
            let observation = session.hooks.platform.observations[1];
            assert_eq!(observation.phase, AccTxPhase::WriteError { kind, errno });
            assert!(observation.to_string().ends_with("delivery=unknown"));
            assert!(!observation.to_string().contains("sent_bytes"));
            // The original error is consumed, but repeated timers and duplicate
            // OFF/final frames must not re-arm or silently retry the completion.
            session.hooks.writer.io.fail_after = None;
            session.advance(25_501).unwrap();
            session.hooks.writer.io.input = io::Cursor::new([ACC_OFF, FINAL_STAGE].concat());
            session.read_once(26_000).unwrap();
            session.advance(60_000).unwrap();
            assert_eq!(session.hooks.platform.observations.len(), 2);
            assert_eq!(session.state(), State::FinalStage);
            assert!(!session.hardware_wake(session.wake_token(), SuspendProof::Completed, 60_001).unwrap());
            session.hooks.writer.io.input = io::Cursor::new(ACC_ON.to_vec());
            session.read_once(60_002).unwrap();
            assert_eq!(session.state(), State::Resumed);
            assert_eq!(session.hooks.writer.io.written, [&TX_ACC_OFF_COMPLETE[..prefix], TX_ACC_ON_ANSWER].concat());
            assert!(!session.hooks.platform.events.iter().any(|event| event.starts_with("sleep:") || event.contains("Suspending")));
            assert_eq!(session.hooks.platform.cycle_events.len(), 4);
            assert_eq!(session.hooks.platform.cycle_events.last(), Some(&CycleEvent::Resume { off_generation: 1 }));
            // Only a fresh OFF cycle may attempt another final completion.
            session.hooks.writer.io.input = io::Cursor::new([ACC_OFF, FINAL_STAGE].concat());
            session.read_once(61_000).unwrap();
            session.advance(61_500).unwrap();
            assert_eq!(session.state(), State::Suspending);
            assert_eq!(session.hooks.platform.events.iter().filter(|event| event.starts_with("sleep:")).count(), 1);
        }
    }

    #[test]
    fn lost_observation_cannot_become_a_protocol_failure() {
        let platform = RecordingPlatform { discard_evidence: true, ..Default::default() };
        let mut session = AccSession::new(TraceIo::default(), platform);
        session.hooks.transmit(Tx::AccOffComplete);
        session.hooks.transmit(Tx::AccOnAnswer);
        session.hooks.finish().unwrap();
        assert!(session.hooks.platform.observations.is_empty());
        assert_eq!(session.hooks.writer.io.written, [TX_ACC_OFF_COMPLETE, TX_ACC_ON_ANSWER].concat());
    }

    #[test]
    fn config_is_bridge_equal_115200_raw_8n1_without_flow_control() {
        let (_master, slave) = pty_pair();
        let mut termios = KernelTermios::default();
        // SAFETY: same live writable UAPI object as configure_uart.
        assert_eq!(
            unsafe {
                ioctl(
                    slave.as_raw_fd(),
                    TCGETS,
                    &mut termios as *mut KernelTermios,
                )
            },
            0
        );
        assert_eq!(termios.iflag, 0);
        assert_eq!(termios.oflag, 0);
        assert_eq!(termios.lflag, 0);
        assert_eq!(termios.cflag & CBAUD, B115200);
        assert_eq!(termios.cflag & CSIZE, CS8);
        assert_eq!(termios.cflag & (PARENB | PARODD | CSTOPB | CRTSCTS), 0);
        assert_eq!(termios.cflag & (CREAD | CLOCAL), CREAD | CLOCAL);
        assert_eq!(termios.cc[VMIN], 0);
        assert_eq!(termios.cc[VTIME], 10);
    }

    /// Read the live termios of `fd` through the same TCGETS the production
    /// setup uses, so a test asserts on what the kernel actually holds.
    fn termios_of(fd: RawFd) -> KernelTermios {
        let mut termios = KernelTermios::default();
        // SAFETY: same live writable UAPI object as configure_raw_8n1.
        assert_eq!(
            unsafe { ioctl(fd, TCGETS, &mut termios as *mut KernelTermios) },
            0,
            "TCGETS: {}",
            io::Error::last_os_error()
        );
        termios
    }

    /// The CAN half of the shared `configure_raw_8n1` contract, asserted on a
    /// real pty through the PRODUCTION open (`can::port::open_readonly`) —
    /// not by re-running the setup by hand.
    ///
    /// It lives beside the MCU's 115200 test because both are properties of
    /// one function, and because the UAPI constants are declared here.
    #[test]
    fn can_port_config_is_raw_8n1_at_38400_without_flow_control() {
        let (_master, slave_name) = test_support::pty_pair_with_name();
        let port = crate::can::port::open_readonly(Path::new(&slave_name), 38_400)
            .expect("production CAN open on a pty slave");
        let termios = termios_of(port.as_raw_fd());
        let b38400 = crate::can::port::cbaud_for(38_400).unwrap();
        assert_eq!(b38400, 0x0f, "the CBAUD code for 38400 is B38400");
        assert_eq!(termios.iflag, 0, "no input processing");
        assert_eq!(termios.oflag, 0, "no output processing");
        assert_eq!(termios.lflag, 0, "not a canonical/echoing line discipline");
        assert_eq!(termios.cflag & CBAUD, b38400, "38400, not the MCU's 115200");
        assert_eq!(termios.cflag & CSIZE, CS8, "8 data bits");
        assert_eq!(
            termios.cflag & (PARENB | PARODD | CSTOPB | CRTSCTS),
            0,
            "no parity, one stop bit, and NO hardware flow control: SE5 is              two-wire (TLMM 16/17), exactly like SE0"
        );
        assert_eq!(termios.cflag & (CREAD | CLOCAL), CREAD | CLOCAL);
        assert_eq!(termios.cc[VMIN], 0, "VMIN preserved");
        assert_eq!(termios.cc[VTIME], 10, "VTIME preserved: a silent box still ticks");
    }

    /// The regression that makes the shared helper worth having: the MCU port
    /// and a CAN port, both opened by their own production function on their
    /// own pty, must produce termios that differ in NOTHING but the baud bits.
    ///
    /// A future edit that touches one port's setup and not the other's turns
    /// this red. Written for `configure_uart` -> `configure_raw_8n1(fd, cbaud)`.
    #[test]
    fn mcu_and_can_configurations_differ_only_in_the_baud_bits() {
        let (_m1, mcu_slave) = test_support::pty_pair_with_name();
        let mcu = open_configured(Path::new(&mcu_slave)).expect("production MCU open");
        let (_m2, can_slave) = test_support::pty_pair_with_name();
        let can = crate::can::port::open_readonly(Path::new(&can_slave), 38_400)
            .expect("production CAN open");

        let a = termios_of(mcu.as_raw_fd());
        let b = termios_of(can.as_raw_fd());
        assert_eq!(a.iflag, b.iflag);
        assert_eq!(a.oflag, b.oflag);
        assert_eq!(a.lflag, b.lflag);
        assert_eq!(a.cc, b.cc, "VMIN/VTIME and every other control char agree");
        assert_eq!(
            a.cflag ^ b.cflag,
            B115200 ^ crate::can::port::cbaud_for(38_400).unwrap(),
            "the two ports may differ in the baud bits and nothing else \
             (mcu cflag {:#010x}, can cflag {:#010x})",
            a.cflag,
            b.cflag
        );
        assert_eq!(a.cflag & CBAUD, B115200, "the MCU keeps 115200");
    }

    #[test]
    fn go_fixtures_cross_a_real_pty_codec_machine_and_gated_writer() {
        let expected_states: &[&[State]] = &[
            &[
                State::AccOffGrace,
                State::FinalStage,
                State::FinalStage,
                State::Suspending,
                State::Resumed,
                State::Resumed,
            ],
            &[
                State::AccOffGrace,
                State::FinalStage,
                State::Resumed,
                State::Resumed,
                State::Resumed,
            ],
            &[
                State::AccOffGrace,
                State::FinalStage,
                State::FinalStage,
                State::Suspending,
            ],
            &[State::Idle, State::Resumed],
        ];

        for (fixture_index, fixture) in GO_FIXTURES.iter().enumerate() {
            let (mut master, slave) = pty_pair();
            let mut session = AccSession::new(slave, BridgeHooksTodo);
            let mut states = Vec::new();

            for step in fixture.steps {
                match step {
                    FixtureStep::Frame { at_ms, bytes } => {
                        master.write_all(bytes).unwrap();
                        assert_ne!(session.read_once(*at_ms).unwrap(), 0);
                    }
                    FixtureStep::Advance { at_ms } => session.advance(*at_ms).unwrap(),
                    FixtureStep::UartReopened => {
                        session.uart_reopened().unwrap();
                    }
                }
                states.push(session.state());
            }

            let expected_bytes: Vec<u8> = fixture
                .expected_tx
                .iter()
                .flat_map(|frame| frame.iter().copied())
                .collect();
            assert_eq!(
                read_all_available(&mut master),
                expected_bytes,
                "fixture {} bytes out",
                fixture.name
            );
            assert_eq!(
                states, expected_states[fixture_index],
                "fixture {} state transitions",
                fixture.name
            );
        }
    }

    #[test]
    fn startup_query_and_acc_answer_cross_the_pty_gated_writer() {
        let (mut master, slave) = pty_pair();
        let mut session = AccSession::new(slave, BridgeHooksTodo);
        session.send_version_query().unwrap();
        session.send_startup_acc_on_answer().unwrap();
        let expected: Vec<u8> = TX_VERSION_QUERY
            .iter()
            .chain(TX_ACC_ON_ANSWER.iter())
            .copied()
            .collect();
        assert_eq!(read_all_available(&mut master), expected);
    }

    #[test]
    fn pty_final_stage_waits_500ms_then_queues_sleep_before_suspending_publication() {
        use crate::mcu::acc_go_fixtures::{ACC_OFF, FINAL_STAGE};

        let (mut master, slave) = pty_pair();
        let mut session = AccSession::new(slave, RecordingPlatform::default());
        master.write_all(ACC_OFF).unwrap();
        session.read_once(0).unwrap();
        master.write_all(FINAL_STAGE).unwrap();
        session.read_once(25_000).unwrap();
        session.advance(25_499).unwrap();
        assert!(read_all_available(&mut master).is_empty());
        session.advance(25_500).unwrap();
        assert_eq!(read_all_available(&mut master), TX_ACC_OFF_COMPLETE);
        assert_eq!(
            session.hooks.platform.events,
            [
                "state:AccOffGrace:mcu-12-03:0",
                "state:FinalStage:mcu-05-10:25000",
                "sleep:25500",
                "state:Suspending:timer:25500",
            ]
        );
    }

    #[test]
    fn pty_acc_on_cancels_pending_final_stage_without_sleep_request() {
        use crate::mcu::acc_go_fixtures::{ACC_OFF, ACC_ON, FINAL_STAGE};

        let (mut master, slave) = pty_pair();
        let mut session = AccSession::new(slave, RecordingPlatform::default());
        master.write_all(ACC_OFF).unwrap();
        session.read_once(0).unwrap();
        master.write_all(FINAL_STAGE).unwrap();
        session.read_once(25_000).unwrap();
        master.write_all(ACC_ON).unwrap();
        session.read_once(25_499).unwrap();
        session.advance(25_500).unwrap();
        assert_eq!(read_all_available(&mut master), TX_ACC_ON_ANSWER);
        assert!(!session
            .hooks
            .platform
            .events
            .iter()
            .any(|event| event.starts_with("sleep:")));
        assert_eq!(session.state(), State::Resumed);
        assert_eq!(session.generation(), 2);
        assert_eq!(session.hooks.platform.cycle_events, [
            CycleEvent::Grace { off_generation: 1 },
            CycleEvent::Prepare { off_generation: 1 },
            CycleEvent::Resume { off_generation: 1 },
        ]);
    }

    #[test]
    fn diagnostics_real_reader_retains_version_without_changing_dispatch() {
        use crate::mcu::diagnostics::Diagnostics;
        let payload = [0x33, 0, 0x23, 0, 0x7d, 0xfe, 9];
        let mut frame = vec![0x5a, 0xa5, 8];
        frame.extend(payload);
        frame.push(crate::mcu::checksum(&payload));
        let (mut master, slave) = pty_pair();
        let mut session = AccSession::new(slave, BridgeHooksTodo);
        session.set_diagnostic_session(7);
        master.write_all(&[0xff, 0xfe, 0x5a, 0xa5, 8]).unwrap();
        session.read_once(123).unwrap();
        let partial = session.diagnostics();
        assert_eq!((partial.read_calls, partial.rx_bytes, partial.frames, partial.discarded, partial.pending), (1, 5, 0, 2, 3));
        master.write_all(&frame[3..]).unwrap();
        let mut seen = Vec::new();
        session.read_once_observed(124, |f, v| seen.push((f.raw().to_vec(), v))).unwrap();
        assert_eq!(seen, [(frame, FrameValidity::Valid)]);
        assert_eq!(session.state(), State::Idle);
        assert!(read_all_available(&mut master).is_empty(), "diagnostics must not transmit");
        let diagnostic = session.diagnostics();
        assert_eq!((diagnostic.read_calls, diagnostic.frames, diagnostic.valid, diagnostic.pending), (2, 1, 1, 0));
        assert_eq!(diagnostic.records[0].version, Some((0x2300, 0x09fe7d00)));
        let shared = Diagnostics::default();
        shared.publish(diagnostic);
        let mut output = Vec::new();
        shared.dump(2000, &mut output, &[]).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("session=7"));
        assert!(output.contains("kind=version"));
    }

    #[test]
    fn diagnostics_real_reader_counts_empty_and_failed_reads() {
        let mut empty = AccSession::new(io::Cursor::new(Vec::<u8>::new()), BridgeHooksTodo);
        empty.read_once(42).unwrap();
        let snapshot = empty.diagnostics();
        assert_eq!((snapshot.read_calls, snapshot.rx_bytes, snapshot.zero_reads, snapshot.frames), (1, 0, 1, 0));
        struct FailedRead;
        impl Read for FailedRead {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> { Err(io::Error::from_raw_os_error(5)) }
        }
        impl Write for FailedRead {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> { panic!("unexpected TX") }
            fn flush(&mut self) -> io::Result<()> { panic!("unexpected flush") }
        }
        let mut failed = AccSession::new(FailedRead, BridgeHooksTodo);
        assert!(failed.read_once(43).is_err());
        let snapshot = failed.diagnostics();
        assert_eq!((snapshot.read_calls, snapshot.read_errors, snapshot.last_errno, snapshot.last_read_ms), (1, 1, Some(5), 43));
        assert_eq!(failed.state(), State::Idle);
    }

    #[test]
    fn copied_device1_frames_cross_the_observed_pty_reader_loop() {
        use crate::mcu::device1_fixtures::DEVICE1_BUTTON_FRAMES;

        let (mut master, slave) = pty_pair();
        let mut session = AccSession::new(slave, BridgeHooksTodo);
        for fixture in DEVICE1_BUTTON_FRAMES {
            master.write_all(fixture.bytes).unwrap();
            let mut observed = Vec::new();
            session
                .read_once_observed(123, |frame, validity| {
                    observed.push((frame.raw().to_vec(), validity));
                })
                .unwrap();
            assert_eq!(
                observed,
                vec![(fixture.bytes.to_vec(), FrameValidity::Valid)],
                "device-1 fixture {}",
                fixture.name
            );
        }
        let diagnostic = session.diagnostics();
        assert_eq!(diagnostic.frames, DEVICE1_BUTTON_FRAMES.len() as u64);
        assert!(diagnostic.records[..diagnostic.retained].iter().all(|r| r.kind == "key-redacted" && r.state.is_none() && r.version.is_none()));
    }

    #[test]
    fn bad_checksum_frame_is_observed_but_never_decoded_into_the_machine() {
        use crate::mcu::device1_fixtures::DEVICE1_BUTTON_FRAMES;

        let (mut master, slave) = pty_pair();
        let mut session = AccSession::new(slave, BridgeHooksTodo);
        let mut corrupt = DEVICE1_BUTTON_FRAMES[0].bytes.to_vec();
        *corrupt.last_mut().unwrap() ^= 1;
        master.write_all(&corrupt).unwrap();
        let mut observed = Vec::new();
        session
            .read_once_observed(123, |frame, validity| {
                observed.push((frame.raw().to_vec(), validity));
            })
            .unwrap();
        assert_eq!(observed, vec![(corrupt, FrameValidity::BadChecksum)]);
        assert_eq!(session.state(), State::Idle);
        assert_eq!((session.diagnostics().frames, session.diagnostics().bad), (1, 1));
        assert_eq!(session.diagnostics().records[0].version, None);
    }

    #[test]
    fn every_forbidden_shape_is_refused_at_the_pty_write_site() {
        let (mut master, slave) = pty_pair();
        let mut writer = TxWriter::new(slave);
        for (command, sub, _) in DENY {
            let payload = [*command, *sub];
            let frame = [0x5a, 0xa5, 0x03, payload[0], payload[1], checksum(&payload)];
            assert!(matches!(writer.send(&frame), Err(SessionError::Refused(_))));
        }
        assert!(read_all_available(&mut master).is_empty());
    }
}
