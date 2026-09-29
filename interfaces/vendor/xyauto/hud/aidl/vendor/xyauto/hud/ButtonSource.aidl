/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

/**
 * Where a physical button press was observed.
 *
 * Device 3 has exactly one strip of five bezel buttons, but two possible
 * transports for them, and which one is live is a MEASURED property of the
 * unit rather than a design choice:
 *
 *  - MCU: a decoded UART key frame. This is what the hardware documentation
 *    describes and what device-1/2 use.
 *  - BEZEL: a touchscreen contact off the left edge of the panel. This is what
 *    device 3 actually does -- measured twice on 2026-09-04, both passes, all
 *    five buttons, with the MCU sending nothing at all.
 *
 * The owner-facing UI must never show this. It exists because a diagnostic
 * view needs it, and because a unit whose MCU starts answering later must not
 * require a new interface version to be understood.
 */
@VintfStability
@Backing(type="int")
enum ButtonSource {
    UNKNOWN = 0,
    MCU = 1,
    BEZEL = 2,
}
