//! The server's names the forms' `Ref` fields pick from (logins,
//! databases, categories, operators, proxies…), read in one batch: each
//! list after a marker row naming it, guarded by the permission it needs
//! (a list the login can't read comes empty instead of failing the form).

use super::Ctx;
use crate::create_db::literal;
use crate::jobs::{self, describe, text, ResultSet, ScheduleRow};
use crate::SqlServerSession;
use dbine_driver::Result;
use std::collections::BTreeMap;

/// What a step runs as when it has no proxy.
pub(crate) const SERVICE_ACCOUNT: &str = "Cuenta de servicio del Agente SQL Server";
/// An alert's database when it watches all of them.
pub(crate) const ALL_DATABASES: &str = "(todas las bases de datos)";

/// Lists by source (`login`, `category:job`, `proxy:CmdExec`…), each of
/// `(value, label)`.
#[derive(Debug, Default, Clone)]
pub(crate) struct Catalog {
    pub(crate) lists: BTreeMap<String, Vec<(String, String)>>,
}

impl Catalog {
    pub(crate) fn get(&self, source: &str) -> &[(String, String)] {
        self.lists.get(source).map_or(&[], Vec::as_slice)
    }

    /// The values of a source.
    pub(crate) fn values(&self, source: &str) -> Vec<String> {
        self.get(source).iter().map(|(v, _)| v.clone()).collect()
    }

    /// The subsystems the Agent lists (`TSQL`, `CmdExec`…).
    pub(crate) fn subsystems(&self) -> Vec<String> {
        self.values("subsystem")
    }
}

/// The SQL for one source, after its marker.
fn source_sql(source: &str, sysadmin: bool) -> Option<String> {
    let sql = match source {
        "login" => "SELECT name AS v, name AS l FROM sys.server_principals WHERE type IN ('S', 'U', 'G', 'E', 'X') AND name NOT LIKE N'##%' ORDER BY name;",
        "database" => "SELECT name AS v, name AS l FROM sys.databases ORDER BY name;",
        "category" => "SELECT name AS v, name AS l, CASE category_class WHEN 1 THEN N'job' WHEN 2 THEN N'alert' ELSE N'operator' END AS x FROM msdb.dbo.syscategories ORDER BY name;",
        "operator" => "IF HAS_PERMS_BY_NAME(N'msdb.dbo.sp_help_operator', N'OBJECT', N'EXECUTE') = 1 EXEC msdb.dbo.sp_help_operator;",
        "alert" => "IF HAS_PERMS_BY_NAME(N'msdb.dbo.sysalerts', N'OBJECT', N'SELECT') = 1 SELECT name AS v, name AS l FROM msdb.dbo.sysalerts ORDER BY name;",
        "job" => "SELECT LOWER(CONVERT(nvarchar(36), job_id)) AS v, name AS l FROM msdb.dbo.sysjobs_view ORDER BY name;",
        "schedule" => "SELECT schedule_id, schedule_uid, name AS schedule_name, enabled, freq_type, freq_interval, freq_subday_type, freq_subday_interval, freq_relative_interval, freq_recurrence_factor, active_start_date, active_end_date, active_start_time, active_end_time FROM msdb.dbo.sysschedules_localserver_view ORDER BY name;",
        "credential" => "SELECT name AS v, name AS l FROM sys.credentials ORDER BY name;",
        "msdb_role" => "SELECT name AS v, name AS l FROM msdb.sys.database_principals WHERE type = 'R' AND name <> N'public' ORDER BY name;",
        "server_role" => "SELECT name AS v, name AS l FROM sys.server_principals WHERE type = 'R' AND name <> N'public' ORDER BY name;",
        "target_server" => "IF HAS_PERMS_BY_NAME(N'msdb.dbo.systargetservers', N'OBJECT', N'SELECT') = 1 SELECT server_name AS v, server_name AS l FROM msdb.dbo.systargetservers ORDER BY server_name;",
        "perf_object" => "IF HAS_PERMS_BY_NAME(NULL, NULL, N'VIEW SERVER STATE') = 1 SELECT DISTINCT RTRIM(object_name) AS v, RTRIM(object_name) AS l FROM sys.dm_os_performance_counters ORDER BY 1;",
        "subsystem" => "IF HAS_PERMS_BY_NAME(N'msdb.dbo.sp_enum_sqlagent_subsystems', N'OBJECT', N'EXECUTE') = 1 EXEC msdb.dbo.sp_enum_sqlagent_subsystems;",
        // sysadmin reads msdb's tables; the rest only see what they may
        // use (see `proxy_sql`).
        "proxy" if sysadmin => "SELECT p.name AS v, p.name AS l, s.subsystem AS x FROM msdb.dbo.sysproxies p JOIN msdb.dbo.sysproxysubsystem ps ON ps.proxy_id = p.proxy_id JOIN msdb.dbo.syssubsystems s ON s.subsystem_id = ps.subsystem_id ORDER BY p.name;",
        _ => return None,
    };
    Some(sql.to_string())
}

