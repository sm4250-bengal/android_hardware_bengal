/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

import vendor.xyauto.hud.AccState;

/** Cached daemon status; reading it performs no device I/O. */
@VintfStability
parcelable HudStatus {
    long startedElapsedRealtimeMs;
    boolean mcuConnected;
    long frameCount;
    long validFrames;
    long invalidFrames;
    long lastEventSequence;
    AccState accState;
}
