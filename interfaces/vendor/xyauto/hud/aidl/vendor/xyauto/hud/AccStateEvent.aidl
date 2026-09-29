/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

import vendor.xyauto.hud.AccState;

/** One confirmed ACC transition. */
@VintfStability
parcelable AccStateEvent {
    long sequence;
    long elapsedRealtimeMs;
    AccState state;
    String reason;
}
