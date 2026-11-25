//! What depends on an object (`Session::dependents`), from `system.tables`
//! filtered by the target: one read of the server's tables that return only
//! the rows that can matter, not `database_schema()` plus one `SHOW CREATE`
//! per view.
//!
//! The query brings, from the server's own databases:
//!
//! - the target's row, whose `dependencies_*` list the views and
//!   materialized views that read it (Confirmed);
//! - the rows whose `loading_dependencies_*` list the target (a dictionary's
//!   source table, a `dictGet` in a default: Confirmed), on servers that have
//!   those columns;
//! - the rows whose `create_table_query` or `engine_full` contain the
//!   target's name (`positionCaseInsensitive`, server side).
//!
//! Each of those rows is read as `references` reads an object (the same
//! functions: a view's query, a materialized view's `TO` and `DEPENDS ON`,
//! the engine of Distributed, Buffer, Dictionary… tables, a dictionary's
//! `SOURCE`), and kept when it names the target. What the catalog lists is
//! Confirmed; the rest is as the code says (Probable) with its lines. A
//! Distributed table counts when its cluster has this server. Merge tables
//! are found only when their pattern names the table literally.
//!
//! A column is looked for in the views' and materialized views' text (as
//! the contract's scan does), and in the table's sorting, partition,
//! primary and sampling keys and data-skipping indexes.
//!
//! When the server refuses the read, the contract's scan runs and the note
//! says why.

use crate::references::{
    code_refs, dictionary_source, engine_refs, in_databases, mentions_at, sort, view_clauses,
    Found, Wanted, LOCAL_CLUSTER_SQL, LOCAL_DATABASES_SQL,
};
use crate::rename::word_regex;
use crate::{kind_of, text, ClickHouseSession, Flavor, DICTIONARY};
use dbine_driver::dependencies::{
    find_mentions, Confidence, DependencyReport, DependencyScan, DependencyTarget, Dependent,
    Relation,
};
use dbine_driver::{kinds, Result};
use serde_json::Value;

/// The note when the contract's scan answers instead of `system.tables`.
pub(crate) const NOTE_UNREADABLE: &str = "No se pudo leer el catálogo de dependencias de system.tables: se buscó en las definiciones que se pueden leer.";

/// The rows to read: the target's own and those that may name it.
/// `{databases}` the server's databases, `{loading}` the
/// `loading_dependencies_*` match where the server has the columns.
fn rows_sql(databases: &str, loading: bool) -> String {
    let (cols, matching) = if loading {
        (
            "loading_dependencies_database, loading_dependencies_table",
            "\n    OR arrayExists((d, n) -> d = {db:String} AND n = {t:String}, loading_dependencies_database, loading_dependencies_table)",
        )
    } else {
        (
            "[] AS loading_dependencies_database, [] AS loading_dependencies_table",
            "",
        )
    };
    format!(
        "SELECT database, name, engine, engine_full, create_table_query,
       dependencies_database, dependencies_table, {cols}
  FROM system.tables
 WHERE {databases}
   AND ((database = {{db:String}} AND name = {{t:String}}){matching}
    OR positionCaseInsensitive(create_table_query, {{t:String}}) > 0
    OR positionCaseInsensitive(engine_full, {{t:String}}) > 0)"
    )
}

/// A column's key and index membership on its table.
const COLUMN_KEYS_SQL: &str =
    "SELECT is_in_partition_key, is_in_sorting_key, is_in_primary_key, is_in_sampling_key
  FROM system.columns WHERE database = {db:String} AND table = {t:String} AND name = {c:String}";
const COLUMN_INDEXES_SQL: &str = "SELECT name, expr FROM system.data_skipping_indices
 WHERE database = {db:String} AND table = {t:String} AND match(expr, {re:String})";

fn pairs(dbs: Option<&Value>, names: Option<&Value>) -> Vec<(String, String)> {
    let strings = |v: Option<&Value>| -> Vec<String> {
        match v {
            Some(Value::Array(a)) => a.iter().map(text).collect(),
            Some(Value::String(s)) => serde_json::from_str::<Vec<String>>(s).unwrap_or_default(),
            _ => Vec::new(),
        }
    };
    strings(dbs).into_iter().zip(strings(names)).collect()
}

