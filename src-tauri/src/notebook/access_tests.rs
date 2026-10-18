//! Tests for the file approval of results cells (`notebook::access`).

use super::*;
use crate::notebook::files::resolve;

fn temp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dbine-nbaccess-{tag}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn data(path: &str) -> DataFile {
    DataFile { path: path.into(), display: path.into(), size: 1, mtime_ms: 1, ..Default::default() }
}

#[test]
fn the_always_blocked_list() {
    let home = Path::new("/home/ana");
    let p = Policy::new(Some(home), &[PathBuf::from("/home/ana/.local/state/dbine")], &["/etc", "/private/etc", "/var", "/private/var"], vec![]);
    let blocked = |s: &str| p.blocked(Path::new(s));
    for (path, rule) in [
        ("/home/ana/.ssh/id_ed25519", "~/.ssh"),
        ("/home/ana/.SSH/id_rsa", "~/.ssh"),
        ("/home/ana/.aws/credentials", "~/.aws"),
        ("/home/ana/.gnupg/pubring.kbx", "~/.gnupg"),
        ("/home/ana/.config/gcloud/credentials.db", "~/.config"),
        ("/home/ana/.kube/config", "~/.kube"),
        ("/home/ana/.docker/config.json", "~/.docker"),
        ("/home/ana/.azure/msal_token_cache.json", "~/.azure"),
        ("/home/ana/Library/Keychains/login.keychain-db", "~/Library/Keychains"),
        ("/home/ana/Library/Application Support/Google/Chrome/Default/Login Data", "~/Library/Application Support/Google/Chrome"),
        ("/home/ana/Library/Application Support/Firefox/Profiles/x/logins.json", "~/Library/Application Support/Firefox"),
        ("/home/ana/AppData/Roaming/Microsoft/Credentials/ABC", "~/AppData/Roaming/Microsoft/Credentials"),
        ("/home/ana/.local/state/dbine/state.db", "/home/ana/.local/state/dbine"),
        ("/etc/passwd", "/etc"),
        ("/private/etc/master.passwd", "/private/etc"),
        ("/var/log/system.log", "/var"),
        ("/private/var/db/x.csv", "/private/var"),
        ("/home/ana/.zsh_history", "~/.zsh_history"),
        ("/home/ana/.bash_history", "~/.bash_history"),
        ("/home/ana/.psql_history", "~/.psql_history"),
        ("/home/ana/.zshrc", "~/.zshrc"),
        ("/home/ana/.bashrc", "~/.bashrc"),
        ("/home/ana/.profile", "~/.profile"),
        ("/home/ana/.netrc", ".netrc"),
        ("/home/ana/.npmrc", ".npmrc"),
        ("/home/ana/.pypirc", ".pypirc"),
        ("/home/ana/.git-credentials", ".git-credentials"),
        ("/home/ana/.pgpass", ".pgpass"),
        ("/home/ana/.my.cnf", ".my.cnf"),
        ("/home/ana/proyecto/.env", ".env"),
        ("/home/ana/proyecto/.env.local", ".env"),
        ("/srv/app/.ENV.production", ".env"),
    ] {
        // On macOS /etc and /var are links to /private/…: either name.
        let got = blocked(path);
        let alias = rule.strip_prefix("/private").is_some_and(|r| got.as_deref() == Some(r));
        assert!(got.as_deref() == Some(rule) || alias, "{path}: {got:?}");
    }
    for path in [
        "/home/ana/Downloads/clientes.csv",
        "/home/ana/proyecto/.zshrc.csv",
        "/home/ana/datos/env.csv",
        "/home/ana/datos/.environment.csv",
        "/home/ana/.sshfoo/a.csv",
        "/home/ana/Library/Application Support/Otro/a.csv",
        "/etcetera/a.csv",
        "/tmp/a.csv",
    ] {
        assert_eq!(blocked(path), None, "{path}");
    }
    // Windows' own folders, wherever %APPDATA% points.
    let w = Policy::new(None, &[], &[], vec![(PathBuf::from("/d/roaming/Microsoft/Credentials"), "%APPDATA%\\Microsoft\\Credentials".into())]);
    assert!(w.blocked(Path::new("/d/roaming/Microsoft/Credentials/x")).is_some());
}

