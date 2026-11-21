//! What depends on an object (`Session::dependents`): the impact of
//! renaming, dropping or changing a table, a column, a view or a routine.
//!
//! The generic scan ([`scan`]) works on any engine that reports foreign keys
//! or object definitions: foreign keys, indexes and checks come from
//! [`Session::database_schema`] (the engine's catalog: confirmed); views,
//! routines and triggers are read one by one ([`Session::definition`]) and
//! searched for the target's name ([`find_mentions`]). Drivers override
//! `Session::dependents` with catalog queries where the engine tracks
//! dependencies (faster, and confirmed), and can still use
//! [`find_mentions`] to point at the lines.
//!
//! A text match is a name, not a resolved reference: `Probable` when the
//! body names the target as code (for a column, along with its table);
//! `Review` when the name only shows inside a string (dynamic SQL), so
//! nobody can tell without reading it. Comments never count.

mod graph;
mod job_scan;
pub use graph::{
    graph_scan, LineageConsumer, LineageEdge, LineageExternal, LineageGraph, LineageJob,
    LineageScope, GRAPH_DEFINITIONS, GRAPH_OBJECTS,
};
pub use job_scan::{job_dependents_scan, job_hit, job_references_scan, merge_jobs, JOBS_READ};

use crate::info::DriverInfo;
use crate::model::{DbObject, ObjectRef};
use crate::schema::TableSchema;
use crate::sql::{code_tokens, NameToken, ScriptDialect, TokenKind};
use crate::{kinds, Result, Session};
use serde::{Deserialize, Serialize};

/// What to look for: an object, or one of its columns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyTarget {
    pub object: ObjectRef,
    /// A column of `object` (a table or a view).
    #[serde(default)]
    pub column: Option<String>,
}

/// What the generic scan needs from the driver: which kinds have source to
/// read, how its SQL is quoted, and whether the catalog has foreign keys
/// ([`DependencyScan::new`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyScan {
    /// Kinds whose source is searched ([`code_kinds`]).
    pub source_kinds: Vec<String>,
    pub dialect: ScriptDialect,
    /// [`crate::Capabilities::foreign_keys`]: `database_schema` reports
    /// foreign keys, indexes and checks.
    pub foreign_keys: bool,
    /// Kinds whose definition is code someone wrote (not the engine's
    /// generated DDL): [`references_scan`] reads the target's when it's one.
    #[serde(default)]
    pub written_kinds: Vec<String>,
    /// [`DriverInfo::has_schemas`]: in a qualified name the database comes
    /// before the schema (`ventas.dbo.Clientes`); without schemas it's the
    /// first part (`ventas.clientes`). The job scans ([`job_dependents_scan`])
    /// tell a name in another database by it.
    #[serde(default)]
    pub has_schemas: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DependencyReport {
    pub items: Vec<Dependent>,
    /// Definitions read.
    #[serde(default)]
    pub scanned: u32,
    /// Objects whose definition couldn't be read (`schema.name`): what
    /// depends on the target may hide there.
    #[serde(default)]
    pub unreadable: Vec<String>,
    /// What the UI tells the user about the result (a missing permission…).
    #[serde(default)]
    pub note: Option<String>,
    /// [`crate::Session::references`]: what the target reads from outside
    /// the database (another server or database, a file, a service): a Power
    /// BI partition's source, a linked server, a foreign table's remote one.
    #[serde(default)]
    pub external: Vec<ExternalRef>,
    /// [`crate::Session::job_dependents`]: the server's jobs whose steps run
    /// or name the target (a SQL Server Agent step, a pg_cron command, a
    /// MySQL event…).
    #[serde(default)]
    pub jobs: Vec<JobDependent>,
    /// What the UI tells the user about the jobs read, each a whole message
    /// (the UI translates them one by one): the engine's notes on its list
    /// ("se ven solo los jobs a los que tiene acceso este usuario"), the
    /// cap, a list that couldn't be read.
    #[serde(default)]
    pub job_notes: Vec<String>,
    /// [`crate::Session::dependents`]: what outside the database reads the
    /// target (a Power BI report or dashboard that reads a semantic model,
    /// a Databricks notebook…), the other direction of [`DependencyReport::external`].
    #[serde(default)]
    pub external_users: Vec<ExternalRef>,
}

/// A server job whose steps run or name the target (see
/// [`DependencyReport::jobs`]). Its id is the one the jobs calls take
/// ([`crate::jobs::ServerJob::id`]): lineage reads what it uses with
/// [`crate::Session::job_references`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobDependent {
    pub id: String,
    pub name: String,
    /// The engine's word for it ("SQL Server Agent", "pg_cron",
    /// "DBMS_SCHEDULER", "Event"…), as [`crate::jobs::ServerJob::kind`].
    pub kind: String,
    /// Its schema, owner, database or category, where the engine groups them.
    #[serde(default)]
    pub group: Option<String>,
    /// The steps that run or name the target, by name.
    #[serde(default)]
    pub steps: Vec<String>,
    /// `Confirmed` when the engine states the step runs the target (a
    /// procedure step); otherwise as for code ([`find_mentions`]).
    pub confidence: Confidence,
    /// The steps' lines that name the target (numbered within each step).
    #[serde(default)]
    pub mentions: Vec<Mention>,
}

/// A source outside the database (see [`DependencyReport::external`]). Each
/// part is what the engine says; none is required.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExternalRef {
    /// The kind of system, as the engine names it (`SQL Server`,
    /// `Databricks`, `postgres_fdw`, `Excel`…).
    #[serde(default)]
    pub system: Option<String>,
    /// Its server, host or URL.
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub schema: Option<String>,
    /// The table, file or object read there.
    #[serde(default)]
    pub name: Option<String>,
    /// What the UI shows besides (the partition, the link's name…).
    #[serde(default)]
    pub detail: Option<String>,
    /// Where the target's source names it.
    #[serde(default)]
    pub mentions: Vec<Mention>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Dependent {
    /// The object's kind (`view`, `procedure`, `table`…); for a foreign
    /// key, index or check, the table that holds it.
    pub kind: String,
    #[serde(default)]
    pub schema: Option<String>,
    pub name: String,
    /// The owner of a dependent object (a trigger's table…).
    #[serde(default)]
    pub parent: Option<String>,
    pub relation: Relation,
    pub confidence: Confidence,
    /// The constraint or index, as the UI shows it
    /// (`FK_Pedidos_Clientes (cliente_id) → dbo.Clientes (id)`).
    #[serde(default)]
    pub detail: Option<String>,
    /// Where the body names the target, in order.
    #[serde(default)]
    pub mentions: Vec<Mention>,
}

/// How the dependent uses the target.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    ForeignKey,
    Index,
    Check,
    /// A view, routine, trigger… whose source uses it.
    Code,
}

