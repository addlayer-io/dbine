//! The manifest the app carries says exactly what the drivers say: written
//! by `--manifest`, read back, and compared with the drivers themselves.

use dbine_plugin::DriverMeta;

#[test]
fn manifest_round_trips_every_driver() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_dbine-plugin-host")).arg("--manifest").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let read = dbine_plugin::parse_manifest(&text).unwrap();
    let mut expected: Vec<DriverMeta> = Vec::new();
    for (package, drivers) in dbine_drivers::packages() {
        for d in drivers {
            expected.push(DriverMeta::of(package, d.as_ref()));
        }
    }
    assert_eq!(read.len(), expected.len());
    // Serialized again, what was read equals what the drivers say.
    assert_eq!(serde_json::to_value(&read).unwrap(), serde_json::to_value(&expected).unwrap());
    // Reading it twice doesn't grow the interned strings.
    let before = dbine_driver::serde_static::interned();
    let _ = dbine_plugin::parse_manifest(&text).unwrap();
    assert_eq!(dbine_driver::serde_static::interned(), before);
}
