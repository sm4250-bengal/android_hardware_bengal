// SPDX-License-Identifier: Apache-2.0
//! Read-only parameter transport through HUD's sole audio-guard descriptor.
//! The kernel brackets the complete snapshot with physical output mute and
//! serializes it against ACC and source writes. No raw-I2C fallback exists.

use std::io;
use std::sync::{Arc, Mutex};
use super::guard::{AudioGuard, ABI_VERSION, FEATURE_MUTED_READ};
use xyauto_dsp::adau1701::{ParameterRamBus, I2C_ADDRESS, PARAMETER_WORDS};

/// Physical adapter behind the guard's bound I2C client, for identity reports.
pub const DEFAULT_ADAPTER: &str = "/dev/i2c-3";

/// Parameter-RAM transport sharing the sole audio-guard owner and its mute scope.
pub struct LinuxParameterRamBus {
    guard: Arc<Mutex<AudioGuard>>,
}

impl LinuxParameterRamBus {
    /// Reuse the existing owner; never open a second guard or raw adapter.
    pub fn open(guard: Arc<Mutex<AudioGuard>>) -> io::Result<Self> {
        let status = guard.lock().map_err(|_| io::Error::other("audio guard mutex poisoned"))?.status();
        let supported = status.raw.is_some_and(|raw| raw.abi_version == ABI_VERSION &&
            raw.reserved[0] & FEATURE_MUTED_READ != 0);
        if !supported {
            return Err(io::Error::other("kernel-muted parameter read unavailable; update audio guard"));
        }
        Ok(Self { guard })
    }
}

impl ParameterRamBus for LinuxParameterRamBus {
    type Error = io::Error;

    fn read_parameter_words(&mut self, chip_address: u16, start_word: u16,
                            output: &mut [u8]) -> io::Result<()> {
        if chip_address != I2C_ADDRESS || output.is_empty() || output.len() % 4 != 0 ||
            output.len() / 4 > PARAMETER_WORDS ||
            usize::from(start_word) + output.len() / 4 > PARAMETER_WORDS {
            return Err(io::Error::from_raw_os_error(22));
        }
        self.guard.lock().map_err(|_| io::Error::other("audio guard mutex poisoned"))?
            .read_parameter_words(start_word, output)
            .map_err(|error| io::Error::other(error.to_string()))
    }

    fn adapter_label(&self) -> String { DEFAULT_ADAPTER.to_owned() }
}
