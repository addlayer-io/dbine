//! A driver published before the app it runs with: a call it doesn't know
//! is answered "no soportado" and the channel stays up.

use dbine_driver::Error;
use dbine_plugin::proto::{read_frame, write_frame, Call, FromHost, Hello, Ready, Reply, ToHost, PROTOCOL};
use serde::Serialize;
use std::io::{BufReader, BufWriter};
use std::process::{Command, Stdio};

const EXE: &str = env!("CARGO_BIN_EXE_dbine-plugin-host");

#[test]
fn an_unknown_call_is_unsupported_and_the_host_keeps_serving() {
    let mut child = Command::new(EXE)
        .args(["--package", "sqlite"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut w = BufWriter::new(child.stdin.take().unwrap());
    let mut r = BufReader::new(child.stdout.take().unwrap());
    write_frame(&mut w, &Hello { protocol: PROTOCOL, version: "9.9.9".into(), components_dir: None }).unwrap();
    let ready: Ready = read_frame(&mut r).unwrap().unwrap();
    assert_eq!(ready.protocol, PROTOCOL);

    // What a newer app sends.
    #[derive(Serialize)]
    enum NewCall {
        FutureThing { session: u64 },
    }
    #[derive(Serialize)]
    enum NewToHost {
        Call { id: u64, call: NewCall },
    }
    write_frame(&mut w, &NewToHost::Call { id: 7, call: NewCall::FutureThing { session: 1 } }).unwrap();
    match read_frame::<FromHost>(&mut r).unwrap().unwrap() {
        FromHost::Reply { id: 7, result: Err(e) } => {
            let e = Error::from(e);
            assert!(matches!(e, Error::Unsupported(_)), "{e}");
            assert!(e.to_string().contains("FutureThing"), "{e}");
        }
        other => panic!("{other:?}"),
    }

    // Still serving.
    write_frame(&mut w, &ToHost::Call { id: 8, call: Call::Manifest }).unwrap();
    match read_frame::<FromHost>(&mut r).unwrap().unwrap() {
        FromHost::Reply { id: 8, result: Ok(Reply::Manifest(m)) } => assert!(m.iter().any(|d| d.info.id == "sqlite")),
        other => panic!("{other:?}"),
    }
    // Closing its stdin (the app is gone) ends it.
    drop(w);
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(started.elapsed().as_secs() < 10, "the host didn't exit after its stdin closed");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
