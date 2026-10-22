//! With `plugins` (release builds) the registry lists the built-in drivers
//! from their crates and every other engine from the catalog the build
//! carries, whichever other driver features are on: the release app is built
//! without them (`--no-default-features`) and lists the same as with them.
//!
//! `DBINE_PLUGIN_CATALOG=<plugins.json> cargo test -p dbine-drivers --no-default-features --features plugins --test plugins_catalog`
#![cfg(feature = "plugins")]

#[test]
fn lists_the_built_in_drivers_and_the_catalog() {
    // The catalog's versions, not the drivers installed on this machine.
    std::env::set_var(
        "DBINE_DRIVERS_DIR",
        std::env::temp_dir().join("dbine-test-no-drivers"),
    );
    let mut expected: Vec<String> = Vec::new();
    for (package, drivers) in dbine_drivers::packages() {
        assert!(
            dbine_drivers::BUILT_IN.contains(&package),
            "{package} isn't built in"
        );
        expected.extend(drivers.iter().map(|d| d.info().name.to_string()));
    }
    let downloadable = dbine_drivers::plugins::packages();
    if option_env!("DBINE_PLUGIN_CATALOG").is_some() {
        assert!(!downloadable.is_empty(), "the catalog has no drivers");
    }
    expected.extend(downloadable.into_iter().flat_map(|p| p.drivers));
    expected.sort();
    let mut listed: Vec<String> = dbine_drivers::infos()
        .into_iter()
        .map(|i| i.name.to_string())
        .collect();
    listed.sort();
    assert_eq!(listed, expected);
    println!("{} engines: {}", listed.len(), listed.join(", "));
}
