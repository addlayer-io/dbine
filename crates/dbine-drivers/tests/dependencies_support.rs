//! "Ver dependencias…" shows wherever the engine reports foreign keys or has
//! objects with source (`Driver::supports_dependencies`). The engines left
//! out are listed, with their reason, in docs/engine-support.md.

#[test]
fn engines_without_dependencies_are_documented() {
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/engine-support.md")).unwrap();
    let section = doc.split("### Engines without dependencies").nth(1).expect("section in engine-support.md");
    let section = section.split("\n## ").next().unwrap_or(section);
    // Names wrap across lines in the doc.
    let section = section.split_whitespace().collect::<Vec<_>>().join(" ");
    let missing: Vec<&str> = dbine_drivers::all()
        .iter()
        .filter(|d| !d.supports_dependencies() && !section.contains(d.info().name))
        .map(|d| d.info().name)
        .collect();
    assert!(missing.is_empty(), "without dependencies and not in the doc: {missing:?}");
}
