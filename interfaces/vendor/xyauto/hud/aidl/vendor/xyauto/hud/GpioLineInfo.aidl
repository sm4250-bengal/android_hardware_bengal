/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

import vendor.xyauto.hud.GpioDirection;

/** One non-mutating GPIO line-info/census row. */
@VintfStability
parcelable GpioLineInfo {
    int offset;
    String name;
    String consumer;
    GpioDirection direction;
    /** Physical level, or -1 when the read-only sources cannot report one. */
    int value;
    boolean used;
    boolean activeLow;
    String label;
    String meaning;
    String group;
    String function;
    String source;
    /** True only when the daemon itself owns this line. Not a write grant. */
    boolean daemonOwned;
}
