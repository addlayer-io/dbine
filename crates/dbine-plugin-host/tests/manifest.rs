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

#[test]
fn a_manifest_without_schema_spec_reads_as_none() {
    // A driver published before "Nuevo esquema…": its manifest lacks the field.
    let (package, drivers) = dbine_drivers::packages().into_iter().next().unwrap();
    let mut v = serde_json::to_value(DriverMeta::of(package, drivers[0].as_ref())).unwrap();
    v.as_object_mut().unwrap().remove("schema_spec");
    let meta: DriverMeta = serde_json::from_value(v).unwrap();
    assert!(meta.schema_spec.is_none());
}
