//! "Calidad de código": the query editor's linter (see `crate::lint`). Text
//! analysis only: nothing reaches the database.

use crate::commands::query::Utf16;
use crate::commands::schema::driver_of;
use crate::error::CommandResult;
use crate::lint::{self, Profile, Rule, Severity, RULES};
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tauri::State;

#[derive(Deserialize)]
pub struct LintArgs {
    pub connection_id: String,
    pub sql: String,
}

/// A problem for the editor: offsets are JS string (UTF-16) indices.
#[derive(Serialize, Debug)]
pub struct LintFinding {
    pub rule: &'static str,
    pub severity: Severity,
    pub start: usize,
    pub end: usize,
    /// 1-based line of `start`.
    pub line: u32,
    /// Values the message shows (`{{fn}}`, `{{table}}`…).
    pub params: BTreeMap<&'static str, String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn lint_script(state: State<'_, AppState>, args: LintArgs) -> CommandResult<Vec<LintFinding>> {
    let driver = driver_of(&state, &args.connection_id)?;
    let profile = Profile::of(driver.info(), driver.script_dialect());
    Ok(findings(&args.sql, profile))
}

/// Every rule, for Settings.
#[tauri::command]
pub async fn lint_rules() -> CommandResult<&'static [Rule]> {
    Ok(RULES)
}

fn findings(sql: &str, profile: Profile) -> Vec<LintFinding> {
    let pos = Utf16::new(sql);
    let mut line = 1u32;
    let mut at = 0usize;
    lint::lint(sql, profile)
        .into_iter()
        .map(|f| {
            // Findings come in text order: count newlines once.
            line += sql.as_bytes()[at..f.start.max(at)].iter().filter(|&&c| c == b'\n').count() as u32;
            at = f.start.max(at);
            LintFinding {
                rule: f.rule,
                severity: lint::severity(f.rule),
                start: pos.at(f.start),
                end: pos.at(f.end),
                line,
                params: f.params.into_iter().collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_are_utf16_and_lines_count() {
        let p = Profile::of(dbine_drivers::find("postgres").unwrap().info(), dbine_drivers::find("postgres").unwrap().script_dialect());
        let f = findings("select 'ñandú😀';\n\nselect * from t", p);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule, "select-star");
        assert_eq!(f[0].line, 3);
        // 'ñandú😀' is 9 UTF-16 units and 13 bytes.
        let star = "select 'ñandú😀';\n\nselect ".encode_utf16().count();
        assert_eq!((f[0].start, f[0].end), (star, star + 1));
    }

    /// Each engine gets the rules of its family.
    #[test]
    fn engines_map_to_their_profile() {
        use crate::lint::Flavor;
        let profile = |id: &str| {
            let d = dbine_drivers::find(id).unwrap();
            Profile::of(d.info(), d.script_dialect())
        };
        assert!(matches!(profile("sqlserver"), Profile::Sql(_, Flavor::Tsql)));
        assert!(matches!(profile("postgres"), Profile::Sql(_, Flavor::Postgres)));
        assert!(matches!(profile("mysql"), Profile::Sql(_, Flavor::Mysql)));
        assert!(matches!(profile("oracle"), Profile::Sql(_, Flavor::Oracle)));
        assert!(matches!(profile("influxdb1"), Profile::Sql(_, Flavor::Influxql)));
        assert!(matches!(profile("sqlite"), Profile::Sql(_, Flavor::Generic)));
        assert!(matches!(profile("cassandra"), Profile::Cql(_)));
        assert_eq!(profile("mongodb"), Profile::Mongodb);
        assert_eq!(profile("couchdb"), Profile::Couchdb);
        assert_eq!(profile("elasticsearch"), Profile::Search);
        assert_eq!(profile("solr"), Profile::Search);
        assert_eq!(profile("redis"), Profile::Redis);
        assert_eq!(profile("etcd"), Profile::Etcd);
        assert_eq!(profile("neo4j"), Profile::Cypher);
        assert_eq!(profile("influxdb"), Profile::None);
    }

    /// The engine → rule groups mapping, printed for the report:
    /// `cargo test -p dbine lint_mapping -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn lint_mapping() {
        for d in dbine_drivers::all() {
            let i = d.info();
            println!("{}\t{:?}\t{}\t{:?}", i.id, i.language, i.dialect, Profile::of(i, d.script_dialect()));
        }
    }
}
