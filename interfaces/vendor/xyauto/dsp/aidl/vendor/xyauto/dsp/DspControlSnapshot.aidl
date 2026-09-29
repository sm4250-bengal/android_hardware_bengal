/*
 * Copyright (C) 2026 The XYAUTO device-3 project
 * SPDX-License-Identifier: Apache-2.0
 */
package vendor.xyauto.dsp;

import vendor.xyauto.dsp.DspEqBand;
import vendor.xyauto.dsp.DspOutputState;

/**
 * The daemon's complete last-applied control model, PER CONTROL.
 *
 * EXPLICITLY NOT A CHIP READBACK. Every field is what this daemon successfully
 * wrote during this boot; nothing here is read from the ADAU1701.
 * `readParameterRam` (V1) stays the independent readback, and a UI that wants
 * to be honest shows both and lets them disagree.
 *
 * A successful ioctl updates ONLY the control it wrote and sets ONLY that
 * control's applied flag, so a caller can tell "this daemon set it" apart from
 * "this is the daemon's inert default" one control at a time.
 */
@VintfStability
parcelable DspControlSnapshot {
    /** One row per DspOutput, in enum order, always `outputCount` long. */
    DspOutputState[] outputs;
    /** One row per band 0..3, in band order, always `eqBandCount` long. */
    DspEqBand[] eq;
    /**
     * Increments once per successfully applied control write and never
     * otherwise. A refused or failed write leaves it and the applied control
     * values unchanged (lastError may change), so a caller that saw generation
     * N and sees N again knows no successful control write landed in between.
     */
    long generation;
    /**
     * The last refusal or failure, empty if the last write succeeded or none
     * has been attempted. Diagnostic text for a human; never parse it.
     */
    String lastError;
}
