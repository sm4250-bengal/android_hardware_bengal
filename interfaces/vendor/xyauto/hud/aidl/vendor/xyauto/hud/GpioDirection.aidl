/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

/** Direction reported by GPIO_V2_GET_LINEINFO. */
@VintfStability
@Backing(type="int")
enum GpioDirection {
    UNKNOWN = 0,
    INPUT = 1,
    OUTPUT = 2,
}
