/*
 * Copyright (C) 2026 The XYAUTO device-3 project
 * SPDX-License-Identifier: Apache-2.0
 */
package vendor.xyauto.fm;

import vendor.xyauto.fm.RdsInfo;

/**
 * Asynchronous FM events. One registered client only (the HU service app).
 *
 * Every callback is one-way: a blocked client must never stall the daemon's
 * bus thread. The tuner is polled, and a poll that queues behind a slow binder
 * reply is a poll that misses an RDS group.
 */
@VintfStability
oneway interface IFmRadioCallback {
    /** A seek finished on a station. */
    void onSeekComplete(int frequencyKHz);

    /** stopSeek(), or the band was walked without clearing the SNR floor. */
    void onSeekStopped(boolean bandExhausted);

    /**
     * A COMPLETE programme service name, or new PI. Never fires for a partial
     * PS -- see RdsInfo.programmeService.
     */
    void onRdsUpdate(in RdsInfo rds);

    /**
     * The daemon lost the tuner: an I2C transaction failed, or the chip ID
     * stopped reading 0x84. The app should show FM as unavailable rather than
     * a frozen last-known station.
     */
    void onTunerLost(String reason);
}
