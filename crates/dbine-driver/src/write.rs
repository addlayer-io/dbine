//! Writing rows into a table by mode: a notebook's write cell
//! (docs/notebooks.md › Writing a DataFrame to a table).
//!
//! The rows come as [`RowBatch`]es (the transfer's lossless cells) and go
//! to [`Session::write_rows`](crate::Session::write_rows), with one of four
//! [`WriteMode`]s. Drivers say which ones they do with
//! [`Driver::write_modes`](crate::Driver::write_modes); without any, the
//! app still inserts through [`Session::bulk_load`](crate::Session::bulk_load).
//!
//! SQL engines share one path, [`staged_write`]: the rows are bulk loaded
//! into a **staging table** that copies the target's column types (a
//! temporary table where the engine's bulk load reaches one), then applied
//! to the target with plain set-based statements ([`staged_plan`]) inside
//! one transaction:
//!
//! - **Insert**: `INSERT INTO t (…) SELECT … FROM staging`.
//! - **Update by key**: the engine's update-from-join (`UPDATE … FROM`,
//!   `UPDATE … JOIN`, `MERGE … WHEN MATCHED`) of the non-key columns.
//! - **Upsert**: that update, then `INSERT … SELECT … WHERE NOT EXISTS` of
//!   the keys the target doesn't have. Two statements rather than one
//!   `MERGE` / `ON CONFLICT`: it needs no unique index on the key, gives
//!   the updated and inserted counts apart, and is the same on every
//!   engine.
//! - **Replace**: `DELETE FROM t` (never `TRUNCATE`: it isn't
//!   transactional everywhere, and foreign keys refuse it), then the insert.
//!
//! Values never go into SQL text: they travel through the bulk load; the
//! statements carry only identifiers, quoted for the dialect. A failure
//! rolls the transaction back, so the target is as it was.

use crate::sql::{qualified_name, quote_ident, Quote};
use crate::transfer::{BatchSource, LoadSpec, Progress, TransferColumn};
use crate::{Error, ObjectRef, QueryOutcome, Result, Session};
use serde::{Deserialize, Serialize};

/// How the rows go into the table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    /// Add every row (the default: it changes no row already there).
    #[default]
    Insert,
    /// Update the rows whose key matches; rows without a match are left out.
    Update,
    /// Update the rows whose key matches and insert the others.
    Upsert,
    /// Delete every row of the table, then insert these.
    Replace,
}

impl WriteMode {
    pub const ALL: [WriteMode; 4] = [
        WriteMode::Insert,
        WriteMode::Update,
        WriteMode::Upsert,
        WriteMode::Replace,
    ];

    /// The mode matches rows by key columns.
    pub fn needs_key(self) -> bool {
        matches!(self, WriteMode::Update | WriteMode::Upsert)
    }
}

/// What to write, and where.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteSpec {
    pub table: ObjectRef,
    /// Target columns, in the order of the batches' cells.
    pub columns: Vec<String>,
    /// The key (Update and Upsert): some of `columns`.
    #[serde(default)]
    pub key: Vec<String>,
    pub mode: WriteMode,
}

impl WriteSpec {
    /// The columns, key and mode make sense together (checked before
    /// anything runs).
    pub fn check(&self) -> Result<()> {
        if self.columns.is_empty() {
            return Err(Error::Query("no hay columnas para escribir".into()));
        }
        let mut seen = std::collections::HashSet::new();
        for c in &self.columns {
            if c.trim().is_empty() {
                return Err(Error::Query("hay una columna de destino sin nombre".into()));
            }
            if !seen.insert(c.to_lowercase()) {
                return Err(Error::Query(format!(
                    "la columna de destino «{c}» está repetida"
                )));
            }
        }
        if self.mode.needs_key() {
            if self.key.is_empty() {
                return Err(Error::Query(
                    "elegí la columna clave para actualizar por clave".into(),
                ));
            }
            for k in &self.key {
                if !self.columns.iter().any(|c| c.eq_ignore_ascii_case(k)) {
                    return Err(Error::Query(format!(
                        "la clave «{k}» no está entre las columnas que se escriben"
                    )));
                }
            }
            if self.mode == WriteMode::Update && self.non_key().is_empty() {
                return Err(Error::Query(
                    "no hay columnas para actualizar además de la clave".into(),
                ));
            }
        }
        Ok(())
    }

