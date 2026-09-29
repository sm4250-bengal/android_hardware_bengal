/*
 * Copyright (C) 2026 The XYAUTO device-3 project
 * SPDX-License-Identifier: Apache-2.0
 */
package vendor.xyauto.audio;

import vendor.xyauto.audio.AudioGuardStatus;

/**
 * Physical-audio safety boundary for the XYAUTO SM4250 head unit.
 *
 * Served only by `xyauto-hud`, which retains the one `/dev/hu-audio-guard`
 * descriptor for its whole lifetime.  No client obtains a descriptor or can
 * perform raw GPIO, I2C, DSP-address, coefficient, socket, or amplifier/DAC
 * operation.  Every method is a narrow semantic operation already enforced
 * by the kernel guard state machine.
 */
@VintfStability
interface IAudioGuard {
    /** Cached availability and guard status. Safe while the node is absent. */
    AudioGuardStatus getStatus();

    /** Set the reviewed shared ADAU master index, inclusive range 0..61. */
    void setMasterVolumeIndex(int masterIndex);

    /** Enable or disable FM rails through the guard. Never writes GPIO directly. */
    void setFmPower(boolean enabled);

    /**
     * Prepare Android I2S under mute.  The kernel requires master index 61
     * and FM mix 0, so they are not caller-controlled arguments.
     */
    void prepareAndroid();

    /** Commit exactly the successful Android prepare; otherwise the kernel returns EPIPE. */
    void commitAndroid();

    /**
     * One-call guarded Android fallback. Unlike COMMIT, this is valid with no
     * pending PREPARE and clears a stale transition entirely inside the guard.
     */
    void transitionToAndroid();

    /**
     * Prepare FM under mute with the reviewed master 0..61 and mix 0..100.
     * FM power must already be enabled or the guard returns EPIPE.
     */
    void prepareFm(int masterIndex, int fmMixLevel);

    /** Commit exactly the successful FM prepare; all three request fields must match. */
    void commitFm(int masterIndex, int fmMixLevel);

    /**
     * Claim and prepare opt-in mixed Android/FM playback under exact mute.
     * The token is an opaque Binder lifetime anchor, not a numeric handle.
     */
    void prepareFmMix(in IBinder ownerToken);

    /** Commit only the mixed preparation owned by the same live token. */
    void commitFmMix(in IBinder ownerToken);

    /**
     * Set semantic FM attenuation for the same live owner, in 0..9600 mB.
     * 9600 mB is exact digital mute; raw DSP coefficients are never exposed.
     */
    void setFmAttenuation(in IBinder ownerToken, int attenuationMillibels);

    /**
     * V3: select the FM trim base for the same live owner as the OEM
     * radio-mix level 0..100 (level 100 is the factory program's +14 dB
     * power-up trim, the loudest the OEM ever shipped; 27 is -0.03 dB).
     * Accepted only after commitFmMix; the owner's attenuation keeps applying
     * on top of it and is re-applied at once.  The kernel maps the level to
     * its reviewed coefficient table; nothing above level 100 is accepted.
     */
    void setFmMixBaseLevel(in IBinder ownerToken, int level);
}
