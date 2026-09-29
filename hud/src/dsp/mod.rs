// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! ADAU1701 DSP. **Placeholder: constants and refusals only.**
//!
//! Nothing here writes. The DSP is the subsystem this project has already
//! damaged once: an EQ derivation that was armed without hearing it wrote nine
//! cells that were not EQ cells at all, clobbered the persistent IC60
//! parameter image, and produced beeping, cracking and screeching
//! (`mem:dsp-adau1701-daemon-app`). The lesson recorded there — "derivation
//! quality is not authorisation" — is why this module ships as constants until
//! Phase 4c has a by-ear treatment for each address.

use crate::i2c::ADAU1701_ADDRESS;

/// The typed, exclusive `/dev/hu-audio-guard` owner used by the Android
/// service adapter.  It intentionally does not expose raw GPIO, I2C, DSP
/// address, or coefficient operations.
pub mod guard;

/// The IDsp V2 control path — validation, the last-applied model and the
/// exception mapping. Deliberately free of `#[cfg(feature = "binder")]` so
/// `cargo test --offline` covers everything that can be got wrong.
pub mod controls;

/// The two post-success FM diagnostic tells and their ordering.
pub mod tells;

/// Android-only typed IDsp adapter. It remains in this process so reads and
/// future armed writes cannot race a second ADAU1701 owner.
#[cfg(feature = "binder")]
pub mod binder;

/// Android-only resolved `/dev/i2c-3` parameter-read transport.
#[cfg(feature = "binder")]
pub mod readback;

/// The DSP's address on the `i2c-dsp` bus (TLMM 29/30).
///
/// That bus is `status = "disabled"` in our board DT
/// (`kernel/dts/sm4250-xyauto-hu.dts:204-218`) until Phase 4c enables it
/// together with the codec module, so [`crate::i2c::adapter::DSP_NEEDLES`]
/// resolves to nothing today. That is expected, not a fault.
pub const ADDRESS: u16 = ADAU1701_ADDRESS;

/// Confirmed by ear on device 1 and safe to port: mute and gain.
pub mod armed {
    /// Mute, command 0x08, leader 0x60.
    pub const MUTE_CELLS: [u16; 4] = [0x012c, 0x0132, 0x012e, 0x0134];
    /// Gain, command 0x05.
    pub const GAIN_CELLS: [u16; 4] = [0x0114, 0x011a, 0x0116, 0x011c];
}

/// NOT armed, and each absence is deliberate.
pub mod disarmed {
    /// The four real EQ cells on this SoC (`CpuType=1`), spaced by five
    /// because a second-order biquad is five consecutive words.
    ///
    /// Correct as addresses; still not written by this daemon. The 17-band
    /// figure that caused the incident came from the library's platform-0
    /// path, which this product does not use.
    pub const EQ_CELLS: [u16; 4] = [0x0092, 0x0097, 0x009c, 0x00a1];
}

/// Fixed point is 5.23, biquad order `b0,b1,b2,-a1,-a2`, fs = 48 kHz — pinned
/// by reproducing the OEM's own captured 340 Hz Butterworth high-pass to 5e-7,
/// and independently corroborated by MCUdude's SigmaDSP library.
pub const FRACTIONAL_BITS: u32 = 23;

/// Sample rate the coefficient maths assumes.
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// Convert to the chip's 5.23 fixed point.
pub fn to_5_23(value: f64) -> i32 {
    (value * (1i32 << FRACTIONAL_BITS) as f64).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unity_is_the_oem_neutral_value() {
        // The OEM neutral band is 00800000 ... -- i.e. b0 = 1.0 in 5.23.
        assert_eq!(to_5_23(1.0), 0x0080_0000);
        assert_eq!(to_5_23(0.0), 0);
        assert_eq!(to_5_23(-1.0), -0x0080_0000);
    }

    #[test]
    fn eq_cells_are_five_apart() {
        // The spacing is what makes the four-cell finding and a standard
        // second-order biquad layout mutually consistent. If someone "fixes"
        // these addresses to be contiguous, this fails.
        let cells = disarmed::EQ_CELLS;
        for pair in cells.windows(2) {
            assert_eq!(pair[1] - pair[0], 5);
        }
    }
}
