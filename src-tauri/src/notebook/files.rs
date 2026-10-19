//! Local data files in "DataFrame SQL" cells (docs/notebooks.md ›
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
//! - **What may be read** is decided in one place, [`allowed_files`]:
//!   never a file of the always-blocked list, and otherwise only a file
//!   the user allowed for this notebook (a grant) or for this run
//!   ([`super::access`]); any other stops the run and the UI asks.
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
//! - **Geographic files** (`st_read`, DuckDB's `spatial` extension, GDAL):
//!   one local file of the formats in [`GEO_FORMATS`], read only through
//!   the GDAL drivers of [`GEO_DRIVERS`] (`allowed_drivers`, always set by
//!   DBine: a VRT renamed `.geojson` is not opened as a VRT, since a VRT
//!   points GDAL at other files outside DuckDB's file checks). Its sidecars
//!   (`.shx`, `.dbf`, `.prj`… next to a `.shp`, [`sidecars`]) are granted
//!   with it and checked like it. GDAL virtual paths (`/vsi…`), URLs and the
//!   parameters that could reach other files are refused ([`geo_args`]).
//!   Its geometry column comes into the notebook's session as `GEOMETRY`
//!   (WKB in between, [`super::local`]).
//! - **Database files**: `sqlite_scan('x.db', 'tabla')` is read by DBine
//!   itself through SQLite (read-only and `immutable`, so nothing next to
//!   the file is created or changed; a pending `-wal` or `-journal` is a
//!   sidecar and the file is read from a private copy, [`read_sqlite`]),
//!   never with DuckDB's sqlite extension, whose SQLite opens files outside
//!   DuckDB's `allowed_paths`. `read_duckdb('x.duckdb', 'tabla')` is
//!   DuckDB's own `read_duckdb` (it attaches the file read-only), in a
//!   reader session granted that file. Table names are checked
//!   ([`table_name`]) and passed as strings, never written into SQL.
//! - **One reader session per call**: each reader call is read in a session
//!   granted only its own files, with only the extension it needs.

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
    "st_read",
    "sqlite_scan",
    "read_duckdb",
];

/// What `st_read` reads, by the file's extension (lowercase, `shp.zip` for
/// a zipped shapefile): GeoJSON, GeoJSON sequences, TopoJSON, Shapefile
/// (also a lone `.dbf`, and zipped, which GDAL opens by itself),
/// GeoPackage, KML, GPX, FlatGeobuf, CSV (WKT or coordinate columns) and DXF.
pub const GEO_FORMATS: &[&str] = &["geojson", "json", "geojsonl", "geojsons", "topojson", "shp", "shp.zip", "shz", "dbf", "gpkg", "kml", "gpx", "fgb", "csv", "dxf"];

/// The GDAL drivers `st_read` may use (its `allowed_drivers`): formats that
/// read the file and its sidecars only. Left out on purpose: `OGR_VRT` (an
/// XML file pointing at other files), `GML` (schemas and xlinks it may
/// fetch), `SQLite` (SQLite virtual tables over other data sources),
/// `MapInfo File` (seamless tables over other files), `OSM` (temporary
/// files), and every network driver (WFS, OAPIF, Elasticsearch, Carto…).
pub const GEO_DRIVERS: &[&str] = &["GeoJSON", "GeoJSONSeq", "ESRIJSON", "TopoJSON", "ESRI Shapefile", "GPKG", "KML", "GPX", "FlatGeobuf", "CSV", "DXF"];

/// Extensions `st_read` refuses with their own message: formats that point
/// GDAL at other files or the network.
const GEO_INDIRECT: &[&str] = &["vrt", "xml", "gml", "xsd", "gfs", "tab", "map", "osm", "pbf", "ovr", "wfs", "mbtiles", "pmtiles"];

/// `st_read`'s named parameters a cell may set. `allowed_drivers` and
/// `sibling_files` are DBine's.
pub const GEO_PARAMS: &[&str] = &["layer", "keep_wkb", "max_batch_size", "open_options"];

/// GDAL open options `st_read` may pass (`open_options = ['KEY=VALUE']`):
/// how a CSV's coordinates, a shapefile's encoding or nested GeoJSON are
/// read. None of them names a file or a URL, and their values can't either.
pub const GEO_OPEN_OPTIONS: &[&str] = &[
    "X_POSSIBLE_NAMES",
    "Y_POSSIBLE_NAMES",
    "Z_POSSIBLE_NAMES",
    "GEOM_POSSIBLE_NAMES",
    "KEEP_GEOM_COLUMNS",
    "SEPARATOR",
    "HEADERS",
    "AUTODETECT_TYPE",
    "AUTODETECT_WIDTH",
    "AUTODETECT_SIZE_LIMIT",
    "QUOTED_FIELDS_AS_STRING",
    "EMPTY_STRING_AS_NULL",
    "MAX_LINE_SIZE",
    "ENCODING",
    "ADJUST_TYPE",
    "ADJUST_GEOM_TYPE",
    "FLATTEN_NESTED_ATTRIBUTES",
    "NESTED_ATTRIBUTE_SEPARATOR",
    "ARRAY_AS_STRING",
    "DATE_AS_STRING",
    "LIST_ALL_TABLES",
];

