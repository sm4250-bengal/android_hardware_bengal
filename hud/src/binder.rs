// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Android-only Binder adapter for [`crate::service::HudService`].
//!
//! This module is compiled only by Soong with the `binder` feature. The host
//! Cargo manifest intentionally has no Binder/AIDL dependencies, so the desk
//! proof covers the trait implementation while the first announced Android
//! build remains responsible for type-checking this generated-code adapter.

use crate::callback_registry::{CallbackToken, GenerationRegistry, Publish, Registration};
use crate::buttons::actions::{ActionRegistry, ActionSink, Reserve};
use crate::buttons::ButtonSystemAction;
use crate::gpio::chardev::GpioV2;
use crate::mcu::acc::State as AccMachineState;
use crate::mcu::serial::{AccSession, AccTxObservation, AccTxPhase, FrameValidity, PlatformHooks};
use crate::mcu::uinput::{
    ButtonInjector, UinputDevice, DEVICE3_BUTTONS, DEVICE3_MAPPING_CONFIRMED,
};
use crate::mcu::{resolve_port, ButtonState, Event, SE0_MMIO, SE3_MMIO_NOT_OURS};
use crate::service::{self as core, HudApi as _};
use crate::usb::{LinuxHubSysfs, ThreadSleeper, UsbOwner};
use binder::binder_impl::IBinderInternal;
use binder::{
    BinderFeatures, DeathRecipient, ExceptionCode, IBinder, Interface, SpIBinder, Status, Strong,
};
use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant};
use xyauto_fm::qn8035;
use xyauto_fm::service::{RdsProgress, SeekProgress};
// The daemon runtime is bengal-hud, but this is generated from the stable
// vendor.xyauto.hud AIDL descriptor and therefore keeps its AIDL crate name.
use vendor_xyauto_hud::aidl::vendor::xyauto::hud::{
    AccState::AccState as AidlAccState,
    AccStateEvent::AccStateEvent as AidlAccStateEvent,
    GpioCensus::GpioCensus as AidlGpioCensus,
    GpioDirection::GpioDirection as AidlGpioDirection,
    GpioLineInfo::GpioLineInfo as AidlGpioLineInfo,
    HubPortState::HubPortState as AidlHubPortState,
    ButtonBinding::ButtonBinding as AidlButtonBinding,
    ButtonEvent::ButtonEvent as AidlButtonEvent,
    ButtonSource::ButtonSource as AidlButtonSource,
    ButtonSystemAction::ButtonSystemAction as AidlButtonSystemAction,
    IButtonActionListener::IButtonActionListener,
    HudEvent::HudEvent as AidlHudEvent,
    HudEventType::HudEventType as AidlHudEventType,
    HudStatus::HudStatus as AidlHudStatus,
    IHud::{BnHud, IHud},
    IHudCallback::IHudCallback,
    McuKeyAction::McuKeyAction as AidlMcuKeyAction,
    McuKeyEvent::McuKeyEvent as AidlMcuKeyEvent,
    UsbHubStatus::UsbHubStatus as AidlUsbHubStatus,
    UsbMode::UsbMode as AidlUsbMode,
    UsbModeStatus::UsbModeStatus as AidlUsbModeStatus,
};

/// Stable service-manager name from the VINTF fragment.
pub const HUD_SERVICE_NAME: &str = "vendor.xyauto.hud.IHud/default";

const DEFAULT_MCU_DEVICE: &str = "/dev/ttyHS1";
const UINPUT_DEVICE: &str = "/dev/uinput";
const INJECT_KEYS_PROPERTY: &str = "ro.vendor.bengal.hud.inject_keys";
const PROP_VALUE_MAX: usize = 92;
const ANDROID_LOG_INFO: i32 = 4;
const ANDROID_LOG_ERROR: i32 = 6;

extern "C" {
    fn __android_log_write(priority: i32, tag: *const c_char, text: *const c_char) -> i32;
    fn __system_property_get(name: *const c_char, value: *mut c_char) -> i32;
}

type ProductionCore =
    HudService<UsbOwner<GpioV2, LinuxHubSysfs, ThreadSleeper>, core::InventoryGpioCensus>;
type ProductionFm = xyauto_fm::service::FmService<crate::fm::backend::ProductionFmBackend>;

use core::HudService;

/// Register `IHud/default` and join the normal `/dev/binder` thread pool.
pub fn register_and_join(mcu_device: Option<PathBuf>) -> binder::Result<()> {
    let sysfs = crate::sysfs_root();
    let mcu_device = mcu_device.unwrap_or_else(|| PathBuf::from(DEFAULT_MCU_DEVICE));
    let inject_keys = bool_property(INJECT_KEYS_PROPERTY);
    let usb = UsbOwner::new(
        GpioV2::gpiochip0(),
        LinuxHubSysfs::new(&sysfs),
        ThreadSleeper,
        0,
    ).with_kernel_role(&sysfs);
    let started = Instant::now();
    let core = Arc::new(Mutex::new(HudService::new(usb, core::InventoryGpioCensus)));
    let callback_registry = Arc::new(CallbackRegistry::default());
    let callback_mailbox = Arc::new(CallbackMailbox::default());
    let action_registry = Arc::new(ActionRegistry::default());
    action_registry.start().map_err(|error| Status::new_exception_str(
        ExceptionCode::ILLEGAL_STATE, Some(format!("button action workers: {error}")),
    ))?;
    // The binding table is the daemon's, not the app's -- IHud.aidl says so,
    // and it is the only workable side: boot to a running app was measured at
    // 52 s (mem:bengal-boot-time-first-measurement-2026-09-04), so an app that
    // owned the table would leave all five bezel buttons dead for the first
    // minute after the key turns. Loading it here means they work as soon as
    // this daemon does.
    let (button_service, load) =
        crate::buttons::ButtonService::with_store(crate::buttons::ButtonStore::default_path());
    match &load {
        crate::buttons::LoadOutcome::Absent => log_both(
            false,
            format!(
                "button bindings none path={} reason=never-taught",
                crate::buttons::DEFAULT_BINDINGS_PATH
            ),
        ),
        crate::buttons::LoadOutcome::Loaded { count } => log_both(
            false,
            format!(
                "button bindings loaded count={count} path={}",
                crate::buttons::DEFAULT_BINDINGS_PATH
            ),
        ),
        // The only trace of a table that existed a moment ago and does not now.
        // An error line on purpose: the owner is about to find five dead
        // buttons, and this is the sentence that explains them.
        crate::buttons::LoadOutcome::Refused { error } => log_both(
            true,
            format!(
                "button bindings REFUSED path={} error={error}; starting with no bindings, re-teach them",
                crate::buttons::DEFAULT_BINDINGS_PATH
            ),
        ),
    }
    let buttons = Arc::new(Mutex::new(button_service));
    // Must happen before any subscribe can be served, i.e. before the service
    // joins Binder below. Without it a crashed setup app leaves learn mode --
    // and therefore the five bezel buttons -- dead for the full five-minute
    // timeout. See CallbackRegistry::on_death.
    callback_registry.attach_learner(&buttons);
    let service = BinderHud {
        core: Arc::clone(&core),
        buttons: Arc::clone(&buttons),
        started,
        callback_registry: Arc::clone(&callback_registry),
        callback_mailbox: Arc::clone(&callback_mailbox),
        action_registry: Arc::clone(&action_registry),
    };
    // Open exactly once.  The guard enforces a single opener and runs its
    // all-safe shutdown on close, so no Binder method may reopen this node.
    // An absent node remains a declared inert service whose status says why;
    // it never falls back to GPIO/I2C/DSP or another daemon/socket.
    let audio_guard = Arc::new(Mutex::new(crate::dsp::guard::AudioGuard::open_default()));
    // Load intent/install the restore hook before any activation or ACC worker
    // can enable the outputs. Both DSP apps use this same model and store.
    let dsp_controls = Arc::new(crate::dsp::controls::DspControls::with_persistence(
        Arc::clone(&audio_guard), crate::dsp::controls::SETTINGS_PATH.into(),
    ));
    // HuRadio first commits IAudioGuard's muted FM route, then calls
    // IFmRadio.setEnabled(). The real backend therefore owns only QN8035
    // reset/CID/RX/tune/unmute; it neither duplicates the DSP mux nor opens a
    // legacy socket/raw HuRadio device path.
    let fm = Arc::new(Mutex::new(ProductionFm::new(
        crate::fm::backend::ProductionFmBackend::new(&sysfs),
    )));
    #[cfg(feature = "acc-power-channel")]
    let acc_power = if bool_property(crate::acc_power::bootstrap::ENABLE_PROPERTY) {
        let channel = (|| -> std::io::Result<crate::acc_power::Channel> {
            let mut channel = crate::acc_power::Channel::new()?;
            audio_guard.lock().map_err(|_| std::io::Error::other("audio lock poisoned"))?
                .attach_acc_validity(channel.native_validity());
            channel.start_participants(NativeAccOwners {
                audio: audio_guard.clone(), fm: fm.clone(),
                deep: crate::acc_power::deep_owner::new(),
            })?;
            Ok(channel)
        })();
        match channel {
            Ok(channel) => {
                let channel = Arc::new(channel);
                match crate::acc_power::bootstrap::register(channel.clone()) {
                    Ok(()) => Some(channel),
                    Err(error) => {
                        log_both(true, format!("ACC power bootstrap unavailable: {error}"));
                        None
                    }
                }
            }
            Err(error) => {
                log_both(true, format!("ACC preparation worker unavailable: {error}"));
                None
            }
        }
    } else { None };
    // Without this the service accepts startSeek and nothing ever steps it:
    // the seek hangs and no callback fires. See spawn_fm_seek_worker.
    spawn_fm_seek_worker(Arc::clone(&fm));
    // D3-0120: hold /dev/cpu_dma_latency while the panel is on (docs/research/
    // 2026-09-05-cpu-latency-hold-design.md). Reads the ACC state from `core`.
    spawn_cpu_latency_hold(Arc::clone(&core), sysfs.clone());
    spawn_can_reader(Arc::clone(&core), sysfs.clone());
    spawn_bezel_reader(
        Arc::clone(&buttons),
        started,
        Arc::clone(&core),
        Arc::clone(&callback_registry),
        Arc::clone(&callback_mailbox),
        inject_keys,
        action_registry,
    );
    let hud_binder = BnHud::new_binder(service, BinderFeatures::default());
    // Audio registration is intentionally nonfatal at this staging point:
    // FM, HUD and MCU capture must stay alive if a product/VINTF integration
    // error is found before the audio module/firmware lane arrives. An absent
    // guard node itself is not this failure -- it registers an inert status
    // service successfully.
    let fm_for_audio_cleanup = Arc::clone(&fm);
    let fm_cleanup: crate::audio_binder::FmCleanup = Arc::new(move |rails_live| {
        fm_for_audio_cleanup
            .lock()
            .map_err(|_| "FM service mutex poisoned".to_owned())?
            .audio_route_lost(rails_live)
            .map_err(|error| error.to_string())
    });
    // IDsp shares the existing sole guard owner; keep its reference before
    // activation takes ownership. Registration itself issues no DSP writes.
    let dsp_audio_guard = Arc::clone(&audio_guard);
    match crate::audio_binder::register(Arc::clone(&audio_guard), fm_cleanup) {
        Ok(()) => spawn_audio_guard_activation(audio_guard),
        Err(error) => {
            log_both(
                true,
                format!(
                    "audio guard Binder registration failed; continuing without physical audio service error={error}"
                ),
            );
        }
    }
    // IDsp readback and typed V2 controls share this process with IAudioGuard. Keep
    // registration nonfatal like the sibling FM/audio services: a declaration
    // or policy integration error must not take vehicle input and tuner
    // service down with it. Adapter-open failure itself is represented by a
    // successfully registered inert IDsp identity and is never retried here.
    if let Err(error) = crate::dsp::binder::open_and_register_with_guard(dsp_audio_guard, dsp_controls) {
        log_both(
            true,
            format!("DSP Binder registration failed; continuing error={error}"),
        );
    }
    crate::startup::register_services_and_start_mcu(
        || xyauto_fm::binder::register(fm),
        || -> binder::Result<()> {
            Ok(binder::add_service(
                HUD_SERVICE_NAME,
                hud_binder.as_binder(),
            )?)
        },
        || {
            spawn_mcu_reader(
                core,
                started,
                sysfs,
                mcu_device,
                inject_keys,
                callback_registry,
                callback_mailbox,
                #[cfg(feature = "acc-power-channel")]
                acc_power,
            )
        },
        |error| {
            log_both(
                true,
                format!("FM Binder registration failed; continuing error={error}"),
            )
        },
    )?;
    binder::ProcessState::join_thread_pool();
    Ok(())
}

