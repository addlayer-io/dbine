//! The drivers index: every host published for a target, signed with the
//! app updater's key. Drivers are released apart from the app; the app
//! reads the index to pick, per driver crate, the newest version it can run
//! (docs/on-demand-drivers.md).
//!
//! A driver's id is `<version>+p<protocol>.e<epoch>`: the crate's own
//! version, the protocol the app and the host speak (`proto::PROTOCOL`) and
//! the drivers epoch. The app only takes versions with the same protocol
//! and epoch as the one it was built with (its "floor"), never older than
//! the floor, never yanked, never one that failed here, and only those whose
//! `min_app` it satisfies.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// The updater's public key (`plugins.updater.pubkey` in
/// src-tauri/tauri.conf.json, checked by a test): the same key signs the
/// app's updates and the drivers index.
pub const UPDATER_PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEM2M0U0QzU3MTZBMTY2QUQKUldTdFpxRVdWMHcreGp2MWg2RDZaRVd1YXFwekNKMDdlV0VWb3JLMnZoeituaXRVdkR0andNU24K";

/// A published driver host's id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DriverId {
    /// The driver crate's version, without build metadata.
    pub version: semver::Version,
    pub protocol: u32,
    pub epoch: u32,
}

impl DriverId {
    pub fn parse(s: &str) -> Option<DriverId> {
        let v = semver::Version::parse(s).ok()?;
        let (p, e) = v.build.as_str().split_once('.')?;
        let protocol = p.strip_prefix('p')?.parse().ok()?;
        let epoch = e.strip_prefix('e')?.parse().ok()?;
        let version = semver::Version { build: semver::BuildMetadata::EMPTY, ..v };
        Some(DriverId { version, protocol, epoch })
    }

    /// Same protocol and epoch: a host this app can talk to.
    pub fn same_line(&self, other: &DriverId) -> bool {
        self.protocol == other.protocol && self.epoch == other.epoch
    }
}

impl fmt::Display for DriverId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}+p{}.e{}", self.version, self.protocol, self.epoch)
    }
}

/// `index-<target>.json`. Unknown fields are ignored: the scripts may add
/// some, and older apps never read this file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Index {
    pub target: String,
    #[serde(default)]
    pub schema: u32,
    /// Grows with every publish (unix time): an older index is a replay.
    #[serde(default)]
    pub seq: u64,
    /// Unix time after which the index is stale and refused (past
    /// [`EXPIRY_GRACE`]): signed with the rest, so a mirror can't serve an
    /// old index forever. None (indexes published before it existed) =
    /// never expires; those are only protected by `seq`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<u64>,
    /// Driver crate → driver id → its published host.
    #[serde(default)]
    pub drivers: BTreeMap<String, BTreeMap<String, IndexEntry>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    /// The gzip, relative to the index's folder.
    pub file: String,
    pub size: u64,
    pub sha256: String,
    /// What its drivers say about themselves (`DriverMeta`s), kept as JSON:
    /// a newer manifest this app can't read only costs the new options.
    #[serde(default)]
    pub manifest: serde_json::Value,
    /// Oldest app version that runs it; none = any.
    #[serde(default)]
    pub min_app: Option<String>,
    /// Why it was withdrawn; none = it wasn't.
    #[serde(default)]
    pub yanked: Option<String>,
    #[serde(default)]
    pub published_at: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// The signature or the key is malformed, or doesn't match.
    Signature(String),
    /// Signed, but not an index.
    Parse(String),
    /// Older than the one already accepted, or than the one the app was
    /// released with.
    OldSeq { got: u64, have: u64 },
    /// Past its `expires` (and the grace): a replayed or frozen index.
    Expired { expires: u64, now: u64 },
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyError::Signature(e) => write!(f, "la firma del índice de drivers no es válida: {e}"),
            VerifyError::Parse(e) => write!(f, "el índice de drivers no se pudo leer: {e}"),
            VerifyError::OldSeq { got, have } => write!(f, "el índice de drivers es más viejo que el que ya se tenía ({got} < {have})"),
            VerifyError::Expired { expires, now } => write!(f, "el índice de drivers venció ({expires} < {now}): el servidor de drivers no publicó uno nuevo"),
        }
    }
}

fn b64_text(s: &str) -> Result<String, VerifyError> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(s.trim()).map_err(|e| VerifyError::Signature(e.to_string()))?;
    String::from_utf8(bytes).map_err(|e| VerifyError::Signature(e.to_string()))
}

