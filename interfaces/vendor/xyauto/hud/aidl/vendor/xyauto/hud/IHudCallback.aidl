/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

import vendor.xyauto.hud.AccStateEvent;
import vendor.xyauto.hud.ButtonEvent;
import vendor.xyauto.hud.HudEvent;
import vendor.xyauto.hud.McuKeyEvent;

/** Asynchronous daemon events; callbacks must never block hardware threads. */
@VintfStability
oneway interface IHudCallback {
    /** Confirmed ACC lifecycle transition, including initial subscription state. */
    void onAccStateChanged(in AccStateEvent event);

    /** Semantic Android-key candidate from an MCU button or rotary packet. */
    void onMcuKeyEvent(in McuKeyEvent event);

    /** Diagnostic stream equivalent to the bridge event subscription. */
    void onEvent(in HudEvent event);

    /**
     * V2: one observed physical button press, from either transport.
     *
     * Delivered whether or not the button is bound and whether or not learn
     * mode is on -- observation and injection are separate concerns, and the
     * learn wizard needs the presses precisely when nothing is being injected.
     */
    void onButtonEvent(in ButtonEvent event);
}
