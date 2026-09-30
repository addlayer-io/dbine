//! A driver crate served over stdin / stdout to the DBine app.
//!
//! - `dbine-plugin-host`: serve the one driver crate it was built with.
//! - `dbine-plugin-host --package <crate>`: serve that crate (a build with
//!   several, like the tests').
//! - `dbine-plugin-host --manifest`: print what every built-in driver says
//!   about itself (JSON), for the app to carry.
//! - `dbine-plugin-host --check-catalog <plugins.json>`: check that this
//!   code reads a catalog (older drivers' manifests included); the release
//!   runs it before building the app with it.

fn main() {
    // Before any driver opens a TLS connection (see Cargo.toml).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args: Vec<String> = std::env::args().collect();
    if let Some(path) = args.iter().position(|a| a == "--check-catalog").and_then(|i| args.get(i + 1)) {
        match check_catalog(path) {
            Ok(n) => println!("catálogo válido: {n} drivers"),
            Err(e) => {
                eprintln!("catálogo inválido ({path}): {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    let mut packages = dbine_drivers::packages();
    if args.iter().any(|a| a == "--manifest") {
        let metas: Vec<dbine_plugin::DriverMeta> = packages
            .iter()
            .flat_map(|(package, drivers)| drivers.iter().map(move |d| dbine_plugin::DriverMeta::of(package, d.as_ref())))
            .collect();
        println!("{}", serde_json::to_string_pretty(&metas).expect("manifest"));
        return;
    }
    let wanted = args.iter().position(|a| a == "--package").and_then(|i| args.get(i + 1)).cloned();
    let (package, drivers) = match wanted {
        Some(w) => match packages.iter().position(|(p, _)| *p == w) {
            Some(i) => packages.swap_remove(i),
            None => {
                eprintln!("este host no incluye el driver «{w}»");
                std::process::exit(2);
            }
        },
        None if packages.len() == 1 => packages.remove(0),
        None => {
            eprintln!("este host incluye varios drivers: indicá cuál con --package");
            std::process::exit(2);
        }
    };
    dbine_plugin::host::run(package, drivers)
}

/// The catalog parses with this code's types, and every published host has
/// its drivers' manifest (and the other way around).
fn check_catalog(path: &str) -> Result<usize, String> {
    #[derive(serde::Deserialize)]
    struct File {
        #[serde(flatten)]
        catalog: dbine_plugin::install::Catalog,
        drivers: serde_json::Value,
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let file: File = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let metas = dbine_plugin::parse_manifest(&file.drivers.to_string()).map_err(|e| e.to_string())?;
    for package in file.catalog.hosts.keys() {
        if !metas.iter().any(|m| &m.package == package) {
            return Err(format!("el driver «{package}» no tiene manifiesto"));
        }
    }
    if let Some(m) = metas.iter().find(|m| !file.catalog.hosts.contains_key(&m.package)) {
        return Err(format!("«{}» tiene manifiesto pero no host publicado", m.package));
    }
    Ok(metas.len())
}
