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

use crate::info::DriverInfo;
use crate::model::ObjectRef;
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
    info.object_kinds.iter().filter(|k| k.has_definition && CODE_KINDS.contains(&k.id)).map(|k| k.id.to_string()).collect()
}

impl DependencyScan {
    pub fn new(info: &DriverInfo, dialect: ScriptDialect, foreign_keys: bool) -> Self {
        DependencyScan { source_kinds: code_kinds(info), dialect, foreign_keys }
    }
}

/// The generic scan: foreign keys, indexes and checks from the catalog
/// (when the engine reports them), then every other object with source.
pub async fn scan<S: Session + ?Sized>(s: &mut S, target: &DependencyTarget, ctx: &DependencyScan) -> Result<DependencyReport> {
    let mut report = DependencyReport::default();
    if ctx.foreign_keys {
        report.items.extend(schema_dependents(&s.database_schema().await?, target));
    }
    for o in s.list_objects().await? {
        if !ctx.source_kinds.contains(&o.kind) {
            continue;
        }
        let obj = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
        if same_object(&obj, &target.object) {
            continue;
        }
        let body = match s.definition(&obj).await {
            Ok(Some(b)) => b,
            Ok(None) => continue,
            Err(_) => {
                report.unreadable.push(display_name(o.schema.as_deref(), &o.name));
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
            let points_here = eq(&fk.ref_table, &t.name) && fk.ref_schema.as_deref().is_none_or(|s| t.schema().is_none_or(|ts| eq(s, ts)));
            let hit = match col {
                None => points_here && !is_target,
                Some(c) => (points_here && fk.ref_columns.iter().any(|r| eq(r, c))) || (is_target && fk.columns.iter().any(|r| eq(r, c))),
            };
            if hit {
                let to = display_name(fk.ref_schema.as_deref(), &fk.ref_table);
                let name = fk.name.as_deref().map(|n| format!("{n} ")).unwrap_or_default();
                out.push(holder(Relation::ForeignKey, format!("{name}({}) → {to} ({})", fk.columns.join(", "), fk.ref_columns.join(", "))));
            }
        }
        let Some(c) = col.filter(|_| is_target) else { continue };
        for ix in &table.indexes {
            let filter_hit = ix.filter.as_deref().is_some_and(|f| names_word(f, c));
            if ix.columns.iter().chain(&ix.include).any(|k| eq(strip_order(k), c)) || filter_hit {
                out.push(holder(Relation::Index, format!("{} ({})", ix.name, ix.columns.join(", "))));
            }
        }
        if let Some(pk) = table.primary_key.as_ref().filter(|pk| pk.columns.iter().any(|k| eq(k, c))) {
            let name = pk.name.clone().unwrap_or_else(|| "PRIMARY KEY".into());
            out.push(holder(Relation::Index, format!("{name} ({})", pk.columns.join(", "))));
        }
        for ck in table.checks.iter().filter(|ck| names_word(&ck.expression, c)) {
            let name = ck.name.as_deref().map(|n| format!("{n}: ")).unwrap_or_default();
            out.push(holder(Relation::Check, format!("{name}{}", ck.expression)));
        }
    }
    out
}

/// Where `body` names the target, and how sure that is; `None` when it
/// doesn't. A column counts only in a body that also names its table (a
/// column called `id` is in every routine). A name qualified with another
/// schema (`ventas.Clientes` for `dbo.Clientes`) doesn't count.
pub fn find_mentions(body: &str, dialect: &ScriptDialect, target: &DependencyTarget) -> Option<(Confidence, Vec<Mention>)> {
    let toks = code_tokens(body, dialect);
    let t = &target.object;
    // A schema counts only as a qualifier (`ventas.x`), not a column called that.
    let schema = t.kind == SCHEMA;
    let mut table_code = false;
    let mut table_string = false;
    let mut hits: Vec<(usize, bool)> = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        match tok.kind {
            TokenKind::Name if eq(field(tok.text), &t.name) && qualifier_ok(&toks, i, t.schema()) && (!schema || qualifies_next(&toks, i)) => {
                table_code = true;
                if target.column.is_none() {
                    hits.push((tok.start, false));
                }
            }
            TokenKind::Name if target.column.as_deref().is_some_and(|c| eq(field(tok.text), c)) && !qualifies(&toks, i, &t.name) => {
                hits.push((tok.start, false));
            }
            TokenKind::String => {
                if names_word(tok.text, &t.name) {
                    table_string = true;
                    if target.column.is_none() {
                        hits.push((tok.start, true));
                    }
                }
                if target.column.as_deref().is_some_and(|c| names_word(tok.text, c)) {
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
    let confidence = if code { Confidence::Probable } else { Confidence::Review };
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
        out.push(Mention { line, text: body[from..to].trim().chars().take(LINE_CHARS).collect(), dynamic });
    }
    out
}

/// `x.` follows the name at `i`, then a name.
fn qualifies_next(toks: &[NameToken<'_>], i: usize) -> bool {
    toks.get(i + 1).is_some_and(|t| t.kind == TokenKind::Punct && t.text == ".") && toks.get(i + 2).is_some_and(|t| t.kind == TokenKind::Name)
}

/// The name at `i` is qualified (`x.name`) with a schema other than
/// `schema`. Only judged when the target has a schema.
pub(crate) fn qualifier_ok(toks: &[NameToken<'_>], i: usize, schema: Option<&str>) -> bool {
    let Some(schema) = schema else { return true };
    match (i.checked_sub(2).map(|k| &toks[k]), i.checked_sub(1).map(|k| &toks[k])) {
        (Some(q), Some(dot)) if dot.kind == TokenKind::Punct && dot.text == "." && q.kind == TokenKind::Name => eq(q.text, schema),
        _ => true,
    }
}

/// The name at `i` isn't a column: it qualifies the next one
/// (`alias.col`), or it's an alias (`AS x`, `Clientes x`).
pub(crate) fn qualifies(toks: &[NameToken<'_>], i: usize, table: &str) -> bool {
    let dot_after = toks.get(i + 1).is_some_and(|t| t.kind == TokenKind::Punct && t.text == ".") && toks.get(i + 2).is_some_and(|t| t.kind == TokenKind::Name);
    let alias = i.checked_sub(1).map(|k| &toks[k]).is_some_and(|p| p.kind == TokenKind::Name && (eq(p.text, "as") || eq(p.text, table)));
    dot_after || alias
}

/// `word` shows in `text` as a whole word (an identifier inside a string or
/// an expression), ignoring case.
pub(crate) fn names_word(text: &str, word: &str) -> bool {
    let (t, w) = (text.to_lowercase(), word.to_lowercase());
    let ident = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '@' || c == '#' || c == '$');
    t.match_indices(&w).any(|(p, _)| !ident(t[..p].chars().next_back()) && !ident(t[p + w.len()..].chars().next()))
}

/// A field reference in a pipeline (`$total`) is the field.
fn field(name: &str) -> &str {
    name.strip_prefix('$').unwrap_or(name)
}

/// An index key as the catalog spells it (`total DESC`).
fn strip_order(key: &str) -> &str {
    let k = key.trim();
    k.strip_suffix(" DESC").or_else(|| k.strip_suffix(" ASC")).or_else(|| k.strip_suffix(" desc")).or_else(|| k.strip_suffix(" asc")).unwrap_or(k)
}

fn names_table(schema: Option<&str>, name: &str, t: &ObjectRef) -> bool {
    eq(name, &t.name) && (schema.is_none() || t.schema().is_none() || eq(schema.unwrap_or_default(), t.schema().unwrap_or_default()))
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
        (a.confidence, a.relation, a.schema.as_deref().unwrap_or(""), a.name.to_lowercase()).cmp(&(
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

    fn table(schema: &str, name: &str) -> DependencyTarget {
        DependencyTarget { object: ObjectRef { kind: kinds::TABLE.into(), schema: Some(schema.into()), name: name.into() }, column: None }
    }

    fn column(schema: &str, name: &str, col: &str) -> DependencyTarget {
        DependencyTarget { column: Some(col.into()), ..table(schema, name) }
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
        assert!(find_mentions("SELECT PepeViejo FROM Clientes", &tsql(), &column("dbo", "Clientes", "Pepe")).is_none());
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
        let (conf, _) = find_mentions("SELECT $$ public.clientes $$", &pg, &table("public", "clientes")).unwrap();
        assert_eq!(conf, Confidence::Review);
    }

    #[test]
    fn a_schema_counts_only_as_a_qualifier() {
        let target = DependencyTarget { object: ObjectRef { kind: SCHEMA.into(), schema: None, name: "ventas".into() }, column: None };
        assert!(find_mentions("SELECT ventas FROM dbo.t", &tsql(), &target).is_none());
        assert!(find_mentions("SELECT x FROM ventas.t", &tsql(), &target).is_some());
        assert!(schema_dependents(&schema(), &DependencyTarget { object: ObjectRef { kind: SCHEMA.into(), schema: None, name: "Clientes".into() }, column: None }).is_empty());
    }

    #[test]
    fn pipeline_fields() {
        let body = r#"[{"$lookup": {"from": "clientes"}}, {"$project": {"total": "$pepe"}}]"#;
        let target = DependencyTarget { object: ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: "clientes".into() }, column: Some("pepe".into()) };
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
                primary_key: Some(KeyDef { name: Some("PK_Clientes".into()), columns: vec!["id".into()] }),
                indexes: vec![IndexDef { name: "IX_Pepe".into(), columns: vec!["Pepe DESC".into()], ..Default::default() }],
                checks: vec![CheckDef { name: Some("CK_Pepe".into()), expression: "([Pepe]>(0))".into() }],
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
        assert_eq!(deps[0].detail.as_deref(), Some("FK_Pedidos_Clientes (cliente_id) → dbo.Clientes (id)"));
    }

    #[test]
    fn a_columns_keys_indexes_and_checks() {
        let pepe: Vec<_> = schema_dependents(&schema(), &column("dbo", "Clientes", "Pepe")).into_iter().map(|d| d.relation).collect();
        assert_eq!(pepe, vec![Relation::Index, Relation::Check]);
        let id: Vec<_> = schema_dependents(&schema(), &column("dbo", "Clientes", "id")).into_iter().map(|d| (d.name, d.relation)).collect();
        assert_eq!(id, vec![("Clientes".into(), Relation::Index), ("Pedidos".into(), Relation::ForeignKey)]);
    }
}
