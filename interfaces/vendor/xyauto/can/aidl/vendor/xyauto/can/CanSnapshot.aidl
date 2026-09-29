// SPDX-License-Identifier: Apache-2.0
package vendor.xyauto.can;
@VintfStability
parcelable CanSnapshot {
    String sessionId;
    boolean enabled;
    String state;
    String detail;
    long bytesReceived;
    long validFrames;
    long badFrames;
    long discardedBytes;
    /** -1 before first byte. */
    long lastRxAgeMs;
    long oldestSequence;
    long latestSequence;
    /** Live ring overrun/reset; does not indicate disk capture loss. */
    boolean gap;
    String captureName;
    /** active / off / stopped (budget or IO failure). RX can continue. */
    String captureState;
    /** JSON objects with process-local sequence, UTC time, event and payload.
     * rx rows are chunks, not extra frames. Decoded fields may be unverified.
     * At most 32 records of 8192 UTF8 bytes each. */
    String[] records;
}
