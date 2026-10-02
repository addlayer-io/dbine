//! One live connection. Catalog lookups try `information_schema` first
//! and fall back to `SHOW` statements; a variant that lacks one catalog
//! (routines, triggers, materialized views…) still lists what it has.

use crate::cells::{cell, type_name, value_text};
use crate::plan::{self, Flavor, StmtKind};
use crate::{err, stmt_err, Variant};
use dbine_driver::sql::{leading_keyword, quote_ident, select_top, Limit, Quote};
use dbine_driver::{
    async_trait, kinds, ColumnDef, MonitorSnapshot, ColumnInfo, DbObject, Error, ForeignKeyDef, IndexDef, KeyDef, ObjectRef, Plan,
    Message, MessageLevel, QueryOutcome, ResultColumn, Result, Session, StatementKind, TableSchema, TxState,
};
use mysql_async::consts::StatusFlags;
use std::collections::{BTreeMap, HashMap};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, Row};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct MySqlSession {
    pub(crate) conn: Conn,
    opts: Opts,
    pub(crate) variant: Variant,
    /// The product the user picked (Aurora, Cloud SQL, VeloDB…); `variant`
    /// is the engine it behaves as.
    pub(crate) product: Variant,
    /// Database and table sizes for the monitor, refreshed once a minute.
    pub(crate) sizes: Option<crate::monitor::Sizes>,
    /// The database the session opened, for engines without `DATABASE()`.
    database: Option<String>,
    /// Set by the interrupter: KILL QUERY on `SLEEP()` returns normally.
    cancelled: Arc<AtomicBool>,
    /// The running profiler, if any.
    profiler: Option<crate::profiler::State>,
}

/// A string literal for a text-protocol query. Doubling `'` is safe with
/// and without NO_BACKSLASH_ESCAPES; backslashes are escaped for the
/// default mode.
pub(crate) fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

/// Object kind from `information_schema.TABLES.TABLE_TYPE`; `None` for
/// what the explorer doesn't show (sequences, system views).
pub(crate) fn table_kind(table_type: &str) -> Option<&'static str> {
    let t = table_type.to_ascii_uppercase();
    if t == "VIEW" {
        Some(kinds::VIEW)
    } else if t.contains("VIEW") || t == "SEQUENCE" {
        None
    } else {
        // BASE TABLE, SYSTEM VERSIONED (MariaDB), TABLE, EXTERNAL TABLE…
        Some(kinds::TABLE)
    }
}

/// A cell by position.
pub(crate) fn at(r: &Row, i: usize) -> Option<String> {
    r.as_ref(i).and_then(value_text)
}

/// A cell by column name (any of `names`, case-insensitive).
pub(crate) fn named(r: &Row, names: &[&str]) -> Option<String> {
    let i = r.columns_ref().iter().position(|c| names.iter().any(|n| c.name_str().eq_ignore_ascii_case(n)))?;
    at(r, i)
}

impl MySqlSession {
    pub(crate) fn new(conn: Conn, opts: Opts, product: Variant, database: Option<String>) -> Self {
        Self { conn, opts, variant: product.base(), product, sizes: None, database, cancelled: Arc::default(), profiler: None }
    }

    pub(crate) async fn rows(&mut self, sql: &str) -> Result<Vec<Row>> {
        self.conn.query::<Row, _>(sql).await.map_err(err)
    }

    /// Rows of an optional catalog query: empty when the engine lacks it.
    pub(crate) async fn optional_rows(&mut self, sql: &str) -> Vec<Row> {
        match self.rows(sql).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("{:?}: {sql}: {e}", self.variant);
                Vec::new()
            }
        }
    }

    async fn current_database(&mut self) -> Option<String> {
        match self.rows("SELECT DATABASE()").await {
            Ok(rows) => rows.first().and_then(|r| at(r, 0)).or_else(|| self.database.clone()),
            Err(_) => self.database.clone(),
        }
    }

    /// Tables and views: information_schema, else `SHOW FULL TABLES`,
    /// else `SHOW TABLES` (all tables).
    async fn relations(&mut self, db: &str) -> Result<Vec<DbObject>> {
        let obj = |kind: &str, name: String| DbObject { kind: kind.into(), schema: None, name, parent: None };
        if self.variant != Variant::Manticore {
            let sql = format!(
                "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = {}",
                lit(db)
            );
            match self.rows(&sql).await {
                Ok(rows) if !rows.is_empty() => {
                    return Ok(rows
                        .iter()
                        .filter_map(|r| Some(obj(table_kind(&at(r, 1).unwrap_or_default())?, at(r, 0)?)))
                        .collect())
                }
                Ok(_) => {}
                Err(e) => tracing::debug!("{:?}: information_schema.TABLES: {e}", self.variant),
            }
            match self.rows("SHOW FULL TABLES").await {
                Ok(rows) => {
                    return Ok(rows
                        .iter()
                        .filter_map(|r| Some(obj(table_kind(&at(r, 1).unwrap_or_default())?, at(r, 0)?)))
                        .collect())
                }
                Err(e) => tracing::debug!("{:?}: SHOW FULL TABLES: {e}", self.variant),
            }
        }
        let rows = self.rows("SHOW TABLES").await?;
        Ok(rows.iter().filter_map(|r| Some(obj(kinds::TABLE, at(r, 0)?))).collect())
    }

    async fn columns_from_info_schema(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let schema = match obj.schema() {
            Some(s) => lit(s),
            None => match self.current_database().await {
                Some(db) => lit(&db),
                None => "DATABASE()".into(),
            },
        };
        let sql = format!(
            "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, EXTRA, COLUMN_DEFAULT
             FROM information_schema.COLUMNS
             WHERE TABLE_SCHEMA = {schema} AND TABLE_NAME = {}
             ORDER BY ORDINAL_POSITION",
            lit(&obj.name)
        );
        Ok(self
            .rows(&sql)
            .await?
            .iter()
            .map(|r| {
                let extra = at(r, 4).unwrap_or_default().to_ascii_lowercase();
                ColumnInfo {
                    name: at(r, 0).unwrap_or_default(),
                    data_type: at(r, 1).unwrap_or_default(),
                    nullable: at(r, 2).is_none_or(|n| n.eq_ignore_ascii_case("YES")),
                    primary_key: at(r, 3).as_deref() == Some("PRI"),
                    auto_increment: extra.contains("auto_increment"),
                    default_value: at(r, 5),
                }
            })
            .collect())
    }

    /// `DESCRIBE`: Field/Type/Null/Key/Default/Extra on MySQL; other
    /// engines rename or drop some of them.
    async fn columns_from_describe(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let t = dbine_driver::sql::qualified_name(Quote::Backtick, obj.schema(), &obj.name);
        Ok(self
            .rows(&format!("DESCRIBE {t}"))
            .await?
            .iter()
            .map(|r| {
                let key = named(r, &["Key"]).unwrap_or_default().to_ascii_uppercase();
                let extra = named(r, &["Extra", "Properties"]).unwrap_or_default().to_ascii_lowercase();
                ColumnInfo {
                    name: named(r, &["Field", "Column", "column_name", "Name"]).unwrap_or_default(),
                    data_type: named(r, &["Type", "data_type"]).unwrap_or_default(),
                    nullable: named(r, &["Null", "is_nullable"]).is_none_or(|n| n.eq_ignore_ascii_case("YES")),
                    primary_key: key.starts_with("PRI"),
                    auto_increment: extra.contains("auto_increment"),
                    default_value: named(r, &["Default", "default"]),
                }
            })
            .collect())
    }

    /// `SHOW CREATE <what> <name>`, reading the column that holds the DDL.
    async fn show_create(&mut self, what: &str, obj: &ObjectRef, column: &str) -> Result<Option<String>> {
        let q = dbine_driver::sql::qualified_name(Quote::Backtick, obj.schema(), &obj.name);
        let rows = self.rows(&format!("SHOW CREATE {what} {q}")).await?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        // NULL when the login lacks the privilege to see a routine's body.
        Ok(named(row, &[column]).or_else(|| {
            // Emulations name the column differently; the DDL is the last one
            // that starts with CREATE.
            (0..row.len()).rev().filter_map(|i| at(row, i)).find(|s| s.trim_start().to_uppercase().starts_with("CREATE"))
        }))
    }

    async fn run(&mut self, sql: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let mut result = self.conn.query_iter(sql).await.map_err(|e| stmt_err(sql, e))?;
        // Warning counts of the result sets, in order.
        let mut warned: Vec<u16> = Vec::new();
        // One pass per result set; `next` returns None at each set's end
        // and moves on to the following one.
        while let Some(cols) = result.columns() {
            if cols.is_empty() {
                out.push_affected(result.affected_rows());
                // "Records: 3  Duplicates: 0  Warnings: 0", "Rows matched: …".
                let info = result.info();
                if !info.trim().is_empty() {
                    out.info(info.trim().to_string());
                }
                if let Some(id) = result.last_insert_id().filter(|&id| id > 0) {
                    out.info(format!("Último id generado: {id}"));
                }
            } else {
                out.begin_result(
                    cols.iter()
                        .map(|c| ResultColumn { name: c.name_str().into_owned(), type_name: type_name(c.column_type()) })
                        .collect(),
                );
            }
            while let Some(row) = result.next().await.map_err(|e| stmt_err(sql, e))? {
                let cols = row.columns();
                let cells = cols.iter().zip(row.unwrap()).map(|(c, v)| cell(c, v)).collect();
                out.push_row(cells, max_rows);
            }
            warned.push(result.warnings());
        }
        // `columns()` reads an error in a later statement as "no more
        // results"; this surfaces it (and clears it from the connection).
        result.drop_result().await.map_err(|e| stmt_err(sql, e))?;
        // SHOW WARNINGS lists the last statement's: their text for it, the
        // count for earlier statements of a multi-statement text.
        let last = warned.pop().unwrap_or(0);
        for n in warned {
            if let Some(note) = warnings_note(n) {
                out.warning(note);
            }
        }
        if last > 0 {
            match self.show_warnings().await {
                Some(list) if !list.is_empty() => {
                    for m in list {
                        out.message(m);
                    }
                }
                _ => out.warning(warnings_note(last).unwrap_or_default()),
            }
        }
        Ok(())
    }

    /// `SHOW WARNINGS` as messages (Note as info); `None` when the engine
    /// refuses it.
    async fn show_warnings(&mut self) -> Option<Vec<Message>> {
        let rows = match self.rows("SHOW WARNINGS").await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::debug!("{:?}: SHOW WARNINGS: {e}", self.variant);
                return None;
            }
        };
        Some(
            rows.iter()
                .filter_map(|r| {
                    let text = named(r, &["Message"]).or_else(|| at(r, 2))?;
                    let level = named(r, &["Level"]).or_else(|| at(r, 0)).unwrap_or_default();
                    let level = if level.eq_ignore_ascii_case("note") { MessageLevel::Info } else { MessageLevel::Warning };
                    let code = named(r, &["Code"]).or_else(|| at(r, 1)).filter(|c| !c.is_empty() && c != "0");
                    Some(Message { level, text, code, ..Default::default() })
                })
                .collect(),
        )
    }

    /// `sql`'s statements, or `sql` itself when no word `use` is in it
    /// (no need to split: only USE is looked for).
    fn statements_of(&self, sql: &str) -> Vec<String> {
        if sql.to_ascii_lowercase().contains("use") {
            self.statements(sql)
        } else {
            Vec::new()
        }
    }

    /// Statements of `sql` as the mysql CLI sends them (DELIMITER applied,
    /// client commands left out).
    fn statements(&self, sql: &str) -> Vec<String> {
        let d = crate::script_dialect(self.variant);
        dbine_driver::sql::split_script(sql, &d)
            .into_iter()
            .filter(|u| u.kind != StatementKind::ClientCommand)
            .map(|u| u.text)
            .collect()
    }
}