/// How sure the scan is.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// The engine's catalog records it.
    Confirmed,
    /// The source names it as code.
    Probable,
    /// The name only shows inside a string (dynamic SQL): read it.
    Review,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Mention {
    /// 1-based.
    pub line: u32,
    /// The line, trimmed and cut at [`LINE_CHARS`].
    pub text: String,
    /// Inside a string: dynamic SQL.
    #[serde(default)]
    pub dynamic: bool,
}

/// Mentions kept per object; the rest are left out.
pub const MAX_MENTIONS: usize = 20;
/// Characters kept of a mentioned line.
pub const LINE_CHARS: usize = 200;

/// Kinds whose source is code that can use other objects. Tables, keys,
/// indexes, sequences and types also have a definition, but it's their own
/// shape: reading every key of a Redis database to find a name makes no sense.
/// The kind of a [`DependencyTarget`] that is a schema (what a schema
/// rename looks for: `ventas.` qualifiers).
pub const SCHEMA: &str = "schema";
/// The kind of a [`DependencyTarget`] that is a table's constraint.
pub const CONSTRAINT: &str = "constraint";

pub const CODE_KINDS: &[&str] = &[
    kinds::VIEW,
    kinds::MATERIALIZED_VIEW,
    kinds::PROCEDURE,
    kinds::FUNCTION,
    kinds::TRIGGER,
    kinds::SYNONYM,
    kinds::STREAM,
    "package",
    "alias",
    "task",
    "sink",
];

/// The kinds of `info` whose source the generic scan searches.
pub fn code_kinds(info: &DriverInfo) -> Vec<String> {
    info.object_kinds
        .iter()
        .filter(|k| k.has_definition && CODE_KINDS.contains(&k.id))
        .map(|k| k.id.to_string())
        .collect()
}

impl DependencyScan {
    pub fn new(info: &DriverInfo, dialect: ScriptDialect, foreign_keys: bool) -> Self {
        DependencyScan {
            source_kinds: code_kinds(info),
            dialect,
            foreign_keys,
            written_kinds: info
                .object_kinds
                .iter()
                .filter(|k| k.has_definition && !k.generated_definition)
                .map(|k| k.id.to_string())
                .collect(),
            has_schemas: info.has_schemas,
        }
    }
}

/// The generic scan: foreign keys, indexes and checks from the catalog
/// (when the engine reports them), then every other object with source.
pub async fn scan<S: Session + ?Sized>(
    s: &mut S,
    target: &DependencyTarget,
    ctx: &DependencyScan,
) -> Result<DependencyReport> {
    let mut report = DependencyReport::default();
    if ctx.foreign_keys {
        report
            .items
            .extend(schema_dependents(&s.database_schema().await?, target));
    }
    for o in s.list_objects().await? {
        if !ctx.source_kinds.contains(&o.kind) {
            continue;
        }
        let obj = ObjectRef {
            kind: o.kind.clone(),
            schema: o.schema.clone(),
            name: o.name.clone(),
        };
        if same_object(&obj, &target.object) {
            continue;
        }
        let body = match s.definition(&obj).await {
            Ok(Some(b)) => b,
            Ok(None) => continue,
            Err(_) => {
                report
                    .unreadable
                    .push(display_name(o.schema.as_deref(), &o.name));
                continue;
            }
        };
        report.scanned += 1;
        if let Some((confidence, mentions)) = find_mentions(&body, &ctx.dialect, target) {
            report.items.push(Dependent {
                kind: o.kind,
                schema: o.schema,
                name: o.name,
                parent: o.parent,
                relation: Relation::Code,
                confidence,
                detail: None,
                mentions,
            });
        }
    }
    sort(&mut report.items);
    Ok(report)
}

// -- what the target uses (`Session::references`) -----------------------------------------

/// More names than this in one body are resolved against the whole object
/// list (read once) instead of one lookup each.
pub const LOOKUP_CAP: usize = 40;

/// Words after which a name is an object the body uses (`FROM x`, `JOIN
/// x`, `INSERT INTO x`, `EXEC x`…).
const USES_AFTER: &[&str] = &[
    "from", "join", "into", "update", "table", "exec", "execute", "call", "using", "merge",
    "truncate", "apply", "perform", "lateral",
];
/// Words that end a `FROM a, b` list.
const LIST_ENDS: &[&str] = &[
    "where",
    "group",
    "order",
    "having",
    "union",
    "intersect",
    "except",
    "minus",
    "select",
    "set",
    "values",
    "on",
    "limit",
    "window",
    "qualify",
    "returning",
    "when",
    "then",
    "else",
    "end",
    "begin",
    "pivot",
    "unpivot",
];
/// Words a name followed by `(` usually is that are no object of the
/// database (keywords and built-in functions): not looked up.
const NOT_OBJECTS: &[&str] = &[
    "in",
    "exists",
    "values",
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "cast",
    "convert",
    "coalesce",
    "isnull",
    "nullif",
    "ifnull",
    "nvl",
    "nvl2",
    "row_number",
    "rank",
    "dense_rank",
    "ntile",
    "lag",
    "lead",
    "over",
    "upper",
    "lower",
    "trim",
    "ltrim",
    "rtrim",
    "substring",
    "substr",
    "len",
    "length",
    "replace",
    "concat",
    "round",
    "floor",
    "ceiling",
    "ceil",
    "abs",
    "dateadd",
    "datediff",
    "getdate",
    "now",
    "date",
    "year",
    "month",
    "day",
    "format",
    "iif",
    "if",
    "case",
    "not",
    "and",
    "or",
    "any",
    "all",
    "some",
    "with",
    "as",
    "on",
    "select",
    "insert",
    "update",
    "delete",
    "into",
    "table",
    "returns",
    "return",
    "begin",
    "end",
    "declare",
    "set",
    "exec",
    "execute",
    "call",
    "default",
    "primary",
    "key",
    "references",
    "foreign",
    "check",
    "unique",
    "index",
    "varchar",
    "nvarchar",
    "char",
    "nchar",
    "int",
    "integer",
    "bigint",
    "smallint",
    "tinyint",
    "decimal",
    "numeric",
    "float",
    "real",
    "datetime",
    "datetime2",
    "timestamp",
    "time",
    "text",
    "boolean",
    "bit",
    "money",
    "varbinary",
    "binary",
    "string",
    "array",
    "map",
    "struct",
    "row",
    "using",
    "extract",
    "position",
    "overlay",
    "trunc",
    "to_char",
    "to_date",
    "to_number",
    "string_agg",
    "listagg",
    "group_concat",
    "json_value",
    "json_query",
    "openjson",
    "try_cast",
    "try_convert",
    "datepart",
    "datename",
    "eomonth",
    "isnumeric",
    "newid",
    "scope_identity",
    "object_id",
    "db_name",
    "user_name",
    "current_user",
    "session_user",
    "system_user",
    "raiserror",
    "throw",
    "print",
    "power",
    "sqrt",
    "exp",
    "log",
    "mod",
    "sign",
    "random",
    "rand",
    "greatest",
    "least",
    "decode",
    "regexp_replace",
    "regexp_like",
    "regexp_substr",
    "split_part",
    "array_agg",
    "unnest",
    "generate_series",
    "filter",
    "within",
    "partition",
    "rows",
    "range",
    "interval",
    "type",
    "returning",
];

