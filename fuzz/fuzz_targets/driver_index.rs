#![no_main]
//! The signed index of downloadable drivers (`dbine_plugin::index`): its
//! `.sig` and the index itself come from the download server.
//!
//! Input: the signature text, a NUL byte, then the index bytes. The
//! signature is checked against the test key in the crate's testdata, so a
//! seed signed with it reaches the parse through `verify_at`. Since any
//! change breaks the signature, the index is also parsed directly and read
//! the way the updater reads a verified one: driver ids, the version to
//! run (`resolve`) and the manifests it carries.

use dbine_plugin::index::{check_fresh, resolve, verify_at, DriverId, Index};
use dbine_plugin::DriverMeta;
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeMap;

const TEST_PK: &str = include_str!("../../crates/dbine-plugin/src/testdata/test.key.pub");

fuzz_target!(|data: &[u8]| {
    let (sig, body) = match data.iter().position(|&b| b == 0) {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (&[][..], data),
    };
    if let Ok(sig) = std::str::from_utf8(sig) {
        let _ = verify_at(body, sig, TEST_PK, 0, 0);
    }

    let Ok(index) = serde_json::from_slice::<Index>(body) else {
        return;
    };
    let _ = check_fresh(&index, u64::MAX);
    let app = semver::Version::new(1, 4, 0);
    let bad = BTreeMap::new();
    for entries in index.drivers.values() {
        for (key, entry) in entries {
            if let Some(id) = DriverId::parse(key) {
                // The id written back (logs, the `bad` list, the UI) reads
                // as the same id.
                assert_eq!(DriverId::parse(&id.to_string()).as_ref(), Some(&id));
                let _ = resolve(&id, Some(entries), &app, &bad);
            }
            let _ = serde_json::from_value::<Vec<DriverMeta>>(entry.manifest.clone());
        }
    }
});
