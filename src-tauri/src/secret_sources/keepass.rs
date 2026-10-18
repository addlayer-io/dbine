//! KeePass: one field of an entry of a .kdbx file (KDBX 3.1 and 4), read
//! here with the `keepass` crate, without any KeePass program. The file is
//! opened with the master password and/or the key file on a blocking
//! thread (Argon2 takes its time on purpose), the value taken and the rest
//! of the database dropped.
//!
//! The entry: `Group/Subgroup/Title` (from the root's children; the root's
//! own name may lead), a title alone when it's unique, or the entry's UUID.
//! Entries in the recycle bin don't count. Messages name entries and fields,
//! never a value.

use keepass::db::{fields, DatabaseOpenError};
use keepass::error::{CryptographyError, DatabaseKeyError};
use keepass::{Database, DatabaseKey};
use std::path::{Path, PathBuf};

/// The master password or the key file didn't open it. `fill` checks for
/// this exact text to ask an asked-for password again.
pub const WRONG_KEY: &str = "la contraseña maestra o el archivo de clave no abren la base de KeePass";

pub struct Request {
    pub file: PathBuf,
    pub entry: String,
    /// Empty: `Password`.
    pub attribute: String,
    pub key_file: Option<PathBuf>,
    pub username: bool,
}

/// `(value, username)`. `master`: none for a key file alone.
pub async fn read(req: Request, master: Option<String>) -> Result<(String, Option<String>), String> {
    tokio::task::spawn_blocking(move || {
        let mut master = master;
        let r = read_blocking(&req, master.as_deref());
        if let Some(m) = &mut master {
            dbine_core::secret_sources::wipe(m);
        }
        r
    })
    .await
    .map_err(|e| format!("la lectura de la base de KeePass se interrumpió: {e}"))?
}

pub fn read_blocking(req: &Request, master: Option<&str>) -> Result<(String, Option<String>), String> {
    let db = open(&req.file, req.key_file.as_deref(), master)?;
    let (value, username) = {
        let entry = find_entry(&db, &req.entry)?;
        let value = field(&entry, &req.attribute, &req.entry)?;
        let username = if req.username { entry.get(fields::USERNAME).map(str::to_string).filter(|u| !u.is_empty()) } else { None };
        (value, username)
    };
    if req.username && username.is_none() {
        let mut v = value;
        dbine_core::secret_sources::wipe(&mut v);
        return Err(format!("la entrada «{}» de KeePass no tiene usuario (UserName)", req.entry.trim()));
    }
    Ok((value, username))
}

fn open(file: &Path, key_file: Option<&Path>, master: Option<&str>) -> Result<Database, String> {
    let mut key = DatabaseKey::new();
    if let Some(m) = master.filter(|m| !m.is_empty()) {
        key = key.with_password(m);
    }
    if let Some(kf) = key_file {
        let mut f = std::fs::File::open(kf).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("no existe el archivo de clave «{}»", kf.display()),
            _ => format!("no se pudo leer el archivo de clave «{}»: {e}", kf.display()),
        })?;
        key = key.with_keyfile(&mut f).map_err(|e| format!("no se pudo leer el archivo de clave «{}»: {e}", kf.display()))?;
    }
    if key.is_empty() {
        return Err("falta la contraseña maestra de la base de KeePass".into());
    }
    let mut f = std::fs::File::open(file).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("no existe el archivo de KeePass «{}»", file.display()),
        _ => format!("no se pudo leer el archivo de KeePass «{}»: {e}", file.display()),
    })?;
    Database::open(&mut f, key).map_err(|e| open_error(e, file))
}

