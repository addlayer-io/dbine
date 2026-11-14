//! The editor's completion (#97) for the SQL lines of the console
//! (`SELECT`, `SHOW`, `DESCRIBE` and `DESC` lines, see [`crate::console`]).
//! Its requests are the query DSL, which has no functions to complete, so
//! every entry here has the language `sql`: the editor offers them only on
//! those lines.
//!
//! - **Elasticsearch**: `SHOW FUNCTIONS` (one request), with the arguments
//!   and result types of the static catalog, which is also the fallback when
//!   the server can't be asked (no privilege, a network error). A server
//!   without the SQL that the console runs offers nothing: before 7.0 (6.x
//!   serves SQL at `/_xpack/sql`, the console sends it to `/_sql`), the OSS
//!   distribution, SQL disabled.
//! - **OpenSearch** and **Open Distro**: their SQL plugin has no SHOW
//!   FUNCTIONS, so a static catalog of the plugin's documented functions,
//!   filtered by the plugin's version. `_cat/plugins` says whether the plugin
//!   is installed and its version; when it can't be read the catalog is
//!   offered anyway (the console sends SQL there either way).
//!
//! The async functions only make the requests; what their answers lead to
//! is decided by [`from_show_functions_reply`] and [`from_plugins`].

use crate::json::J;
use crate::{http, EsSession};
use dbine_driver::functions::{parse_catalog, version_at_least, FunctionInfo, FunctionKind};
use std::time::Duration;

const ELASTICSEARCH: &str = include_str!("functions/elasticsearch.txt");
const OPENSEARCH: &str = include_str!("functions/opensearch.txt");
const OPENDISTRO: &str = include_str!("functions/opendistro.txt");

/// The editor language of every entry: the console's SQL lines.
const SQL: &str = "sql";

/// Elasticsearch documents these but `SHOW FUNCTIONS` doesn't list them
/// (they are grammar, not registered functions).
const UNLISTED: &[&str] = &["EXTRACT", "MATCH", "QUERY"];

/// `SHOW FUNCTIONS` lists `CASE`, which is syntax (`CASE WHEN … END`), not a
/// call.
const NOT_A_CALL: &[&str] = &["CASE"];

/// How long a catalog request may take.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The first Elasticsearch with SQL at `/_sql`, where the console (and the
/// catalog's request) sends it. SQL arrived in 6.3, but 6.x serves it at
/// `/_xpack/sql`.
const ES_SQL_SINCE: &str = "7.0";

pub async fn catalog(s: &EsSession) -> Vec<FunctionInfo> {
    if s.opensearch || s.opendistro {
        plugin_catalog(s).await
    } else {
        elastic_catalog(s).await
    }
}

async fn elastic_catalog(s: &EsSession) -> Vec<FunctionInfo> {
    let version = s.version.as_str();
    // Without SQL at `/_sql`, nothing to ask.
    if !has_sql(version) {
        return Vec::new();
    }
    let rb = s
        .request("POST", "/_sql?format=json")
        .json(&serde_json::json!({ "query": "SHOW FUNCTIONS" }))
        .timeout(TIMEOUT);
    let reply = http::send(rb).await.ok();
    from_show_functions_reply(reply.as_ref().map(|(st, b)| (*st, b.as_str())), version)
}

async fn plugin_catalog(s: &EsSession) -> Vec<FunctionInfo> {
    let rb = s
        .request("GET", "/_cat/plugins?format=json&h=component,version")
        .timeout(TIMEOUT);
    let plugins = match http::send(rb).await {
        Ok((status, body)) if status < 400 => J::parse(&body).ok(),
        _ => None,
    };
    from_plugins(plugins.as_ref(), s.opendistro, &s.version)
}

/// Whether an Elasticsearch `version` has SQL at `/_sql` (an unreadable one
/// counts as new enough).
fn has_sql(version: &str) -> bool {
    version_at_least(version, ES_SQL_SINCE)
}

