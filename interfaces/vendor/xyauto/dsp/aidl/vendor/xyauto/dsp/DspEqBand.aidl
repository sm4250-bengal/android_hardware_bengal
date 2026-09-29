/*
 * Copyright (C) 2026 The XYAUTO device-3 project
 * SPDX-License-Identifier: Apache-2.0
 */
package vendor.xyauto.dsp;

/**
 * One SM4250 EQ band's last successfully applied request step, as recorded by
 * this daemon. Like DspOutputState this is the model, not a chip readback.
 */
@VintfStability
parcelable DspEqBand {
    /**
     * Band index 0..3. The shipped CpuType-1 program has exactly four bands,
     * five consecutive parameter words each, at `0x0092 + 5 * band`.
     * Derived centre frequencies are in DspControlCapabilities.eqBandCentreHz.
     */
    int band;
    /** False until THIS daemon wrote this band in THIS boot. */
    boolean applied;
    /**
     * Last applied uAPI REQUEST STEP, -20..+20. Not decibels, and not one
     * filter per step.
     *
     * The OEM encoder truncates `step / 2` (`AdauMap.kt:181-200`,
     * `BiquadTest.theEqTableHasOnlyTwentyOneDistinctFilters`), so the 41
     * requestable steps produce only 21 distinct filters: -19/-18, -17/-16 …
     * 18/19 are pairwise identical, -1/0/+1 are all the exact bypass
     * `{0x00800000,0,0,0,0}`, and only +/-20 are unshared. A readback can
     * therefore recover the PAIR a step belongs to, never the step, which is
     * precisely why this model field exists.
     *
     * To label a step in decibels, use DspControlCapabilities.eqStepMillibels
     * — which is per FILTER step, not per request step — and say so; see the
     * derivation recorded there.
     */
    int steps;
}