    /// The columns that aren't part of the key.
    pub fn non_key(&self) -> Vec<&String> {
        self.columns
            .iter()
            .filter(|c| !self.key.iter().any(|k| k.eq_ignore_ascii_case(c)))
            .collect()
    }
}

/// What a write did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteResult {
    pub inserted: u64,
    pub updated: u64,
    pub deleted: u64,
    /// It was one transaction: a failure would have left the table as it
    /// was. `false`: the engine wrote as it went (what was written before
    /// a failure stays).
    #[serde(default)]
    pub atomic: bool,
    /// What the user should know (Spanish, for the cell's result).
    #[serde(default)]
    pub notes: Vec<String>,
}

// ---------------------------------------------------------------------------
// The SQL engines' path: a staging table and set-based statements.

/// How a SQL dialect stages and applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageFlavor {
    /// `#temp` from `SELECT TOP 0 … INTO`, `UPDATE t … FROM … JOIN`.
    /// `identity_insert`: a written column is the table's identity, so the
    /// inserts run between `SET IDENTITY_INSERT … ON` / `OFF`.
    SqlServer { identity_insert: bool },
    /// `CREATE TEMPORARY TABLE … AS … LIMIT 0`, `UPDATE … FROM`.
    Postgres,
    /// `CREATE TEMPORARY TABLE … AS … LIMIT 0`, `UPDATE … JOIN`.
    MySql,
    /// A global temporary table (rows private to the session) in the
    /// user's own schema, `MERGE … WHEN MATCHED`.
    Oracle,
    /// `CREATE TEMP TABLE … AS … LIMIT 0`, `UPDATE … FROM` (SQLite 3.33+).
    Sqlite,
    /// A table next to the target (DuckDB's appender loads only the
    /// database's own catalogs), `UPDATE … FROM`.
    DuckDb,
}

impl StageFlavor {
    fn quote(self) -> Quote {
        match self {
            StageFlavor::SqlServer { .. } => Quote::Bracket,
            StageFlavor::MySql => Quote::Backtick,
            _ => Quote::Double,
        }
    }
}

/// What a step's row count is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counts {
    Inserted,
    Updated,
    Deleted,
    /// Not counted (`SET IDENTITY_INSERT`).
    Nothing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub sql: String,
    pub counts: Counts,
    /// Counts the rows `sql` is about to change, right before it, in the
    /// same transaction (engines don't all report affected rows, and
    /// those that do count differently: MySQL counts only changed rows).
    pub count: Option<String>,
}

/// The statements of a staged write.
#[derive(Debug, Clone)]
pub struct StagedPlan {
    /// Where the rows are bulk loaded.
    pub staging: ObjectRef,
    /// Creates the staging table, empty, with the target's column types.
    pub create: String,
    /// Applied in one transaction, in order.
    pub apply: Vec<Step>,
    /// Drops the staging table (after the transaction).
    pub drop: Vec<String>,
}