/// Check `bytes` against `sig` (the `.sig` that `tauri signer sign` writes:
/// the minisign signature file, in base64) with `pubkey` (base64 of the
/// minisign public key file, as in tauri.conf.json), the same way the
/// updater plugin checks the app's updates; then parse it and refuse it if
/// its `seq` is lower than `min_seq` or it has expired.
///
/// `min_seq` is the highest of the last index accepted here and the one the
/// app was released with ([`crate::install::Catalog::min_index_seq`]), so a
/// fresh install can't be handed an index older than its own release.
pub fn verify(bytes: &[u8], sig: &str, pubkey: &str, min_seq: u64) -> Result<Index, VerifyError> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    verify_at(bytes, sig, pubkey, min_seq, now)
}

/// Clocks that are off by this much still accept an index about to expire.
pub const EXPIRY_GRACE: u64 = 7 * 24 * 3600;

/// [`verify`] at the unix time `now`.
pub fn verify_at(bytes: &[u8], sig: &str, pubkey: &str, min_seq: u64, now: u64) -> Result<Index, VerifyError> {
    let pk = minisign_verify::PublicKey::decode(&b64_text(pubkey)?).map_err(|e| VerifyError::Signature(e.to_string()))?;
    let signature = minisign_verify::Signature::decode(&b64_text(sig)?).map_err(|e| VerifyError::Signature(e.to_string()))?;
    pk.verify(bytes, &signature, true).map_err(|e| VerifyError::Signature(e.to_string()))?;
    let index: Index = serde_json::from_slice(bytes).map_err(|e| VerifyError::Parse(e.to_string()))?;
    if index.seq < min_seq {
        return Err(VerifyError::OldSeq { got: index.seq, have: min_seq });
    }
    check_fresh(&index, now)?;
    Ok(index)
}

/// Refuse an index past its `expires` (plus [`EXPIRY_GRACE`]) at `now`.
pub fn check_fresh(index: &Index, now: u64) -> Result<(), VerifyError> {
    match index.expires {
        Some(expires) if now > expires.saturating_add(EXPIRY_GRACE) => Err(VerifyError::Expired { expires, now }),
        _ => Ok(()),
    }
}

/// What a driver crate should run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub id: DriverId,
    /// Its index entry; none when it's the floor and the index doesn't have
    /// it (the catalog the app carries describes it).
    pub entry: Option<IndexEntry>,
    /// A newer version this app can't run: (its id, the app it needs).
    pub needs_app: Option<(DriverId, String)>,
}

impl PartialEq for IndexEntry {
    fn eq(&self, o: &Self) -> bool {
        self.file == o.file && self.sha256 == o.sha256
    }
}
impl Eq for IndexEntry {}