/// A name the body uses as an object: its qualifiers (nearest first, as
/// [`match_qualifiers`] takes them), the name, where it's written and
/// whether it's called (`name(`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsedName {
    pub quals: Vec<String>,
    pub name: String,
    pub at: usize,
    pub call: bool,
}

/// The names `body` uses as objects: after `FROM`, `JOIN`, `INTO`, `UPDATE`,
/// `EXEC`, `CALL`… (and the rest of a `FROM a, b` list), and those called
/// (`f(`) that aren't keywords or built-ins. Strings and comments never
/// count. A name may still be a CTE or an alias: resolving it against the
/// database's objects drops those.
pub fn used_names(body: &str, dialect: &ScriptDialect) -> Vec<UsedName> {
    let toks = code_tokens(body, dialect);
    let is = |k: usize, p: &str| {
        toks.get(k)
            .is_some_and(|t| t.kind == TokenKind::Punct && t.text == p)
    };
    let word = |t: &NameToken<'_>| {
        // A quoted name is never a keyword.
        let quoted = matches!(body.as_bytes().get(t.start), Some(b'"' | b'`' | b'['));
        (t.kind == TokenKind::Name && !quoted).then(|| t.text.to_ascii_lowercase())
    };
    let mut out = Vec::new();
    let mut depth = 0i32;
    // Inside a FROM list, at this depth.
    let mut list: Option<i32> = None;
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        if t.kind == TokenKind::Punct {
            match t.text {
                "(" => depth += 1,
                ")" => {
                    depth -= 1;
                    if list.is_some_and(|d| depth < d) {
                        list = None;
                    }
                }
                ";" => list = None,
                _ => {}
            }
            i += 1;
            continue;
        }
        if t.kind != TokenKind::Name {
            i += 1;
            continue;
        }
        // The chain `a.b.c` starting here.
        let mut parts = vec![t.text];
        let mut j = i;
        while is(j + 1, ".") && toks.get(j + 2).is_some_and(|n| n.kind == TokenKind::Name) {
            parts.push(toks[j + 2].text);
            j += 2;
        }
        let single = (parts.len() == 1).then(|| word(t)).flatten();
        if let Some(w) = single.as_deref() {
            if w == "from" {
                list = Some(depth);
            } else if LIST_ENDS.contains(&w) && list == Some(depth) {
                list = None;
            }
        }
        let prev = i.checked_sub(1).map(|k| &toks[k]);
        let uses_word = |t: Option<&NameToken<'_>>| {
            t.and_then(word)
                .is_some_and(|w| USES_AFTER.contains(&w.as_str()))
        };
        // `FROM x`, and `FROM (x JOIN y …)` (how MySQL stores a view's
        // joins); not `TABLE (id int)`'s column list.
        let opens_join = toks
            .get(i.wrapping_sub(2))
            .filter(|_| i >= 2 && is(i - 1, "("))
            .and_then(word)
            .is_some_and(|w| w == "from" || w == "join");
        let after_word = uses_word(prev) || opens_join;
        let in_list = list == Some(depth)
            && prev.is_some_and(|p| p.kind == TokenKind::Punct && p.text == ",");
        let call = is(j + 1, "(");
        // Keywords are never objects; built-ins only when called (`log(x)`
        // is the function, `FROM log` a table called log).
        let keyword = single.as_deref().is_some_and(|w| {
            USES_AFTER.contains(&w) || LIST_ENDS.contains(&w) || (call && NOT_OBJECTS.contains(&w))
        });
        let local = !at_variable(&toks, i, dialect) && !remote_object(&toks, j);
        if (after_word || in_list || call) && !keyword && local {
            let name = parts.pop().unwrap_or_default().to_string();
            out.push(UsedName {
                quals: parts.iter().rev().map(|p| p.to_string()).collect(),
                name,
                at: toks[j].start,
                call,
            });
        }
        i = j + 1;
    }
    out
}

/// The generic [`crate::Session::references`]: a table's foreign keys
/// (the tables it points at, from the catalog: confirmed), and the objects
/// the target's own source names, resolved against the database's objects
/// (probable). Only a kind whose definition someone wrote is read (a SQL
/// table's is generated DDL).
pub async fn references_scan<S: Session + ?Sized>(
    s: &mut S,
    target: &DependencyTarget,
    ctx: &DependencyScan,
) -> Result<DependencyReport> {
    let t = &target.object;
    let mut report = DependencyReport::default();
    if ctx.foreign_keys && t.kind == kinds::TABLE {
        let fks = match s.index_usage(t).await {
            Ok(Some(u)) if !u.foreign_keys.is_empty() => u.foreign_keys,
            _ => s
                .database_schema()
                .await?
                .into_iter()
                .find(|x| names_table(x.schema.as_deref(), &x.name, t))
                .map(|x| x.foreign_keys)
                .unwrap_or_default(),
        };
        report.items.extend(foreign_key_parents(&fks, t));
    }
    if ctx.written_kinds.contains(&t.kind) {
        match s.definition(t).await {
            Ok(Some(body)) => {
                report.scanned = 1;
                let used = used_names(&body, &ctx.dialect);
                let objects = resolve_names(s, &used).await?;
                report.items.extend(used_objects(&body, &used, &objects, t));
            }
            Ok(None) => {}
            Err(_) => report.unreadable.push(display_name(t.schema(), &t.name)),
        }
    }
    sort(&mut report.items);
    Ok(report)
}

/// The tables `fks` (the target's own foreign keys) point at.
pub fn foreign_key_parents(
    fks: &[crate::schema::ForeignKeyDef],
    target: &ObjectRef,
) -> Vec<Dependent> {
    fks.iter()
        .map(|fk| {
            let name = fk
                .name
                .as_deref()
                .map(|n| format!("{n} "))
                .unwrap_or_default();
            Dependent {
                kind: kinds::TABLE.to_string(),
                schema: fk.ref_schema.clone().or_else(|| target.schema.clone()),
                name: fk.ref_table.clone(),
                parent: None,
                relation: Relation::ForeignKey,
                confidence: Confidence::Confirmed,
                detail: Some(format!(
                    "{name}({}) → {} ({})",
                    fk.columns.join(", "),
                    display_name(fk.ref_schema.as_deref(), &fk.ref_table),
                    fk.ref_columns.join(", ")
                )),
                mentions: Vec::new(),
            }
        })
        .collect()
}

