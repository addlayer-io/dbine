//! The editor's completion (#97) for Solr, from static catalogs filtered by
//! the server's version (Solr lists neither):
//! - its function queries (used in `fl`, `sort`, boosts and `{!func}`), in
//!   the console's JSON;
//! - on SolrCloud where Parallel SQL runs, its functions, with the language
//!   `sql`: the editor offers them only on the console's SQL lines
//!   (`SELECT`…), which go to the `/sql` handler. A standalone server runs no
//!   SQL.
//!
//! Parallel SQL is built in before 9.0; from 9.0 it is the `sql` module,
//! which has to be installed. Every collection declares `/sql` either way
//! (an implicit handler, so the config API lists it), but without the module
//! its class doesn't load. So the catalog asks the first collection's `/sql`
//! once, with no statement: the handler answers its own error ("stmt
//! parameter cannot be null") without running anything ([`sql_runs`]).

use crate::SolrSession;
use dbine_driver::functions::{parse_catalog, FunctionInfo};
use dbine_driver_elasticsearch::http;
use dbine_driver_elasticsearch::json::J;
use std::time::Duration;

const SOLR: &str = include_str!("functions/solr.txt");
const SOLR_SQL: &str = include_str!("functions/solr_sql.txt");

/// How long each request of the `/sql` check may take.
const TIMEOUT: Duration = Duration::from_secs(10);

pub async fn catalog(s: &SolrSession) -> Vec<FunctionInfo> {
    let sql = s.cloud && sql_handler_runs(s).await;
    for_version(Some(&s.version), sql)
}

/// The function queries of `version` and, with `sql`, Parallel SQL's
/// functions.
pub fn for_version(version: Option<&str>, sql: bool) -> Vec<FunctionInfo> {
    let mut out = parse_catalog(SOLR, version);
    if sql {
        out.extend(
            parse_catalog(SOLR_SQL, version)
                .into_iter()
                .map(|f| f.language("sql")),
        );
    }
    out
}

/// Whether Parallel SQL runs on this SolrCloud, asked of its first
/// collection (see [`sql_runs`]). Without a collection there is nothing to
/// run SQL on: no. A list that can't be read can't tell: yes, as the console
/// sends SQL there either way.
async fn sql_handler_runs(s: &SolrSession) -> bool {
    let list = s
        .client
        .get(format!(
            "{}/solr/admin/collections?action=LIST&wt=json",
            s.base
        ))
        .timeout(TIMEOUT);
    let collection = match http::send(list).await {
        Ok((status, body)) if status < 400 => match J::parse(&body) {
            Ok(j) => first_collection(&j),
            Err(_) => return true,
        },
        _ => return true,
    };
    let Some(collection) = collection else {
        return false;
    };
    let probe = s
        .client
        .get(format!("{}/solr/{collection}/sql?wt=json", s.base))
        .timeout(TIMEOUT);
    let reply = http::send(probe).await.ok();
    sql_runs(reply.as_ref().map(|(st, b)| (*st, b.as_str())))
}

/// The first name in a `collections?action=LIST` answer.
pub fn first_collection(list: &J) -> Option<String> {
    list.get("collections")?
        .as_arr()?
        .iter()
        .find_map(|c| c.as_str().filter(|n| !n.is_empty()).map(str::to_string))
}

