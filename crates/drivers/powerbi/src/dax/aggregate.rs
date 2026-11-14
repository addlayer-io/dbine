//! Charts aggregated on the server (spec §4.5 `aggregate_query`, G12).
//! Owned by part B.
//!
//! One-`EVALUATE` units only: `DEFINE <user defines> VAR __dbine_q = <table
//! expr> EVALUATE TOPN(max_groups + 1, GROUPBY(__dbine_q, <x>, <split>,
//! "dbine_n", SUMX(CURRENTGROUP(), 1), "dbine_m0", SUMX(CURRENTGROUP(), <c>)
//! …), <x>, ASC, <split>, ASC) ORDER BY <x> ASC, <split> ASC`: the columns
//! `dbine::aggregate::from_grouped` reads by position (x, split, rows,
//! measures). The user's `ORDER BY` is dropped (a VAR can't hold it);
//! `START AT` → `Unsupported` (dropping it would change the rows).
//! Count(col) → `COUNTX`, Count(*) → `SUMX(CURRENTGROUP(), 1)`, Avg/Min/Max
//! → `AVERAGEX`/`MINX`/`MAXX`; no `x` → `ROW(…)` over the whole result.
//! Result columns are named as the service names them (`Date[Year]` →
//! `'Date'[Year]`, `[Ventas]`), or plainly on the driver's own table
//! browses (`Year` → `'Date'[Year]`). `Unsupported` lets the app aggregate
//! the rows itself.

use super::browse::parse_browse;
use super::lexer::{tokenize, TokenKind};
use super::quote::result_column_ref;
use super::split::{parts, split};
use dbine_driver::aggregate::{validate, AggFn, AggregateSpec};
use dbine_driver::sql::StatementKind;
use dbine_driver::{Error, Result};

fn unsupported(why: &str) -> Error {
    Error::Unsupported(why.into())
}

/// Where the `EVALUATE`'s table expression ends (the top-level `ORDER BY`
/// or `START AT`, else the end), and whether it has a `START AT`.
fn expression_end(text: &str, from: usize) -> (usize, bool) {
    let toks: Vec<_> = tokenize(&text[from..])
        .into_iter()
        .filter(|t| !t.is_trivia())
        .collect();
    let mut depth = 0i32;
    let mut end = None;
    let mut start_at = false;
    for (i, t) in toks.iter().enumerate() {
        if t.kind == TokenKind::Punct {
            match t.text {
                "(" | "{" => depth += 1,
                ")" | "}" => depth -= 1,
                _ => {}
            }
            continue;
        }
        if depth != 0 {
            continue;
        }
        let next = toks.get(i + 1);
        if t.is_word("order") && next.is_some_and(|n| n.is_word("by")) {
            end.get_or_insert(from + t.start);
        }
        if t.is_word("start") && next.is_some_and(|n| n.is_word("at")) {
            end.get_or_insert(from + t.start);
            start_at = true;
        }
    }
    (end.unwrap_or(text.len()), start_at)
}

