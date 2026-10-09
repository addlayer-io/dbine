//! "Ejecutar en varias bases…" shows on every engine with several databases
//! (`DriverInfo::databases_label` not empty): it runs through
//! `Session::execute`, which every driver has. Engines with a single
//! namespace don't show it, and are listed in docs/engine-support.md.

#[test]
fn single_namespace_engines_are_documented() {
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/engine-support.md")).unwrap();
    let section = doc.split("## Run on several databases").nth(1).expect("section in engine-support.md");
    let section = section.split("\n## ").next().unwrap_or(section);
    // Names wrap across lines in the doc.
    let section = section.split_whitespace().collect::<Vec<_>>().join(" ");
    let missing: Vec<&str> = dbine_drivers::all()
        .iter()
        .filter(|d| d.info().databases_label.is_empty() && !section.contains(d.info().name))
        .map(|d| d.info().name)
        .collect();
    assert!(missing.is_empty(), "single namespace and not in the doc: {missing:?}");
}
