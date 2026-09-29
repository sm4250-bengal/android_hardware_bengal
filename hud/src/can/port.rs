// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Opening the decoder box's tty — read-only by default, or one bounded
//! read/write fd when the explicitly enabled status-refresh path requests it.
//!
//! The default `O_RDONLY | O_NOCTTY` fd cannot be written (EBADF), preserving
//! the receive-only product path.  The request path uses one `O_RDWR` fd with
//! `O_NONBLOCK | O_NOFOLLOW`; a third layer is the product's DAC/SELinux grant
//! on its exact CAN type.
//! The termios setup is the
//! MCU session's ([`crate::mcu::serial`]) with the baud swapped: `VMIN=0`,
//! `VTIME=10` so a silent box returns the read loop once per second for the
//! tell, no flow control (SE5 is two-wire, TLMM 16/17).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::mcu::serial::{configure_raw_8n1, O_NOCTTY};

/// Linux `CBAUD` codes for the rates a decoder box may use.
pub fn cbaud_for(baud: u32) -> Option<u32> {
    Some(match baud {
        9_600 => 0x0000_000d,
        19_200 => 0x0000_000e,
        38_400 => 0x0000_000f,
        57_600 => 0x0000_1001,
        115_200 => 0x0000_1002,
        230_400 => 0x0000_1003,
        460_800 => 0x0000_1004,
        500_000 => 0x0000_1005,
        576_000 => 0x0000_1006,
        921_600 => 0x0000_1007,
        _ => return None,
    })
}

/// Open `path` read-only and configure raw 8N1 at `baud`.
pub fn open_readonly(path: &Path, baud: u32) -> io::Result<File> {
    open_with_flags(path, baud, O_NOCTTY, false)
}

/// Product open additionally refuses symlinks so the exact labelled node is
/// the node opened. The desk CLI permits its synthetic PTY symlink.
pub fn open_product_readonly(path: &Path, baud: u32) -> io::Result<File> {
    const O_NOFOLLOW: i32 = 0x0002_0000;
    open_with_flags(path, baud, O_NOCTTY | O_NOFOLLOW, false)
}

/// Open the exact product node read/write and nonblocking for one-attempt
/// bounded status writes.  This is called only when the request property is
/// true; it still does not send anything during open.
pub fn open_product_readwrite(path: &Path, baud: u32) -> io::Result<File> {
    const O_NOFOLLOW: i32 = 0x0002_0000;
    const O_NONBLOCK: i32 = 0x0000_0800;
    open_with_flags(path, baud, O_NOCTTY | O_NOFOLLOW | O_NONBLOCK, true)
}