/// An item that reads the target; `cluster` is set for a Distributed table,
/// which counts only when that cluster has this server.
pub(crate) struct User {
    pub item: Dependent,
    pub cluster: Option<String>,
}

/// What the rows of [`rows_sql`] say about who reads the target
/// (`tdb`.`target`): `session_db` is the connection's database (an item in
/// it has no schema).
pub(crate) fn users(
    rows: &[Vec<Value>],
    target: &DependencyTarget,
    tdb: &str,
    session_db: &str,
    scan: &DependencyScan,
    flavor: Flavor,
    scanned: &mut u32,
) -> Vec<User> {
    let t = &target.object;
    let function = t.kind == kinds::FUNCTION;
    let is_target = |db: &str, name: &str| !function && db == tdb && name == t.name;
    let cell = |r: &Vec<Value>, i: usize| r.get(i).map(text).unwrap_or_default();
    // The catalog's list of what reads the target.
    let readers: Vec<(String, String)> = rows
        .iter()
        .find(|r| is_target(&cell(r, 0), &cell(r, 1)))
        .map(|r| pairs(r.get(5), r.get(6)))
        .unwrap_or_default();
    let mut out: Vec<User> = Vec::new();
    for r in rows {
        let (db, name, engine) = (cell(r, 0), cell(r, 1), cell(r, 2));
        if is_target(&db, &name) {
            continue;
        }
        let (engine_full, create) = (cell(r, 3), cell(r, 4));
        let kind = kind_of(&engine, flavor);
        let (dialect, written) = (&scan.dialect, &scan.written_kinds);
        let code = written.iter().any(|k| k == kind)
            && matches!(kind, kinds::VIEW | kinds::MATERIALIZED_VIEW);
        let body = if code { create.as_str() } else { "" };
        let mut found = Found::default();
        if code && !create.is_empty() {
            *scanned += 1;
            code_refs(&create, dialect, &db, &mut found);
        }
        if engine == "MaterializedView" {
            for w in view_clauses(&create, dialect, &db) {
                found.want(w);
            }
        }
        engine_refs(&engine_full, &db, &mut found);
        if kind == DICTIONARY {
            dictionary_source(&create, dialect, &db, &mut found);
        }
        let loading = pairs(r.get(7), r.get(8));
        let confirmed = (!function && readers.iter().any(|(d, n)| *d == db && *n == name))
            || (!function && loading.iter().any(|(d, n)| d == tdb && *n == t.name));
        let names_target = |w: &Wanted| {
            if function {
                w.call && w.name == t.name
            } else {
                w.database == tdb && w.name == t.name
            }
        };
        let mut hit: Option<(Confidence, Option<String>, Vec<usize>)> = None;
        let mut cluster = None;
        for w in found.wanted.iter().filter(|w| names_target(w)) {
            hit = Some((w.confidence, w.detail.clone(), w.at.clone()));
        }
        if hit.is_none() {
            // A Distributed table on a cluster: it reads the target where
            // the cluster is this server.
            if let Some((c, w)) = found.clusters.iter().find(|(_, w)| names_target(w)) {
                hit = Some((w.confidence, w.detail.clone(), w.at.clone()));
                cluster = Some(c.clone());
            }
        }
        if hit.is_none() && !confirmed {
            continue;
        }
        let (mut confidence, detail, at) = hit.unwrap_or((Confidence::Confirmed, None, Vec::new()));
        let mut mentions = mentions_at(body, &at);
        if confirmed {
            confidence = Confidence::Confirmed;
        }
        if target.column.is_some() {
            // A column: only a view's text can name it.
            if !code {
                continue;
            }
            match find_mentions(body, dialect, target) {
                Some((c, m)) => {
                    confidence = c;
                    mentions = m;
                }
                None => continue,
            }
        }
        out.push(User {
            item: Dependent {
                kind: kind.into(),
                schema: (db != session_db).then(|| db.clone()),
                name,
                parent: None,
                relation: Relation::Code,
                confidence,
                detail,
                mentions,
            },
            cluster,
        });
    }
    out
}