/// A shapefile's sidecars: same stem, these extensions.
const SHAPE_SIDECARS: &[&str] = &["shx", "dbf", "prj", "cpg", "sbn", "sbx", "qix", "fix", "qpj", "shp.xml"];
/// A CSV's (GDAL reads its column types and projection from them).
const CSV_SIDECARS: &[&str] = &["csvt", "prj"];
/// Files a pattern (or a run) may read at most.
pub const MAX_FILES: usize = 1_000;
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
            "«{what}» no se puede usar en una celda DataFrame SQL: su base solo lee archivos y nunca escribe, adjunta ni configura nada"
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
    /// The same arguments one by one.
    pub args: Vec<Arg>,
}

/// One argument after a reader's path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arg {
    /// `name = …` / `name := …`, lowercase; `None` for a positional one.
    pub name: Option<String>,
    /// Its value when it is a literal.
    pub value: Option<Lit>,
}

/// A literal argument value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lit {
    /// `'text'`.
    Str(String),
    /// `['a', 'b']`.
    List(Vec<String>),
    /// A number, `true` or `false`, as written.
    Word(String),
}

impl Lit {
    /// As SQL again.
    fn sql(&self) -> String {
        match self {
            Lit::Str(s) => lit(s),
            Lit::List(v) => format!("[{}]", v.iter().map(|s| lit(s)).collect::<Vec<_>>().join(", ")),
            Lit::Word(w) => w.clone(),
        }
    }
}

/// The arguments in `toks` (from the `,` after the path to before the
/// call's `)`), split at the top-level commas.
fn split_args(sql: &str, toks: &[NameToken<'_>]) -> Vec<Arg> {
    let mut groups: Vec<Vec<&NameToken<'_>>> = Vec::new();
    let mut depth = 0i32;
    for t in toks {
        match (t.kind, t.text) {
            (TokenKind::Punct, "(" | "[" | "{") => depth += 1,
            (TokenKind::Punct, ")" | "]" | "}") => depth -= 1,
            (TokenKind::Punct, ",") if depth == 0 => {
                groups.push(Vec::new());
                continue;
            }
            _ => {}
        }
        if let Some(g) = groups.last_mut() {
            g.push(t);
        }
    }
    let is = |t: Option<&&NameToken<'_>>, p: &str| t.is_some_and(|t| t.kind == TokenKind::Punct && t.text == p);
    groups
        .into_iter()
        .map(|g| {
            let named = g.first().is_some_and(|t| t.kind == TokenKind::Name && !sql[t.start..].starts_with('"'));
            let (name, value) = if named && is(g.get(1), "=") {
                (Some(g[0].text.to_ascii_lowercase()), &g[2..])
            } else if named && is(g.get(1), ":") && is(g.get(2), "=") {
                (Some(g[0].text.to_ascii_lowercase()), &g[3..])
            } else {
                (None, &g[..])
            };
            let value = match value {
                [t] if t.kind == TokenKind::String => plain_string(sql, t).map(Lit::Str),
                [t] if t.kind == TokenKind::Name && !sql[t.start..].starts_with('"') => {
                    let w = t.text;
                    (w.eq_ignore_ascii_case("true") || w.eq_ignore_ascii_case("false") || w.bytes().all(|b| b.is_ascii_digit())).then(|| Lit::Word(w.to_ascii_lowercase()))
                }
                [open, inner @ .., close] if open.kind == TokenKind::Punct && open.text == "[" && close.kind == TokenKind::Punct && close.text == "]" => {
                    let mut items = Vec::new();
                    let mut ok = true;
                    for (k, t) in inner.iter().enumerate() {
                        if k % 2 == 0 {
                            match (t.kind == TokenKind::String).then(|| plain_string(sql, t)).flatten() {
                                Some(s) => items.push(s),
                                None => ok = false,
                            }
                        } else if !(t.kind == TokenKind::Punct && t.text == ",") {
                            ok = false;
                        }
                    }
                    (ok && inner.len() % 2 == 1 || inner.is_empty()).then_some(Lit::List(items))
                }
                _ => None,
            };
            Arg { name, value }
        })
        .collect()
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
        let args = split_args(sql, &toks[rest_from..close]);
        out.push(ReaderCall { function, start: t.start, end: toks[close].end, paths, list, rest, args });
        i = j;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Resolution

/// One file a reader reads, as it is now: a local file, or an object on
/// the web or in cloud storage (`host` set, [`super::remote`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DataFile {
    /// Canonical: absolute, symlinks resolved, no `..`.
    pub path: String,
    /// Shortened for the cell: relative to the notebook's folder when it's
    /// inside it, else with `~` for the home folder.
    pub display: String,
    pub size: u64,
    /// Last modified (ms since the epoch).
    pub mtime_ms: i64,
    /// The glob it was found by, resolved to its folder
    /// (`/Users/ana/exports/ventas_*.csv`); none for a file written out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// Its content changed since this notebook last read it: the
    /// modification time it had then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_mtime_ms: Option<i64>,
    /// The files read with it (a shapefile's `.shx`, `.dbf`, `.prj`…; a
    /// SQLite file's pending `-wal`): granted, checked and approved with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sidecars: Vec<DataFile>,
    /// An object on the web or in cloud storage: what the dialog shows
    /// first (`example.com`, `ventas (Amazon S3)`). `path` is its canonical
    /// URL (or, before it is approved, the pattern).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// An object's version (its `ETag`, else `Last-Modified` and size): the
    /// change notice compares it instead of a hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl DataFile {
    /// The file and its sidecars.
    pub fn all(&self) -> impl Iterator<Item = &DataFile> {
        std::iter::once(self).chain(self.sidecars.iter())
    }
}

/// Anything with `://` (GDAL's `WFS:http://…`, `/vsicurl/https://…`) that
/// [`super::remote::parse`] didn't take: not a local file.
fn is_url(p: &str) -> bool {
    p.contains("://")
}

/// A GDAL virtual path (`/vsizip/…`, `/vsicurl/…`, `vsimem`…): a segment
/// `vsi…` anywhere, since GDAL takes them where it finds them.
fn is_virtual(p: &str) -> bool {
    p.split(['/', '\\']).any(|seg| {
        let s = seg.to_ascii_lowercase();
        s.starts_with("vsi") && s.len() > 3 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'?' || b == b'=')
    })
}

