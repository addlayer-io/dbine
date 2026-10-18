//! Local data files in "Resultados del notebook" cells (docs/notebooks.md ›
//! Reading local files): `read_csv('~/Downloads/clientes.csv')` and the
//! other DuckDB readers ([`READERS`]), joined with the cells' tables.
//!
//! - **Detection** ([`reader_calls`]): a call to a reader outside strings
//!   and comments whose first argument is a string literal or a list of
//!   them (`'a.csv'`, `['a.csv', 'b.csv']`, globs included). Any other
//!   first argument (a column, an expression) is refused.
//! - **Resolution** ([`resolve`]): `~` is the home folder, a relative path
//!   is relative to the notebook's folder (refused for an unsaved one),
//!   globs are expanded here, and every file is canonicalized (symlinks
//!   resolved, no `..`). URLs are refused: no network.
//! - **What may be read** is decided in one place, [`allowed_files`]
//!   (today: every file detected; the per-file approval plugs in there).
//! - **Reading** happens in a DuckDB session of its own ([`open_reader`]):
//!   in memory, granted exactly those files through `allowed_paths`, then
//!   locked down like the notebook's ([`super::local::SANDBOX`]). It runs
//!   only `SELECT * FROM <the reader call>`, rebuilt from its tokens with the
//!   resolved paths, never the cell's SQL. Its rows are loaded into the
//!   notebook's session as a table and the call in the cell's SQL is
//!   replaced by that table ([`rewrite`]). The notebook's session itself
//!   is granted no file at all, so DuckDB refuses every file access from
//!   the cell's SQL (DuckDB's `allowed_paths` would also allow writing
//!   them: a `COPY … (USE_TMP_FILE false)` overwrites an allowed file).
//! - **Statements that write or attach anything** ([`refused`]: `COPY`,
//!   `EXPORT`, `ATTACH`, `INSTALL`, `SET`…) are refused before anything
//!   runs, on top of DuckDB's lockdown.

use crate::commands::projects::sha256_hex;
use crate::error::{CommandError, CommandResult};
use dbine_driver::sql::{name_tokens, split_script, NameToken, ScriptDialect, TokenKind};
use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde::Serialize;
use std::path::{Component, Path, PathBuf};

/// DuckDB's table functions that read data files, as the cells may use them.
pub const READERS: &[&str] = &[
    "read_csv",
    "read_csv_auto",
    "read_parquet",
    "parquet_scan",
    "read_json",
    "read_json_auto",
    "read_ndjson",
    "read_ndjson_auto",
    "read_xlsx",
];
/// Files a pattern (or a run) may read at most.
const MAX_FILES: usize = 1_000;
/// Directory entries a glob may look at.
const MAX_SCANNED: usize = 200_000;
/// Folders deep a `**` goes.
const MAX_DEPTH: usize = 32;