#[cfg(unix)]
#[test]
fn symlinks_into_blocked_places_are_blocked_after_canonicalization() {
    let home = temp("home");
    write(&home.join(".ssh/id_rsa"), "-----BEGIN-----");
    write(&home.join("dotfiles/zshrc"), "export X=1");
    write(&home.join("proyecto/.env"), "TOKEN=x");
    std::os::unix::fs::symlink(home.join(".ssh/id_rsa"), home.join("Downloads.csv")).unwrap();
    std::os::unix::fs::symlink(home.join("dotfiles/zshrc"), home.join(".zshrc")).unwrap();
    std::os::unix::fs::symlink(home.join("proyecto/.env"), home.join("proyecto/datos.csv")).unwrap();
    // Not the system's folders: the temp folder is under /var on macOS.
    let p = Policy::new(Some(&home), &[], &[], vec![]);
    let canon = |raw: &str| resolve(raw, Some(&home), Some(&home)).unwrap().remove(0);
    assert_eq!(p.blocked(&canon("Downloads.csv")).as_deref(), Some("~/.ssh"));
    assert_eq!(p.blocked(&canon("proyecto/datos.csv")).as_deref(), Some(".env"));
    // ~/.zshrc linked into a dotfiles folder: its target is blocked too.
    assert_eq!(p.blocked(&canon(".zshrc")).as_deref(), Some("~/.zshrc"));
    assert_eq!(p.blocked(&canon("dotfiles/zshrc")).as_deref(), Some("~/.zshrc"));
    let a = Access { policy: p, approved: vec![canon("Downloads.csv").to_string_lossy().to_string()], ..Default::default() };
    let f = data(&canon("Downloads.csv").to_string_lossy());
    assert!(matches!(a.gate(&[f]), Err(Gate::Blocked(m)) if m.contains("id_rsa") && m.contains("~/.ssh")));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn pending_files_are_asked_about_and_grants_or_approvals_let_them_through() {
    let (a, b) = (data("/d/a.csv"), data("/d/b.csv"));
    let mut acc = Access { notebook: Some("/nb/x.ipynb".into()), ..Default::default() };
    assert_eq!(acc.gate(&[a.clone(), b.clone()]), Err(Gate::Ask(vec![a.clone(), b.clone()])));
    acc.grants = vec![FileGrant { notebook: "/nb/x.ipynb".into(), target: "/d/a.csv".into(), pattern: false, granted_at: String::new() }];
    assert_eq!(acc.gate(&[a.clone(), b.clone()]), Err(Gate::Ask(vec![b.clone()])));
    acc.approved = vec!["/d/b.csv".into()];
    assert_eq!(acc.gate(&[a.clone(), b.clone()]), Ok(vec![a.clone(), b.clone()]));
    // A pattern grant: files of that pattern, and only them.
    let acc = Access { grants: vec![FileGrant { notebook: "/nb/x.ipynb".into(), target: "/d/v_*.csv".into(), pattern: true, granted_at: String::new() }], ..Default::default() };
    assert!(acc.gate(&[data("/d/v_9.csv")]).is_ok());
    assert!(acc.gate(&[data("/d/sub/v_9.csv")]).is_err());
    assert!(acc.gate(&[data("/d/w_9.csv")]).is_err());
}

#[test]
fn once_always_and_deny() {
    let store = StateStore::open_in_memory().unwrap();
    let nb = Some("/nb/x.ipynb");
    let mut glob = data("/d/exports/v_1.csv");
    glob.pattern = Some("/d/exports/v_*.csv".into());
    let files = vec![data("/d/a.csv"), glob];

    // Once: this run only, and the token is spent; nothing remembered.
    let r = ask("tab1", nb, files.clone()).unwrap();
    assert!(r.saved);
    assert!(take(&r.token, "tab1", nb).is_err(), "not answered yet");
    decide(&store, &r.token, Decision::Once).unwrap();
    assert!(take(&r.token, "tab2", nb).is_err(), "another tab");
    assert!(take(&r.token, "tab1", Some("/nb/y.ipynb")).is_err(), "another notebook");
    assert_eq!(take(&r.token, "tab1", nb).unwrap(), vec!["/d/a.csv".to_string(), "/d/exports/v_1.csv".to_string()]);
    assert!(take(&r.token, "tab1", nb).is_err(), "spent");
    assert!(store.list_file_grants(None).unwrap().is_empty());

    // Always: remembered for this notebook, a glob's file as its pattern.
    let r = ask("tab1", nb, files.clone()).unwrap();
    decide(&store, &r.token, Decision::Always).unwrap();
    let g = store.list_file_grants(Some("/nb/x.ipynb")).unwrap();
    assert_eq!(g.iter().map(|g| (g.target.as_str(), g.pattern)).collect::<Vec<_>>(), vec![("/d/a.csv", false), ("/d/exports/v_*.csv", true)]);
    assert!(take(&r.token, "tab1", nb).is_ok());
    // Another notebook has none.
    assert!(store.list_file_grants(Some("/nb/y.ipynb")).unwrap().is_empty());

    // Deny: the request is gone.
    let r = ask("tab1", nb, files.clone()).unwrap();
    decide(&store, &r.token, Decision::Deny).unwrap();
    assert!(take(&r.token, "tab1", nb).is_err());
    assert!(decide(&store, &r.token, Decision::Once).is_err());

    // An unsaved notebook: "always" can't be remembered; once works.
    let r = ask("tab3", None, files.clone()).unwrap();
    assert!(!r.saved);
    assert!(matches!(decide(&store, &r.token, Decision::Always), Err(CommandError::BadRequest(m)) if m.contains("guardá el notebook")));
    decide(&store, &r.token, Decision::Once).unwrap();
    assert_eq!(take(&r.token, "tab3", None).unwrap().len(), 2);
    assert_eq!(store.list_file_grants(None).unwrap().len(), 2);

    // A token nobody issued.
    assert!(decide(&store, "made-up", Decision::Always).is_err());
    assert_eq!(store.list_file_grants(None).unwrap().len(), 2);

    // A new request for the tab replaces its earlier one.
    let r1 = ask("tab4", nb, files.clone()).unwrap();
    let r2 = ask("tab4", nb, files).unwrap();
    assert!(decide(&store, &r1.token, Decision::Once).is_err());
    assert!(decide(&store, &r2.token, Decision::Once).is_ok());
}

#[test]
fn a_glob_file_outside_its_pattern_is_granted_as_itself() {
    let mut f = data("/elsewhere/real.csv");
    f.pattern = Some("/d/exports/*.csv".into());
    let mut g = data("/d/exports/b.csv");
    g.pattern = Some("/d/exports/*.csv".into());
    let mut h = data("/d/exports/c.csv");
    h.pattern = Some("/d/exports/*.csv".into());
    assert_eq!(grant_targets(&[f, g, h]), vec![("/elsewhere/real.csv".to_string(), false), ("/d/exports/*.csv".to_string(), true)]);
}

#[tokio::test]
async fn the_hash_is_computed_only_when_size_or_time_changed() {
    let dir = temp("hash");
    let path = dir.join("a.csv");
    write(&path, "n\n1\n");
    let store = StateStore::open_in_memory().unwrap();
    let acc = Access { notebook: Some("nb".into()), store: Some(&store), ..Default::default() };
    let file = |p: &Path| {
        let m = std::fs::metadata(p).unwrap();
        let mtime_ms = m.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        DataFile { path: p.to_string_lossy().to_string(), display: String::new(), size: m.len(), mtime_ms, ..Default::default() }
    };
    let key = path.to_string_lossy().to_string();

    // First read: hashed, no notice.
    let f1 = file(&path);
    let seen = changes(&acc, std::slice::from_ref(&f1)).await;
    assert!(seen[0].dirty && seen[0].previous_mtime_ms.is_none());
    assert_eq!(seen[0].record.sha256, hash_file(&path).unwrap());
    remember(&acc, &seen);

    // Same size and time: not hashed (a stored hash that isn't the file's stays).
    store.put_file_record("nb", &key, &FileRecord { size: f1.size, mtime_ms: f1.mtime_ms, sha256: "not-hashed".into() }).unwrap();
    let seen = changes(&acc, std::slice::from_ref(&f1)).await;
    assert_eq!(seen[0].record.sha256, "not-hashed");
    assert!(!seen[0].dirty && seen[0].previous_mtime_ms.is_none());
    store.put_file_record("nb", &key, &FileRecord { size: f1.size, mtime_ms: f1.mtime_ms, sha256: hash_file(&path).unwrap() }).unwrap();

    // Touched, same content: hashed again, no notice.
    let mut f2 = f1.clone();
    f2.mtime_ms += 5_000;
    let seen = changes(&acc, std::slice::from_ref(&f2)).await;
    assert!(seen[0].dirty && seen[0].previous_mtime_ms.is_none());
    remember(&acc, &seen);

    // Same size, new content and time: the notice, with the time it had.
    write(&path, "n\n2\n");
    let mut f3 = file(&path);
    f3.mtime_ms = f2.mtime_ms + 5_000;
    let seen = changes(&acc, std::slice::from_ref(&f3)).await;
    assert_eq!(seen[0].previous_mtime_ms, Some(f2.mtime_ms));
    remember(&acc, &seen);
    assert_eq!(store.file_record("nb", &key).unwrap().unwrap().mtime_ms, f3.mtime_ms);

    // An unsaved notebook keeps no records.
    let unsaved = Access { store: Some(&store), ..Default::default() };
    assert!(changes(&unsaved, &[f3]).await.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
