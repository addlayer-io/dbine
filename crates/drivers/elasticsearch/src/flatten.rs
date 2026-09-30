//! Turning JSON responses into tables (shared with the Solr driver).

use crate::json::{Obj, J};
use dbine_driver::{QueryOutcome, ResultColumn};

fn col(name: &str) -> ResultColumn {
    ResultColumn { name: name.to_string(), type_name: String::new() }
}

/// One row per document; columns are the union of the documents' top-level
/// keys in order of appearance, nested values as compact JSON. Only the
/// first `max_rows` documents are kept, the rest are counted.
pub fn push_docs(out: &mut QueryOutcome, docs: impl IntoIterator<Item = Obj>, max_rows: usize) {
    let mut kept: Vec<Obj> = Vec::new();
    let mut dropped = 0u64;
    for d in docs {
        if kept.len() < max_rows {
            kept.push(d);
        } else {
            dropped += 1;
        }
    }
    let mut names: Vec<&str> = Vec::new();
    for d in &kept {
        for (k, _) in d {
            if !names.contains(&k.as_str()) {
                names.push(k);
            }
        }
    }
    let rows: Vec<Vec<serde_json::Value>> = kept
        .iter()
        .map(|d| {
            names
                .iter()
                .map(|n| d.iter().find(|(k, _)| k == n).map_or(serde_json::Value::Null, |(_, v)| v.cell()))
                .collect()
        })
        .collect();
    out.begin_result(names.iter().map(|n| col(n)).collect());
    for r in rows {
        out.push_row(r, max_rows);
    }
    for _ in 0..dropped {
        out.push_row(Vec::new(), max_rows);
    }
}

/// A search hit (or an `_mget` doc) as a row: `_index`, `_id`, `_score`,
/// then the `_source` fields (and `fields` ones not already there).
pub fn hit_doc(hit: &J) -> Obj {
    let mut d: Obj = Vec::new();
    for k in ["_index", "_id", "_score"] {
        d.push((k.to_string(), hit.get(k).cloned().unwrap_or(J::Null)));
    }
    for part in ["_source", "fields"] {
        if let Some(o) = hit.get(part).and_then(J::as_obj) {
            for (k, v) in o {
                if !d.iter().any(|(e, _)| e == k) {
                    d.push((k.clone(), v.clone()));
                }
            }
        }
    }
    d
}

/// `hits.total` as a number and relation ("eq" / "gte").
fn total_hits(resp: &J) -> Option<String> {
    let t = resp.at(&["hits", "total"])?;
    if let Some(n) = t.as_u64() {
        return Some(n.to_string());
    }
    let n = t.get("value")?.as_u64()?;
    Some(if t.get("relation").and_then(J::as_str) == Some("gte") { format!("≥ {n}") } else { n.to_string() })
}

/// A search response: the hits, then one result set per bucket
/// aggregation and one for the metric aggregations.
pub fn push_search(out: &mut QueryOutcome, resp: &J, max_rows: usize) {
    let hits = resp.at(&["hits", "hits"]).and_then(J::as_arr).unwrap_or(&[]);
    let has_aggs = resp.get("aggregations").is_some();
    // A `size: 0` aggregation query: skip the empty hits table.
    if !(hits.is_empty() && has_aggs) {
        push_docs(out, hits.iter().map(hit_doc), max_rows);
    }
    if let Some(total) = total_hits(resp) {
        let took = resp.get("took").map(J::text).unwrap_or_default();
        out.messages.push(format!("{total} documentos coinciden ({took} ms)."));
    }
    if let Some(aggs) = resp.get("aggregations") {
        push_aggs(out, aggs, max_rows);
    }
}

/// The value of a metric aggregation: `value` (or `value_as_string`) when
/// it has one, the whole object otherwise (stats, percentiles…).
fn metric_value(agg: &J) -> J {
    if let Some(s) = agg.get("value_as_string") {
        return s.clone();
    }
    if let Some(v) = agg.get("value") {
        return v.clone();
    }
    match agg {
        J::Obj(o) => J::Obj(o.iter().filter(|(k, _)| k != "meta").cloned().collect()),
        other => other.clone(),
    }
}

fn buckets(agg: &J) -> Option<Vec<(Option<String>, &J)>> {
    match agg.get("buckets")? {
        J::Arr(a) => Some(a.iter().map(|b| (None, b)).collect()),
        // Keyed buckets (filters, keyed ranges…).
        J::Obj(o) => Some(o.iter().map(|(k, b)| (Some(k.clone()), b)).collect()),
        _ => None,
    }
}

