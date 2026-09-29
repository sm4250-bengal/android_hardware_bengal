/* SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.dsp;

/** Last successful writes in this daemon boot; never an independent chip readback. */
@VintfStability
parcelable DspEnhancementSnapshot {
    boolean bassApplied;
    int bassLevel;
    boolean trebleApplied;
    int trebleLevel;
    boolean loudnessApplied;
    /** Last applied mode. False means program default, not a proven hard bypass. */
    boolean loudnessEnabled;
    /** Latest valid policy INPUT; can differ from the last applied basis after failure. */
    boolean volumeIndexKnown;
    int volumeIndex;
    /** -1 for program default/unapplied, otherwise the last successful 0..63 basis. */
    int loudnessAppliedVolumeIndex;
    /** Shared counter of successful DSP writes; input-only notifications do not advance it. */
    long generation;
    String lastError;
}