/// The newest usable version of a driver crate: same protocol and epoch as
/// `floor`, not older than it, not yanked, not in `bad`, and `min_app` at
/// most `app`. The floor when nothing is (or the index has nothing).
pub fn resolve(floor: &DriverId, entries: Option<&BTreeMap<String, IndexEntry>>, app: &semver::Version, bad: &BTreeMap<String, String>) -> Choice {
    let mut best: Option<(DriverId, &IndexEntry)> = None;
    let mut blocked: Option<(DriverId, String)> = None;
    for (key, entry) in entries.into_iter().flatten() {
        let Some(id) = DriverId::parse(key) else { continue };
        // An empty `yanked` or `min_app` counts as none (as the scripts read them).
        let yanked = entry.yanked.as_deref().is_some_and(|y| !y.is_empty());
        if !id.same_line(floor) || id.version < floor.version || yanked || bad.contains_key(&id.to_string()) {
            continue;
        }
        let runs = match entry.min_app.as_deref().filter(|m| !m.is_empty()) {
            None => true,
            Some(m) => semver::Version::parse(m).map(|m| m <= *app).unwrap_or(false),
        };
        if runs {
            if best.as_ref().is_none_or(|(b, _)| id.version > b.version) {
                best = Some((id, entry));
            }
        } else if blocked.as_ref().is_none_or(|(b, _)| id.version > b.version) {
            blocked = Some((id, entry.min_app.clone().unwrap_or_default()));
        }
    }
    let (id, entry) = match best {
        Some((id, e)) => (id, Some(e.clone())),
        None => (floor.clone(), None),
    };
    let needs_app = blocked.filter(|(b, _)| b.version > id.version);
    Choice { id, entry, needs_app }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> DriverId {
        DriverId::parse(s).unwrap()
    }

    fn entry(min_app: Option<&str>, yanked: Option<&str>) -> IndexEntry {
        IndexEntry {
            file: "f".into(),
            size: 1,
            sha256: "00".into(),
            manifest: serde_json::Value::Null,
            min_app: min_app.map(str::to_string),
            yanked: yanked.map(str::to_string),
            published_at: None,
        }
    }

    #[test]
    fn parses_driver_ids() {
        let d = id("1.2.3+p1.e3");
        assert_eq!((d.version.to_string(), d.protocol, d.epoch), ("1.2.3".into(), 1, 3));
        assert_eq!(d.to_string(), "1.2.3+p1.e3");
        assert_eq!(id("0.1.0-rc.1+p2.e10").to_string(), "0.1.0-rc.1+p2.e10");
        assert!(id("1.0.0+p1.e3").same_line(&id("2.0.0+p1.e3")));
        assert!(!id("1.0.0+p1.e3").same_line(&id("1.0.0+p2.e3")));
        assert!(!id("1.0.0+p1.e3").same_line(&id("1.0.0+p1.e4")));
        for bad in ["1.2.3", "1.2.3+p1", "1.2.3+x1.e3", "1.2.3+p1.x3", "1.2+p1.e3", "", "1.2.3+p.e3"] {
            assert_eq!(DriverId::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn resolves_the_newest_usable_version() {
        let floor = id("1.0.0+p1.e3");
        let app = semver::Version::parse("0.2.0").unwrap();
        let none = BTreeMap::new();
        let pick = |entries: &[(&str, IndexEntry)], bad: &BTreeMap<String, String>| {
            let m: BTreeMap<String, IndexEntry> = entries.iter().map(|(k, e)| (k.to_string(), e.clone())).collect();
            let c = resolve(&floor, Some(&m), &app, bad);
            (c.id.to_string(), c.needs_app.map(|(i, m)| format!("{i} {m}")))
        };
        // Empty index, or none at all: the floor.
        assert_eq!(pick(&[], &none), ("1.0.0+p1.e3".into(), None));
        assert_eq!(resolve(&floor, None, &app, &none).id, floor);
        // Highest semver, not lexicographic.
        assert_eq!(pick(&[("1.0.2+p1.e3", entry(None, None)), ("1.0.10+p1.e3", entry(None, None))], &none).0, "1.0.10+p1.e3");
        // min_app: equal runs, higher doesn't and is reported.
        assert_eq!(
            pick(&[("1.0.1+p1.e3", entry(Some("0.2.0"), None)), ("1.0.2+p1.e3", entry(Some("0.3.0"), None))], &none),
            ("1.0.1+p1.e3".into(), Some("1.0.2+p1.e3 0.3.0".into()))
        );
        // Empty yanked / min_app: none.
        assert_eq!(pick(&[("1.0.1+p1.e3", entry(Some(""), Some("")))], &none).0, "1.0.1+p1.e3");
        // An unreadable min_app is never satisfied.
        assert_eq!(pick(&[("1.0.1+p1.e3", entry(Some("nope"), None))], &none).0, "1.0.0+p1.e3");
        // Yanked.
        assert_eq!(pick(&[("1.0.1+p1.e3", entry(None, None)), ("1.0.2+p1.e3", entry(None, Some("crash")))], &none).0, "1.0.1+p1.e3");
        // Bad here.
        let bad = BTreeMap::from([("1.0.2+p1.e3".to_string(), "exited".to_string())]);
        assert_eq!(pick(&[("1.0.1+p1.e3", entry(None, None)), ("1.0.2+p1.e3", entry(None, None))], &bad).0, "1.0.1+p1.e3");
        // Other protocol or epoch.
        assert_eq!(pick(&[("2.0.0+p2.e3", entry(None, None)), ("2.0.0+p1.e4", entry(None, None))], &none).0, "1.0.0+p1.e3");
        // Floor newer than the index: never below it.
        assert_eq!(pick(&[("0.9.0+p1.e3", entry(None, None))], &none).0, "1.0.0+p1.e3");
        // The floor's own entry is returned with its data.
        let m = BTreeMap::from([("1.0.0+p1.e3".to_string(), entry(None, None))]);
        assert!(resolve(&floor, Some(&m), &app, &none).entry.is_some());
        // A blocked version older than the pick isn't reported.
        assert_eq!(
            pick(&[("1.0.1+p1.e3", entry(Some("9.0.0"), None)), ("1.0.2+p1.e3", entry(None, None))], &none),
            ("1.0.2+p1.e3".into(), None)
        );
        // Garbage keys are skipped.
        assert_eq!(pick(&[("latest", entry(None, None))], &none).0, "1.0.0+p1.e3");
    }

    const PK: &str = include_str!("testdata/test.key.pub");
    const IDX100: &[u8] = include_bytes!("testdata/index-seq100.json");
    const SIG100: &str = include_str!("testdata/index-seq100.json.sig");
    const IDX50: &[u8] = include_bytes!("testdata/index-seq50.json");
    const SIG50: &str = include_str!("testdata/index-seq50.json.sig");

    #[test]
    fn verifies_the_signature() {
        let i = verify(IDX100, SIG100, PK, 0).unwrap();
        assert_eq!((i.target.as_str(), i.schema, i.seq), ("test", 2, 100));
        let e = &i.drivers["postgres"]["0.1.10+p1.e3"];
        assert_eq!((e.min_app.as_deref(), e.yanked.as_deref()), (Some("0.1.9"), None));
        // Same seq again is fine; a lower one is a replay.
        assert!(verify(IDX100, SIG100, PK, 100).is_ok());
        assert_eq!(verify(IDX50, SIG50, PK, 100).unwrap_err(), VerifyError::OldSeq { got: 50, have: 100 });
        // Tampered bytes, a signature of other bytes, another key, garbage.
        let mut tampered = IDX100.to_vec();
        let n = tampered.len();
        tampered[n - 2] = b' ';
        assert!(matches!(verify(&tampered, SIG100, PK, 0), Err(VerifyError::Signature(_))));
        assert!(matches!(verify(IDX100, SIG50, PK, 0), Err(VerifyError::Signature(_))));
        assert!(matches!(verify(IDX100, SIG100, UPDATER_PUBKEY, 0), Err(VerifyError::Signature(_))));
        assert!(matches!(verify(IDX100, "no", PK, 0), Err(VerifyError::Signature(_))));
        assert!(matches!(verify(IDX100, "", PK, 0), Err(VerifyError::Signature(_))));
    }

    #[test]
    fn the_shipped_seq_is_a_floor() {
        // A fresh install (nothing accepted yet) released when seq 100 was
        // published: an older signed index (a replay, a rolled-back mirror)
        // is refused; the same or newer is taken.
        assert_eq!(verify(IDX50, SIG50, PK, 0u64.max(100)).unwrap_err(), VerifyError::OldSeq { got: 50, have: 100 });
        assert_eq!(verify(IDX100, SIG100, PK, 0u64.max(100)).unwrap().seq, 100);
        assert_eq!(verify(IDX100, SIG100, PK, 0u64.max(101)).unwrap_err(), VerifyError::OldSeq { got: 100, have: 101 });
    }

    #[test]
    fn an_expired_index_is_refused() {
        let i = |expires| Index { target: "t".into(), schema: 2, seq: 1, expires, drivers: BTreeMap::new() };
        let day = 24 * 3600;
        // No `expires` (older indexes): never stale.
        assert!(check_fresh(&i(None), u64::MAX).is_ok());
        // Before it, and within the grace for a clock that runs ahead.
        assert!(check_fresh(&i(Some(1000 * day)), 999 * day).is_ok());
        assert!(check_fresh(&i(Some(1000 * day)), 1000 * day + EXPIRY_GRACE).is_ok());
        assert_eq!(
            check_fresh(&i(Some(1000 * day)), 1000 * day + EXPIRY_GRACE + 1),
            Err(VerifyError::Expired { expires: 1000 * day, now: 1000 * day + EXPIRY_GRACE + 1 })
        );
        // It's parsed from the signed bytes; the fixtures have none.
        let parsed: Index = serde_json::from_str(r#"{"target":"t","seq":5,"expires":42,"drivers":{}}"#).unwrap();
        assert_eq!(parsed.expires, Some(42));
        assert_eq!(verify_at(IDX100, SIG100, PK, 0, u64::MAX).unwrap().expires, None);
    }

    #[test]
    fn the_embedded_key_is_the_updaters() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../../../src-tauri/tauri.conf.json")).unwrap();
        assert_eq!(conf["plugins"]["updater"]["pubkey"].as_str(), Some(UPDATER_PUBKEY));
    }
}