/// The catalog from the answer to `SHOW FUNCTIONS` on `/_sql` (`None`: no
/// answer at all) of a server at `version`:
/// - an answer: its rows ([`from_show_functions`]), or the static catalog
///   when the body isn't JSON;
/// - 400, 404 or 405: no SQL endpoint (the OSS distribution, SQL turned
///   off), nothing;
/// - anything else (no privilege to run it, a server error, a network
///   failure): the static catalog.
pub fn from_show_functions_reply(reply: Option<(u16, &str)>, version: &str) -> Vec<FunctionInfo> {
    match reply {
        Some((status, body)) if status < 400 => match J::parse(body) {
            Ok(j) => from_show_functions(&j, version),
            Err(_) => elasticsearch(Some(version)),
        },
        Some((400 | 404 | 405, _)) => Vec::new(),
        _ => elasticsearch(Some(version)),
    }
}

/// The plugin's catalog from `_cat/plugins?format=json` (`None`: it couldn't
/// be read) on OpenSearch, or Open Distro when `open_distro`, whose server
/// gives `server_version`:
/// - read without the SQL plugin: nothing;
/// - read with it: the catalog of the plugin's version;
/// - unreadable: OpenSearch's plugin goes with OpenSearch's version, so the
///   server's filters it; Open Distro's has its own numbers (the server
///   gives Elasticsearch's), so no filter.
pub fn from_plugins(
    plugins: Option<&J>,
    open_distro: bool,
    server_version: &str,
) -> Vec<FunctionInfo> {
    let version = match plugins.map(sql_plugin) {
        Some(None) => return Vec::new(),
        Some(Some(v)) => Some(v),
        None if open_distro => None,
        None => Some(server_version.to_string()),
    };
    if open_distro {
        opendistro(version.as_deref())
    } else {
        opensearch(version.as_deref())
    }
}

fn sql_entries(text: &str, version: Option<&str>) -> Vec<FunctionInfo> {
    parse_catalog(text, version)
        .into_iter()
        .map(|f| f.language(SQL))
        .collect()
}

/// The static Elasticsearch catalog for `version`: nothing before SQL
/// existed.
pub fn elasticsearch(version: Option<&str>) -> Vec<FunctionInfo> {
    if version.is_some_and(|v| !has_sql(v)) {
        return Vec::new();
    }
    sql_entries(ELASTICSEARCH, version)
}

pub fn opensearch(version: Option<&str>) -> Vec<FunctionInfo> {
    sql_entries(OPENSEARCH, version)
}

pub fn opendistro(version: Option<&str>) -> Vec<FunctionInfo> {
    sql_entries(OPENDISTRO, version)
}

/// The rows of `SHOW FUNCTIONS` (`name`, `type`): `AGGREGATE` is an
/// aggregate, the rest (`SCALAR`, `CONDITIONAL`, `GROUPING`, `SCORE`) are
/// called where an expression goes. Each takes its arguments and result
/// type from the static catalog (all of a name's forms); one the catalog
/// doesn't know comes bare. The documented functions the server doesn't
/// list are added.
pub fn from_show_functions(resp: &J, version: &str) -> Vec<FunctionInfo> {
    let known = parse_catalog(ELASTICSEARCH, None);
    let cols = resp.get("columns").and_then(J::as_arr).unwrap_or(&[]);
    let col = |name: &str| {
        cols.iter()
            .position(|c| c.get("name").and_then(J::as_str) == Some(name))
    };
    let (Some(ni), Some(ti)) = (col("name"), col("type")) else {
        return elasticsearch(Some(version));
    };
    let mut out = Vec::new();
    for row in resp.get("rows").and_then(J::as_arr).unwrap_or(&[]) {
        let cell = |i: usize| row.as_arr().and_then(|r| r.get(i)).map(J::text);
        let Some(name) = cell(ni).filter(|n| !n.is_empty()) else {
            continue;
        };
        if NOT_A_CALL.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            continue;
        }
        let kind = match cell(ti).unwrap_or_default().as_str() {
            "AGGREGATE" => FunctionKind::Aggregate,
            _ => FunctionKind::Scalar,
        };
        let forms: Vec<&FunctionInfo> = known
            .iter()
            .filter(|f| f.name.eq_ignore_ascii_case(&name))
            .collect();
        if forms.is_empty() {
            out.push(FunctionInfo::new(name, kind).language(SQL));
            continue;
        }
        for f in forms {
            out.push(
                FunctionInfo::new(&name, kind)
                    .args(f.args.clone())
                    .returns(f.returns.clone())
                    .language(SQL),
            );
        }
    }
    out.extend(
        elasticsearch(Some(version))
            .into_iter()
            .filter(|f| UNLISTED.contains(&f.name.as_str())),
    );
    out
}

