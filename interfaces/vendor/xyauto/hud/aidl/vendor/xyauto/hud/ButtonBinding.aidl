/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

import vendor.xyauto.hud.ButtonSource;
import vendor.xyauto.hud.ButtonSystemAction;

/**
 * One learned button -> Android key mapping. The APP owns learning; the daemon
 * owns injection. This parcelable is the whole contract between them.
 *
 * WHY A BEZEL BINDING IS A RANGE AND NOT A POINT. The same physical button does
 * not report the same y twice. Measured across two passes on two different
 * boots, 2026-09-04:
 *
 * ```text
 *   POWER        y  94, 112, 124, 124
 *   HOME         y 187, 187, 189, 190, 191, 192
 *   BACK         y 263, 264, 268, 276, 279
 *   VOLUME UP    y 346, 347, 353, 354, 361
 *   VOLUME DOWN  y 428, 433, 433, 434, 438
 * ```
 *
 * The worst spread inside one band was 16 px and the bands are ~50 px apart, so
 * ranges separate cleanly -- but a binding stored as a single y would match
 * almost nothing. The daemon matches a press when yMin <= y <= yMax.
 *
 * These numbers are evidence for the SHAPE of the binding. They are deliberately
 * NOT compiled in as defaults: the mapping is per-unit and is learned, and a
 * hard-coded table would defeat the feature it is evidence for.
 */
@VintfStability
parcelable ButtonBinding {
    /** Which transport this binding matches against. */
    ButtonSource source;

    /** MCU only: exact raw code to match. Zero for BEZEL. */
    int rawCode;

    /**
     * BEZEL only: inclusive y range, in raw touchscreen coordinates. Zero for MCU.
     *
     * The daemon rejects a binding whose range overlaps another BEZEL binding:
     * two buttons that claim the same contact would make injection depend on
     * table order, which is not something an owner can debug.
     */
    int yMin;
    int yMax;

    /** The Android key code to inject. Zero means "learned but not assigned". */
    int androidKeyCode;

    /**
     * Owner-facing name for this button, for the app's own UI and for logs.
     * The daemon treats it as opaque and never parses it.
     */
    String label;

    /** V3: mutually exclusive with a nonzero androidKeyCode. */
    ButtonSystemAction systemAction = ButtonSystemAction.NONE;
}
