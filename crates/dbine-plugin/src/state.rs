//! What this machine knows about its downloaded drivers, beside them
//! (`components/drivers/state.json`): the index's `seq` it last accepted,
//! and per driver crate the version in use, the one before it, and the
//! versions that failed here (never picked again).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LocalState {
    /// The newest index accepted: an older one is refused.
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub packages: HashMap<String, PkgState>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PkgState {
    /// The driver id new connections use.
    #[serde(default)]
    pub active: Option<String>,
    /// The one used before (what a rollback goes back to).
    #[serde(default)]
    pub previous: Option<String>,
    /// Driver id → why it was dropped ("user" when the user went back).
    #[serde(default)]
    pub bad: BTreeMap<String, String>,
    /// The version the last rollback left, while it's still the latest
    /// news for the settings page.
    #[serde(default)]
    pub rolled_back_from: Option<String>,
}

impl LocalState {
    /// The saved state; empty when there's none or it's unreadable.
    pub fn load(path: &Path) -> LocalState {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    /// Write it beside and rename it: never half a file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        write_atomic(path, &serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)
    }

    pub fn pkg(&mut self, package: &str) -> &mut PkgState {
        self.packages.entry(package.to_string()).or_default()
    }

    /// `id` becomes the active version; the active one becomes previous.
    pub fn activate(&mut self, package: &str, id: &str) {
        let p = self.pkg(package);
        if p.active.as_deref() == Some(id) {
            return;
        }
        p.previous = p.active.take();
        p.active = Some(id.to_string());
        p.rolled_back_from = None;
    }
}

/// Write `bytes` to `path` through a temporary file and a rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("state.json");
        assert_eq!(LocalState::load(&path), LocalState::default());
        let mut s = LocalState { seq: 7, ..Default::default() };
        s.activate("postgres", "1.0.0+p1.e3");
        s.activate("postgres", "1.0.1+p1.e3");
        s.activate("postgres", "1.0.1+p1.e3");
        s.pkg("postgres").bad.insert("1.0.2+p1.e3".into(), "user".into());
        s.save(&path).unwrap();
        let back = LocalState::load(&path);
        assert_eq!(back, s);
        let p = &back.packages["postgres"];
        assert_eq!((p.active.as_deref(), p.previous.as_deref()), (Some("1.0.1+p1.e3"), Some("1.0.0+p1.e3")));
        assert!(!dir.path().join("sub").join("state.json.tmp").exists());
        // Garbage reads as empty; unknown fields are ignored.
        std::fs::write(&path, b"{nope").unwrap();
        assert_eq!(LocalState::load(&path), LocalState::default());
        std::fs::write(&path, br#"{"seq":3,"future":1,"packages":{"x":{"active":"1.0.0+p1.e3","other":2}}}"#).unwrap();
        assert_eq!(LocalState::load(&path).packages["x"].active.as_deref(), Some("1.0.0+p1.e3"));
    }
}