fn dialect() -> ScriptDialect {
    ScriptDialect::postgres()
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// ---------------------------------------------------------------------------
// Statements refused in these cells

/// Why `sql` can't run in a results cell: a statement that writes or
/// attaches anything, installs or loads an extension, or touches the
/// configuration (`SET VARIABLE` is fine). `None`: it may run.
pub fn refused(sql: &str) -> Option<String> {
    let d = dialect();
    for st in split_script(sql, &d) {
        let toks = name_tokens(&st.text, &d);
        // A word as written (a quoted identifier is never a keyword).
        let word = |i: usize| -> Option<String> {
            let t = toks.get(i)?;
            (t.kind == TokenKind::Name && !st.text[t.start..].starts_with('"')).then(|| t.text.to_ascii_lowercase())
        };
        let mut i = 0;
        // EXPLAIN [ANALYZE] [(…)] runs what follows.
        while let Some(w) = word(i).filter(|w| matches!(w.as_str(), "explain" | "analyze" | "analyse" | "verbose")) {
            i += 1;
            if w == "explain" && toks.get(i).is_some_and(|t| t.kind == TokenKind::Punct && t.text == "(") {
                let mut depth = 0;
                while let Some(t) = toks.get(i) {
                    i += 1;
                    match (t.kind, t.text) {
                        (TokenKind::Punct, "(") => depth += 1,
                        (TokenKind::Punct, ")") => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // `CREATE [OR REPLACE] [PERSISTENT | TEMPORARY] SECRET`, `DROP [PERSISTENT] SECRET`.
        let secret = |mut j: usize| {
            while word(j).is_some_and(|w| matches!(w.as_str(), "or" | "replace" | "persistent" | "temporary" | "temp")) {
                j += 1;
            }
            word(j).as_deref() == Some("secret")
        };
        let Some(first) = word(i) else { continue };
        let next = word(i + 1).unwrap_or_default();
        let what = match first.as_str() {
            "copy" | "export" | "import" | "attach" | "detach" | "install" | "load" | "force" | "pragma" | "checkpoint" => first.to_uppercase(),
            "set" | "reset" if next != "variable" => first.to_uppercase(),
            "update" if next == "extensions" => "UPDATE EXTENSIONS".into(),
            "create" | "drop" if secret(i + 1) => format!("{} SECRET", first.to_uppercase()),
            _ => continue,
        };
        return Some(format!(
            "«{what}» no se puede usar en una celda de Resultados del notebook: su base solo lee archivos y nunca escribe, adjunta ni configura nada"
        ));
    }
    None
}

// ---------------------------------------------------------------------------
// Detection

/// A reader call as written in a cell's SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderCall {
    /// The reader, lowercase (`read_csv`).
    pub function: String,
    /// `sql[start..end]` is the whole call, its name to its `)`.
    pub start: usize,
    pub end: usize,
    /// The paths as written (quotes off, `''` unescaped).
    pub paths: Vec<String>,
    /// The first argument is a list (`['a.csv', 'b.csv']`).
    pub list: bool,
    /// The rest of the arguments (`, header = true`), rebuilt from their
    /// tokens: no comments, nothing past the call's `)`.
    pub rest: String,
}

/// A plain string literal's value (`'it''s'` → `it's`); `None` for any
/// other string (`E'…'`, `$$…$$`, unterminated).
fn plain_string(sql: &str, t: &NameToken<'_>) -> Option<String> {
    let raw = &sql[t.start..t.end];
    let prefixed = t.start > 0 && sql.as_bytes()[t.start - 1].is_ascii_alphanumeric();
    let inner = raw.strip_prefix('\'')?.strip_suffix('\'')?;
    (!prefixed && raw.len() >= 2).then(|| inner.replace("''", "'"))
}

/// The reader calls of `sql`, in order. Refused: a first argument that
/// isn't a plain string or a list of them, a `;` inside the call, a call
/// left open.
pub fn reader_calls(sql: &str) -> Result<Vec<ReaderCall>, String> {
    let toks = name_tokens(sql, &dialect());
    let punct = |i: usize, p: &str| toks.get(i).is_some_and(|t| t.kind == TokenKind::Punct && t.text == p);
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        let function = t.text.to_ascii_lowercase();
        let qualified = i > 0 && punct(i - 1, ".");
        if t.kind != TokenKind::Name || qualified || !READERS.contains(&function.as_str()) || !punct(i + 1, "(") {
            i += 1;
            continue;
        }
        let not_literal = || format!("«{function}» lee archivos solo con la ruta escrita entre comillas simples: '…' o una lista ['…', '…']");
        // The first argument: a string or a list of strings.
        let mut j = i + 2;
        let mut paths = Vec::new();
        let list = punct(j, "[");
        if list {
            j += 1;
            loop {
                let s = toks.get(j).filter(|t| t.kind == TokenKind::String).and_then(|t| plain_string(sql, t)).ok_or_else(not_literal)?;
                paths.push(s);
                j += 1;
                if punct(j, ",") {
                    j += 1;
                } else if punct(j, "]") {
                    j += 1;
                    break;
                } else {
                    return Err(not_literal());
                }
            }
        } else {
            let s = toks.get(j).filter(|t| t.kind == TokenKind::String).and_then(|t| plain_string(sql, t)).ok_or_else(not_literal)?;
            paths.push(s);
            j += 1;
        }
        if !(punct(j, ",") || punct(j, ")")) {
            return Err(not_literal());
        }
        // The rest, to the matching `)`.
        let rest_from = j;
        let mut depth = 1;
        while depth > 0 {
            let Some(t) = toks.get(j) else {
                return Err(format!("falta cerrar el paréntesis de «{function}»"));
            };
            match (t.kind, t.text) {
                (TokenKind::Punct, "(") => depth += 1,
                (TokenKind::Punct, ")") => depth -= 1,
                (TokenKind::Punct, ";") => return Err(format!("falta cerrar el paréntesis de «{function}»")),
                (TokenKind::String, _) if plain_string(sql, t).is_none() => return Err(format!("«{function}»: escribí sus textos entre comillas simples ('…')")),
                _ => {}
            }
            j += 1;
        }
        let close = j - 1;
        // Tokens as written; a gap between two is kept as one space or none
        // (`:=` stays together), so comments are left out.
        let mut rest = String::new();
        for (k, r) in toks[rest_from..close].iter().enumerate() {
            if k > 0 && toks[rest_from + k - 1].end < r.start {
                rest.push(' ');
            }
            rest.push_str(&sql[r.start..r.end]);
        }
        out.push(ReaderCall { function, start: t.start, end: toks[close].end, paths, list, rest });
        i = j;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Resolution

/// One file a reader reads, as it is now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DataFile {
    /// Canonical: absolute, symlinks resolved, no `..`.
    pub path: String,
    /// Shortened for the cell: relative to the notebook's folder when it's
    /// inside it, else with `~` for the home folder.
    pub display: String,
    pub size: u64,
    /// Last modified (ms since the epoch).
    pub mtime_ms: i64,
}

/// A URL (`https://…`, `s3://…`, `hf://…`): not a local file.
fn is_url(p: &str) -> bool {
    p.split_once("://").is_some_and(|(scheme, _)| scheme.len() > 1 && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)))
}

fn has_glob(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// Without Windows' `\\?\` prefix, which DuckDB doesn't take.
fn plain(p: PathBuf) -> PathBuf {
    match p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p,
    }
}

/// `name` matches the glob segment `pat` (`*`, `?`, `[a-z]`, `[!x]`).
fn wild(pat: &[char], name: &[char]) -> bool {
    match pat.first() {
        None => name.is_empty(),
        Some('*') => (0..=name.len()).any(|k| wild(&pat[1..], &name[k..])),
        Some('?') => !name.is_empty() && wild(&pat[1..], &name[1..]),
        Some('[') => {
            let Some(close) = pat.iter().skip(2).position(|c| *c == ']').map(|p| p + 2) else {
                return name.first() == Some(&'[') && wild(&pat[1..], &name[1..]);
            };
            let Some(c) = name.first() else { return false };
            let set = &pat[1..close];
            let (neg, set) = match set.first() {
                Some('!' | '^') => (true, &set[1..]),
                _ => (false, set),
            };
            let mut hit = false;
            let mut k = 0;
            while k < set.len() {
                if k + 2 < set.len() && set[k + 1] == '-' {
                    hit |= (set[k]..=set[k + 2]).contains(c);
                    k += 3;
                } else {
                    hit |= set[k] == *c;
                    k += 1;
                }
            }
            hit != neg && wild(&pat[close + 1..], &name[1..])
        }
        Some(p) => name.first() == Some(p) && wild(&pat[1..], &name[1..]),
    }
}

/// The files `pattern` matches (`**` is any number of folders).
fn expand(pattern: &Path) -> Result<Vec<PathBuf>, String> {
    let mut root = PathBuf::new();
    let mut rest: Vec<String> = Vec::new();
    for c in pattern.components() {
        let s = c.as_os_str().to_string_lossy().to_string();
        if rest.is_empty() && !(matches!(c, Component::Normal(_)) && has_glob(&s)) {
            root.push(c.as_os_str());
        } else {
            rest.push(s);
        }
    }
    struct Walk {
        out: Vec<PathBuf>,
        scanned: usize,
    }
    fn entries(dir: &Path, w: &mut Walk) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut v: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        w.scanned += v.len();
        v.sort();
        v
    }
    fn walk(dir: &Path, rest: &[String], depth: usize, w: &mut Walk) {
        if w.out.len() > MAX_FILES || w.scanned > MAX_SCANNED || depth > MAX_DEPTH {
            return;
        }
        let Some(seg) = rest.first() else {
            if dir.is_file() {
                w.out.push(dir.to_path_buf());
            }
            return;
        };
        if seg == "**" {
            if rest.len() == 1 {
                // `dir/**`: every file below.
                for e in entries(dir, w) {
                    if e.is_dir() { walk(&e, rest, depth + 1, w) } else if e.is_file() { w.out.push(e) }
                }
                return;
            }
            walk(dir, &rest[1..], depth, w);
            for e in entries(dir, w).into_iter().filter(|e| e.is_dir()) {
                walk(&e, rest, depth + 1, w);
            }
        } else if !has_glob(seg) {
            walk(&dir.join(seg), &rest[1..], depth, w);
        } else {
            let pat: Vec<char> = seg.chars().collect();
            for e in entries(dir, w) {
                let name: Vec<char> = e.file_name().map(|n| n.to_string_lossy().chars().collect()).unwrap_or_default();
                if wild(&pat, &name) {
                    walk(&e, &rest[1..], depth + 1, w);
                }
            }
        }
    }
    let mut w = Walk { out: Vec::new(), scanned: 0 };
    walk(&root, &rest, 0, &mut w);
    w.out.sort();
    if w.out.len() > MAX_FILES || w.scanned > MAX_SCANNED {
        return Err(format!("«{}» abarca demasiados archivos (hasta {MAX_FILES})", pattern.display()));
    }
    Ok(w.out)
}

/// The files a path as written stands for: `~` expanded, relative to
/// `base` (the notebook's folder), globs expanded, each one canonical.
pub fn resolve(raw: &str, base: Option<&Path>, home: Option<&Path>) -> Result<Vec<PathBuf>, String> {
    let p = raw.trim();
    if p.is_empty() {
        return Err("la ruta de un archivo está vacía".into());
    }
    if is_url(p) {
        return Err(format!("«{p}» no es un archivo local: estas celdas no leen de la red (http, s3, gs, azure…)"));
    }
    let tilde = p == "~" || p.starts_with("~/") || p.starts_with("~\\");
    let path = if tilde {
        let home = home.ok_or_else(|| format!("no se encontró la carpeta personal para «{p}»"))?;
        home.join(p[1..].trim_start_matches(['/', '\\']))
    } else {
        PathBuf::from(p)
    };
    let path = if path.is_absolute() {
        path
    } else {
        match base {
            Some(b) => b.join(path),
            None => return Err(format!("«{p}» es una ruta relativa: guardá el notebook para leerla desde su carpeta, o escribí la ruta completa")),
        }
    };
    let shown = path.display().to_string();
    let found = if has_glob(&shown) {
        let found = expand(&path)?;
        if found.is_empty() {
            return Err(format!("ningún archivo coincide con «{shown}»"));
        }
        found
    } else {
        vec![path]
    };
    let mut out = Vec::new();
    for f in found {
        let canon = std::fs::canonicalize(&f).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("no existe el archivo «{}»", f.display()),
            _ => format!("no se puede leer «{}»: {e}", f.display()),
        })?;
        if canon.is_dir() {
            return Err(format!("«{}» es una carpeta: escribí un archivo o un patrón como «carpeta/*.csv»", f.display()));
        }
        let canon = plain(canon);
        if !out.contains(&canon) {
            out.push(canon);
        }
    }
    Ok(out)
}

