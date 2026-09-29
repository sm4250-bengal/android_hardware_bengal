/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

import vendor.xyauto.hud.McuKeyAction;

/** One decoded MCU button/knob event. The app owns Android policy/injection. */
@VintfStability
parcelable McuKeyEvent {
    long sequence;
    long elapsedRealtimeMs;
    int rawCode;
    String name;
    McuKeyAction action;
    /** Android KeyEvent code suggested by the daemon's measured mapping. */
    int androidKeyCode;
    /** Rotary acceleration 1..15; one for ordinary button events. */
    int magnitude;
}
