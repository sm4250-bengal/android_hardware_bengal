/*
 * Copyright (C) 2026 The XYAUTO device-3 project
 * SPDX-License-Identifier: Apache-2.0
 */
///////////////////////////////////////////////////////////////////////////////
// THIS FILE IS IMMUTABLE. DO NOT EDIT IN ANY CASE.                          //
///////////////////////////////////////////////////////////////////////////////

// This file is a snapshot of an AIDL file. Do not edit it manually. There are
// two cases:
// 1). this is a frozen version file - do not edit this in any case.
// 2). this is a 'current' file. If you make a backwards compatible change to
//     the interface (from the latest frozen version), the build system will
//     prompt you to update this file with `m <name>-update-api`.
//
// You must not make a backward incompatible change to any AIDL file built
// with the aidl_interface module type with versions property set. The module
// type is used to build AIDL files in a way that they can be used across
// independently updatable components of the system. If a device is shipped
// with such a backward incompatible change, it has a high risk of breaking
// later when a module using the interface is updated, e.g., Mainline modules.

package vendor.xyauto.hud;
@VintfStability
interface IHud {
  vendor.xyauto.hud.HudStatus getStatus();
  void subscribe(in vendor.xyauto.hud.IHudCallback callback);
  void unsubscribe(in vendor.xyauto.hud.IHudCallback callback);
  vendor.xyauto.hud.HudEvent[] getRecentEvents(int maxCount);
  vendor.xyauto.hud.UsbModeStatus getUsbMode();
  vendor.xyauto.hud.UsbModeStatus setUsbMode(vendor.xyauto.hud.UsbMode mode);
  vendor.xyauto.hud.UsbHubStatus checkUsbHub();
  vendor.xyauto.hud.HubPortState[] getHubPorts();
  vendor.xyauto.hud.HubPortState setHubPortEnabled(int portNumber, boolean enabled);
  vendor.xyauto.hud.GpioCensus getGpioCensus();
  vendor.xyauto.hud.ButtonEvent[] getRecentButtonEvents(int maxCount);
  void setButtonLearnMode(boolean enabled);
  boolean isButtonLearnMode();
  vendor.xyauto.hud.ButtonBinding[] getButtonBindings();
  void setButtonBindings(in vendor.xyauto.hud.ButtonBinding[] bindings);
  void registerButtonActionListener(in vendor.xyauto.hud.IButtonActionListener listener);
  void unregisterButtonActionListener(in vendor.xyauto.hud.IButtonActionListener listener);
}
