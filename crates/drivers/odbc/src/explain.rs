//! How each engine behind ODBC is asked for plans (the readers are in
//! [`crate::plan`]). Everything here runs on the session's blocking thread
//! with the connection locked.
//!
//! | Engine | Estimated | Actual |
//! |---|---|---|
//! | SQL Server (generic preset) | `SET SHOWPLAN_ALL ON` | `SET STATISTICS PROFILE ON` |
//! | Db2 LUW | `EXPLAIN PLAN FOR` + explain tables | run + estimated |
//! | Db2 for z/OS | `EXPLAIN PLAN SET QUERYNO` + `PLAN_TABLE` | run + estimated |
//! | Sybase ASE | `SET SHOWPLAN ON` + `SET NOEXEC ON` (messages) | `SET SHOWPLAN ON` and run |
//! | SQL Anywhere | `EXPLANATION('…')` | run + estimated |
//! | Hive, Impala, Teradata, Vertica, Dameng, Ocient, others | `EXPLAIN` text | run + estimated |
//! | Netezza | `EXPLAIN VERBOSE` (notices) | run + estimated |
//! | Exasol | — (no EXPLAIN) | session profiling |
//! | CUBRID | — (plans only in csql) | `SET TRACE ON` + `SHOW TRACE` |
//! | Informix, GBase 8s, Altibase, Db2 for i | — | — |

