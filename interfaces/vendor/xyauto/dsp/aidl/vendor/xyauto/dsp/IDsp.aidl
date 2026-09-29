/*
 * Copyright (C) 2026 The XYAUTO device-3 project
 * SPDX-License-Identifier: Apache-2.0
 */
package vendor.xyauto.dsp;

import vendor.xyauto.dsp.DspControlCapabilities;
import vendor.xyauto.dsp.DspControlSnapshot;
import vendor.xyauto.dsp.DspIdentity;
import vendor.xyauto.dsp.DspOutput;
import vendor.xyauto.dsp.DspParameterBlock;
import vendor.xyauto.dsp.DspEnhancementCapabilities;
import vendor.xyauto.dsp.DspEnhancementSnapshot;

/**
 * Typed ADAU1701 service for device 3.
 *
 * V1 is intentionally read-only. It exposes parameter-word addresses, never
 * arbitrary registers or an I2C transaction. Mute, master volume and output
 * gain belong to a later version after this readback path has flown.
 *
 * V2 appends the five semantic controls the kernel audio guard already
 * validates and applies — output mute, phase, delay and gain, and the four-band
 * SM4250 EQ — and nothing else. The V1 methods above are unchanged.
 *
 * Every value crossing this interface is a uAPI REQUEST STEP, never a physical
 * unit: the guard's tables decide what a step means, and
 * DspControlCapabilities reports that meaning where it is derivable and 0 =
 * unknown where it is not.
 *
 * What V2 deliberately does NOT add, and why:
 *
 *   * master volume and FM trim/mix (`HU_AUDIO_DSP_MASTER_VOLUME`,
 *     `HU_AUDIO_DSP_RADIO_MIX`). Android owns stream volume upstream of this
 *     DSP and the guard makes the master write a silent no-op while the
 *     Android or FM-mix route is selected; the FM session owns its own trim
 *     through IAudioGuard. A second writer here would be a second owner of a
 *     shared cell, which is the failure this project has already paid for.
 *   * balance and fader. They are derived in the app into bounded per-output
 *     `setOutputGainSteps` calls that touch only the outputs they concern, so
 *     they cannot silently overwrite an unrelated channel's own setting.
 *   * crossover, filter design, band centres/Q, program or EEPROM writes, and
 *     any caller-supplied address or coefficient. The kernel accepts no such
 *     operation, and inventing one in userspace would mean writing cells whose
 *     meaning has not been established by ear. That is exactly how this
 *     project destroyed a persistent IC60 parameter image once already.
 *
 * Every setter is one guarded ioctl per semantic change through the single
 * `/dev/hu-audio-guard` owner the daemon already holds. There is no startup,
 * reconnect or default-render write burst: a setter runs only when a caller
 * asks for a value change, and a successful write marks ONLY the control it
 * wrote as applied.
 */
@VintfStability
interface IDsp {
    /** Return the configured part identity and bounded-read capability. */
    DspIdentity getIdentity();

    /**
     * Read 1..maxReadWords consecutive words from parameter RAM.
     * The complete range must lie within 0..parameterWordCount-1.
     */
    DspParameterBlock readParameterRam(int startWord, int wordCount);

    /**
     * Report what this build can write and in which units. Cheap: it consults
     * guard status for `writable` and returns constants for everything else.
     */
    DspControlCapabilities getControlCapabilities();

    /**
     * Return the daemon's last-applied model, per control. Never a chip
     * readback — see DspControlSnapshot. Performs no hardware operation.
     */
    DspControlSnapshot getControlSnapshot();

    /**
     * Mute or unmute one logical output.
     *
     * @throws EX_ILLEGAL_ARGUMENT for an unknown output.
     * @throws EX_ILLEGAL_STATE when the guard is absent, its ABI mismatches,
     *         the IC60 program is not loaded, a fault is latched, or a source
     *         transition is pending (kernel -EBUSY/-EUCLEAN/-EPIPE; the errno
     *         appears in the message).
     * @throws EX_SERVICE_SPECIFIC with the negative errno for any other ioctl
     *         failure.
     */
    void setOutputMute(DspOutput output, boolean muted);