fn open_with_flags(path: &Path, baud: u32, flags: i32, writable: bool) -> io::Result<File> {
    let cbaud = cbaud_for(baud).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported CAN baud {baud}"),
        )
    })?;
    let file = OpenOptions::new()
        .read(true)
        .write(writable)
        .custom_flags(flags)
        .open(path)?;
    configure_raw_8n1(file.as_raw_fd(), cbaud)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::can::envelope::encode_frame;
    use crate::can::profile::Profile;
    use crate::can::reader::{Reader, VecSink};
    use crate::mcu::serial::test_support::{pty_pair_with_name, read_all_available};
    use std::io::{Read, Write};

    #[test]
    fn cbaud_table_covers_the_oem_selector_rates_we_accept() {
        assert_eq!(cbaud_for(38_400), Some(0x0f));
        assert_eq!(cbaud_for(115_200), Some(0x1002));
        assert_eq!(cbaud_for(12_345), None);
        assert!(open_readonly(Path::new("/dev/null"), 12_345).is_err());
    }

    #[test]
    fn product_open_refuses_a_symlink_and_preserves_readonly_pty_behavior() {
        let (mut master, slave_name) = pty_pair_with_name();
        let path = Path::new(&slave_name);
        let link = std::env::temp_dir().join(format!("can-product-link-{}", std::process::id()));
        std::os::unix::fs::symlink(path, &link).unwrap();
        assert_eq!(open_product_readonly(&link, 38400).unwrap_err().raw_os_error(), Some(40));
        let mut port = open_product_readonly(path, 38400).unwrap();
        assert_eq!(port.write_all(&[0xff]).unwrap_err().raw_os_error(), Some(9));
        master.write_all(&[0x2e, 0x10, 0, 0xef]).unwrap();
        let mut buf = [0; 4];
        port.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [0x2e, 0x10, 0, 0xef]);
        assert!(read_all_available(&mut master).is_empty());
        std::fs::remove_file(link).unwrap();
    }

    #[test]
    fn product_readwrite_open_is_one_nonblocking_fd_for_refreshes() {
        let (mut master, slave_name) = pty_pair_with_name();
        let mut port = open_product_readwrite(Path::new(&slave_name), 38_400).unwrap();
        let frame = [0x2e, 0x90, 0x02, 0x37, 0x00, 0x36];
        assert_eq!(port.write(&frame).unwrap(), frame.len());
        let mut received = [0u8; 6];
        master.read_exact(&mut received).unwrap();
        assert_eq!(received, frame);
        master.write_all(&[0x2e, 0x10, 0x00, 0xef]).unwrap();
        let mut rx = [0u8; 4];
        port.read_exact(&mut rx).unwrap();
        assert_eq!(rx, [0x2e, 0x10, 0x00, 0xef]);
    }

    #[test]
    fn a_readonly_port_cannot_be_written_and_reads_what_the_box_sends() {
        let (mut master, slave_name) = pty_pair_with_name();
        let mut port = open_readonly(Path::new(&slave_name), 38_400).expect("open pty slave read-only");
        // The box speaks; the reader hears it.
        master.write_all(&[0x2e, 0x10, 0x00, 0xef]).unwrap();
        let mut buf = [0u8; 8];
        let n = port.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], &[0x2e, 0x10, 0x00, 0xef]);
        // A write through the read-only fd is refused by the kernel (EBADF),
        // before any byte could reach the box...
        let err = port.write_all(&[0xff]).expect_err("O_RDONLY fd must refuse a write");
        assert_eq!(err.raw_os_error(), Some(9), "{err}");
        // ...and nothing arrived at the box side.
        assert!(read_all_available(&mut master).is_empty());
    }

    #[test]
    fn the_whole_reader_over_a_pty_never_writes_back_to_the_box() {
        // The receive-only proof at the level the daemon runs: real tty
        // semantics (VTIME timeout returns Ok(0)), the shipped profile, the
        // reader loop -- and the box side stays empty. The OEM would have
        // answered each of these frames with 0xff.
        let (mut master, slave_name) = pty_pair_with_name();
        let profile = Profile::parse(include_str!("../../can/profiles/xinpu-vw-pq.toml")).unwrap();
        // A second slave handle keeps the pty alive after the reader drops its
        // fd, so the master-side drain below sees data (or nothing), not EIO.
        let _keep_open = open_readonly(Path::new(&slave_name), profile.baud).unwrap();
        let port = open_readonly(Path::new(&slave_name), profile.baud).unwrap();
        let mut reader = Reader::new(Some(profile), None, &slave_name, super::super::SE5_MMIO, 38_400);
        let mut sink = VecSink::default();
        let mut stream = encode_frame(0x41, &[0x01, 0x10]);
        stream.extend(encode_frame(0x20, &[0x06, 0x01]));
        master.write_all(&stream).unwrap();
        let started = std::time::Instant::now();
        let clock = || started.elapsed().as_millis() as u64;
        reader
            .run(port, clock, &mut sink, || started.elapsed() > std::time::Duration::from_millis(1500))
            .unwrap();
        assert_eq!(reader.stats.frames_ok, 2);
        assert_eq!(reader.snapshot.get("trunk").unwrap().0.raw, 1);
        assert_eq!(reader.snapshot.last_key().unwrap().0.action.as_deref(), Some("MUTE"));
        assert!(
            read_all_available(&mut master).is_empty(),
            "the reader wrote to the box side"
        );
        assert!(sink.0.iter().any(|(_, l)| l.starts_with("canuart: port=")));
    }
}
