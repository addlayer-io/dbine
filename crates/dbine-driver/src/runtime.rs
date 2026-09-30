//! Components a driver downloads on first use instead of shipping inside
//! the app (e.g. DuckDB's native library): where they go and how their
//! download progress reaches the UI. The app sets both once at startup;
//! without it (tests, tools) components go to a temp folder and progress
//! is dropped.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

static DIR: OnceLock<PathBuf> = OnceLock::new();
static SINK: OnceLock<Box<dyn Fn(&ComponentProgress) + Send + Sync>> = OnceLock::new();

/// A component download's progress, in bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentProgress {
    /// What is being downloaded, as the UI names it ("DuckDB").
    pub component: String,
    /// The drivers (ids) that wait for it.
    pub drivers: Vec<String>,
    pub done: u64,
    pub total: u64,
}

/// Where downloaded components live (the app's data folder).
pub fn set_components_dir(dir: PathBuf) {
    let _ = DIR.set(dir);
}

pub fn components_dir() -> PathBuf {
    DIR.get().cloned().unwrap_or_else(|| std::env::temp_dir().join("dbine-components"))
}

/// Where download progress goes (the app forwards it to the UI).
pub fn set_progress_sink(f: impl Fn(&ComponentProgress) + Send + Sync + 'static) {
    let _ = SINK.set(Box::new(f));
}

pub fn report_progress(p: &ComponentProgress) {
    if let Some(f) = SINK.get() {
        f(p);
    }
}