    /** Invert or restore one logical output's polarity. Errors as setOutputMute. */
    void setOutputPhaseInverted(DspOutput output, boolean inverted);

    /**
     * Set one logical output's delay, 0..145 request steps. The physical
     * duration of a step is unproven; see
     * DspControlCapabilities.delayStepMicros, which reports 0 = unknown.
     *
     * @throws EX_UNSUPPORTED_OPERATION for SUBWOOFER, which has no delay cell.
     *         The daemon refuses before touching the guard, so the ioctl the
     *         kernel would reject with -EINVAL is never issued.
     * @throws EX_ILLEGAL_ARGUMENT outside 0..145, or for an unknown output.
     * @throws EX_ILLEGAL_STATE, EX_SERVICE_SPECIFIC as setOutputMute.
     */
    void setOutputDelaySteps(DspOutput output, int steps);

    /**
     * Set one logical output's gain, -200..200 request steps. That curve is
     * confirmed 0.1 dB per step (DspControlCapabilities.gainStepMillibels).
     * Errors as setOutputMute, plus EX_ILLEGAL_ARGUMENT outside the range.
     */
    void setOutputGainSteps(DspOutput output, int steps);

    /**
     * Set one SM4250 EQ band's uAPI request step, -20..+20.
     *
     * STEPS, NOT DECIBELS, and not one filter per step: the OEM encoder
     * truncates `step / 2`, so 41 steps produce 21 distinct filters. See
     * DspEqBand.steps and DspControlCapabilities.eqStepsPerFilter.
     *
     * @throws EX_ILLEGAL_ARGUMENT for a band outside 0..3 or a step outside
     *         -20..+20.
     * @throws EX_ILLEGAL_STATE, EX_SERVICE_SPECIFIC as setOutputMute.
     */
    void setEqBandSteps(int band, int steps);

    /** V3: fixed, compiled IC60 enhancement support and current readiness. */
    DspEnhancementCapabilities getEnhancementCapabilities();

    /** V3: applied enhancements, separately labelled policy basis, and write counter. */
    DspEnhancementSnapshot getEnhancementSnapshot();

    /** Fixed 100 Hz low shelf; OEM level -32..32, not dB. Zero is unity. */
    void setBassLevel(int level);

    /** Fixed 5000 Hz high shelf; OEM level -32..32, not dB. Zero is unity. */
    void setTrebleLevel(int level);

    /**
     * Explicit mode change. Enabling needs a known policy volume index and a
     * successful guarded write. Disabling restores approved IC60 0x00c00000,
     * NOT a hard-bypass coefficient. No startup/reconnect mode replay occurs.
     */
    void setLoudnessEnabled(boolean enabled);

    /**
     * Only the persistent volume-policy runner calls this, never either DSP UI.
     * Cache the current normalized Android media index 0..63. While disabled,
     * this performs no hardware operation; while enabled it updates only the
     * loudness scalar, never master, output gains, EQ, source or FM state.
     * An invalid index is rejected, not clamped. On write failure the latest
     * valid input remains visible but all applied fields and generation remain
     * unchanged. A daemon restart forgets both the mode and the input basis.
     */
    void notifyVolumeIndex(int index);

    /** V4: configured common Android/FM output gain in whole dB, -20..0.
     * This is calibration, not the media-volume index or a RAM readback.
     * Throws UNSUPPORTED_OPERATION if the kernel lacks the shared-gain feature.
     */
    int getSharedOutputGainDb();

    /** V4: immediately apply common output calibration, -20..0 dB.
     * Preserves source attenuation, output levels and all mute states.
     * The system audio-policy owner persists this setting, not either UI.
     * Refuses pending source transitions, ACC hold, faults and unavailable DSP.
     */
    void setSharedOutputGainDb(int gainDb);
}
