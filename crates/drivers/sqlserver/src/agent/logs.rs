//! SQL Server Agent's error log (`SQLAGENT.OUT` and its archives), as SSMS's
//! "Error Logs" node under the Agent shows it: `sp_enumerrorlogs 2` lists
//! the archives, `sp_readerrorlog <n>, 2` reads each. securityadmin may
//! read them on every version, VIEW SERVER STATE or VIEW ANY ERROR LOG from
//! SQL Server 2022.

use super::ready;
use crate::create_db::literal;
use crate::jobs::{self, datetime, int, text, ResultSet};
use crate::SqlServerSession;
use dbine_driver::{Error, MonitorTable, Result};

const NO_PERMISSION: &str = "Leer el registro del Agente SQL Server requiere el rol securityadmin (o, desde SQL Server 2022, VIEW SERVER STATE o VIEW ANY ERROR LOG).";
const CURRENT: &str = "Registro actual";

/// The archives the Agent keeps (current plus 9).
const MAX_ARCHIVES: i64 = 9;

fn read_sql(archives: &[i64]) -> String {
    let mut sql = String::from("SET NOCOUNT ON;\n");
    for n in archives {
        // Numbers the server listed, not text.
        sql.push_str(&format!(
            "SELECT {} AS dbine_log;\nEXEC sys.sp_readerrorlog {n}, 2;\n",
            literal(&n.to_string())
        ));
    }
    sql
}

fn level(n: Option<i64>) -> &'static str {
    match n {
        Some(1) => "Error",
        Some(2) => "Advertencia",
        _ => "Información",
    }
}

/// The logs from the batch's sets, each newest line first.
fn tables(sets: &[ResultSet], dates: &[(i64, String)]) -> Vec<MonitorTable> {
    let mut out = Vec::new();
    let mut current: Option<i64> = None;
    for set in sets {
        if set.has("dbine_log") {
            current = set
                .rows
                .first()
                .and_then(|r| text(r, "dbine_log"))
                .and_then(|t| t.parse().ok());
            continue;
        }
        let Some(n) = current.take() else { continue };
        let mut t = MonitorTable::new(
            "agent_log",
            "Registro del Agente",
            &["Fecha", "Tipo", "Mensaje"],
        );
        t.key = format!("agent_log_{n}");
        t.title = if n == 0 {
            CURRENT.to_string()
        } else {
            match dates.iter().find(|(a, _)| *a == n) {
                Some((_, d)) => format!("Archivo {n} del {d}"),
                None => format!("Archivo {n}"),
            }
        };
        let mut rows: Vec<Vec<serde_json::Value>> = set
            .rows
            .iter()
            .map(|r| {
                vec![
                    serde_json::json!(datetime(r, "LogDate")
                        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_default()),
                    serde_json::json!(level(int(r, "ErrorLevel"))),
                    serde_json::json!(text(r, "Text").unwrap_or_default()),
                ]
            })
            .collect();
        rows.reverse();
        t.rows = rows;
        out.push(t);
    }
    out
}

pub(crate) async fn logs(s: &mut SqlServerSession) -> Result<Vec<MonitorTable>> {
    let c = ready(s).await?;
    if !c.logs() {
        return Err(Error::Query(NO_PERMISSION.into()));
    }
    let list = jobs::sets(
        s,
        "SET NOCOUNT ON;\nEXEC sys.sp_enumerrorlogs 2;",
        &[],
        None,
    )
    .await?;
    let mut dates: Vec<(i64, String)> = list
        .iter()
        .flat_map(|s| s.rows.iter())
        .filter_map(|r| Some((int(r, "Archive #")?, text(r, "Date").unwrap_or_default())))
        .filter(|(n, _)| (1..=MAX_ARCHIVES).contains(n))
        .collect();
    dates.sort_by_key(|(n, _)| *n);
    let mut archives = vec![0];
    archives.extend(dates.iter().map(|(n, _)| *n));
    let sets = jobs::sets(s, &read_sql(&archives), &[], None).await?;
    Ok(tables(&sets, &dates))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_logs_are_read_by_number() {
        let sql = read_sql(&[0, 1, 2]);
        assert!(sql.contains("SELECT N'0' AS dbine_log;\nEXEC sys.sp_readerrorlog 0, 2;"));
        assert!(sql.contains("EXEC sys.sp_readerrorlog 2, 2;"));
        assert_eq!(level(Some(1)), "Error");
        assert_eq!(level(Some(2)), "Advertencia");
        assert_eq!(level(Some(3)), "Información");
    }
}