/// Aggregations, best effort: each bucket aggregation becomes a table
/// (`key`, `doc_count`, then its sub-aggregations' values); metric
/// aggregations go together in an `aggregation`/`value` table. Aggregation
/// wrappers without buckets (filter, nested, global…) are walked into.
pub fn push_aggs(out: &mut QueryOutcome, aggs: &J, max_rows: usize) {
    let mut metrics: Obj = Vec::new();
    walk_aggs(out, aggs, "", &mut metrics, max_rows);
    if !metrics.is_empty() {
        out.begin_result(vec![col("aggregation"), col("value")]);
        for (k, v) in metrics {
            out.push_row(vec![serde_json::Value::String(k), v.cell()], max_rows);
        }
    }
}

fn walk_aggs(out: &mut QueryOutcome, aggs: &J, prefix: &str, metrics: &mut Obj, max_rows: usize) {
    let Some(o) = aggs.as_obj() else { return };
    for (name, agg) in o {
        let full = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        if let Some(bs) = buckets(agg) {
            let docs = bs.into_iter().map(|(key, b)| bucket_doc(key, b));
            out.messages.push(format!("Agregación {full}: {} buckets.", agg.get("buckets").map_or(0, count_of)));
            push_docs(out, docs, max_rows);
        } else if agg.get("doc_count").is_some() && agg.as_obj().is_some_and(|o| o.iter().any(|(_, v)| v.as_obj().is_some())) {
            // Single-bucket wrapper: its count is a metric, its children aggs.
            metrics.push((format!("{full}.doc_count"), agg.get("doc_count").cloned().unwrap_or(J::Null)));
            let children: Obj = agg.as_obj().into_iter().flatten().filter(|(_, v)| v.as_obj().is_some() && *v != J::Null).cloned().collect();
            walk_aggs(out, &J::Obj(children), &full, metrics, max_rows);
        } else {
            metrics.push((full, metric_value(agg)));
        }
    }
}

fn count_of(b: &J) -> usize {
    match b {
        J::Arr(a) => a.len(),
        J::Obj(o) => o.len(),
        _ => 0,
    }
}

fn bucket_doc(key: Option<String>, b: &J) -> Obj {
    let mut d: Obj = Vec::new();
    let k = key.map(J::Str).or_else(|| b.get("key_as_string").cloned()).or_else(|| b.get("key").cloned()).unwrap_or(J::Null);
    d.push(("key".into(), k));
    d.push(("doc_count".into(), b.get("doc_count").cloned().unwrap_or(J::Null)));
    for (name, v) in b.as_obj().into_iter().flatten() {
        if matches!(name.as_str(), "key" | "key_as_string" | "doc_count") {
            continue;
        }
        let cell = match v {
            J::Obj(_) if v.get("buckets").is_none() => metric_value(v),
            other => other.clone(),
        };
        d.push((name.clone(), cell));
    }
    d
}

/// Any other response: an array of objects becomes rows; an object whose
/// values are all scalars becomes `key`/`value` rows; anything else is one
/// `response` cell with the pretty JSON.
pub fn push_generic(out: &mut QueryOutcome, resp: &J, max_rows: usize) {
    match resp {
        J::Arr(a) if !a.is_empty() && a.iter().all(|v| v.as_obj().is_some()) => {
            push_docs(out, a.iter().filter_map(|v| v.as_obj().cloned()), max_rows);
        }
        J::Obj(o) if !o.is_empty() && o.iter().all(|(_, v)| v.is_scalar()) => {
            out.begin_result(vec![col("key"), col("value")]);
            for (k, v) in o {
                out.push_row(vec![serde_json::Value::String(k.clone()), v.cell()], max_rows);
            }
        }
        other => push_text(out, &other.pretty(), max_rows),
    }
}

/// A single `response` cell.
pub fn push_text(out: &mut QueryOutcome, text: &str, max_rows: usize) {
    out.begin_result(vec![col("response")]);
    out.push_row(vec![serde_json::Value::String(text.to_string())], max_rows);
}

/// Fields of an index mapping (`{"properties": {…}}`) as `a.b.c` → type.
/// Objects without a type are `object`; multi-fields show as `a.keyword`.
pub fn mapping_fields(mapping: &J) -> Vec<(String, String)> {
    let mut out = Vec::new();
    walk_props(mapping.get("properties"), "", &mut out);
    out
}