use crate::odbc::{Conn, StmtSlot};
use crate::plan;
use dbine_driver::plan::{plan_from_text, tree_from_indented_text};
use dbine_driver::{Error, Plan, PlanNode, QueryOutcome, Result};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    SqlServer,
    Db2Luw,
    Db2Zos,
    Sybase,
    SqlAnywhere,
    Hive,
    Impala,
    Teradata,
    Vertica,
    Netezza,
    Dameng,
    Exasol,
    Cubrid,
    /// `EXPLAIN <stmt>` with a text answer.
    Text,
    /// `<prefix> <stmt>` with a text answer (`EXPLAIN CALCITE`, `EXPLAIN PLAN FOR`).
    Prefixed(&'static str),
    /// The statement inside a call: `{q}` is it as a quoted literal's body
    /// (`'` doubled), `{raw}` as is (Virtuoso `explain('…')`, SQream
    /// `EXPLAIN($$…$$)`).
    Wrapped(&'static str),
    /// OpenEdge: the plan of the last statement run (`_Sql_Qplan`).
    OpenEdge,
    Unsupported(&'static str),
}

const INFORMIX: &str = "Informix y GBase 8s escriben el plan (SET EXPLAIN) en un archivo del servidor \
    (sqexplain.out); no lo devuelven por ODBC.";
const ALTIBASE: &str = "Altibase entrega el plan (ALTER SESSION SET EXPLAIN PLAN) solo a su cliente iSQL; \
    su ODBC no lo expone.";
const INGRES: &str = "Ingres y Actian Vector muestran el plan (SET QEP) en la salida de traza del monitor de terminal; \
    no lo devuelven por ODBC.";
const MIMER: &str = "Mimer SQL da el plan solo con SET EXPLAIN en BSQL; no hay forma de pedirlo por ODBC.";
const CACHE: &str = "Caché no tiene EXPLAIN por SQL: el plan se ve con Show Plan en el Portal de administración.";
const ZEN: &str = "Actian Zen escribe el plan en un archivo del servidor (SET QRYPLAN); no lo devuelve por ODBC.";
const JET: &str = "El motor de Access (ACE/Jet) escribe el plan en SHOWPLAN.OUT solo con una clave de registro de depuración; \
    no lo devuelve por ODBC.";
const NETSUITE: &str = "SuiteAnalytics Connect no expone planes de ejecución.";
const DB2I: &str = "Db2 for i no tiene EXPLAIN por SQL: los planes se ven con Visual Explain \
    (IBM i Access Client Solutions) o el monitor de base de datos.";

/// The plan dialect of a preset; the generic preset goes by the DBMS name.
pub fn dialect(preset: &str, dbms: &str) -> Dialect {
    if let Some(d) = by_preset(preset) {
        return d;
    }
    by_dbms(dbms)
}

/// Every preset but the generic one knows its dialect.
fn by_preset(preset: &str) -> Option<Dialect> {
    match preset {
        "db2" => Some(Dialect::Db2Luw),
        "db2zos" => Some(Dialect::Db2Zos),
        "db2i" => Some(Dialect::Unsupported(DB2I)),
        "sybase" => Some(Dialect::Sybase),
        "sqlanywhere" => Some(Dialect::SqlAnywhere),
        "hive" => Some(Dialect::Hive),
        "impala" => Some(Dialect::Impala),
        "teradata" => Some(Dialect::Teradata),
        "vertica" => Some(Dialect::Vertica),
        "netezza" => Some(Dialect::Netezza),
        "dameng" => Some(Dialect::Dameng),
        "exasol" => Some(Dialect::Exasol),
        "cubrid" => Some(Dialect::Cubrid),
        "informix" | "gbase8s" => Some(Dialect::Unsupported(INFORMIX)),
        "altibase" => Some(Dialect::Unsupported(ALTIBASE)),
        "ocient" => Some(Dialect::Text),
        "cloudera" => Some(Dialect::Hive),
        "spark" | "kyuubi" | "monetdb" | "iris" | "maxdb" | "nuodb" | "machbase" | "ignite" => Some(Dialect::Text),
        "heavydb" => Some(Dialect::Prefixed("EXPLAIN CALCITE")),
        "ignite3" => Some(Dialect::Prefixed("EXPLAIN PLAN FOR")),
        "virtuoso" => Some(Dialect::Wrapped("explain('{q}')")),
        "sqream" => Some(Dialect::Wrapped("SELECT EXPLAIN($${raw}$$)")),
        "openedge" => Some(Dialect::OpenEdge),
        "ingres" => Some(Dialect::Unsupported(INGRES)),
        "mimer" => Some(Dialect::Unsupported(MIMER)),
        "cache" => Some(Dialect::Unsupported(CACHE)),
        "zen" => Some(Dialect::Unsupported(ZEN)),
        "access" | "dbase" => Some(Dialect::Unsupported(JET)),
        "netsuite" => Some(Dialect::Unsupported(NETSUITE)),
        _ => None,
    }
}

fn by_dbms(dbms: &str) -> Dialect {
    let d = dbms.to_ascii_lowercase();
    let has = |s: &str| d.contains(s);
    if has("microsoft sql server") || has("azure sql") {
        Dialect::SqlServer
    } else if has("adaptive server") || has("sybase") {
        Dialect::Sybase
    } else if has("sql anywhere") {
        Dialect::SqlAnywhere
    } else if d.starts_with("dsn") || has("db2 for z") {
        Dialect::Db2Zos
    } else if (d.starts_with("as") && has("400")) || has("db2 for i") {
        Dialect::Unsupported(DB2I)
    } else if has("db2") {
        Dialect::Db2Luw
    } else if has("impala") {
        Dialect::Impala
    } else if has("hive") || has("spark") {
        Dialect::Hive
    } else if has("teradata") {
        Dialect::Teradata
    } else if has("vertica") {
        Dialect::Vertica
    } else if has("netezza") {
        Dialect::Netezza
    } else if has("exa") && has("sol") {
        Dialect::Exasol
    } else if has("cubrid") {
        Dialect::Cubrid
    } else if has("informix") || has("gbase") {
        Dialect::Unsupported(INFORMIX)
    } else if has("altibase") {
        Dialect::Unsupported(ALTIBASE)
    } else if d == "dm" || has("dameng") || has("dm8") {
        Dialect::Dameng
    } else {
        Dialect::Text
    }
}

/// Presets whose plans can't be had at all (the UI hides the buttons).
pub fn preset_supports_explain(preset: &str) -> bool {
    !matches!(dialect(preset, ""), Dialect::Unsupported(_))
}

/// Statements with a plan: queries and DML.
fn plannable(stmt: &str) -> bool {
    let mut s = stmt.trim_start();
    loop {
        if let Some(r) = s.strip_prefix("--") {
            s = r.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else if let Some(r) = s.strip_prefix("/*") {
            s = r.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else {
            break;
        }
    }
    let head: String = s.trim_start_matches('(').chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_uppercase();
    matches!(
        head.as_str(),
        "SELECT" | "WITH" | "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "UPSERT" | "VALUES" | "SEL" | "INS" | "UPD" | "DEL" | "REPLACE"
    )
}

/// Everything a statement answers, every cell as text.
#[derive(Default)]
struct Collected {
    sets: Vec<(Vec<String>, Vec<Vec<Option<String>>>)>,
    messages: Vec<String>,
}

impl Collected {
    /// Every row's cells joined, then the messages: the plan text.
    fn lines(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .sets
            .iter()
            .flat_map(|(_, rows)| rows.iter())
            .map(|r| r.iter().flatten().cloned().collect::<Vec<_>>().join(" "))
            .collect();
        out.extend(self.messages.iter().flat_map(|m| m.lines().map(str::to_string)));
        out
    }

    fn first(&self) -> Option<String> {
        self.sets.first()?.1.first()?.first().cloned().flatten()
    }
}

fn collect(c: &Conn, slot: &StmtSlot, sql: &str) -> Result<Collected> {
    let st = c.stmt(slot)?;
    let mut out = Collected::default();
    st.exec(sql)?;
    out.messages.extend(st.messages());
    loop {
        let n = st.num_cols()?;
        if n > 0 {
            let cols = st.describe(n)?.into_iter().map(|c| c.name).collect();
            out.sets.push((cols, st.text_rows()?));
        }
        let more = st.more_results();
        out.messages.extend(st.messages());
        if !more? {
            break;
        }
    }
    Ok(out)
}

/// Run a session setting, ignoring what it answers.
fn set(c: &Conn, slot: &StmtSlot, sql: &str) -> Result<()> {
    collect(c, slot, sql).map(|_| ())
}

/// Plans of `stmts` into `out` (and, with `analyze`, their results).
pub fn explain_all(d: Dialect, c: &Conn, slot: &StmtSlot, stmts: &[String], analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
    match d {
        Dialect::Unsupported(why) => Err(Error::Unsupported(why.into())),
        Dialect::SqlServer => sqlserver(c, slot, stmts, analyze, max_rows, out),
        Dialect::Sybase => sybase(c, slot, stmts, analyze, max_rows, out),
        Dialect::Exasol if !analyze => Err(Error::Unsupported(
            "Exasol no tiene plan estimado (no hay EXPLAIN): usá «Ejecutar con plan», que perfila la sesión.".into(),
        )),
        Dialect::Exasol => exasol(c, slot, stmts, max_rows, out),
        Dialect::Cubrid if !analyze => Err(Error::Unsupported(
            "CUBRID muestra el plan estimado solo en csql (;plan): usá «Ejecutar con plan» para ver la traza real (SET TRACE ON)."
                .into(),
        )),
        Dialect::Cubrid => cubrid(c, slot, stmts, max_rows, out),
        Dialect::OpenEdge if !analyze => Err(Error::Unsupported(
            "OpenEdge no tiene EXPLAIN: guarda el plan de cada sentencia al ejecutarla (_Sql_Qplan). Usá «Ejecutar con plan».".into(),
        )),
        Dialect::OpenEdge => openedge(c, slot, stmts, max_rows, out),
        _ => {
            let mut noted = false;
            for s in stmts {
                if plannable(s) {
                    let p = estimated(d, c, slot, s)?;
                    out.plans.push(p);
                    if analyze && !noted {
                        out.messages.push(
                            "Este motor no da cifras reales por operador por ODBC: se muestra el plan estimado junto al resultado."
                                .into(),
                        );
                        noted = true;
                    }
                } else if !analyze {
                    out.messages.push(format!("Sin plan para «{}»: solo se explican consultas y DML.", short(s)));
                }
                if analyze {
                    crate::run_one(c, slot, s, max_rows, out)?;
                }
            }
            Ok(())
        }
    }
}

fn short(stmt: &str) -> String {
    let one = stmt.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 60 {
        format!("{}…", one.chars().take(60).collect::<String>())
    } else {
        one
    }
}

fn text_plan(statement: &str, text: &str, root: PlanNode) -> Plan {
    Plan { statement: statement.trim().to_string(), root, actual: false, raw_format: "text".into(), raw: text.to_string() }
}

fn estimated(d: Dialect, c: &Conn, slot: &StmtSlot, s: &str) -> Result<Plan> {
    let stmt = s.trim().trim_end_matches(';');
    let explain = |prefix: &str| -> Result<String> { Ok(collect(c, slot, &format!("{prefix} {stmt}"))?.lines().join("\n")) };
    Ok(match d {
        Dialect::Db2Luw => db2(c, slot, stmt)?,
        Dialect::Db2Zos => db2zos(c, slot, stmt)?,
        Dialect::SqlAnywhere => {
            let text = collect(c, slot, &format!("SELECT EXPLANATION('{}')", stmt.replace('\'', "''")))?.first().unwrap_or_default();
            plan_from_text(stmt, &text, false)
        }
        Dialect::Hive => {
            let t = explain("EXPLAIN")?;
            text_plan(stmt, &t, plan::hive_tree(&t))
        }
        Dialect::Impala => {
            let t = explain("EXPLAIN")?;
            text_plan(stmt, &t, plan::impala_tree(&t))
        }
        Dialect::Teradata => {
            let t = explain("EXPLAIN")?;
            text_plan(stmt, &t, plan::teradata_tree(&t))
        }
        Dialect::Vertica => {
            let t = explain("EXPLAIN")?;
            let root = plan::vertica_tree(&t).unwrap_or_else(|| tree_from_indented_text(&t));
            text_plan(stmt, &t, root)
        }
        Dialect::Netezza => {
            let t = explain("EXPLAIN VERBOSE")?;
            text_plan(stmt, &t, plan::netezza_tree(&t))
        }
        Dialect::Dameng => {
            let t = explain("EXPLAIN")?;
            let root = plan::dameng_tree(&t).unwrap_or_else(|| tree_from_indented_text(&t));
            text_plan(stmt, &t, root)
        }
        Dialect::Wrapped(template) => {
            let sql = template.replace("{q}", &stmt.replace('\'', "''")).replace("{raw}", stmt);
            match collect(c, slot, &sql) {
                Ok(t) => plan_from_text(stmt, &t.lines().join("\n"), false),
                Err(Error::Query(m)) => {
                    return Err(Error::Unsupported(format!("El servidor no aceptó el pedido de plan: {m}")));
                }
                Err(e) => return Err(e),
            }
        }
        Dialect::Prefixed(prefix) => match explain(prefix) {
            Ok(t) => plan_from_text(stmt, &t, false),
            Err(Error::Query(m)) => {
                return Err(Error::Unsupported(format!("El servidor no aceptó {prefix}, así que no hay plan por ODBC: {m}")));
            }
            Err(e) => return Err(e),
        },
        _ => match explain("EXPLAIN") {
            Ok(t) => plan_from_text(stmt, &t, false),
            Err(Error::Query(m)) => {
                return Err(Error::Unsupported(format!("El servidor no aceptó EXPLAIN, así que no hay plan por ODBC: {m}")));
            }
            Err(e) => return Err(e),
        },
    })
}

/// A cell of a result set as text (numbers as they print).
fn cell_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

// ------------------------------------------------------------ SQL Server

fn plans_from_rows(rows: &[HashMap<String, String>], fallback: &str, actual: bool, format: &str, out: &mut QueryOutcome) {
    let raw_of = |id: &str| {
        rows.iter()
            .filter(|r| r.get("StmtId").map(String::as_str) == Some(id))
            .filter_map(|r| r.get("StmtText").cloned())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut ids: Vec<String> = Vec::new();
    for r in rows {
        let id = r.get("StmtId").cloned().unwrap_or_default();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    for ((text, root), id) in plan::sqlserver_trees(rows).into_iter().zip(ids) {
        let statement = if text.is_empty() { fallback.trim().to_string() } else { text };
        out.plans.push(Plan { statement, root, actual, raw_format: format.into(), raw: raw_of(&id) });
    }
}

fn sqlserver(c: &Conn, slot: &StmtSlot, stmts: &[String], analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
    let option = if analyze { "STATISTICS PROFILE" } else { "SHOWPLAN_ALL" };
    set(c, slot, &format!("SET {option} ON"))?;
    let result = (|| -> Result<()> {
        for s in stmts {
            if !analyze {
                let got = collect(c, slot, s)?;
                let mut rows = Vec::new();
                for (cols, set_rows) in &got.sets {
                    if cols.iter().any(|c| c == "StmtText") && cols.iter().any(|c| c == "NodeId") {
                        for r in set_rows {
                            rows.push(cols.iter().cloned().zip(r.iter().map(|v| v.clone().unwrap_or_default())).collect());
                        }
                    }
                }
                plans_from_rows(&rows, s, false, "showplan_all", out);
                continue;
            }
            // The profile comes as an extra result set after each
            // statement's own: taken out of the results. The run keeps more
            // rows than asked so a long profile isn't cut.
            let mut local = QueryOutcome::default();
            let r = crate::run_one(c, slot, s, max_rows.max(100_000), &mut local);
            out.messages.append(&mut local.messages);
            let mut rows = Vec::new();
            for mut rs in local.results {
                let names: Vec<&str> = rs.columns.iter().map(|c| c.name.as_str()).collect();
                if names.starts_with(&["Rows", "Executes", "StmtText"]) {
                    for r in &rs.rows {
                        rows.push(names.iter().map(|n| n.to_string()).zip(r.iter().map(cell_text)).collect());
                    }
                    continue;
                }
                if rs.rows.len() > max_rows {
                    rs.rows.truncate(max_rows);
                    rs.truncated = true;
                }
                out.results.push(rs);
            }
            plans_from_rows(&rows, s, true, "statistics_profile", out);
            r?;
        }
        Ok(())
    })();
    let off = set(c, slot, &format!("SET {option} OFF"));
    result.and(off)
}

// ----------------------------------------------------------------- Db2

fn db2(c: &Conn, slot: &StmtSlot, stmt: &str) -> Result<Plan> {
    let schema = collect(
        c,
        slot,
        "SELECT RTRIM(TABSCHEMA) FROM SYSCAT.TABLES WHERE TABNAME = 'EXPLAIN_OPERATOR' \
         AND TABSCHEMA IN (CURRENT USER, SESSION_USER, 'SYSTOOLS') \
         ORDER BY CASE WHEN TABSCHEMA = 'SYSTOOLS' THEN 1 ELSE 0 END FETCH FIRST 1 ROW ONLY",
    )?
    .first()
    .ok_or_else(|| {
        Error::Query(
            "Faltan las tablas de EXPLAIN de Db2 (EXPLAIN_INSTANCE, EXPLAIN_OPERATOR, EXPLAIN_STREAM…). \
             Crealas una vez con: CALL SYSPROC.SYSINSTALLOBJECTS('EXPLAIN', 'C', CAST(NULL AS VARCHAR(128)), CAST(NULL AS VARCHAR(128)))"
                .into(),
        )
    })?;
    set(c, slot, &format!("EXPLAIN PLAN FOR {stmt}"))?;
    let q = |t: &str| format!("\"{}\".{t}", schema.replace('"', "\"\""));
    let filter = format!(
        "EXPLAIN_REQUESTER IN (CURRENT USER, SESSION_USER) AND EXPLAIN_TIME = \
         (SELECT MAX(EXPLAIN_TIME) FROM {} WHERE EXPLAIN_REQUESTER IN (CURRENT USER, SESSION_USER))",
        q("EXPLAIN_INSTANCE")
    );
    let ops = collect(
        c,
        slot,
        &format!(
            "SELECT OPERATOR_ID, OPERATOR_TYPE, TOTAL_COST, IO_COST, CPU_COST, FIRST_ROW_COST FROM {} WHERE {filter} ORDER BY OPERATOR_ID",
            q("EXPLAIN_OPERATOR")
        ),
    )?;
    let streams = collect(
        c,
        slot,
        &format!(
            "SELECT SOURCE_TYPE, SOURCE_ID, TARGET_TYPE, TARGET_ID, RTRIM(OBJECT_SCHEMA) || '.' || RTRIM(OBJECT_NAME), STREAM_COUNT \
             FROM {} WHERE {filter}",
            q("EXPLAIN_STREAM")
        ),
    )?;
    let s = |r: &[Option<String>], i: usize| r.get(i).cloned().flatten().unwrap_or_default().trim().to_string();
    let f = |r: &[Option<String>], i: usize| plan::number(&s(r, i));
    let ops: Vec<plan::Db2Op> = ops
        .sets
        .iter()
        .flat_map(|(_, rows)| rows)
        .map(|r| plan::Db2Op {
            id: s(r, 0),
            kind: s(r, 1),
            total_cost: f(r, 2),
            io_cost: f(r, 3),
            cpu_cost: f(r, 4),
            first_row_cost: f(r, 5),
        })
        .collect();
    let streams: Vec<plan::Db2Stream> = streams
        .sets
        .iter()
        .flat_map(|(_, rows)| rows)
        .map(|r| plan::Db2Stream {
            source_type: s(r, 0),
            source_id: s(r, 1),
            target_type: s(r, 2),
            target_id: s(r, 3),
            object: s(r, 4),
            count: f(r, 5),
        })
        .collect();
    if ops.is_empty() {
        return Err(Error::Query("Db2 no dejó el plan en las tablas de EXPLAIN.".into()));
    }
    let root = plan::db2_tree(&ops, &streams);
    Ok(Plan { statement: stmt.to_string(), raw: plan::render(&root), root, actual: false, raw_format: "text".into() })
}

fn db2zos(c: &Conn, slot: &StmtSlot, stmt: &str) -> Result<Plan> {
    let qno = 1_000_000
        + std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_micros() as u64 % 1_000_000);
    set(c, slot, &format!("EXPLAIN PLAN SET QUERYNO = {qno} FOR {stmt}"))?;
    let rows = collect(
        c,
        slot,
        &format!(
            "SELECT QBLOCKNO, PLANNO, METHOD, RTRIM(CREATOR) || '.' || RTRIM(TNAME), ACCESSTYPE, MATCHCOLS, \
             RTRIM(ACCESSCREATOR) || '.' || RTRIM(ACCESSNAME), INDEXONLY, PREFETCH \
             FROM PLAN_TABLE WHERE QUERYNO = {qno} ORDER BY QBLOCKNO, PLANNO"
        ),
    );
    let cost = collect(c, slot, &format!("SELECT PROCMS, PROCSU, COST_CATEGORY, TOTAL_COST FROM DSN_STATEMNT_TABLE WHERE QUERYNO = {qno}"));
    for t in ["PLAN_TABLE", "DSN_STATEMNT_TABLE"] {
        let _ = set(c, slot, &format!("DELETE FROM {t} WHERE QUERYNO = {qno}"));
    }
    let s = |r: &[Option<String>], i: usize| r.get(i).cloned().flatten().unwrap_or_default().trim().trim_matches('.').to_string();
    let rows: Vec<plan::ZRow> = rows?
        .sets
        .iter()
        .flat_map(|(_, rows)| rows)
        .map(|r| plan::ZRow {
            qblock: s(r, 0),
            method: s(r, 2),
            table: s(r, 3),
            access_type: s(r, 4),
            match_cols: s(r, 5),
            index: s(r, 6),
            index_only: s(r, 7),
            prefetch: s(r, 8),
        })
        .collect();
    let mut root = plan::db2zos_tree(&rows);
    if let Ok(cost) = cost {
        if let Some(r) = cost.sets.first().and_then(|(_, rows)| rows.first()) {
            root.total_cost = plan::number(&s(r, 3));
            for (i, k) in [(0, "Tiempo estimado (ms)"), (1, "Unidades de servicio"), (2, "Categoría de costo")] {
                let v = s(r, i);
                if !v.is_empty() {
                    root.props.push((k.into(), v));
                }
            }
        }
    }
    Ok(Plan { statement: stmt.to_string(), raw: plan::render(&root), root, actual: false, raw_format: "text".into() })
}

// ---------------------------------------------------------- Sybase ASE

fn sybase(c: &Conn, slot: &StmtSlot, stmts: &[String], analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
    set(c, slot, "SET SHOWPLAN ON")?;
    if !analyze {
        if let Err(e) = set(c, slot, "SET NOEXEC ON") {
            let _ = set(c, slot, "SET SHOWPLAN OFF");
            return Err(e);
        }
    }
    let result = (|| -> Result<()> {
        for s in stmts {
            let text = if analyze {
                let mut local = QueryOutcome::default();
                let r = crate::run_one(c, slot, s, max_rows, &mut local);
                out.results.append(&mut local.results);
                r?;
                local.messages.join("\n")
            } else {
                collect(c, slot, s)?.messages.join("\n")
            };
            let trees = plan::sybase_trees(&text);
            if trees.is_empty() {
                out.messages.push(format!("El servidor no devolvió plan para «{}».", short(s)));
            }
            for root in trees {
                out.plans.push(Plan { statement: s.trim().to_string(), root, actual: false, raw_format: "text".into(), raw: text.clone() });
            }
        }
        Ok(())
    })();
    // NOEXEC OFF is the one statement NOEXEC still runs.
    let off = if analyze { Ok(()) } else { set(c, slot, "SET NOEXEC OFF") };
    let off2 = set(c, slot, "SET SHOWPLAN OFF");
    if analyze && result.is_ok() {
        out.messages.push("Sybase ASE no da cifras reales por operador por ODBC: se muestra el plan con el resultado.".into());
    }
    result.and(off).and(off2)
}

// ------------------------------------------------------------- Exasol

fn exasol(c: &Conn, slot: &StmtSlot, stmts: &[String], max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
    let before = collect(c, slot, "SELECT CURRENT_STATEMENT")?.first().and_then(|v| plan::number(&v)).unwrap_or(0.0) as i64;
    set(c, slot, "ALTER SESSION SET PROFILE = 'ON'")?;
    let ran = (|| -> Result<()> {
        for s in stmts {
            crate::run_one(c, slot, s, max_rows, out)?;
        }
        Ok(())
    })();
    let off = set(c, slot, "ALTER SESSION SET PROFILE = 'OFF'");
    ran?;
    off?;
    set(c, slot, "FLUSH STATISTICS")?;
    let got = collect(
        c,
        slot,
        &format!(
            "SELECT STMT_ID, COMMAND_NAME, PART_NAME, PART_INFO, OBJECT_SCHEMA, OBJECT_NAME, OBJECT_ROWS, OUT_ROWS, \
             DURATION, CPU, TEMP_DB_RAM_PEAK, REMARKS \
             FROM EXA_STATISTICS.EXA_USER_PROFILE_LAST_DAY \
             WHERE SESSION_ID = CURRENT_SESSION AND STMT_ID > {before} \
             AND COMMAND_NAME NOT IN ('ALTER SESSION', 'FLUSH STATISTICS') ORDER BY STMT_ID, PART_ID"
        ),
    )?;
    let s = |r: &[Option<String>], i: usize| r.get(i).cloned().flatten().unwrap_or_default().trim().to_string();
    let mut groups: Vec<(String, Vec<plan::ExaPart>)> = Vec::new();
    for r in got.sets.iter().flat_map(|(_, rows)| rows) {
        let id = s(r, 0);
        let object = [s(r, 4), s(r, 5)].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(".");
        let part = plan::ExaPart {
            name: s(r, 2),
            info: s(r, 3),
            object,
            object_rows: plan::number(&s(r, 6)),
            out_rows: plan::number(&s(r, 7)),
            duration_s: plan::number(&s(r, 8)),
            cpu: plan::number(&s(r, 9)),
            mem_mib: plan::number(&s(r, 10)),
            remarks: s(r, 11),
        };
        match groups.last_mut() {
            Some((g, parts)) if *g == id => parts.push(part),
            _ => groups.push((id, vec![part])),
        }
    }
    if groups.is_empty() {
        out.messages.push("Exasol no registró el perfil de la ejecución (EXA_USER_PROFILE_LAST_DAY).".into());
    }
    for (i, (_, parts)) in groups.into_iter().enumerate() {
        let root = plan::exasol_tree(&parts);
        let statement = stmts.get(i).map(|s| s.trim().to_string()).unwrap_or_default();
        out.plans.push(Plan { statement, raw: plan::render(&root), root, actual: true, raw_format: "text".into() });
    }
    Ok(())
}

// ------------------------------------------------------------- OpenEdge

/// Runs each statement, then reads its plan from `_Sql_Qplan` (the
/// connection keeps the last ten).
fn openedge(c: &Conn, slot: &StmtSlot, stmts: &[String], max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
    for s in stmts {
        crate::run_one(c, slot, s, max_rows, out)?;
        if !plannable(s) {
            continue;
        }
        let got = collect(
            c,
            slot,
            "SELECT \"_Description\" FROM PUB.\"_Sql_Qplan\"
              WHERE \"_Pnumber\" = (SELECT MAX(\"_Pnumber\") FROM PUB.\"_Sql_Qplan\" WHERE \"_Ptype\" > 0)",
        );
        match got {
            Ok(g) => {
                let text = g.lines().join("\n");
                if text.trim().is_empty() {
                    out.messages.push(format!("OpenEdge no registró el plan de «{}».", short(s)));
                } else {
                    let mut p = plan_from_text(s.trim(), &text, false);
                    p.actual = true;
                    out.plans.push(p);
                }
            }
            Err(e) => out.messages.push(format!("No se pudo leer _Sql_Qplan: {e}")),
        }
    }
    Ok(())
}

// ------------------------------------------------------------- CUBRID

fn cubrid(c: &Conn, slot: &StmtSlot, stmts: &[String], max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
    set(c, slot, "SET TRACE ON OUTPUT TEXT")?;
    let result = (|| -> Result<()> {
        for s in stmts {
            crate::run_one(c, slot, s, max_rows, out)?;
            if !plannable(s) {
                continue;
            }
            let text = collect(c, slot, "SHOW TRACE")?.lines().join("\n");
            if !text.trim().is_empty() {
                out.plans.push(Plan {
                    statement: s.trim().to_string(),
                    root: plan::cubrid_tree(&text),
                    actual: true,
                    raw_format: "text".into(),
                    raw: text,
                });
            }
        }
        Ok(())
    })();
    let off = set(c, slot, "SET TRACE OFF");
    result.and(off)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialects() {
        assert_eq!(dialect("odbc", "Microsoft SQL Server"), Dialect::SqlServer);
        assert_eq!(dialect("odbc", "DB2/LINUXX8664"), Dialect::Db2Luw);
        assert_eq!(dialect("odbc", "Adaptive Server Enterprise"), Dialect::Sybase);
        assert_eq!(dialect("odbc", "EXASolution"), Dialect::Exasol);
        assert_eq!(dialect("odbc", "PostgreSQL"), Dialect::Text);
        assert_eq!(dialect("db2", ""), Dialect::Db2Luw);
        assert!(matches!(dialect("informix", ""), Dialect::Unsupported(_)));
        assert!(preset_supports_explain("teradata") && preset_supports_explain("odbc"));
        assert!(!preset_supports_explain("gbase8s") && !preset_supports_explain("db2i") && !preset_supports_explain("altibase"));
        assert_eq!(dialect("heavydb", ""), Dialect::Prefixed("EXPLAIN CALCITE"));
        assert!(preset_supports_explain("openedge") && preset_supports_explain("virtuoso") && preset_supports_explain("sqream"));
        for p in ["ingres", "mimer", "cache", "zen", "access", "dbase", "netsuite"] {
            assert!(!preset_supports_explain(p), "{p}");
        }
        assert_eq!(dialect("cloudera", ""), Dialect::Hive);
        // Every preset has a dialect of its own (the generic one goes by the DBMS name).
        for p in crate::presets::PRESETS.iter().filter(|p| !p.is_generic()) {
            assert!(by_preset(p.id).is_some(), "{} has no plan dialect", p.id);
        }
    }

    #[test]
    fn what_has_a_plan() {
        assert!(plannable("  select 1"));
        assert!(plannable("-- c\nWITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(plannable("/* c */ (SELECT 1) UNION (SELECT 2)"));
        assert!(plannable("SEL * FROM t"));
        assert!(!plannable("CREATE TABLE t (a int)"));
        assert!(!plannable("SET SCHEMA x"));
    }
}