/// Whether a column type takes unquoted numeric defaults.
fn numeric_type(ty: &str) -> bool {
    let base = ty.trim().to_ascii_lowercase();
    let base = base.split(['(', ' ']).next().unwrap_or("");
    [
        "tinyint", "smallint", "mediumint", "int", "integer", "bigint", "largeint", "decimal", "numeric", "float",
        "double", "real", "year", "boolean", "bool", "int8", "int16", "int32", "int64", "uint8", "uint16", "uint32",
        "uint64", "float32", "float64",
    ]
    .contains(&base)
}

fn is_now(d: &str) -> bool {
    let l = d.to_ascii_lowercase();
    l.starts_with("current_timestamp") || l.starts_with("now(") || l.starts_with("localtimestamp")
}

/// `information_schema.COLUMNS.COLUMN_DEFAULT` as a DEFAULT clause body.
/// MariaDB already gives an SQL expression (`'x'`, `NULL`, `current_timestamp()`);
/// MySQL gives the bare literal, and expressions (EXTRA `DEFAULT_GENERATED`)
/// without their parentheses. `ON UPDATE` from EXTRA rides along.
pub(crate) fn column_default(raw: Option<&str>, extra: &str, ty: &str, maria: bool) -> Option<String> {
    let extra_l = extra.to_ascii_lowercase();
    let d = raw.and_then(|d| {
        if maria {
            return (!d.eq_ignore_ascii_case("NULL")).then(|| d.to_string());
        }
        Some(if is_now(d) {
            d.to_string()
        } else if extra_l.contains("default_generated") {
            // MySQL shows the expression's quotes escaped: concat(_utf8mb4\'a\').
            format!("({})", d.replace("\\'", "'"))
        } else if (d.starts_with("b'") && ty.starts_with("bit")) || numeric_type(ty) {
            d.to_string()
        } else if d.ends_with(')') && d.contains('(') && !d.contains(' ') && !ty.contains("char") && !ty.contains("text") {
            // Function defaults of the emulations (GreptimeDB's current_timestamp()…).
            d.to_string()
        } else {
            lit(d)
        })
    });
    match extra_l.find("on update ") {
        Some(i) => Some(format!("{} ON UPDATE {}", d.as_deref().unwrap_or("NULL"), &extra[i + "on update ".len()..].trim())),
        None => d,
    }
}

