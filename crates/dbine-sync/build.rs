//! The cloud providers' client IDs come from the environment at build time
//! (CI uses repository secrets). For local builds they can also sit in a
//! `.env` at the repository root (git-ignored); the environment wins.

use std::path::Path;

const KEYS: [&str; 3] = ["DBINE_GOOGLE_CLIENT_ID", "DBINE_GOOGLE_CLIENT_SECRET", "DBINE_MICROSOFT_CLIENT_ID"];

fn main() {
    let env_file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env");
    println!("cargo:rerun-if-changed=build.rs");
    for key in KEYS {
        println!("cargo:rerun-if-env-changed={key}");
    }
    // Watched only when it exists: a missing path would rebuild every time.
    // After creating it the first time: `cargo clean -p dbine-sync`.
    let Ok(text) = std::fs::read_to_string(&env_file) else { return };
    println!("cargo:rerun-if-changed={}", env_file.display());
    for line in text.lines() {
        let line = line.trim();
        let Some((key, value)) = line.strip_prefix("export ").unwrap_or(line).split_once('=') else { continue };
        let key = key.trim();
        if !KEYS.contains(&key) || std::env::var_os(key).is_some() {
            continue;
        }
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        println!("cargo:rustc-env={key}={value}");
    }
}
