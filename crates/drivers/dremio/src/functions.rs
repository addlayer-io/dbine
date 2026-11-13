//! The function catalog for the editor's completion (#97). Dremio has no
//! `sys.functions`; its server lists the functions its own SQL editor
//! completes at `/apiv2/sql/functions`, with each signature's parameters,
//! return type, categories and description. If that read fails (older
//! servers), the static catalog generated from it on Dremio 26: only the
//! few functions whose entry names a `dremioVersion` are left out of older
//! servers, the rest are offered whatever the version. The user's functions
//! come from `sys.user_defined_functions`, with their path as the schema
//! ([`udf`], which the Flight SQL driver shares).

use crate::{text, DremioSession};
use dbine_driver::functions::{parse_catalog, FunctionInfo, FunctionKind};
use dbine_driver::{Result, Session};
use serde_json::Value;

mod udf;
use udf::{from_udf, UDF_SQL};

/// Dremio's built-ins, generated from `/apiv2/sql/functions` on Dremio
/// 26.0, with the version a function names (`dremioVersion`) as `since=`
/// where it names one.
const DREMIO: &str = include_str!("functions/dremio.txt");

impl DremioSession {
    pub(crate) async fn function_list(&mut self) -> Result<Vec<FunctionInfo>> {
        let mut list = match self
            .conn
            .send(reqwest::Method::GET, "/apiv2/sql/functions", None)
            .await
        {
            Ok(v) => from_server(&v),
            Err(e) => {
                tracing::debug!("dremio: /apiv2/sql/functions failed: {e}");
                Vec::new()
            }
        };
        if list.is_empty() {
            let version = self.server_version().await.ok();
            list = parse_catalog(DREMIO, version.as_deref());
        }
        match self.records(UDF_SQL).await {
            Ok(rows) => list.extend(rows.iter().filter_map(from_udf)),
            Err(e) => tracing::debug!("dremio: sys.user_defined_functions unreadable: {e}"),
        }
        Ok(list)
    }
}

/// `/apiv2/sql/functions`' answer: one entry per signature.
pub(crate) fn from_server(v: &Value) -> Vec<FunctionInfo> {
    let Some(functions) = v.get("functions").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for f in functions {
        let name = f.get("name").map(text).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let categories: Vec<String> = f
            .get("functionCategories")
            .and_then(Value::as_array)
            .map(|c| c.iter().map(text).collect())
            .unwrap_or_default();
        let kind = if categories.iter().any(|c| c == "AGGREGATE") {
            FunctionKind::Aggregate
        } else if categories.iter().any(|c| c == "WINDOW") {
            FunctionKind::Window
        } else {
            FunctionKind::Scalar
        };
        let description = f
            .get("description")
            .map(text)
            .unwrap_or_default()
            .trim()
            .to_string();
        let signatures = f
            .get("signatures")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if signatures.is_empty() {
            out.push(FunctionInfo::new(name, kind).description(description));
            continue;
        }
        for s in &signatures {
            let own = s.get("description").map(text).unwrap_or_default();
            // Dremio leaves a placeholder in some signatures.
            let description = if description.is_empty() && own != "sample signature description" {
                own.trim().to_string()
            } else {
                description.clone()
            };
            out.push(
                FunctionInfo::new(name.clone(), kind)
                    .args(signature_args(s))
                    .returns(s.get("returnType").map(text).unwrap_or_default())
                    .description(description),
            );
        }
    }
    out
}

/// A signature's parameters: `(numeric_expression NUMERIC)`, optional ones
/// in brackets, a repeated one with `...`.
pub(crate) fn signature_args(s: &Value) -> String {
    let mut out = String::from("(");
    let mut first = true;
    for p in s
        .get("parameters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = p.get("name").map(text).unwrap_or_default();
        let ty = p.get("type").map(text).unwrap_or_default();
        let one = [name, ty]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let sep = if first { "" } else { ", " };
        match p.get("kind").map(text).as_deref() {
            Some("OPTIONAL") => out.push_str(&format!("[{sep}{one}]")),
            Some("VARARG") => out.push_str(&format!("{sep}{one}...")),
            _ => out.push_str(&format!("{sep}{one}")),
        }
        first = false;
    }
    out.push(')');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_static_catalog_parses() {
        let mut n = 0;
        for line in DREMIO.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            n += 1;
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
        assert!(n > 300, "{n}");
        let has = |v, name: &str| parse_catalog(DREMIO, v).iter().any(|f| f.name == name);
        assert!(has(None, "APPROX_COUNT_DISTINCT"));
        assert!(has(Some("Dremio 24.0.0"), "LOWER"));
        let gated: Vec<FunctionInfo> = parse_catalog(DREMIO, Some("Dremio 24.0"));
        assert!(gated.len() < parse_catalog(DREMIO, None).len());
    }

    #[test]
    fn server_answers_map_into_entries() {
        let v = json!({"functions": [
            {
                "name": "ABS",
                "signatures": [{
                    "returnType": "NUMERIC",
                    "parameters": [{"kind": "REGULAR", "type": "NUMERIC", "name": "numeric_expression"}],
                    "description": "sample signature description"
                }],
                "functionCategories": ["MATH"],
                "description": "Computes the absolute value of a numeric expression.\n"
            },
            {
                "name": "CONCAT",
                "signatures": [{
                    "returnType": "CHARACTERS",
                    "parameters": [
                        {"kind": "REGULAR", "type": "CHARACTERS", "name": "expression"},
                        {"kind": "VARARG", "type": "CHARACTERS", "name": "expression"}
                    ]
                }]
            },
            {
                "name": "SUM",
                "signatures": [{
                    "returnType": "NUMERIC",
                    "parameters": [
                        {"kind": "REGULAR", "type": "NUMERIC", "name": "numeric_expression"},
                        {"kind": "OPTIONAL", "type": "CHARACTERS", "name": "partition_expression"}
                    ]
                }],
                "functionCategories": ["AGGREGATE", "WINDOW"]
            },
            {
                "name": "ROW_NUMBER",
                "signatures": [{"returnType": "INT", "parameters": []}],
                "functionCategories": ["WINDOW"]
            }
        ]});
        let list = from_server(&v);
        assert_eq!(list.len(), 4);
        assert_eq!(list[0].args, "(numeric_expression NUMERIC)");
        assert_eq!(list[0].returns, "NUMERIC");
        assert_eq!(
            list[0].description,
            "Computes the absolute value of a numeric expression."
        );
        assert_eq!(
            list[1].args,
            "(expression CHARACTERS, expression CHARACTERS...)"
        );
        assert_eq!(list[1].description, "");
        assert_eq!(list[2].kind, FunctionKind::Aggregate);
        assert_eq!(
            list[2].args,
            "(numeric_expression NUMERIC[, partition_expression CHARACTERS])"
        );
        assert_eq!(list[3].kind, FunctionKind::Window);
        assert_eq!(list[3].args, "()");
        assert!(from_server(&json!({"errorMessage": "x"})).is_empty());
    }
}
