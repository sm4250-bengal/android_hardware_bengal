// SPDX-License-Identifier: Apache-2.0
//! Parse the entire command line before starting any service or hardware owner.

use std::ffi::OsString;
use std::path::PathBuf;

/// Side-effect-free daemon command.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Print usage without opening Binder, files, UARTs or GPIOs.
    Help,
    /// Start the daemon with the configured MCU enumeration fallback.
    Run(Option<PathBuf>),
}

/// User-facing usage; the MMIO resolver still owns actual MCU selection.
pub const USAGE: &str = "Usage: bengal-hud [--mcu-device /dev/ttyNAME]\n\
    bengal-hud --help\n\
Normally started by Android init; only one hardware owner may run.";

/// Reject unknown, duplicate and incomplete options, including trailing ones.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let args: Vec<_> = args.into_iter().collect();
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        return Ok(Command::Help);
    }
    let mut mcu = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg != "--mcu-device" {
            return Err(format!("unknown option: {}", arg.to_string_lossy()));
        }
        if mcu.is_some() {
            return Err("duplicate --mcu-device".into());
        }
        let path = args
            .next()
            .map(PathBuf::from)
            .ok_or("--mcu-device requires an absolute path")?;
        if !path.is_absolute() {
            return Err("--mcu-device requires an absolute path".into());
        }
        mcu = Some(path);
    }
    Ok(Command::Run(mcu))
}

/// Dispatch only a completely parsed run command to the hardware entry point.
pub fn execute(
    args: impl IntoIterator<Item = OsString>,
    run: impl FnOnce(Option<PathBuf>) -> Result<(), String>,
) -> u8 {
    match parse(args) {
        Ok(Command::Help) => {
            println!("{USAGE}");
            0
        }
        Ok(Command::Run(path)) => match run(path) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("bengal-hud: {error}");
                1
            }
        },
        Err(error) => {
            eprintln!("bengal-hud: {error}\n{USAGE}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn default_and_init_arguments_preserve_the_configuration() {
        assert_eq!(parse([]), Ok(Command::Run(None)));
        assert_eq!(
            parse(["--mcu-device", "/dev/ttyHS1"].map(OsString::from)),
            Ok(Command::Run(Some(PathBuf::from("/dev/ttyHS1"))))
        );
    }

    #[test]
    fn help_never_enters_the_service() {
        for flag in ["--help", "-h"] {
            assert_eq!(
                execute([OsString::from(flag)], |_| panic!("hardware entry")),
                0
            );
        }
    }

    #[test]
    fn invalid_arguments_never_enter_the_service() {
        for args in [
            vec!["--unknown"],
            vec!["--mcu-device"],
            vec!["--mcu-device", ""],
            vec!["--mcu-device", "--help"],
            vec!["--mcu-device", "ttyHS1"],
            vec!["--mcu-device", "/dev/ttyHS1", "--unknown"],
            vec!["--mcu-device", "/dev/ttyHS1", "--mcu-device", "/dev/ttyHS2"],
            vec!["--help", "--unknown"],
        ] {
            assert_eq!(
                execute(args.into_iter().map(OsString::from), |_| panic!(
                    "hardware entry"
                )),
                2
            );
        }
    }

    #[test]
    fn valid_run_enters_once_and_reports_startup_failure() {
        let calls = Cell::new(0);
        assert_eq!(
            execute([], |path| {
                assert_eq!(path, None);
                calls.set(calls.get() + 1);
                Err("already owned".into())
            }),
            1
        );
        assert_eq!(calls.get(), 1);
    }
}
