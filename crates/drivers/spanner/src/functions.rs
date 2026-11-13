//! The function catalog for the editor's completion (#97). Spanner doesn't
//! list its built-ins, so they come from a static catalog of Google's
//! GoogleSQL documentation for Spanner; the database's own functions (the
//! change stream read functions, `READ_<stream>`) come from
//! `INFORMATION_SCHEMA.ROUTINES` with their `PARAMETERS`, in one query.
//! The emulator has no `ROUTINES`: there they come from
//! `INFORMATION_SCHEMA.CHANGE_STREAMS`, with the documented signature.

use crate::SpannerSession;
use dbine_driver::functions::{parse_catalog, FunctionInfo, FunctionKind};
use dbine_driver::Result;

/// Spanner's GoogleSQL built-ins, from Google's documentation (the "All
/// functions" list and each function's syntax).
const SPANNER: &str = include_str!("functions/spanner.txt");

const ROUTINES_SQL: &str = "SELECT r.ROUTINE_NAME, r.DATA_TYPE, \
     ARRAY_TO_STRING(ARRAY(SELECT CONCAT(p.PARAMETER_NAME, ' ', p.DATA_TYPE) \
       FROM INFORMATION_SCHEMA.PARAMETERS AS p WHERE p.SPECIFIC_NAME = r.SPECIFIC_NAME \
       ORDER BY p.ORDINAL_POSITION), ', ') \
     FROM INFORMATION_SCHEMA.ROUTINES AS r";

const CHANGE_STREAMS_SQL: &str = "SELECT CHANGE_STREAM_NAME FROM INFORMATION_SCHEMA.CHANGE_STREAMS";

/// The arguments of a change stream's read function, as Spanner documents
/// them for GoogleSQL.
const READ_ARGS: &str = "(start_timestamp TIMESTAMP, end_timestamp TIMESTAMP, \
     partition_token STRING, heartbeat_milliseconds INT64[, read_options ARRAY<STRING>])";

impl SpannerSession {
    pub(crate) async fn function_list(&mut self) -> Result<Vec<FunctionInfo>> {
        let mut list = built_ins();
        match self.text_rows(ROUTINES_SQL, &[]).await {
            Ok(rows) => list.extend(rows.iter().filter_map(|r| from_routine(r))),
            Err(e) => {
                tracing::debug!("spanner: INFORMATION_SCHEMA.ROUTINES unreadable: {e}");
                match self.text_rows(CHANGE_STREAMS_SQL, &[]).await {
                    Ok(rows) => list.extend(rows.iter().filter_map(|r| read_function(r))),
                    Err(e) => tracing::debug!("spanner: change streams unreadable: {e}"),
                }
            }
        }
        Ok(list)
    }
}

/// `READ_<stream>` for a row of `INFORMATION_SCHEMA.CHANGE_STREAMS`.
pub(crate) fn read_function(r: &[Option<String>]) -> Option<FunctionInfo> {
    let stream = r.first().cloned().flatten().filter(|s| !s.is_empty())?;
    Some(FunctionInfo::new(format!("READ_{stream}"), FunctionKind::Table).args(READ_ARGS))
}

/// The static catalog, with namespaced names (`NET.HOST`, `ML.PREDICT`)
/// split into their namespace and name.
pub(crate) fn built_ins() -> Vec<FunctionInfo> {
    parse_catalog(SPANNER, None)
        .into_iter()
        .map(split_namespace)
        .collect()
}

fn split_namespace(f: FunctionInfo) -> FunctionInfo {
    match f.name.rsplit_once('.') {
        Some((ns, name)) => {
            let (ns, name) = (ns.to_string(), name.to_string());
            FunctionInfo { name, ..f }.schema(ns)
        }
        None => f,
    }
}

/// A row of the routines query: a change stream read function, used where
/// a table goes.
pub(crate) fn from_routine(r: &[Option<String>]) -> Option<FunctionInfo> {
    let cell = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
    let name = cell(0);
    if name.is_empty() {
        return None;
    }
    Some(
        FunctionInfo::new(name, FunctionKind::Table)
            .args(format!("({})", cell(2)))
            .returns(cell(1)),
    )
}

/// The guard against documentation examples, shared with BigQuery's
/// catalog, which comes from the same GoogleSQL pages.
#[cfg(test)]
#[path = "../../bigquery/src/functions/examples.rs"]
mod examples;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_static_catalog_parses() {
        let mut n = 0;
        for line in SPANNER.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            n += 1;
            let parsed = parse_catalog(line, None);
            assert_eq!(parsed.len(), 1, "unreadable: {line}");
            assert!(
                !examples::is_example(&parsed[0].args),
                "an example, not a syntax form: {line}"
            );
        }
        assert!(n > 200, "{n}");
        let list = built_ins();
        let find = |name: &str| list.iter().find(|f| f.name == name);
        // Bare names only: `AI.IF` isn't `IF`.
        let kinds = |name: &str| {
            list.iter()
                .filter(|f| f.name == name && f.schema.is_empty())
                .map(|f| f.kind)
                .collect::<Vec<_>>()
        };
        assert_eq!(find("COUNT").unwrap().kind, FunctionKind::Aggregate);
        assert_eq!(kinds("COUNT").len(), 2);
        assert_eq!(kinds("SEARCH").len(), 1);
        assert_eq!(kinds("FORMAT").len(), 1);
        assert_eq!(find("ABS").unwrap().args, "(X)");
        assert!(find("PENDING_COMMIT_TIMESTAMP").is_some());
        let host = find("HOST").unwrap();
        assert_eq!(host.schema, "NET");
        assert!(list.iter().all(|f| !f.name.contains('.')));
        // BigQuery's, not Spanner's.
        assert!(find("APPENDS").is_none());
        for name in [
            "ADDDATE",
            "SUBDATE",
            "LCASE",
            "UCASE",
            "FLOAT32",
            "FLOAT32_ARRAY",
            "FLOAT64",
            "FLOAT64_ARRAY",
            "LAX_FLOAT64",
            "COALESCE",
            "IF",
            "IFNULL",
            "NULLIF",
            "ELEMENT_ID",
            "LABELS",
            "PATH_LENGTH",
        ] {
            assert_eq!(kinds(name), [FunctionKind::Scalar], "{name}");
        }
        assert_eq!(kinds("IS_FIRST"), [FunctionKind::Window]);
    }

    #[test]
    fn routine_rows_map_into_entries() {
        let f = from_routine(&[
            Some("READ_SingersStream".into()),
            Some("ARRAY<STRUCT<…>>".into()),
            Some("start_timestamp TIMESTAMP, end_timestamp TIMESTAMP, partition_token STRING, heartbeat_milliseconds INT64, read_options ARRAY<STRING>".into()),
        ])
        .unwrap();
        assert_eq!(f.kind, FunctionKind::Table);
        assert!(f.args.starts_with("(start_timestamp TIMESTAMP"));
        assert!(f.schema.is_empty());
        assert!(from_routine(&[None, None, None]).is_none());

        let f = read_function(&[Some("SingersStream".into())]).unwrap();
        assert_eq!(f.name, "READ_SingersStream");
        assert_eq!(f.kind, FunctionKind::Table);
        assert!(f
            .args
            .starts_with("(start_timestamp TIMESTAMP, end_timestamp"));
        assert!(read_function(&[None]).is_none());
    }
}
