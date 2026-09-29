// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Dependency-free Linux GPIO character-device v2 output requests.
//!
//! The initial value is part of `GPIO_V2_GET_LINE_IOCTL`, so requesting a
//! line never produces the opposite-level glitch caused by direction-first
//! sysfs GPIO. The value is then written explicitly and the request released;
//! the physical level persists while other owners are free to request it.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};

const GPIO_V2_GET_LINE_IOCTL: u64 = 0xc250_b407;
const GPIO_V2_LINE_SET_VALUES_IOCTL: u64 = 0xc010_b40f;
const GPIO_V2_LINE_FLAG_OUTPUT: u64 = 0x8;
const GPIO_V2_LINE_ATTR_ID_OUTPUT_VALUES: u32 = 2;
const REQUEST_SIZE: usize = 592;
const REQUEST_CONSUMER: usize = 256;
const REQUEST_CONFIG: usize = 288;
const REQUEST_NUM_LINES: usize = 560;
const REQUEST_FD: usize = 588;
const CONFIG_NUM_ATTRS: usize = 8;
const CONFIG_ATTRS: usize = 32;

extern "C" {
    fn ioctl(fd: RawFd, request: u64, ...) -> i32;
}

/// A sink for physical (not active-low translated) GPIO output levels.
///
/// USB policy is generic over this trait, so desk tests cannot open a real
/// `/dev/gpiochip0` even accidentally.
pub trait OutputLines {
    /// Request, drive, and release one TLMM offset.
    fn drive(&mut self, offset: u32, high: bool) -> io::Result<()>;
}

/// Production GPIO v2 backend. Device 3 supplies `/dev/gpiochip0`.
#[derive(Debug, Clone)]
pub struct GpioV2 {
    chip: PathBuf,
    consumer: [u8; 32],
}

impl GpioV2 {
    /// Construct a backend for a specific gpiochip node.
    pub fn new(chip: impl Into<PathBuf>) -> Self {
        let mut consumer = [0; 32];
        let name = b"bengal-hud usb-owner";
        consumer[..name.len()].copy_from_slice(name);
        Self {
            chip: chip.into(),
            consumer,
        }
    }

    /// Production device-3 gpiochip path.
    pub fn gpiochip0() -> Self {
        Self::new("/dev/gpiochip0")
    }
}

impl OutputLines for GpioV2 {
    fn drive(&mut self, offset: u32, high: bool) -> io::Result<()> {
        drive_output(&self.chip, offset, high, &self.consumer)
    }
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn build_output_request(offset: u32, high: bool, consumer: &[u8; 32]) -> [u8; REQUEST_SIZE] {
    let mut request = [0; REQUEST_SIZE];
    put_u32(&mut request, 0, offset);
    request[REQUEST_CONSUMER..REQUEST_CONSUMER + 32].copy_from_slice(consumer);
    put_u64(&mut request, REQUEST_CONFIG, GPIO_V2_LINE_FLAG_OUTPUT);
    put_u32(&mut request, REQUEST_CONFIG + CONFIG_NUM_ATTRS, 1);
    let attr = REQUEST_CONFIG + CONFIG_ATTRS;
    put_u32(&mut request, attr, GPIO_V2_LINE_ATTR_ID_OUTPUT_VALUES);
    put_u64(&mut request, attr + 8, u64::from(high));
    put_u64(&mut request, attr + 16, 1);
    put_u32(&mut request, REQUEST_NUM_LINES, 1);
    request
}

fn drive_output(chip_path: &Path, offset: u32, high: bool, consumer: &[u8; 32]) -> io::Result<()> {
    let chip = OpenOptions::new().read(true).write(true).open(chip_path)?;
    let mut request = build_output_request(offset, high, consumer);
    // SAFETY: request has the exact 592-byte kernel UAPI layout, is writable,
    // and remains live until the ioctl returns.
    if unsafe {
        ioctl(
            chip.as_raw_fd(),
            GPIO_V2_GET_LINE_IOCTL,
            request.as_mut_ptr(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let fd = i32::from_le_bytes(request[REQUEST_FD..REQUEST_FD + 4].try_into().unwrap());
    // SAFETY: a successful GET_LINE ioctl returns ownership of this new fd.
    let line = unsafe { File::from_raw_fd(fd) };
    let mut values = [0u8; 16];
    put_u64(&mut values, 0, u64::from(high));
    put_u64(&mut values, 8, 1);
    // SAFETY: values is the live 16-byte gpio_v2_line_values UAPI object.
    if unsafe {
        ioctl(
            line.as_raw_fd(),
            GPIO_V2_LINE_SET_VALUES_IOCTL,
            values.as_mut_ptr(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_layout_matches_the_go_gpio_test_fixture() {
        let mut consumer = [0; 32];
        consumer[..22].copy_from_slice(b"hu-mcu-bridge usb-mode");
        let request = build_output_request(85, true, &consumer);
        assert_eq!(request.len(), 592);
        assert_eq!(u32::from_le_bytes(request[0..4].try_into().unwrap()), 85);
        assert_eq!(&request[256..278], b"hu-mcu-bridge usb-mode");
        assert_eq!(request[287], 0);
        assert_eq!(u64::from_le_bytes(request[288..296].try_into().unwrap()), 8);
        assert_eq!(u32::from_le_bytes(request[296..300].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(request[320..324].try_into().unwrap()), 2);
        assert_eq!(u64::from_le_bytes(request[328..336].try_into().unwrap()), 1);
        assert_eq!(u64::from_le_bytes(request[336..344].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(request[560..564].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(request[588..592].try_into().unwrap()), 0);

        let low = build_output_request(34, false, &[0; 32]);
        assert_eq!(u64::from_le_bytes(low[328..336].try_into().unwrap()), 0);
        assert_eq!(u64::from_le_bytes(low[336..344].try_into().unwrap()), 1);
        assert_eq!(GPIO_V2_GET_LINE_IOCTL, 0xc250_b407);
        assert_eq!(GPIO_V2_LINE_SET_VALUES_IOCTL, 0xc010_b40f);
    }
}