pub fn aggregate_query(query: &str, spec: &AggregateSpec) -> Result<String> {
    let spec = validate(spec)?;
    let units = split(query);
    let [unit] = units.as_slice() else {
        return Err(unsupported(
            "el gráfico agrupa en el servidor una sola consulta",
        ));
    };
    if unit.error.is_some()
        || unit.kind != StatementKind::Sql
        || !dbine_driver::read_only::dax_is_query(&unit.text)
    {
        return Err(unsupported(
            "el gráfico agrupa en el servidor una consulta con un solo EVALUATE",
        ));
    }
    let text = unit.text.as_str();
    let p = parts(text);
    let [ev] = p.evaluates.as_slice() else {
        return Err(unsupported(
            "el gráfico agrupa en el servidor una consulta con un solo EVALUATE",
        ));
    };
    let body_from = ev + "EVALUATE".len();
    let (end, start_at) = expression_end(text, body_from);
    if start_at {
        return Err(unsupported(
            "la consulta usa START AT: el gráfico agrupa las filas en DBine",
        ));
    }
    let expr = text[body_from..end].trim();
    if expr.is_empty() {
        return Err(unsupported("la consulta no tiene una expresión de tabla"));
    }
    let defines = p
        .define
        .map(|d| text[d + "DEFINE".len()..*ev].trim())
        .filter(|d| !d.is_empty());
    let table = parse_browse(text).map(|b| b.table);
    let col = |c: &str| result_column_ref(c, table.as_deref());
    let mut head = String::from("DEFINE\n");
    if let Some(d) = defines {
        head.push_str(d);
        head.push('\n');
    }
    head.push_str(&format!("VAR __dbine_q = {expr}\nEVALUATE\n"));
    let Some(x) = &spec.x else {
        let mut cols = vec!["\"dbine_n\", COUNTROWS(__dbine_q)".to_string()];
        for (i, m) in spec.measures.iter().enumerate() {
            let e = match (&m.column, m.func) {
                (None, _) => "COUNTROWS(__dbine_q)".to_string(),
                (Some(c), f) => format!("{}(__dbine_q, {})", x_function(f), col(c)),
            };
            cols.push(format!("\"dbine_m{i}\", {e}"));
        }
        return Ok(format!("{head}ROW({})", cols.join(", ")));
    };
    let mut keys = vec![col(x)];
    if let Some(s) = &spec.split {
        keys.push(col(s));
    }
    let mut cols = keys.clone();
    cols.push("\"dbine_n\", SUMX(CURRENTGROUP(), 1)".to_string());
    for (i, m) in spec.measures.iter().enumerate() {
        let e = match (&m.column, m.func) {
            (None, _) => "SUMX(CURRENTGROUP(), 1)".to_string(),
            (Some(c), f) => format!("{}(CURRENTGROUP(), {})", x_function(f), col(c)),
        };
        cols.push(format!("\"dbine_m{i}\", {e}"));
    }
    let n = u64::from(spec.max_groups) + 1;
    let topn_order: Vec<String> = keys.iter().map(|k| format!("{k}, ASC")).collect();
    let order_by: Vec<String> = keys.iter().map(|k| format!("{k} ASC")).collect();
    Ok(format!(
        "{head}TOPN({n}, GROUPBY(__dbine_q, {}), {})\nORDER BY {}",
        cols.join(", "),
        topn_order.join(", "),
        order_by.join(", ")
    ))
}

