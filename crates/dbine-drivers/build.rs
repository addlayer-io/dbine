//! With the `plugins` feature, the catalog of downloadable drivers (what
//! each says about itself and where its host is published) is built into
//! the registry: `DBINE_PLUGIN_CATALOG` points to it (the release workflow
//! writes it; see scripts/build-driver-hosts.py). Without it the build has
//! only its built-in drivers.

fn main() {
    println!("cargo:rerun-if-env-changed=DBINE_PLUGIN_CATALOG");
    if std::env::var_os("CARGO_FEATURE_PLUGINS").is_none() {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("plugins.json");
    match std::env::var("DBINE_PLUGIN_CATALOG") {
        Ok(path) if !path.is_empty() => {
            println!("cargo:rerun-if-changed={path}");
            std::fs::copy(&path, &out).unwrap_or_else(|e| panic!("DBINE_PLUGIN_CATALOG ({path}): {e}"));
        }
        _ => {
            println!("cargo:warning=feature `plugins` sin DBINE_PLUGIN_CATALOG: la app no va a ofrecer drivers descargables");
            std::fs::write(&out, r#"{"target":"","base_url":"","hosts":{},"drivers":[]}"#).unwrap();
        }
    }
}