/// `path` shortened for the cell (see [`DataFile::display`]).
fn display(path: &Path, base: Option<&Path>, home: Option<&Path>) -> String {
    let canon = |p: &Path| std::fs::canonicalize(p).map(plain).unwrap_or_else(|_| p.to_path_buf());
    if let Some(rel) = base.and_then(|b| path.strip_prefix(canon(b)).ok()) {
        return rel.display().to_string();
    }
    if let Some(rel) = home.and_then(|h| path.strip_prefix(canon(h)).ok()) {
        return Path::new("~").join(rel).display().to_string();
    }
    path.display().to_string()
}

fn data_file(path: &Path, base: Option<&Path>, home: Option<&Path>) -> Result<DataFile, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("no se puede leer «{}»: {e}", path.display()))?;
    let mtime_ms = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64);
    Ok(DataFile { path: path.to_string_lossy().to_string(), display: display(path, base, home), size: meta.len(), mtime_ms })
}

/// The home folder.
pub fn home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}

// ---------------------------------------------------------------------------
// Planning

/// A reader call, resolved: what the reader session runs and the table
/// the notebook's session gets.
#[derive(Debug, Clone)]
pub struct FileRead {
    pub call: ReaderCall,
    /// The call rebuilt with the resolved paths: `read_csv('/…/a.csv', header = true)`.
    pub text: String,
    pub files: Vec<DataFile>,
    /// Its table in the notebook's session: `file:<hash>`, a name no cell
    /// can have.
    pub table: String,
    /// What identifies its rows: the call, its files as they are now
    /// (size and modification time), the cap. Loaded again when it changes.
    pub key: String,
    /// For the cell: the file, or the paths as written for several.
    pub display: String,
}