/// The proxies a login that isn't sysadmin may use for each subsystem
/// (`sp_help_proxy` answers for the login itself).
fn proxy_sql(subsystems: &[String]) -> String {
    let mut sql =
        String::from("DECLARE @dbine_me nvarchar(256);\nSET @dbine_me = SUSER_SNAME();\n");
    for sub in subsystems
        .iter()
        .filter(|s| !s.eq_ignore_ascii_case("TSQL"))
    {
        sql.push_str(&format!(
            "SELECT {} AS dbine_source;\nEXEC msdb.dbo.sp_help_proxy @subsystem_name = {}, @name = @dbine_me;\n",
            literal(&format!("proxy:{sub}")),
            literal(sub)
        ));
    }
    sql
}

/// The batch for these sources.
fn batch(sources: &[&str], sysadmin: bool) -> String {
    let mut sql = String::from("SET NOCOUNT ON;\n");
    for source in sources {
        if let Some(q) = source_sql(source, sysadmin) {
            sql.push_str(&format!(
                "SELECT {} AS dbine_source;\n{q}\n",
                literal(source)
            ));
        }
    }
    sql
}

/// The lists from the batch's result sets.
fn read(sets: &[ResultSet], into: &mut Catalog) {
    let mut current: Option<String> = None;
    for set in sets {
        if set.has("dbine_source") {
            current = set.rows.first().and_then(|r| text(r, "dbine_source"));
            if let Some(c) = &current {
                into.lists.entry(c.clone()).or_default();
            }
            continue;
        }
        let Some(source) = current.clone() else {
            continue;
        };
        for r in &set.rows {
            match source.as_str() {
                "schedule" => {
                    if let (Some(id), Some(row)) =
                        (jobs::int(r, "schedule_id"), ScheduleRow::read(r))
                    {
                        // A pattern with words of its own, so the app
                        // finds it to translate (and the description in
                        // it, on its own).
                        into.lists.entry(source.clone()).or_default().push((
                            id.to_string(),
                            format!("{} · Cuándo: {}", row.name, describe(&row)),
                        ));
                    }
                }
                "operator" => {
                    if let Some(name) = text(r, "name") {
                        into.lists
                            .entry(source.clone())
                            .or_default()
                            .push((name.clone(), name));
                    }
                }
                "subsystem" => {
                    if let Some(sub) = text(r, "subsystem") {
                        into.lists
                            .entry(source.clone())
                            .or_default()
                            .push((sub.clone(), sub));
                    }
                }
                "category" => {
                    if let (Some(v), Some(x)) = (text(r, "v"), text(r, "x")) {
                        into.lists
                            .entry(format!("category:{x}"))
                            .or_default()
                            .push((v.clone(), v));
                    }
                }
                "proxy" => {
                    if let (Some(v), Some(x)) = (text(r, "v"), text(r, "x")) {
                        into.lists
                            .entry(format!("proxy:{x}"))
                            .or_default()
                            .push((v.clone(), v));
                    }
                }
                s if s.starts_with("proxy:") => {
                    if let Some(name) = text(r, "name") {
                        into.lists
                            .entry(source.clone())
                            .or_default()
                            .push((name.clone(), name));
                    }
                }
                _ => {
                    if let Some(v) = text(r, "v") {
                        let l = text(r, "l").unwrap_or_else(|| v.clone());
                        into.lists.entry(source.clone()).or_default().push((v, l));
                    }
                }
            }
        }
    }
}