/// Manticore's statements: split by the lexer (backslash escapes, `;`
/// inside comments), comments taken out.
fn manticore_statements(sql: &str) -> Vec<String> {
    let d = crate::script_dialect(Variant::Manticore);
    dbine_driver::sql::split_script(sql, &d)
        .into_iter()
        .map(|u| dbine_driver::sql::strip_comments(&u.text, &d, false).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// How long a StarRocks / Doris schema change waits for the table's running one.
const OLAP_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// One schema-change statement (safe to send again: it failed as a whole).
fn olap_schema_change(sql: &str) -> bool {
    let s = sql.trim().trim_end_matches(';').trim();
    let u = s.to_ascii_uppercase();
    !s.contains(';') && (u.starts_with("ALTER TABLE") || u.starts_with("CREATE INDEX") || u.starts_with("DROP INDEX"))
}

/// StarRocks' and Doris' "the table is busy with another schema change".
fn olap_busy(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("schema change operation is in progress") || m.contains("is not normal") || m.contains("do not allow doing alter ops")
}

fn fk_rule(r: Option<String>) -> Option<String> {
    r.filter(|r| !r.eq_ignore_ascii_case("NO ACTION") && !r.eq_ignore_ascii_case("RESTRICT"))
}

impl MySqlSession {
    /// Every table from information_schema: one query per catalog view.
    async fn catalog_schema(&mut self) -> Result<Vec<TableSchema>> {
        let v = self.variant;
        let Some(db) = self.current_database().await else {
            return Ok(Vec::new());
        };
        let dbl = lit(&db);
        let maria = v == Variant::MariaDb
            || (v == Variant::MySql && self.server_version().await.unwrap_or_default().contains("MariaDB"));
        let olap = matches!(v, Variant::StarRocks | Variant::Doris);

        let mut tables: BTreeMap<String, TableSchema> = BTreeMap::new();
        for r in self.rows(&format!("SELECT * FROM information_schema.TABLES WHERE TABLE_SCHEMA = {dbl}")).await? {
            let Some(name) = named(&r, &["TABLE_NAME"]) else { continue };
            if table_kind(&named(&r, &["TABLE_TYPE"]).unwrap_or_default()) != Some(kinds::TABLE) {
                continue;
            }
            let mut t = TableSchema {
                kind: kinds::TABLE.into(),
                name: name.clone(),
                comment: named(&r, &["TABLE_COMMENT"]).filter(|c| !c.is_empty()),
                ..Default::default()
            };
            if v.is_mysql_server() {
                if let Some(e) = named(&r, &["ENGINE"]) {
                    t.options.insert("engine".into(), e);
                }
            }
            if matches!(v, Variant::MySql | Variant::MariaDb | Variant::TiDb | Variant::OceanBase) {
                if let Some(c) = named(&r, &["TABLE_COLLATION"]).filter(|c| !c.is_empty()) {
                    t.options.insert("charset".into(), c.split('_').next().unwrap_or_default().to_string());
                    t.options.insert("collation".into(), c);
                }
            }
            if v == Variant::TiDb {
                // Kept only for tables with a primary key (below).
                if let Some(pk) = named(&r, &["TIDB_PK_TYPE"]).filter(|p| !p.is_empty()) {
                    t.options.insert("clustered_index".into(), pk.to_ascii_uppercase());
                }
            }
            tables.insert(name, t);
        }

        // Columns, plus key columns by COLUMN_KEY for engines without
        // KEY_COLUMN_USAGE.
        let mut cols: Vec<(String, u64, ColumnDef, String)> = Vec::new();
        for r in self.rows(&format!("SELECT * FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = {dbl}")).await? {
            let (Some(table), Some(name)) = (named(&r, &["TABLE_NAME"]), named(&r, &["COLUMN_NAME"])) else { continue };
            if !tables.contains_key(&table) {
                continue;
            }
            let extra = named(&r, &["EXTRA"]).unwrap_or_default();
            let mut ty = named(&r, &["COLUMN_TYPE"])
                .filter(|t| !t.is_empty())
                .or_else(|| named(&r, &["DATA_TYPE"]))
                .unwrap_or_default();
            if v == Variant::MySql && !maria {
                // A spatial column's SRID (MySQL 8), which its spatial index needs.
                if let Some(srid) = named(&r, &["SRS_ID"]).filter(|s| !s.is_empty()) {
                    ty = format!("{ty} SRID {srid}");
                }
            }
            let generated = named(&r, &["GENERATION_EXPRESSION"]).filter(|g| !g.is_empty());
            let extra_l = extra.to_ascii_lowercase();
            let raw_default = named(&r, &["COLUMN_DEFAULT"]);
            let mut default_value = column_default(raw_default.as_deref(), &extra, &ty.to_ascii_lowercase(), maria);
            if let Some(g) = generated.filter(|_| extra_l.contains("generated") && !extra_l.contains("default_generated")) {
                let kind = if extra_l.contains("stored") || extra_l.contains("persistent") { "STORED" } else { "VIRTUAL" };
                ty = format!("{ty} GENERATED ALWAYS AS ({g}) {kind}");
                default_value = None;
            }
            let ord = named(&r, &["ORDINAL_POSITION"]).and_then(|o| o.parse().ok()).unwrap_or(0);
            let key = named(&r, &["COLUMN_KEY"]).unwrap_or_default().to_ascii_uppercase();
            let def = ColumnDef {
                name,
                data_type: ty,
                nullable: named(&r, &["IS_NULLABLE"]).is_none_or(|n| n.eq_ignore_ascii_case("YES")),
                default_value,
                auto_increment: extra_l.contains("auto_increment"),
                comment: named(&r, &["COLUMN_COMMENT"]).filter(|c| !c.is_empty()),
                ..Default::default()
            };
            cols.push((table, ord, def, key));
        }
        cols.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        let mut col_pk: HashMap<String, Vec<String>> = HashMap::new();
        for (table, _, def, key) in cols {
            let t = tables.get_mut(&table).expect("filtered above");
            match key.as_str() {
                "TIME INDEX" => {
                    t.options.insert("time_index".into(), def.name.clone());
                }
                "PRI" | "UNI" if olap => {
                    t.primary_key.get_or_insert_with(Default::default).columns.push(def.name.clone());
                    if key == "UNI" {
                        t.options.insert("key_model".into(), "unique".into());
                    }
                }
                "PRI" => col_pk.entry(table.clone()).or_default().push(def.name.clone()),
                "DUP" | "AGG" if olap => {
                    t.options.insert("key_model".into(), "duplicate".into());
                    let k = t.options.entry("key_columns".into()).or_default();
                    k.push_str(&format!("{}{}", if k.is_empty() { "" } else { ", " }, def.name));
                }
                _ => {}
            }
            t.columns.push(def);
        }

        if !olap {
            // Primary keys and foreign keys, columns in key order.
            let mut kcu = self
                .optional_rows(&format!("SELECT * FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA = {dbl}"))
                .await;
            let pos = |r: &Row| named(r, &["ORDINAL_POSITION"]).and_then(|o| o.parse::<u64>().ok()).unwrap_or(0);
            kcu.sort_by_key(pos);
            let mut rules: HashMap<(String, String), (Option<String>, Option<String>)> = HashMap::new();
            if v.has_foreign_keys() {
                let sql = format!("SELECT * FROM information_schema.REFERENTIAL_CONSTRAINTS WHERE CONSTRAINT_SCHEMA = {dbl}");
                for r in self.optional_rows(&sql).await {
                    if let (Some(t), Some(c)) = (named(&r, &["TABLE_NAME"]), named(&r, &["CONSTRAINT_NAME"])) {
                        rules.insert((t, c), (fk_rule(named(&r, &["DELETE_RULE"])), fk_rule(named(&r, &["UPDATE_RULE"]))));
                    }
                }
            }
            let mut fks: BTreeMap<(String, String), ForeignKeyDef> = BTreeMap::new();
            let mut pks: HashMap<String, Vec<String>> = HashMap::new();
            for r in &kcu {
                let (Some(table), Some(cname), Some(col)) =
                    (named(r, &["TABLE_NAME"]), named(r, &["CONSTRAINT_NAME"]), named(r, &["COLUMN_NAME"]))
                else {
                    continue;
                };
                if cname == "PRIMARY" {
                    pks.entry(table).or_default().push(col);
                } else if let Some(ref_table) = named(r, &["REFERENCED_TABLE_NAME"]).filter(|_| v.has_foreign_keys()) {
                    let (on_delete, on_update) = rules.get(&(table.clone(), cname.clone())).cloned().unwrap_or_default();
                    let fk = fks.entry((table, cname.clone())).or_insert_with(|| ForeignKeyDef {
                        name: Some(cname),
                        ref_schema: named(r, &["REFERENCED_TABLE_SCHEMA"]).filter(|s| *s != db),
                        ref_table,
                        on_delete,
                        on_update,
                        ..Default::default()
                    });
                    fk.columns.push(col);
                    fk.ref_columns.push(named(r, &["REFERENCED_COLUMN_NAME"]).unwrap_or_default());
                }
            }
            for (name, t) in tables.iter_mut() {
                // Engines without KEY_COLUMN_USAGE: COLUMN_KEY, in column order.
                let pk = pks.remove(name).or_else(|| col_pk.remove(name)).unwrap_or_default();
                if !pk.is_empty() {
                    t.primary_key = Some(KeyDef { name: None, columns: pk });
                }
            }
            for ((table, _), fk) in fks {
                if let Some(t) = tables.get_mut(&table) {
                    t.foreign_keys.push(fk);
                }
            }
        }

        if v != Variant::Databend {
            self.catalog_indexes(&dbl, &mut tables).await;
        } else {
            // Inverted, ngram and aggregating indexes.
            for r in self.optional_rows("SELECT * FROM system.indexes").await {
                let s = |n: &str| named(&r, &[n]).unwrap_or_default();
                if let Some((table, ix)) = crate::structure::databend_index(&db, &s("name"), &s("type"), &s("original"), &s("definition")) {
                    if let Some(t) = tables.get_mut(&table) {
                        t.indexes.push(ix);
                    }
                }
            }
            for t in tables.values_mut() {
                t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
            }
        }
        if v.has_checks() {
            self.catalog_checks(&dbl, &mut tables).await;
        }
        self.show_create_details(&mut tables).await;
        for t in tables.values_mut() {
            if t.primary_key.is_none() {
                t.options.remove("clustered_index");
            }
        }
        Ok(tables.into_values().collect())
    }

    /// Indexes and unique constraints, with what makes two of them
    /// different: key prefixes, expressions (functional indexes), DESC
    /// parts, INVISIBLE / IGNORED, comments; StarRocks / Doris index types
    /// and their properties; GreptimeDB's column indexes.
    async fn catalog_indexes(&mut self, dbl: &str, tables: &mut BTreeMap<String, TableSchema>) {
        let v = self.variant;
        let olap = matches!(v, Variant::StarRocks | Variant::Doris);
        let mut stats: Vec<(String, Row)> = Vec::new();
        if olap {
            // Their information_schema.STATISTICS is empty.
            let names: Vec<String> = tables.keys().cloned().collect();
            for n in names {
                let sql = format!("SHOW INDEX FROM {}", quote_ident(Quote::Backtick, &n));
                stats.extend(self.optional_rows(&sql).await.into_iter().map(|r| (n.clone(), r)));
            }
        } else {
            // TiDB's STATISTICS leaves SUB_PART empty; TIDB_INDEXES has it.
            let view = if v == Variant::TiDb { "TIDB_INDEXES" } else { "STATISTICS" };
            let rows = self.optional_rows(&format!("SELECT * FROM information_schema.{view} WHERE TABLE_SCHEMA = {dbl}")).await;
            stats.extend(rows.into_iter().filter_map(|r| Some((named(&r, &["TABLE_NAME"])?, r))));
        }
        stats.sort_by_key(|(_, r)| named(r, &["SEQ_IN_INDEX"]).and_then(|o| o.parse::<u64>().ok()).unwrap_or(0));
        let yes = |r: &Row, names: &[&str], value: &str| named(r, names).is_some_and(|x| x.eq_ignore_ascii_case(value));
        let mut idx: BTreeMap<(String, String), Option<IndexDef>> = BTreeMap::new();
        let mut desc: HashMap<(String, String), Vec<String>> = HashMap::new();
        for (table, r) in &stats {
            let Some(name) = named(r, &["INDEX_NAME", "KEY_NAME"]) else { continue };
            if name == "PRIMARY" || !tables.contains_key(table) {
                continue;
            }
            let ty = named(r, &["INDEX_TYPE"]).filter(|k| !k.is_empty());
            // GreptimeDB also lists its primary key and time index.
            if v == Variant::GreptimeDb && !ty.as_deref().is_some_and(|t| ["INVERTED", "FULLTEXT", "SKIPPING"].contains(&t.to_ascii_uppercase().as_str())) {
                continue;
            }
            let key = (table.clone(), name.clone());
            let entry = idx.entry(key.clone()).or_insert_with(|| {
                let (kind, mut options) = match ty.clone() {
                    Some(t) if olap => crate::structure::olap_index_type(&t),
                    t => (t, BTreeMap::new()),
                };
                if olap {
                    // Doris shows the properties apart.
                    if let Some(p) = named(r, &["Properties"]).filter(|p| p.contains('=')) {
                        options.extend(crate::structure::key_values(&p));
                    }
                }
                let comment = if olap { named(r, &["Index_comment", "Comment"]) } else { named(r, &["INDEX_COMMENT"]) };
                if let Some(c) = comment.filter(|c| !c.is_empty()) {
                    options.insert("COMMENT".into(), c);
                }
                if yes(r, &["IS_VISIBLE", "Visible"], "NO") {
                    options.insert("INVISIBLE".into(), "YES".into());
                }
                if yes(r, &["IGNORED"], "YES") {
                    options.insert("IGNORED".into(), "YES".into());
                }
                Some(IndexDef { name, unique: !olap && named(r, &["NON_UNIQUE"]).as_deref() == Some("0"), kind, options, ..Default::default() })
            });
            let spatial = ty.as_deref().is_some_and(|t| t.eq_ignore_ascii_case("SPATIAL"));
            // TiDB's TIDB_INDEXES writes the text NULL in both columns.
            let expression = named(r, &["EXPRESSION"]).filter(|e| !e.is_empty() && e != "NULL");
            let part = match (named(r, &["COLUMN_NAME"]), expression) {
                // Functional key part: MySQL shows its quotes escaped.
                (_, Some(e)) => Some(format!("({})", e.replace("\\'", "'"))),
                // A spatial index reports a SUB_PART it doesn't take.
                (Some(col), None) => Some(match named(r, &["SUB_PART"]).filter(|n| !n.is_empty() && !spatial) {
                    Some(n) => format!("{col}({n})"),
                    None => col,
                }),
                _ => None,
            };
            match (entry.as_mut(), part) {
                (Some(ix), Some(p)) => {
                    if yes(r, &["COLLATION"], "D") {
                        desc.entry(key).or_default().push(p.clone());
                    }
                    ix.columns.push(p);
                }
                _ => *entry = None,
            }
        }
        for ((table, name), ix) in idx {
            if let (Some(mut ix), Some(t)) = (ix, tables.get_mut(&table)) {
                if let Some(d) = desc.remove(&(table, name)) {
                    ix.options.insert("desc".into(), d.join(", "));
                }
                t.indexes.push(ix);
            }
        }
    }

    /// CHECK constraints. MariaDB's CHECK_CONSTRAINTS names the table;
    /// MySQL's goes through TABLE_CONSTRAINTS (which says whether it's
    /// enforced); TiDB's says neither, so its CHECKs come from SHOW CREATE
    /// TABLE.
    async fn catalog_checks(&mut self, dbl: &str, tables: &mut BTreeMap<String, TableSchema>) {
        let v = self.variant;
        let cc = self.optional_rows(&format!("SELECT * FROM information_schema.CHECK_CONSTRAINTS WHERE CONSTRAINT_SCHEMA = {dbl}")).await;
        if cc.is_empty() {
            return;
        }
        let with_table = cc.first().is_some_and(|r| r.columns_ref().iter().any(|c| c.name_str().eq_ignore_ascii_case("TABLE_NAME")));
        let mut owner: HashMap<String, (String, bool)> = HashMap::new();
        if !with_table {
            let sql = format!("SELECT * FROM information_schema.TABLE_CONSTRAINTS WHERE CONSTRAINT_SCHEMA = {dbl} AND CONSTRAINT_TYPE = 'CHECK'");
            for r in self.optional_rows(&sql).await {
                if let (Some(t), Some(n)) = (named(&r, &["TABLE_NAME"]), named(&r, &["CONSTRAINT_NAME"])) {
                    let off = named(&r, &["ENFORCED"]).is_some_and(|e| e.eq_ignore_ascii_case("NO"));
                    owner.insert(n, (t, off));
                }
            }
        }
        let mut unplaced = false;
        for r in &cc {
            let (Some(name), Some(clause)) = (named(r, &["CONSTRAINT_NAME"]), named(r, &["CHECK_CLAUSE"])) else { continue };
            let (table, off) = match named(r, &["TABLE_NAME"]) {
                Some(t) => (t, false),
                None => match owner.get(&name) {
                    Some(o) => o.clone(),
                    None => {
                        unplaced = true;
                        continue;
                    }
                },
            };
            // MySQL shows the condition's quotes escaped, like its defaults.
            let clause = if v == Variant::MariaDb { clause } else { clause.replace("\\'", "'") };
            let off = if off { crate::structure::NOT_ENFORCED } else { "" };
            // MariaDB's column CHECKs (named after the column) belong to the
            // column: only redefining it drops them.
            if named(r, &["LEVEL"]).is_some_and(|l| l.eq_ignore_ascii_case("Column")) {
                if let Some(c) = tables.get_mut(&table).and_then(|t| t.columns.iter_mut().find(|c| c.name == name)) {
                    c.data_type = format!("{}{}{clause})", c.data_type, crate::structure::COLUMN_CHECK);
                    continue;
                }
            }
            if let Some(t) = tables.get_mut(&table) {
                t.checks.push(dbine_driver::CheckDef { name: Some(name), expression: format!("{clause}{off}") });
            }
        }
        if unplaced {
            let names: Vec<String> = tables.keys().cloned().collect();
            for n in names {
                if let Some(create) = self.show_create_table(&n).await {
                    if let Some(t) = tables.get_mut(&n) {
                        t.checks = crate::structure::create_checks(&create);
                    }
                }
            }
        }
        for t in tables.values_mut() {
            t.checks.sort_by(|a, b| a.name.cmp(&b.name));
        }
    }

    /// The database's sequences, with their CREATE where the catalog is
    /// all there is to build it from (OceanBase, Databend; `None` elsewhere:
    /// SHOW CREATE SEQUENCE gives it).
    async fn sequences(&mut self, db: &str) -> Vec<(String, Option<String>)> {
        let dbl = lit(db);
        match self.variant {
            Variant::OceanBase => {
                let sql = format!("SELECT * FROM oceanbase.DBA_OB_SEQUENCE_OBJECTS WHERE DATABASE_NAME = {dbl}");
                self.optional_rows(&sql)
                    .await
                    .iter()
                    .filter_map(|r| {
                        let name = named(r, &["SEQUENCE_NAME"])?;
                        let g = |n: &[&str], d: &str| named(r, n).unwrap_or_else(|| d.to_string());
                        let flag = |n: &str| named(r, &[n]).is_some_and(|f| f.eq_ignore_ascii_case("YES") || f == "1" || f.eq_ignore_ascii_case("Y"));
                        let ddl = crate::structure::oceanbase_sequence(
                            &name,
                            [
                                &g(&["START_WITH", "START_VALUE"], "1"),
                                &g(&["INCREMENT_BY"], "1"),
                                &g(&["MIN_VALUE"], "1"),
                                &g(&["MAX_VALUE"], "9223372036854775807"),
                                &g(&["CACHE_SIZE"], "0"),
                            ],
                            flag("CYCLE_FLAG"),
                            flag("ORDER_FLAG"),
                        );
                        Some((name, Some(ddl)))
                    })
                    .collect()
            }
            Variant::Databend => self
                .optional_rows("SHOW SEQUENCES")
                .await
                .iter()
                .filter_map(|r| {
                    let name = named(r, &["name"])?;
                    let ddl = crate::structure::databend_sequence(&name, named(r, &["start"]).as_deref(), named(r, &["interval", "increment"]).as_deref(), named(r, &["comment"]).as_deref());
                    Some((name, Some(ddl)))
                })
                .collect(),
            _ => {
                let sql = format!("SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = {dbl} AND TABLE_TYPE = 'SEQUENCE'");
                self.optional_rows(&sql).await.iter().filter_map(|r| Some((at(r, 0)?, None))).collect()
            }
        }
    }

    async fn show_create_table(&mut self, name: &str) -> Option<String> {
        let obj = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: name.to_string() };
        self.show_create("TABLE", &obj, "Create Table").await.ok().flatten()
    }

    /// What only SHOW CREATE TABLE says: full-text parsers (MySQL,
    /// MariaDB), GreptimeDB's index settings, StarRocks / Doris bloom
    /// filter columns.
    async fn show_create_details(&mut self, tables: &mut BTreeMap<String, TableSchema>) {
        let v = self.variant;
        let fulltext = |t: &TableSchema| t.indexes.iter().any(|i| i.kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("FULLTEXT")));
        let names: Vec<String> = tables
            .iter()
            .filter(|(_, t)| match v {
                Variant::MySql | Variant::MariaDb | Variant::OceanBase => fulltext(t),
                Variant::GreptimeDb | Variant::StarRocks | Variant::Doris | Variant::SingleStore => true,
                _ => false,
            })
            .map(|(n, _)| n.clone())
            .collect();
        for n in names {
            let Some(create) = self.show_create_table(&n).await else { continue };
            // Rollups: DESC … ALL lists every index of the table with its fields.
            let mut desc_all = Vec::new();
            if matches!(v, Variant::StarRocks | Variant::Doris) {
                let sql = format!("DESC {} ALL", quote_ident(Quote::Backtick, &n));
                let mut last = String::new();
                for r in self.optional_rows(&sql).await {
                    let index = named(&r, &["IndexName"]).unwrap_or_default();
                    if !index.is_empty() {
                        last = index.clone();
                    }
                    desc_all.push((index, if last.is_empty() { String::new() } else { named(&r, &["Field"]).unwrap_or_default() }));
                }
            }
            let Some(t) = tables.get_mut(&n) else { continue };
            match v {
                Variant::GreptimeDb => {
                    for (col, kind, opts) in crate::structure::greptime_indexes(&create) {
                        if let Some(ix) = t.indexes.iter_mut().find(|i| i.kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case(&kind)) && i.columns == [col.clone()]) {
                            ix.options.extend(opts);
                        }
                    }
                    // Table options (ttl, append_mode, compaction…);
                    // compaction.override is the server's own mark.
                    t.options.extend(crate::structure::greptime_with(&create).into_iter().filter(|(k, _)| k != "compaction.override"));
                }
                Variant::StarRocks | Variant::Doris => {
                    if let Some(b) = crate::structure::olap_properties(&create).get("bloom_filter_columns").filter(|b| !b.trim().is_empty()) {
                        t.options.insert("bloom_filter_columns".into(), crate::structure::column_set(b));
                    }
                    let cols: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
                    for (name, columns) in crate::structure::olap_rollups(&n, &cols, &desc_all) {
                        t.indexes.push(IndexDef { name, columns, kind: Some("ROLLUP".into()), ..Default::default() });
                    }
                    t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
                }
                Variant::SingleStore => {
                    let k = crate::structure::singlestore_keys(&create);
                    let mut drop_names = vec!["__SHARDKEY".to_string(), "__UNORDERED".to_string()];
                    for (key, part) in [("shard_key", &k.shard), ("sort_key", &k.sort)] {
                        if let Some((name, parts)) = part {
                            drop_names.push(name.clone());
                            if !parts.is_empty() {
                                t.options.insert(key.into(), parts.clone());
                            }
                        }
                    }
                    if let Some(ty) = k.table_type {
                        t.options.insert("table_type".into(), ty);
                    }
                    // Shard and sort keys are table settings, not indexes.
                    t.indexes.retain(|i| !drop_names.contains(&i.name));
                    for ix in &mut t.indexes {
                        if k.hash.contains(&ix.name) {
                            ix.kind = Some("HASH".into());
                        } else if let Some((_, version)) = k.fulltext.iter().find(|(n, _)| *n == ix.name) {
                            ix.kind = Some("FULLTEXT".into());
                            if let Some(ver) = version {
                                ix.options.insert("VERSION".into(), ver.clone());
                            }
                        } else {
                            // SKIPLIST / COLUMNSTORE: the storage's own kind.
                            ix.kind = None;
                        }
                    }
                }
                _ => {
                    for (ix_name, parser) in crate::structure::fulltext_parsers(&create) {
                        if let Some(ix) = t.indexes.iter_mut().find(|i| i.name == ix_name) {
                            ix.options.insert("WITH PARSER".into(), parser);
                        }
                    }
                }
            }
        }
    }

    /// Manticore: no catalog; DESCRIBE per table.
    async fn describe_schema(&mut self) -> Result<Vec<TableSchema>> {
        let mut out = Vec::new();
        let mut tables: Vec<String> =
            self.relations("").await?.into_iter().filter(|o| o.kind == kinds::TABLE).map(|o| o.name).collect();
        tables.sort();
        for name in tables {
            let obj = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: name.clone() };
            let cols = self.columns_from_describe(&obj).await?;
            // Table settings (morphology, min_infix_len…) change how it's indexed.
            let options = match self.show_create_table(&name).await {
                Some(create) => crate::structure::manticore_settings(&create),
                None => BTreeMap::new(),
            };
            out.push(TableSchema {
                options,
                kind: kinds::TABLE.into(),
                name,
                columns: cols
                    .into_iter()
                    .map(|c| ColumnDef { name: c.name, data_type: c.data_type, nullable: c.nullable, ..Default::default() })
                    .collect(),
                ..Default::default()
            });
        }
        Ok(out)
    }

    async fn explain_script(&mut self, sql: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let version = match self.rows("SELECT VERSION()").await {
            Ok(rows) => rows.first().and_then(|r| at(r, 0)).unwrap_or_default(),
            Err(_) => String::new(),
        };
        let flavor = Flavor::detect(self.variant, &version);
        if analyze && !flavor.can_analyze() {
            out.messages.push("Este servidor no da cifras reales en EXPLAIN: se muestran los planes estimados.".into());
        }
        for stmt in plan::split_keeping_hints(self.variant, sql) {
            let kind = plan::classify(&stmt);
            match (analyze, kind) {
                (false, StmtKind::Other) => {
                    out.messages.push(format!("Sin plan (no se ejecutó): {}", plan::short(&stmt)));
                }
                (false, _) => {
                    let p = self.plan_of(flavor, &stmt, false).await?;
                    out.plans.push(p);
                }
                (true, StmtKind::Read) if flavor.can_analyze() => {
                    self.run(&stmt, max_rows, out).await?;
                    let p = self.plan_of(flavor, &stmt, true).await?;
                    out.plans.push(p);
                }
                (true, StmtKind::Other) => self.run(&stmt, max_rows, out).await?,
                (true, _) => {
                    let p = self.plan_of(flavor, &stmt, false).await?;
                    out.plans.push(p);
                    self.run(&stmt, max_rows, out).await?;
                }
            }
        }
        Ok(())
    }

    async fn plan_of(&mut self, flavor: Flavor, stmt: &str, actual: bool) -> Result<Plan> {
        match flavor {
            Flavor::MySqlTree { .. } => {
                let q = if actual { format!("EXPLAIN ANALYZE {stmt}") } else { format!("EXPLAIN FORMAT=TREE {stmt}") };
                let (_, rows) = self.string_rows(&q).await?;
                let raw = first_column(&rows);
                if raw.contains("not executable by iterator executor") {
                    let (header, rows) = self.string_rows(&format!("EXPLAIN {stmt}")).await?;
                    return Ok(plan::tabular(stmt, &header, &rows));
                }
                Ok(plan::mysql_tree(stmt, &raw, actual))
            }
            Flavor::MariaDb => {
                let q = if actual { format!("ANALYZE FORMAT=JSON {stmt}") } else { format!("EXPLAIN FORMAT=JSON {stmt}") };
                let (_, rows) = self.string_rows(&q).await?;
                plan::maria_json(stmt, &first_column(&rows), actual).map_err(Error::Query)
            }
            Flavor::TiDb => {
                let q = if actual { format!("EXPLAIN ANALYZE {stmt}") } else { format!("EXPLAIN FORMAT='brief' {stmt}") };
                let (header, rows) = self.string_rows(&q).await?;
                Ok(plan::tidb(stmt, &header, &rows, actual))
            }
            Flavor::Plain => {
                let (header, rows) = self.string_rows(&format!("EXPLAIN {stmt}")).await?;
                if header.len() == 1 {
                    Ok(plan::text_lines(stmt, &first_column(&rows)))
                } else {
                    Ok(plan::tabular(stmt, &header, &rows))
                }
            }
        }
    }

    /// Column names and every cell as text ("NULL" for NULL).
    async fn string_rows(&mut self, sql: &str) -> Result<(Vec<String>, Vec<Vec<String>>)> {
        let mut result = self.conn.query_iter(sql).await.map_err(err)?;
        let header: Vec<String> =
            result.columns().map(|c| c.iter().map(|c| c.name_str().into_owned()).collect()).unwrap_or_default();
        let mut rows = Vec::new();
        while let Some(row) = result.next().await.map_err(err)? {
            rows.push((0..row.len()).map(|i| at(&row, i).unwrap_or_else(|| "NULL".into())).collect());
        }
        result.drop_result().await.map_err(err)?;
        Ok((header, rows))
    }
}

