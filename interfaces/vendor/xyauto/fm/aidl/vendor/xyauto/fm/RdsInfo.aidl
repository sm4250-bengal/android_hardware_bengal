/*
 * Copyright (C) 2026 The XYAUTO device-3 project
 * SPDX-License-Identifier: Apache-2.0
 */
package vendor.xyauto.fm;

/**
 * Decoded RDS for the current channel.
 *
 * The daemon does the decoding; the app never sees blocks. QN8035 RDS lives in
 * registers 0x0b..0x12 (blocks A..D) with STATUS2 at 0x13 carrying
 * blkerr[3:0], RDSSYNC[4] and RDS_RXUPD[7]. The daemon accepts a group only
 * when RDSSYNC is set AND blkerr is zero -- measured on device 1: a station
 * gives SNR 31-32 / RDSSYNC 1 / blkerr 0, empty air SNR 6 / RDSSYNC 0.
 */
@VintfStability
parcelable RdsInfo {
    /**
     * Programme Identification, block A. 0 when never synced.
     * Measured example from a real station on device 1: 0x8412.
     */
    int programmeIdentification;

    /**
     * Programme Service name, group 0A, eight characters assembled from four
     * two-character segments indexed by blockB[1:0].
     *
     * EMPTY UNTIL COMPLETE. A partially assembled PS is the classic RDS bug:
     * segments arrive out of order and a display that shows them as they land
     * flickers through garbage. The daemon only publishes a PS once all four
     * segments have been seen for the same PI since the last tune.
     */
    String programmeService;

    /** True while RDSSYNC has been set continuously since the last tune. */
    boolean synchronised;
}
