/*
 * Copyright (C) 2026 The XYAUTO device-3 project
 * SPDX-License-Identifier: Apache-2.0
 */
package vendor.xyauto.dsp;

import vendor.xyauto.dsp.DspOutput;

/**
 * One logical output's last SUCCESSFULLY APPLIED control state, as recorded by
 * this daemon.
 *
 * This is the daemon's model, never a chip readback. `readParameterRam` (V1)
 * remains the one independent view of what the ADAU1701 actually holds, and a
 * UI that shows only this parcelable is showing what it asked for, not what
 * happened. The two disagree legitimately after a kernel-side fault-safe, an
 * ACC-off/ADSP park, or an IC60 reload.
 *
 * # Validity is PER CONTROL
 *
 * Each `*Applied` flag is false until THIS daemon's ioctl for THAT control
 * succeeded in THIS boot, and a successful write sets ONLY its own flag. A
 * successful mute therefore does not make the phase, delay or gain fields
 * meaningful: those stay at the daemon's inert defaults, which say nothing
 * about the chip — the IC60 image's own resting values are what the hardware
 * holds, and only `readParameterRam` can show them.
 *
 * That granularity is the point. A caller replaying saved settings must SKIP
 * every control whose flag was false when the settings were saved; one flag
 * for the whole output would let a remembered mute drag three default values
 * onto the hardware with it.
 */
@VintfStability
parcelable DspOutputState {
    /** Which logical output this row describes. */
    DspOutput output;

    /** True once a mute write for this output succeeded this boot. */
    boolean muteApplied;
    /** Last applied mute. Kernel `HU_AUDIO_DSP_OUTPUT_MUTE`, value 0 or 1. */
    boolean muted;

    /** True once a phase write for this output succeeded this boot. */
    boolean phaseApplied;
    /**
     * Last applied polarity. Kernel `HU_AUDIO_DSP_OUTPUT_PHASE`, value 0 or 1;
     * inverted writes 5.23 `0xff800000` (-1.0) instead of unity.
     */
    boolean phaseInverted;

    /**
     * True once a delay write for this output succeeded this boot. Always
     * false for SUBWOOFER, which has no delay cell to write.
     */
    boolean delayApplied;
    /**
     * Last applied delay, 0..145 REQUEST STEPS; -1 means the output has no
     * delay cell at all. SUBWOOFER is the only such output today — the kernel
     * refuses `HU_AUDIO_DSP_OUTPUT_DELAY` for guard index 2 with -EINVAL
     * (`xyauto_audio_guard.c:1009-1015`), so the daemon refuses it before the
     * ioctl and this field stays -1 for it forever.
     *
     * The PHYSICAL unit of a step is not proven on device 3; see
     * DspControlCapabilities.delayStepMicros, which reports 0 = unknown.
     */
    int delaySteps;

    /** True once a gain write for this output succeeded this boot. */
    boolean gainApplied;
    /**
     * Last applied output gain in REQUEST STEPS, -200..200, indexing the
     * guard's `output_gain_coeff[401]` table. That curve is confirmed to be
     * 0.1 dB per step, so DspControlCapabilities.gainStepMillibels is 10; the
     * derivation is recorded there.
     */
    int gainSteps;
}