/// The statements for `spec` with a staging table named `stage` (letters,
/// digits and `_`; [`staged_write`] makes it unique).
pub fn staged_plan(flavor: StageFlavor, spec: &WriteSpec, stage: &str) -> Result<StagedPlan> {
    spec.check()?;
    let q = flavor.quote();
    let id = |n: &str| quote_ident(q, n);
    let target = qualified_name(q, spec.table.schema(), &spec.table.name);
    let (staging, create, drop) = match flavor {
        StageFlavor::SqlServer { .. } => {
            let name = format!("#{stage}");
            let cols = spec
                .columns
                .iter()
                .map(|c| id(c))
                .collect::<Vec<_>>()
                .join(", ");
            let s = id(&name);
            (
                ObjectRef {
                    kind: "table".into(),
                    schema: None,
                    name,
                },
                format!("SELECT TOP 0 {cols} INTO {s} FROM {target}"),
                vec![format!("DROP TABLE {s}")],
            )
        }
        StageFlavor::Postgres | StageFlavor::MySql | StageFlavor::Sqlite => {
            let cols = spec
                .columns
                .iter()
                .map(|c| id(c))
                .collect::<Vec<_>>()
                .join(", ");
            let s = id(stage);
            let temp = if flavor == StageFlavor::Sqlite {
                "TEMP"
            } else {
                "TEMPORARY"
            };
            let drop = if flavor == StageFlavor::MySql {
                format!("DROP TEMPORARY TABLE {s}")
            } else {
                format!("DROP TABLE {s}")
            };
            (
                ObjectRef {
                    kind: "table".into(),
                    schema: None,
                    name: stage.to_string(),
                },
                format!("CREATE {temp} TABLE {s} AS SELECT {cols} FROM {target} LIMIT 0"),
                vec![drop],
            )
        }
        StageFlavor::Oracle => {
            let name = stage.to_ascii_uppercase();
            let cols = spec
                .columns
                .iter()
                .map(|c| id(c))
                .collect::<Vec<_>>()
                .join(", ");
            let s = id(&name);
            (
                ObjectRef { kind: "table".into(), schema: None, name },
                format!("CREATE GLOBAL TEMPORARY TABLE {s} ON COMMIT PRESERVE ROWS AS SELECT {cols} FROM {target} WHERE 1 = 0"),
                // A temporary table the session has rows in can't be dropped.
                vec![format!("TRUNCATE TABLE {s}"), format!("DROP TABLE {s} PURGE")],
            )
        }
        StageFlavor::DuckDb => {
            let cols = spec
                .columns
                .iter()
                .map(|c| id(c))
                .collect::<Vec<_>>()
                .join(", ");
            let r = ObjectRef {
                kind: "table".into(),
                schema: spec.table.schema.clone(),
                name: stage.to_string(),
            };
            let s = qualified_name(q, r.schema(), &r.name);
            (
                r,
                format!("CREATE TABLE {s} AS SELECT {cols} FROM {target} LIMIT 0"),
                vec![format!("DROP TABLE {s}")],
            )
        }
    };
    let s = qualified_name(q, staging.schema(), &staging.name);
    let cols = spec
        .columns
        .iter()
        .map(|c| id(c))
        .collect::<Vec<_>>()
        .join(", ");
    let s_cols = spec
        .columns
        .iter()
        .map(|c| format!("s.{}", id(c)))
        .collect::<Vec<_>>()
        .join(", ");
    let on = spec
        .key
        .iter()
        .map(|k| format!("t.{k} = s.{k}", k = id(k)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let rest = spec.non_key();

    let insert_all = Step {
        sql: format!("INSERT INTO {target} ({cols}) SELECT {cols} FROM {s}"),
        counts: Counts::Inserted,
        count: Some(format!("SELECT count(*) FROM {s}")),
    };
    let missing = format!("FROM {s} s WHERE NOT EXISTS (SELECT 1 FROM {target} t WHERE {on})");
    let insert_missing = Step {
        sql: format!("INSERT INTO {target} ({cols}) SELECT {s_cols} {missing}"),
        counts: Counts::Inserted,
        count: Some(format!("SELECT count(*) {missing}")),
    };
    let matched =
        format!("SELECT count(*) FROM {s} s WHERE EXISTS (SELECT 1 FROM {target} t WHERE {on})");
    let update = (!rest.is_empty()).then(|| {
        let sql = match flavor {
            StageFlavor::SqlServer { .. } => {
                let set = rest.iter().map(|c| format!("t.{c} = s.{c}", c = id(c))).collect::<Vec<_>>().join(", ");
                format!("UPDATE t SET {set} FROM {target} AS t INNER JOIN {s} AS s ON {on}")
            }
            StageFlavor::Postgres | StageFlavor::Sqlite | StageFlavor::DuckDb => {
                let set = rest.iter().map(|c| format!("{c} = s.{c}", c = id(c))).collect::<Vec<_>>().join(", ");
                format!("UPDATE {target} AS t SET {set} FROM {s} AS s WHERE {on}")
            }
            StageFlavor::MySql => {
                let set = rest.iter().map(|c| format!("t.{c} = s.{c}", c = id(c))).collect::<Vec<_>>().join(", ");
                format!("UPDATE {target} AS t INNER JOIN {s} AS s ON {on} SET {set}")
            }
            StageFlavor::Oracle => {
                let set = rest.iter().map(|c| format!("t.{c} = s.{c}", c = id(c))).collect::<Vec<_>>().join(", ");
                format!("MERGE INTO {target} t USING {s} s ON ({on}) WHEN MATCHED THEN UPDATE SET {set}")
            }
        };
        Step { sql, counts: Counts::Updated, count: Some(matched) }
    });
    let mut apply = match spec.mode {
        WriteMode::Insert => vec![insert_all],
        WriteMode::Replace => vec![
            Step {
                sql: format!("DELETE FROM {target}"),
                counts: Counts::Deleted,
                count: Some(format!("SELECT count(*) FROM {target}")),
            },
            insert_all,
        ],
        WriteMode::Update => update.into_iter().collect(),
        WriteMode::Upsert => update.into_iter().chain([insert_missing]).collect(),
    };
    if let StageFlavor::SqlServer {
        identity_insert: true,
    } = flavor
    {
        // Around each insert: only one table per session may have it on.
        let mut wrapped = Vec::new();
        for st in apply {
            if st.counts == Counts::Inserted {
                wrapped.push(Step {
                    sql: format!("SET IDENTITY_INSERT {target} ON"),
                    counts: Counts::Nothing,
                    count: None,
                });
                wrapped.push(st);
                wrapped.push(Step {
                    sql: format!("SET IDENTITY_INSERT {target} OFF"),
                    counts: Counts::Nothing,
                    count: None,
                });
            } else {
                wrapped.push(st);
            }
        }
        apply = wrapped;
    }
    Ok(StagedPlan {
        staging,
        create,
        apply,
        drop,
    })
}

/// A staging table's name, unique enough to never meet another one.
pub fn stage_name() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("dbine_w_{:x}", n ^ (std::process::id() as u128) << 64)
}

async fn run(s: &mut (impl Session + ?Sized), sql: &str) -> Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1, &mut out).await?;
    if let Some(e) = out
        .error
        .take()
        .or_else(|| out.errors.first().map(|e| e.message.clone()))
    {
        return Err(Error::Query(e));
    }
    Ok(out)
}

