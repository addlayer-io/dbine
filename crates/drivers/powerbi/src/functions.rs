//! The function catalog for the editor's completion (spec §4.6). Owned by
//! part C.
//!
//! Tier A: `INFO.FUNCTIONS()` (names, `PARAMETER_LIST`, `RETURN_TYPE`,
//! `DESCRIPTION`) plus `INFO.USERDEFINEDFUNCTIONS()`. Otherwise (or when
//! `INFO.FUNCTIONS` is refused) the static catalog [`DAX`] plus the model's
//! UDFs from Tier B. Kinds: `Table` for functions returning tables,
//! `Aggregate` for the aggregators, `Scalar` otherwise.
//!
//! The server's list gives no kinds: they come from the static catalog by
//! name (an unknown `INFO.*` is a table function, anything else scalar).
//! The server's own `DESCRIPTION` is shown as it writes it; the static
//! catalog has none.

use crate::catalog::Model;
use crate::meta::rows::Rows;
use crate::meta::service::Service;
use crate::query::{ApiKind, QueryOpts, Target};
use crate::Ctx;
use dbine_driver::functions::{parse_catalog, FunctionInfo, FunctionKind};
use dbine_driver::Result;
use std::collections::{BTreeMap, BTreeSet};

/// DAX's built-ins from the DAX function reference, in
/// [`parse_catalog`]'s format.
pub const DAX: &str = include_str!("functions/dax.txt");

/// The static catalog.
pub fn static_catalog() -> Vec<FunctionInfo> {
    parse_catalog(DAX, None)
}

pub async fn function_catalog(cx: &mut Ctx) -> Result<Vec<FunctionInfo>> {
    catalog_in(cx).await
}

pub(crate) async fn catalog_in<S: Service>(s: &mut S) -> Result<Vec<FunctionInfo>> {
    let builtin = static_catalog();
    // The listing session has no model: the static catalog.
    let Some(m) = s.session_model().cloned() else {
        return Ok(builtin);
    };
    let mut out = match server_catalog(s, &m, &builtin).await {
        Ok(Some(list)) => list,
        Ok(None) => builtin,
        Err(e) if crate::meta::is_fatal(&e) => return Err(e),
        Err(_) => builtin,
    };
    // The model's own functions, from whichever tier reads them.
    if let Ok(meta) = crate::meta::model_for(s, &m).await {
        let have: BTreeSet<String> = out.iter().map(|f| f.name.to_ascii_uppercase()).collect();
        for f in &meta.functions {
            if have.contains(&f.name.to_ascii_uppercase()) {
                continue;
            }
            let mut info = FunctionInfo::new(&f.name, FunctionKind::Scalar)
                .args(udf_args(f.expression.as_deref()));
            if let Some(d) = &f.description {
                info = info.description(d);
            }
            out.push(info);
        }
    }
    Ok(out)
}

/// A UDF's parameter list: what's between the parentheses before `=>`
/// (`(amount : NUMERIC) => …` → `(amount : NUMERIC)`).
fn udf_args(expression: Option<&str>) -> String {
    let Some(e) = expression else {
        return String::new();
    };
    let Some(arrow) = e.find("=>") else {
        return String::new();
    };
    let head = e[..arrow].trim();
    if head.starts_with('(') && head.ends_with(')') {
        head.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        String::new()
    }
}

/// `INFO.FUNCTIONS()` on Arrow (it isn't on `executeQueries`); `None` when
/// the model takes JSON only.
async fn server_catalog<S: Service>(
    s: &mut S,
    m: &Model,
    builtin: &[FunctionInfo],
) -> Result<Option<Vec<FunctionInfo>>> {
    let target = Target::of(m);
    if !s.arrow_available(&target).await? {
        return Ok(None);
    }
    let opts = QueryOpts {
        api: Some(ApiKind::Arrow),
        ..QueryOpts::metadata()
    };
    let sets = s.dax(&target, "EVALUATE INFO.FUNCTIONS()", &opts).await?;
    let Some(set) = sets.first() else {
        return Ok(None);
    };
    let kinds: BTreeMap<String, (FunctionKind, String)> = builtin
        .iter()
        .map(|f| (f.name.to_ascii_uppercase(), (f.kind, f.args.clone())))
        .collect();
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for r in Rows::new(set).iter() {
        let Some(name) = r.str("FUNCTION_NAME") else {
            continue;
        };
        let upper = name.to_ascii_uppercase();
        let known = kinds.get(&upper);
        let kind = known.map(|k| k.0).unwrap_or(if upper.starts_with("INFO.") {
            FunctionKind::Table
        } else {
            FunctionKind::Scalar
        });
        let args = match r.str("PARAMETER_LIST") {
            Some(p) => paren(&p),
            None => known.map(|k| k.1.clone()).unwrap_or_default(),
        };
        if !seen.insert((upper, args.clone())) {
            continue;
        }
        let mut f = FunctionInfo::new(name, kind).args(args);
        if let Some(d) = r.str("DESCRIPTION") {
            f = f.description(d);
        }
        out.push(f);
    }
    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some(out))
}

