// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Narrow `/dev/uinput` output with separate bezel and CAN capabilities.
//!
//! The physical Power button is deliberately absent: device 3 already has a
//! real PMIC power key, and synthesising a second one would duplicate Android
//! power policy. The device-3 MCU frame codes have not been captured yet, so
//! [`DEVICE3_MAPPING_CONFIRMED`] is false and every raw-code slot in
//! [`DEVICE3_BUTTONS`] is `None`. The raw MCU mapping remains disabled until a
//! named-order capture fills those slots and review flips the confirmation bit.
//! CAN uses profile-decoded actions and its own seven-key device; the confirmed
//! bezel touch path continues to use the original four-key device.

use super::ButtonState;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const SYN_REPORT: u16 = 0;

const UI_DEV_CREATE: u64 = 0x5501;
const UI_DEV_DESTROY: u64 = 0x5502;
const UI_DEV_SETUP: u64 = 0x405c_5503;
const UI_SET_EVBIT: u64 = 0x4004_5564;
const UI_SET_KEYBIT: u64 = 0x4004_5565;

const BUS_VIRTUAL: u16 = 0x0006;
const DEVICE_NAME: &[u8] = b"XYAUTO device-3 bezel keys";
const CAN_DEVICE_NAME: &[u8] = b"Bengal CAN wheel keys";
const BEZEL_KEYS: &[Key] = &[Key::Home, Key::Back, Key::VolumeUp, Key::VolumeDown];
const CAN_KEYS: &[Key] = &[
    Key::VolumeUp, Key::VolumeDown, Key::VolumeMute, Key::MediaNext,
    Key::MediaPrevious, Key::Call, Key::VoiceAssist,
];

extern "C" {
    fn ioctl(fd: RawFd, request: u64, ...) -> i32;
}

/// Linux input key emitted by this daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Android Home.
    Home,
    /// Android Back.
    Back,
    /// Android media volume up.
    VolumeUp,
    /// Android media volume down.
    VolumeDown,
    /// Android media volume mute.
    VolumeMute,
    /// Android media next track.
    MediaNext,
    /// Android media previous track.
    MediaPrevious,
    /// Android call key (requires an installed handler).
    Call,
    /// Android voice assist key (requires an installed handler).
    VoiceAssist,
}

impl Key {
    fn linux_code(self) -> u16 {
        match self {
            Self::Home => 172,
            Self::Back => 158,
            Self::VolumeUp => 115,
            Self::VolumeDown => 114,
            Self::VolumeMute => 113,
            Self::MediaNext => 163,
            Self::MediaPrevious => 165,
            Self::Call => 169,
            Self::VoiceAssist => 582,
        }
    }
}

/// One device-3 raw MCU code to Linux-key assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ButtonMapping {
    /// Human-readable bezel function.
    pub name: &'static str,
    /// Captured MCU code, or `None` while the table is provisional.
    pub raw_code: Option<u8>,
    /// Linux input key. There is intentionally no Power variant.
    pub key: Key,
}

/// False until Capture B records two consistent ordered passes on device 3.
pub const DEVICE3_MAPPING_CONFIRMED: bool = false;
const _: () = assert!(!DEVICE3_MAPPING_CONFIRMED);

/// PROVISIONAL device-3 map, top-to-bottom after excluding physical Power.
///
/// Do not copy device 1's codes here: that panel had knobs and eight different
/// buttons. Fill each `None` only from the device-3 capture instrument.
pub const DEVICE3_BUTTONS: &[ButtonMapping] = &[
    ButtonMapping {
        name: "home",
        raw_code: None,
        key: Key::Home,
    },
    ButtonMapping {
        name: "back",
        raw_code: None,
        key: Key::Back,
    },
    ButtonMapping {
        name: "volume-up",
        raw_code: None,
        key: Key::VolumeUp,
    },
    ButtonMapping {
        name: "volume-down",
        raw_code: None,
        key: Key::VolumeDown,
    },
];

/// Output seam used by the production uinput fd and host unit tests.
pub trait KeyEmitter {
    /// Emit one press/release state followed by `SYN_REPORT`.
    fn emit(&mut self, key: Key, pressed: bool) -> io::Result<()>;
}

/// Mapping-and-enable gate in front of a key emitter.
pub struct ButtonInjector<E> {
    emitter: E,
    mappings: &'static [ButtonMapping],
    enabled: bool,
}

impl<E: KeyEmitter> ButtonInjector<E> {
    /// Construct an injector. Production passes the immutable boot property.
    pub fn new(emitter: E, mappings: &'static [ButtonMapping], enabled: bool) -> Self {
        Self {
            emitter,
            mappings,
            enabled,
        }
    }