/// The first cell of each row, one per line.
fn first_column(rows: &[Vec<String>]) -> String {
    rows.iter().filter_map(|r| r.first().cloned()).collect::<Vec<_>>().join("\n")
}

/// A `DELIMITER` line (mysql CLI client command) in the script.
fn has_delimiter_command(sql: &str) -> bool {
    sql.lines().any(|l| l.trim_start().get(..10).is_some_and(|w| w.eq_ignore_ascii_case("delimiter ")))
}

fn warnings_note(n: u16) -> Option<String> {
    (n > 0).then(|| format!("{n} advertencia(s); SHOW WARNINGS para verlas"))
}

#[async_trait]
impl Session for MySqlSession {
    async fn server_version(&mut self) -> Result<String> {
        let rows = self.rows("SELECT VERSION()").await?;
        Ok(rows.first().and_then(|r| at(r, 0)).unwrap_or_default())
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        if self.variant.single_namespace() {
            return Ok(vec!["Manticore".into()]);
        }
        Ok(self.rows("SHOW DATABASES").await?.iter().filter_map(|r| at(r, 0)).collect())
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let db = if self.variant.single_namespace() {
            String::new()
        } else {
            match self.current_database().await {
                Some(db) => db,
                None => return Ok(Vec::new()),
            }
        };
        let mut out = self.relations(&db).await?;
        let dbl = lit(&db);
        if self.variant == Variant::StarRocks {
            let rows = self
                .optional_rows(&format!(
                    "SELECT TABLE_NAME FROM information_schema.materialized_views WHERE TABLE_SCHEMA = {dbl}"
                ))
                .await;
            let mvs: Vec<String> = rows.iter().filter_map(|r| at(r, 0)).collect();
            out.retain(|o| !mvs.contains(&o.name));
            out.extend(mvs.into_iter().map(|name| DbObject {
                kind: kinds::MATERIALIZED_VIEW.into(),
                schema: None,
                name,
                parent: None,
            }));
        }
        if self.variant.has_routines() {
            let rows = self
                .optional_rows(&format!(
                    "SELECT ROUTINE_NAME, ROUTINE_TYPE FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = {dbl}"
                ))
                .await;
            out.extend(rows.iter().filter_map(|r| {
                let kind = if at(r, 1)?.eq_ignore_ascii_case("PROCEDURE") { kinds::PROCEDURE } else { kinds::FUNCTION };
                Some(DbObject { kind: kind.into(), schema: None, name: at(r, 0)?, parent: None })
            }));
        }
        if self.variant.has_triggers() {
            let rows = self
                .optional_rows(&format!(
                    "SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = {dbl}"
                ))
                .await;
            out.extend(rows.iter().filter_map(|r| {
                Some(DbObject { kind: kinds::TRIGGER.into(), schema: None, name: at(r, 0)?, parent: at(r, 1) })
            }));
        }
        if self.variant.has_sequences() {
            let names: Vec<String> = self.sequences(&db).await.into_iter().map(|(n, _)| n).collect();
            out.extend(names.into_iter().map(|name| DbObject { kind: kinds::SEQUENCE.into(), schema: None, name, parent: None }));
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        if self.variant != Variant::Manticore {
            match self.columns_from_info_schema(obj).await {
                Ok(c) if !c.is_empty() => return Ok(c),
                Ok(_) => {}
                Err(e) => tracing::debug!("{:?}: information_schema.COLUMNS: {e}", self.variant),
            }
        }
        self.columns_from_describe(obj).await
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        // OceanBase and Databend don't show a sequence's CREATE: it's built from the catalog.
        if obj.kind == kinds::SEQUENCE && matches!(self.variant, Variant::OceanBase | Variant::Databend) {
            let db = self.current_database().await.unwrap_or_default();
            return Ok(self.sequences(&db).await.into_iter().find(|(n, _)| *n == obj.name).and_then(|(_, d)| d));
        }
        let attempts: &[(&str, &str)] = match obj.kind.as_str() {
            kinds::TABLE => &[("TABLE", "Create Table")],
            kinds::VIEW => &[("VIEW", "Create View"), ("TABLE", "Create Table")],
            kinds::MATERIALIZED_VIEW => &[("MATERIALIZED VIEW", "Create Materialized View"), ("TABLE", "Create Table")],
            kinds::PROCEDURE => &[("PROCEDURE", "Create Procedure")],
            kinds::FUNCTION => &[("FUNCTION", "Create Function")],
            kinds::TRIGGER => &[("TRIGGER", "SQL Original Statement")],
            // MariaDB answers in "Create Table", TiDB in "Create Sequence".
            kinds::SEQUENCE => &[("SEQUENCE", "Create Table")],
            _ => &[],
        };
        let mut last = None;
        for (what, column) in attempts {
            match self.show_create(what, obj, column).await {
                Ok(Some(d)) => return Ok(Some(d)),
                Ok(None) => {}
                Err(e) => last = Some(e),
            }
        }
        match last {
            // A table the engine can't describe as DDL: the app builds one.
            Some(e) if obj.kind == kinds::TABLE => {
                tracing::debug!("{:?}: SHOW CREATE TABLE: {e}", self.variant);
                Ok(None)
            }
            Some(e) => Err(e),
            None => Ok(None),
        }
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        select_top(Quote::Backtick, Limit::Limit, obj.schema(), &obj.name, limit)
    }

    async fn execute(&mut self, sql: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.cancelled.store(false, Ordering::SeqCst);
        let res = if self.variant == Variant::Manticore || has_delimiter_command(sql) {
            // One statement per request: Manticore takes no more, and a
            // `DELIMITER` is the client's to apply (the server reads it as
            // an error).
            let stmts = if self.variant == Variant::Manticore { manticore_statements(sql) } else { self.statements(sql) };
            let mut res = Ok(());
            for stmt in stmts {
                res = self.run(&stmt, max_rows, out).await;
                if res.is_err() {
                    break;
                }
            }
            res
        } else if matches!(self.variant, Variant::StarRocks | Variant::Doris) && olap_schema_change(sql) {
            // A table takes one schema change job at a time: a single
            // ALTER / CREATE INDEX / DROP INDEX waits for the one running
            // (scripts like "Comparar esquemas" send several in a row).
            let start = std::time::Instant::now();
            loop {
                match self.run(sql, max_rows, out).await {
                    Err(e) if e.is_query() && olap_busy(&e.to_string()) && start.elapsed() < OLAP_WAIT && !self.cancelled.load(Ordering::SeqCst) => {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                    res => break res,
                }
            }
        } else {
            self.run(sql, max_rows, out).await
        };
        if self.cancelled.swap(false, Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        // `USE other`: the tab follows the session's new database.
        if !self.variant.single_namespace() && self.statements_of(sql).iter().any(|s| leading_keyword(s, &crate::script_dialect(self.variant)).as_deref() == Some("use")) {
            let before = self.database.clone();
            if let Ok(rows) = self.rows("SELECT DATABASE()").await {
                if let Some(db) = rows.first().and_then(|r| at(r, 0)) {
                    if before.as_deref() != Some(db.as_str()) {
                        self.database = Some(db.clone());
                        out.database = Some(db);
                    }
                }
            }
        }
        res
    }

    async fn transaction_state(&mut self) -> Result<Option<TxState>> {
        if !self.variant.has_transactions() {
            return Ok(None);
        }
        // The last OK packet's status says it; after an error there's none
        // (a deadlock rolls the transaction back): ask with a no-op.
        if self.conn.last_ok_packet().is_none() {
            self.conn.query_drop("DO 0").await.map_err(err)?;
        }
        let open = self.conn.last_ok_packet().is_some_and(|ok| ok.status_flags().contains(StatusFlags::SERVER_STATUS_IN_TRANS));
        Ok(Some(if open { TxState::Open } else { TxState::Idle }))
    }

    /// `SET autocommit`: off, the first statement opens a transaction that
    /// stays open until COMMIT / ROLLBACK. Turning it back on commits the
    /// open one (the server does).
    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        if !self.variant.has_transactions() {
            return if on { Ok(()) } else { Err(Error::Unsupported("este motor no permite transacciones manuales desde DBine".into())) };
        }
        self.conn.query_drop(if on { "SET autocommit = 1" } else { "SET autocommit = 0" }).await.map_err(err)
    }

    async fn commit(&mut self) -> Result<()> {
        if !self.variant.has_transactions() {
            return Err(Error::Unsupported("este motor no permite transacciones manuales desde DBine".into()));
        }
        self.conn.query_drop("COMMIT").await.map_err(err)
    }

    async fn rollback(&mut self) -> Result<()> {
        if !self.variant.has_transactions() {
            return Err(Error::Unsupported("este motor no permite transacciones manuales desde DBine".into()));
        }
        self.conn.query_drop("ROLLBACK").await.map_err(err)
    }

    /// Plans per statement (optimizer hints are kept when splitting).
    ///
    /// Estimated, nothing runs: MySQL 8.0.16+ `EXPLAIN FORMAT=TREE` (the
    /// single-table UPDATE/DELETE it can't draw fall back to the tabular
    /// EXPLAIN), MariaDB `EXPLAIN FORMAT=JSON`, TiDB `EXPLAIN
    /// FORMAT='brief'`, anything else plain `EXPLAIN`.
    ///
    /// Actual: each statement runs as with `execute`; a read then runs a
    /// second time under MySQL's `EXPLAIN ANALYZE` (8.0.18+), MariaDB's
    /// `ANALYZE FORMAT=JSON` or TiDB's `EXPLAIN ANALYZE`. Writes get their
    /// estimated plan before they run (those commands would apply them
    /// again). Engines without an analyzing EXPLAIN give estimated plans.
    async fn explain(&mut self, sql: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if self.variant == Variant::Manticore {
            return Err(Error::Unsupported("Manticore Search no ofrece planes de ejecución por SQL".into()));
        }
        self.cancelled.store(false, Ordering::SeqCst);
        let res = self.explain_script(sql, analyze, max_rows, out).await;
        if self.cancelled.swap(false, Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        res
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        self.snapshot().await
    }

    async fn blocking(&mut self) -> Result<Vec<dbine_driver::BlockedSession>> {
        if !self.variant.has_lock_waits() {
            return Err(Error::Unsupported("este motor no informa bloqueos entre sesiones".into()));
        }
        self.blocking_chains().await
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        if !crate::security::supported(self.variant) {
            return Err(Error::Unsupported("este motor no administra usuarios desde DBine".into()));
        }
        crate::security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        if !crate::security::supported(self.variant) {
            return Err(Error::Unsupported("este motor no administra usuarios desde DBine".into()));
        }
        crate::security::grants(self, principal).await
    }

    async fn kill_session(&mut self, id: &str) -> Result<()> {
        if !self.variant.has_lock_waits() {
            return Err(Error::Unsupported("este motor no permite terminar sesiones desde DBine".into()));
        }
        self.kill_connection(id).await
    }

    async fn profiler_start(&mut self, opts: &dbine_driver::ProfilerOptions) -> Result<dbine_driver::ProfilerStarted> {
        let (state, started) = crate::profiler::start(self, opts).await?;
        self.profiler = Some(state);
        Ok(started)
    }

    async fn profiler_poll(&mut self) -> Result<Vec<dbine_driver::ProfiledStatement>> {
        let mut state = self.profiler.take().ok_or_else(|| Error::State("el profiler no está iniciado".into()))?;
        let r = crate::profiler::poll(self, &mut state).await;
        self.profiler = Some(state);
        r
    }

    async fn profiler_stop(&mut self) -> Result<()> {
        match self.profiler.take() {
            Some(state) => crate::profiler::stop(self, state).await,
            None => Ok(()),
        }
    }

    async fn backups(&mut self, database: Option<&str>) -> Result<Vec<dbine_driver::BackupEntry>> {
        crate::backup::history(self, database).await
    }

    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        if self.variant == Variant::Manticore {
            self.describe_schema().await
        } else {
            self.catalog_schema().await
        }
    }

    async fn create_database(&mut self, name: &str) -> Result<()> {
        if !crate::design::capabilities(self.variant).create_database {
            return Err(Error::Unsupported("este motor no crea bases desde DBine".into()));
        }
        self.conn.query_drop(format!("CREATE DATABASE {}", quote_ident(Quote::Backtick, name))).await.map_err(err)
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        if !crate::design::capabilities(self.variant).drop_database {
            return Err(Error::Unsupported("este motor no borra bases desde DBine".into()));
        }
        if self.current_database().await.is_some_and(|db| db.eq_ignore_ascii_case(name)) {
            return Err(Error::Query(format!(
                "No se puede borrar la base «{name}» porque es la de esta sesión; conéctese a otra base para borrarla."
            )));
        }
        self.conn.query_drop(format!("DROP DATABASE {}", quote_ident(Quote::Backtick, name))).await.map_err(err)
    }

    async fn read_batches(&mut self, spec: &dbine_driver::transfer::ReadSpec, sink: dbine_driver::transfer::BatchSinkRef) -> Result<u64> {
        crate::transfer::read_batches(self, spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::transfer::LoadSpec,
        _columns: &[dbine_driver::transfer::TransferColumn],
        source: &mut dyn dbine_driver::transfer::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        crate::transfer::bulk_load(self, spec, source, progress).await
    }

    fn as_any(&mut self) -> Option<&mut (dyn std::any::Any + Send)> {
        Some(self)
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        if !self.variant.has_kill_query() {
            return None;
        }
        // Dropping the connection leaves the statement running on the
        // server; KILL QUERY from a second connection stops it.
        let (opts, id, flag) = (self.opts.clone(), self.conn.id(), self.cancelled.clone());
        let kill = if self.variant == Variant::TiDb { "KILL TIDB QUERY" } else { "KILL QUERY" };
        let rt = tokio::runtime::Handle::try_current().ok()?;
        Some(Arc::new(move || {
            let (opts, flag) = (opts.clone(), flag.clone());
            rt.spawn(async move {
                flag.store(true, Ordering::SeqCst);
                match mysql_async::Conn::new(opts).await {
                    Ok(mut c) => {
                        if let Err(e) = c.query_drop(format!("{kill} {id}")).await {
                            tracing::debug!("mysql cancel failed: {e}");
                        }
                        let _ = c.disconnect().await;
                    }
                    Err(e) => tracing::debug!("mysql cancel connection failed: {e}"),
                }
            });
        }))
    }

    /// `SHOW INDEX` plus the engine's usage counters (see `index_usage`).
    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        if !crate::index_usage::supported(self.variant) {
            return Ok(None);
        }
        crate::index_usage::report(self, table).await.map(Some)
    }

    /// One query: SHOW GRANTS, or StarRocks' roles (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        crate::permissions::check(self, database).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_are_escaped() {
        assert_eq!(lit("a'b\\c"), "'a''b\\\\c'");
    }

    #[test]
    fn table_types() {
        assert_eq!(table_kind("BASE TABLE"), Some(kinds::TABLE));
        assert_eq!(table_kind("SYSTEM VERSIONED"), Some(kinds::TABLE));
        assert_eq!(table_kind("VIEW"), Some(kinds::VIEW));
        assert_eq!(table_kind("SYSTEM VIEW"), None);
        assert_eq!(table_kind("SEQUENCE"), None);
    }

    #[test]
    fn manticore_scripts_split_outside_escaped_quotes_and_comments() {
        let s = manticore_statements("INSERT INTO t VALUES ('a\\';b'); SELECT 1;\n");
        assert_eq!(s, ["INSERT INTO t VALUES ('a\\';b')", "SELECT 1"]);
        let s = manticore_statements("/* a; b */ SELECT 1; -- c; d\n# e; f\nSELECT 2 /* ' */;");
        assert_eq!(s, ["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn delimiter_lines_are_found() {
        assert!(has_delimiter_command("select 1;\n  DELIMITER //\ncreate procedure p() begin end//"));
        assert!(!has_delimiter_command("select 'delimiter x'"));
        assert!(!has_delimiter_command("select delimiter from t"));
    }

    #[test]
    fn defaults_become_sql() {
        let d = |raw: Option<&str>, extra: &str, ty: &str, maria: bool| column_default(raw, extra, ty, maria);
        assert_eq!(d(Some("x'y"), "", "varchar(10)", false).as_deref(), Some("'x''y'"));
        assert_eq!(d(Some("0.00"), "", "decimal(10,2)", false).as_deref(), Some("0.00"));
        assert_eq!(d(Some("concat(_utf8mb4\\'a\\')"), "DEFAULT_GENERATED", "varchar(9)", false).as_deref(), Some("(concat(_utf8mb4'a'))"));
        assert_eq!(
            d(Some("CURRENT_TIMESTAMP"), "DEFAULT_GENERATED on update CURRENT_TIMESTAMP", "timestamp", false).as_deref(),
            Some("CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP")
        );
        assert_eq!(d(Some("current_timestamp()"), "", "timestamp(3)", false).as_deref(), Some("current_timestamp()"));
        assert_eq!(d(Some("NULL"), "", "int(11)", true), None);
        assert_eq!(d(Some("'a'"), "", "varchar(3)", true).as_deref(), Some("'a'"));
        assert_eq!(d(None, "", "int", false), None);
    }

    #[test]
    fn olap_schema_changes_wait_only_when_alone() {
        assert!(olap_schema_change("ALTER TABLE `t` ADD ROLLUP `r` (`a`);"));
        assert!(olap_schema_change("create index i on t (a) using bitmap"));
        assert!(!olap_schema_change("ALTER TABLE t ADD INDEX i (a); ALTER TABLE t SET (\"x\" = \"1\")"));
        assert!(!olap_schema_change("SELECT 1"));
        assert!(olap_busy("A schema change operation is in progress on the table docs. Please wait"));
        assert!(olap_busy("Table[t]'s state is not NORMAL. Do not allow doing ALTER ops"));
        assert!(!olap_busy("Unknown column"));
    }

    #[test]
    fn warnings_are_summarised() {
        assert_eq!(warnings_note(0), None);
        assert!(warnings_note(2).unwrap().starts_with('2'));
    }
}