/// `Table,Expression` → `(Table, Expression)`.
fn paren(list: &str) -> String {
    let l = list.trim();
    if l.starts_with('(') {
        return l.to_string();
    }
    let parts: Vec<&str> = l.split(',').map(str::trim).collect();
    format!("({})", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::fake::{rowset, Fake};
    use serde_json::json;

    #[test]
    fn the_static_catalog_parses_without_duplicates() {
        let all = static_catalog();
        assert!(all.len() > 400, "{}", all.len());
        let mut seen = BTreeSet::new();
        for f in &all {
            assert!(seen.insert(f.name.clone()), "duplicate {}", f.name);
            assert!(f.args.starts_with('('), "{}: {}", f.name, f.args);
            assert!(f.description.is_empty());
        }
        let kind = |n: &str| all.iter().find(|f| f.name == n).unwrap().kind;
        assert_eq!(kind("SUMMARIZECOLUMNS"), FunctionKind::Table);
        assert_eq!(kind("DISTINCT"), FunctionKind::Table);
        assert_eq!(kind("TOPNSKIP"), FunctionKind::Table);
        assert_eq!(kind("FIRSTNONBLANK"), FunctionKind::Table);
        assert_eq!(kind("LASTNONBLANKVALUE"), FunctionKind::Scalar);
        assert_eq!(kind("INFO.VIEW.TABLES"), FunctionKind::Table);
        assert_eq!(kind("DATESYTD"), FunctionKind::Table);
        assert_eq!(kind("SUMX"), FunctionKind::Aggregate);
        assert_eq!(kind("STDEVX.S"), FunctionKind::Aggregate);
        assert_eq!(kind("CALCULATE"), FunctionKind::Scalar);
        assert_eq!(kind("RANKX"), FunctionKind::Scalar);
        assert_eq!(
            all.iter().find(|f| f.name == "SUMX").unwrap().args,
            "(table, expression)"
        );
    }

    #[tokio::test]
    async fn json_only_models_get_the_static_catalog_and_their_udfs() {
        let mut f = Fake::writer();
        f.definition(crate::definition::tmsl::tests::SAMPLE);
        let list = catalog_in(&mut f).await.unwrap();
        assert!(list.iter().any(|x| x.name == "SUMMARIZECOLUMNS"));
        let udf = list.iter().find(|x| x.name == "AddTax").unwrap();
        assert_eq!(udf.args, "( amount : NUMERIC )");
        assert_eq!(udf.description, "Suma el IVA");
    }

    #[tokio::test]
    async fn arrow_models_list_the_servers_functions() {
        let mut f = Fake::new();
        f.arrow = true;
        f.dax_answer(
            "INFO.FUNCTIONS()",
            vec![rowset(
                &[
                    "[FUNCTION_NAME]",
                    "[DESCRIPTION]",
                    "[PARAMETER_LIST]",
                    "[RETURN_TYPE]",
                    "[ORIGIN]",
                    "[INTERFACE_NAME]",
                ],
                vec![
                    json!([
                        "SUMX",
                        "Returns the sum of an expression evaluated for each row in a table.",
                        "Table,Expression",
                        5,
                        4,
                        "MATHTRIG"
                    ]),
                    json!(["SUMX", "dup", "Table,Expression", 5, 4, "MATHTRIG"]),
                    json!(["INFO.NEWTHING", "New.", null, 0, 4, "INFO"]),
                    json!(["NEWSCALAR", null, "Value", 5, 4, "MATH"]),
                ],
            )],
        );
        // No metadata answers: the UDFs are skipped, the list still comes.
        let list = catalog_in(&mut f).await.unwrap();
        assert_eq!(list.len(), 3, "{list:?}");
        assert_eq!(list[0].kind, FunctionKind::Aggregate);
        assert_eq!(list[0].args, "(Table, Expression)");
        assert!(list[0].description.starts_with("Returns the sum"));
        assert_eq!(list[1].kind, FunctionKind::Table);
        assert_eq!(list[2].kind, FunctionKind::Scalar);
        // Refused (not an admin): the static catalog.
        let mut f = Fake::new();
        f.arrow = true;
        f.dax_refuse("INFO.FUNCTIONS()");
        assert!(catalog_in(&mut f).await.unwrap().len() > 400);
        // The listing session: static, no request.
        let mut f = Fake::new();
        f.model = None;
        assert!(catalog_in(&mut f).await.unwrap().len() > 400);
        assert_eq!(f.dax_calls(), 0);
    }
}