    /// Apply one decoded MCU button state, if enabled and mapped.
    pub fn handle(&mut self, raw_code: u8, state: ButtonState) -> io::Result<bool> {
        if !self.enabled {
            return Ok(false);
        }
        let Some(mapping) = self
            .mappings
            .iter()
            .find(|mapping| mapping.raw_code == Some(raw_code))
        else {
            return Ok(false);
        };
        let pressed = match state {
            ButtonState::Press => true,
            ButtonState::Release => false,
            ButtonState::Other(_) => return Ok(false),
        };
        self.emitter.emit(mapping.key, pressed)?;
        Ok(true)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    sec: i64,
    usec: i64,
    event_type: u16,
    code: u16,
    value: i32,
}

fn ioctl_int(fd: RawFd, request: u64, value: i32) -> io::Result<()> {
    // SAFETY: these uinput requests consume one integer value synchronously.
    if unsafe { ioctl(fd, request, value) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn write_event(writer: &mut impl Write, event: InputEvent) -> io::Result<()> {
    // SAFETY: InputEvent is repr(C), fully initialized, and borrowed only for
    // the synchronous write. Device and desk targets are both 64-bit Linux.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&event as *const InputEvent).cast::<u8>(),
            std::mem::size_of::<InputEvent>(),
        )
    };
    writer.write_all(bytes)
}

/// Owned uinput device with a source-specific capability allowlist.
pub struct UinputDevice {
    file: File,
}

impl UinputDevice {
    /// Create the unchanged four-key bezel device at `path`.
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_keys(path, DEVICE_NAME, 0x0003, BEZEL_KEYS)
    }

    /// Create a CAN-only device; no HOME, BACK or POWER capabilities.
    pub fn open_can_keys(path: &Path) -> io::Result<Self> {
        Self::open_keys(path, CAN_DEVICE_NAME, 0x0004, CAN_KEYS)
    }

    fn open_keys(path: &Path, name: &[u8], product: u16, keys: &[Key]) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let fd = file.as_raw_fd();
        ioctl_int(fd, UI_SET_EVBIT, i32::from(EV_KEY))?;
        for key in keys {
            ioctl_int(fd, UI_SET_KEYBIT, i32::from(key.linux_code()))?;
        }

        let mut setup = UinputSetup {
            id: InputId {
                bustype: BUS_VIRTUAL,
                vendor: 0x5859,
                product,
                version: 1,
            },
            name: [0; 80],
            ff_effects_max: 0,
        };
        setup.name[..name.len()].copy_from_slice(name);
        // SAFETY: UI_DEV_SETUP synchronously reads the exact UAPI structure.
        if unsafe { ioctl(fd, UI_DEV_SETUP, &setup as *const UinputSetup) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: UI_DEV_CREATE consumes no third argument and creates the
        // device configured on this owned fd.
        if unsafe { ioctl(fd, UI_DEV_CREATE) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { file })
    }
}

impl KeyEmitter for UinputDevice {
    fn emit(&mut self, key: Key, pressed: bool) -> io::Result<()> {
        emit_key(&mut self.file, key, pressed)
    }
}

fn emit_key(writer: &mut impl Write, key: Key, pressed: bool) -> io::Result<()> {
    write_event(
        writer,
        InputEvent {
            sec: 0,
            usec: 0,
            event_type: EV_KEY,
            code: key.linux_code(),
            value: i32::from(pressed),
        },
    )?;
    write_event(
        writer,
        InputEvent {
            sec: 0,
            usec: 0,
            event_type: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        },
    )
}

