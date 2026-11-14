//! The functions an engine's query language offers, for the editor's
//! completion (issue #97): SQL functions, and what plays their part in other
//! languages (Redis commands, MongoDB operators, Cypher and Flux functions…).
//!
//! A driver reads them from the server where the server lists them, so its
//! version, extensions and the user's own functions count. Otherwise it
//! returns the built-ins its engine documents, from a static catalog
//! ([`parse_catalog`]) filtered by the server's version. Static catalogs carry
//! no descriptions: a description is shown only when the server gives one,
//! as the server writes it.

use serde::{Deserialize, Serialize};

/// What a catalog entry is, which decides where the editor offers it and
/// what selecting it inserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionKind {
    /// A function called with parentheses: `upper(x)`.
    Scalar,
    /// An aggregate: `count(*)`, `approx_distinct(x)`.
    Aggregate,
    /// A window function: `row_number() OVER (…)`.
    Window,
    /// A table function, used where a table goes: `generate_series(1, 10)`.
    Table,
    /// An operator written as a name, without parentheses: MongoDB's `$match`,
    /// CouchDB's `$regex`.
    Operator,
    /// A command at the start of a line: Redis `HSET`, etcd `get`.
    Command,
}

impl FunctionKind {
    /// The one-letter code of the static catalogs.
    pub fn from_code(code: &str) -> Option<FunctionKind> {
        Some(match code {
            "s" => FunctionKind::Scalar,
            "a" => FunctionKind::Aggregate,
            "w" => FunctionKind::Window,
            "t" => FunctionKind::Table,
            "o" => FunctionKind::Operator,
            "c" => FunctionKind::Command,
            _ => return None,
        })
    }
}

/// One entry of the catalog. Overloads are separate entries with the same
/// name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionInfo {
    pub name: String,
    pub kind: FunctionKind,
    /// The arguments as the engine documents them, with their parentheses:
    /// `(x, accuracy)`, `(key field value [field value ...])` for a command.
    /// Empty when there is nothing to show.
    #[serde(default)]
    pub args: String,
    /// The result type, when known: `bigint`, `double`.
    #[serde(default)]
    pub returns: String,
    /// What the server says about it, as the server writes it. Empty for
    /// static catalogs.
    #[serde(default)]
    pub description: String,
    /// The schema, package or namespace it lives in when it isn't a
    /// built-in reached by its bare name (a user's function in `sales`,
    /// Flux's `strings`). Empty otherwise.
    #[serde(default)]
    pub schema: String,
    /// The editor language the entry belongs to when it isn't the driver's
    /// own: `sql` for the SQL lines of a JSON console (Elasticsearch,
    /// OpenSearch and Solr run `SELECT`/`SHOW` lines next to their JSON
    /// requests). Empty for the driver's language.
    #[serde(default)]
    pub language: String,
}

impl FunctionInfo {
    pub fn new(name: impl Into<String>, kind: FunctionKind) -> Self {
        FunctionInfo {
            name: name.into(),
            kind,
            args: String::new(),
            returns: String::new(),
            description: String::new(),
            schema: String::new(),
            language: String::new(),
        }
    }
    pub fn args(mut self, args: impl Into<String>) -> Self {
        self.args = args.into();
        self
    }
    pub fn returns(mut self, returns: impl Into<String>) -> Self {
        self.returns = returns.into();
        self
    }
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }
    pub fn schema(mut self, schema: impl Into<String>) -> Self {
        self.schema = schema.into();
        self
    }
    pub fn language(mut self, language: impl Into<String>) -> Self {
        self.language = language.into();
        self
    }
}

