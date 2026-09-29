/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

/** Physical TLMM-85 role selection. */
@VintfStability
@Backing(type="int")
enum UsbMode {
    UNKNOWN = 0,
    HOST = 1,
    DEVICE = 2,
}
