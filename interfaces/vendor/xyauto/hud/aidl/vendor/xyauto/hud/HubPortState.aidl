/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

/** One internal `1-1-portN/disable` node and its direct child. */
@VintfStability
parcelable HubPortState {
    int portNumber;
    String sysfsName;
    boolean enabled;
    String devicePath;
    String vendorId;
    String productId;
    boolean logStick;
}
