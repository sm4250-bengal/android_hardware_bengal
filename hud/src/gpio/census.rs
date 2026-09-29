// SPDX-License-Identifier: Apache-2.0
//! Read-only GPIO metadata. Never requests a line, reads a voltage, or sweeps
//! debugfs: some unlisted TLMM pads belong to TZ. Chip discovery reads only
//! chip descriptors; line-info reads only the curated device-3 inventory.

use super::{AUDIO, FM_POWER, TLMM_CHIP_LABEL, USB};
use crate::service::{GpioCensus, GpioDirection, GpioLineInfo};
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::raw::c_ulong;
use std::path::{Path, PathBuf};

const CHIP_INFO: c_ulong = 0x8044_b401;
const LINE_INFO: c_ulong = 0xc100_b405;
const LIVE_SOURCE: &str = "GPIO_V2_GET_LINEINFO (kernel metadata)";

extern "C" {
    fn ioctl(fd: RawFd, request: c_ulong, ...) -> i32;
}

#[repr(C)]
#[derive(Default)]
struct ChipInfo {
    name: [u8; 32],
    label: [u8; 32],
    lines: u32,
}

// gpio_v2_line_info in the pinned GKI UAPI. The opaque attributes are not
// consumed, but must remain present and aligned for the kernel copy.
#[repr(C, align(8))]
struct LineInfo {
    name: [u8; 32],
    consumer: [u8; 32],
    offset: u32,
    num_attrs: u32,
    flags: u64,
    attrs: [u8; 160],
    padding: [u32; 4],
}

impl LineInfo {
    fn new(offset: u32) -> Self {
        Self {
            name: [0; 32],
            consumer: [0; 32],
            offset,
            num_attrs: 0,
            flags: 0,
            attrs: [0; 160],
            padding: [0; 4],
        }
    }
}

const _: () = assert!(std::mem::size_of::<ChipInfo>() == 68);
const _: () = assert!(std::mem::size_of::<LineInfo>() == 256);

fn c_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len())])
        .into_owned()
}

trait MetadataReader {
    fn resolve(&mut self) -> io::Result<(String, u32)>;
    fn line(&mut self, offset: u32) -> io::Result<LineInfo>;
}

#[derive(Default)]
struct LinuxReader {
    chip: Option<File>,
}

fn select_tlmm<T>(
    chips: impl IntoIterator<Item = (String, String, u32, T)>,
) -> io::Result<(String, u32, T)> {
    let mut selected = None;
    for (path, label, count, handle) in chips {
        if label != TLMM_CHIP_LABEL {
            continue;
        }
        if selected.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "multiple device-3 TLMM chips; refusing ambiguous census",
            ));
        }
        selected = Some((path, count, handle));
    }
    selected.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "TLMM label 500000.pinctrl not found",
        )
    })
}

// The GPIO bus uses the same gpiochipN names as the character devices. The
// daemon can search /dev to open its labelled nodes, but cannot list /dev.
// Sysfs discovery needs no extra privilege and never assumes a chip number;
// GET_CHIPINFO below still verifies the exact board label before line reads.
fn chip_paths(gpio_bus: &Path, dev_root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(gpio_bus)? {
        let name = entry?.file_name();
        let text = name.to_string_lossy();
        let Some(suffix) = text.strip_prefix("gpiochip") else {
            continue;
        };
        if !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()) {
            paths.push(dev_root.join(name));
        }
    }
    paths.sort();
    Ok(paths)
}

impl MetadataReader for LinuxReader {
    fn resolve(&mut self) -> io::Result<(String, u32)> {
        let mut chips = Vec::new();
        let mut failures = Vec::new();
        for path in chip_paths(Path::new("/sys/bus/gpio/devices"), Path::new("/dev"))? {
            let result = (|| -> io::Result<_> {
                let file = File::open(&path)?; // O_RDONLY, never O_RDWR
                let mut info = ChipInfo::default();
                // SAFETY: writable 68-byte C UAPI object outlives this ioctl.
                if unsafe { ioctl(file.as_raw_fd(), CHIP_INFO, &mut info as *mut ChipInfo) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok((
                    path.display().to_string(),
                    c_string(&info.label),
                    info.lines,
                    file,
                ))
            })();
            match result {
                Ok(chip) => chips.push(chip),
                Err(error) => failures.push(format!("{}: {error}", path.display())),
            }
        }
        let (path, count, file) = select_tlmm(chips).map_err(|error| {
            if failures.is_empty() {
                error
            } else {
                io::Error::new(error.kind(), format!("{error}; {}", failures.join("; ")))
            }
        })?;
        self.chip = Some(file);
        Ok((path, count))
    }

    fn line(&mut self, offset: u32) -> io::Result<LineInfo> {
        let file = self
            .chip
            .as_ref()
            .ok_or_else(|| io::Error::other("TLMM not resolved"))?;
        let mut info = LineInfo::new(offset);
        // SAFETY: exact 256-byte, 8-aligned UAPI object, reserved fields zero.
        // GET_LINEINFO only reads descriptors; it is NOT GET_LINE (ownership).
        if unsafe { ioctl(file.as_raw_fd(), LINE_INFO, &mut info as *mut LineInfo) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if info.offset != offset {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "line-info offset mismatch",
            ));
        }
        Ok(info)
    }
}

