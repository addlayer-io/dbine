//! "Ir a la fila referenciada" / "Filas que la referencian" works on every
//! engine that reports foreign keys (`Capabilities::foreign_keys`): the
//! relations come from `database_schema` and the jump is `filtered_browse`,
//! which every driver has. The engines without foreign keys don't show it,
//! and are listed in docs/engine-support.md with the reason.

#[test]
fn engines_without_foreign_keys_are_documented() {
    let doc = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/engine-support.md"
    ))
    .unwrap();
    let section = doc
        .split("## Jump by foreign key")
        .nth(1)
        .expect("section in engine-support.md");
    let section = section.split("\n## ").next().unwrap_or(section);
    // Names wrap across lines in the doc.
    let section = section.split_whitespace().collect::<Vec<_>>().join(" ");
    let missing: Vec<&str> = dbine_drivers::all()
        .iter()
        .filter(|d| !d.capabilities().foreign_keys && !section.contains(d.info().name))
        .map(|d| d.info().name)
        .collect();
    assert!(
        missing.is_empty(),
        "no foreign keys and not in the doc: {missing:?}"
    );
}