/// Parses a static catalog, keeping the entries the server's `version` has.
///
/// One entry per line: `<kind> <name><args> [-> <returns>] [since=<v>]
/// [until=<v>]`, where `<kind>` is `s`, `a`, `w`, `t`, `o` or `c`
/// ([`FunctionKind::from_code`]), `<args>` starts at the first `(` (or the
/// first space for operators and commands) and runs up to ` -> ` or the
/// first ` since=` / ` until=`. `since` is the first version that has it and
/// `until` the first that no longer does. Blank lines and lines starting
/// with `#` are skipped. With no `version` (unknown), every entry is kept.
///
/// ```text
/// # MySQL
/// a count(expr) -> bigint
/// s json_value(json_doc, path) -> varchar since=8.0.21
/// o $dateTrunc since=5.0
/// c HSET key field value [field value ...] since=2.0.0
/// ```
pub fn parse_catalog(text: &str, version: Option<&str>) -> Vec<FunctionInfo> {
    let have = version.map(version_numbers).filter(|v| !v.is_empty());
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((code, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let Some(kind) = FunctionKind::from_code(code) else {
            continue;
        };
        let mut rest = rest.trim();
        let mut since = None;
        let mut until = None;
        // Trailing `since=` / `until=`, in either order.
        while let Some(at) = rest.rfind(char::is_whitespace) {
            let last = &rest[at + 1..];
            if let Some(v) = last.strip_prefix("since=") {
                since = Some(v);
            } else if let Some(v) = last.strip_prefix("until=") {
                until = Some(v);
            } else {
                break;
            }
            rest = rest[..at].trim_end();
        }
        if let Some(have) = &have {
            if since.is_some_and(|s| compare(have, &version_numbers(s)).is_lt()) {
                continue;
            }
            if until.is_some_and(|u| compare(have, &version_numbers(u)).is_ge()) {
                continue;
            }
        }
        let (sig, returns) = match rest.split_once(" -> ") {
            Some((s, r)) => (s.trim(), r.trim()),
            None => (rest, ""),
        };
        let split = match kind {
            FunctionKind::Operator | FunctionKind::Command => sig.find(char::is_whitespace),
            _ => sig.find('('),
        };
        let (name, args) = match split {
            Some(i) => (sig[..i].trim(), sig[i..].trim()),
            None => (sig, ""),
        };
        if name.is_empty() {
            continue;
        }
        out.push(FunctionInfo::new(name, kind).args(args).returns(returns));
    }
    out
}

/// The first dotted number in a version string: `PostgreSQL 16.2 on …` →
/// `[16, 2]`, `8.0.36-0ubuntu` → `[8, 0, 36]`, `v7.2.4` → `[7, 2, 4]`. Empty
/// when there is none.
pub fn version_numbers(version: &str) -> Vec<u32> {
    let bytes = version.as_bytes();
    let Some(start) = bytes.iter().position(u8::is_ascii_digit) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cur: Option<u32> = None;
    for &b in &bytes[start..] {
        if b.is_ascii_digit() {
            let d = u32::from(b - b'0');
            cur = Some(cur.unwrap_or(0).saturating_mul(10).saturating_add(d));
        } else if b == b'.' && cur.is_some() {
            out.extend(cur.take());
        } else {
            break;
        }
    }
    out.extend(cur);
    out
}

/// Whether `version` is at least `min`, both as [`version_numbers`] reads
/// them. An unreadable `version` counts as new enough.
pub fn version_at_least(version: &str, min: &str) -> bool {
    let have = version_numbers(version);
    have.is_empty() || compare(&have, &version_numbers(min)).is_ge()
}

/// Compares versions component by component; missing components are 0.
fn compare(a: &[u32], b: &[u32]) -> std::cmp::Ordering {
    let n = a.len().max(b.len());
    for i in 0..n {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        match x.cmp(&y) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

/// The static catalogs shared by more than one driver crate (a dialect
/// reached both natively and through ODBC, say). Each text follows
/// [`parse_catalog`]; a crate whose catalog is its own keeps it in its own
/// folder.
pub mod shared {
    /// SQL Server (also Azure SQL and the legacy TDS driver).
    pub const MSSQL: &str = include_str!("functions/mssql.txt");
    /// SAP ASE / Sybase.
    pub const SYBASE: &str = include_str!("functions/sybase.txt");
    /// Oracle.
    pub const ORACLE: &str = include_str!("functions/oracle.txt");
    /// SQLite (also libSQL / Turso), for builds without
    /// `pragma_function_list`.
    pub const SQLITE: &str = include_str!("functions/sqlite.txt");
    /// The SQL standard's functions: the base for an engine reached through
    /// ODBC or Flight SQL with no catalog of its own. The ODBC scalar
    /// functions come from what the driver reports (SQLGetInfo,
    /// GetSqlInfo), not from here.
    pub const ANSI: &str = include_str!("functions/ansi.txt");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_catalog_line_is_read_whole() {
        let c = parse_catalog(
            "# comment\n\
             a count(expr) -> bigint\n\
             s json_value(json_doc, path) -> varchar since=8.0.21\n\
             s old_password(str) until=5.7.5\n\
             o $dateTrunc since=5.0\n\
             c HSET key field value [field value ...] since=2.0.0\n\
             w row_number()\n\
             \n\
             x junk\n",
            None,
        );
        assert_eq!(c.len(), 6);
        assert_eq!(c[0].name, "count");
        assert_eq!(c[0].args, "(expr)");
        assert_eq!(c[0].returns, "bigint");
        assert_eq!(c[0].kind, FunctionKind::Aggregate);
        assert_eq!(c[1].args, "(json_doc, path)");
        assert_eq!(c[3].name, "$dateTrunc");
        assert_eq!(c[3].args, "");
        assert_eq!(c[4].name, "HSET");
        assert_eq!(c[4].args, "key field value [field value ...]");
        assert_eq!(c[5].args, "()");
        assert!(c.iter().all(|f| f.description.is_empty()));
    }

    #[test]
    fn the_servers_version_filters_the_catalog() {
        let text = "s a() since=8.0.21\ns b() until=5.7.5\ns c()\n";
        let names = |v| {
            parse_catalog(text, v)
                .into_iter()
                .map(|f| f.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(Some("8.0.36-0ubuntu")), ["a", "c"]);
        assert_eq!(names(Some("8.0.20")), ["c"]);
        assert_eq!(names(Some("5.7.4")), ["b", "c"]);
        assert_eq!(names(Some("")), ["a", "b", "c"]);
        assert_eq!(names(None), ["a", "b", "c"]);
    }

    #[test]
    fn versions_are_read_from_banners() {
        assert_eq!(version_numbers("PostgreSQL 16.2 on aarch64"), [16, 2]);
        assert_eq!(version_numbers("v7.2.4"), [7, 2, 4]);
        assert_eq!(version_numbers("Oracle Database 23ai"), [23]);
        assert!(version_numbers("unknown").is_empty());
        assert!(version_at_least("8.0.36", "8.0.21"));
        assert!(!version_at_least("8.0", "8.0.21"));
        assert!(version_at_least("8.0.21", "8.0.21"));
        assert!(version_at_least("?", "9"));
    }

    #[test]
    fn the_shared_catalogs_parse() {
        for text in [
            shared::MSSQL,
            shared::SYBASE,
            shared::ORACLE,
            shared::SQLITE,
            shared::ANSI,
        ] {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
            }
        }
    }
}
