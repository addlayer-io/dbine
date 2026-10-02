//! "Ver dependencias…" shows wherever the engine reports foreign keys or has
//! objects with source (`Driver::supports_dependencies`). The engines left
//! out are listed, with their reason, in docs/soporte-por-motor.md.

#[test]
fn engines_without_dependencies_are_documented() {
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/soporte-por-motor.md")).unwrap();
    let section = doc.split("### Motores sin dependencias").nth(1).expect("section in soporte-por-motor.md");
    let section = section.split("\n## ").next().unwrap_or(section);
    let missing: Vec<&str> = dbine_drivers::all()
        .iter()
        .filter(|d| !d.supports_dependencies() && !section.contains(d.info().name))
        .map(|d| d.info().name)
        .collect();
    assert!(missing.is_empty(), "without dependencies and not in the doc: {missing:?}");
}
