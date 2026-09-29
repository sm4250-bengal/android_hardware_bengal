/*
 * Copyright (C) 2026 The XYAUTO device-3 project
 * SPDX-License-Identifier: Apache-2.0
 */
package vendor.xyauto.fm;

/** Raw QN8035 signal registers, unscaled. */
@VintfStability
parcelable SignalQuality {
    /** Register 0x02, estimated RF input carrier-to-noise ratio. */
    int snr;

    /** Register 0x03, in-band signal strength. */
    int rssi;

    /**
     * True when snr clears the station floor the daemon seeks on (20 on
     * device 1). Exposed as a boolean so the app never re-implements the
     * threshold and the two can never disagree about what "a station" is.
     */
    boolean stationPresent;
}