fn open_error(e: DatabaseOpenError, file: &Path) -> String {
    match e {
        DatabaseOpenError::Key(DatabaseKeyError::IncorrectKey) => WRONG_KEY.into(),
        // KDBX 3.1: the payload's padding is checked before its first bytes,
        // so a wrong key ends there.
        DatabaseOpenError::Cryptography(CryptographyError::InvalidPadding(_)) => WRONG_KEY.into(),
        DatabaseOpenError::Key(DatabaseKeyError::InvalidKeyFile | DatabaseKeyError::Xml(_)) => "el archivo de clave no es válido".into(),
        DatabaseOpenError::Key(e) => format!("la clave de la base de KeePass no es válida: {e}"),
        DatabaseOpenError::Io(e) => format!("no se pudo leer el archivo de KeePass «{}»: {e}", file.display()),
        DatabaseOpenError::UnsupportedVersion => format!("«{}» es de una versión de KeePass que no se puede leer (se leen KDBX 3.1 y 4)", file.display()),
        e => format!("«{}» no es una base de KeePass (.kdbx) válida: {e}", file.display()),
    }
}

/// The names from below the root down to the entry's group.
fn group_path(entry: &keepass::db::EntryRef<'_>) -> (Vec<String>, String) {
    let mut names = Vec::new();
    let mut g = entry.parent();
    let root_name;
    loop {
        match g.parent() {
            Some(p) => {
                names.push(g.name.clone());
                // `p` borrows the database, not `g`: walk up by id.
                let id = p.id();
                g = entry.database().group(id).expect("the parent is in the database");
            }
            None => {
                root_name = g.name.clone();
                break;
            }
        }
    }
    names.reverse();
    (names, root_name)
}

/// The entry's place, for messages: `Group/Sub/Title`.
fn display_path(entry: &keepass::db::EntryRef<'_>) -> String {
    let (mut p, _) = group_path(entry);
    p.push(entry.get_title().unwrap_or("").to_string());
    p.join("/")
}

fn in_recycle_bin(db: &Database, entry: &keepass::db::EntryRef<'_>) -> bool {
    let Some(bin) = db.recycle_bin().map(|b| b.id()) else { return false };
    let mut g = entry.parent();
    loop {
        if g.id() == bin {
            return true;
        }
        match g.parent() {
            Some(p) => {
                let id = p.id();
                g = db.group(id).expect("the parent is in the database");
            }
            None => return false,
        }
    }
}

/// A UUID as KeePass shows it: with or without braces and dashes.
fn parse_uuid(s: &str) -> Option<uuid::Uuid> {
    let t = s.trim().trim_start_matches('{').trim_end_matches('}');
    let hex: String = t.chars().filter(|c| *c != '-').collect();
    (hex.len() == 32 && hex.chars().all(|c| c.is_ascii_hexdigit())).then(|| uuid::Uuid::parse_str(&hex).ok()).flatten()
}

pub fn find_entry<'a>(db: &'a Database, wanted: &str) -> Result<keepass::db::EntryRef<'a>, String> {
    let wanted = wanted.trim();
    let entries: Vec<_> = db.iter_all_entries().filter(|e| !in_recycle_bin(db, e)).collect();
    if let Some(id) = parse_uuid(wanted) {
        if let Some(i) = entries.iter().position(|e| e.id().uuid() == id) {
            return Ok(entries.into_iter().nth(i).expect("found above"));
        }
    }
    let segs: Vec<&str> = wanted.split('/').map(str::trim).filter(|s| !s.is_empty()).collect();
    let (title, groups) = segs.split_last().map(|(t, g)| (*t, g.to_vec())).unwrap_or(("", vec![]));
    let matches_path = |e: &keepass::db::EntryRef<'_>| {
        if e.get_title().unwrap_or("") != title {
            return false;
        }
        if groups.is_empty() {
            return true;
        }
        let (path, root) = group_path(e);
        path.iter().map(String::as_str).eq(groups.iter().copied()) || std::iter::once(root.as_str()).chain(path.iter().map(String::as_str)).eq(groups.iter().copied())
    };
    let mut found: Vec<usize> = (0..entries.len()).filter(|&i| matches_path(&entries[i])).collect();
    // A title that has slashes in it.
    if found.is_empty() && !groups.is_empty() {
        found = (0..entries.len()).filter(|&i| entries[i].get_title() == Some(wanted)).collect();
    }
    match found.len() {
        1 => Ok(entries.into_iter().nth(found[0]).expect("index in range")),
        0 => Err(not_found(&entries, wanted, title)),
        n => {
            let mut paths: Vec<String> = found.iter().take(6).map(|&i| display_path(&entries[i])).collect();
            paths.sort();
            Err(format!("hay {n} entradas «{wanted}» en la base de KeePass ({}): indicá la ruta del grupo o el UUID", paths.join(", ")))
        }
    }
}

