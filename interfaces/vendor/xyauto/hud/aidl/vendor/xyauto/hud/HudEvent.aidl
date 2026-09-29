/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

import vendor.xyauto.hud.HudEventType;

/** Diagnostic event retained in the daemon's bounded oldest-first ring. */
@VintfStability
parcelable HudEvent {
    long sequence;
    long elapsedRealtimeMs;
    HudEventType type;
    String command;
    String state;
    String reason;
    String error;
}
