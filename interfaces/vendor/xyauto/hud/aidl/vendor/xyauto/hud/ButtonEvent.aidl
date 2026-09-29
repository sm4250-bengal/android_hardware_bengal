/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

import vendor.xyauto.hud.ButtonSource;
import vendor.xyauto.hud.ButtonSystemAction;
import vendor.xyauto.hud.McuKeyAction;

/**
 * One physical button press as OBSERVED, before any naming or policy.
 *
 * This is deliberately not McuKeyEvent. An MCU key has a raw code and no
 * position; a bezel press has a position and no raw code. Forcing a bezel
 * contact into McuKeyEvent would mean inventing a rawCode, and an invented
 * identifier is exactly what the learn flow exists to avoid.
 *
 * Fields that do not apply to the reporting source are zero. Which ones apply
 * is fixed by `source` and is documented per field rather than left to the
 * reader, because "0" and "not applicable" are otherwise indistinguishable.
 */
@VintfStability
parcelable ButtonEvent {
    /** Monotonic per-daemon-lifetime counter. Resets when the daemon restarts. */
    long sequence;

    /** CLOCK_BOOTTIME milliseconds, so it stays sane across suspend. */
    long elapsedRealtimeMs;

    /** Which transport saw it. Never shown to the owner. */
    ButtonSource source;

    /** MCU only: the raw key code from the frame. Zero for BEZEL. */
    int rawCode;

    /**
     * BEZEL only: contact position in raw touchscreen coordinates. Zero for MCU.
     *
     * On device 3 a bezel contact is always at NEGATIVE x -- measured -39..-25 --
     * because the buttons sit off the left edge of the digitiser. `y` is what
     * distinguishes one button from another.
     */
    int x;
    int y;

    /** PRESS or RELEASE. The rotary actions never occur on this path. */
    McuKeyAction action;

    /**
     * How long the button was held, filled in on RELEASE only; zero on PRESS.
     *
     * Present from V2 so that binding a long press does not need a V3. The
     * capture instrument already records long presses, so the data exists even
     * though the first UI ships short-press only.
     */
    int pressDurationMs;

    /**
     * The Android key this press is currently bound to, or zero if unbound.
     *
     * Zero is the normal state before setup and is not an error: an unbound
     * button is a button the owner has not taught yet.
     */
    int androidKeyCode;

    /** V3: descriptive binding at observation time; reading events never acts. */
    ButtonSystemAction systemAction = ButtonSystemAction.NONE;
}