/// Edit distance, for "did you mean".
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push((prev[j] + usize::from(ca != *cb)).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

fn not_found(entries: &[keepass::db::EntryRef<'_>], wanted: &str, title: &str) -> String {
    let t = title.to_lowercase();
    let mut near: Vec<(usize, String)> = entries
        .iter()
        .filter_map(|e| {
            let et = e.get_title()?.to_lowercase();
            let d = if !t.is_empty() && (et.contains(&t) || t.contains(&et)) { 0 } else { distance(&t, &et) };
            (d <= (t.chars().count() / 2).max(2)).then(|| (d, display_path(e)))
        })
        .collect();
    near.sort();
    near.dedup_by(|a, b| a.1 == b.1);
    if near.is_empty() {
        return format!("no hay una entrada «{wanted}» en la base de KeePass");
    }
    let list: Vec<String> = near.into_iter().take(8).map(|(_, p)| p).collect();
    format!("no hay una entrada «{wanted}» en la base de KeePass (parecidas: {})", list.join(", "))
}

/// The entry's `attribute` (empty: the password). Exact name first, then
/// ignoring case (`password` finds `Password`).
pub fn field(entry: &keepass::db::EntryRef<'_>, attribute: &str, wanted: &str) -> Result<String, String> {
    let attr = Some(attribute.trim()).filter(|a| !a.is_empty()).unwrap_or(fields::PASSWORD);
    if let Some(v) = entry.get(attr) {
        return Ok(v.to_string());
    }
    if let Some((_, v)) = entry.fields.iter().find(|(k, _)| k.eq_ignore_ascii_case(attr)) {
        return Ok(v.get().to_string());
    }
    let mut names: Vec<&str> = entry.fields.keys().map(String::as_str).take(30).collect();
    names.sort_unstable();
    Err(format!("la entrada «{}» de KeePass no tiene el campo «{attr}» (tiene: {})", wanted.trim(), names.join(", ")))
}

// ---- browse ------------------------------------------------------------------

/// The database's entries (outside the recycle bin) with their group paths,
/// or one entry's field names (`item`). Opened on a blocking thread; the
/// master password is wiped after.
pub async fn browse(file: PathBuf, key_file: Option<PathBuf>, master: Option<String>, item: Option<String>) -> Result<super::browse::Listing, String> {
    tokio::task::spawn_blocking(move || {
        let mut master = master;
        let r = browse_blocking(&file, key_file.as_deref(), master.as_deref(), item.as_deref());
        if let Some(m) = &mut master {
            dbine_core::secret_sources::wipe(m);
        }
        r
    })
    .await
    .map_err(|e| format!("la lectura de la base de KeePass se interrumpió: {e}"))?
}

/// The standard fields first, in KeePass' order; then the others by name.
const STANDARD: [&str; 5] = [fields::TITLE, fields::USERNAME, fields::PASSWORD, fields::URL, fields::NOTES];

pub fn browse_blocking(file: &Path, key_file: Option<&Path>, master: Option<&str>, item: Option<&str>) -> Result<super::browse::Listing, String> {
    use super::browse::{Entry, Listing};
    let db = open(file, key_file, master)?;
    if let Some(wanted) = item {
        let entry = find_entry(&db, wanted)?;
        let mut names: Vec<String> = entry.fields.keys().cloned().collect();
        names.sort_by_key(|n| (STANDARD.iter().position(|s| s == n).unwrap_or(STANDARD.len()), n.to_lowercase()));
        return Ok(Listing::fields(names));
    }
    let entries: Vec<_> = db.iter_all_entries().filter(|e| !in_recycle_bin(&db, e)).collect();
    // (path, title, uuid, a group or the title has a slash or edge spaces).
    let placed: Vec<(String, String, String, bool)> = entries
        .iter()
        .map(|e| {
            let (path, _) = group_path(e);
            let title = e.get_title().unwrap_or("").to_string();
            let odd = path.iter().chain(std::iter::once(&title)).any(|s| s.contains('/') || s.trim() != s || s.is_empty());
            (path.join("/"), title, e.id().uuid().hyphenated().to_string(), odd)
        })
        .collect();
    // `Group/Title` when that names one entry (find_entry reads it back the
    // same way), else the UUID.
    let full = |p: &str, t: &str| if p.is_empty() { t.to_string() } else { format!("{p}/{t}") };
    let wholes: Vec<String> = placed.iter().map(|(p, t, _, _)| full(p, t)).collect();
    let unique = super::browse::unique_names(wholes.iter().map(String::as_str));
    let titles = super::browse::unique_names(placed.iter().map(|(_, t, _, _)| t.as_str()));
    let mut out: Vec<Entry> = placed
        .into_iter()
        .map(|(path, title, uuid, odd)| {
            let whole = full(&path, &title);
            // At the top a title alone matches that title anywhere.
            let named = if path.is_empty() { titles.contains(&title) } else { unique.contains(&whole) };
            let id = if !odd && named { whole } else { uuid.clone() };
            let name = if title.is_empty() { uuid.clone() } else { title };
            Entry::item(id, name, path, uuid)
        })
        .collect();
    out.sort_by(|a, b| (a.path.to_lowercase(), a.name.to_lowercase()).cmp(&(b.path.to_lowercase(), b.name.to_lowercase())));
    Ok(Listing::complete(out))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use keepass::config::{DatabaseConfig, KdfConfig, OuterCipherConfig};
    use keepass::db::EntryMut;

    /// A KDBX 4 database written here: Argon2 with small costs (the test
    /// runs in a debug build), the given outer cipher, and these entries:
    /// `Databases/Prod/pg-main` and `Databases/Staging/pg-main` (the same
    /// title twice), `Databases/Prod/mysql` with a custom field, and one
    /// in the recycle bin.
    pub fn sample_db(cipher: OuterCipherConfig, argon2id: bool) -> Database {
        let mut config = DatabaseConfig::default();
        config.outer_cipher_config = cipher;
        // The default is Argon2d (1 GiB): the same, cheaper.
        let KdfConfig::Argon2 { version, .. } = config.kdf_config.clone() else { panic!("Argon2d by default") };
        config.kdf_config = if argon2id {
            KdfConfig::Argon2id { iterations: 2, memory: 64 * 1024, parallelism: 1, version }
        } else {
            KdfConfig::Argon2 { iterations: 2, memory: 64 * 1024, parallelism: 1, version }
        };
        let mut db = Database::with_config(config);
        db.root_mut().name = "Root".into();
        let mut root = db.root_mut();
        let mut dbs = root.add_group();
        dbs.name = "Databases".into();
        let mut prod = dbs.add_group();
        prod.name = "Prod".into();
        prod.add_entry().edit(|e: &mut EntryMut<'_>| {
            e.set_unprotected(fields::TITLE, "pg-main");
            e.set_unprotected(fields::USERNAME, "app_user");
            e.set_protected(fields::PASSWORD, "pg-secret-1");
        });
        prod.add_entry().edit(|e: &mut EntryMut<'_>| {
            e.set_unprotected(fields::TITLE, "mysql");
            e.set_unprotected(fields::USERNAME, "root");
            e.set_protected(fields::PASSWORD, "my-secret");
            e.set_protected("API token", "tok-123");
        });
        let mut root = db.root_mut();
        let mut dbs2 = root.group_by_name_mut("Databases").unwrap();
        let mut staging = dbs2.add_group();
        staging.name = "Staging".into();
        staging.add_entry().edit(|e: &mut EntryMut<'_>| {
            e.set_unprotected(fields::TITLE, "pg-main");
            e.set_unprotected(fields::USERNAME, "stage_user");
            e.set_protected(fields::PASSWORD, "staging-secret");
        });
        let mut root = db.root_mut();
        let mut bin = root.add_group();
        bin.name = "Recycle Bin".into();
        let bin_id = bin.id();
        bin.add_entry().edit(|e: &mut EntryMut<'_>| {
            e.set_unprotected(fields::TITLE, "old-db");
            e.set_protected(fields::PASSWORD, "deleted");
        });
        db.meta.recyclebin_uuid = Some(bin_id.uuid());
        db.meta.recyclebin_enabled = Some(true);
        db
    }

    pub fn save(db: &Database, path: &Path, password: Option<&str>, key_file: Option<&Path>) {
        let mut key = DatabaseKey::new();
        if let Some(p) = password {
            key = key.with_password(p);
        }
        if let Some(k) = key_file {
            key = key.with_keyfile(&mut std::fs::File::open(k).unwrap()).unwrap();
        }
        db.save(&mut std::fs::File::create(path).unwrap(), key).unwrap();
    }

    fn req(file: &Path, entry: &str) -> Request {
        Request { file: file.to_path_buf(), entry: entry.into(), attribute: String::new(), key_file: None, username: false }
    }

    /// The fixtures (src/secret_sources/testdata/): `kdbx31-keyfile.kdbx`
    /// was written by KeePassXC 2.7 (KDBX 3.1, AES-KDF, master password
    /// `dummy-master` plus `kdbx31.keyx`); `kdbx31-password.kdbx` is the
    /// keepass-rs project's `test_db_with_password.kdbx` (KDBX 3.1, MIT,
    /// password `demopass`). Dummy secrets only.
    fn testdata(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/secret_sources/testdata").join(name)
    }

    #[test]
    fn kdbx4_argon2_with_a_key_file() {
        let dir = super::super::tests::temp_dir("kdbx4");
        let kf = dir.join("db.keyx");
        std::fs::write(&kf, b"0123456789abcdef0123456789abcdef-some-random-key-file-bytes").unwrap();
        for (i, (cipher, id)) in [(OuterCipherConfig::AES256, false), (OuterCipherConfig::ChaCha20, true), (OuterCipherConfig::Twofish, false)].into_iter().enumerate() {
            let file = dir.join(format!("db{i}.kdbx"));
            save(&sample_db(cipher, id), &file, Some("dummy-master"), Some(&kf));
            let mut r = req(&file, "Databases/Prod/pg-main");
            r.key_file = Some(kf.clone());
            r.username = true;
            let (v, u) = read_blocking(&r, Some("dummy-master")).unwrap();
            assert_eq!((v.as_str(), u.as_deref()), ("pg-secret-1", Some("app_user")));
            // The password alone, or the key file alone, isn't enough.
            r.key_file = None;
            assert_eq!(read_blocking(&r, Some("dummy-master")).unwrap_err(), WRONG_KEY);
            r.key_file = Some(kf.clone());
            assert_eq!(read_blocking(&r, Some("wrong")).unwrap_err(), WRONG_KEY);
            assert_eq!(read_blocking(&r, None).unwrap_err(), WRONG_KEY);
        }
        // A key file alone.
        let file = dir.join("keyonly.kdbx");
        save(&sample_db(OuterCipherConfig::AES256, false), &file, None, Some(&kf));
        let mut r = req(&file, "Databases/Staging/pg-main");
        r.key_file = Some(kf.clone());
        assert_eq!(read_blocking(&r, None).unwrap().0, "staging-secret");
    }

    #[test]
    fn entries_by_path_title_and_uuid() {
        let dir = super::super::tests::temp_dir("kdbx-entries");
        let file = dir.join("db.kdbx");
        let db = sample_db(OuterCipherConfig::AES256, false);
        save(&db, &file, Some("pw"), None);
        let db = open(&file, None, Some("pw")).unwrap();
        let path = |w: &str| find_entry(&db, w).map(|e| display_path(&e));
        assert_eq!(path("Databases/Prod/pg-main").unwrap(), "Databases/Prod/pg-main");
        assert_eq!(path("Root/Databases/Staging/pg-main").unwrap(), "Databases/Staging/pg-main", "the root's name may lead");
        assert_eq!(path(" /Databases/Prod/pg-main/ ").unwrap(), "Databases/Prod/pg-main");
        assert_eq!(path("mysql").unwrap(), "Databases/Prod/mysql", "a unique title alone");
        // The same title twice: ambiguous, with where each one is.
        let e = path("pg-main").unwrap_err();
        assert!(e.contains("hay 2 entradas") && e.contains("Databases/Prod/pg-main") && e.contains("Databases/Staging/pg-main") && e.contains("UUID"), "{e}");
        // Not found: nearby titles, never values.
        let e = path("Databases/Prod/pg-mian").unwrap_err();
        assert!(e.contains("no hay una entrada") && e.contains("Databases/Prod/pg-main") && !e.contains("secret"), "{e}");
        let e = path("zzzzzzzzzz").unwrap_err();
        assert_eq!(e, "no hay una entrada «zzzzzzzzzz» en la base de KeePass");
        // The recycle bin doesn't count.
        assert!(path("old-db").is_err());
        // By UUID, in any of the ways KeePass shows it.
        let id = find_entry(&db, "mysql").unwrap().id().uuid();
        for form in [id.hyphenated().to_string(), format!("{{{}}}", id.hyphenated()), id.simple().to_string().to_uppercase()] {
            assert_eq!(path(&form).unwrap(), "Databases/Prod/mysql", "{form}");
        }
    }

    #[test]
    fn fields() {
        let dir = super::super::tests::temp_dir("kdbx-fields");
        let file = dir.join("db.kdbx");
        save(&sample_db(OuterCipherConfig::AES256, false), &file, Some("pw"), None);
        let mut r = req(&file, "Databases/Prod/mysql");
        assert_eq!(read_blocking(&r, Some("pw")).unwrap().0, "my-secret");
        r.attribute = "UserName".into();
        assert_eq!(read_blocking(&r, Some("pw")).unwrap().0, "root");
        r.attribute = "API token".into();
        assert_eq!(read_blocking(&r, Some("pw")).unwrap().0, "tok-123", "a custom field");
        r.attribute = "password".into();
        assert_eq!(read_blocking(&r, Some("pw")).unwrap().0, "my-secret", "case doesn't matter");
        r.attribute = "PIN".into();
        let e = read_blocking(&r, Some("pw")).unwrap_err();
        assert!(e.contains("no tiene el campo «PIN»") && e.contains("API token") && e.contains("Password") && !e.contains("my-secret") && !e.contains("tok-123"), "{e}");
    }

    #[test]
    fn file_errors() {
        let dir = super::super::tests::temp_dir("kdbx-errors");
        let r = req(&dir.join("none.kdbx"), "x");
        assert!(read_blocking(&r, Some("pw")).unwrap_err().starts_with("no existe el archivo de KeePass"));
        let not_kdbx = dir.join("not.kdbx");
        std::fs::write(&not_kdbx, b"hello, this is not a database").unwrap();
        let e = read_blocking(&req(&not_kdbx, "x"), Some("pw")).unwrap_err();
        assert!(e.contains("no es una base de KeePass"), "{e}");
        let mut r = req(&testdata("kdbx31-password.kdbx"), "Sample Entry");
        r.key_file = Some(dir.join("none.keyx"));
        assert!(read_blocking(&r, Some("demopass")).unwrap_err().starts_with("no existe el archivo de clave"));
        r.key_file = None;
        assert_eq!(read_blocking(&r, None).unwrap_err(), "falta la contraseña maestra de la base de KeePass");
    }

    /// Browsing: every entry outside the recycle bin with its group path, a
    /// path where it names one entry and the UUID where it doesn't; then an
    /// entry's field names, never a value.
    #[test]
    fn browse_entries_and_fields() {
        let dir = super::super::tests::temp_dir("kdbx-browse");
        let file = dir.join("db.kdbx");
        save(&sample_db(OuterCipherConfig::AES256, false), &file, Some("pw"), None);
        let l = browse_blocking(&file, None, Some("pw"), None).unwrap();
        assert!(l.complete && !l.truncated && l.fields.is_none());
        let shown: Vec<(&str, &str)> = l.entries.iter().map(|e| (e.path.as_str(), e.name.as_str())).collect();
        assert_eq!(shown, [("Databases/Prod", "mysql"), ("Databases/Prod", "pg-main"), ("Databases/Staging", "pg-main")]);
        assert_eq!(l.entries[0].id, "Databases/Prod/mysql");
        // Each id reads back the same entry.
        let db = open(&file, None, Some("pw")).unwrap();
        for e in &l.entries {
            assert_eq!(find_entry(&db, &e.id).unwrap().id().uuid().hyphenated().to_string(), e.detail, "{}", e.id);
        }
        let json = serde_json::to_string(&l).unwrap();
        assert!(!json.contains("secret") && !json.contains("tok-123") && !json.contains("old-db"), "{json}");
        let f = browse_blocking(&file, None, Some("pw"), Some("Databases/Prod/mysql")).unwrap();
        assert_eq!(f.fields.unwrap(), ["Title", "UserName", "Password", "API token"]);
        assert_eq!(browse_blocking(&file, None, Some("wrong"), None).unwrap_err(), WRONG_KEY);
        // KeePassXC's file with a key file: the twin titles go by UUID only
        // where their paths are the same.
        let l = browse_blocking(&testdata("kdbx31-keyfile.kdbx"), Some(&testdata("kdbx31.keyx")), Some("dummy-master"), None).unwrap();
        assert!(l.entries.iter().any(|e| e.id == "Databases/Prod/pg-main"), "{:?}", l.entries);
    }

    #[test]
    fn kdbx31_from_other_writers() {
        let mut r = req(&testdata("kdbx31-password.kdbx"), "Sample Entry");
        r.username = true;
        let (v, u) = read_blocking(&r, Some("demopass")).unwrap();
        assert_eq!((v.as_str(), u.as_deref()), ("Password", Some("User Name")));
        assert_eq!(read_blocking(&r, Some("nope")).unwrap_err(), WRONG_KEY);
        assert_eq!(read_blocking(&req(&testdata("kdbx31-password.kdbx"), "General/Subgroup/test entry"), Some("demopass")).map(|_| ()), Ok(()));
        // KeePassXC, with a key file.
        let mut r = req(&testdata("kdbx31-keyfile.kdbx"), "Databases/Prod/pg-main");
        r.key_file = Some(testdata("kdbx31.keyx"));
        assert_eq!(read_blocking(&r, Some("dummy-master")).unwrap().0, "pg-secret-1");
        r.entry = "{53f5b102-441f-42d5-8c97-79edd12aecde}".into();
        assert_eq!(read_blocking(&r, Some("dummy-master")).unwrap().0, "pg-secret-1");
        r.entry = "pg-main".into();
        assert!(read_blocking(&r, Some("dummy-master")).unwrap_err().contains("hay 2 entradas"));
    }
}
