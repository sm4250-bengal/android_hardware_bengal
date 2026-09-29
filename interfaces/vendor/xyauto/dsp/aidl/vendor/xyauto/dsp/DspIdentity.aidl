/*
 * Copyright (C) 2026 The XYAUTO device-3 project
 * SPDX-License-Identifier: Apache-2.0
 */
package vendor.xyauto.dsp;

/** Configured DSP identity and the bounded readback capability. */
@VintfStability
parcelable DspIdentity {
    /** True when bengal-hud owns the resolved I2C adapter descriptor. */
    boolean available;
    /** Stable open/identity failure; empty only when available. */
    String unavailableReason;
    /** Board-qualified part this service is allowed to address. */
    String chipModel;
    /** Resolved adapter node, expected to be /dev/i2c-3 on device 3. */
    String i2cAdapter;
    /** Seven-bit address, 0x34 for the ADAU1701. */
    int i2cAddress;
    /** Parameter RAM contains 1024 addressable words. */
    int parameterWordCount;
    /** One ADAU1701 parameter word occupies four transfer bytes. */
    int parameterWordBytes;
    /** Maximum words accepted by one readParameterRam transaction. */
    int maxReadWords;
    /** Minimum spacing charged between attempted hardware reads. */
    int minReadIntervalMs;
}
