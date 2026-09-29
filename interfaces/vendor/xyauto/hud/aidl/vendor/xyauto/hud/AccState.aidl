/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

/** Confirmed daemon ACC lifecycle state, never a raw MCU byte. */
@VintfStability
@Backing(type="int")
enum AccState {
    UNKNOWN = 0,
    ACC_ON = 1,
    ACC_OFF_GRACE = 2,
    FINAL_STAGE = 3,
    SUSPENDING = 4,
}
