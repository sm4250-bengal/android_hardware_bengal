// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 The XYAUTO device-3 project

//! Desk-only decoder for captured CAN-box bytes (the former
//! `xyauto-can-decode`), now over the daemon's own envelope parser so
//! `tools/oem-can-decoder-agreement.py` compares the OEM `canbox_rx_uart()`
//! with the code that ships. Not built by Soong (Android.bp names only
//! `src/main.rs`).

use std::env;
use std::fs;
use std::process::ExitCode;

use bengal_hud::can::envelope::{decode_hex, encode_hex, Item, Parser};

fn usage(program: &str) {
    eprintln!("usage: {program} CAPTURE.bin | {program} --hex HEX-BYTES");
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let bytes = match args.as_slice() {
        [_, path] => match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("read {path}: {error}");
                return ExitCode::FAILURE;
            }
        },
        [_, flag, text] if flag == "--hex" => match decode_hex(text) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("hex input: {error}");
                return ExitCode::from(2);
            }
        },
        _ => {
            usage(args.first().map_or("bengal-can-decode", String::as_str));
            return ExitCode::from(2);
        }
    };

    let mut parser = Parser::new();
    let mut items = parser.push(&bytes);
    items.extend(parser.finish());
    for item in items {
        print_item(&item);
    }
    ExitCode::SUCCESS
}

fn print_item(item: &Item) {
    match item {
        Item::Discarded(raw) => println!("discard raw={}", encode_hex(raw)),
        Item::Incomplete(raw) => println!("incomplete raw={}", encode_hex(raw)),
        Item::Frame(frame) => println!(
            "frame ok=1 type=0x{:02x} len={} data={} checksum=0x{:02x} raw={}",
            frame.message_type,
            frame.data.len(),
            encode_hex(&frame.data),
            frame.checksum,
            encode_hex(&frame.raw)
        ),
        Item::BadChecksum(frame) => println!(
            "frame ok=0 type=0x{:02x} len={} data={} checksum=0x{:02x} expected=0x{:02x} raw={}",
            frame.message_type,
            frame.data.len(),
            encode_hex(&frame.data),
            frame.checksum,
            frame.expected_checksum,
            encode_hex(&frame.raw)
        ),
    }
}