/// The database's objects named like `used`: one lookup per name (narrow on
/// most engines), or the whole list once when there are many names or the
/// driver can't look up by name.
async fn resolve_names<S: Session + ?Sized>(s: &mut S, used: &[UsedName]) -> Result<Vec<DbObject>> {
    let mut names: Vec<String> = Vec::new();
    for u in used {
        if !names.iter().any(|n| eq(n, &u.name)) {
            names.push(u.name.clone());
        }
    }
    if names.is_empty() {
        return Ok(Vec::new());
    }
    if names.len() <= LOOKUP_CAP {
        let mut found = Vec::new();
        for name in &names {
            let q = crate::ObjectQuery {
                name: Some(name.clone()),
                limit: 50,
                ..Default::default()
            };
            match s.list_objects_page(&q).await {
                Ok(page) => found.extend(page.objects),
                Err(crate::Error::Unsupported(_)) => return s.list_objects().await,
                Err(e) => return Err(e),
            }
        }
        return Ok(found);
    }
    s.list_objects().await
}

/// What `used` resolves to among `objects`, each once with the lines that
/// name it. A qualified name must match the object's schema; an unqualified
/// one that several schemas have takes the target's schema's when there's
/// one. A call prefers routines, anything else prefers tables and views.
pub fn used_objects(
    body: &str,
    used: &[UsedName],
    objects: &[DbObject],
    target: &ObjectRef,
) -> Vec<Dependent> {
    const ROUTINES: &[&str] = &[kinds::FUNCTION, kinds::PROCEDURE, "package", "aggregate"];
    let mut hits: Vec<(DbObject, Vec<(usize, bool)>)> = Vec::new();
    for u in used {
        let named: Vec<&DbObject> = objects
            .iter()
            .filter(|o| eq(&o.name, &u.name))
            .filter(|o| match (&o.schema, u.quals.is_empty()) {
                (_, true) => true,
                (Some(sc), false) => match_qualifiers(&u.quals, sc) != Qualified::Other,
                (None, false) => true,
            })
            .filter(|o| {
                !same_object(
                    &ObjectRef {
                        kind: o.kind.clone(),
                        schema: o.schema.clone(),
                        name: o.name.clone(),
                    },
                    target,
                )
            })
            .collect();
        if named.is_empty() {
            continue;
        }
        // Several schemas: the target's own first.
        let in_own: Vec<&DbObject> = named
            .iter()
            .copied()
            .filter(|o| u.quals.is_empty() && o.schema.is_some() && o.schema == target.schema)
            .collect();
        let pool = if in_own.is_empty() { named } else { in_own };
        // A call: the routines among them; otherwise the rest first.
        let fits: Vec<&DbObject> = pool
            .iter()
            .copied()
            .filter(|o| ROUTINES.contains(&o.kind.as_str()) == u.call)
            .collect();
        let chosen = if fits.is_empty() { pool } else { fits };
        for o in chosen {
            match hits.iter_mut().find(|(h, _)| h == o) {
                Some((_, at)) => at.push((u.at, false)),
                None => hits.push((o.clone(), vec![(u.at, false)])),
            }
        }
    }
    hits.into_iter()
        .map(|(o, mut at)| {
            at.sort();
            Dependent {
                kind: o.kind,
                schema: o.schema,
                name: o.name,
                parent: o.parent,
                relation: Relation::Code,
                confidence: Confidence::Probable,
                detail: None,
                mentions: mentions(body, &at),
            }
        })
        .collect()
}

/// Foreign keys that point at the target (or hold the column), and the
/// target table's indexes and checks on the column.
pub fn schema_dependents(tables: &[TableSchema], target: &DependencyTarget) -> Vec<Dependent> {
    let t = &target.object;
    // A schema, index or constraint (a rename's target) holds no foreign keys
    // of its own: a table that happens to share its name isn't it.
    if matches!(t.kind.as_str(), SCHEMA | kinds::INDEX | CONSTRAINT) {
        return Vec::new();
    }
    let col = target.column.as_deref();
    let mut out = Vec::new();
    for table in tables {
        let is_target = names_table(table.schema.as_deref(), &table.name, t);
        let holder = |relation, detail: String| Dependent {
            kind: table.kind.clone(),
            schema: table.schema.clone(),
            name: table.name.clone(),
            parent: None,
            relation,
            confidence: Confidence::Confirmed,
            detail: Some(detail),
            mentions: Vec::new(),
        };
        for fk in &table.foreign_keys {
            let points_here = eq(&fk.ref_table, &t.name)
                && fk
                    .ref_schema
                    .as_deref()
                    .is_none_or(|s| t.schema().is_none_or(|ts| eq(s, ts)));
            let hit = match col {
                None => points_here && !is_target,
                Some(c) => {
                    (points_here && fk.ref_columns.iter().any(|r| eq(r, c)))
                        || (is_target && fk.columns.iter().any(|r| eq(r, c)))
                }
            };
            if hit {
                let to = display_name(fk.ref_schema.as_deref(), &fk.ref_table);
                let name = fk
                    .name
                    .as_deref()
                    .map(|n| format!("{n} "))
                    .unwrap_or_default();
                out.push(holder(
                    Relation::ForeignKey,
                    format!(
                        "{name}({}) → {to} ({})",
                        fk.columns.join(", "),
                        fk.ref_columns.join(", ")
                    ),
                ));
            }
        }
        let Some(c) = col.filter(|_| is_target) else {
            continue;
        };
        for ix in &table.indexes {
            let filter_hit = ix.filter.as_deref().is_some_and(|f| names_word(f, c));
            if ix
                .columns
                .iter()
                .chain(&ix.include)
                .any(|k| eq(strip_order(k), c))
                || filter_hit
            {
                out.push(holder(
                    Relation::Index,
                    format!("{} ({})", ix.name, ix.columns.join(", ")),
                ));
            }
        }
        if let Some(pk) = table
            .primary_key
            .as_ref()
            .filter(|pk| pk.columns.iter().any(|k| eq(k, c)))
        {
            let name = pk.name.clone().unwrap_or_else(|| "PRIMARY KEY".into());
            out.push(holder(
                Relation::Index,
                format!("{name} ({})", pk.columns.join(", ")),
            ));
        }
        for ck in table
            .checks
            .iter()
            .filter(|ck| names_word(&ck.expression, c))
        {
            let name = ck
                .name
                .as_deref()
                .map(|n| format!("{n}: "))
                .unwrap_or_default();
            out.push(holder(Relation::Check, format!("{name}{}", ck.expression)));
        }
    }
    out
}