/// Reads the sources (`proxy` needs `subsystem` and is split per
/// subsystem; `category` is split per class).
pub(crate) async fn load(s: &mut SqlServerSession, c: &Ctx, sources: &[&str]) -> Result<Catalog> {
    let mut catalog = Catalog::default();
    let mut wanted: Vec<&str> = sources.to_vec();
    let proxies = wanted.contains(&"proxy");
    if proxies && !wanted.contains(&"subsystem") {
        wanted.push("subsystem");
    }
    let sets = jobs::sets(s, &batch(&wanted, c.p.sysadmin), &[], None).await?;
    read(&sets, &mut catalog);
    if proxies && !c.p.sysadmin {
        let subs = catalog.subsystems();
        if !subs.is_empty() {
            let sets = jobs::sets(
                s,
                &format!("SET NOCOUNT ON;\n{}", proxy_sql(&subs)),
                &[],
                None,
            )
            .await?;
            read(&sets, &mut catalog);
        }
    }
    if proxies {
        // Every subsystem but T-SQL may run as the Agent's account.
        for sub in catalog.subsystems() {
            if sub.eq_ignore_ascii_case("TSQL") {
                continue;
            }
            let list = catalog.lists.entry(format!("proxy:{sub}")).or_default();
            list.dedup();
            list.insert(0, (String::new(), SERVICE_ACCOUNT.to_string()));
        }
    }
    for class in ["job", "alert", "operator"] {
        if sources.contains(&"category") {
            catalog
                .lists
                .entry(format!("category:{class}"))
                .or_default();
        }
    }
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_batch_marks_and_guards_each_list() {
        let sql = batch(&["login", "operator", "alert", "proxy", "subsystem"], true);
        assert!(sql.contains("SELECT N'login' AS dbine_source;\nSELECT name AS v"));
        assert!(sql.contains("IF HAS_PERMS_BY_NAME(N'msdb.dbo.sp_help_operator', N'OBJECT', N'EXECUTE') = 1 EXEC msdb.dbo.sp_help_operator;"));
        assert!(
            sql.contains("IF HAS_PERMS_BY_NAME(N'msdb.dbo.sysalerts', N'OBJECT', N'SELECT') = 1")
        );
        assert!(
            sql.contains("FROM msdb.dbo.sysproxies p"),
            "sysadmin reads the tables"
        );
        let user = batch(&["proxy"], false);
        assert!(!user.contains("sysproxies"), "{user}");
        // Unknown sources are left out, never spliced.
        assert_eq!(batch(&["x'; DROP TABLE t; --"], true), "SET NOCOUNT ON;\n");
    }

    #[test]
    fn proxies_for_a_login_go_subsystem_by_subsystem() {
        let sql = proxy_sql(&["TSQL".into(), "CmdExec".into(), "Power'Shell".into()]);
        assert!(!sql.contains("@subsystem_name = N'TSQL'"));
        assert!(sql.contains("SELECT N'proxy:CmdExec' AS dbine_source;\nEXEC msdb.dbo.sp_help_proxy @subsystem_name = N'CmdExec', @name = @dbine_me;"));
        assert!(sql.contains("@subsystem_name = N'Power''Shell'"), "{sql}");
    }
}