/// DSP_ON may wait for the kernel's firmware retry path.  Keep that operation
/// outside registration and all HUD/callback locks: the status Binder service
/// is available immediately and reports any safe activation failure.
/// Drive a queued FM seek, one HALF-channel per step, off every Binder thread.
///
/// This is the "HUD worker" that `FmHardware::start_seek` has always been
/// documented to queue onto and that never existed: the backend returned
/// `NotReady/QN8035-seek-worker-not-wired`, so scanning was declared and
/// unimplemented. `FmService::poll_seek` performs the real bus work and
/// finishes the seek itself, so this loop owns only the cadence.
///
/// THE CADENCE IS THE CORRECTNESS. One channel is two calls -- tune, then
/// measure -- and the gap between them is `SEEK_SETTLE_MS`, because register
/// 0x02 does not report the channel just tuned until the front end has
/// settled on it. D3-0117 did both in one call and slept afterwards, so the
/// walk judged every channel on its predecessor's signal. That is why
/// `poll_seek` returns a phase and not a bool.
///
/// The lock is released between steps deliberately. A 205-channel walk holding
/// it would block `getState`, `stopSeek` and every other FM transaction for the
/// whole scan -- and `stopSeek` in particular must be able to interrupt it.
/// At ~110 ms per channel a full sweep is ~22 s, matching device 1's measured
/// walk (`radio.go`: 100 ms settle, 35 s timeout).
/// Read the touchscreen for bezel contacts and act on the bound ones.
///
/// WHY A THREAD AND NOT A POLL. `BezelReader::next_press` blocks in `read` on
/// the evdev node, which is what an idle daemon should do: no timer, no wakeups
/// between presses, nothing to tune.
///
/// LOGGING BUDGET. kmsg rate-limits to ten records per five seconds PER FD and
/// a Rust `eprintln!` costs three, so this logs the connect line once and then
/// only errors. Presses go to the ring where `getRecentButtonEvents` can fetch
/// them; logging each one would silently evict the lines that matter, which is
/// exactly how D3-0109 lost the only MCU frame of the boot.
///
/// The connect line is deliberately present: the FM seek worker logs only on
/// failure, and when it landed, "no error appeared" was the only evidence it
/// was alive. That is not evidence. This says which node it opened.
fn spawn_bezel_reader(
    buttons: Arc<Mutex<crate::buttons::ButtonService>>,
    started: Instant,
    core: Arc<Mutex<ProductionCore>>,
    callback_registry: Arc<CallbackRegistry>,
    callback_mailbox: Arc<CallbackMailbox>,
    inject_keys: bool,
    action_registry: Arc<BinderActionRegistry>,
) {
    let result = thread::Builder::new()
        .name("xyauto-bezel".into())
        .spawn(move || {
            let mut sleeper = crate::usb::ThreadSleeper;
            let (mut reader, tries) = match crate::bezel::open_touch_with(
                Path::new("/sys/class/input"),
                crate::bezel::TOUCH_NEEDLES,
                crate::bezel::OPEN_ATTEMPTS,
                crate::bezel::OPEN_RETRY_DELAY,
                &mut sleeper,
                crate::bezel::BezelReader::open,
            ) {
                Ok(opened) => opened,
                Err(crate::bezel::OpenTouchError::Resolution(error)) => {
                    log_both(true, format!("bezel: refusing to guess a node: {error}"));
                    return;
                }
                Err(crate::bezel::OpenTouchError::Failed { attempts, error }) => {
                    log_both(
                        true,
                        format!(
                            "bezel: no usable touchscreen node after {attempts} attempts over {}s: {error}; reader idle for this boot",
                            crate::bezel::OPEN_RETRY_DELAY.as_secs()
                                * u64::from(attempts.saturating_sub(1))
                        ),
                    );
                    return;
                }
            };
            if tries > 1 {
                log_both(
                    false,
                    format!(
                        "bezel: node became usable on attempt {tries} of {}",
                        crate::bezel::OPEN_ATTEMPTS
                    ),
                );
            }
            log_both(
                false,
                format!("bezel: reading {} (observe-only until bound)", reader.node().display()),
            );

            // Opened on FIRST INJECTION, never at startup. With no bindings the
            // daemon must not create a virtual keyboard at all: an input device
            // that exists but never emits is a thing to debug later, and until
            // an app has taught us a mapping there is nothing to emit.
            let mut emitter: Option<crate::mcu::uinput::UinputDevice> = None;
            // Says the injection gate closed, once, the first time it matters.
            let mut logged_injection_disabled = false;

            loop {
                let press = match reader.next_press() {
                    Ok(Some(press)) => press,
                    Ok(None) => {
                        log_both(false, "bezel: input stream ended");
                        return;
                    }
                    Err(error) => {
                        log_both(true, format!("bezel: read failed: {error}"));
                        return;
                    }
                };

                let now = i64_saturating(
                    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
                );
                // The lock is held only for the match, never across the blocking
                // read above, so a Binder call is never waiting on a finger.
                // One call, under the lock, deciding everything; the lock
                // covers nothing else. Never the blocking read above, never the
                // Binder transaction below: a finger must not be able to make a
                // Binder call wait, and a Binder call must not be able to make
                // a finger wait.
                //
                // The decision itself lives in buttons::handle_press so it can
                // be driven at the desk over a recorded evdev stream. What is
                // left here is only the acting-on-it, which needs the hardware.
                let action = match buttons.lock() {
                    Ok(mut service) => action_registry.handle_press(
                        &mut service,
                        press.x,
                        press.y,
                        now,
                        inject_keys,
                    ),
                    Err(_) => {
                        log_both(true, "bezel: button mutex poisoned; reader stopping");
                        return;
                    }
                };
                let delivery = action.delivery;

                // Push to IHudCallback.onButtonEvent. This is what makes a
                // setup wizard's live test mode possible: without it a press
                // is only discoverable by polling getRecentButtonEvents, and
                // in learn mode -- where injection is suppressed on purpose --
                // the push is the ONLY signal that the press happened.
                //
                // Deliberately no per-press log line. The daemon's stderr is
                // /dev/kmsg and devkmsg_write() allows 10 records per 5 s per
                // fd, silently; a line per press would spend the whole boot's
                // budget on a finger (mem:bengal-kmsg-ratelimit-stdio-to-kmsg-
                // silent-drops). Only a LOSS is worth saying, and only once.
                if delivery.dropped > 0 {
                    log_both(
                        true,
                        format!(
                            "bezel: {} press(es) dropped before delivery; the app must re-read getRecentButtonEvents",
                            delivery.dropped
                        ),
                    );
                }
                if !delivery.events.is_empty() {
                    if let Some(token) = callback_registry.current_token() {
                        for event in delivery.events {
                            callback_mailbox.push(token, CallbackPayload::Button(event));
                        }
                    }
                    // No subscriber => the events are simply dropped here, and
                    // that is correct: getRecentButtonEvents is the documented
                    // recovery for anything an app was not around to be told.
                    //
                    // Both locks are released before this transacts.
                    flush_callback_mailbox(&callback_registry, &callback_mailbox, |identity| {
                        match core.lock() {
                            Ok(mut service) => service.unsubscribe(identity),
                            Err(_) => {
                                log_both(true, "callback core eviction skipped mutex=poisoned")
                            }
                        }
                    });
                }

                // `None` covers both "unbound" and "learn mode". Either way the
                // press was recorded above and has just been pushed to the app;
                // only the injection stops here.
                let key = match action.emit {
                    crate::buttons::EmitDecision::Emit(key) => key,
                    crate::buttons::EmitDecision::Unbound => continue,
                    crate::buttons::EmitDecision::InjectionDisabled => {
                        // ONCE per boot, not per press: stderr is /dev/kmsg at
                        // 10 records per 5 s per fd, and a bezel button held by
                        // a passenger would otherwise spend the whole budget.
                        if !logged_injection_disabled {
                            logged_injection_disabled = true;
                            log_both(
                                false,
                                format!(
                                    "bezel: bound press not injected property={INJECT_KEYS_PROPERTY} value=off; observation and onButtonEvent continue"
                                ),
                            );
                        }
                        continue;
                    }
                    crate::buttons::EmitDecision::NotEmittable { android_key_code } => {
                        log_both(
                            true,
                            format!("bezel: no emittable key for android code {android_key_code}"),
                        );
                        continue;
                    }
                };

                if emitter.is_none() {
                    match crate::mcu::uinput::UinputDevice::open(Path::new("/dev/uinput")) {
                        Ok(device) => {
                            log_both(false, "bezel: uinput opened for bound keys");
                            emitter = Some(device);
                        }
                        Err(error) => {
                            log_both(true, format!("bezel: cannot open /dev/uinput: {error}"));
                            continue;
                        }
                    }
                }
                if let Some(device) = emitter.as_mut() {
                    let key = match key {
                        crate::buttons::BoundKey::Home => crate::mcu::uinput::Key::Home,
                        crate::buttons::BoundKey::Back => crate::mcu::uinput::Key::Back,
                        crate::buttons::BoundKey::VolumeUp => crate::mcu::uinput::Key::VolumeUp,
                        crate::buttons::BoundKey::VolumeDown => crate::mcu::uinput::Key::VolumeDown,
                    };
                    // A press the owner already finished: emit down then up.
                    use crate::mcu::uinput::KeyEmitter as _;
                    // Down then up as two real statements. Chaining them
                    // through `and_then` would borrow `device` mutably twice --
                    // once for the call, once inside the closure -- and not
                    // compile. The release is skipped if the press failed, so a
                    // dead uinput fd cannot leave a key stuck down.
                    let mut emitted = device.emit(key, true);
                    if emitted.is_ok() {
                        emitted = device.emit(key, false);
                    }
                    if let Err(error) = emitted {
                        log_both(true, format!("bezel: emit failed: {error}"));
                        emitter = None;
                    }
                }
            }
        });
    if let Err(error) = result {
        log_both(true, format!("bezel: cannot spawn reader: {error}"));
    }
}

fn spawn_fm_seek_worker(fm: Arc<Mutex<ProductionFm>>) {
    /// THE WAIT THAT MAKES A SEEK WORK, and the one D3-0117 did not have.
    ///
    // The model requests each dwell explicitly. Full scoring and adaptive
    // screening both sleep here, after the service mutex has been released.
    /// Between judging one channel and tuning the next. Bus courtesy only.
    const STEP_INTERVAL: Duration = Duration::from_millis(10);
    /// Nothing queued: sleep longer so an idle radio costs nothing.
    const IDLE_INTERVAL: Duration = Duration::from_millis(250);
    /// Between RDS polls inside an open window. Device 2's bridge cadence
    /// (`radio.go:409`); see `qn8035::RDS_POLL_MS` for why it is a starting
    /// point and not a proof. Only consulted when no seek is in flight, so a
    /// walk's settle timing is never disturbed by a name read.
    const RDS_POLL_INTERVAL: Duration = Duration::from_millis(qn8035::RDS_POLL_MS);

    let result = thread::Builder::new()
        .name("xyauto-fm-seek".into())
        .spawn(move || {
            // The RDS window's clock. Monotonic, ms, from the worker's birth;
            // the service sets each window's deadline from the first poll it
            // sees, so this only has to be consistent, not aligned to anything.
            let started = Instant::now();
            loop {
                let (seek, rds) = match fm.lock() {
                    Ok(mut service) => {
                        let seek = service.poll_seek();
                        // A walk owns the tuner; RDS only runs between walks.
                        let rds = if seek == SeekProgress::Idle {
                            service.poll_rds(started.elapsed().as_millis() as u64)
                        } else {
                            RdsProgress::Idle
                        };
                        (seek, rds)
                    }
                    Err(_) => {
                        // A poisoned lock means another thread panicked holding it;
                        // the worker cannot recover the tuner, and spinning on it
                        // would only produce noise.
                        log_both(true, "fm seek worker: service mutex poisoned; worker exiting");
                        return;
                    }
                };
                // The lock is dropped by here, so the settle costs no Binder call
                // and an RDS window never queues a tune behind it.
                thread::sleep(match (seek, rds) {
                    (SeekProgress::Settling(wait_ms), _) => Duration::from_millis(wait_ms),
                    (SeekProgress::Stepping, _) => STEP_INTERVAL,
                    (SeekProgress::Idle, RdsProgress::Polling) => RDS_POLL_INTERVAL,
                    (SeekProgress::Idle, _) => IDLE_INTERVAL,
                });
            }
        });
    if let Err(error) = result {
        log_both(
            true,
            format!("fm seek worker could not start; seek stays unavailable error={error}"),
        );
    }
}

fn spawn_audio_guard_activation(audio_guard: Arc<Mutex<crate::dsp::guard::AudioGuard>>) {
    let result = thread::Builder::new()
        .name("xyauto-audio-guard-activate".into())
        .spawn(move || {
            let result = audio_guard
                .lock()
                .map_err(|_| "audio guard owner mutex poisoned".to_owned())
                .and_then(|mut guard| guard.ensure_ready().map_err(|error| error.to_string()));
            match result {
                Ok(()) => log_both(false, "audio guard activation completed"),
                Err(error) => log_both(
                    true,
                    format!("audio guard activation failed safely; status remains visible error={error}"),
                ),
            }
        });
    if let Err(error) = result {
        log_both(
            true,
            format!(
                "audio guard activation thread could not start; status remains inert error={error}"
            ),
        );
    }
}

struct BinderHud {
    core: Arc<Mutex<ProductionCore>>,
    /// Bezel/MCU button state. Separate from `core` because it is fed by its
    /// own reader thread and shares none of the core's hardware.
    buttons: Arc<Mutex<crate::buttons::ButtonService>>,
    action_registry: Arc<BinderActionRegistry>,
    started: Instant,
    callback_registry: Arc<CallbackRegistry>,
    callback_mailbox: Arc<CallbackMailbox>,
}

fn select_mcu_path(
    sysfs_root: &Path,
    configured_fallback: &Path,
) -> Result<(PathBuf, bool), String> {
    match resolve_port(sysfs_root, SE0_MMIO) {
        Ok(Some(path)) => Ok((path, true)),
        Err(error) => Err(format!("SE0 MMIO resolution refused: {error}")),
        Ok(None) => {
            // Refuse what sysfs proves wrong; fall back only when sysfs can
            // prove nothing about the configured enumeration name.
            match resolve_port(sysfs_root, SE3_MMIO_NOT_OURS) {
                Ok(Some(path)) if path == configured_fallback => Err(format!(
                    "configured-name fallback={} is Bluetooth SE3 MMIO=0x{}",
                    configured_fallback.display(),
                    SE3_MMIO_NOT_OURS
                )),
                Err(error) => Err(format!("SE3 MMIO resolution refused: {error}")),
                _ => Ok((configured_fallback.to_path_buf(), false)),
            }
        }
    }
}

fn bool_property(name: &str) -> bool {
    let Ok(name) = CString::new(name) else {
        return false;
    };
    let mut value = [0 as c_char; PROP_VALUE_MAX];
    // SAFETY: both pointers are NUL-terminated/writable for the duration of
    // bionic's bounded property copy.
    let length = unsafe { __system_property_get(name.as_ptr(), value.as_mut_ptr()) };
    if length <= 0 {
        return false;
    }
    // SAFETY: __system_property_get writes a trailing NUL into this fixed-size
    // buffer when it reports a positive length.
    let value = unsafe { CStr::from_ptr(value.as_ptr()) }.to_bytes();
    matches!(value, b"1" | b"true" | b"y" | b"yes" | b"on")
}

/// Same bounded property read, for a decimal `u32`; absent or unparsable → `None`
/// (the caller's default applies — a typo can never switch a feature off).
fn uint_property(name: &str) -> Option<u32> {
    let Ok(name) = CString::new(name) else {
        return None;
    };
    let mut value = [0 as c_char; PROP_VALUE_MAX];
    // SAFETY: both pointers are NUL-terminated/writable for the duration of
    // bionic's bounded property copy.
    let length = unsafe { __system_property_get(name.as_ptr(), value.as_mut_ptr()) };
    if length <= 0 {
        return None;
    }
    // SAFETY: __system_property_get writes a trailing NUL into this fixed-size
    // buffer when it reports a positive length.
    let value = unsafe { CStr::from_ptr(value.as_ptr()) }.to_string_lossy();
    crate::cpu_latency::parse_knob(&value)
}

/// ACC as the frame pipeline's early-release signal: `Some(false)` from the
/// first accepted `12 03` on. `Unknown` (MCU silent so far) is not a veto.
fn acc_on(core: &Arc<Mutex<ProductionCore>>) -> Option<bool> {
    let status = core
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_status();
    match status.acc_state {
        core::AccState::Unknown => None,
        core::AccState::On => Some(true),
        core::AccState::OffGrace | core::AccState::FinalStage | core::AccState::Suspending => {
            Some(false)
        }
    }
}