/// Where `body` names the target, and how sure that is; `None` when it
/// doesn't. A column counts only in a body that also names its table (a
/// column called `id` is in every routine). A name qualified with another
/// schema (`ventas.Clientes` for `dbo.Clientes`) doesn't count.
pub fn find_mentions(
    body: &str,
    dialect: &ScriptDialect,
    target: &DependencyTarget,
) -> Option<(Confidence, Vec<Mention>)> {
    let toks = code_tokens(body, dialect);
    let t = &target.object;
    // A schema counts only as a qualifier (`ventas.x`), not a column called that.
    let schema = t.kind == SCHEMA;
    let mut table_code = false;
    let mut table_string = false;
    let mut hits: Vec<(usize, bool)> = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        match tok.kind {
            TokenKind::Name
                if eq(field(tok.text), &t.name)
                    && !at_variable(&toks, i, dialect)
                    && qualifier_ok(&toks, i, t.schema())
                    && (!schema || qualifies_next(&toks, i)) =>
            {
                table_code = true;
                if target.column.is_none() {
                    hits.push((tok.start, false));
                }
            }
            TokenKind::Name
                if target
                    .column
                    .as_deref()
                    .is_some_and(|c| eq(field(tok.text), c))
                    && !at_variable(&toks, i, dialect)
                    && !qualifies(&toks, i, &t.name) =>
            {
                hits.push((tok.start, false));
            }
            TokenKind::String => {
                if names_word(tok.text, &t.name) {
                    table_string = true;
                    if target.column.is_none() {
                        hits.push((tok.start, true));
                    }
                }
                if target
                    .column
                    .as_deref()
                    .is_some_and(|c| names_word(tok.text, c))
                {
                    hits.push((tok.start, true));
                }
            }
            _ => {}
        }
    }
    if hits.is_empty() || !(table_code || table_string) {
        return None;
    }
    let code = hits.iter().any(|&(_, dynamic)| !dynamic) && table_code;
    let confidence = if code {
        Confidence::Probable
    } else {
        Confidence::Review
    };
    Some((confidence, mentions(body, &hits)))
}

/// One mention per line, at most [`MAX_MENTIONS`].
fn mentions(body: &str, hits: &[(usize, bool)]) -> Vec<Mention> {
    let mut out: Vec<Mention> = Vec::new();
    let mut line = 1u32;
    let mut at = 0usize;
    for &(start, dynamic) in hits {
        line += body[at..start].bytes().filter(|&b| b == b'\n').count() as u32;
        at = start;
        if let Some(last) = out.last_mut().filter(|m| m.line == line) {
            last.dynamic &= dynamic;
            continue;
        }
        if out.len() == MAX_MENTIONS {
            break;
        }
        let from = body[..start].rfind('\n').map_or(0, |p| p + 1);
        let to = body[start..].find('\n').map_or(body.len(), |p| start + p);
        out.push(Mention {
            line,
            text: body[from..to].trim().chars().take(LINE_CHARS).collect(),
            dynamic,
        });
    }
    out
}

/// `@name` is a variable in T-SQL and MySQL (`@ids`, `@@ROWCOUNT`), never
/// an object (elsewhere `@` may name one: a Snowflake stage).
fn at_variable(toks: &[NameToken<'_>], i: usize, d: &ScriptDialect) -> bool {
    (d.tsql_blocks || d.backtick_idents)
        && i > 0
        && toks[i - 1].kind == TokenKind::Punct
        && toks[i - 1].text == "@"
        && toks[i - 1].end == toks[i].start
}

/// `name@link`: an object on another database (an Oracle database link),
/// not the local one of that name.
fn remote_object(toks: &[NameToken<'_>], j: usize) -> bool {
    toks.get(j + 1)
        .is_some_and(|t| t.kind == TokenKind::Punct && t.text == "@" && t.start == toks[j].end)
}

/// `x.` follows the name at `i`, then a name.
fn qualifies_next(toks: &[NameToken<'_>], i: usize) -> bool {
    toks.get(i + 1)
        .is_some_and(|t| t.kind == TokenKind::Punct && t.text == ".")
        && toks.get(i + 2).is_some_and(|t| t.kind == TokenKind::Name)
}

/// The name at `i` isn't qualified (`x.name`, `a.b.name`) with a schema
/// other than `schema`. Only judged when the target has a schema.
pub(crate) fn qualifier_ok(toks: &[NameToken<'_>], i: usize, schema: Option<&str>) -> bool {
    let Some(schema) = schema else { return true };
    let mut quals = Vec::new();
    let mut k = i;
    while k >= 2
        && toks[k - 1].kind == TokenKind::Punct
        && toks[k - 1].text == "."
        && toks[k - 2].kind == TokenKind::Name
    {
        quals.push(toks[k - 2].text.to_string());
        k -= 2;
    }
    match_qualifiers(&quals, schema) != Qualified::Other
}

/// How the qualifiers written before a name compare with a schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qualified {
    /// None written.
    None,
    /// The schema: its last parts (`dfs.tmp.v`, `` `dfs`.`tmp`.v ``,
    /// `` `dfs.tmp`.v `` for `dfs.tmp`), maybe after a database or catalog.
    Same,
    /// Only the schema's last parts (`tmp.v` for `dfs.tmp`): the schema
    /// through a default the text doesn't show, maybe.
    Partial,
    Other,
}

/// `quals`, the qualifiers before a name nearest first (`["tmp", "dfs"]`
/// for `dfs.tmp.v`), against `schema`, which may have dots of its own
/// (Drill's `dfs.tmp`, Dremio's `espacio.carpeta`). A quoted part may hold
/// the dots too: parts are joined with `.` before comparing.
pub fn match_qualifiers(quals: &[String], schema: &str) -> Qualified {
    if quals.is_empty() {
        return Qualified::None;
    }
    let mut joined = String::new();
    for q in quals {
        joined = if joined.is_empty() {
            q.clone()
        } else {
            format!("{q}.{joined}")
        };
        if eq(&joined, schema) {
            return Qualified::Same;
        }
    }
    if schema
        .to_lowercase()
        .ends_with(&format!(".{}", joined.to_lowercase()))
    {
        Qualified::Partial
    } else {
        Qualified::Other
    }
}

/// The name at `i` isn't a column: it qualifies the next one
/// (`alias.col`), or it's an alias (`AS x`, `Clientes x`).
pub(crate) fn qualifies(toks: &[NameToken<'_>], i: usize, table: &str) -> bool {
    let dot_after = toks
        .get(i + 1)
        .is_some_and(|t| t.kind == TokenKind::Punct && t.text == ".")
        && toks.get(i + 2).is_some_and(|t| t.kind == TokenKind::Name);
    let alias = i
        .checked_sub(1)
        .map(|k| &toks[k])
        .is_some_and(|p| p.kind == TokenKind::Name && (eq(p.text, "as") || eq(p.text, table)));
    dot_after || alias
}

/// `word` shows in `text` as a whole word (an identifier inside a string or
/// an expression), ignoring case.
pub(crate) fn names_word(text: &str, word: &str) -> bool {
    let (t, w) = (text.to_lowercase(), word.to_lowercase());
    let ident = |c: Option<char>| {
        c.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '@' || c == '#' || c == '$')
    };
    t.match_indices(&w)
        .any(|(p, _)| !ident(t[..p].chars().next_back()) && !ident(t[p + w.len()..].chars().next()))
}

/// A field reference in a pipeline (`$total`) is the field.
fn field(name: &str) -> &str {
    name.strip_prefix('$').unwrap_or(name)
}

/// An index key as the catalog spells it (`total DESC`).
fn strip_order(key: &str) -> &str {
    let k = key.trim();
    k.strip_suffix(" DESC")
        .or_else(|| k.strip_suffix(" ASC"))
        .or_else(|| k.strip_suffix(" desc"))
        .or_else(|| k.strip_suffix(" asc"))
        .unwrap_or(k)
}

fn names_table(schema: Option<&str>, name: &str, t: &ObjectRef) -> bool {
    eq(name, &t.name)
        && (schema.is_none()
            || t.schema().is_none()
            || eq(schema.unwrap_or_default(), t.schema().unwrap_or_default()))
}

fn same_object(a: &ObjectRef, b: &ObjectRef) -> bool {
    a.kind == b.kind && names_table(a.schema(), &a.name, b)
}

pub(crate) fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
}

fn display_name(schema: Option<&str>, name: &str) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    }
}

