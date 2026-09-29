/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

/** Narrow destination for a committed physical button binding. */
@VintfStability
@Backing(type="int")
enum ButtonSystemAction {
    NONE = 0,
    SHOW_GLOBAL_ACTIONS = 1,
}