impl Drop for UinputDevice {
    fn drop(&mut self) {
        // SAFETY: this tears down only the device owned by this live fd.
        let _ = unsafe { ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIRMED_TEST_MAP: &[ButtonMapping] = &[
        ButtonMapping {
            name: "home",
            raw_code: Some(0x31),
            key: Key::Home,
        },
        ButtonMapping {
            name: "back",
            raw_code: Some(0x32),
            key: Key::Back,
        },
        ButtonMapping {
            name: "volume-up",
            raw_code: Some(0x33),
            key: Key::VolumeUp,
        },
        ButtonMapping {
            name: "volume-down",
            raw_code: Some(0x34),
            key: Key::VolumeDown,
        },
    ];

    #[derive(Default)]
    struct MockEmitter(Vec<(Key, bool)>);

    impl KeyEmitter for MockEmitter {
        fn emit(&mut self, key: Key, pressed: bool) -> io::Result<()> {
            self.0.push((key, pressed));
            Ok(())
        }
    }

    #[test]
    fn production_mapping_is_explicitly_provisional_and_empty() {
        assert!(DEVICE3_BUTTONS
            .iter()
            .all(|mapping| mapping.raw_code.is_none()));
        assert_eq!(DEVICE3_BUTTONS.len(), 4, "Power must never be injected");
    }

    #[test]
    fn linux_64_bit_uinput_layout_matches_the_uapi_ioctl_size() {
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        assert_eq!(std::mem::size_of::<InputEvent>(), 24);
    }

    #[test]
    fn linux_codes_reach_the_android_generic_keylayout_destinations() {
        assert_eq!(Key::Home.linux_code(), 172);
        assert_eq!(Key::Back.linux_code(), 158);
        assert_eq!(Key::VolumeUp.linux_code(), 115);
        assert_eq!(Key::VolumeDown.linux_code(), 114);
    }

    #[test]
    fn can_capabilities_and_production_wire_events_are_separate_from_bezel() {
        assert_eq!(BEZEL_KEYS, &[Key::Home, Key::Back, Key::VolumeUp, Key::VolumeDown]);
        let expected = [(Key::VolumeUp, 115), (Key::VolumeDown, 114),
            (Key::VolumeMute, 113), (Key::MediaNext, 163),
            (Key::MediaPrevious, 165), (Key::Call, 169), (Key::VoiceAssist, 582)];
        assert_eq!(CAN_KEYS.len(), expected.len());
        for (key, linux) in expected {
            assert!(CAN_KEYS.contains(&key));
            let mut wire = Vec::new();
            emit_key(&mut wire, key, true).unwrap();
            emit_key(&mut wire, key, false).unwrap();
            let mut want = Vec::new();
            for (event_type, code, value) in [(EV_KEY, linux, 1i32),
                (EV_SYN, SYN_REPORT, 0), (EV_KEY, linux, 0), (EV_SYN, SYN_REPORT, 0)] {
                want.extend_from_slice(&[0; 16]);
                want.extend_from_slice(&event_type.to_ne_bytes());
                want.extend_from_slice(&code.to_ne_bytes());
                want.extend_from_slice(&value.to_ne_bytes());
            }
            assert_eq!(wire, want, "production EV_KEY/SYN_REPORT tap for {key:?}");
        }
        assert!(!CAN_KEYS.contains(&Key::Home));
        assert!(!CAN_KEYS.contains(&Key::Back));
    }

    #[test]
    #[ignore = "requires BENGAL_GENERIC_KEYLAYOUT pointing to the actual Android build tree"]
    fn can_keys_match_actual_android_generic_keylayout() {
        let path = std::env::var("BENGAL_GENERIC_KEYLAYOUT").expect("actual Generic.kl path");
        let layout = std::fs::read_to_string(path).expect("read actual Generic.kl");
        for (key, android) in [(Key::VolumeUp, "VOLUME_UP"),
            (Key::VolumeDown, "VOLUME_DOWN"), (Key::VolumeMute, "VOLUME_MUTE"),
            (Key::MediaNext, "MEDIA_NEXT"), (Key::MediaPrevious, "MEDIA_PREVIOUS"),
            (Key::Call, "CALL"), (Key::VoiceAssist, "VOICE_ASSIST")] {
            let linux = key.linux_code().to_string();
            assert!(layout.lines().any(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                fields.len() >= 3 && fields[0] == "key" && fields[1] == linux && fields[2] == android
            }), "{key:?}: Linux {linux} must map to Android {android}");
        }
    }

    #[test]
    fn property_off_blocks_even_a_confirmed_mapping() {
        let mut injector = ButtonInjector::new(MockEmitter::default(), CONFIRMED_TEST_MAP, false);
        assert!(!injector.handle(0x31, ButtonState::Press).unwrap());
        assert!(injector.emitter.0.is_empty());
    }

    #[test]
    fn confirmed_non_power_buttons_emit_press_and_release_once() {
        let mut injector = ButtonInjector::new(MockEmitter::default(), CONFIRMED_TEST_MAP, true);
        for (raw, key) in [
            (0x31, Key::Home),
            (0x32, Key::Back),
            (0x33, Key::VolumeUp),
            (0x34, Key::VolumeDown),
        ] {
            assert!(injector.handle(raw, ButtonState::Press).unwrap());
            assert!(injector.handle(raw, ButtonState::Release).unwrap());
            assert_eq!(injector.emitter.0.pop(), Some((key, false)));
            assert_eq!(injector.emitter.0.pop(), Some((key, true)));
        }
    }

    #[test]
    fn unknown_and_non_binary_states_never_emit() {
        let mut injector = ButtonInjector::new(MockEmitter::default(), CONFIRMED_TEST_MAP, true);
        assert!(!injector.handle(0xff, ButtonState::Press).unwrap());
        assert!(!injector.handle(0x31, ButtonState::Other(3)).unwrap());
        assert!(injector.emitter.0.is_empty());
    }
}