pub(crate) fn has_glob(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// [`plain`], for the access rules.
pub(crate) fn plain_path(p: PathBuf) -> PathBuf {
    plain(p)
}

/// Without Windows' `\\?\` prefix, which DuckDB doesn't take.
fn plain(p: PathBuf) -> PathBuf {
    match p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p,
    }
}

/// `name` matches the glob segment `pat` (`*`, `?`, `[a-z]`, `[!x]`).
pub(crate) fn wild(pat: &[char], name: &[char]) -> bool {
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

/// A glob split into the folder before its first wildcard and the
/// segments from there on.
fn glob_root(pattern: &Path) -> (PathBuf, Vec<String>) {
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
    (root, rest)
}

/// A glob with its folder canonical (`~/x/../exports/v_*.csv` →
/// `/Users/ana/exports/v_*.csv`): what a "Permitir siempre" stores for it.
fn canonical_pattern(pattern: &Path) -> Option<String> {
    let (root, rest) = glob_root(pattern);
    let mut p = plain(std::fs::canonicalize(&root).ok()?);
    for seg in rest {
        p.push(seg);
    }
    Some(p.to_string_lossy().to_string())
}

/// `path` (canonical) matches `pattern` (a [`canonical_pattern`]): segment
/// by segment, `**` standing for any number of folders.
pub fn pattern_matches(pattern: &str, path: &str) -> bool {
    fn segs(p: &str) -> Vec<String> {
        Path::new(p).components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect()
    }
    fn go(pat: &[String], name: &[String]) -> bool {
        match pat.first().map(String::as_str) {
            None => name.is_empty(),
            Some("**") => (0..=name.len()).any(|k| go(&pat[1..], &name[k..])),
            Some(p) => {
                let Some(n) = name.first() else { return false };
                let hit = if has_glob(p) { wild(&p.chars().collect::<Vec<_>>(), &n.chars().collect::<Vec<_>>()) } else { p == n };
                hit && go(&pat[1..], &name[1..])
            }
        }
    }
    go(&segs(pattern), &segs(path))
}

/// The files `pattern` matches (`**` is any number of folders).
fn expand(pattern: &Path) -> Result<Vec<PathBuf>, String> {
    let (root, rest) = glob_root(pattern);
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
#[cfg_attr(not(test), allow(dead_code))]
pub fn resolve(raw: &str, base: Option<&Path>, home: Option<&Path>) -> Result<Vec<PathBuf>, String> {
    Ok(resolve_found(raw, base, home)?.0)
}

/// [`resolve`], with the glob it was (canonical, [`canonical_pattern`]).
fn resolve_found(raw: &str, base: Option<&Path>, home: Option<&Path>) -> Result<(Vec<PathBuf>, Option<String>), String> {
    let p = raw.trim();
    if p.is_empty() {
        return Err("la ruta de un archivo está vacía".into());
    }
    if is_virtual(p) {
        return Err(format!("«{p}» es una ruta virtual de GDAL (/vsi…): estas celdas leen archivos locales escritos con su ruta, o URLs (https://, s3://, gs://, az://)"));
    }
    if is_url(p) {
        return Err(format!("«{p}» no es un archivo local ni una URL que estas celdas lean (https://, s3://, gs://, az://)"));
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
    let pattern = if has_glob(&shown) { canonical_pattern(&path) } else { None };
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
    Ok((out, pattern))
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
    Ok(DataFile { path: path.to_string_lossy().to_string(), display: display(path, base, home), size: meta.len(), mtime_ms, ..Default::default() })
}

/// The home folder.
pub fn home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}

// ---------------------------------------------------------------------------
// Planning

/// How a reader call is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadKind {
    /// `SELECT * FROM <text>` in a DuckDB reader session ([`open_reader`]),
    /// with this extension loaded first (`excel`, `spatial`).
    Duck { extension: Option<&'static str> },
    /// A SQLite file's table, read by DBine ([`read_sqlite`]).
    Sqlite { table: String },
}

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
    /// What identifies its rows: the call, its files and their sidecars as
    /// they are now (size and modification time), the cap. Loaded again
    /// when it changes.
    pub key: String,
    /// For the cell: the file, or the paths as written for several.
    pub display: String,
    pub kind: ReadKind,
    /// What goes after the path in [`FileRead::text`] (`, header = true`),
    /// as DBine passes it: the text again with other paths ([`FileRead::text_for`]).
    pub rest: String,
    /// Its files are objects on the web or in cloud storage: downloaded by
    /// DBine once approved ([`super::remote`]), read from local copies.
    pub remote: bool,
    /// A GeoPackage with a pending `-wal` or `-journal`: read from a private
    /// copy with them applied ([`gpkg_copy`]), so the originals are never
    /// written.
    pub gpkg_copy: bool,
}

impl FileRead {
    /// The call over `paths` instead of its files (the local copies of its
    /// objects, a GeoPackage's copy).
    pub fn text_for(&self, paths: &[String]) -> String {
        let single = !self.call.list && paths.len() == 1 && !self.call.paths.iter().any(|p| has_glob(p));
        let arg = if single { lit(&paths[0]) } else { format!("[{}]", paths.iter().map(|p| lit(p)).collect::<Vec<_>>().join(", ")) };
        format!("{}({arg}{})", self.call.function, self.rest)
    }

    /// Its key again, once its objects are known ([`super::remote::expand`]):
    /// their versions instead of the files' times. An object without one is
    /// read every time.
    pub fn rekey(&mut self, cap: u64) {
        let metas: Vec<(&str, u64, i64, Option<&str>)> = self.files.iter().flat_map(|f| f.all()).map(|f| (f.path.as_str(), f.size, f.mtime_ms, f.version.as_deref())).collect();
        let nonce = if self.files.iter().any(|f| f.host.is_some() && f.version.is_none()) { uuid::Uuid::new_v4().simple().to_string() } else { String::new() };
        self.key = sha256_hex(serde_json::json!([self.text, metas, cap, nonce]).to_string().as_bytes());
    }
}

/// A table name for `sqlite_scan` / `read_duckdb`: `tabla` or, where
/// `schema` is allowed, `esquema.tabla`. Each part is letters, digits,
/// `_`, `$`, `-` or spaces (no quotes, dots or control characters), so it
/// can't be anything but a name; it still goes to SQL as a string.
pub fn table_name(raw: &str, schema: bool, function: &str) -> Result<(Option<String>, String), String> {
    let bad = || {
        format!("«{function}»: «{raw}» no es un nombre de tabla válido (letras, números, _, $, - o espacios{})", if schema { "; con su esquema: esquema.tabla" } else { "" })
    };
    let part_ok = |p: &str| !p.trim().is_empty() && p.len() <= 128 && p.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '$' | '-' | ' '));
    let parts: Vec<&str> = raw.split('.').collect();
    match parts.as_slice() {
        [t] if part_ok(t) => Ok((None, t.to_string())),
        [s, t] if schema && part_ok(s) && part_ok(t) => Ok((Some(s.to_string()), t.to_string())),
        _ => Err(bad()),
    }
}

