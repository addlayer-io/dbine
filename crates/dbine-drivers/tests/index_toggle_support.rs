//! "Deshabilitar / Habilitar índice" shows where the engine can do it
//! (`Driver::supports_index_toggle`). Every engine that lists indexes but
//! can't disable them is listed, with its reason, in
//! docs/engine-support.md.

#[test]
fn engines_without_index_toggle_are_documented() {
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/engine-support.md")).unwrap();
    let section = doc.split("### Engines without disabling indexes").nth(1).expect("section in engine-support.md");
    let section = section.split("\n## ").next().unwrap_or(section);
    // Names wrap across lines in the doc.
    let section = section.split_whitespace().collect::<Vec<_>>().join(" ");
    let missing: Vec<&str> = dbine_drivers::all()
        .iter()
        .filter(|d| d.supports_index_usage() && !d.supports_index_toggle() && !section.contains(d.info().name))
        .map(|d| d.info().name)
        .collect();
    assert!(missing.is_empty(), "lists indexes, can't disable them, and isn't in the doc: {missing:?}");
}