/// The keys and indexes of the target's table that hold the column.
fn column_holders(
    keys: &[Vec<Value>],
    indexes: &[Vec<Value>],
    table: &str,
    schema: Option<String>,
) -> Vec<Dependent> {
    let flag = |v: Option<&Value>| match v {
        Some(Value::Bool(b)) => *b,
        Some(v) => matches!(text(v).as_str(), "1" | "true"),
        None => false,
    };
    let holder = |detail: String| Dependent {
        kind: kinds::TABLE.into(),
        schema: schema.clone(),
        name: table.to_string(),
        parent: None,
        relation: Relation::Index,
        confidence: Confidence::Confirmed,
        detail: Some(detail),
        mentions: Vec::new(),
    };
    let mut out = Vec::new();
    if let Some(r) = keys.first() {
        for (i, label) in ["PARTITION BY", "ORDER BY", "PRIMARY KEY", "SAMPLE BY"]
            .into_iter()
            .enumerate()
        {
            if flag(r.get(i)) {
                out.push(holder(label.to_string()));
            }
        }
    }
    for r in indexes {
        out.push(holder(format!(
            "{} ({})",
            r.first().map(text).unwrap_or_default(),
            r.get(1).map(text).unwrap_or_default()
        )));
    }
    out
}

impl ClickHouseSession {
    pub(crate) async fn dependents_of(
        &mut self,
        target: &DependencyTarget,
        scan: &DependencyScan,
    ) -> Result<DependencyReport> {
        match self.dependents_native(target, scan).await {
            Some(report) => Ok(report),
            None => {
                let mut report = dbine_driver::dependencies::scan(self, target, scan).await?;
                report.note = Some(NOTE_UNREADABLE.into());
                Ok(report)
            }
        }
    }