/// The file's extension as [`GEO_FORMATS`] has it (`shp.zip` for a zipped
/// shapefile), lowercase.
fn geo_ext(path: &Path) -> String {
    geo_ext_of(&path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())
}

/// [`geo_ext`] of a file name (a URL's last segment).
fn geo_ext_of(name: &str) -> String {
    let name = name.to_lowercase();
    if name.ends_with(".shp.zip") {
        return "shp.zip".into();
    }
    name.rsplit_once('.').map(|(_, e)| e.to_string()).unwrap_or_default()
}

/// The sidecars of `main` that exist (canonical): files next to it with
/// its stem and one of `exts` (any case: `zonas.SHX` too).
pub fn sidecars(main: &Path, exts: &[&str]) -> Vec<PathBuf> {
    let (Some(dir), Some(name)) = (main.parent(), main.file_name().map(|n| n.to_string_lossy().to_lowercase())) else { return Vec::new() };
    let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s).to_string();
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<PathBuf> = Vec::new();
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_lowercase();
        let hit = exts.iter().any(|x| n == format!("{stem}.{x}"));
        if hit && e.path() != main {
            if let Ok(c) = std::fs::canonicalize(e.path()) {
                let c = plain(c);
                if c.is_file() && c != main && !out.contains(&c) {
                    out.push(c);
                }
            }
        }
    }
    out.sort();
    out
}

/// A SQLite file's pending journal: its `-wal` or `-journal`, when not
/// empty (the file alone isn't the whole database then).
fn sqlite_journals(main: &Path) -> Vec<PathBuf> {
    ["-wal", "-journal"]
        .iter()
        .filter_map(|s| {
            let p = PathBuf::from(format!("{}{s}", main.display()));
            std::fs::metadata(&p).ok().filter(|m| m.is_file() && m.len() > 0).map(|_| plain(std::fs::canonicalize(&p).unwrap_or(p)))
        })
        .collect()
}

/// `st_read`'s arguments checked ([`GEO_PARAMS`], [`GEO_OPEN_OPTIONS`]),
/// as DBine passes them: the cell's, then `allowed_drivers`.
fn geo_args(call: &ReaderCall) -> Result<String, String> {
    let mut out = String::new();
    for a in &call.args {
        let Some(name) = a.name.as_deref() else {
            return Err("«st_read» lee un archivo: después de la ruta van solo opciones con nombre (layer = '…')".into());
        };
        if !GEO_PARAMS.contains(&name) {
            return Err(format!("«st_read» no acepta «{name}» en estas celdas (sí: {})", GEO_PARAMS.join(", ")));
        }
        let Some(value) = &a.value else {
            return Err(format!("«st_read»: el valor de «{name}» tiene que ser un texto, un número, true/false o una lista de textos escritos tal cual"));
        };
        match (name, value) {
            ("layer", Lit::Str(_)) | ("keep_wkb", Lit::Word(_)) | ("max_batch_size", Lit::Word(_)) => {}
            ("open_options", Lit::List(opts)) => {
                for o in opts {
                    let (k, v) = o.split_once('=').unwrap_or((o, ""));
                    let key = k.trim().to_ascii_uppercase();
                    if !GEO_OPEN_OPTIONS.contains(&key.as_str()) {
                        return Err(format!("«st_read»: la opción de GDAL «{}» no se puede usar en estas celdas (sí: {})", k.trim(), GEO_OPEN_OPTIONS.join(", ")));
                    }
                    if v.contains(['/', '\\', ':']) || v.to_ascii_lowercase().contains("vsi") {
                        return Err(format!("«st_read»: el valor de «{key}» no puede ser una ruta ni una dirección"));
                    }
                }
            }
            _ => return Err(format!("«st_read»: «{name}» no lleva ese valor")),
        }
        out.push_str(&format!(", {name} = {}", value.sql()));
    }
    out.push_str(&format!(", allowed_drivers = {}", Lit::List(GEO_DRIVERS.iter().map(|d| d.to_string()).collect()).sql()));
    Ok(out)
}