/// Whether Parallel SQL runs, from the answer of a collection's `/sql` to a
/// call without a statement (`None`: no answer at all):
/// - no: 404 (no such handler), or an error loading its class (Solr 9+
///   without the `sql` module: `Error loading class 'solr.SQLHandler'`, a
///   `ClassNotFoundException`);
/// - yes: the handler's own answer (a `result-set` with "stmt parameter
///   cannot be null"), and whatever can't tell (no privilege, another
///   server error, no answer), as the console sends SQL there either way.
pub fn sql_runs(reply: Option<(u16, &str)>) -> bool {
    match reply {
        Some((404, _)) => false,
        Some((status, body)) if status >= 400 => {
            !(body.contains("ClassNotFoundException") || body.contains("Error loading class"))
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::FunctionKind;

    #[test]
    fn the_catalogs_parse_line_by_line() {
        for text in [SOLR, SOLR_SQL] {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
            }
        }
        let all = for_version(None, false);
        assert!(all.iter().all(|f| f.kind == FunctionKind::Scalar));
        assert!(all.iter().all(|f| f.language.is_empty()));
        let recip = all.iter().find(|f| f.name == "recip").unwrap();
        assert_eq!(recip.args, "(x, m, a, b)");
        assert_eq!(
            all.iter().filter(|f| f.name == "vectorSimilarity").count(),
            2
        );
    }

    #[test]
    fn the_servers_version_filters_functions() {
        let has = |v, n: &str| for_version(Some(v), false).iter().any(|f| f.name == n);
        assert!(has("Apache Solr 9.8.1", "vectorSimilarity"));
        assert!(!has("Apache Solr 9.2.1", "vectorSimilarity"));
        assert!(has("Apache Solr 9.2.1", "isnan"));
        assert!(!has("Apache Solr 8.11.4 (SolrCloud)", "isnan"));
        assert!(has("Apache Solr 8.11.4 (SolrCloud)", "recip"));
        assert!(!has("Apache Solr 9.8.1", "lateVector"));
        assert!(has("Apache Solr 10.1.0", "lateVector"));
    }

    #[test]
    fn parallel_sql_adds_its_functions() {
        let sql = |v, runs| {
            for_version(Some(v), runs)
                .into_iter()
                .filter(|f| f.language == "sql")
                .collect::<Vec<_>>()
        };
        assert!(sql("Apache Solr 9.10.1 (SolrCloud)", false).is_empty());
        let c = sql("Apache Solr 9.10.1 (SolrCloud)", true);
        assert!(c.iter().all(|f| f.kind == FunctionKind::Aggregate));
        let names: Vec<&str> = c.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "COUNT",
                "COUNT",
                "APPROX_COUNT_DISTINCT",
                "MIN",
                "MAX",
                "SUM",
                "AVG"
            ]
        );
        assert_eq!(c[0].args, "(*)");
        let old = sql("Apache Solr 8.9.0 (SolrCloud)", true);
        assert!(!old.iter().any(|f| f.name == "APPROX_COUNT_DISTINCT"));
        assert_eq!(old.iter().filter(|f| f.name == "COUNT").count(), 1);
        // The function queries stay in the JSON's language.
        assert!(for_version(Some("Apache Solr 9.10.1"), true)
            .iter()
            .any(|f| f.name == "recip" && f.language.is_empty()));
    }

    #[test]
    fn the_sql_handlers_answer_says_whether_it_runs() {
        // Its own error: it runs (Solr 9.10.1 with the `sql` module).
        let own =
            r#"{"result-set":{"docs":[{"EXCEPTION":"stmt parameter cannot be null","EOF":true}]}}"#;
        assert!(sql_runs(Some((200, own))));
        // Solr 9.10.1 without the module: the implicit handler's class
        // doesn't load.
        let missing = r#"{"error":{"metadata":["error-class","org.apache.solr.common.SolrException","root-error-class","java.lang.ClassNotFoundException"],"msg":" Error loading class 'solr.SQLHandler'","code":500}}"#;
        assert!(!sql_runs(Some((500, missing))));
        assert!(!sql_runs(Some((404, "<p>Searching for Solr?</p>"))));
        // Can't tell: the console sends SQL there either way.
        for reply in [
            Some((401, "")),
            Some((403, "")),
            Some((500, r#"{"error":{"msg":"other"}}"#)),
            Some((503, "")),
            None,
        ] {
            assert!(sql_runs(reply), "{reply:?}");
        }
    }

    #[test]
    fn the_first_collection_is_asked() {
        let j = |s: &str| J::parse(s).unwrap();
        assert_eq!(
            first_collection(&j(
                r#"{"responseHeader":{"status":0},"collections":["a","b"]}"#
            )),
            Some("a".into())
        );
        assert_eq!(first_collection(&j(r#"{"collections":[]}"#)), None);
        assert_eq!(first_collection(&j("{}")), None);
    }
}
