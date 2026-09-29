/* SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.dsp;

/** Fixed IC60 enhancements. No caller-controlled frequency, Q or coefficient. */
@VintfStability
parcelable DspEnhancementCapabilities {
    /** Actual additive kernel feature, false with an older ABI-5 guard. */
    boolean supported;
    /** Current readiness, not a guarantee against a later route transition. */
    boolean writable;
    /** -32 and 32: OEM request levels, NOT decibels. */
    int minLevel;
    int maxLevel;
    /** Fixed shelf design frequencies 100 and 5000 Hz, core rate 48000 Hz. */
    int bassFrequencyHz;
    int trebleFrequencyHz;
    int sampleRateHz;
    /** Normalized policy basis 0..63, independent of the legacy master uAPI. */
    int volumeIndexMin;
    int volumeIndexMax;
    /** Disabled restores IC60 coefficient 0x00c00000 (1.5), NOT hard bypass. */
    boolean disabledRestoresProgramDefault;
}