/// CAN has its own thread and uses the existing dual log sink. Binder only
/// queues typed status refreshes; this thread remains the sole UART owner.
///
/// Steering-wheel keys arrive through the same sink:
/// `can::keys::CanKeyInjector` is the only path from a key frame to uinput,
/// behind `can::KEYS_PROPERTY` (default off = no external action) and the bezel
/// path's `inject_keys` gate, for volume/mute/media/call/voice taps. Like the
/// bezel reader it opens its own uinput device on the first emitted tap, never
/// at startup.
fn spawn_can_reader(core: Arc<Mutex<ProductionCore>>, sysfs_root: PathBuf) {
    let session = format!("{}-{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default().as_nanos());
    let mut initial_monitor = crate::can::monitor::Monitor::new(session);
    initial_monitor.vehicle = crate::can::selection::VehicleSelection::load(
        Path::new("/vendor/etc/bengal/can/vehicles.tsv"),
        Path::new("/data/vendor/bengal-hud/can-vehicle"),
    );
    let monitor = Arc::new(Mutex::new(initial_monitor));
    if let Err(error) = crate::can_binder::register(monitor.clone()) {
        log_both(true, format!("canuart: monitor registration failed: {error}"));
    }
    struct LogSink {
        keys: crate::can::keys::CanKeyInjector<crate::mcu::uinput::UinputDevice>,
    }
    impl crate::can::reader::Sink for LogSink {
        fn vehicle_changed(&mut self, _revision: u64) {
            self.keys.reset();
        }
        fn line(&mut self, error: bool, message: &str) {
            log_both(error, message);
        }
        fn key(&mut self, key: &crate::can::decode::KeyEvent, now_ms: u64) {
            // Per key frame, so `setprop` acts without a restart. Off is the
            // default: clear stale hold, but no emission, device open or line.
            let enabled = bool_property(crate::can::KEYS_PROPERTY);
            self.keys
                .handle(enabled, key, now_ms, &mut |error, message| log_both(error, message));
        }
    }
    // The same immutable boot property the bezel and MCU paths honour.
    let inject_keys = bool_property(INJECT_KEYS_PROPERTY);
    let result = thread::Builder::new().name("bengal-can-rx".into()).spawn(move || {
        let configured = std::env::var_os(crate::can::DEVICE_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(crate::can::DEFAULT_DEVICE));
        let mut sink = LogSink {
            keys: crate::can::keys::CanKeyInjector::new(
                inject_keys,
                Box::new(|| crate::mcu::uinput::UinputDevice::open_can_keys(Path::new(UINPUT_DEVICE))),
            ),
        };
        crate::can::runtime::run_observed_with_requests(
            &sysfs_root, &configured,
            || bool_property(crate::can::ENABLE_PROPERTY),
            || acc_on(&core),
            || bool_property(crate::can::REQUESTS_PROPERTY),
            &mut sink, monitor,
        );
    });
    if let Err(error) = result {
        log_both(true, format!("canuart: could not start RX thread: {error}"));
    }
}

/// D3-0120 confirmation vehicle for the cpuidle finding: while the DRM connector
/// says the panel is on and ACC has not been confirmed off, keep one fd on
/// `/dev/cpu_dma_latency` at `vendor.bengal.hud.cpu_latency_us` (default 500 µs);
/// release it within one poll interval of screen-off or ACC-off. Logs
/// transitions only — the per-fd kmsg rate limit (10 records / 5 s) makes
/// per-tick lines the surest way to lose the one that matters.
fn spawn_cpu_latency_hold(core: Arc<Mutex<ProductionCore>>, sysfs_root: PathBuf) {
    use crate::cpu_latency::{
        decide, DevNode, DisplayProbe as _, DrmDpms, Governor, Transition, DEFAULT_MICROS,
        NODE_PATH, POLL_INTERVAL, PROPERTY,
    };

    let result = thread::Builder::new()
        .name("xyauto-cpu-latency".into())
        .spawn(move || {
            let Some(mut display) = DrmDpms::discover(&sysfs_root) else {
                // Not fatal: a unit without a DRM connector has no panel to hold
                // for. Say so once, stop, and never hold blind.
                log_both(
                    true,
                    format!(
                        "cpu-latency: no dpms node under {}; hold disabled",
                        sysfs_root.join("class/drm").display()
                    ),
                );
                return;
            };
            log_both(
                false,
                format!(
                    "cpu-latency: watching {} node={} default_us={} knob={}",
                    display.path().display(),
                    NODE_PATH,
                    DEFAULT_MICROS,
                    PROPERTY
                ),
            );
            let mut governor = Governor::new(DevNode::default_node());
            let started = Instant::now();
            loop {
                let micros = uint_property(PROPERTY).unwrap_or(DEFAULT_MICROS);
                let display_on = display.is_on();
                let acc = acc_on(&core);
                let decision = decide(display_on, acc, micros);
                let now_ms = started.elapsed().as_millis() as u64;
                let describe = |value: Option<bool>| match value {
                    Some(true) => "on",
                    Some(false) => "off",
                    None => "unknown",
                };
                match governor.step(decision, now_ms) {
                    Some(Transition::Held { micros }) => log_both(
                        false,
                        format!(
                            "cpu-latency: hold {micros}us display={} acc={}",
                            describe(display_on),
                            describe(acc)
                        ),
                    ),
                    Some(Transition::Released) => log_both(
                        false,
                        format!(
                            "cpu-latency: release display={} acc={} knob_us={micros}",
                            describe(display_on),
                            describe(acc)
                        ),
                    ),
                    Some(Transition::HoldFailed { micros, error }) => log_both(
                        true,
                        format!(
                            "cpu-latency: hold {micros}us FAILED node={NODE_PATH} error={error}"
                        ),
                    ),
                    None => {}
                }
                thread::sleep(POLL_INTERVAL);
            }
        });
    if let Err(error) = result {
        log_both(true, format!("cpu-latency: thread spawn failed: {error}"));
    }
}

/// The liblog tag, NUL-terminated for `__android_log_write`.
///
/// It must stay identical to [`crate::logsink::LOG_TAG`], which is the `tag: `
/// prefix `LogKlog` parses back out of the kernel record — if the two drifted,
/// the same line would arrive under two different tags in the two sinks and
/// nobody would notice until a capture was already wasted. The const block
/// below makes that a compile error instead; it is checked at the desk by
/// `platform/scripts/hud-binder-desk-test.sh`, which compiles this module.
///
/// The check is an inline `const _` block rather than a `const fn` called from
/// one: Soong compiles this crate with clippy under `-D warnings`, and rustc
/// 1.88's dead-code lint reports a function whose only caller is an anonymous
/// const item as never used (vendor build 2026-09-03 23:07, invocation
/// a006d202…). A const block has no name to be "unused".
const LIBLOG_TAG: &[u8] = b"bengal-hud\0";

const _: () = {
    let text = crate::logsink::LOG_TAG.as_bytes();
    assert!(
        LIBLOG_TAG.len() == text.len() + 1,
        "the liblog tag and logsink::LOG_TAG must be the same string (length)"
    );
    let mut i = 0;
    while i < text.len() {
        assert!(
            LIBLOG_TAG[i] == text[i],
            "the liblog tag and logsink::LOG_TAG must be the same string (byte)"
        );
        i += 1;
    }
    assert!(
        LIBLOG_TAG[text.len()] == 0,
        "the liblog tag must be NUL-terminated"
    );
};

/// Emit one message to logcat (liblog) and to the kernel ring (stderr, which
/// `bengal-hud.rc`'s `stdio_to_kmsg` points at `/dev/kmsg`).
///
/// **The kmsg half is deliberately a single `write_all`.** It used to be
/// `eprintln!("bengal-hud: {}", message)`, and because Rust's `Stderr` is
/// unbuffered that issued one `write()` — hence one kernel record — per format
/// fragment: three per line. `/dev/kmsg` rate-limits at 10 records per 5 s per
/// open fd and never reports the drops for a long-lived fd, so the daemon's
/// real budget was about three log lines per five seconds and the losses were
/// invisible. On D3-0109 that destroyed the `mcu-rx` line for the only MCU
/// frame of the whole boot. See [`crate::logsink`] and
/// `docs/research/2026-09-03-mcu-button-capture-prep.md` §1.
///
/// A failed kmsg write is deliberately ignored: logging must never take down a
/// hardware daemon, and the liblog copy below is the independent sink.
fn log_both(error: bool, message: impl AsRef<str>) {
    use std::io::Write as _;

    let message = message.as_ref();
    let line = crate::logsink::kmsg_line(error, message);
    let _ = std::io::stderr().write_all(line.as_bytes());
    let sanitized = message.replace('\0', "?");
    if let Ok(text) = CString::new(sanitized) {
        // SAFETY: tag is a static NUL-terminated string and text owns a valid
        // NUL-terminated buffer for this synchronous liblog call.
        unsafe {
            __android_log_write(
                if error {
                    ANDROID_LOG_ERROR
                } else {
                    ANDROID_LOG_INFO
                },
                LIBLOG_TAG.as_ptr().cast(),
                text.as_ptr(),
            );
        }
    }
}

fn spawn_mcu_reader(
    core: Arc<Mutex<ProductionCore>>,
    started: Instant,
    sysfs_root: PathBuf,
    mcu_device: PathBuf,
    inject_keys: bool,
    callback_registry: Arc<CallbackRegistry>,
    callback_mailbox: Arc<CallbackMailbox>,
    #[cfg(feature = "acc-power-channel")]
    acc_power: Option<Arc<crate::acc_power::Channel>>,
) {
    let result = thread::Builder::new()
        .name("xyauto-mcu-reader".into())
        .spawn(move || {
            mcu_reader(
                core,
                started,
                sysfs_root,
                mcu_device,
                inject_keys,
                callback_registry,
                callback_mailbox,
                #[cfg(feature = "acc-power-channel")]
                acc_power,
            )
        });
    if let Err(error) = result {
        log_both(true, format!("mcu-reader spawn failed error={}", error));
    }
}

/// Local work emitted by the UART-owned ACC machine. It is deliberately not a
/// Binder payload: the reader drains it only after the serial operation has
/// returned, then the existing callback mailbox delivers it after the core and
/// registry locks are gone.
enum AccControl {
    State {
        state: AccMachineState,
        reason: &'static str,
        now_ms: u64,
    },
    SleepRequest {
        now_ms: u64,
    },
}

#[derive(Default)]
struct AccControlMailbox {
    queue: Mutex<VecDeque<AccControl>>,
}

impl AccControlMailbox {
    fn push(&self, control: AccControl) {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(control);
    }

    fn drain(&self) -> Vec<AccControl> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .collect()
    }
}

// Diagnostic records have their OWN bounded storage: never crowd out a State
// or SleepRequest. No allocation, formatting, sink I/O or blocking lock in push.
const ACC_TX_CAPACITY: usize = 32;

#[derive(Default)]
struct AccTxMailbox {
    queue: Mutex<[Option<AccTxObservation>; ACC_TX_CAPACITY]>,
    dropped: AtomicU64,
}

impl AccTxMailbox {
    fn push(&self, observation: AccTxObservation) {
        if let Ok(mut queue) = self.queue.try_lock() {
            if let Some(slot) = queue.iter_mut().find(|slot| slot.is_none()) {
                *slot = Some(observation);
                return;
            }
        }
        // Full, contended or poisoned: lose only evidence, never a control.
        let _ = self.dropped.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            Some(n.saturating_add(1))
        });
    }

    fn drain(&self) -> ([Option<AccTxObservation>; ACC_TX_CAPACITY], u64) {
        let records = std::mem::replace(
            &mut *self.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
            [None; ACC_TX_CAPACITY],
        );
        (records, self.dropped.swap(0, Ordering::Relaxed))
    }
}

// Called only after the serial operation returns, outside HUD core/control/
// diagnostic mailbox locks. Both existing sinks remain best-effort. Even the
// "attempted" record is deferred, NOT a durable prewrite/power-loss bookend.
fn flush_acc_tx(mailbox: &AccTxMailbox) {
    let (records, dropped) = mailbox.drain();
    for record in records.into_iter().flatten() {
        log_both(
            matches!(record.phase, AccTxPhase::Refused | AccTxPhase::WriteError { .. }),
            record.to_string(),
        );
    }
    if dropped != 0 {
        log_both(true, format!("mcu-acc-tx evidence-lost dropped_at_least={dropped} delivery=unknown"));
    }
}

#[cfg(feature = "acc-power-channel")]
struct NativeAccOwners {
    audio: Arc<Mutex<crate::dsp::guard::AudioGuard>>,
    fm: Arc<Mutex<ProductionFm>>,
    deep: Box<dyn crate::acc_power::participants::DeepOwner>,
}

#[cfg(feature = "acc-power-channel")]
impl crate::acc_power::participants::Owner for NativeAccOwners {
    fn arm(&mut self, token: u64, deadline_ms: u64) -> bool { self.deep.arm(token, deadline_ms) }
    fn disarm(&mut self, token: u64) -> bool { self.deep.disarm(token) }
    fn hold(&mut self, token: u64) -> bool {
        let result = (|| -> Result<(), String> {
            self.audio.lock().map_err(|_| "audio mutex poisoned")?
                .acc_operation(token, 1).map_err(|e| e.to_string())?;
            self.fm.lock().map_err(|_| "FM mutex poisoned")?
                .acc_hold(token).map_err(|e| e.to_string())?;
            // HUD has no removable-file sink: its actual sinks are kmsg,
            // liblog and CAN's fixed /data/vendor path. No detach is invented.
            // External collectors and public-volume unmount belong to Android.
            self.audio.lock().map_err(|_| "audio mutex poisoned")?
                .acc_operation(token, 3).map_err(|e| e.to_string())
        })();
        log_both(result.is_err(), format!("acc-native hold token={token} result={result:?} hud_removable_sinks=0"));
        result.is_ok()
    }
    fn confirmed(&mut self, token: u64) -> bool {
        self.audio.lock().ok().is_some_and(|mut audio| audio.acc_operation(token, 3).is_ok())
    }
    fn release(&mut self, token: u64) -> bool {
        // Keep physical mute while the same FM owner restores only its saved
        // intent. A user stop/lease death during hold has revoked that replay.
        // A refused release is recovered like a daemon restart would
        // (acc_power::release): FAULT_SAFE, then the boot activation.
        use crate::acc_power::release::{release_with_recovery, ReleaseOutcome};
        let outcome = release_with_recovery(&self.audio, &self.fm, token);
        let result: Result<(), &str> = match &outcome {
            ReleaseOutcome::Released => Ok(()),
            ReleaseOutcome::Recovered { cause, .. } | ReleaseOutcome::Failed { cause, .. } => {
                Err(cause.as_str())
            }
        };
        // Unchanged record shape: readers key on this exact line.
        log_both(result.is_err(), format!("acc-native release token={token} result={result:?}"));
        match &outcome {
            ReleaseOutcome::Released => {}
            ReleaseOutcome::Recovered { notes, .. } => log_both(
                !notes.is_empty(),
                format!(
                    "acc-native release recovery token={token} result=Ok(()) \
                     action=fault-safe+dsp-reload fm=route-lost notes={notes:?}"
                ),
            ),
            ReleaseOutcome::Failed { recovery, notes, .. } => log_both(
                true,
                format!(
                    "acc-native release recovery token={token} result=Err({recovery:?}) \
                     outputs=fenced-or-safe notes={notes:?}"
                ),
            ),
        }
        outcome.released()
    }
}

/// The FM service as the release orchestration sees it. Inherent methods are
/// named explicitly so the trait can never recurse into itself.
#[cfg(feature = "acc-power-channel")]
impl crate::acc_power::release::FmAccParticipant for ProductionFm {
    fn acc_release(&mut self, token: u64) -> Result<(), String> {
        xyauto_fm::service::FmService::acc_release(self, token).map_err(|error| error.to_string())
    }
    fn audio_route_lost(&mut self, rails_live: bool) -> Result<(), String> {
        xyauto_fm::service::FmService::audio_route_lost(self, rails_live)
            .map_err(|error| error.to_string())
    }
    fn tuner_lost(&mut self, reason: &str) {
        xyauto_fm::service::FmService::tuner_lost(self, reason);
    }
}

