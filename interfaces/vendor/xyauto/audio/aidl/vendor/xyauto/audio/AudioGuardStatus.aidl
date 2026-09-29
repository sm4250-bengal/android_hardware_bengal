/*
 * Copyright (C) 2026 The XYAUTO device-3 project
 * SPDX-License-Identifier: Apache-2.0
 */
package vendor.xyauto.audio;

/** Cached status from the one daemon-owned `/dev/hu-audio-guard` descriptor. */
@VintfStability
parcelable AudioGuardStatus {
    /** True only when the guard node has uAPI V4 and its IC60 program is loaded. */
    boolean available;

    /** Stable NotReady reason; empty only when available. */
    String unavailableReason;

    /** Kernel `HU_AUDIO_GUARD_ABI_VERSION`, or zero when the node is absent. */
    int abiVersion;
    boolean dspPowered;
    boolean programLoaded;
    boolean fmPowered;
    boolean dacEnabled;
    boolean routeReady;
    boolean amplifierEnabled;
    /** Kernel source enum: 0 none, 1 Android, 2 FM. */
    int source;
    boolean faultLatched;
    int lastError;
    boolean transitionPending;
}