/// The reader calls of `sql`, resolved against `base` (the notebook's
/// folder; `None` for an unsaved notebook).
pub fn plan(sql: &str, base: Option<&Path>, home: Option<&Path>, cap: u64) -> Result<Vec<FileRead>, String> {
    let mut out = Vec::new();
    for call in reader_calls(sql)? {
        let mut paths = Vec::new();
        for raw in &call.paths {
            for p in resolve(raw, base, home)? {
                if !paths.contains(&p) {
                    paths.push(p);
                }
            }
        }
        if paths.len() > MAX_FILES {
            return Err(format!("«{}» lee más de {MAX_FILES} archivos", call.function));
        }
        if call.function == "read_xlsx" && paths.len() != 1 {
            return Err(format!("«read_xlsx» lee un solo archivo y «{}» son {}", call.paths.join(", "), paths.len()));
        }
        let files = paths.iter().map(|p| data_file(p, base, home)).collect::<Result<Vec<_>, _>>()?;
        let single = !call.list && paths.len() == 1 && !call.paths.iter().any(|p| has_glob(p));
        let arg = if single { lit(&files[0].path) } else { format!("[{}]", files.iter().map(|f| lit(&f.path)).collect::<Vec<_>>().join(", ")) };
        let text = format!("{}({arg}{})", call.function, call.rest);
        let table = format!("file:{}", &sha256_hex(text.as_bytes())[..16]);
        let metas: Vec<(&str, u64, i64)> = files.iter().map(|f| (f.path.as_str(), f.size, f.mtime_ms)).collect();
        let key = sha256_hex(serde_json::json!([text, metas, cap]).to_string().as_bytes());
        let display = if files.len() == 1 { files[0].display.clone() } else { call.paths.join(", ") };
        out.push(FileRead { call, text, files, table, key, display });
    }
    Ok(out)
}