/// The number a `SELECT count(*)` returned.
async fn run_count(s: &mut (impl Session + ?Sized), sql: &str) -> Result<u64> {
    let out = run(s, sql).await?;
    let v = out
        .results
        .iter()
        .find(|r| !r.columns.is_empty())
        .and_then(|r| r.rows.first())
        .and_then(|r| r.first())
        .cloned();
    match v {
        Some(serde_json::Value::Number(n)) => Ok(n
            .as_u64()
            .or_else(|| n.as_f64().map(|f| f as u64))
            .unwrap_or(0)),
        Some(serde_json::Value::String(t)) => t
            .trim()
            .parse()
            .map_err(|_| Error::Query(format!("la cuenta de filas no es un número: {t}"))),
        _ => Err(Error::Query("la cuenta de filas no devolvió nada".into())),
    }
}

/// Write `source`'s rows into `spec.table` through a staging table (see
/// the module docs): `columns` are the rows' own columns (the read's), in
/// the order of `spec.columns`. `transactional`: the session does manual
/// transactions ([`Session::set_autocommit`]); the statements that change
/// the target then run in one, rolled back on failure.
pub async fn staged_write<S: Session + ?Sized>(
    s: &mut S,
    flavor: StageFlavor,
    spec: &WriteSpec,
    columns: &[TransferColumn],
    source: &mut dyn BatchSource,
    progress: Progress<'_>,
    transactional: bool,
) -> Result<WriteResult> {
    let plan = staged_plan(flavor, spec, &stage_name())?;
    run(s, &plan.create).await?;
    let r = apply(
        s,
        &plan,
        flavor,
        spec,
        columns,
        source,
        progress,
        transactional,
    )
    .await;
    // The staging table goes whatever happened (a failed load may have
    // closed the session, and the temporary table with it).
    for d in &plan.drop {
        let _ = run(s, d).await;
    }
    r
}