/// The iterator `GROUPBY` takes over `CURRENTGROUP()` for each aggregate.
fn x_function(f: AggFn) -> &'static str {
    match f {
        AggFn::Count => "COUNTX",
        AggFn::Sum => "SUMX",
        AggFn::Avg => "AVERAGEX",
        AggFn::Min => "MINX",
        AggFn::Max => "MAXX",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::aggregate::Measure;

    fn spec(
        x: Option<&str>,
        split: Option<&str>,
        measures: Vec<(Option<&str>, AggFn)>,
    ) -> AggregateSpec {
        AggregateSpec {
            x: x.map(str::to_string),
            split: split.map(str::to_string),
            measures: measures
                .into_iter()
                .map(|(c, func)| Measure {
                    column: c.map(str::to_string),
                    func,
                })
                .collect(),
            max_groups: 100,
        }
    }

    #[test]
    fn groups_with_every_aggregate() {
        let q = "// ventas\nDEFINE MEASURE 'Sales'[Total] = SUM('Sales'[Amount])\nEVALUATE SUMMARIZECOLUMNS('Date'[Year], 'Geo'[Country], \"Ventas\", [Total])\nORDER BY 'Date'[Year]";
        let s = spec(
            Some("Date[Year]"),
            Some("Geo[Country]"),
            vec![
                (None, AggFn::Count),
                (Some("[Ventas]"), AggFn::Count),
                (Some("[Ventas]"), AggFn::Sum),
                (Some("[Ventas]"), AggFn::Avg),
                (Some("[Ventas]"), AggFn::Min),
                (Some("[Ventas]"), AggFn::Max),
            ],
        );
        assert_eq!(
            aggregate_query(q, &s).unwrap(),
            "DEFINE\nMEASURE 'Sales'[Total] = SUM('Sales'[Amount])\nVAR __dbine_q = SUMMARIZECOLUMNS('Date'[Year], 'Geo'[Country], \"Ventas\", [Total])\nEVALUATE\nTOPN(101, GROUPBY(__dbine_q, 'Date'[Year], 'Geo'[Country], \"dbine_n\", SUMX(CURRENTGROUP(), 1), \"dbine_m0\", SUMX(CURRENTGROUP(), 1), \"dbine_m1\", COUNTX(CURRENTGROUP(), [Ventas]), \"dbine_m2\", SUMX(CURRENTGROUP(), [Ventas]), \"dbine_m3\", AVERAGEX(CURRENTGROUP(), [Ventas]), \"dbine_m4\", MINX(CURRENTGROUP(), [Ventas]), \"dbine_m5\", MAXX(CURRENTGROUP(), [Ventas])), 'Date'[Year], ASC, 'Geo'[Country], ASC)\nORDER BY 'Date'[Year] ASC, 'Geo'[Country] ASC"
        );
    }

    #[test]
    fn without_x_one_row_over_everything() {
        let q = "EVALUATE TOPN(200, 'Sales')";
        let s = spec(
            None,
            None,
            vec![(None, AggFn::Count), (Some("Amount"), AggFn::Sum)],
        );
        assert_eq!(
            aggregate_query(q, &s).unwrap(),
            "DEFINE\nVAR __dbine_q = TOPN(200, 'Sales')\nEVALUATE\nROW(\"dbine_n\", COUNTROWS(__dbine_q), \"dbine_m0\", COUNTROWS(__dbine_q), \"dbine_m1\", SUMX(__dbine_q, 'Sales'[Amount]))"
        );
        // A plain name of the driver's browse belongs to its table.
        let s = spec(Some("Year"), None, vec![(Some("Amount"), AggFn::Avg)]);
        let out = aggregate_query(q, &s).unwrap();
        assert!(
            out.contains("GROUPBY(__dbine_q, 'Sales'[Year], \"dbine_n\""),
            "{out}"
        );
        assert!(out.ends_with("ORDER BY 'Sales'[Year] ASC"), "{out}");
    }

    #[test]
    fn what_it_leaves_to_the_app() {
        let s = spec(Some("[a]"), None, vec![(None, AggFn::Count)]);
        for q in [
            "EVALUATE 'T' ORDER BY 'T'[a] START AT 5",
            "EVALUATE {1}\nEVALUATE {2}",
            "DEFINE VAR x = 1 EVALUATE {x} EVALUATE {x}",
            "DEFINE VAR x = 1",
            "GRANT Read TO \"a\"",
            "SELECT 1",
            "EVALUATE",
        ] {
            assert!(
                matches!(aggregate_query(q, &s), Err(Error::Unsupported(_))),
                "{q}"
            );
        }
        // ORDER BY inside the expression stays; the one of the EVALUATE goes.
        let out = aggregate_query(
            "EVALUATE TOPN(5, 'T', 'T'[a], DESC) ORDER BY 'T'[a] DESC",
            &s,
        )
        .unwrap();
        assert!(
            out.contains("VAR __dbine_q = TOPN(5, 'T', 'T'[a], DESC)\n"),
            "{out}"
        );
        // A spec the chart can't draw is the app's error, as on SQL engines.
        assert!(matches!(
            aggregate_query(
                "EVALUATE 'T'",
                &spec(None, Some("[b]"), vec![(None, AggFn::Count)])
            ),
            Err(Error::Query(_))
        ));
    }
}
