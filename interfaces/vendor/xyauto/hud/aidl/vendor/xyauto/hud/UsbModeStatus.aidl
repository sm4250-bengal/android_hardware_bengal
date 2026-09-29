/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

import vendor.xyauto.hud.UsbMode;

/** Last requested USB role and its application result. */
@VintfStability
parcelable UsbModeStatus {
    UsbMode mode;
    String chip;
    long appliedElapsedRealtimeMs;
    String error;
}
