/*
 * Copyright (C) 2026 The XYAUTO device-3 project
 * SPDX-License-Identifier: Apache-2.0
 */
package vendor.xyauto.fm;

/** Cached tuner state. Reading it never touches the I2C bus. */
@VintfStability
parcelable TunerState {
    /** setEnabled() succeeded and the tuner is powered and un-muted. */
    boolean enabled;

    /** Current channel in kHz, or 0 when never tuned. */
    int frequencyKHz;

    /** A startSeek() is in progress. */
    boolean seeking;

    /**
     * Which adapter the daemon resolved, e.g. "/dev/i2c-1 (i2c-fm)".
     *
     * IN THE INTERFACE ON PURPOSE. The bus number is allocation order and
     * MOVES: the FM bus was i2c-0 on D3-0015 and i2c-1 on D3-0043, same board.
     * A wrong bus does not fail loudly -- it addresses 0x10 somewhere else and
     * reports the NAK as "no tuner". Putting the resolved adapter where a
     * bugreport can see it turns that silent class of failure into a readable
     * line.
     */
    String i2cAdapter;
}
