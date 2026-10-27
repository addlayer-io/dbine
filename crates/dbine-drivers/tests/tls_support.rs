//! Connection encryption: every engine without TLS is listed, with its
//! reason, in docs/engine-support.md ("Connection encryption").

use dbine_driver::TlsSupport;

#[test]
fn engines_without_tls_are_documented() {
    let doc = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/engine-support.md"
    ))
    .unwrap();
    let section = doc
        .split("## Connection encryption")
        .nth(1)
        .expect("section in engine-support.md");
    let section = section.split("\n## ").next().unwrap_or(section);
    let table = section
        .split("Engines without TLS:")
        .nth(1)
        .expect("table of engines without TLS");
    let missing: Vec<&str> = dbine_drivers::all()
        .iter()
        .map(|d| d.info())
        .filter(|i| i.tls == TlsSupport::None && !table.contains(&format!("| {} |", i.name)))
        .map(|i| i.name)
        .collect();
    assert!(
        missing.is_empty(),
        "no TLS and not in the table: {missing:?}"
    );
}
