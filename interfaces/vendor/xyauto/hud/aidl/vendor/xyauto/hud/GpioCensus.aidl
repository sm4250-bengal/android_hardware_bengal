/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

import vendor.xyauto.hud.GpioLineInfo;

/** Read-only TLMM census. No V1 method can drive a census line. */
@VintfStability
parcelable GpioCensus {
    String chip;
    String chipLabel;
    GpioLineInfo[] lines;
    String[] warnings;
}
