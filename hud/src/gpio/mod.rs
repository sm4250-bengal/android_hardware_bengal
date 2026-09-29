// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Board GPIO inventory and the narrowly-scoped GPIO chardev-v2 output owner.
//!
//! Every number below was read out of device 3's OWN live stock device tree
//! (`docs/device/2026-08-28-stock-first-look/stock-live.dts`) on 2026-08-30 —
//! not carried across from device 1. The ADAU node describes its pins as
//! thirteen named pinctrl states rather than `strap-*-gpios`, which is why
//! grepping for "strap" returns nothing and reads as absence.

pub mod chardev;
pub mod census;

/// Curated TLMM wiring inventory; not a claim of current ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line {
    /// TLMM offset.
    pub offset: u32,
    /// What it does.
    pub what: &'static str,
    /// Where the number comes from.
    pub source: &'static str,
}

/// The audio path, from `stock-live.dts:17932`'s `adau1701@34` pinctrl states.
pub const AUDIO: &[Line] = &[
    Line {
        offset: 36,
        what: "ADAU analogue mux strap A (a0=low, a1=high)",
        source: "adau1701_pins_a0/a1",
    },
    Line {
        offset: 37,
        what: "ADAU analogue mux strap B (b0=low, b1=high)",
        source: "adau1701_pins_b0/b1",
    },
    Line {
        offset: 83,
        what: "ESS9080 DAC enable",
        source: "ess9080_pins_en0/en1",
    },
    Line {
        offset: 93,
        what: "amplifier enable",
        source: "amp_pins_en0/en1",
    },
    Line {
        offset: 106,
        what: "ADAU 3V3 enable",
        source: "adau1701_pins_3v3en0/3v3en1",
    },
    Line {
        offset: 107,
        what: "ADAU reset",
        source: "adau1701_pins_rst0/rst1",
    },
];

/// The FM power-sequencing glue, from device 3's stock `fragment@38`.
pub const FM_POWER: &[Line] = &[
    Line {
        offset: 96,
        what: "fm_power_ctrl",
        source: "stock fragment@38",
    },
    Line {
        offset: 84,
        what: "fmic_power_ctrl",
        source: "stock fragment@38",
    },
    Line {
        offset: 54,
        what: "fm_reset_ctrl",
        source: "stock fragment@38",
    },
    Line {
        offset: 52,
        what: "voice_mute_ctrl",
        source: "stock fragment@38",
    },
];

/// USB, measured on our own kernel (Phase 4g).
pub const USB: &[Line] = &[
    Line {
        offset: 34,
        what: "OTG jack VBUS-out (NOT the hub 5 V)",
        source: "device-3 bengal_usb_standby board-enable-gpios",
    },
    Line {
        offset: 85,
        what: "host/device mux (LOW = host)",
        source: "usb_hub_lines_default",
    },
];

/// The TLMM gpiochip's label on device 3.
///
/// **`500000.pinctrl`, not `400000.pinctrl`.** Device 1's bridge hard-codes
/// the latter (`gpio.go:168`) and would silently fall back to
/// `/dev/gpiochip0` here. Resolve the chip by this label, the same way the I2C
/// adapter is resolved by name.
pub const TLMM_CHIP_LABEL: &str = "500000.pinctrl";

/// Lines this daemon must never drive, with the reason.
pub const REFUSED: &[(u32, &str)] = &[
    (
        82,
        "panel reset -- pulsing it is the black screen (D3-0037..0043)",
    ),
    (35, "backlight enable, active low, held on through bring-up"),
    (19, "ICN6202 bridge reset"),
    (71, "touch reset"),
    (31, "touch VDD"),
];

/// Is this line one the daemon refuses to touch?
pub fn is_refused(offset: u32) -> Option<&'static str> {
    REFUSED
        .iter()
        .find(|(o, _)| *o == offset)
        .map(|(_, why)| *why)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mux_straps_are_36_and_37_on_device_3() {
        // Measured on THIS board, not inherited from device 1 -- which happens
        // to agree, but that is a result, not an assumption.
        let a = AUDIO.iter().find(|l| l.what.contains("strap A")).unwrap();
        let b = AUDIO.iter().find(|l| l.what.contains("strap B")).unwrap();
        assert_eq!((a.offset, b.offset), (36, 37));
    }

    #[test]
    fn panel_and_touch_lines_are_refused() {
        assert!(is_refused(82).is_some(), "the panel reset must be refused");
        assert!(is_refused(35).is_some());
        assert!(is_refused(71).is_some());
        // ...and the lines we DO own are not refused
        for line in AUDIO.iter().chain(FM_POWER).chain(USB) {
            assert!(is_refused(line.offset).is_none(), "{:?}", line);
        }
    }

    #[test]
    fn tlmm_label_is_device_3s_not_device_1s() {
        assert_eq!(TLMM_CHIP_LABEL, "500000.pinctrl");
        assert_ne!(TLMM_CHIP_LABEL, "400000.pinctrl");
    }
}