/// Fresh, bounded metadata snapshot; failed lines retain labelled unknowns.
pub fn read() -> GpioCensus {
    snapshot(&mut LinuxReader::default())
}

fn snapshot(reader: &mut impl MetadataReader) -> GpioCensus {
    let mut census = GpioCensus {
        chip: String::new(),
        chip_label: TLMM_CHIP_LABEL.into(),
        lines: Vec::new(),
        warnings: Vec::new(),
    };
    let count = match reader.resolve() {
        Ok((path, count)) => {
            census.chip = path;
            Some(count)
        }
        Err(error) => {
            census
                .warnings
                .push(format!("GPIO metadata unavailable: {error}"));
            None
        }
    };
    for (group, line) in AUDIO
        .iter()
        .map(|l| ("audio", l))
        .chain(FM_POWER.iter().map(|l| ("fm", l)))
        .chain(USB.iter().map(|l| ("usb", l)))
    {
        let mut row = GpioLineInfo {
            offset: line.offset,
            name: String::new(),
            consumer: String::new(),
            direction: GpioDirection::Unknown,
            value: -1,
            used: false,
            active_low: false,
            label: line.what.into(),
            meaning: line.source.into(),
            group: group.into(),
            function: String::new(),
            source: "inventory only; live metadata unavailable".into(),
            // Production GPIOs are held by kernel drivers, not this reader.
            daemon_owned: false,
        };
        if let Some(count) = count {
            let result = if line.offset < count {
                reader.line(line.offset)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "offset exceeds TLMM line count",
                ))
            };
            match result {
                Ok(info) => {
                    row.name = c_string(&info.name);
                    row.consumer = c_string(&info.consumer);
                    row.used = info.flags & 1 != 0;
                    row.active_low = info.flags & 2 != 0;
                    row.direction = match info.flags & 12 {
                        4 => GpioDirection::Input,
                        8 => GpioDirection::Output,
                        _ => GpioDirection::Unknown,
                    };
                    row.source = LIVE_SOURCE.into();
                }
                Err(error) => census
                    .warnings
                    .push(format!("GPIO {}: {error}", line.offset)),
            }
        }
        census.lines.push(row);
    }
    // Voltage and pinmux are deliberately unknown: GET_LINEINFO cannot expose
    // either. This is an API limitation, not an incomplete implementation.
    census
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_chips_without_listing_dev() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = std::env::temp_dir().join(format!("hud-gpio-discovery-{}", std::process::id()));
        let bus = root.join("gpio-bus");
        let dev = root.join("dev");
        fs::create_dir_all(&bus).unwrap();
        fs::create_dir_all(&dev).unwrap();
        fs::write(dev.join("gpiochip7"), b"chip fixture").unwrap();
        symlink("/unused/sysfs/target", bus.join("gpiochip7")).unwrap();
        for name in ["gpiochip", "gpiochip7x", "gpiochip-1", "not-a-chip"] {
            fs::write(bus.join(name), b"").unwrap();
        }
        // Model SELinux's /dev search-only grant with Unix directory mode:
        // opening a known node works; enumerating the directory does not.
        fs::set_permissions(&dev, fs::Permissions::from_mode(0o111)).unwrap();
        let denied = fs::read_dir(&dev).err().map(|e| e.kind());
        let opened = File::open(dev.join("gpiochip7")).is_ok();
        let discovered = chip_paths(&bus, &dev);
        fs::set_permissions(&dev, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(
            denied,
            Some(io::ErrorKind::PermissionDenied),
            "run unprivileged"
        );
        assert!(opened);
        assert_eq!(discovered.unwrap(), vec![dev.join("gpiochip7")]);
    }

    struct Fake {
        offsets: Vec<u32>,
        count: u32,
        missing: bool,
        fail: Option<u32>,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self {
                offsets: vec![],
                count: 114,
                missing: false,
                fail: None,
            }
        }
    }
    impl MetadataReader for Fake {
        fn resolve(&mut self) -> io::Result<(String, u32)> {
            if self.missing {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            } else {
                Ok(("/dev/gpiochip7".into(), self.count))
            }
        }
        fn line(&mut self, offset: u32) -> io::Result<LineInfo> {
            self.offsets.push(offset);
            if self.fail == Some(offset) {
                return Err(io::Error::from_raw_os_error(25));
            }
            let mut info = LineInfo::new(offset);
            info.flags = 11; // USED | ACTIVE_LOW | OUTPUT, not a voltage!
            info.consumer[..6].copy_from_slice(b"kernel");
            Ok(info)
        }
    }
    #[test]
    fn uapi_layout_and_requests_are_exact() {
        assert_eq!(std::mem::offset_of!(LineInfo, offset), 64);
        assert_eq!(std::mem::offset_of!(LineInfo, flags), 72);
        assert_eq!(std::mem::offset_of!(LineInfo, attrs), 80);
        assert_eq!(std::mem::offset_of!(LineInfo, padding), 240);
        assert_eq!(std::mem::align_of::<LineInfo>(), 8);
        assert_eq!(CHIP_INFO, 0x8044b401);
        assert_eq!(LINE_INFO, 0xc100b405);
    }
    #[test]
    fn resolve_exact_label_not_gpiochip_number_or_device_one() {
        let chips = vec![
            ("gpiochip0".into(), "400000.pinctrl".into(), 114, ()),
            ("gpiochip7".into(), TLMM_CHIP_LABEL.into(), 114, ()),
        ];
        assert_eq!(select_tlmm(chips).unwrap().0, "gpiochip7");
        assert!(select_tlmm(vec![("gpiochip0".into(), "other".into(), 114, ())]).is_err());
        assert!(select_tlmm(vec![
            ("a".into(), TLMM_CHIP_LABEL.into(), 114, ()),
            ("b".into(), TLMM_CHIP_LABEL.into(), 114, ())
        ])
        .is_err());
    }
    #[test]
    fn reads_only_inventory_and_never_claims_voltage_or_daemon_ownership() {
        let mut reader = Fake::default();
        let result = snapshot(&mut reader);
        assert_eq!(
            reader.offsets,
            vec![36, 37, 83, 93, 106, 107, 96, 84, 54, 52, 34, 85]
        );
        assert_eq!(result.chip, "/dev/gpiochip7");
        assert!(result.warnings.is_empty());
        for row in result.lines {
            assert!(row.used && row.active_low);
            assert_eq!(row.direction, GpioDirection::Output);
            assert_eq!(row.consumer, "kernel");
            assert_eq!(row.value, -1);
            assert!(!row.daemon_owned);
            assert!(row.function.is_empty());
        }
    }
    #[test]
    fn partial_failure_does_not_discard_other_lines() {
        let result = snapshot(&mut Fake {
            fail: Some(83),
            ..Fake::default()
        });
        assert_eq!(result.lines.len(), 12);
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].starts_with("GPIO 83:"));
        assert_eq!(result.lines[2].direction, GpioDirection::Unknown);
        assert_eq!(result.lines[3].direction, GpioDirection::Output);
    }
    #[test]
    fn missing_chip_preserves_inventory_as_unknown_without_fallback() {
        let mut reader = Fake {
            missing: true,
            ..Fake::default()
        };
        let result = snapshot(&mut reader);
        assert!(result.chip.is_empty() && reader.offsets.is_empty());
        assert_eq!(result.lines.len(), 12);
        assert!(result
            .lines
            .iter()
            .all(|r| r.direction == GpioDirection::Unknown && !r.daemon_owned));
        assert!(result.warnings[0].contains("denied"));
    }
    #[test]
    fn out_of_range_pins_never_reach_ioctl() {
        let mut reader = Fake {
            count: 35,
            ..Fake::default()
        };
        let result = snapshot(&mut reader);
        assert_eq!(reader.offsets, vec![34]);
        assert_eq!(result.warnings.len(), 11);
    }
    #[test]
    fn bounded_c_strings_do_not_need_a_terminator() {
        assert_eq!(c_string(b"abc\0ignored"), "abc");
        assert_eq!(c_string(&[b'x'; 32]).len(), 32);
    }
}
