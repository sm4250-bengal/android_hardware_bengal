/*
 * Copyright (C) 2026 The XYAUTO device-3 project
 * SPDX-License-Identifier: Apache-2.0
 */
package vendor.xyauto.dsp;

/** One coherent, bounded ADAU1701 parameter-RAM read. */
@VintfStability
parcelable DspParameterBlock {
    /** First parameter-word address represented by words[0]. */
    int startWord;
    /** Raw big-endian transfer words converted to host integers. */
    int[] words;
}