/// Confirmed first, then by relation and name.
fn sort(items: &mut [Dependent]) {
    items.sort_by(|a, b| {
        (
            a.confidence,
            a.relation,
            a.schema.as_deref().unwrap_or(""),
            a.name.to_lowercase(),
        )
            .cmp(&(
                b.confidence,
                b.relation,
                b.schema.as_deref().unwrap_or(""),
                b.name.to_lowercase(),
            ))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{CheckDef, ForeignKeyDef, IndexDef, KeyDef};

    // -- references ------------------------------------------------------------------------

    const PROC: &str = "CREATE PROCEDURE dbo.sp_cargar AS
BEGIN
  INSERT INTO indicadores.medidas (id, valor)
  SELECT p.id, sum(p.valor) FROM dbo.Pedidos p, ventas.facturas f
    JOIN [dbo].[Clientes] c ON c.id = p.cliente_id
  WHERE p.fecha > getdate()
  EXEC dbo.sp_log 'ok'
  SELECT dbo.fn_total(1)
  -- FROM comentario
  EXEC('SELECT * FROM dinamica')
END";

    fn obj(kind: &str, schema: &str, name: &str) -> DbObject {
        DbObject {
            kind: kind.into(),
            schema: Some(schema.into()),
            name: name.into(),
            parent: None,
        }
    }

    fn objects() -> Vec<DbObject> {
        vec![
            obj("table", "indicadores", "medidas"),
            obj("table", "dbo", "Pedidos"),
            obj("table", "dbo", "Clientes"),
            obj("table", "ventas", "Clientes"),
            obj("table", "ventas", "facturas"),
            obj("procedure", "dbo", "sp_log"),
            obj("procedure", "dbo", "sp_cargar"),
            obj("function", "dbo", "fn_total"),
            obj("table", "dbo", "p"),
        ]
    }

    #[test]
    fn the_names_a_body_uses_as_objects() {
        let used = used_names(PROC, &ScriptDialect::tsql());
        let got: Vec<String> = used
            .iter()
            .map(|u| {
                let q: Vec<&str> = u.quals.iter().rev().map(String::as_str).collect();
                format!(
                    "{}{}{}",
                    q.join("."),
                    if q.is_empty() { "" } else { "." },
                    u.name
                )
            })
            .collect();
        // Not the aliases, the columns after ON, sum() or getdate(), the
        // comment, the dynamic SQL in the string, nor the procedure itself.
        assert_eq!(
            got,
            [
                "indicadores.medidas",
                "dbo.Pedidos",
                "ventas.facturas",
                "dbo.Clientes",
                "dbo.sp_log",
                "dbo.fn_total"
            ]
        );
        assert!(used.iter().find(|u| u.name == "fn_total").unwrap().call);
    }

    #[test]
    fn a_nested_join_and_a_table_named_like_a_function() {
        // How MySQL stores a view: the first table inside `from (`.
        let body = "select `a`.`id` from (`ventas`.`a` join `ventas`.`b` on((`a`.`id` = `b`.`id`))) join log l";
        let names: Vec<String> = used_names(body, &ScriptDialect::mysql())
            .into_iter()
            .map(|u| u.name)
            .collect();
        assert_eq!(names, ["a", "b", "log"]);
        // A subquery isn't a table, and log() is the built-in.
        let names: Vec<String> =
            used_names("select log(x) from (select 1) s", &ScriptDialect::generic())
                .into_iter()
                .map(|u| u.name)
                .collect();
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    fn variables_and_remote_objects_are_not_local_objects() {
        // T-SQL: a table variable and @@ROWCOUNT aren't the table dbo.ids.
        let body = "DECLARE @ids TABLE (id int); SELECT * FROM @ids i JOIN dbo.ids d ON d.id = i.id; SELECT @@ROWCOUNT";
        let names: Vec<String> = used_names(body, &ScriptDialect::tsql())
            .into_iter()
            .map(|u| u.name)
            .collect();
        assert_eq!(names, ["ids"]);
        let target = DependencyTarget {
            object: ObjectRef {
                kind: "table".into(),
                schema: Some("dbo".into()),
                name: "ids".into(),
            },
            column: None,
        };
        assert!(find_mentions(
            "DECLARE @ids int; SELECT @ids",
            &ScriptDialect::tsql(),
            &target
        )
        .is_none());
        let (_, m) = find_mentions(body, &ScriptDialect::tsql(), &target).unwrap();
        assert_eq!(m.len(), 1);
        // MySQL user variables likewise.
        assert!(
            used_names("SELECT * FROM t WHERE x = @t", &ScriptDialect::mysql())
                .iter()
                .all(|u| u.name == "t" && u.at == 14)
        );
        // Oracle: emp@remote is on another database.
        let names: Vec<String> = used_names(
            "SELECT * FROM emp@remote e, dept d",
            &ScriptDialect::oracle(),
        )
        .into_iter()
        .map(|u| u.name)
        .collect();
        assert_eq!(names, ["dept"]);
    }

    #[test]
    fn used_names_resolve_to_objects_with_their_lines() {
        let target = ObjectRef {
            kind: "procedure".into(),
            schema: Some("dbo".into()),
            name: "sp_cargar".into(),
        };
        let used = used_names(PROC, &ScriptDialect::tsql());
        let got = used_objects(PROC, &used, &objects(), &target);
        let names: Vec<String> = got
            .iter()
            .map(|d| {
                format!(
                    "{} {}.{} {:?}",
                    d.kind,
                    d.schema.as_deref().unwrap(),
                    d.name,
                    d.mentions.iter().map(|m| m.line).collect::<Vec<_>>()
                )
            })
            .collect();
        // dbo.Clientes, not ventas.Clientes: the name is qualified.
        assert_eq!(
            names,
            [
                "table indicadores.medidas [3]",
                "table dbo.Pedidos [4]",
                "table ventas.facturas [4]",
                "table dbo.Clientes [5]",
                "procedure dbo.sp_log [7]",
                "function dbo.fn_total [8]",
            ]
        );
        assert!(got
            .iter()
            .all(|d| d.relation == Relation::Code && d.confidence == Confidence::Probable));
    }

    #[test]
    fn an_unqualified_name_takes_the_targets_schema_and_a_call_takes_the_routine() {
        let target = ObjectRef {
            kind: "view".into(),
            schema: Some("ventas".into()),
            name: "v".into(),
        };
        let body = "CREATE VIEW ventas.v AS SELECT x FROM Clientes WHERE total(1) > 0";
        let mut objs = objects();
        objs.push(obj("table", "dbo", "total"));
        objs.push(obj("function", "ventas", "total"));
        let used = used_names(body, &ScriptDialect::tsql());
        let got = used_objects(body, &used, &objs, &target);
        let names: Vec<String> = got
            .iter()
            .map(|d| format!("{} {}.{}", d.kind, d.schema.as_deref().unwrap(), d.name))
            .collect();
        assert_eq!(names, ["table ventas.Clientes", "function ventas.total"]);
    }

    /// A session with fixed objects and bodies; with `lookups`, it answers
    /// `list_objects_page` by name and counts the calls.
    struct Fake {
        objects: Vec<DbObject>,
        tables: Vec<TableSchema>,
        lookups: Option<u32>,
        whole_lists: u32,
    }

    #[async_trait::async_trait]
    impl Session for Fake {
        async fn server_version(&mut self) -> Result<String> {
            Ok(String::new())
        }
        async fn list_databases(&mut self) -> Result<Vec<String>> {
            Ok(vec![])
        }
        async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
            self.whole_lists += 1;
            Ok(self.objects.clone())
        }
        async fn list_objects_page(&mut self, q: &crate::ObjectQuery) -> Result<crate::ObjectPage> {
            let Some(n) = self.lookups.as_mut() else {
                return Err(crate::Error::Unsupported("no".into()));
            };
            *n += 1;
            let name = q.name.clone().unwrap_or_default();
            Ok(crate::ObjectPage {
                objects: self
                    .objects
                    .iter()
                    .filter(|o| eq(&o.name, &name))
                    .cloned()
                    .collect(),
                next: None,
            })
        }
        async fn columns(&mut self, _: &ObjectRef) -> Result<Vec<crate::ColumnInfo>> {
            Ok(vec![])
        }
        async fn definition(&mut self, o: &ObjectRef) -> Result<Option<String>> {
            Ok((o.name == "sp_cargar").then(|| PROC.to_string()))
        }
        async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
            Ok(self.tables.clone())
        }
        fn browse_query(&self, _: &ObjectRef, _: u32) -> String {
            String::new()
        }
        async fn execute(&mut self, _: &str, _: usize, _: &mut crate::QueryOutcome) -> Result<()> {
            Ok(())
        }
    }

    fn ready<T>(f: impl std::future::Future<Output = T>) -> T {
        let mut f = std::pin::pin!(f);
        match f
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        {
            std::task::Poll::Ready(v) => v,
            std::task::Poll::Pending => panic!("waited"),
        }
    }

    fn ctx() -> DependencyScan {
        DependencyScan {
            source_kinds: vec!["procedure".into(), "view".into()],
            dialect: ScriptDialect::tsql(),
            foreign_keys: true,
            written_kinds: vec!["procedure".into(), "view".into()],
            has_schemas: true,
        }
    }

    #[test]
    fn references_of_a_procedure_by_lookup_or_by_the_whole_list() {
        let target = DependencyTarget {
            object: ObjectRef {
                kind: "procedure".into(),
                schema: Some("dbo".into()),
                name: "sp_cargar".into(),
            },
            column: None,
        };
        // Lookups by name: one per distinct name, never the whole list.
        let mut s = Fake {
            objects: objects(),
            tables: vec![],
            lookups: Some(0),
            whole_lists: 0,
        };
        let r = ready(references_scan(&mut s, &target, &ctx())).unwrap();
        assert_eq!(r.items.len(), 6);
        assert_eq!(r.scanned, 1);
        assert_eq!((s.lookups, s.whole_lists), (Some(6), 0));
        // No lookup by name: the whole list, once.
        let mut s = Fake {
            objects: objects(),
            tables: vec![],
            lookups: None,
            whole_lists: 0,
        };
        let r = ready(references_scan(&mut s, &target, &ctx())).unwrap();
        assert_eq!(r.items.len(), 6);
        assert_eq!(s.whole_lists, 1);
    }

    #[test]
    fn a_tables_references_are_its_foreign_keys_and_its_ddl_is_not_read() {
        let mut s = Fake {
            objects: objects(),
            tables: schema(),
            lookups: Some(0),
            whole_lists: 0,
        };
        let r = ready(references_scan(&mut s, &table("dbo", "Pedidos"), &ctx())).unwrap();
        assert_eq!(r.scanned, 0);
        assert_eq!(r.items.len(), 1);
        assert_eq!(
            (
                r.items[0].name.as_str(),
                r.items[0].relation,
                r.items[0].confidence
            ),
            ("Clientes", Relation::ForeignKey, Confidence::Confirmed)
        );
        assert!(r.items[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("→ dbo.Clientes"));
    }

    fn table(schema: &str, name: &str) -> DependencyTarget {
        DependencyTarget {
            object: ObjectRef {
                kind: kinds::TABLE.into(),
                schema: Some(schema.into()),
                name: name.into(),
            },
            column: None,
        }
    }

    fn column(schema: &str, name: &str, col: &str) -> DependencyTarget {
        DependencyTarget {
            column: Some(col.into()),
            ..table(schema, name)
        }
    }

    fn tsql() -> ScriptDialect {
        ScriptDialect::for_hint("mssql")
    }

    #[test]
    fn a_view_on_the_table_is_probable() {
        let body = "CREATE VIEW dbo.v AS\nSELECT c.Pepe, c.id\nFROM dbo.Clientes c";
        let (conf, m) = find_mentions(body, &tsql(), &column("dbo", "Clientes", "Pepe")).unwrap();
        assert_eq!(conf, Confidence::Probable);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].line, 2);
        assert_eq!(m[0].text, "SELECT c.Pepe, c.id");
    }

    #[test]
    fn a_column_without_its_table_does_not_count() {
        let body = "SELECT Pepe FROM dbo.Pedidos";
        assert!(find_mentions(body, &tsql(), &column("dbo", "Clientes", "Pepe")).is_none());
    }

    #[test]
    fn comments_and_other_schemas_do_not_count() {
        let body = "-- reads dbo.Clientes\nSELECT * FROM ventas.Clientes /* Clientes */";
        assert!(find_mentions(body, &tsql(), &table("dbo", "Clientes")).is_none());
    }

    #[test]
    fn dynamic_sql_asks_for_review() {
        let body = "EXEC sp_executesql N'SELECT Pepe FROM dbo.Clientes'";
        let (conf, m) = find_mentions(body, &tsql(), &column("dbo", "Clientes", "Pepe")).unwrap();
        assert_eq!(conf, Confidence::Review);
        assert!(m[0].dynamic);
    }

    #[test]
    fn quoted_names_and_partial_words() {
        let body = "SELECT [Pepe], PepeViejo FROM [dbo].[Clientes]";
        let (conf, m) = find_mentions(body, &tsql(), &column("dbo", "Clientes", "pepe")).unwrap();
        assert_eq!(conf, Confidence::Probable);
        assert_eq!(m.len(), 1);
        assert!(find_mentions(
            "SELECT PepeViejo FROM Clientes",
            &tsql(),
            &column("dbo", "Clientes", "Pepe")
        )
        .is_none());
    }

    #[test]
    fn an_alias_is_not_the_column() {
        let body = "SELECT pepe.total FROM dbo.Clientes pepe";
        assert!(find_mentions(body, &tsql(), &column("dbo", "Clientes", "Pepe")).is_none());
    }

    #[test]
    fn postgres_routine_bodies_are_code() {
        let pg = ScriptDialect::for_hint("postgres");
        let body = "CREATE FUNCTION f() RETURNS int AS $$ SELECT count(*) FROM public.clientes $$ LANGUAGE sql";
        let (conf, _) = find_mentions(body, &pg, &table("public", "clientes")).unwrap();
        assert_eq!(conf, Confidence::Probable);
        // Dynamic SQL inside plpgsql is still a string.
        let dynamic = "CREATE FUNCTION g() RETURNS void LANGUAGE plpgsql AS $$ BEGIN EXECUTE 'DELETE FROM public.clientes'; END $$";
        let (conf, m) = find_mentions(dynamic, &pg, &table("public", "clientes")).unwrap();
        assert_eq!(conf, Confidence::Review);
        assert!(m[0].dynamic);
        // A dollar string that isn't a routine body stays a string.
        let (conf, _) = find_mentions(
            "SELECT $$ public.clientes $$",
            &pg,
            &table("public", "clientes"),
        )
        .unwrap();
        assert_eq!(conf, Confidence::Review);
    }

    #[test]
    fn a_schema_counts_only_as_a_qualifier() {
        let target = DependencyTarget {
            object: ObjectRef {
                kind: SCHEMA.into(),
                schema: None,
                name: "ventas".into(),
            },
            column: None,
        };
        assert!(find_mentions("SELECT ventas FROM dbo.t", &tsql(), &target).is_none());
        assert!(find_mentions("SELECT x FROM ventas.t", &tsql(), &target).is_some());
        assert!(schema_dependents(
            &schema(),
            &DependencyTarget {
                object: ObjectRef {
                    kind: SCHEMA.into(),
                    schema: None,
                    name: "Clientes".into()
                },
                column: None
            }
        )
        .is_empty());
    }

    #[test]
    fn dotted_schemas_match_consecutive_qualifiers() {
        let g = ScriptDialect::generic();
        let target = || table("dfs.tmp", "v");
        for body in [
            "SELECT * FROM dfs.tmp.v",
            "SELECT * FROM `dfs`.`tmp`.`v`",
            "SELECT * FROM `dfs.tmp`.v",
            "SELECT * FROM v",
            "SELECT * FROM tmp.v",
        ] {
            assert!(find_mentions(body, &g, &target()).is_some(), "{body}");
        }
        for body in [
            "SELECT * FROM dfs.otro.v",
            "SELECT * FROM otro.v",
            "SELECT * FROM `cp`.`tmp2`.v",
        ] {
            assert!(find_mentions(body, &g, &target()).is_none(), "{body}");
        }
        let pg = ScriptDialect::postgres();
        assert!(find_mentions(
            "SELECT * FROM \"esp\".\"carpeta\".\"v\"",
            &pg,
            &table("esp.carpeta", "v")
        )
        .is_some());
        assert!(find_mentions("SELECT * FROM esp.v", &pg, &table("esp.carpeta", "v")).is_none());
        let q = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            match_qualifiers(&q(&["tmp", "dfs"]), "dfs.tmp"),
            Qualified::Same
        );
        assert_eq!(match_qualifiers(&q(&["dbo", "db"]), "dbo"), Qualified::Same);
        assert_eq!(
            match_qualifiers(&q(&["tmp"]), "dfs.tmp"),
            Qualified::Partial
        );
        assert_eq!(match_qualifiers(&q(&["dfs"]), "dfs.tmp"), Qualified::Other);
        assert_eq!(match_qualifiers(&[], "dfs.tmp"), Qualified::None);
    }

    #[test]
    fn pipeline_fields() {
        let body = r#"[{"$lookup": {"from": "clientes"}}, {"$project": {"total": "$pepe"}}]"#;
        let target = DependencyTarget {
            object: ObjectRef {
                kind: kinds::COLLECTION.into(),
                schema: None,
                name: "clientes".into(),
            },
            column: Some("pepe".into()),
        };
        assert!(find_mentions(body, &ScriptDialect::generic(), &target).is_some());
    }

    #[test]
    fn many_mentions_on_a_line_are_one() {
        let body = "SELECT Pepe, Pepe + 1 FROM Clientes\nWHERE Pepe > 0";
        let (_, m) = find_mentions(body, &tsql(), &column("dbo", "Clientes", "Pepe")).unwrap();
        assert_eq!(m.iter().map(|m| m.line).collect::<Vec<_>>(), vec![1, 2]);
    }

    fn schema() -> Vec<TableSchema> {
        vec![
            TableSchema {
                schema: Some("dbo".into()),
                name: "Clientes".into(),
                primary_key: Some(KeyDef {
                    name: Some("PK_Clientes".into()),
                    columns: vec!["id".into()],
                }),
                indexes: vec![IndexDef {
                    name: "IX_Pepe".into(),
                    columns: vec!["Pepe DESC".into()],
                    ..Default::default()
                }],
                checks: vec![CheckDef {
                    name: Some("CK_Pepe".into()),
                    expression: "([Pepe]>(0))".into(),
                }],
                ..Default::default()
            },
            TableSchema {
                schema: Some("dbo".into()),
                name: "Pedidos".into(),
                foreign_keys: vec![ForeignKeyDef {
                    name: Some("FK_Pedidos_Clientes".into()),
                    columns: vec!["cliente_id".into()],
                    ref_schema: Some("dbo".into()),
                    ref_table: "Clientes".into(),
                    ref_columns: vec!["id".into()],
                    ..Default::default()
                }],
                ..Default::default()
            },
        ]
    }

    #[test]
    fn foreign_keys_into_the_table() {
        let deps = schema_dependents(&schema(), &table("dbo", "Clientes"));
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "Pedidos");
        assert_eq!(
            deps[0].detail.as_deref(),
            Some("FK_Pedidos_Clientes (cliente_id) → dbo.Clientes (id)")
        );
    }

    #[test]
    fn a_columns_keys_indexes_and_checks() {
        let pepe: Vec<_> = schema_dependents(&schema(), &column("dbo", "Clientes", "Pepe"))
            .into_iter()
            .map(|d| d.relation)
            .collect();
        assert_eq!(pepe, vec![Relation::Index, Relation::Check]);
        let id: Vec<_> = schema_dependents(&schema(), &column("dbo", "Clientes", "id"))
            .into_iter()
            .map(|d| (d.name, d.relation))
            .collect();
        assert_eq!(
            id,
            vec![
                ("Clientes".into(), Relation::Index),
                ("Pedidos".into(), Relation::ForeignKey)
            ]
        );
    }
}
