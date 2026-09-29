/*
 * Copyright (C) 2026 The XYAUTO device-3 project
 * SPDX-License-Identifier: Apache-2.0
 */
package vendor.xyauto.fm;

import vendor.xyauto.fm.IFmRadioCallback;
import vendor.xyauto.fm.RdsInfo;
import vendor.xyauto.fm.SignalQuality;
import vendor.xyauto.fm.TunerState;

/**
 * FM radio on the XYAUTO/Podofo SM4250 head unit (device 3).
 *
 * Served by the single D18 vendor daemon, which is also the only owner of the
 * QN8035's I2C bus, of the MCU UART and of the analogue input mux. There is no
 * second path to the tuner: no kernel driver exists or is wanted
 * (mem:qn8035-direct-i2c), and the device tree deliberately instantiates no
 * client node on the i2c-fm bus (kernel/dts/sm4250-xyauto-hu.dts:183-193).
 *
 * WHAT THIS INTERFACE DOES NOT HAVE, and why:
 *   - no register read/write. The tuner is not a bus the framework may drive.
 *   - no "select source" or "set volume". Source policy and volume belong to
 *     the HU service app (D18); this interface only makes FM *available*.
 *   - no synchronous seek. A band walk costs ~205 settles; it is a callback.
 */
@VintfStability
interface IFmRadio {
    /**
     * Power the tuner up and make FM the audible source, or power it down.
     *
     * ONE call, because the ordering is not the caller's business and two
     * callers interleaving it would be silent breakage. The daemon performs,
     * in this order (settles measured on device 1, apps/mcu-toolbox/bridge/
     * radio.go — they are a starting point for device 3, not a guarantee):
     *   1. MCU source select, frame 5a a5 03 05 03 f8 (off: ...05 04 f7).
     *      radioMCUSelectSettle = 100 ms. It may gate board-side tuner
     *      support circuitry, so it comes first.
     *   2. QN8035 soft reset, chip-ID verification, RX+RDS mode.
     *      radioPowerOnSettle = 250 ms covers the PLL lock.
     *   3. the analogue input mux in front of the ADAU1701's ADC pair.
     *      On device 3 the strap lines are TLMM 36 (A) and TLMM 37 (B) --
     *      MEASURED on this board, from the thirteen pinctrl states of
     *      stock-live.dts:17932's adau1701@34 node (adau1701_pins_a0/a1 are
     *      gpio36 output-low/high, b0/b1 are gpio37; the same node also gives
     *      reset 107, 3V3 enable 106, ESS9080 DAC enable 83, amp enable 93).
     *      ***THE POLARITY IS THE INVERTED-DOC TRAP***
     *      (mem:adau-source-mux-inverted-doc-trap): FM is A=low B=high, i.e.
     *      gpio36 low / gpio37 high. The OEM reference doc's "Physical A/B"
     *      column says the opposite and was once copied verbatim into a driver
     *      comment, which routed FM from the phone-call input. The pins are
     *      measured; that polarity is still device 1's decode, and only an
     *      audible test separates them.
     *   4. clear the tuner's own MUTE_EN. radioUnmuteSettle = 250 ms before
     *      the audio guard is asked to expose FM, so the analogue edge is
     *      upstream of a still-disconnected mux.
     * Power-down runs the reverse order, muting first.
     *
     * Idempotent. Throws EX_ILLEGAL_STATE if the chip ID does not read 0x84.
     */
    void setEnabled(boolean enabled);

    /** Current state; safe to call at any time, never touches the bus. */
    TunerState getState();

    /**
     * Chip identity, read live from register 0x06. Expected 0x84.
     *
     * This is the Phase-4d proof point and it is deliberately callable on its
     * own: it answers "is the bus right, is the address right, is the part
     * there" in one transaction, before anything is powered or muxed.
     */
    int getChipId();

    /**
     * Tune to a frequency in kHz. 87500..108000, 100 kHz grid.
     *
     * Throws EX_ILLEGAL_ARGUMENT off-grid or out of band — the channel word
     * is only defined there (bridge qn8035ChannelWord rejects 87400, 87525,
     * 108100 in its own tests).
     *
     * ★ LEGAL WHILE DISABLED, AND THE CHANNEL IS REMEMBERED. A tune() before
     * setEnabled(true) stores the channel and applies it during the enable
     * sequence (between the power-up and the mux), so it does NOT make the
     * tuner audible.
     *
     * ADDED 2026-08-30 BY THE HuRadio PORT, and it is a contract on the
     * still-unwritten daemon, not a new method. The app's audio handover is
     * two-phase and the order is load-bearing:
     *   tuner.prepare(khz) -> dsp.prepareFm() -> dsp.commitFm() -> tuner.commit()
     * (RadioController.kt:137-149). setEnabled() is documented above as the one
     * call that also does the mux and the un-mute — i.e. it is the *commit*.
     * Without this sentence there is nothing left to carry the *prepare*, and
     * the only alternative mapping (prepare = setEnabled+tune, commit = no-op)
     * makes FM audible before the DSP guard has committed, which is the exact
     * failure the two-phase design exists to prevent.
     * If the daemon cannot honour this, the interface needs an explicit
     * two-phase enable INSTEAD — and that decision has to be made before
     * `m vendor.xyauto.fm-freeze-api`, not after.
     */
    void tune(int frequencyKHz);

    /**
     * Signal quality at the current channel.
     *
     * Callers should judge "is there a station" on SNR, not RSSI. Measured on
     * device 1: 97.5 MHz read SNR 32 with its neighbours at 10 and 15 and
     * empty air at a flat 6, while RSSI managed only 89 against 83. The
     * RSSI-based seek that margin replaced landed +/-0.1 MHz off and missed
     * one time in eight.
     */
    SignalQuality getSignalQuality();

    /**
     * Start a hardware-assisted seek. Returns immediately; the result arrives
     * as IFmRadioCallback.onSeekComplete or onSeekStopped.
     *
     * Only one seek at a time; a second call throws EX_ILLEGAL_STATE.
     */
    void startSeek(boolean upward);

    /** Stop a running seek. No-op when none is running. */
    void stopSeek();

    /**
     * Latest decoded RDS for the current channel, or an RdsInfo with
     * programmeService empty when nothing has synced yet.
     */
    RdsInfo getRdsInfo();

    /**
     * Register the one callback. A second registration replaces the first --
     * there is exactly one legitimate client, the HU service app.
     */
    void setCallback(in @nullable IFmRadioCallback callback);
}