/// `sql` with each reader call replaced by its table.
pub fn rewrite(sql: &str, reads: &[FileRead]) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut at = 0;
    let mut calls: Vec<&FileRead> = reads.iter().collect();
    calls.sort_by_key(|r| r.call.start);
    for r in calls {
        if r.call.start < at {
            continue;
        }
        out.push_str(&sql[at..r.call.start]);
        out.push_str(&dbine_driver::sql::quote_ident(dbine_driver::sql::Quote::Double, &r.table));
        at = r.call.end;
    }
    out.push_str(&sql[at..]);
    out
}

// ---------------------------------------------------------------------------
// What may be read

/// The one place that decides which of the files a run detected it may
/// read; the reader session is granted exactly these, and a run that
/// needs any other is refused. Today: every file detected (they exist and
/// were resolved above).
///
/// TODO(file-approval): ask before reading a file the user hasn't approved
/// for this notebook (path + content hash, remembered per notebook), and
/// return only the approved ones.
pub fn allowed_files(files: &[DataFile]) -> Vec<DataFile> {
    files.to_vec()
}

// ---------------------------------------------------------------------------
// The reader session

/// A DuckDB session of its own, in memory, that may read `files` and
/// nothing else: `allowed_paths` first (DuckDB only takes it while
/// external access is on), the `excel` extension loaded when a workbook
/// is read, then the notebook's lockdown ([`super::local::sandbox`]).
pub async fn open_reader(files: &[DataFile], excel: bool) -> CommandResult<Box<dyn Session>> {
    let mut cfg = ConnectionConfig { driver: "duckdb".into(), host: ":memory:".into(), ..Default::default() };
    cfg.options.insert("isolated".into(), "true".into());
    let mut s = dbine_drivers::open_session(&cfg, None).await?;
    let fail = |e: String| CommandError::Internal(format!("no se pudo preparar la lectura de archivos: {e}"));
    let grant = format!("SET allowed_paths = [{}]", files.iter().map(|f| lit(&f.path)).collect::<Vec<_>>().join(", "));
    let mut out = QueryOutcome::default();
    s.execute(&grant, 1, &mut out).await.map_err(|e| fail(e.to_string()))?;
    if let Some(e) = out.error {
        return Err(fail(e));
    }
    if excel {
        // Fetched once into DuckDB's extensions folder (as the data-file
        // connections do for .xlsx), loaded before the lockdown.
        let mut out = QueryOutcome::default();
        let loaded = s.execute("LOAD excel", 1, &mut out).await.is_ok() && out.error.is_none();
        if !loaded {
            let mut out = QueryOutcome::default();
            let r = s.execute("INSTALL excel; LOAD excel", 1, &mut out).await;
            if let Some(e) = r.err().map(|e| e.to_string()).or(out.error) {
                return Err(CommandError::Internal(format!("no se pudo cargar la extensión excel de DuckDB para leer el libro: {e}")));
            }
        }
    }
    super::local::sandbox(s.as_mut()).await?;
    Ok(s)
}

#[cfg(test)]
#[path = "files_tests.rs"]
mod tests;
