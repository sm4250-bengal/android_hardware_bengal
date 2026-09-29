/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

/** Semantic action decoded from the MCU key family. */
@VintfStability
@Backing(type="int")
enum McuKeyAction {
    UNKNOWN = 0,
    PRESS = 1,
    RELEASE = 2,
    ROTATE_LEFT = 3,
    ROTATE_RIGHT = 4,
}