/// The reader calls of `sql`, resolved against `base` (the notebook's
/// folder; `None` for an unsaved notebook).
#[cfg_attr(not(test), allow(dead_code))]
pub fn plan(sql: &str, base: Option<&Path>, home: Option<&Path>, cap: u64) -> Result<Vec<FileRead>, String> {
    plan_with(sql, base, home, cap, &super::remote::Net::default())
}

/// A reader call over URLs (`https://`, `s3://`, `gs://`, `az://`): its
/// entries are the URLs and patterns as written, canonical, with nothing
/// fetched yet (the approval comes first, [`super::remote::expand`] after).
fn plan_remote(call: ReaderCall, urls: Vec<super::remote::RemoteUrl>, cap: u64) -> Result<FileRead, String> {
    use super::remote::{pending_file, Cloud, REMOTE_READERS};
    let f = call.function.as_str();
    if !REMOTE_READERS.contains(&f) {
        return Err(format!("«{f}» lee solo archivos locales; desde la red leen read_csv, read_parquet, read_json y read_ndjson (y st_read desde https://)"));
    }
    let mut rest = call.rest.clone();
    let mut extension = None;
    if f == "st_read" {
        let [u] = urls.as_slice() else {
            return Err(format!("«st_read» lee un solo archivo y «{}» son {}", call.paths.join(", "), urls.len()));
        };
        if call.list {
            return Err(format!("«st_read» lee un solo archivo y «{}» es una lista", call.paths.join(", ")));
        }
        if u.cloud != Cloud::Https {
            return Err(format!("«st_read» lee de la red solo https://: «{}»", u.canonical));
        }
        let name = u.key.split('?').next().unwrap_or("").rsplit('/').next().unwrap_or("").to_string();
        let ext = geo_ext_of(&name);
        if GEO_INDIRECT.contains(&ext.as_str()) {
            return Err(format!("«st_read» no lee archivos «.{ext}» en estas celdas: apuntan a otros archivos o a la red (un VRT, un GML…)"));
        }
        if matches!(ext.as_str(), "shp" | "dbf") {
            return Err(format!("«st_read» lee un shapefile desde la red solo comprimido («.shp.zip» o «.shz»): «{}»", u.canonical));
        }
        if ext == "zip" {
            return Err(format!("«st_read» lee comprimido solo un shapefile «.shp.zip» o «.shz»: «{}»", u.canonical));
        }
        if !GEO_FORMATS.contains(&ext.as_str()) {
            let list: Vec<String> = GEO_FORMATS.iter().filter(|e| !matches!(**e, "shp" | "dbf")).map(|e| format!(".{e}")).collect();
            return Err(format!("«st_read» no lee «.{ext}» en estas celdas (sí: {})", list.join(", ")));
        }
        rest = geo_args(&call)?;
        extension = Some("spatial");
    }
    let mut files: Vec<DataFile> = Vec::new();
    for u in &urls {
        if !files.iter().any(|x| x.path == u.canonical) {
            files.push(pending_file(u));
        }
    }
    if files.len() > MAX_FILES {
        return Err(format!("«{f}» lee más de {MAX_FILES} archivos"));
    }
    let single = !call.list && files.len() == 1 && !urls[0].glob;
    let arg = if single { lit(&files[0].path) } else { format!("[{}]", files.iter().map(|x| lit(&x.path)).collect::<Vec<_>>().join(", ")) };
    let text = format!("{f}({arg}{rest})");
    let table = format!("file:{}", &sha256_hex(text.as_bytes())[..16]);
    let display = if files.len() == 1 { files[0].display.clone() } else { call.paths.join(", ") };
    let mut r = FileRead { call, text, files, table, key: String::new(), display, kind: ReadKind::Duck { extension }, rest, remote: true, gpkg_copy: false };
    r.rekey(cap);
    Ok(r)
}

