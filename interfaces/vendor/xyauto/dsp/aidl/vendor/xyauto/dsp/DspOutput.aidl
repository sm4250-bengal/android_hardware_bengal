/*
 * Copyright (C) 2026 The XYAUTO device-3 project
 * SPDX-License-Identifier: Apache-2.0
 */
package vendor.xyauto.dsp;

/**
 * The five logical outputs of the recovered SM4250/CpuType-1 IC60 program.
 *
 * These values are the kernel's `enum hu_audio_dsp_channel`
 * (`xyauto_audio_guard.h:59-66`) verbatim, so no translation table can drift.
 * They are LOGICAL outputs of the DSP program, not proven speaker identities:
 * a successful write says the coefficient reached the cell, never which cone
 * moved. Device 1 established the physical mapping by muting one output at a
 * time with music playing; device 3 has not repeated that experiment.
 */
@VintfStability
@Backing(type="int")
enum DspOutput {
    /** Program slot 1, bank 0 (guard output index 0). */
    FRONT_LEFT = 1,
    /** Program slot 1, bank 1 (guard output index 3). */
    FRONT_RIGHT = 2,
    /** Program slot 2, bank 0 (guard output index 1). */
    REAR_LEFT = 3,
    /** Program slot 2, bank 1 (guard output index 4). */
    REAR_RIGHT = 4,
    /** Program slot 3, bank 0 (guard output index 2). Has no delay cell. */
    SUBWOOFER = 5,
}