#[allow(clippy::too_many_arguments)]
async fn apply<S: Session + ?Sized>(
    s: &mut S,
    plan: &StagedPlan,
    flavor: StageFlavor,
    spec: &WriteSpec,
    columns: &[TransferColumn],
    source: &mut dyn BatchSource,
    progress: Progress<'_>,
    transactional: bool,
) -> Result<WriteResult> {
    let load = LoadSpec {
        table: plan.staging.clone(),
        columns: spec.columns.clone(),
        table_lock: false,
        // The staging table copies an identity column with its property.
        keep_identity: matches!(flavor, StageFlavor::SqlServer { .. }),
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    s.bulk_load(&load, columns, source, progress).await?;
    let mut out = WriteResult {
        atomic: transactional,
        ..Default::default()
    };
    if transactional {
        s.set_autocommit(false).await?;
    }
    let mut failed = None;
    for st in &plan.apply {
        let step = async {
            let n = match &st.count {
                Some(c) => run_count(s, c).await?,
                None => 0,
            };
            run(s, &st.sql).await?;
            Ok::<u64, Error>(n)
        };
        match step.await {
            Ok(n) => match st.counts {
                Counts::Inserted => out.inserted += n,
                Counts::Updated => out.updated += n,
                Counts::Deleted => out.deleted += n,
                Counts::Nothing => {}
            },
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    if transactional {
        let end = match failed {
            None => s.commit().await,
            Some(_) => s.rollback().await,
        };
        let back = s.set_autocommit(true).await;
        if failed.is_none() {
            end?;
            back?;
        }
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(mode: WriteMode, key: &[&str]) -> WriteSpec {
        WriteSpec {
            table: ObjectRef {
                kind: "table".into(),
                schema: Some("dbo".into()),
                name: "ventas".into(),
            },
            columns: vec!["id".into(), "region".into(), "total".into()],
            key: key.iter().map(|k| k.to_string()).collect(),
            mode,
        }
    }

    fn sqls(p: &StagedPlan) -> Vec<&str> {
        p.apply.iter().map(|s| s.sql.as_str()).collect()
    }

    #[test]
    fn the_spec_is_checked_before_anything_runs() {
        assert!(spec(WriteMode::Insert, &[]).check().is_ok());
        assert!(spec(WriteMode::Update, &[])
            .check()
            .unwrap_err()
            .to_string()
            .contains("clave"));
        assert!(spec(WriteMode::Upsert, &["nope"])
            .check()
            .unwrap_err()
            .to_string()
            .contains("«nope»"));
        let mut only_key = spec(WriteMode::Update, &["id"]);
        only_key.columns = vec!["id".into()];
        assert!(only_key
            .check()
            .unwrap_err()
            .to_string()
            .contains("además de la clave"));
        // Upsert with only the key: inserts the missing keys.
        only_key.mode = WriteMode::Upsert;
        assert!(only_key.check().is_ok());
        let mut dup = spec(WriteMode::Insert, &[]);
        dup.columns.push("ID".into());
        assert!(dup.check().unwrap_err().to_string().contains("repetida"));
    }

    #[test]
    fn sql_server_stages_in_tempdb_and_updates_from_a_join() {
        let p = staged_plan(
            StageFlavor::SqlServer {
                identity_insert: false,
            },
            &spec(WriteMode::Upsert, &["id"]),
            "dbine_w_1",
        )
        .unwrap();
        assert_eq!(p.staging.name, "#dbine_w_1");
        assert_eq!(
            p.create,
            "SELECT TOP 0 [id], [region], [total] INTO [#dbine_w_1] FROM [dbo].[ventas]"
        );
        assert_eq!(
            sqls(&p),
            [
                "UPDATE t SET t.[region] = s.[region], t.[total] = s.[total] FROM [dbo].[ventas] AS t INNER JOIN [#dbine_w_1] AS s ON t.[id] = s.[id]",
                "INSERT INTO [dbo].[ventas] ([id], [region], [total]) SELECT s.[id], s.[region], s.[total] FROM [#dbine_w_1] s WHERE NOT EXISTS (SELECT 1 FROM [dbo].[ventas] t WHERE t.[id] = s.[id])",
            ]
        );
        assert_eq!(p.drop, ["DROP TABLE [#dbine_w_1]"]);
        // An identity column written: each insert between ON and OFF.
        let p = staged_plan(
            StageFlavor::SqlServer {
                identity_insert: true,
            },
            &spec(WriteMode::Replace, &[]),
            "x",
        )
        .unwrap();
        assert_eq!(
            sqls(&p),
            [
                "DELETE FROM [dbo].[ventas]",
                "SET IDENTITY_INSERT [dbo].[ventas] ON",
                "INSERT INTO [dbo].[ventas] ([id], [region], [total]) SELECT [id], [region], [total] FROM [#x]",
                "SET IDENTITY_INSERT [dbo].[ventas] OFF",
            ]
        );
    }

    #[test]
    fn each_family_has_its_update_from_join() {
        let up = |f| {
            staged_plan(f, &spec(WriteMode::Update, &["id"]), "w")
                .unwrap()
                .apply[0]
                .sql
                .clone()
        };
        assert_eq!(up(StageFlavor::Postgres), "UPDATE \"dbo\".\"ventas\" AS t SET \"region\" = s.\"region\", \"total\" = s.\"total\" FROM \"w\" AS s WHERE t.\"id\" = s.\"id\"");
        assert_eq!(up(StageFlavor::MySql), "UPDATE `dbo`.`ventas` AS t INNER JOIN `w` AS s ON t.`id` = s.`id` SET t.`region` = s.`region`, t.`total` = s.`total`");
        assert_eq!(up(StageFlavor::Oracle), "MERGE INTO \"dbo\".\"ventas\" t USING \"W\" s ON (t.\"id\" = s.\"id\") WHEN MATCHED THEN UPDATE SET t.\"region\" = s.\"region\", t.\"total\" = s.\"total\"");
        assert_eq!(up(StageFlavor::DuckDb), "UPDATE \"dbo\".\"ventas\" AS t SET \"region\" = s.\"region\", \"total\" = s.\"total\" FROM \"dbo\".\"w\" AS s WHERE t.\"id\" = s.\"id\"");
        let p = staged_plan(StageFlavor::Postgres, &spec(WriteMode::Insert, &[]), "w").unwrap();
        assert_eq!(p.create, "CREATE TEMPORARY TABLE \"w\" AS SELECT \"id\", \"region\", \"total\" FROM \"dbo\".\"ventas\" LIMIT 0");
        let p = staged_plan(StageFlavor::MySql, &spec(WriteMode::Insert, &[]), "w").unwrap();
        assert_eq!(p.drop, ["DROP TEMPORARY TABLE `w`"]);
        let p = staged_plan(
            StageFlavor::Oracle,
            &spec(WriteMode::Insert, &[]),
            "dbine_w_1",
        )
        .unwrap();
        assert_eq!(p.create, "CREATE GLOBAL TEMPORARY TABLE \"DBINE_W_1\" ON COMMIT PRESERVE ROWS AS SELECT \"id\", \"region\", \"total\" FROM \"dbo\".\"ventas\" WHERE 1 = 0");
        assert_eq!(
            p.drop,
            [
                "TRUNCATE TABLE \"DBINE_W_1\"",
                "DROP TABLE \"DBINE_W_1\" PURGE"
            ]
        );
        let p = staged_plan(StageFlavor::Sqlite, &spec(WriteMode::Insert, &[]), "w").unwrap();
        assert!(p.create.starts_with("CREATE TEMP TABLE \"w\""));
    }

    #[test]
    fn identifiers_are_quoted_never_spliced() {
        let mut s = spec(WriteMode::Upsert, &["i]d"]);
        s.columns = vec!["i]d".into(), "a\"b".into(), "c`d".into()];
        s.table.name = "t]x".into();
        let p = staged_plan(
            StageFlavor::SqlServer {
                identity_insert: false,
            },
            &s,
            "w",
        )
        .unwrap();
        assert!(
            p.apply[0].sql.contains("[dbo].[t]]x]"),
            "{}",
            p.apply[0].sql
        );
        assert!(p.apply[0].sql.contains("t.[i]]d] = s.[i]]d]"));
        let p = staged_plan(StageFlavor::Postgres, &s, "w").unwrap();
        assert!(p.apply[0].sql.contains("\"a\"\"b\" = s.\"a\"\"b\""));
        let p = staged_plan(StageFlavor::MySql, &s, "w").unwrap();
        assert!(p.apply[0].sql.contains("`c``d`"));
    }

    #[test]
    fn replace_deletes_then_inserts_and_upsert_with_only_the_key_inserts_the_missing_ones() {
        let p = staged_plan(StageFlavor::Postgres, &spec(WriteMode::Replace, &[]), "w").unwrap();
        assert_eq!(
            p.apply.iter().map(|s| s.counts).collect::<Vec<_>>(),
            [Counts::Deleted, Counts::Inserted]
        );
        assert_eq!(p.apply[0].sql, "DELETE FROM \"dbo\".\"ventas\"");
        let mut k = spec(WriteMode::Upsert, &["id"]);
        k.columns = vec!["id".into()];
        let p = staged_plan(StageFlavor::Postgres, &k, "w").unwrap();
        assert_eq!(p.apply.len(), 1);
        assert_eq!(p.apply[0].counts, Counts::Inserted);
    }

    #[test]
    fn stage_names_are_plain_identifiers() {
        let n = stage_name();
        assert!(
            n.starts_with("dbine_w_") && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "{n}"
        );
    }
}