/// [`plan`], reading URLs as `net` allows ([`super::remote::parse`]).
pub fn plan_with(sql: &str, base: Option<&Path>, home: Option<&Path>, cap: u64, net: &super::remote::Net) -> Result<Vec<FileRead>, String> {
    let mut out = Vec::new();
    for call in reader_calls(sql)? {
        let f = call.function.as_str();
        let urls: Vec<Option<super::remote::RemoteUrl>> = call.paths.iter().map(|p| super::remote::parse(p.trim(), net)).collect::<Result<_, _>>()?;
        if urls.iter().any(Option::is_some) {
            if urls.iter().any(Option::is_none) {
                return Err(format!("«{f}» no puede mezclar archivos locales y URLs en la misma llamada"));
            }
            out.push(plan_remote(call, urls.into_iter().flatten().collect(), cap)?);
            continue;
        }
        let mut paths = Vec::new();
        let mut patterns: Vec<Option<String>> = Vec::new();
        for raw in &call.paths {
            let (found, pattern) = resolve_found(raw, base, home)?;
            for p in found {
                if !paths.contains(&p) {
                    paths.push(p);
                    patterns.push(pattern.clone());
                }
            }
        }
        if paths.len() > MAX_FILES {
            return Err(format!("«{f}» lee más de {MAX_FILES} archivos"));
        }
        let one = matches!(f, "read_xlsx" | "st_read" | "sqlite_scan");
        if one && (paths.len() != 1 || call.list || call.paths.iter().any(|p| has_glob(p))) {
            return Err(format!("«{f}» lee un solo archivo y «{}» son {}", call.paths.join(", "), paths.len()));
        }
        let mut files = paths.iter().map(|p| data_file(p, base, home)).collect::<Result<Vec<_>, _>>()?;
        for (f, pattern) in files.iter_mut().zip(patterns) {
            f.pattern = pattern;
        }
        // What each reader takes after the path, and the files read with it.
        let mut kind = ReadKind::Duck { extension: (f == "read_xlsx").then_some("excel") };
        let mut rest = call.rest.clone();
        // (the file it goes with, the sidecar)
        let mut side: Vec<(usize, PathBuf)> = Vec::new();
        let mut gpkg = false;
        match f {
            "st_read" => {
                let ext = geo_ext(&paths[0]);
                if GEO_INDIRECT.contains(&ext.as_str()) {
                    return Err(format!("«st_read» no lee archivos «.{ext}» en estas celdas: apuntan a otros archivos o a la red (un VRT, un GML…)"));
                }
                if ext == "zip" {
                    return Err(format!("«st_read» lee comprimido solo un shapefile «.shp.zip» o «.shz»: descomprimí «{}» y leé el archivo de adentro", files[0].display));
                }
                if !GEO_FORMATS.contains(&ext.as_str()) {
                    let list: Vec<String> = GEO_FORMATS.iter().map(|e| format!(".{e}")).collect();
                    return Err(format!("«st_read» no lee «.{ext}» en estas celdas (sí: {})", list.join(", ")));
                }
                rest = geo_args(&call)?;
                let exts = match ext.as_str() {
                    "shp" | "dbf" => SHAPE_SIDECARS,
                    "csv" => CSV_SIDECARS,
                    _ => &[],
                };
                side = sidecars(&paths[0], exts).into_iter().map(|p| (0, p)).collect();
                // A GeoPackage is a SQLite file: a pending -wal or -journal
                // is part of it, read from a private copy ([`gpkg_copy`]).
                if ext == "gpkg" {
                    side.extend(sqlite_journals(&paths[0]).into_iter().map(|p| (0, p)));
                    gpkg = !side.is_empty();
                }
                kind = ReadKind::Duck { extension: Some("spatial") };
            }
            "sqlite_scan" => {
                let table = match call.args.as_slice() {
                    [Arg { name: None, value: Some(Lit::Str(t)) }] => table_name(t, false, f)?.1,
                    _ => return Err("«sqlite_scan» lleva el archivo y el nombre de la tabla: sqlite_scan('archivo.db', 'tabla')".into()),
                };
                rest = format!(", {}", lit(&table));
                side = sqlite_journals(&paths[0]).into_iter().map(|p| (0, p)).collect();
                kind = ReadKind::Sqlite { table };
            }
            "read_duckdb" => {
                let (mut schema, mut table) = (None, None);
                for a in &call.args {
                    match (a.name.as_deref(), &a.value) {
                        (None, Some(Lit::Str(t))) if table.is_none() => (schema, table) = table_name(t, true, f).map(|(s, t)| (s, Some(t)))?,
                        (Some("table_name"), Some(Lit::Str(t))) => table = Some(table_name(t, false, f)?.1),
                        (Some("schema_name"), Some(Lit::Str(s))) => schema = Some(table_name(s, false, f)?.1),
                        _ => return Err("«read_duckdb» lleva el archivo y la tabla: read_duckdb('archivo.duckdb', 'tabla') o 'esquema.tabla' (o table_name = '…', schema_name = '…')".into()),
                    }
                }
                let Some(table) = table else {
                    return Err("«read_duckdb» necesita la tabla: read_duckdb('archivo.duckdb', 'tabla')".into());
                };
                rest = format!(", table_name = {}", lit(&table));
                if let Some(s) = schema {
                    rest.push_str(&format!(", schema_name = {}", lit(&s)));
                }
                // A DuckDB file's write-ahead log, when there is one.
                for (i, p) in paths.iter().enumerate() {
                    let wal = PathBuf::from(format!("{}.wal", p.display()));
                    if wal.is_file() {
                        side.push((i, plain(std::fs::canonicalize(&wal).unwrap_or(wal))));
                    }
                }
            }
            _ => {}
        }
        for (owner, s) in &side {
            let d = data_file(s, base, home)?;
            files[*owner].sidecars.push(d);
        }
        let single = !call.list && paths.len() == 1 && !call.paths.iter().any(|p| has_glob(p));
        let arg = if single { lit(&files[0].path) } else { format!("[{}]", files.iter().map(|f| lit(&f.path)).collect::<Vec<_>>().join(", ")) };
        let text = format!("{f}({arg}{rest})");
        let table = format!("file:{}", &sha256_hex(text.as_bytes())[..16]);
        let metas: Vec<(&str, u64, i64)> = files.iter().flat_map(|f| f.all()).map(|f| (f.path.as_str(), f.size, f.mtime_ms)).collect();
        let key = sha256_hex(serde_json::json!([text, metas, cap]).to_string().as_bytes());
        let display = if files.len() == 1 { files[0].display.clone() } else { call.paths.join(", ") };
        out.push(FileRead { call, text, files, table, key, display, kind, rest, remote: false, gpkg_copy: gpkg });
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
/// read; the reader session is granted exactly these. A file of the
/// always-blocked list refuses the run ([`super::access::Policy`]); a file
/// neither granted to this notebook nor approved for this run stops it
/// before anything runs, and the UI asks ([`super::access::Gate::Ask`]).
pub fn allowed_files(files: &[DataFile], access: &super::access::Access<'_>) -> Result<Vec<DataFile>, super::access::Gate> {
    access.gate(files)
}

// ---------------------------------------------------------------------------
// The reader session

/// What a DuckDB extension is loaded for: each has its own whole message
/// when it can't be loaded.
#[derive(Clone, Copy)]
pub enum ExtensionUse {
    CellSpatial,
    ReadWorkbook,
    ReadGeoFiles,
}

/// `LOAD` a DuckDB extension, fetching it into DuckDB's extensions folder
/// the first time (`INSTALL`, as the data-file connections do for
/// `.xlsx`); only while the session isn't locked down yet. `what`: what it
/// is for, for the message.
pub async fn load_extension(s: &mut dyn Session, name: &str, what: ExtensionUse) -> CommandResult<()> {
    let mut out = QueryOutcome::default();
    let loaded = s.execute(&format!("LOAD {name}"), 1, &mut out).await.is_ok() && out.error.is_none();
    if !loaded {
        let mut out = QueryOutcome::default();
        let r = s.execute(&format!("INSTALL {name}; LOAD {name}"), 1, &mut out).await;
        if let Some(e) = r.err().map(|e| e.to_string()).or(out.error) {
            return Err(CommandError::Internal(match what {
                ExtensionUse::CellSpatial => format!("no se pudo cargar la extensión {name} de DuckDB para las funciones geográficas (ST_…) de esta celda: {e}"),
                ExtensionUse::ReadWorkbook => format!("no se pudo cargar la extensión {name} de DuckDB para leer el libro: {e}"),
                ExtensionUse::ReadGeoFiles => format!("no se pudo cargar la extensión {name} de DuckDB para leer archivos geográficos (st_read): {e}"),
            }));
        }
    }
    Ok(())
}

/// What the reader session of `r` may read: its files and their sidecars
/// (and, for `read_duckdb`, the path of each file's write-ahead log, which
/// DuckDB looks for even when there is none).
pub fn reader_paths(r: &FileRead) -> Vec<String> {
    let mut out: Vec<String> = r.files.iter().flat_map(|f| f.all()).map(|f| f.path.clone()).collect();
    if r.call.function == "read_duckdb" {
        for f in &r.files {
            let wal = format!("{}.wal", f.path);
            if !out.contains(&wal) {
                out.push(wal);
            }
        }
    }
    out
}

/// A DuckDB session of its own, in memory, for one reader call: it may
/// read `paths` (its files, [`reader_paths`]) and nothing else.
/// `allowed_paths` first (DuckDB only takes it while external access is
/// on), the call's extension (`excel`, `spatial`) loaded, then the
/// notebook's lockdown ([`super::local::sandbox`]).
pub async fn open_reader(paths: &[String], extension: Option<&str>) -> CommandResult<Box<dyn Session>> {
    let mut cfg = ConnectionConfig { driver: "duckdb".into(), host: ":memory:".into(), ..Default::default() };
    cfg.options.insert("isolated".into(), "true".into());
    let mut s = dbine_drivers::open_session(&cfg, None).await?;
    let fail = |e: String| CommandError::Internal(format!("no se pudo preparar la lectura de archivos: {e}"));
    let grant = format!("SET allowed_paths = [{}]", paths.iter().map(|p| lit(p)).collect::<Vec<_>>().join(", "));
    let mut out = QueryOutcome::default();
    s.execute(&grant, 1, &mut out).await.map_err(|e| fail(e.to_string()))?;
    if let Some(e) = out.error {
        return Err(fail(e));
    }
    match extension {
        Some("excel") => load_extension(s.as_mut(), "excel", ExtensionUse::ReadWorkbook).await?,
        Some("spatial") => load_extension(s.as_mut(), "spatial", ExtensionUse::ReadGeoFiles).await?,
        Some(other) => return Err(fail(format!("extensión desconocida «{other}»"))),
        None => {}
    }
    super::local::sandbox(s.as_mut()).await?;
    Ok(s)
}

// ---------------------------------------------------------------------------
// SQLite files

/// A private copy, in `dir`, of a GeoPackage with a pending `-wal` or
/// `-journal` (its sidecars), with them applied: SQLite checkpoints the
/// WAL into the copy (or rolls the journal back) and leaves it in rollback
/// mode, so GDAL reads one self-contained file and creates nothing. The
/// originals are only read. Returns the copy's path.
pub fn gpkg_copy(main: &DataFile, dir: &Path) -> Result<String, String> {
    use rusqlite::OpenFlags;
    let name = Path::new(&main.path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "copia.gpkg".into());
    let db = dir.join(&name);
    std::fs::copy(&main.path, &db).map_err(|e| format!("no se pudo copiar «{}»: {e}", main.display))?;
    for s in &main.sidecars {
        let suffix = s.path.strip_prefix(&main.path).unwrap_or("");
        if matches!(suffix, "-wal" | "-journal") {
            std::fs::copy(&s.path, dir.join(format!("{name}{suffix}"))).map_err(|e| format!("no se pudo copiar «{}»: {e}", s.display))?;
        }
    }
    let fail = |e: rusqlite::Error| format!("no se pudo aplicar el -wal de «{}» a su copia: {e}", main.display);
    {
        let c = rusqlite::Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(fail)?;
        let _ = c.pragma_update(None, "trusted_schema", false);
        c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())).map_err(fail)?;
        c.query_row("PRAGMA journal_mode = DELETE", [], |_| Ok(())).map_err(fail)?;
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(dir.join(format!("{name}{suffix}")));
    }
    Ok(db.to_string_lossy().to_string())
}