    /// `None` when the server won't answer the catalog read.
    async fn dependents_native(
        &mut self,
        target: &DependencyTarget,
        scan: &DependencyScan,
    ) -> Option<DependencyReport> {
        let t = &target.object;
        let db = t.schema().unwrap_or(&self.database).to_string();
        let local: Vec<String> = match self.rows(LOCAL_DATABASES_SQL, &[]).await {
            Ok(rows) => rows.iter().filter_map(|r| r.first()).map(text).collect(),
            Err(_) => vec![db.clone()],
        };
        let (clause, mut params) = in_databases(&local);
        params.push(("db".into(), db.clone()));
        params.push(("t".into(), t.name.clone()));
        let params: Vec<(&str, &str)> = params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        // Servers without `loading_dependencies_*` (22.8) refuse the first.
        let rows = match self.rows(&rows_sql(&clause, true), &params).await {
            Ok(r) => r,
            Err(_) => self.rows(&rows_sql(&clause, false), &params).await.ok()?,
        };
        if t.kind != kinds::FUNCTION
            && !rows.iter().any(|r| {
                r.first().map(text).as_deref() == Some(db.as_str())
                    && r.get(1).map(text).as_deref() == Some(t.name.as_str())
            })
        {
            // Not in `system.tables` (a name that isn't an object here).
            return None;
        }
        let mut report = DependencyReport::default();
        let found = users(
            &rows,
            target,
            &db,
            &self.database,
            scan,
            self.flavor,
            &mut report.scanned,
        );
        let mut clusters: Vec<(String, bool)> = Vec::new();
        for u in found {
            if let Some(c) = &u.cluster {
                let known = clusters.iter().find(|(n, _)| n == c).map(|(_, l)| *l);
                let local = match known {
                    Some(l) => l,
                    None => {
                        let l = self
                            .rows(LOCAL_CLUSTER_SQL, &[("c", c)])
                            .await
                            .ok()
                            .and_then(|r| r.first().and_then(|r| crate::jobs::num(r.first())))
                            .is_some_and(|n| n > 0.0);
                        clusters.push((c.clone(), l));
                        l
                    }
                };
                if !local {
                    continue;
                }
            }
            report.items.push(u.item);
        }
        if let (Some(c), kinds::TABLE) = (target.column.as_deref(), t.kind.as_str()) {
            let keys = self
                .rows(COLUMN_KEYS_SQL, &[("db", &db), ("t", &t.name), ("c", c)])
                .await
                .unwrap_or_default();
            let re = word_regex(c);
            let indexes = self
                .rows(
                    COLUMN_INDEXES_SQL,
                    &[("db", &db), ("t", &t.name), ("re", &re)],
                )
                .await
                .unwrap_or_default();
            let schema = (db != self.database).then(|| db.clone());
            report
                .items
                .extend(column_holders(&keys, &indexes, &t.name, schema));
        }
        sort(&mut report.items);
        Some(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Value {
        Value::String(v.into())
    }
    fn arr(v: &[&str]) -> Value {
        Value::String(serde_json::to_string(v).unwrap())
    }
    /// `rows_sql`'s row.
    fn row(
        db: &str,
        name: &str,
        engine: &str,
        engine_full: &str,
        create: &str,
        deps: (&[&str], &[&str]),
        loading: (&[&str], &[&str]),
    ) -> Vec<Value> {
        vec![
            s(db),
            s(name),
            s(engine),
            s(engine_full),
            s(create),
            arr(deps.0),
            arr(deps.1),
            arr(loading.0),
            arr(loading.1),
        ]
    }

    fn target(kind: &str, name: &str, column: Option<&str>) -> DependencyTarget {
        DependencyTarget {
            object: dbine_driver::ObjectRef {
                kind: kind.into(),
                schema: None,
                name: name.into(),
            },
            column: column.map(Into::into),
        }
    }

    fn read(rows: &[Vec<Value>], t: &DependencyTarget) -> (Vec<User>, u32) {
        let scan = DependencyScan {
            source_kinds: Vec::new(),
            dialect: crate::dialect(),
            foreign_keys: false,
            written_kinds: vec![
                kinds::VIEW.to_string(),
                kinds::MATERIALIZED_VIEW.to_string(),
            ],
            has_schemas: false,
            databases: None,
        };
        let mut scanned = 0;
        let u = users(rows, t, "d", "d", &scan, Flavor::ClickHouse, &mut scanned);
        (u, scanned)
    }

    fn rows() -> Vec<Vec<Value>> {
        let none: (&[&str], &[&str]) = (&[], &[]);
        vec![
            // The target: a materialized view and a plain view read it.
            row("d", "src", "MergeTree", "MergeTree ORDER BY id", "CREATE TABLE d.src (id UInt64, v String)", (&["d", "d"], &["mv", "vw"]), none),
            row("d", "mv", "MaterializedView", "", "CREATE MATERIALIZED VIEW d.mv TO d.dst AS SELECT id, v FROM d.src", none, none),
            row("d", "vw", "View", "", "CREATE VIEW d.vw AS SELECT id FROM d.src", none, none),
            // Only the text names it (a subquery the catalog doesn't list).
            row("d", "v2", "View", "", "CREATE VIEW d.v2 AS SELECT id FROM d.other WHERE id IN (SELECT id FROM d.src)", none, none),
            // The name inside a longer one, and in another database.
            row("d", "v3", "View", "", "CREATE VIEW d.v3 AS SELECT id FROM d.src2", none, none),
            row("x", "v4", "View", "", "CREATE VIEW x.v4 AS SELECT id FROM d.src", none, none),
            row("x", "v5", "View", "", "CREATE VIEW x.v5 AS SELECT id FROM x.src", none, none),
            row("d", "buf", "Buffer", "Buffer('d', 'src', 1, 10, 100, 10000, 1000000, 10000000, 100000000)", "CREATE TABLE d.buf AS d.src ENGINE = Buffer(...)", none, none),
            row("d", "dist", "Distributed", "Distributed('cl', 'd', 'src', rand())", "CREATE TABLE d.dist AS d.src ENGINE = Distributed(...)", none, none),
            row("d", "dict", "Dictionary", "Dictionary(d.dict)", "CREATE DICTIONARY d.dict (id UInt64) PRIMARY KEY id SOURCE(CLICKHOUSE(TABLE 'src' DB 'd')) LAYOUT(FLAT()) LIFETIME(0)", none, (&["d"], &["src"])),
        ]
    }

    #[test]
    fn what_the_catalog_lists_is_confirmed_and_the_text_is_probable() {
        let (u, scanned) = read(&rows(), &target(kinds::TABLE, "src", None));
        let by = |n: &str, schema: Option<&str>| {
            u.iter()
                .find(|u| u.item.name == n && u.item.schema.as_deref() == schema)
                .map(|u| (u.item.kind.as_str(), u.item.confidence))
        };
        assert_eq!(
            by("mv", None),
            Some((kinds::MATERIALIZED_VIEW, Confidence::Confirmed))
        );
        assert_eq!(by("vw", None), Some((kinds::VIEW, Confidence::Confirmed)));
        assert_eq!(by("v2", None), Some((kinds::VIEW, Confidence::Probable)));
        assert_eq!(
            by("v4", Some("x")),
            Some((kinds::VIEW, Confidence::Probable))
        );
        assert_eq!(by("buf", None), Some((kinds::TABLE, Confidence::Confirmed)));
        assert_eq!(by("dict", None), Some((DICTIONARY, Confidence::Confirmed)));
        // `src2` and another database's `src` aren't the target.
        assert!(by("v3", None).is_none());
        assert!(by("v5", Some("x")).is_none());
        // The target isn't its own dependent.
        assert!(by("src", None).is_none());
        assert_eq!(scanned, 6);
        let v2 = u.iter().find(|u| u.item.name == "v2").unwrap();
        assert_eq!(v2.item.mentions.len(), 1);
    }

    #[test]
    fn a_distributed_table_waits_for_its_cluster() {
        let (u, _) = read(&rows(), &target(kinds::TABLE, "src", None));
        let dist = u.iter().find(|u| u.item.name == "dist").unwrap();
        assert_eq!(dist.cluster.as_deref(), Some("cl"));
        assert!(u.iter().filter(|u| u.cluster.is_some()).count() == 1);
    }

    #[test]
    fn a_column_is_named_by_a_views_text() {
        let mut rs = rows();
        rs[1][4] = s("CREATE MATERIALIZED VIEW d.mv TO d.dst AS SELECT id, v FROM d.src");
        let (u, _) = read(&rs, &target(kinds::TABLE, "src", Some("v")));
        let names: Vec<_> = u.iter().map(|u| u.item.name.as_str()).collect();
        // `v` is in the materialized view's text; the view `vw` names only `id`.
        assert_eq!(names, ["mv"]);
        assert_eq!(u[0].item.confidence, Confidence::Probable);
    }

    #[test]
    fn a_function_is_found_where_it_is_called() {
        let none: (&[&str], &[&str]) = (&[], &[]);
        let rs = vec![
            row(
                "d",
                "va",
                "View",
                "",
                "CREATE VIEW d.va AS SELECT dbine_f(id) FROM d.t",
                none,
                none,
            ),
            row(
                "d",
                "vb",
                "View",
                "",
                "CREATE VIEW vb AS SELECT dbine_f FROM d.t",
                none,
                none,
            ),
        ];
        let (u, _) = read(&rs, &target(kinds::FUNCTION, "dbine_f", None));
        let names: Vec<_> = u.iter().map(|u| u.item.name.as_str()).collect();
        assert_eq!(names, ["va"]);
    }

    #[test]
    fn the_query_is_filtered_by_the_target() {
        let sql = rows_sql("database IN ({l0:String})", true);
        assert!(sql.contains("positionCaseInsensitive(create_table_query, {t:String})"));
        assert!(sql.contains("positionCaseInsensitive(engine_full, {t:String})"));
        assert!(sql.contains("loading_dependencies_table)"));
        let old = rows_sql("0", false);
        assert!(old.contains("[] AS loading_dependencies_database"));
        assert!(!old.contains("arrayExists"));
    }

    #[test]
    fn a_column_is_held_by_keys_and_skipping_indexes() {
        let keys = vec![vec![
            Value::from(0),
            Value::from(1),
            Value::from(1),
            Value::from(0),
        ]];
        let ix = vec![vec![s("ix_v"), s("v")]];
        let out = column_holders(&keys, &ix, "src", None);
        let d: Vec<_> = out.iter().map(|d| d.detail.clone().unwrap()).collect();
        assert_eq!(d, ["ORDER BY", "PRIMARY KEY", "ix_v (v)"]);
        assert!(out
            .iter()
            .all(|d| d.relation == Relation::Index && d.name == "src"));
    }
}