/// The SQL plugin among `_cat/plugins?format=json` rows (`opensearch-sql`,
/// `opendistro-sql`, `opendistro_sql`): its version, or `None` when it
/// isn't installed. On several nodes the first row counts.
pub fn sql_plugin(rows: &J) -> Option<String> {
    rows.as_arr()?.iter().find_map(|r| {
        let component = r.get("component").and_then(J::as_str)?;
        (component.ends_with("-sql") || component.ends_with("_sql"))
            .then(|| r.get("version").map(J::text).unwrap_or_default())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn each_line_parses(text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
    }

    #[test]
    fn the_catalogs_parse_line_by_line() {
        for text in [ELASTICSEARCH, OPENSEARCH, OPENDISTRO] {
            each_line_parses(text);
        }
        for all in [elasticsearch(None), opensearch(None), opendistro(None)] {
            assert!(all.iter().all(|f| f.language == "sql"));
            assert!(all.iter().all(|f| f.description.is_empty()));
            assert!(!all.iter().any(|f| f.name == "CASE"));
            assert!(all.iter().all(|f| matches!(
                f.kind,
                FunctionKind::Scalar | FunctionKind::Aggregate | FunctionKind::Window
            )));
        }
        let es = elasticsearch(None);
        let avg = es.iter().find(|f| f.name == "AVG").unwrap();
        assert_eq!(avg.kind, FunctionKind::Aggregate);
        assert_eq!(avg.args, "(numeric_field)");
        assert_eq!(avg.returns, "double");
        assert_eq!(es.iter().filter(|f| f.name == "COUNT").count(), 3);
        let os = opensearch(None);
        assert_eq!(
            os.iter().find(|f| f.name == "ROW_NUMBER").unwrap().kind,
            FunctionKind::Window
        );
    }

    #[test]
    fn versions_filter_the_static_catalogs() {
        let has = |c: Vec<FunctionInfo>, n: &str| c.iter().any(|f| f.name == n);
        // Elasticsearch: SQL at `/_sql` from 7.0 (6.x has it at
        // `/_xpack/sql`), functions as SHOW FUNCTIONS added them.
        assert!(elasticsearch(Some("Elasticsearch 6.2.4")).is_empty());
        assert!(elasticsearch(Some("Elasticsearch 6.8.23")).is_empty());
        assert!(has(elasticsearch(Some("Elasticsearch 7.0.0")), "AVG"));
        assert!(!has(elasticsearch(Some("Elasticsearch 7.0.0")), "TO_CHAR"));
        assert!(has(elasticsearch(Some("Elasticsearch 7.17.0")), "TO_CHAR"));
        assert!(!has(
            elasticsearch(Some("Elasticsearch 8.4.3")),
            "DATE_FORMAT"
        ));
        assert!(has(
            elasticsearch(Some("Elasticsearch 8.15.3")),
            "DATE_FORMAT"
        ));
        // OpenSearch: the plugin's version is OpenSearch's.
        assert!(!has(opensearch(Some("2.3.0.0")), "TYPEOF"));
        assert!(has(opensearch(Some("2.4.0.0")), "TYPEOF"));
        assert!(!has(opensearch(Some("2.14.0.0")), "PERCENTILE"));
        assert!(has(opensearch(Some("OpenSearch 2.17.1")), "PERCENTILE"));
        assert!(has(opensearch(Some("1.0.0.0")), "MATCH_QUERY"));
        assert!(!has(opensearch(Some("1.0.0.0")), "MATCH"));
        // Open Distro: its plugin's own numbers.
        assert!(!has(opendistro(Some("1.10.0.0")), "DAYNAME"));
        assert!(has(opendistro(Some("1.13.2.0")), "DAYNAME"));
        assert!(!has(opendistro(Some("1.12.0.0")), "NULLIF"));
        assert!(has(opendistro(None), "NULLIF"));
        for n in ["CONV", "CRC32", "MOD", "TRUNCATE"] {
            assert!(!has(opendistro(Some("1.9.0.0")), n), "{n}");
            assert!(has(opendistro(Some("1.9.0.1")), n), "{n}");
        }
        assert!(has(opendistro(Some("1.13.2.0")), "MODULUS"));
    }

    fn names(c: Vec<FunctionInfo>) -> std::collections::BTreeSet<String> {
        c.into_iter().map(|f| f.name).collect()
    }

    #[test]
    fn the_show_functions_reply_decides_the_elasticsearch_catalog() {
        let v = "Elasticsearch 8.15.3";
        let fallback = names(elasticsearch(Some(v)));
        assert!(fallback.contains("DATE_FORMAT"));
        // An answer: its rows, and the documented functions it doesn't list.
        let listed = r#"{"columns":[{"name":"name","type":"keyword"},{"name":"type","type":"keyword"}],
                         "rows":[["AVG","AGGREGATE"]]}"#;
        assert_eq!(
            names(from_show_functions_reply(Some((200, listed)), v)),
            std::collections::BTreeSet::from(
                ["AVG", "EXTRACT", "MATCH", "QUERY"].map(String::from)
            )
        );
        // A 2xx that isn't JSON: the static catalog.
        assert_eq!(
            names(from_show_functions_reply(Some((200, "<html></html>")), v)),
            fallback
        );
        // No SQL endpoint: nothing.
        for status in [400, 404, 405] {
            assert!(
                from_show_functions_reply(Some((status, r#"{"error":"x"}"#)), v).is_empty(),
                "{status}"
            );
        }
        // Not asked (no privilege, a server error, no answer): the static
        // catalog.
        for reply in [
            Some((401, "")),
            Some((403, "")),
            Some((500, "")),
            Some((503, "")),
            None,
        ] {
            assert_eq!(
                names(from_show_functions_reply(reply, v)),
                fallback,
                "{reply:?}"
            );
        }
        // Below 7.0 `elastic_catalog` asks nothing (no `/_sql`).
        assert!(!has_sql("Elasticsearch 6.8.23"));
        assert!(!has_sql("6.3.0"));
        assert!(has_sql("Elasticsearch 7.0.0"));
        assert!(has_sql("Elasticsearch 8.15.3"));
        assert!(has_sql("unreadable"));
    }

    #[test]
    fn the_plugins_decide_the_plugin_catalogs() {
        let j = |s: &str| J::parse(s).unwrap();
        // Read without the SQL plugin: nothing.
        let others = j(r#"[{"component":"opensearch-knn","version":"2.17.1.0"}]"#);
        assert!(from_plugins(Some(&others), false, "OpenSearch 2.17.1").is_empty());
        assert!(from_plugins(Some(&j("[]")), true, "Elasticsearch 7.10.2").is_empty());
        // Read with it: the plugin's version filters.
        let os = j(r#"[{"component":"opensearch-sql","version":"2.3.0.0"}]"#);
        let c = names(from_plugins(Some(&os), false, "OpenSearch 2.17.1"));
        assert!(c.contains("ABS") && !c.contains("TYPEOF"));
        let od = j(r#"[{"component":"opendistro-sql","version":"1.12.0.0"}]"#);
        let c = names(from_plugins(Some(&od), true, "Elasticsearch 7.10.2"));
        assert!(c.contains("DAYNAME") && !c.contains("NULLIF"));
        // Unreadable: OpenSearch's version filters OpenSearch's plugin…
        let c = names(from_plugins(None, false, "OpenSearch 2.3.0"));
        assert!(c.contains("ABS") && !c.contains("TYPEOF"));
        assert!(names(from_plugins(None, false, "OpenSearch 2.17.1")).contains("TYPEOF"));
        // …and Open Distro's whole catalog goes (the server's version is
        // Elasticsearch's, not the plugin's).
        assert_eq!(
            names(from_plugins(None, true, "Elasticsearch 7.10.2")),
            names(opendistro(None))
        );
    }

    #[test]
    fn show_functions_rows_take_the_catalogs_arguments() {
        let resp = J::parse(
            r#"{"columns":[{"name":"name","type":"keyword"},{"name":"type","type":"keyword"}],
                "rows":[["AVG","AGGREGATE"],["COUNT","AGGREGATE"],["HISTOGRAM","GROUPING"],
                        ["CASE","CONDITIONAL"],["IIF","CONDITIONAL"],["SCORE","SCORE"],
                        ["ABS","SCALAR"],["NEW_ONE","SCALAR"]]}"#,
        )
        .unwrap();
        let c = from_show_functions(&resp, "Elasticsearch 8.15.3");
        let by = |n: &str| c.iter().filter(|f| f.name == n).collect::<Vec<_>>();
        assert!(c.iter().all(|f| f.language == "sql"));
        assert_eq!(by("AVG")[0].kind, FunctionKind::Aggregate);
        assert_eq!(by("AVG")[0].args, "(numeric_field)");
        assert_eq!(by("COUNT").len(), 3);
        assert_eq!(by("HISTOGRAM").len(), 2);
        assert!(by("HISTOGRAM")
            .iter()
            .all(|f| f.kind == FunctionKind::Scalar));
        assert!(by("CASE").is_empty());
        assert_eq!(by("IIF")[0].kind, FunctionKind::Scalar);
        assert_eq!(by("SCORE")[0].returns, "double");
        assert_eq!(by("NEW_ONE")[0].args, "");
        // Documented, not listed.
        for n in UNLISTED {
            assert_eq!(by(n).len(), 1, "{n}");
        }
        // Not in this list, though the catalog has it.
        assert!(by("DATE_FORMAT").is_empty());
        // A reply without the columns: the static catalog.
        let c = from_show_functions(&J::parse("{}").unwrap(), "8.15.3");
        assert!(c.iter().any(|f| f.name == "DATE_FORMAT"));
    }

    #[test]
    fn the_sql_plugin_is_found_by_its_component() {
        let rows = |s: &str| J::parse(s).unwrap();
        assert_eq!(
            sql_plugin(&rows(
                r#"[{"component":"opensearch-knn","version":"2.17.1.0"},{"component":"opensearch-sql","version":"2.17.1.0"}]"#
            )),
            Some("2.17.1.0".into())
        );
        assert_eq!(
            sql_plugin(&rows(
                r#"[{"component":"opendistro-sql","version":"1.13.2.0"}]"#
            )),
            Some("1.13.2.0".into())
        );
        assert_eq!(
            sql_plugin(&rows(
                r#"[{"component":"opendistro_sql","version":"1.4.0.0"}]"#
            )),
            Some("1.4.0.0".into())
        );
        assert_eq!(
            sql_plugin(&rows(
                r#"[{"component":"opensearch-skills","version":"2.17.1.0"}]"#
            )),
            None
        );
        assert_eq!(sql_plugin(&rows("[]")), None);
    }
}