fn walk_props(props: Option<&J>, prefix: &str, out: &mut Vec<(String, String)>) {
    let Some(o) = props.and_then(J::as_obj) else { return };
    for (name, f) in o {
        let full = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        let ty = f.get("type").and_then(J::as_str).unwrap_or("object").to_string();
        if !out.iter().any(|(n, _)| *n == full) {
            out.push((full.clone(), ty));
        }
        walk_props(f.get("properties"), &full, out);
        walk_props(f.get("fields"), &full, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn j(s: &str) -> J {
        J::parse(s).unwrap()
    }

    #[test]
    fn hits_flatten_in_order() {
        let resp = j(r#"{"took":3,"hits":{"total":{"value":3,"relation":"eq"},"hits":[
            {"_index":"i","_id":"1","_score":1.0,"_source":{"title":"a","meta":{"x":1}}},
            {"_index":"i","_id":"2","_score":0.5,"_source":{"year":2020,"title":"b"}},
            {"_index":"i","_id":"3","_score":null,"_source":{"tags":["p","q"]}}]}}"#);
        let mut out = QueryOutcome::default();
        push_search(&mut out, &resp, 2);
        let r = &out.results[0];
        let names: Vec<_> = r.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["_index", "_id", "_score", "title", "meta", "year"]);
        assert_eq!(r.rows[0], vec![json!("i"), json!("1"), json!(1.0), json!("a"), json!("{\"x\":1}"), json!(null)]);
        assert_eq!(r.rows[1][5], json!(2020));
        assert_eq!(r.total_rows, 3);
        assert!(r.truncated);
        assert!(out.messages[0].starts_with("3 documentos"));
    }

    #[test]
    fn aggregations_become_tables() {
        let resp = j(r#"{"hits":{"total":{"value":10,"relation":"eq"},"hits":[]},"aggregations":{
            "by_genre":{"doc_count_error_upper_bound":0,"buckets":[
                {"key":"rock","doc_count":6,"avg_year":{"value":1990.5}},
                {"key":"jazz","doc_count":4,"avg_year":{"value":1970.0}}]},
            "max_year":{"value":2020.0,"value_as_string":"2020"},
            "stats":{"count":2,"min":1.0}}}"#);
        let mut out = QueryOutcome::default();
        push_search(&mut out, &resp, 100);
        assert_eq!(out.results.len(), 2, "no empty hits table, one bucket table, one metric table");
        let b = &out.results[0];
        let names: Vec<_> = b.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["key", "doc_count", "avg_year"]);
        assert_eq!(b.rows[0], vec![json!("rock"), json!(6), json!(1990.5)]);
        let m = &out.results[1];
        assert_eq!(m.rows[0], vec![json!("max_year"), json!("2020")]);
        assert_eq!(m.rows[1], vec![json!("stats"), json!("{\"count\":2,\"min\":1.0}")]);
    }

    #[test]
    fn filter_wrapper_is_walked() {
        let aggs = j(r#"{"recent":{"doc_count":5,"top":{"buckets":[{"key":"a","doc_count":5}]}}}"#);
        let mut out = QueryOutcome::default();
        push_aggs(&mut out, &aggs, 10);
        assert_eq!(out.results[0].rows[0], vec![json!("a"), json!(5)]);
        assert_eq!(out.results[1].rows[0], vec![json!("recent.doc_count"), json!(5)]);
    }

    #[test]
    fn mapping_flattens() {
        let m = j(r#"{"properties":{
            "title":{"type":"text","fields":{"keyword":{"type":"keyword"}}},
            "author":{"properties":{"name":{"type":"text"},"age":{"type":"integer"}}},
            "tags":{"type":"nested","properties":{"v":{"type":"keyword"}}}}}"#);
        let f = mapping_fields(&m);
        let got: Vec<String> = f.iter().map(|(n, t)| format!("{n}:{t}")).collect();
        assert_eq!(
            got,
            ["title:text", "title.keyword:keyword", "author:object", "author.name:text", "author.age:integer", "tags:nested", "tags.v:keyword"]
        );
    }

    #[test]
    fn generic_shapes() {
        let mut out = QueryOutcome::default();
        push_generic(&mut out, &j(r#"[{"index":"a","health":"green"},{"index":"b","docs.count":"3"}]"#), 10);
        assert_eq!(out.results[0].columns.len(), 3);
        push_generic(&mut out, &j(r#"{"acknowledged":true}"#), 10);
        assert_eq!(out.results[1].rows[0], vec![json!("acknowledged"), json!(true)]);
        push_generic(&mut out, &j(r#"{"count":1,"_shards":{"total":1}}"#), 10);
        assert_eq!(out.results[2].columns[0].name, "response");
    }
}
