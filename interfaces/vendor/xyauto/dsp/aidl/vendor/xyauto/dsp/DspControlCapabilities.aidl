/*
 * Copyright (C) 2026 The XYAUTO device-3 project
 * SPDX-License-Identifier: Apache-2.0
 */
package vendor.xyauto.dsp;

/**
 * What this build's control path can actually do, and in which units.
 *
 * Every number here is pinned from a source in this repository — the kernel
 * guard's validation (`xyauto_audio_guard.c:970-1045`) and its coefficient
 * tables (`xyauto_audio_guard_coefficients.h`). **A unit that cannot be derived
 * from a source is reported as 0 = UNKNOWN, never guessed**, and a caller must
 * treat 0 as "do not print a physical unit for this control" rather than as a
 * zero-sized step.
 *
 * Note what "derived" means here: the dB figures below are properties of the
 * SHIPPED COEFFICIENT TABLE the guard writes, computed from that table. They
 * are not acoustic measurements on device 3, and nothing in this interface
 * measures a speaker.
 */
@VintfStability
parcelable DspControlCapabilities {
    /**
     * True when the guard node is open, its ABI matches, the IC60 program is
     * loaded and no fault is latched — i.e. a write attempted now would reach
     * the chip. False makes every setter fail with EX_ILLEGAL_STATE; it is a
     * live condition, not a build property, and it changes across ACC cycles.
     */
    boolean writable;
    /** 5. The logical outputs of DspOutput. */
    int outputCount;
    /** 4. SM4250 EQ bands at `0x0092 + 5 * band`. */
    int eqBandCount;

    /** 0. Lowest accepted delay request step. */
    int delayMinSteps;
    /** 145. Kernel bound: `value < 0 || value > 145` is -EINVAL. */
    int delayMaxSteps;
    /**
     * **0 = UNKNOWN, and deliberately so.** The physical duration of one delay
     * step is NOT proven on device 3.
     *
     * The hypothesis, recorded here so it is not re-derived from scratch and
     * not mistaken for a result: the guard writes `steps + 1` into the ADAU
     * delay cell (`xyauto_audio_guard.c:1016-1018`), the ADAU1701 delay block
     * counts whole samples, and this program's core rate is pinned at 48 kHz
     * in `hud/src/dsp/mod.rs` (by reproducing the OEM's own captured 340 Hz
     * Butterworth high-pass to 5e-7). Device 1's shipped UI captions the same
     * OEM control "N samples / N/48.0 ms". If all of that holds, one step is
     * 20.8333 us and the full 0..145 range is 0..3.02 ms — about a metre of
     * path length. None of it has been confirmed against device-3 hardware,
     * and 20.8333 us is not a whole number of microseconds anyway, so this
     * field stays 0 until something measures it.
     */
    int delayStepMicros;

    /** -200. Index 0 of `output_gain_coeff[401]`, -20.07 dB. */
    int gainMinSteps;
    /** 200. Index 400 of `output_gain_coeff[401]`, +20.07 dB. */
    int gainMaxSteps;
    /**
     * 10. One output-gain step is 0.1 dB = 10 millibels.
     *
     * CONFIRMED from the shipped curve rather than assumed: reading
     * `output_gain_coeff[401]` as 5.23 against unity `0x00800000`, index 200
     * is exactly unity (0 dB), the mean step over all 400 intervals is
     * 0.100343 dB (min 0.100335, max 0.100350), and the endpoints are
     * -20.0687 and +20.0687 dB. An Android millibel is 1/100 dB, so a tenth of
     * a decibel is 10 mB — 100 would claim this DSP swings +/-200 dB.
     */
    int gainStepMillibels;

    /** -20. Kernel bound for `HU_AUDIO_DSP_SM4250_EQ` request steps. */
    int eqMinSteps;
    /** +20. Kernel bound for `HU_AUDIO_DSP_SM4250_EQ` request steps. */
    int eqMaxSteps;
    /**
     * 2. Request steps per distinct filter: the OEM encoder truncates
     * `step / 2`, so consecutive pairs of steps write identical coefficients.
     */
    int eqStepsPerFilter;
    /**
     * 21. Distinct filters per band across the 41 requestable steps. Verified
     * against the shipped table by counting unique 5-word rows: exactly 21 in
     * every band, with -1/0/+1 all the bypass row and only +/-20 unshared.
     */
    int eqDistinctFilters;
    /**
     * 25 millibels per FILTER step — NOT per request step.
     *
     * Derived by walking every distinct filter of every band for its response
     * peak (5.23 biquads, word order `b0,b1,b2,-a1,-a2` settled by pole
     * radius, fs 48 kHz). Filter index `trunc(step/2)` runs -10..+10 and its
     * peak gain is 0.25 dB times that index, to within 0.00087 dB worst case
     * across all four bands and all 41 steps.
     *
     * The average request-step density is 12.5 mB, but the actual response
     * is quantized: use integer division TRUNCATING TOWARD ZERO to compute
     * `(steps / eqStepsPerFilter) * eqStepMillibels`. Do not use floating
     * division or `steps * 12.5`: -1/0/+1 all select bypass, and 2/3 both
     * select +25 mB. Mark the table-derived response approximate, while
     * always giving identical filters identical displayed gains.
     */
    int eqStepMillibels;
    /**
     * 250. The full span of DspEqBand.steps: +/-20 request steps is exactly
     * +/-2.500 dB in the shipped table (10 filter steps x 25 mB). This is the
     * number that makes "the EQ is gentle" visible in a UI instead of a
     * surprise; +/-20 is not +/-20 dB.
     */
    int eqFullScaleMillibels;
    /**
     * {20, 96, 470, 2281}. DERIVED FROM `sm4250_eq_coeff`, NOT MEASURED ON
     * DEVICE 3: each band's maximum-boost cell was walked for its response
     * peak, giving 20.2 / 96.0 / 470.1 / 2281.5 Hz at +2.500 dB. Independently
     * equal to device 1's shipped labels. These are the shipped FILTERS'
     * centres — what a microphone in this car would report is a different
     * question that nothing here answers.
     */
    int[] eqBandCentreHz;

    /** False. SUBWOOFER (guard index 2) has no delay cell; -EINVAL. */
    boolean subwooferDelaySupported;
}
