// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Desk-only runner for the daemon's CAN reader against a real tty, a pty or a
//! file: the same `select_can_path` → `open_readonly` → `Reader::run` chain
//! `binder.rs` uses, with stderr as the sink and a deadline instead of forever.
//! `platform/scripts/hud-can-desk-test.sh` drives it over a socat pty pair.
//! Not built by Soong.
//!
//! Exit codes: 0 ran to the deadline; 2 usage; 3 port selection REFUSED
//! (sysfs proved the configured name is SE0/SE3, or SE5 is ambiguous); 4 the
//! port could not be opened; 5 the read loop failed.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use bengal_hud::can::capture::Capture;
use bengal_hud::can::port::open_readonly;
use bengal_hud::can::profile::Profile;
use bengal_hud::can::reader::{Reader, Sink};
use bengal_hud::can::{select_can_path_under, PortAuthority, DEFAULT_DEVICE, DEV_DIR, SE5_MMIO};

struct Stderr;

impl Sink for Stderr {
    fn line(&mut self, error: bool, message: &str) {
        eprintln!("{}{}", if error { "E " } else { "I " }, message);
    }
}

struct Args {
    device: PathBuf,
    dev_dir: PathBuf,
    sysfs_root: PathBuf,
    profile: Option<PathBuf>,
    capture: Option<PathBuf>,
    input: Option<PathBuf>,
    baud: u32,
    exit_after: Option<Duration>,
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: bengal-can-capture [--device PATH] [--dev-dir DIR] [--sysfs-root DIR] \
         [--profile FILE] [--capture FILE] [--input FILE] [--baud N] [--exit-after-ms N]"
    );
    ExitCode::from(2)
}

fn parse_args() -> Result<Args, ExitCode> {
    let mut args = Args {
        device: PathBuf::from(DEFAULT_DEVICE),
        dev_dir: PathBuf::from(DEV_DIR),
        sysfs_root: PathBuf::from("/sys"),
        profile: None,
        capture: None,
        input: None,
        baud: 38_400,
        exit_after: None,
    };
    let mut it = env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(usage)?;
        match flag.as_str() {
            "--device" => args.device = PathBuf::from(value),
            // The directory the accepted-fallback shape rule is anchored on.
            // Production is /dev; the socat harness points it at its scratch
            // pty directory so the SAME rule is exercised end to end instead of
            // being bypassed for the test.
            "--dev-dir" => args.dev_dir = PathBuf::from(value),
            "--sysfs-root" => args.sysfs_root = PathBuf::from(value),
            "--profile" => args.profile = Some(PathBuf::from(value)),
            "--capture" => args.capture = Some(PathBuf::from(value)),
            "--input" => args.input = Some(PathBuf::from(value)),
            "--baud" => args.baud = value.parse().map_err(|_| usage())?,
            "--exit-after-ms" => {
                args.exit_after = Some(Duration::from_millis(value.parse().map_err(|_| usage())?))
            }
            _ => return Err(usage()),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(code) => return code,
    };
    let mut sink = Stderr;

    let profile = match &args.profile {
        Some(path) => match std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|t| Profile::parse(&t).map_err(|e| e.to_string())) {
            Ok(p) => {
                sink.line(false, &format!("canuart: profile {} loaded id={} messages={} verified={}", path.display(), p.id, p.messages.len(), p.verified));
                Some(p)
            }
            Err(e) => {
                sink.line(true, &format!("canuart: profile {} REFUSED ({e}); running envelope-only", path.display()));
                None
            }
        },
        None => None,
    };
    let baud = profile.as_ref().map_or(args.baud, |p| p.baud);
    let capture = match &args.capture {
        Some(path) => match Capture::at(path) {
            Ok(c) => Some(c),
            Err(e) => {
                sink.line(true, &format!("canuart: capture {} FAILED ({e}); continuing without", path.display()));
                None
            }
        },
        None => None,
    };

    let started = Instant::now();
    let clock = || started.elapsed().as_millis() as u64;
    let deadline = args.exit_after;
    let stop = || deadline.is_some_and(|d| started.elapsed() >= d);

    if let Some(input) = &args.input {
        let file = match std::fs::File::open(input) {
            Ok(f) => f,
            Err(e) => {
                sink.line(true, &format!("canuart: open {} FAILED ({e})", input.display()));
                return ExitCode::from(4);
            }
        };
        let mut reader = Reader::new(profile, capture, input.display().to_string(), "file", baud);
        // A file hits EOF (Ok(0)) at once; run until the deadline or, without
        // one, a single pass.
        let once = deadline.is_none();
        let passes = std::cell::Cell::new(0u32);
        let stop_file = || {
            passes.set(passes.get() + 1);
            stop() || (once && passes.get() > 1)
        };
        if let Err(e) = reader.run(file, clock, &mut sink, stop_file) {
            sink.line(true, &format!("canuart: read loop FAILED ({e})"));
            return ExitCode::from(5);
        }
        sink.line(false, &reader.tell(clock()));
        return ExitCode::SUCCESS;
    }

    let (path, authority) =
        match select_can_path_under(&args.dev_dir, &args.sysfs_root, &args.device) {
            Ok(selected) => selected,
            Err(reason) => {
                sink.line(true, &format!("canuart: tty selection REFUSED root={} dev-dir={} configured-fallback={} reason={reason}", args.sysfs_root.display(), args.dev_dir.display(), args.device.display()));
                return ExitCode::from(3);
            }
        };
    sink.line(
        false,
        &format!(
            "canuart: tty selected authority={} path={} configured-fallback={}",
            match authority {
                PortAuthority::Se5Mmio => "se5-mmio",
                // UNPROVEN: sysfs proved nothing about this name. It is not
                // SE5 identity; it only survived the SE0/SE3 refusal and the
                // /dev/ttyHS<N> shape check.
                PortAuthority::NameFallback => "name-fallback-UNPROVEN",
            },
            path.display(),
            args.device.display()
        ),
    );
    let port = match open_readonly(&path, baud) {
        Ok(p) => p,
        Err(e) => {
            sink.line(true, &format!("canuart: open {} read-only FAILED ({e})", path.display()));
            return ExitCode::from(4);
        }
    };
    let mut reader = Reader::new(profile, capture, path.display().to_string(), SE5_MMIO, baud);
    if let Some(cap) = reader.capture_path() {
        sink.line(false, &format!("canuart: capturing to {}", cap.display()));
    }
    if let Err(e) = reader.run(port, clock, &mut sink, stop) {
        sink.line(true, &format!("canuart: read loop FAILED ({e})"));
        return ExitCode::from(5);
    }
    sink.line(false, &reader.tell(clock()));
    ExitCode::SUCCESS
}