/// Reader-local hooks have no blocking platform teardown. They queue TX
/// evidence separately from confirmed state and final-stage sleep intent for
/// the system-side consumer that will be wired later.
struct ReaderHooks {
    control: Arc<AccControlMailbox>,
    tx_evidence: Arc<AccTxMailbox>,
    #[cfg(feature = "acc-power-channel")]
    power: Option<crate::acc_power::Producer>,
}

impl ReaderHooks {
    fn new(control: Arc<AccControlMailbox>, tx_evidence: Arc<AccTxMailbox>) -> Self {
        Self {
            control, tx_evidence,
            #[cfg(feature = "acc-power-channel")]
            power: None,
        }
    }
}

impl PlatformHooks for ReaderHooks {
    #[cfg(feature = "acc-power-channel")]
    fn on_cycle_event(&mut self, event: crate::mcu::acc::CycleEvent) {
        if let Some(power) = self.power.as_mut() { power.event(event); }
    }

    fn observe_acc_tx(&mut self, observation: AccTxObservation) {
        self.tx_evidence.push(observation);
    }

    fn enter_acc_off_grace(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn quiesce_radio(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn detach_stick_sink(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn quiesce_usb(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn resume(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn publish_acc_state(
        &mut self,
        state: AccMachineState,
        reason: &'static str,
        now_ms: u64,
    ) -> std::io::Result<()> {
        self.control.push(AccControl::State {
            state,
            reason,
            now_ms,
        });
        Ok(())
    }

    fn request_sleep(&mut self, now_ms: u64) -> std::io::Result<()> {
        self.control.push(AccControl::SleepRequest { now_ms });
        Ok(())
    }
}

fn mcu_reader(
    core: Arc<Mutex<ProductionCore>>,
    started: Instant,
    sysfs_root: PathBuf,
    mcu_device: PathBuf,
    inject_keys: bool,
    callback_registry: Arc<CallbackRegistry>,
    callback_mailbox: Arc<CallbackMailbox>,
    #[cfg(feature = "acc-power-channel")]
    acc_power: Option<Arc<crate::acc_power::Channel>>,
) {
    let diagnostics = Arc::clone(&core.lock().unwrap_or_else(|p| p.into_inner()).mcu_diagnostics);
    let mut diagnostic_session = 0u64;
    let mut gps_ready = crate::mcu::gps_ready::Scheduler::default();
    let mut gps_ready_tx = crate::mcu::diagnostics::GpsReadyTxEvidence::default();
    let acc_control = Arc::new(AccControlMailbox::default());
    let acc_tx_evidence = Arc::new(AccTxMailbox::default());
    let mut injector = if !inject_keys {
        log_both(
            false,
            format!(
                "uinput disabled property={} value=0 mapping_confirmed={}",
                INJECT_KEYS_PROPERTY, DEVICE3_MAPPING_CONFIRMED
            ),
        );
        None
    } else if !DEVICE3_MAPPING_CONFIRMED {
        log_both(
            true,
            "uinput REFUSED property=1 mapping=PROVISIONAL device3 codes unknown",
        );
        None
    } else {
        match UinputDevice::open(std::path::Path::new(UINPUT_DEVICE)) {
            Ok(device) => {
                log_both(
                    false,
                    "uinput ready keys=HOME,BACK,VOLUMEUP,VOLUMEDOWN power=excluded",
                );
                Some(ButtonInjector::new(device, DEVICE3_BUTTONS, true))
            }
            Err(error) => {
                log_both(
                    true,
                    format!("uinput open failed path={} error={}", UINPUT_DEVICE, error),
                );
                None
            }
        }
    };

    loop {
        gps_ready.disconnected();
        let (selected_path, by_mmio) = match select_mcu_path(&sysfs_root, &mcu_device) {
            Ok(selection) => selection,
            Err(reason) => {
                set_mcu_connected(&core, false);
                log_both(
                    true,
                    format!(
                        "mcu tty selection REFUSED root={} configured-fallback={} reason={}",
                        sysfs_root.display(),
                        mcu_device.display(),
                        reason
                    ),
                );
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        if by_mmio {
            log_both(
                false,
                format!(
                    "mcu tty selected authority=se0-mmio path={} configured-fallback={}",
                    selected_path.display(),
                    mcu_device.display()
                ),
            );
        } else {
            log_both(
                true,
                format!(
                    "mcu SE0 MMIO RESOLUTION FAILED root={} mmio=0x{}; using configured-name fallback={}",
                    sysfs_root.display(),
                    SE0_MMIO,
                    mcu_device.display()
                ),
            );
        }
        #[cfg(feature = "acc-power-channel")]
        let opened = {
            let mut hooks = ReaderHooks::new(Arc::clone(&acc_control), Arc::clone(&acc_tx_evidence));
            // Each replacement AccSession receives a fresh random envelope before
            // construction. Missing channel/entropy never changes startup frames.
            hooks.power = acc_power.as_ref().and_then(|channel| channel.new_uart());
            if by_mmio {
                AccSession::open_se0(&sysfs_root, hooks)
            } else {
                AccSession::open_path(&selected_path, hooks)
            }
        };
        #[cfg(not(feature = "acc-power-channel"))]
        let opened = if by_mmio {
            AccSession::open_se0(&sysfs_root, ReaderHooks::new(Arc::clone(&acc_control), Arc::clone(&acc_tx_evidence)))
        } else {
            AccSession::open_path(&selected_path, ReaderHooks::new(Arc::clone(&acc_control), Arc::clone(&acc_tx_evidence)))
        };
        let mut session = match opened {
            Ok(session) => session,
            Err(error) => {
                set_mcu_connected(&core, false);
                log_both(
                    true,
                    format!(
                        "mcu open failed path={} authority={} error={}",
                        selected_path.display(),
                        if by_mmio { "se0-mmio" } else { "name-fallback" },
                        error
                    ),
                );
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };

        diagnostic_session = diagnostic_session.saturating_add(1);
        session.set_diagnostic_session(diagnostic_session);
        diagnostics.publish(session.diagnostics().with_gps_ready(gps_ready_tx));
        if let Err(error) = session.send_version_query() {
            set_mcu_connected(&core, false);
            log_both(
                true,
                format!("mcu tx failed name=version-query error={}", error),
            );
            thread::sleep(Duration::from_secs(2));
            continue;
        }
        log_both(false, "mcu-tx name=version-query");
        // Copied from mcu-toolbox's run(): let the silent peer enter its active
        // protocol state before sending the boot acknowledgement.
        thread::sleep(Duration::from_millis(750));
        if let Err(error) = session.send_startup_acc_on_answer() {
            set_mcu_connected(&core, false);
            log_both(
                true,
                format!("mcu tx failed name=acc-on-answer error={}", error),
            );
            thread::sleep(Duration::from_secs(2));
            continue;
        }
        log_both(false, "mcu-tx name=acc-on-answer");
        set_mcu_connected(&core, true);
        log_both(
            false,
            format!(
                "mcu connected path={} authority={} uart=115200-8N1-raw tx=version-query,acc-on-answer",
                selected_path.display(),
                if by_mmio { "se0-mmio" } else { "name-fallback" }
            ),
        );

        // Boot eligibility is explicit: startup answer succeeded above; the
        // live parser must still observe a valid version on this connection.
        let mut gps_version_seen = false;
        let _ = gps_ready.poll(diagnostic_session, session.generation(),
            session.state(), false, elapsed_ms(started));
        #[cfg(feature = "acc-power-channel")]
        let mut wake_monitor = crate::acc_power::wake::Monitor::new(crate::acc_power::wake::Sysfs);
        loop {
            #[cfg(feature = "acc-power-channel")]
            let readable = if session.platform_mut().power.is_some() {
                session.wait_acc_input()
            } else { Ok(true) }; // runtime unavailable preserves legacy read cadence
            // Sample AFTER the enabled poll, never before a potentially quiet wait.
            let now_ms = elapsed_ms(started);
            let mut observed = Vec::new();
            #[cfg(feature = "acc-power-channel")]
            {
                let (state, generation) = (session.state(), session.generation());
                if let Some(power) = session.platform_mut().power.as_mut() {
                    match crate::acc_power::boottime_ms() {
                        Ok(sample) => power.checkpoint(state, generation, sample),
                        Err(_) => power.invalidate(),
                    }
                }
            }
            #[cfg(feature = "acc-power-channel")]
            if let Some(channel) = &acc_power { channel.notify(); }
            // The accepted READY arm request is now visible to the preparation
            // worker, outside the MCU hook and before any read/advance operation.
            let observe = |frame: &crate::mcu::Frame, validity: FrameValidity| {
                let event = frame.event();
                let decoded = match (&event, validity) {
                    (_, FrameValidity::BadChecksum) => "not-decoded(bad-checksum)".to_string(),
                    (Event::Unknown, _) => "unknown".to_string(),
                    // These names come from the copied device-1 protocol oracle.
                    // They help recognise the frame family during Capture B but
                    // are not a device-3 key assignment.
                    (Event::Button { .. } | Event::Knob { .. }, _) => {
                        format!("device1-oracle:{:?};device3-map=PROVISIONAL", event)
                    }
                    _ => format!("{:?}", event),
                };
                log_both(
                    validity == FrameValidity::BadChecksum,
                    format!(
                        "mcu-rx elapsed_ms={} checksum={} command=0x{:02x} decoded={} frame={}",
                        now_ms,
                        if validity == FrameValidity::Valid {
                            "ok"
                        } else {
                            "bad"
                        },
                        frame.command(),
                        decoded,
                        frame
                    ),
                );

                observed.push((event, validity));
            };
            #[cfg(feature = "acc-power-channel")]
            let result = match readable {
                Ok(true) => session.read_once_observed(now_ms, observe),
                Ok(false) => session.advance(now_ms).map(|()| 0),
                Err(error) => Err(crate::mcu::serial::SessionError::Io(error)),
            };
            #[cfg(not(feature = "acc-power-channel"))]
            let result = session.read_once_observed(now_ms, observe);
            #[cfg(feature = "acc-power-channel")]
            let result = result.and_then(|count| {
                // Inside the same producer step, outside all MCU hooks/locks.
                // The real PMIC can wake Linux while the UART remains silent.
                if session.platform_mut().power.is_some() {
                    use crate::acc_power::wake::Observation;
                    match wake_monitor.step(&mut session, now_ms)? {
                        Observation::Resumed { success } => log_both(false,
                            format!("acc-wake PMIC completed_suspend={success} resume-generation={} source=kernel-status", session.generation())),
                        Observation::Unavailable(error) => log_both(true,
                            format!("acc-wake status unavailable: {error}")),
                        Observation::None => {}
                    }
                }
                Ok(count)
            });
            let result = result.and_then(|count| {
                gps_version_seen |= observed.iter().any(|(event, validity)|
                    *validity == FrameValidity::Valid && matches!(event, Event::Version { .. }));
                if let Some(notification) = gps_ready.poll(diagnostic_session,
                    session.generation(), session.state(), gps_version_seen, elapsed_ms(started)) {
                    let written = session.send_gps_ready();
                    gps_ready_tx.record_return(notification, diagnostic_session,
                        session.generation(), elapsed_ms(started), written.is_ok());
                    log_both(written.is_err(), format!(
                        "mcu-gps-ready reason={} sequence={} session={} generation={} phase={} boundary=write_all-return mcu_ack=unobserved",
                        notification.reason.label(), notification.sequence, diagnostic_session,
                        session.generation(), if written.is_ok() { "write-complete" } else { "write-error" }));
                    if let Err(error) = written {
                        gps_ready.disconnected();
                        return Err(error);
                    }
                }
                Ok(count)
            });
            #[cfg(feature = "acc-power-channel")]
            if let Some(power) = session.platform_mut().power.as_mut() {
                power.after_step(result.is_ok());
            }
            #[cfg(feature = "acc-power-channel")]
            if let Some(channel) = &acc_power { channel.notify(); }
            // The serial read, state-machine callbacks and UART writes above
            // have completed. Only now may the reader acquire the HUD core.
            diagnostics.publish(session.diagnostics().with_gps_ready(gps_ready_tx));
            flush_acc_tx(&acc_tx_evidence);
            let sleep_requests = {
                let mut sleep_requests = Vec::new();
                let mut service = core.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                for (event, validity) in observed {
                    if validity == FrameValidity::Valid {
                        service.record_mcu_event(&event, now_ms);
                        if let (Some(injector), Event::Button { code, state, .. }) =
                            (injector.as_mut(), &event)
                        {
                            if matches!(state, ButtonState::Press | ButtonState::Release) {
                                if let Err(error) = injector.handle(*code, *state) {
                                    log_both(true, format!("uinput emit failed error={}", error));
                                }
                            }
                        }
                    } else {
                        service.record_invalid_frame();
                    }
                }
                for control in acc_control.drain() {
                    match control {
                        AccControl::State {
                            state,
                            reason,
                            now_ms,
                        } => service.publish_acc_state(state.into(), reason, now_ms),
                        AccControl::SleepRequest { now_ms } => sleep_requests.push(now_ms),
                    }
                }
                sleep_requests
            };
            for request_ms in sleep_requests {
                log_both(
                    false,
                    format!("acc sleep-request queued elapsed_ms={request_ms} owner=system-side"),
                );
            }
            // HudService only queues Binder-facing notifications while its core
            // mutex is held. Transactions happen after both core and registry
            // locks have been released.
            flush_callback_mailbox(&callback_registry, &callback_mailbox, |identity| match core
                .lock()
            {
                Ok(mut service) => service.unsubscribe(identity),
                Err(_) => log_both(true, "callback core eviction skipped mutex=poisoned"),
            });
            if let Err(error) = result {
                set_mcu_connected(&core, false);
                log_both(
                    true,
                    format!("mcu read failed error={} retry_ms=2000", error),
                );
                break;
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn set_mcu_connected(core: &Arc<Mutex<ProductionCore>>, connected: bool) {
    core.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .set_mcu_connected(connected);
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

/// One linked Binder callback. Its recipient is retained for exactly as long
/// as this slot is current; dropping it after `unlink_to_death` is deliberate.
struct CallbackSlot {
    callback: Strong<dyn IHudCallback>,
    binder: SpIBinder,
    death: DeathRecipient,
}

/// Binder-facing owner for the V1 single callback slot.
///
/// Linking/unlinking and callback transactions are all deliberately outside
/// `state`: remote Binder code is allowed to re-enter us or block, whereas the
/// state lock only decides whether one exact token remains current.
struct CallbackRegistry {
    next_identity: AtomicU64,
    state: Mutex<GenerationRegistry<CallbackSlot>>,
    /// Set once by `run` via `attach_learner`; see `on_death`.
    learner: OnceLock<Weak<Mutex<crate::buttons::ButtonService>>>,
}

impl Default for CallbackRegistry {
    fn default() -> Self {
        Self {
            next_identity: AtomicU64::new(1),
            state: Mutex::new(GenerationRegistry::default()),
            learner: OnceLock::new(),
        }
    }
}

impl CallbackRegistry {
    fn reserve(&self) -> CallbackToken {
        let identity = self.next_identity.fetch_add(1, Ordering::Relaxed);
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .reserve(identity)
    }

    /// Link the candidate before publishing it. A concurrently newer caller
    /// wins; in that case the freshly linked, never-published candidate is
    /// unlinked again without disturbing the current callback.
    fn subscribe(
        self: &Arc<Self>,
        callback: &Strong<dyn IHudCallback>,
    ) -> binder::Result<Option<CallbackToken>> {
        let token = self.reserve();
        let registry = Arc::downgrade(self);
        let mut death = DeathRecipient::new(move || {
            if let Some(registry) = Weak::upgrade(&registry) {
                registry.on_death(token);
            }
        });
        let mut binder = callback.as_binder();
        binder.link_to_death(&mut death)?;
        let candidate = CallbackSlot {
            callback: callback.clone(),
            binder,
            death,
        };
        let outcome = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .publish(token, candidate);
        match outcome {
            Publish::Published(retired) => {
                Self::unlink_slot(retired.map(|entry| entry.value));
                Ok(Some(token))
            }
            Publish::Superseded(candidate) => {
                Self::unlink_slot(Some(candidate.value));
                Ok(None)
            }
        }
    }

    /// Everything that happens when a linked subscriber's binder dies.
    ///
    /// Clears the callback slot only if the exact linked registration is still
    /// current — a late death from A cannot remove replacement B, including
    /// when both subscriptions refer to the same remote callback object — and
    /// then releases learn mode, for the same registration and under the same
    /// rule.
    ///
    /// LEARN MODE IS RELEASED HERE, and not in the death closure, for three
    /// reasons. It keeps "the current subscriber died" as one function rather
    /// than a rule restated at the call site; it lets `subscribe` keep its
    /// signature, so this file's semantic controls still exercise the real
    /// path; and it puts the release behind the exact same
    /// `remove_if_current` answer that guards the callback slot, so a stale
    /// death provably cannot disarm the app that replaced the dead one.
    ///
    /// LOCK ORDER, and it matters: the registry guard is a statement-scoped
    /// temporary and is gone before the button lock is taken. The bezel reader
    /// runs the other way round — button lock, released, then
    /// `current_token()` — so neither path ever holds both, and there is no
    /// inversion to deadlock on.
    fn on_death(&self, token: CallbackToken) {
        let removed = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove_if_current(token);
        // DeathRecipient drop handles its dead link. This scope is outside the
        // registry mutex, and no callback Binder transaction occurs here.
        let was_current = removed.is_some();
        drop(removed);
        if !was_current {
            return;
        }
        // IHud.aidl setButtonLearnMode(): "The daemon clears it when the
        // subscribing binder dies AND after a bounded timeout." Only the
        // timeout was ever implemented, so a setup app that crashed mid-wizard
        // left the buttons dead for the full five minutes with nothing to say
        // why. `ButtonService::forget_learner`'s own doc comment claimed it was
        // "called from the binder death recipient"; until now it was called
        // from nowhere at all.
        let Some(buttons) = self.learner.get().and_then(Weak::upgrade) else {
            return;
        };
        // The trailing semicolon is load-bearing: as a tail EXPRESSION the
        // match's `Result<MutexGuard<'_>, _>` temporary outlives the `buttons`
        // Arc it borrows from, and the borrow checker rejects it (E0597). As a
        // statement the guard is dropped first, which is also the order the
        // lock wants.
        match buttons.lock() {
            Ok(mut service) => service.forget_learner(),
            // A poisoned button mutex must not strand learn mode: this is the
            // recovery path, so it takes the inner value and does the job.
            Err(poisoned) => poisoned.into_inner().forget_learner(),
        };
    }

    /// Name the button service whose learn mode a current subscriber's death
    /// releases. Set once, at startup, by `run`.
    ///
    /// A `OnceLock<Weak<_>>` rather than a constructor argument because the
    /// registry is built before the button service in `run()`, and because a
    /// registry with no learner attached is the honest state for this file's
    /// semantic controls — they test callback generation, not buttons, and
    /// should not have to invent a `ButtonService` to do it. `Weak`, because a
    /// `DeathRecipient` outlives the subscription it was made for and must not
    /// keep the daemon's button state alive by itself.
    fn attach_learner(&self, buttons: &Arc<Mutex<crate::buttons::ButtonService>>) {
        let _ = self.learner.set(Arc::downgrade(buttons));
    }

    /// The token of the currently published callback, if there is one.
    ///
    /// The bezel reader needs it to address a mailbox push. It is deliberately
    /// a snapshot: if the subscriber is replaced between this call and the
    /// flush, `flush_callback_mailbox` drops the payload because the token is
    /// no longer current — which is correct, since the new subscriber never
    /// saw that press and the old one is gone.
    fn current_token(&self) -> Option<CallbackToken> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .current()
            .map(|registration| registration.token)
    }

    fn unsubscribe(
        &self,
        callback: &Strong<dyn IHudCallback>,
    ) -> Option<Registration<CallbackSlot>> {
        let requested = callback.as_binder();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let token = state
            .current()
            .filter(|current| current.value.binder == requested)
            .map(|current| current.token)?;
        state.remove_if_current(token)
    }

    fn current_callback(&self, token: CallbackToken) -> Option<Strong<dyn IHudCallback>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_if_current(token)
            .map(|slot| slot.callback.clone())
    }

    /// Evict only the callback whose delivery failed. Returns true only when
    /// the failed delivery was still for the current generation.
    fn evict_failed_delivery(&self, token: CallbackToken) -> bool {
        let removed = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove_if_current(token);
        let present = removed.is_some();
        Self::unlink_slot(removed.map(|entry| entry.value));
        present
    }

    fn unlink_slot(slot: Option<CallbackSlot>) {
        let Some(mut slot) = slot else {
            return;
        };
        // The binder library documents unlink as a no-op/error for a dead
        // endpoint. Avoid that remote call in the normal death path while
        // still explicitly unlinking a live replacement/unsubscribe slot.
        if slot.binder.is_binder_alive() {
            if let Err(error) = slot.binder.unlink_to_death(&mut slot.death) {
                log_both(true, format!("callback unlink failed error={error:?}"));
            }
        }
    }

    /// Publish the queued-only core adapter only while this exact registry
    /// generation remains current. Keeping the registry lock through the core
    /// replacement closes the A-late/B-current interleaving; `HudService`
    /// only enqueues locally here and never issues a Binder transaction.
    fn publish_core_if_current(
        &self,
        token: CallbackToken,
        core: &Arc<Mutex<ProductionCore>>,
        mailbox: &Arc<CallbackMailbox>,
        now_ms: u64,
    ) -> binder::Result<bool> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut result = Ok(false);
        state.publish_if_current(token, || {
            result = core
                .lock()
                .map_err(|_| {
                    Status::new_exception_str(
                        ExceptionCode::ILLEGAL_STATE,
                        Some("HUD core mutex poisoned"),
                    )
                })
                .map(|mut core| {
                    core.subscribe(
                        Box::new(BinderCallback::new(token, Arc::clone(mailbox))),
                        now_ms,
                    );
                    true
                });
        });
        result
    }
}

#[derive(Clone)]
enum CallbackPayload {
    Acc(core::AccStateEvent),
    Key(core::McuKeyEvent),
    Event(core::HudEvent),
    /// A bezel press. Enqueued by the bezel reader after it has released the
    /// button lock, drained here like every other payload.
    Button(crate::buttons::ButtonEvent),
}

struct QueuedCallback {
    token: CallbackToken,
    payload: CallbackPayload,
}

/// The only bridge from the core lock to remote Binder. Enqueue is local and
/// bounded by normal daemon event flow; transactions are drained afterwards.
#[derive(Default)]
struct CallbackMailbox {
    queue: Mutex<VecDeque<QueuedCallback>>,
}

impl CallbackMailbox {
    fn push(&self, token: CallbackToken, payload: CallbackPayload) {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(QueuedCallback { token, payload });
    }

    fn drain(&self) -> Vec<QueuedCallback> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .collect()
    }
}

fn flush_callback_mailbox(
    registry: &CallbackRegistry,
    mailbox: &CallbackMailbox,
    mut evict_core: impl FnMut(u64),
) {
    for queued in mailbox.drain() {
        let Some(callback) = registry.current_callback(queued.token) else {
            continue;
        };
        let result = match queued.payload {
            CallbackPayload::Acc(event) => callback.onAccStateChanged(&aidl_acc_event(event)),
            CallbackPayload::Key(event) => callback.onMcuKeyEvent(&aidl_key_event(event)),
            CallbackPayload::Event(event) => callback.onEvent(&aidl_event(event)),
            CallbackPayload::Button(event) => callback.onButtonEvent(&aidl_button_event(event)),
        };
        if result.is_err() && registry.evict_failed_delivery(queued.token) {
            log_both(
                true,
                format!(
                    "callback delivery failed; evicted identity={} generation={}",
                    queued.token.identity, queued.token.generation
                ),
            );
            // The registry was cleared before this local core update.
            // HudService checks the identity too, so a concurrent replacement
            // cannot be removed.
            evict_core(queued.token.identity);
        }
    }
}

impl Interface for BinderHud {
    fn dump(&self, writer: &mut dyn std::io::Write, args: &[&std::ffi::CStr]) -> std::result::Result<(), binder::StatusCode> {
        let uid = binder::ThreadState::get_calling_uid();
        if uid != 0 && uid != 2000 {
            return Err(binder::StatusCode::PERMISSION_DENIED);
        }
        let diagnostics = Arc::clone(&self.core.lock().map_err(|_| binder::StatusCode::FAILED_TRANSACTION)?.mcu_diagnostics);
        diagnostics.dump(uid, writer, args).map_err(|error| match error.kind() {
            std::io::ErrorKind::PermissionDenied => binder::StatusCode::PERMISSION_DENIED,
            std::io::ErrorKind::InvalidInput => binder::StatusCode::BAD_VALUE,
            std::io::ErrorKind::WouldBlock => binder::StatusCode::WOULD_BLOCK,
            _ => binder::StatusCode::FAILED_TRANSACTION,
        })
    }
}

impl BinderHud {
    fn lock(&self) -> binder::Result<MutexGuard<'_, ProductionCore>> {
        self.core.lock().map_err(|_| {
            Status::new_exception_str(
                ExceptionCode::ILLEGAL_STATE,
                Some("HUD core mutex poisoned"),
            )
        })
    }

    fn lock_buttons(&self) -> binder::Result<MutexGuard<'_, crate::buttons::ButtonService>> {
        self.buttons.lock().map_err(|_| {
            Status::new_exception_str(
                ExceptionCode::ILLEGAL_STATE,
                Some("HUD button mutex poisoned"),
            )
        })
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }

    fn flush_callbacks(&self) {
        flush_callback_mailbox(
            &self.callback_registry,
            &self.callback_mailbox,
            |identity| match self.core.lock() {
                Ok(mut service) => service.unsubscribe(identity),
                Err(_) => log_both(true, "callback core eviction skipped mutex=poisoned"),
            },
        );
    }
}

impl IHud for BinderHud {
    fn getStatus(&self) -> binder::Result<AidlHudStatus> {
        Ok(aidl_status(self.lock()?.get_status()))
    }

    fn subscribe(&self, callback: &Strong<dyn IHudCallback>) -> binder::Result<()> {
        let now_ms = self.now_ms();
        let Some(token) = self.callback_registry.subscribe(callback)? else {
            // A later concurrent subscribe already published a replacement.
            // Its caller owns core publication; this older link was removed.
            return Ok(());
        };
        if !self.callback_registry.publish_core_if_current(
            token,
            &self.core,
            &self.callback_mailbox,
            now_ms,
        )? {
            return Ok(());
        }
        self.flush_callbacks();
        Ok(())
    }

    fn unsubscribe(&self, callback: &Strong<dyn IHudCallback>) -> binder::Result<()> {
        if let Some(slot) = self.callback_registry.unsubscribe(callback) {
            let identity = slot.token.identity;
            {
                let mut core = self.lock()?;
                core.unsubscribe(identity);
            }
            // Binder unlink happens after both local locks have gone away.
            CallbackRegistry::unlink_slot(Some(slot.value));
        }
        Ok(())
    }

    fn getRecentEvents(&self, max_count: i32) -> binder::Result<Vec<AidlHudEvent>> {
        Ok(self
            .lock()?
            .get_recent_events(max_count)
            .into_iter()
            .map(aidl_event)
            .collect())
    }

    fn getUsbMode(&self) -> binder::Result<AidlUsbModeStatus> {
        Ok(aidl_usb_mode_status(self.lock()?.get_usb_mode()))
    }

    fn setUsbMode(&self, mode: AidlUsbMode) -> binder::Result<AidlUsbModeStatus> {
        let mode = match mode {
            AidlUsbMode::HOST => core::UsbMode::Host,
            AidlUsbMode::DEVICE => core::UsbMode::Device,
            _ => core::UsbMode::Unknown,
        };
        let now_ms = self.now_ms();
        let status = {
            let mut core = self.lock()?;
            core.set_usb_mode(mode, now_ms)
        };
        self.flush_callbacks();
        Ok(aidl_usb_mode_status(status))
    }

    fn checkUsbHub(&self) -> binder::Result<AidlUsbHubStatus> {
        let now_ms = self.now_ms();
        let status = {
            let mut core = self.lock()?;
            core.check_usb_hub(now_ms)
        };
        self.flush_callbacks();
        Ok(aidl_usb_hub_status(status))
    }

    fn getHubPorts(&self) -> binder::Result<Vec<AidlHubPortState>> {
        self.lock()?
            .get_hub_ports()
            .map(|ports| ports.into_iter().map(aidl_hub_port).collect())
            .map_err(service_error)
    }

    fn setHubPortEnabled(
        &self,
        port_number: i32,
        enabled: bool,
    ) -> binder::Result<AidlHubPortState> {
        let port_number = u32::try_from(port_number).map_err(service_error)?;
        self.lock()?
            .set_hub_port_enabled(port_number, enabled)
            .map(aidl_hub_port)
            .map_err(service_error)
    }

    fn getGpioCensus(&self) -> binder::Result<AidlGpioCensus> {
        self.lock()?
            .get_gpio_census()
            .map(aidl_gpio_census)
            .map_err(service_error)
    }

    // ---- V2: physical bezel buttons ------------------------------------

    fn getRecentButtonEvents(&self, max_count: i32) -> binder::Result<Vec<AidlButtonEvent>> {
        Ok(self
            .lock_buttons()?
            .recent(max_count)
            .into_iter()
            .map(aidl_button_event)
            .collect())
    }

    fn setButtonLearnMode(&self, enabled: bool) -> binder::Result<()> {
        let now = i64_saturating(self.now_ms());
        self.lock_buttons()?.set_learn_mode(enabled, now);
        Ok(())
    }

    fn isButtonLearnMode(&self) -> binder::Result<bool> {
        let now = i64_saturating(self.now_ms());
        Ok(self.lock_buttons()?.learn_mode(now))
    }

    fn getButtonBindings(&self) -> binder::Result<Vec<AidlButtonBinding>> {
        Ok(self
            .lock_buttons()?
            .bindings()
            .iter()
            .cloned()
            .map(aidl_button_binding)
            .collect())
    }

    fn setButtonBindings(&self, bindings: &[AidlButtonBinding]) -> binder::Result<()> {
        let proposed: Vec<crate::buttons::ButtonBinding> =
            bindings.iter().map(core_button_binding).collect::<binder::Result<_>>()?;
        // A refused table leaves the previous one in force: rejecting a bad
        // edit must not also disarm the buttons that already worked.
        self.lock_buttons()?
            .set_bindings(proposed)
            .map_err(|error| match error {
                crate::buttons::SetBindingsError::Invalid(reason) => Status::new_exception_str(
                    ExceptionCode::ILLEGAL_ARGUMENT,
                    Some(reason.to_string()),
                ),
                // A disk problem is not an argument problem. ILLEGAL_STATE says
                // the table was fine and retrying it unchanged is the right move
                // once /data is writable; ILLEGAL_ARGUMENT would have told the
                // app the opposite.
                //
                // It THROWS rather than logging and returning Ok, which is what
                // this did first. Succeeding would show the owner a binding that
                // saved and that silently will not be there after the next
                // start, which the app cannot detect. The wizard treats a
                // refusal as authoritative and re-reads, and by then the daemon
                // has already guaranteed the previous table still stands in
                // memory AND on disk. The log line is the flight tell; the
                // exception is what the app needs.
                crate::buttons::SetBindingsError::NotPersisted { detail } => {
                    log_both(
                        true,
                        format!(
                            "button bindings NOT PERSISTED {detail}; the previous table stands"
                        ),
                    );
                    Status::new_exception_str(
                        ExceptionCode::ILLEGAL_STATE,
                        Some(format!("binding table could not be stored: {detail}")),
                    )
                }
            })
    }

    fn registerButtonActionListener(&self, listener: &Strong<dyn IButtonActionListener>) -> binder::Result<()> {
        register_action_listener(&self.action_registry, listener, binder::ThreadState::get_calling_uid())
    }

    fn unregisterButtonActionListener(&self, listener: &Strong<dyn IButtonActionListener>) -> binder::Result<()> {
        self.action_registry.unregister(&listener.as_binder());
        Ok(())
    }
}

type BinderActionRegistry = ActionRegistry<SpIBinder, ActionCallback>;

struct ActionCallback {
    callback: Strong<dyn IButtonActionListener>,
    binder: SpIBinder,
    death: DeathRecipient,
}

impl ActionSink for ActionCallback {
    fn send(&self, action: ButtonSystemAction) -> bool {
        self.callback.onButtonAction(aidl_system_action(action)).is_ok()
    }
}

impl Drop for ActionCallback {
    fn drop(&mut self) {
        let _ = self.binder.unlink_to_death(&mut self.death);
    }
}

fn register_action_listener(
    registry: &Arc<BinderActionRegistry>, listener: &Strong<dyn IButtonActionListener>, uid: u32,
) -> binder::Result<()> {
    let mut handle = listener.as_binder();
    let token = match registry.reserve(handle.clone(), uid).map_err(|reason|
        Status::new_exception_str(ExceptionCode::ILLEGAL_STATE, Some(reason)))? {
        Reserve::Existing => return Ok(()),
        Reserve::New(token) => token,
    };
    let weak = Arc::downgrade(registry);
    let mut death = DeathRecipient::new(move || {
        if let Some(registry) = weak.upgrade() { registry.remove_token(token); }
    });
    if let Err(error) = handle.link_to_death(&mut death) {
        registry.remove_token(token);
        return Err(error.into());
    }
    let value = Arc::new(ActionCallback { callback: listener.clone(), binder: handle, death });
    if !registry.publish(token, value) {
        return Err(Status::new_exception_str(ExceptionCode::ILLEGAL_STATE,
            Some("button action listener retired during registration")));
    }
    Ok(())
}

fn aidl_system_action(action: ButtonSystemAction) -> AidlButtonSystemAction {
    match action {
        ButtonSystemAction::None => AidlButtonSystemAction::NONE,
        ButtonSystemAction::ShowGlobalActions => AidlButtonSystemAction::SHOW_GLOBAL_ACTIONS,
    }
}

fn core_system_action(action: AidlButtonSystemAction) -> binder::Result<ButtonSystemAction> {
    match action {
        AidlButtonSystemAction::NONE => Ok(ButtonSystemAction::None),
        AidlButtonSystemAction::SHOW_GLOBAL_ACTIONS => Ok(ButtonSystemAction::ShowGlobalActions),
        _ => Err(Status::new_exception_str(ExceptionCode::ILLEGAL_ARGUMENT,
            Some("unknown button system action"))),
    }
}

fn aidl_button_source(source: crate::buttons::ButtonSource) -> AidlButtonSource {
    match source {
        crate::buttons::ButtonSource::Mcu => AidlButtonSource::MCU,
        crate::buttons::ButtonSource::Bezel => AidlButtonSource::BEZEL,
        crate::buttons::ButtonSource::Unknown => AidlButtonSource::UNKNOWN,
    }
}

fn core_button_source(source: AidlButtonSource) -> crate::buttons::ButtonSource {
    match source {
        AidlButtonSource::MCU => crate::buttons::ButtonSource::Mcu,
        AidlButtonSource::BEZEL => crate::buttons::ButtonSource::Bezel,
        _ => crate::buttons::ButtonSource::Unknown,
    }
}

fn aidl_button_event(event: crate::buttons::ButtonEvent) -> AidlButtonEvent {
    AidlButtonEvent {
        sequence: event.sequence,
        elapsedRealtimeMs: event.elapsed_realtime_ms,
        source: aidl_button_source(event.source),
        rawCode: event.raw_code,
        x: event.x,
        y: event.y,
        pressDurationMs: event.press_duration_ms,
        androidKeyCode: event.android_key_code,
        systemAction: aidl_system_action(event.system_action),
        // RELEASE, not the zero default: the bezel decoder emits exactly one
        // event per COMPLETED press, on the up transition, which is also why
        // pressDurationMs is meaningful here. Leaving this to Default would
        // report UNKNOWN and quietly lose that fact.
        action: AidlMcuKeyAction::RELEASE,
    }
}

fn aidl_button_binding(binding: crate::buttons::ButtonBinding) -> AidlButtonBinding {
    AidlButtonBinding {
        source: aidl_button_source(binding.source),
        rawCode: binding.raw_code,
        yMin: binding.y_min,
        yMax: binding.y_max,
        androidKeyCode: binding.android_key_code,
        label: binding.label,
        systemAction: aidl_system_action(binding.system_action),
    }
}

fn core_button_binding(binding: &AidlButtonBinding) -> binder::Result<crate::buttons::ButtonBinding> {
    Ok(crate::buttons::ButtonBinding {
        source: core_button_source(binding.source),
        raw_code: binding.rawCode,
        y_min: binding.yMin,
        y_max: binding.yMax,
        android_key_code: binding.androidKeyCode,
        label: binding.label.clone(),
        system_action: core_system_action(binding.systemAction)?,
    })
}

struct BinderCallback {
    token: CallbackToken,
    mailbox: Arc<CallbackMailbox>,
}

impl BinderCallback {
    fn new(token: CallbackToken, mailbox: Arc<CallbackMailbox>) -> Self {
        Self { token, mailbox }
    }
}

impl core::HudCallback for BinderCallback {
    fn identity(&self) -> u64 {
        self.token.identity
    }

    fn on_acc_state_changed(&self, event: &core::AccStateEvent) {
        self.mailbox
            .push(self.token, CallbackPayload::Acc(event.clone()));
    }

    fn on_mcu_key_event(&self, event: &core::McuKeyEvent) {
        self.mailbox
            .push(self.token, CallbackPayload::Key(event.clone()));
    }

    fn on_event(&self, event: &core::HudEvent) {
        self.mailbox
            .push(self.token, CallbackPayload::Event(event.clone()));
    }
}

// This is compiled only by platform/scripts/hud-binder-desk-test.sh. It uses
// ABI-shaped Binder/AIDL stubs to execute the actual adapter's death and
// delivery-failure paths without a Soong image or board hardware.
/// Desk-only: runs the adapter's publication, death and delivery-failure
/// controls against the ABI-shaped stubs of hud-binder-desk-test.sh (feature
/// `binder-desk`, never enabled by Soong). Documented because the desk
/// harness carries this crate's `#![warn(missing_docs)]` under `-D warnings`.
#[cfg(feature = "binder-desk")]
pub fn binder_desk_semantic_controls() {
    use std::sync::atomic::{AtomicBool, Ordering};

    #[cfg(feature = "acc-power-channel")]
    crate::acc_power::reader_hook_control(|producer| {
        let control = Arc::new(AccControlMailbox::default());
        let evidence = Arc::new(AccTxMailbox::default());
        let mut hooks = ReaderHooks::new(control, evidence.clone());
        hooks.power = Some(producer);
        let held = evidence.queue.lock().unwrap();
        let bytes = vec![0x5a, 0xa5, 0x03, 0x12, 0x03, 0xeb,
                         0x5a, 0xa5, 0x03, 0x05, 0x10, 0xeb];
        let mut session = AccSession::new(std::io::Cursor::new(bytes), hooks);
        session.read_once(100).unwrap();
        session.advance(600).unwrap();
        drop(held);
        assert_eq!(evidence.drain().1, 2); // both TX observations dropped
        session.platform_mut().power.take().unwrap()
    });

    // Exercise the actual ReaderHooks with the actual serial state machine,
    // not a parallel observer implementation. Diagnostic pressure is isolated
    // from the control mailbox, even when every evidence record is dropped.
    let control = Arc::new(AccControlMailbox::default());
    let evidence = Arc::new(AccTxMailbox::default());
    let hooks = ReaderHooks::new(control.clone(), evidence.clone());
    // Same received fixtures as mcu/acc_go_fixtures.rs (test-only module).
    let bytes = vec![0x5a, 0xa5, 0x03, 0x12, 0x03, 0xeb,
                     0x5a, 0xa5, 0x03, 0x05, 0x10, 0xeb];
    let mut session = AccSession::new(std::io::Cursor::new(bytes), hooks);
    session.read_once(100).unwrap();
    session.advance(599).unwrap();
    assert!(evidence.drain().0.into_iter().all(|record| record.is_none()));
    session.advance(600).unwrap();
    let (records, dropped) = evidence.drain();
    assert_eq!(dropped, 0);
    let records: Vec<_> = records.into_iter().flatten().collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].phase, AccTxPhase::Attempted);
    assert_eq!(records[1].to_string(), "mcu-acc-tx name=acc-off-complete payload=05-15 loop_ms=600 deferred=1 phase=write-complete boundary=write_all-return mcu_ack=unobserved");
    assert!(matches!(control.drain()[2], AccControl::SleepRequest { now_ms: 600 }));
    let record = records[0];
    for _ in 0..ACC_TX_CAPACITY + 3 { evidence.push(record); }
    // The control callbacks remain intact with a full diagnostic queue.
    let mut hooks = ReaderHooks::new(control.clone(), evidence.clone());
    hooks.request_sleep(700).unwrap();
    hooks.publish_acc_state(AccMachineState::Suspending, "test", 700).unwrap();
    let controls = control.drain();
    assert_eq!(controls.len(), 2);
    assert!(matches!(controls[0], AccControl::SleepRequest { now_ms: 700 }));
    let (records, dropped) = evidence.drain();
    assert_eq!(records.into_iter().flatten().count(), ACC_TX_CAPACITY);
    assert_eq!(dropped, 3);
    // Held lock must drop rather than block the UART thread.
    let held = evidence.queue.lock().unwrap();
    hooks.observe_acc_tx(record);
    drop(held);
    assert_eq!(evidence.drain().1, 1);
    assert_eq!(evidence.drain().1, 0);
    // Inject ENOSPC into the REAL production stderr/kmsg logging path, in this
    // host-only driver process. Restore its stderr even on assertion unwinding.
    // No device access: /dev/full is the host's synthetic failing-write sink.
    use std::os::fd::{AsRawFd, FromRawFd};
    extern "C" { fn dup(fd: i32) -> i32; fn dup2(old: i32, new: i32) -> i32; }
    struct RestoreStderr(std::fs::File);
    impl Drop for RestoreStderr {
        fn drop(&mut self) {
            // SAFETY: saved descriptor remains owned/live until after restore.
            assert_eq!(unsafe { dup2(self.0.as_raw_fd(), 2) }, 2);
        }
    }
    let mut full = std::fs::OpenOptions::new().write(true).open("/dev/full").unwrap();
    assert_eq!(std::io::Write::write_all(&mut full, b"control").unwrap_err().raw_os_error(), Some(28));
    // SAFETY: dup creates a new owned descriptor, transferred exactly once.
    let saved_fd = unsafe { dup(2) };
    assert!(saved_fd >= 0);
    let restore = RestoreStderr(unsafe { std::fs::File::from_raw_fd(saved_fd) });
    // SAFETY: full is live and 2 is this desk driver's stderr, not the daemon's.
    assert_eq!(unsafe { dup2(full.as_raw_fd(), 2) }, 2);
    evidence.push(record);
    flush_acc_tx(&evidence);
    drop(restore);
    session.advance(701).unwrap();

    struct RemoveTree(PathBuf);
    impl Drop for RemoveTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    let sysfs = std::env::temp_dir().join(format!("bengal-hud-binder-se0-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sysfs);
    let _sysfs_cleanup = RemoveTree(sysfs.clone());
    for (tty, mmio) in [("ttyHS0", SE0_MMIO), ("ttyHS1", SE3_MMIO_NOT_OURS)] {
        let tty_dir = sysfs.join("class/tty").join(tty);
        std::fs::create_dir_all(&tty_dir).expect("create fake tty class");
        let target = sysfs.join(format!("devices/platform/soc@0/{mmio}.serial"));
        std::fs::create_dir_all(&target).expect("create fake serial device");
        std::os::unix::fs::symlink(&target, tty_dir.join("device")).expect("link fake tty to MMIO");
    }
    let wrong_name = std::path::Path::new("/dev/ttyHS1");
    let (selected, by_mmio) = select_mcu_path(&sysfs, wrong_name).expect("resolve unique SE0");
    assert!(by_mmio, "a configured tty name must not override SE0 MMIO");
    assert_eq!(selected, PathBuf::from("/dev/ttyHS0"));
    let duplicate = sysfs.join("class/tty/ttyHS2");
    std::fs::create_dir_all(&duplicate).expect("create duplicate SE0 tty class");
    let se0_target = sysfs.join(format!("devices/platform/soc@0/{SE0_MMIO}.serial"));
    std::os::unix::fs::symlink(&se0_target, duplicate.join("device"))
        .expect("link duplicate tty to SE0");
    let refusal = select_mcu_path(&sysfs, wrong_name)
        .expect_err("two exact SE0 matches must refuse instead of choosing by tty name");
    assert!(refusal.contains("SE0 MMIO resolution refused"));
    std::fs::remove_dir_all(&sysfs).expect("remove fake tty tree");
    let (selected, by_mmio) =
        select_mcu_path(&sysfs, wrong_name).expect("use unproven named fallback");
    assert!(!by_mmio, "an absent SE0 lookup must use the named fallback");
    assert_eq!(selected, wrong_name);

    let tty_dir = sysfs.join("class/tty/ttyHS1");
    std::fs::create_dir_all(&tty_dir).expect("create SE3-only fake tty class");
    let target = sysfs.join(format!("devices/platform/soc@0/{SE3_MMIO_NOT_OURS}.serial"));
    std::fs::create_dir_all(&target).expect("create SE3-only fake serial device");
    std::os::unix::fs::symlink(&target, tty_dir.join("device"))
        .expect("link configured fallback to SE3");
    let refusal = select_mcu_path(&sysfs, wrong_name)
        .expect_err("a configured fallback positively resolved as Bluetooth SE3 must be refused");
    assert!(refusal.contains("is Bluetooth SE3"));

    struct TestCallback {
        endpoint: std::sync::Arc<crate::Endpoint>,
        fail: AtomicBool,
        /// Android key codes delivered through `onButtonEvent`, in order.
        /// Shared with the control that made this callback, because the
        /// `Strong` swallows the value and there is no other way to see what
        /// the daemon actually pushed.
        buttons_seen: Arc<Mutex<Vec<i32>>>,
    }

    impl Interface for TestCallback {
        fn as_binder(&self) -> SpIBinder {
            self.endpoint.binder()
        }
    }

    impl IHudCallback for TestCallback {
        fn onAccStateChanged(&self, _: &AidlAccStateEvent) -> binder::Result<()> {
            Ok(())
        }

        fn onMcuKeyEvent(&self, _: &AidlMcuKeyEvent) -> binder::Result<()> {
            Ok(())
        }

        fn onButtonEvent(&self, event: &AidlButtonEvent) -> binder::Result<()> {
            self.buttons_seen
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(event.androidKeyCode);
            Ok(())
        }

        fn onEvent(&self, _: &AidlHudEvent) -> binder::Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                Err(Status::new_service_specific_error_str(
                    -1,
                    Some("desk failure"),
                ))
            } else {
                Ok(())
            }
        }
    }

    fn callback(endpoint: Arc<crate::Endpoint>, fail: bool) -> Strong<dyn IHudCallback> {
        counting_callback(endpoint, fail, Arc::new(Mutex::new(Vec::new())))
    }

    fn counting_callback(
        endpoint: Arc<crate::Endpoint>,
        fail: bool,
        buttons_seen: Arc<Mutex<Vec<i32>>>,
    ) -> Strong<dyn IHudCallback> {
        Strong::from_arc(Arc::new(TestCallback {
            endpoint,
            fail: AtomicBool::new(fail),
            buttons_seen,
        }))
    }

    // This constructs the real production core without opening GPIO, USB, or
    // sysfs. The exercised methods below only replace its in-memory callback
    // and feed its state machine, whose first ACC-off edge has no I/O hook.
    fn production_core_for_desk() -> Arc<Mutex<ProductionCore>> {
        let sysfs = crate::sysfs_root();
        let usb = UsbOwner::new(
            GpioV2::gpiochip0(),
            LinuxHubSysfs::new(&sysfs),
            ThreadSleeper,
            0,
        ).with_kernel_role(&sysfs);
        Arc::new(Mutex::new(HudService::new(usb, core::InventoryGpioCensus)))
    }

    // A stale death must leave B current, while B's own death must remove B.
    // The count checks also prove that B remains linked after A was retired.
    let registry = Arc::new(CallbackRegistry::default());
    let endpoint_a = crate::Endpoint::new();
    let callback_a = callback(Arc::clone(&endpoint_a), false);
    let token_a = registry
        .subscribe(&callback_a)
        .expect("A death link")
        .expect("A published");
    // Model Binder having queued A's death callback before B's replacement
    // unlinks A. The queued callback must not remove B.
    let stale_a_death = endpoint_a.snapshot_death().expect("A recipient linked");
    let endpoint_b = crate::Endpoint::new();
    let callback_b = callback(Arc::clone(&endpoint_b), false);
    let token_b = registry
        .subscribe(&callback_b)
        .expect("B death link")
        .expect("B published");
    assert_eq!(endpoint_a.links(), 1);
    assert_eq!(endpoint_a.unlinks(), 1);
    assert_eq!(endpoint_b.links(), 1);
    assert_eq!(endpoint_b.unlinks(), 0);
    stale_a_death.fire();
    assert!(registry.current_callback(token_a).is_none());
    assert!(registry.current_callback(token_b).is_some());
    endpoint_b.fire_death();
    assert!(registry.current_callback(token_b).is_none());

    // Replacing a callback by another subscription using the *same Binder*
    // must unlink A's exact recipient, not B's. This models V1 clients that
    // subscribe the same remote object again while a replacement is pending.
    let same_binder_registry = Arc::new(CallbackRegistry::default());
    let same_binder_endpoint = crate::Endpoint::new();
    let same_binder_a = callback(Arc::clone(&same_binder_endpoint), false);
    let same_binder_a_token = same_binder_registry
        .subscribe(&same_binder_a)
        .expect("same-binder A death link")
        .expect("same-binder A published");
    let same_binder_b = callback(Arc::clone(&same_binder_endpoint), false);
    let same_binder_b_token = same_binder_registry
        .subscribe(&same_binder_b)
        .expect("same-binder B death link")
        .expect("same-binder B published");
    assert_eq!(same_binder_endpoint.links(), 2);
    assert_eq!(same_binder_endpoint.unlinks(), 1);
    assert_eq!(same_binder_endpoint.live_deaths(), 1);
    same_binder_endpoint.fire_death();
    assert!(same_binder_registry
        .current_callback(same_binder_a_token)
        .is_none());
    assert!(same_binder_registry
        .current_callback(same_binder_b_token)
        .is_none());

    // Exercise the real binder.rs publication seam, not merely its generic
    // registry. Once B is current, a late A cannot install an A-tagged core
    // callback. An ACC edge then proves delivery remains tagged for B.
    let publication_registry = Arc::new(CallbackRegistry::default());
    let publication_mailbox = Arc::new(CallbackMailbox::default());
    let publication_core = production_core_for_desk();
    let publication_a = callback(crate::Endpoint::new(), false);
    let publication_a_token = publication_registry
        .subscribe(&publication_a)
        .expect("publication A death link")
        .expect("publication A published");
    let publication_b = callback(crate::Endpoint::new(), false);
    let publication_b_token = publication_registry
        .subscribe(&publication_b)
        .expect("publication B death link")
        .expect("publication B published");
    assert!(!publication_registry
        .publish_core_if_current(
            publication_a_token,
            &publication_core,
            &publication_mailbox,
            0,
        )
        .expect("late A core publication"));
    assert!(publication_registry
        .publish_core_if_current(
            publication_b_token,
            &publication_core,
            &publication_mailbox,
            0,
        )
        .expect("B core publication"));
    let initial = publication_mailbox.drain();
    assert!(!initial.is_empty());
    assert!(initial
        .iter()
        .all(|queued| queued.token == publication_b_token));
    publication_core
        .lock()
        .expect("desk core mutex")
        .publish_acc_state(core::AccState::OffGrace, "mcu-12-03", 1);
    let delivered = publication_mailbox.drain();
    assert!(!delivered.is_empty());
    assert!(delivered
        .iter()
        .all(|queued| queued.token == publication_b_token));

    let failing_registry = Arc::new(CallbackRegistry::default());
    let failing_mailbox = Arc::new(CallbackMailbox::default());
    let failing_endpoint = crate::Endpoint::new();
    let failing_callback = callback(Arc::clone(&failing_endpoint), true);
    let failing_token = failing_registry
        .subscribe(&failing_callback)
        .expect("failing death link")
        .expect("failing callback published");
    failing_mailbox.push(
        failing_token,
        CallbackPayload::Event(core::HudEvent {
            sequence: 1,
            elapsed_realtime_ms: 0,
            event_type: core::HudEventType::Warning,
            command: "desk-failure".into(),
            state: String::new(),
            reason: String::new(),
            error: String::new(),
        }),
    );
    let mut evicted = Vec::new();
    flush_callback_mailbox(&failing_registry, &failing_mailbox, |identity| {
        evicted.push(identity)
    });
    assert_eq!(evicted, vec![failing_token.identity]);
    assert!(failing_registry.current_callback(failing_token).is_none());
    assert_eq!(failing_endpoint.links(), 1);
    assert_eq!(failing_endpoint.unlinks(), 1);

    // A bezel press must actually reach IHudCallback.onButtonEvent. Until this
    // landed nothing in the daemon ever called that method, so a setup wizard
    // could only poll getRecentButtonEvents -- and in learn mode, where
    // injection is suppressed on purpose, the push is the ONLY signal that the
    // press happened. Remove the CallbackPayload::Button arm in
    // flush_callback_mailbox, or the mailbox push in spawn_bezel_reader, and
    // this control fails.
    let button_registry = Arc::new(CallbackRegistry::default());
    let button_mailbox = Arc::new(CallbackMailbox::default());
    let button_seen = Arc::new(Mutex::new(Vec::new()));
    let button_callback =
        counting_callback(crate::Endpoint::new(), false, Arc::clone(&button_seen));
    let button_token = button_registry
        .subscribe(&button_callback)
        .expect("button death link")
        .expect("button callback published");
    // Exactly what the reader queues after draining ButtonService: one event
    // per completed press, carrying the key it resolved to.
    let mut button_service = crate::buttons::ButtonService::new();
    button_service
        .set_bindings(vec![crate::buttons::ButtonBinding {
            source: crate::buttons::ButtonSource::Bezel,
            raw_code: 0,
            y_min: 253,
            y_max: 289,
            android_key_code: 4,
            label: "Back".into(),
            system_action: ButtonSystemAction::None,
        }])
        .expect("desk binding table");
    // Learn mode ON: the press must be delivered even though it is not
    // injected. That is the case the wizard depends on.
    button_service.set_learn_mode(true, 0);
    assert_eq!(
        button_service.observe_bezel(-30, 264, 10, 40),
        None,
        "learn mode must suppress injection"
    );
    let delivery = button_service.drain_pending();
    assert_eq!(delivery.dropped, 0);
    for event in delivery.events {
        button_mailbox.push(button_token, CallbackPayload::Button(event));
    }
    flush_callback_mailbox(&button_registry, &button_mailbox, |_| {
        panic!("a successful onButtonEvent delivery must not evict the subscriber")
    });
    assert_eq!(
        *button_seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec![4],
        "the suppressed press must still reach onButtonEvent, with its key code"
    );

    // A press queued for a subscriber that has since been replaced must be
    // dropped rather than delivered to the replacement, which never saw it.
    let stale_seen = Arc::new(Mutex::new(Vec::new()));
    let stale_callback = counting_callback(crate::Endpoint::new(), false, Arc::clone(&stale_seen));
    let _stale_token = button_registry
        .subscribe(&stale_callback)
        .expect("replacement death link")
        .expect("replacement published");
    button_mailbox.push(
        button_token,
        CallbackPayload::Button(crate::buttons::ButtonEvent {
            sequence: 2,
            elapsed_realtime_ms: 20,
            source: crate::buttons::ButtonSource::Bezel,
            raw_code: 0,
            x: -30,
            y: 264,
            press_duration_ms: 40,
            android_key_code: 4,
            system_action: ButtonSystemAction::None,
        }),
    );
    flush_callback_mailbox(&button_registry, &button_mailbox, |_| {});
    assert!(
        stale_seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty(),
        "a press addressed to a retired subscriber must not be delivered to its replacement"
    );

    // Learn mode must be released when the CURRENT subscriber's binder dies,
    // and must survive a stale death from a replaced generation. IHud.aidl
    // promises both halves; before this, only the five-minute timeout existed,
    // so a setup app that crashed mid-wizard left all five bezel buttons dead
    // with nothing on screen to say why. Delete the forget_learner() call in
    // CallbackRegistry::on_death and this control fails.
    let learn_registry = Arc::new(CallbackRegistry::default());
    let learner = Arc::new(Mutex::new(crate::buttons::ButtonService::new()));
    learn_registry.attach_learner(&learner);
    learner
        .lock()
        .expect("learner lock")
        .set_learn_mode(true, 0);

    let learn_endpoint_a = crate::Endpoint::new();
    let learn_callback_a = callback(Arc::clone(&learn_endpoint_a), false);
    let _learn_token_a = learn_registry
        .subscribe(&learn_callback_a)
        .expect("learn A death link")
        .expect("learn A published");
    let stale_learn_death = learn_endpoint_a
        .snapshot_death()
        .expect("learn A recipient linked");
    let learn_endpoint_b = crate::Endpoint::new();
    let learn_callback_b = callback(Arc::clone(&learn_endpoint_b), false);
    let _learn_token_b = learn_registry
        .subscribe(&learn_callback_b)
        .expect("learn B death link")
        .expect("learn B published");

    stale_learn_death.fire();
    assert!(
        learner.lock().expect("learner lock").learn_mode(0),
        "a stale death must not disarm the app that replaced the dead one"
    );

    learn_endpoint_b.fire_death();
    assert!(
        !learner.lock().expect("learner lock").learn_mode(0),
        "the current subscriber's death must release learn mode, not wait five minutes"
    );

    // setButtonBindings must distinguish its two refusals BY EXCEPTION CODE,
    // through the real Binder method rather than through the mapping helper,
    // because the mapping is only useful if the call site actually uses it.
    //
    // The distinction is the whole contract the wizard reads: ILLEGAL_ARGUMENT
    // means the table is wrong and resending it unchanged will fail again;
    // ILLEGAL_STATE means the table was fine and retrying is right once /data
    // is writable. Collapse the two and an app either retries forever on a
    // table that can never be accepted, or gives up on a transient disk
    // problem and loses the owner's work.
    fn hud_for_desk(store_path: &std::path::Path) -> BinderHud {
        let (service, _) = crate::buttons::ButtonService::with_store(
            crate::buttons::ButtonStore::new(store_path),
        );
        BinderHud {
            core: production_core_for_desk(),
            buttons: Arc::new(Mutex::new(service)),
            started: Instant::now(),
            callback_registry: Arc::new(CallbackRegistry::default()),
            callback_mailbox: Arc::new(CallbackMailbox::default()),
            action_registry: Arc::new(ActionRegistry::default()),
        }
    }

    fn desk_binding(y_min: i32, y_max: i32, code: i32, label: &str) -> AidlButtonBinding {
        AidlButtonBinding {
            source: AidlButtonSource::BEZEL,
            rawCode: 0,
            yMin: y_min,
            yMax: y_max,
            androidKeyCode: code,
            label: label.to_string(),
            systemAction: AidlButtonSystemAction::NONE,
        }
    }

    let status_dir = std::env::temp_dir().join(format!(
        "bengal-hud-binder-status-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&status_dir);
    let _status_cleanup = RemoveTree(status_dir.clone());
    std::fs::create_dir_all(&status_dir).expect("status control scratch");

    // 1. A table the daemon refuses on its own rules: two BEZEL bindings that
    //    overlap. The directory exists, so persistence is not the problem.
    let writable = hud_for_desk(&status_dir.join("button-bindings"));
    // Actual owner read -> fixed snapshot -> actual Binder dump adapter.
    let payload = [0x33u8, 0, 0x23, 0, 0x7d, 0xfe, 9];
    let mut wire = vec![0x5a, 0xa5, 8];
    wire.extend(payload);
    wire.push(crate::mcu::checksum(&payload));
    let mut reader = AccSession::new(std::io::Cursor::new(wire), crate::mcu::serial::BridgeHooksTodo);
    reader.read_once(123).unwrap();
    writable.core.lock().unwrap().mcu_diagnostics.publish(reader.diagnostics());
    let mut dump = Vec::new();
    assert_eq!(Interface::dump(&writable, &mut dump, &[]), Err(binder::StatusCode::PERMISSION_DENIED));
    assert!(dump.is_empty());
    for uid in [0, 2000] {
        binder::ThreadState::set_calling_uid_for_desk(uid);
        dump.clear();
        Interface::dump(&writable, &mut dump, &[]).unwrap();
        assert!(std::str::from_utf8(&dump).unwrap().contains("kind=version"));
    }
    let raw_arg = CString::new("--raw").unwrap();
    dump.clear();
    assert_eq!(Interface::dump(&writable, &mut dump, &[&raw_arg]), Err(binder::StatusCode::BAD_VALUE));
    assert!(dump.is_empty());
    struct UnlockedDump<'a>(&'a BinderHud);
    impl std::io::Write for UnlockedDump<'_> {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            assert!(self.0.core.try_lock().is_ok(), "dump retained service lock during I/O");
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    assert_eq!(Interface::dump(&writable, &mut UnlockedDump(&writable), &[]), Err(binder::StatusCode::FAILED_TRANSACTION));
    binder::ThreadState::set_calling_uid_for_desk(1000);
    let overlapping = [
        desk_binding(100, 200, 3, "Home"),
        desk_binding(150, 250, 4, "Back"),
    ];
    let status = writable
        .setButtonBindings(&overlapping)
        .expect_err("overlapping bezel ranges must be refused");
    assert_eq!(
        status.exception_code(),
        ExceptionCode::ILLEGAL_ARGUMENT,
        "a bad table is an ILLEGAL_ARGUMENT: {status}"
    );

    // 2. The same daemon, a perfectly good table, and nowhere to put it.
    let unwritable = hud_for_desk(&status_dir.join("no-such-dir").join("button-bindings"));
    let good = [desk_binding(100, 200, 3, "Home"), desk_binding(250, 350, 4, "Back")];
    let status = unwritable
        .setButtonBindings(&good)
        .expect_err("a table that cannot be stored must be refused");
    assert_eq!(
        status.exception_code(),
        ExceptionCode::ILLEGAL_STATE,
        "an unstorable table is an ILLEGAL_STATE, not a bad argument: {status}"
    );
    // And the refusal left nothing behind: the previous (empty) table stands.
    assert!(
        unwritable
            .getButtonBindings()
            .expect("read back the table")
            .is_empty(),
        "a refused setButtonBindings must not half-apply"
    );

    // 3. The positive control, so the two above are not passing because
    //    setButtonBindings refuses everything.
    assert!(
        writable.setButtonBindings(&good).is_ok(),
        "a valid, storable table must be accepted"
    );
    assert_eq!(
        writable
            .getButtonBindings()
            .expect("read back the table")
            .len(),
        2
    );

    // V3 exercises the actual IHud setter, persistence and generated adapter.
    let original_bytes = std::fs::read(status_dir.join("button-bindings")).unwrap();
    let mut power = desk_binding(80, 130, 0, "Power");
    power.systemAction = AidlButtonSystemAction::SHOW_GLOBAL_ACTIONS;
    let mut invalid = desk_binding(80, 130, 3, "Bad Power");
    invalid.systemAction = AidlButtonSystemAction::SHOW_GLOBAL_ACTIONS;
    assert_eq!(writable.setButtonBindings(&[invalid]).unwrap_err().exception_code(), ExceptionCode::ILLEGAL_ARGUMENT);
    let mut unknown = desk_binding(80, 130, 0, "Unknown action");
    unknown.systemAction = AidlButtonSystemAction(77);
    assert_eq!(writable.setButtonBindings(&[unknown]).unwrap_err().exception_code(), ExceptionCode::ILLEGAL_ARGUMENT);
    assert_eq!(std::fs::read(status_dir.join("button-bindings")).unwrap(), original_bytes);
    assert_eq!(writable.getButtonBindings().unwrap().len(), 2);
    writable.setButtonBindings(&[power]).unwrap();
    let reloaded = hud_for_desk(&status_dir.join("button-bindings"));
    assert_eq!(reloaded.getButtonBindings().unwrap()[0].systemAction, AidlButtonSystemAction::SHOW_GLOBAL_ACTIONS);

    struct ActionListener {
        endpoint: Arc<crate::Endpoint>,
        seen: Arc<Mutex<Vec<AidlButtonSystemAction>>>,
        fail: bool,
    }
    impl Interface for ActionListener {
        fn as_binder(&self) -> SpIBinder { self.endpoint.binder() }
    }
    impl IButtonActionListener for ActionListener {
        fn onButtonAction(&self, action: AidlButtonSystemAction) -> binder::Result<()> {
            self.seen.lock().unwrap().push(action);
            if self.fail { Err(binder::StatusCode::DEAD_OBJECT.into()) } else { Ok(()) }
        }
    }
    let make_action = |endpoint: Arc<crate::Endpoint>, fail: bool| {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let callback: Strong<dyn IButtonActionListener> = Strong::from_arc(Arc::new(ActionListener {
            endpoint, seen: Arc::clone(&seen), fail,
        }));
        (callback, seen)
    };
    let action_a_endpoint = crate::Endpoint::new();
    let (action_a, seen_a) = make_action(Arc::clone(&action_a_endpoint), false);
    let (action_b, seen_b) = make_action(crate::Endpoint::new(), false);
    let (unrelated, _) = make_action(crate::Endpoint::new(), false);
    let legacy_seen = Arc::new(Mutex::new(Vec::new()));
    let legacy_callback = counting_callback(crate::Endpoint::new(), false, legacy_seen);
    writable.subscribe(&legacy_callback).unwrap();
    let old_slot_before = writable.callback_registry.current_token();
    assert!(old_slot_before.is_some());
    writable.setButtonLearnMode(true).unwrap();
    writable.registerButtonActionListener(&action_a).unwrap();
    assert!(writable.isButtonLearnMode().unwrap(), "V3 registration must not disturb Learn");
    writable.setButtonLearnMode(false).unwrap();
    writable.registerButtonActionListener(&action_b).unwrap();
    writable.registerButtonActionListener(&action_a).unwrap();
    assert_eq!(action_a_endpoint.links(), 1, "same handle must not link twice");
    assert_eq!(writable.callback_registry.current_token(), old_slot_before, "V3 must not touch V2 slot");
    assert!(writable.registerButtonActionListener(&unrelated).is_err(), "UID quota must not evict A or B");
    writable.action_registry.drain_for_desk();
    assert!(seen_a.lock().unwrap().is_empty(), "registration must not trigger an action");
    let press = |hud: &BinderHud, now| {
        let mut buttons = hud.buttons.lock().unwrap();
        hud.action_registry.handle_press(&mut buttons, -30, 110, now, true)
    };
    assert_eq!(press(&writable, 1).system_action, ButtonSystemAction::ShowGlobalActions);
    writable.action_registry.drain_for_desk();
    assert_eq!(seen_a.lock().unwrap().len(), 1);
    assert_eq!(seen_b.lock().unwrap().len(), 1);
    let stale_death = action_a_endpoint.snapshot_death().unwrap();
    press(&writable, 2);
    writable.unregisterButtonActionListener(&unrelated).unwrap();
    writable.unregisterButtonActionListener(&action_a).unwrap();
    writable.registerButtonActionListener(&action_a).unwrap();
    stale_death.fire();
    writable.action_registry.drain_for_desk();
    assert_eq!(seen_a.lock().unwrap().len(), 1, "new A must not receive queued old event");
    assert_eq!(seen_b.lock().unwrap().len(), 2, "A removal must leave B");
    press(&writable, 3);
    writable.action_registry.drain_for_desk();
    assert_eq!(seen_a.lock().unwrap().len(), 2, "stale death must leave new A");
    action_a_endpoint.fire_death();
    let fail_link = crate::Endpoint::new();
    fail_link.fail_next_link();
    let (bad_link, bad_seen) = make_action(fail_link, false);
    assert!(writable.registerButtonActionListener(&bad_link).is_err());
    let (failing, failing_seen) = make_action(crate::Endpoint::new(), true);
    writable.registerButtonActionListener(&failing).unwrap();
    press(&writable, 4);
    writable.action_registry.drain_for_desk();
    press(&writable, 5);
    writable.action_registry.drain_for_desk();
    assert!(bad_seen.lock().unwrap().is_empty(), "failed death link must publish nothing");
    assert_eq!(failing_seen.lock().unwrap().len(), 1, "failed delivery evicts exact listener");
    assert_eq!(seen_b.lock().unwrap().len(), 5);
    writable.setButtonLearnMode(true).unwrap();
    // Use the service clock for the Learn timeout, as the actual reader does.
    press(&writable, i64_saturating(writable.now_ms()));
    writable.getRecentButtonEvents(0).unwrap();
    writable.action_registry.drain_for_desk();
    assert_eq!(seen_b.lock().unwrap().len(), 5, "Learn and diagnostic paths emit nothing");
    writable.setButtonLearnMode(false).unwrap();
    writable.setButtonBindings(&[desk_binding(80, 130, 0, "Power")]).unwrap();
    assert_eq!(writable.getButtonBindings().unwrap()[0].systemAction, AidlButtonSystemAction::NONE,
        "a V2 whole-table setter defaults to NONE and clears the action");
    assert_eq!(press(&writable, 100).system_action, ButtonSystemAction::None);
}

fn service_error(error: impl std::fmt::Display) -> Status {
    Status::new_service_specific_error_str(-1, Some(error.to_string()))
}

fn i64_saturating(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn i32_saturating(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn aidl_acc_state(state: core::AccState) -> AidlAccState {
    match state {
        core::AccState::Unknown => AidlAccState::UNKNOWN,
        core::AccState::On => AidlAccState::ACC_ON,
        core::AccState::OffGrace => AidlAccState::ACC_OFF_GRACE,
        core::AccState::FinalStage => AidlAccState::FINAL_STAGE,
        core::AccState::Suspending => AidlAccState::SUSPENDING,
    }
}

fn aidl_acc_event(event: core::AccStateEvent) -> AidlAccStateEvent {
    AidlAccStateEvent {
        sequence: i64_saturating(event.sequence),
        elapsedRealtimeMs: i64_saturating(event.elapsed_realtime_ms),
        state: aidl_acc_state(event.state),
        reason: event.reason,
    }
}

fn aidl_status(status: core::HudStatus) -> AidlHudStatus {
    AidlHudStatus {
        startedElapsedRealtimeMs: i64_saturating(status.started_elapsed_realtime_ms),
        mcuConnected: status.mcu_connected,
        frameCount: i64_saturating(status.frame_count),
        validFrames: i64_saturating(status.valid_frames),
        invalidFrames: i64_saturating(status.invalid_frames),
        lastEventSequence: i64_saturating(status.last_event_sequence),
        accState: aidl_acc_state(status.acc_state),
    }
}

fn aidl_usb_mode(mode: core::UsbMode) -> AidlUsbMode {
    match mode {
        core::UsbMode::Unknown => AidlUsbMode::UNKNOWN,
        core::UsbMode::Host => AidlUsbMode::HOST,
        core::UsbMode::Device => AidlUsbMode::DEVICE,
    }
}

fn aidl_usb_mode_status(status: core::UsbModeStatus) -> AidlUsbModeStatus {
    AidlUsbModeStatus {
        mode: aidl_usb_mode(status.mode),
        chip: status.chip,
        appliedElapsedRealtimeMs: i64_saturating(status.applied_elapsed_realtime_ms),
        error: status.error,
    }
}

fn aidl_hub_port(port: core::HubPortState) -> AidlHubPortState {
    AidlHubPortState {
        portNumber: i32::try_from(port.port_number).unwrap_or(i32::MAX),
        sysfsName: port.sysfs_name,
        enabled: port.enabled,
        devicePath: port.device_path,
        vendorId: port.vendor_id,
        productId: port.product_id,
        logStick: port.log_stick,
    }
}

fn aidl_usb_hub_status(status: core::UsbHubStatus) -> AidlUsbHubStatus {
    AidlUsbHubStatus {
        trigger: status.trigger,
        healthy: status.healthy,
        pulses: i32_saturating(status.pulses),
        ports: status.ports.into_iter().map(aidl_hub_port).collect(),
        present: Vec::new(),
        seen: Vec::new(),
        expected: "0bda:8153,214b:7250".into(),
        flapping: false,
        carrierChanges: 0,
        flapReason: String::new(),
        budgetExhausted: status.budget_exhausted,
        error: status.error,
    }
}

fn aidl_event_type(event_type: core::HudEventType) -> AidlHudEventType {
    match event_type {
        core::HudEventType::Acc => AidlHudEventType::ACC,
        core::HudEventType::McuKey => AidlHudEventType::MCU_KEY,
        core::HudEventType::UsbMode => AidlHudEventType::USB_MODE,
        core::HudEventType::UsbHub => AidlHudEventType::USB_HUB,
        core::HudEventType::Gpio => AidlHudEventType::GPIO,
        core::HudEventType::Warning => AidlHudEventType::WARNING,
    }
}

fn aidl_event(event: core::HudEvent) -> AidlHudEvent {
    AidlHudEvent {
        sequence: i64_saturating(event.sequence),
        elapsedRealtimeMs: i64_saturating(event.elapsed_realtime_ms),
        r#type: aidl_event_type(event.event_type),
        command: event.command,
        state: event.state,
        reason: event.reason,
        error: event.error,
    }
}

fn aidl_key_action(action: core::McuKeyAction) -> AidlMcuKeyAction {
    match action {
        core::McuKeyAction::Unknown => AidlMcuKeyAction::UNKNOWN,
        core::McuKeyAction::Press => AidlMcuKeyAction::PRESS,
        core::McuKeyAction::Release => AidlMcuKeyAction::RELEASE,
        core::McuKeyAction::RotateLeft => AidlMcuKeyAction::ROTATE_LEFT,
        core::McuKeyAction::RotateRight => AidlMcuKeyAction::ROTATE_RIGHT,
    }
}

fn aidl_key_event(event: core::McuKeyEvent) -> AidlMcuKeyEvent {
    AidlMcuKeyEvent {
        sequence: i64_saturating(event.sequence),
        elapsedRealtimeMs: i64_saturating(event.elapsed_realtime_ms),
        rawCode: i32::from(event.raw_code),
        name: event.name,
        action: aidl_key_action(event.action),
        androidKeyCode: event.android_key_code,
        magnitude: i32::from(event.magnitude),
    }
}

fn aidl_gpio_direction(direction: core::GpioDirection) -> AidlGpioDirection {
    match direction {
        core::GpioDirection::Unknown => AidlGpioDirection::UNKNOWN,
        core::GpioDirection::Input => AidlGpioDirection::INPUT,
        core::GpioDirection::Output => AidlGpioDirection::OUTPUT,
    }
}

fn aidl_gpio_line(line: core::GpioLineInfo) -> AidlGpioLineInfo {
    AidlGpioLineInfo {
        offset: i32::try_from(line.offset).unwrap_or(i32::MAX),
        name: line.name,
        consumer: line.consumer,
        direction: aidl_gpio_direction(line.direction),
        value: line.value,
        used: line.used,
        activeLow: line.active_low,
        label: line.label,
        meaning: line.meaning,
        group: line.group,
        function: line.function,
        source: line.source,
        daemonOwned: line.daemon_owned,
    }
}

fn aidl_gpio_census(census: core::GpioCensus) -> AidlGpioCensus {
    AidlGpioCensus {
        chip: census.chip,
        chipLabel: census.chip_label,
        lines: census.lines.into_iter().map(aidl_gpio_line).collect(),
        warnings: census.warnings,
    }
}
