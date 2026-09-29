/* Copyright (C) 2026 The XYAUTO device-3 project; SPDX-License-Identifier: Apache-2.0 */
package vendor.xyauto.hud;

import vendor.xyauto.hud.ButtonSystemAction;

/** Independent notification capability; never returned or enumerated by HUD. */
@VintfStability
oneway interface IButtonActionListener {
    void onButtonAction(in ButtonSystemAction action);
}
