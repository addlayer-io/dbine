#![no_main]
//! The driver plugin protocol (`dbine_plugin::proto`): length-prefixed
//! MessagePack frames between the app and a driver host, over the host's
//! stdin / stdout. Each side decodes whatever the other process wrote.
//!
//! The first byte picks the side; the rest is the byte stream it reads.
//! Every message that decodes is encoded again, as the side would answer.

use dbine_plugin::proto::{
    read_frame, read_raw, unknown_call, write_frame, FromHost, Hello, Ready, ToHost,
};
use libfuzzer_sys::fuzz_target;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    let Some((&side, stream)) = data.split_first() else {
        return;
    };
    let mut r = Cursor::new(stream);
    if side & 1 == 0 {
        // The host reads the app (host.rs): `Hello`, then `ToHost` frames;
        // a call it doesn't know is answered `Unsupported` by its id.
        if let Ok(Some(hello)) = read_frame::<Hello>(&mut r) {
            let _ = write_frame(&mut Vec::new(), &hello);
        }
        while let Ok(Some(body)) = read_raw(&mut r) {
            match rmp_serde::from_slice::<ToHost>(&body) {
                Ok(msg) => {
                    let _ = write_frame(&mut Vec::new(), &msg);
                }
                Err(_) => {
                    let _ = unknown_call(&body);
                }
            }
        }
    } else {
        // The app reads the host (remote.rs): `Ready`, then `FromHost` frames.
        if let Ok(Some(ready)) = read_frame::<Ready>(&mut r) {
            let _ = write_frame(&mut Vec::new(), &ready);
        }
        while let Ok(Some(msg)) = read_frame::<FromHost>(&mut r) {
            let _ = write_frame(&mut Vec::new(), &msg);
        }
    }
});
