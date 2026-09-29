/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

/** Stable high-level event classes; wire frames are deliberately absent. */
@VintfStability
@Backing(type="int")
enum HudEventType {
    UNKNOWN = 0,
    ACC = 1,
    MCU_KEY = 2,
    USB_MODE = 3,
    USB_HUB = 4,
    GPIO = 5,
    WARNING = 6,
    /** V2: a physical bezel or MCU button press. */
    BUTTON = 7,
}
