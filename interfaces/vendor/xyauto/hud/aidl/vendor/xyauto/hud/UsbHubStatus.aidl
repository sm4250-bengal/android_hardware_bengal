/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

import vendor.xyauto.hud.HubPortState;

/** Outcome of one bounded hub watchdog pass. */
@VintfStability
parcelable UsbHubStatus {
    String trigger;
    boolean healthy;
    int pulses;
    HubPortState[] ports;
    String[] present;
    String[] seen;
    String expected;
    boolean flapping;
    long carrierChanges;
    String flapReason;
    boolean budgetExhausted;
    String error;
}