/// `file:` URI of `path` for SQLite, read-only and `immutable`: SQLite
/// neither locks the file nor creates or touches a `-wal`, `-shm` or
/// `-journal` next to it.
fn sqlite_uri(path: &Path, immutable: bool) -> String {
    let mut p = path.to_string_lossy().replace('\\', "/");
    if !p.starts_with('/') {
        p.insert(0, '/');
    }
    let mut enc = String::with_capacity(p.len() + 16);
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/:".contains(&b) {
            enc.push(b as char);
        } else {
            enc.push_str(&format!("%{b:02X}"));
        }
    }
    format!("file://{enc}?mode=ro{}", if immutable { "&immutable=1" } else { "" })
}

/// A folder of DBine's own in the temp folder, removed when dropped.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The rows of a `sqlite_scan` read by DBine: up to `cap`, and whether there
/// were more. The file is opened read-only and `immutable` (nothing is
/// written next to it). A file with a pending `-wal` or `-journal` (open in
/// another program, or left mid-write) isn't the whole database by itself:
/// it and its journal are copied into a private temp folder, the copy is
/// read (SQLite applies the journal to the copy) and removed.
pub fn read_sqlite(r: &FileRead, cap: u64, interrupt: &dyn Fn(rusqlite::InterruptHandle)) -> Result<(Vec<dbine_driver::ResultColumn>, Vec<Vec<serde_json::Value>>, bool), String> {
    use rusqlite::types::ValueRef;
    use rusqlite::OpenFlags;
    let ReadKind::Sqlite { table } = &r.kind else { return Err("no es una lectura de SQLite".into()) };
    let main = &r.files[0];
    let fail = |e: rusqlite::Error| format!("no se pudo leer la tabla «{table}» de «{}»: {e}", main.display);
    let mut _copy = None;
    let (uri, flags) = if main.sidecars.is_empty() {
        (sqlite_uri(Path::new(&main.path), true), OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX)
    } else {
        let dir = std::env::temp_dir().join(format!("dbine-sqlite-{}", uuid::Uuid::new_v4().simple()));
        let mut b = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        b.create(&dir).map_err(|e| format!("no se pudo preparar la copia de «{}»: {e}", main.display))?;
        let tmp = TempDir(dir);
        let db = tmp.0.join("db");
        std::fs::copy(&main.path, &db).map_err(|e| format!("no se pudo copiar «{}»: {e}", main.display))?;
        for s in &main.sidecars {
            let suffix = s.path.strip_prefix(&main.path).unwrap_or("");
            if matches!(suffix, "-wal" | "-journal") {
                std::fs::copy(&s.path, tmp.0.join(format!("db{suffix}"))).map_err(|e| format!("no se pudo copiar «{}»: {e}", s.display))?;
            }
        }
        let uri = sqlite_uri(&db, false).replace("?mode=ro", "?mode=rw");
        _copy = Some(tmp);
        (uri, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX)
    };
    let conn = rusqlite::Connection::open_with_flags(&uri, flags).map_err(fail)?;
    // No function with side effects runs from the file's views.
    let _ = conn.pragma_update(None, "trusted_schema", false);
    interrupt(conn.get_interrupt_handle());
    let sql = format!("SELECT * FROM {} LIMIT {}", dbine_driver::sql::quote_ident(dbine_driver::sql::Quote::Double, table), cap.saturating_add(1));
    let mut stmt = conn.prepare(&sql).map_err(fail)?;
    let columns: Vec<dbine_driver::ResultColumn> =
        stmt.columns().iter().map(|c| dbine_driver::ResultColumn { name: c.name().to_string(), type_name: c.decl_type().unwrap_or("").to_string() }).collect();
    let n = columns.len();
    let mut rows = Vec::new();
    let mut q = stmt.query([]).map_err(fail)?;
    while let Some(row) = q.next().map_err(fail)? {
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            out.push(match row.get_ref(i).map_err(fail)? {
                ValueRef::Null => serde_json::Value::Null,
                ValueRef::Integer(v) => dbine_driver::json_i64(v),
                ValueRef::Real(v) => dbine_driver::json_f64(v),
                ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned().into(),
                ValueRef::Blob(b) if dbine_driver::is_spatial_blob(b) => dbine_driver::json_spatial_bytes(b),
                ValueRef::Blob(b) => dbine_driver::json_bytes(b),
            });
        }
        rows.push(out);
    }
    let capped = rows.len() as u64 > cap;
    rows.truncate(cap as usize);
    Ok((columns, rows, capped))
}

#[cfg(test)]
#[path = "files_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "files_geo_tests.rs"]
mod geo_tests;
