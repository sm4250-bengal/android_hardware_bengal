/*
 * Copyright (C) 2026 The XYAUTO device-3 project
 * SPDX-License-Identifier: Apache-2.0
 */
package vendor.xyauto.hud;

import vendor.xyauto.hud.ButtonBinding;
import vendor.xyauto.hud.ButtonEvent;
import vendor.xyauto.hud.GpioCensus;
import vendor.xyauto.hud.HubPortState;
import vendor.xyauto.hud.HudEvent;
import vendor.xyauto.hud.HudStatus;
import vendor.xyauto.hud.IHudCallback;
import vendor.xyauto.hud.IButtonActionListener;
import vendor.xyauto.hud.UsbHubStatus;
import vendor.xyauto.hud.UsbMode;
import vendor.xyauto.hud.UsbModeStatus;

/**
 * Vehicle-state and board-control half of the single D18 vendor daemon.
 *
 * This is a typed replacement for the measured hu-mcu-bridge loopback API,
 * not a JSON tunnel. It exposes confirmed ACC state and semantic key events;
 * it never exposes raw MCU transmit bytes. FM remains vendor.xyauto.fm.
 */
@VintfStability
interface IHud {
    /** Cached counters and confirmed ACC state; performs no hardware I/O. */
    HudStatus getStatus();

    /**
     * Register the one HU-service callback and immediately deliver current
     * ACC state. A new callback replaces the old one.
     */
    void subscribe(in IHudCallback callback);

    /** Remove callback only when it is the currently registered binder. */
    void unsubscribe(in IHudCallback callback);

    /**
     * Last `maxCount` diagnostic events, oldest first. Values <= 0 request the
     * whole bounded ring; the implementation may cap an excessive request.
     */
    HudEvent[] getRecentEvents(int maxCount);

    /** Cached USB role. */
    UsbModeStatus getUsbMode();

    /** Select host/device through TLMM 85; TLMM 34 stays high for either role. */
    UsbModeStatus setUsbMode(UsbMode mode);

    /** Run one bounded internal-hub health/recovery pass. */
    UsbHubStatus checkUsbHub();

    /** Read every internal 1-1-portN power-switch state. */
    HubPortState[] getHubPorts();

    /**
     * Enable or disable one internal hub port by number. The daemon refuses
     * the log-stick port outside its ordered ACC-off quiescence path.
     */
    HubPortState setHubPortEnabled(int portNumber, boolean enabled);

    /** GPIO_V2_GET_LINEINFO plus read-only debug census; never requests a line. */
    GpioCensus getGpioCensus();

    /*
     * ---- V2: physical bezel buttons -------------------------------------
     *
     * Added because the five left-bezel buttons reach no app today. Measured
     * 2026-09-04: every press arrives as a touchscreen contact at negative x
     * and Android discards all of them as out-of-frame, while the MCU -- the
     * documented source -- sends nothing. The daemon can see them; an app
     * cannot, and cannot be given the raw node. So the daemon reports presses
     * and injects bound keys, and the app owns the learning and the UI.
     */

    /**
     * Last `maxCount` observed button presses, oldest first, from a bounded
     * ring. Values <= 0 request the whole ring; the implementation may cap an
     * excessive request. Mirrors getRecentEvents() so an app that missed a
     * push can still show what happened.
     */
    ButtonEvent[] getRecentButtonEvents(int maxCount);

    /**
     * Enter or leave learn mode. While learn mode is on the daemon reports
     * presses but INJECTS NOTHING.
     *
     * The suppression is the point, not an optimisation. Teaching the wizard
     * which button should mean Back requires pressing the button that is
     * already bound to Back; if injection stayed live, that press would
     * navigate the wizard away mid-setup. Any flow that learns a
     * navigation key needs this or it cannot work at all.
     *
     * SAFETY: learn mode must not be able to strand the buttons. The daemon
     * clears it when the subscribing binder dies AND after a bounded timeout,
     * so a setup app that crashes mid-wizard leaves a unit whose buttons still
     * work rather than a unit that is silently deaf until the next reboot.
     */
    void setButtonLearnMode(boolean enabled);

    /** Whether learn mode is currently on, including after a daemon-side clear. */
    boolean isButtonLearnMode();

    /** The bindings currently in force, in no guaranteed order. */
    ButtonBinding[] getButtonBindings();

    /**
     * Replace the whole binding table atomically.
     *
     * Whole-table rather than per-entry so the daemon can validate the set as a
     * unit: it rejects the call if two BEZEL bindings overlap, or if one
     * Android key is claimed twice. A partial-update API would let the table
     * pass through states that are individually legal and jointly wrong.
     *
     * The daemon persists the table; the app is not the system of record.
     * It reloads the stored table at startup, so bindings are in force before
     * any app is running -- which is the point, since boot to a running app is
     * about a minute and the bezel buttons are what a driver reaches for first.
     *
     * ATOMIC IN BOTH DIRECTIONS. On return the table is in force AND stored.
     * On a thrown exception NOTHING changed: the previous table is still in
     * force and still stored, so a caller may treat any failure as
     * authoritative and simply re-read rather than guess what state the daemon
     * is in. There is no outcome in which memory and disk disagree.
     *
     *   ILLEGAL_ARGUMENT -- the table itself was rejected: two BEZEL bindings
     *       overlap, one Android key is claimed twice, or a range is inverted.
     *       Sending it again unchanged will fail again.
     *   ILLEGAL_STATE -- the table was accepted but could not be stored (no
     *       space, a read-only /data, a denied write). The table was NOT
     *       applied. Retrying it unchanged is the right response once the
     *       cause is gone.
     *
     * `label` is the app's own: the daemon never parses it and stores it
     * byte for byte, so it is safe to use as a slot identity across the
     * unordered getButtonBindings().
     */
    void setButtonBindings(in ButtonBinding[] bindings);

    /**
     * V3: add an independent read-only listener, linking death before publication.
     * Same-handle registration is idempotent and emits no immediate action.
     * Null/link failure or finite per-UID/global quota exhaustion fails without
     * replacing any listener. Calling UID is accounting, not authentication.
     */
    void registerButtonActionListener(in IButtonActionListener listener);

    /**
     * Remove only this exact capability. A dispatch already in flight may finish;
     * queued events cannot cross registration generations.
     */
    void unregisterButtonActionListener(in IButtonActionListener listener);
}
