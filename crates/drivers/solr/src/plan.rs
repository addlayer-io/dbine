//! Solr `debug` output → [`Plan`] trees.
//!
//! Solr has no plan without running the search. `debug=query` gives the
//! parsed query (`parsedquery_toString`, Lucene syntax) and the parsed
//! filter queries; `debug=timing` the time of each search component in the
//! prepare and process phases; `facet-debug` the facet processors.
//! Estimated: parsed query + filters. Actual: the request's phases and
//! components with their times, the parsed query under `process › query`.

use dbine_driver::{Plan, PlanNode};
use dbine_driver_elasticsearch::json::J;
use dbine_driver_elasticsearch::plan::{clip, lucene_tree, num};

/// Nodes for the parsed query and filter queries.
fn query_nodes(debug: &J) -> Vec<PlanNode> {
    let mut out = Vec::new();
    let parsed = debug.get("parsedquery_toString").or_else(|| debug.get("parsedquery")).map(J::text);
    if let Some(q) = parsed.filter(|q| !q.is_empty()) {
        out.push(lucene_tree(&q));
    }
    let fqs = debug.get("parsed_filter_queries").and_then(J::as_arr).unwrap_or(&[]);
    for fq in fqs {
        let t = fq.text();
        out.push(PlanNode { op: "Filter Query".into(), detail: clip(&t, 300), children: vec![lucene_tree(&t)], ..Default::default() });
    }
    out
}

/// A `facet-debug` entry and its sub-facets.
fn facet_node(f: &J) -> PlanNode {
    let op = ["processor", "action", "field"].iter().find_map(|k| f.get(k)).map(J::text).unwrap_or_else(|| "facet".into());
    let mut n = PlanNode { op, actual_ms: num(f.get("elapse")), ..Default::default() };
    n.object = f.get("field").map(J::text);
    for (k, v) in f.as_obj().map(Vec::as_slice).unwrap_or(&[]) {
        if k == "sub-facet" {
            for s in v.as_arr().unwrap_or(&[]) {
                n.children.push(facet_node(s));
            }
        } else if k != "elapse" {
            n.props.push((k.clone(), clip(&v.text(), 500)));
        }
    }
    n
}

/// The plan of a search from its response with `debug` output.
pub fn from_debug(statement: &str, core: &str, handler: &str, resp: &J, actual: bool) -> Plan {
    let debug = resp.get("debug").cloned().unwrap_or(J::Null);
    let mut root = PlanNode { op: handler.to_string(), object: Some(core.to_string()), ..Default::default() };
    for k in ["QParser", "querystring", "filter_queries"] {
        if let Some(v) = debug.get(k) {
            root.props.push((k.into(), clip(&v.text(), 2000)));
        }
    }
    if debug == J::Null {
        root.warnings.push("La respuesta no trajo la sección debug".into());
    }
    let queries = query_nodes(&debug);
    let timing = debug.get("timing").filter(|_| actual);
    match timing {
        Some(t) => {
            root.actual_ms = num(t.get("time")).or_else(|| num(resp.at(&["responseHeader", "QTime"])));
            root.actual_rows = num(resp.at(&["response", "numFound"]));
            let mut queries = Some(queries);
            for (phase, v) in t.as_obj().map(Vec::as_slice).unwrap_or(&[]) {
                if phase == "time" {
                    continue;
                }
                let mut p = PlanNode { op: capitalize(phase), actual_ms: num(v.get("time")), ..Default::default() };
                for (comp, cv) in v.as_obj().map(Vec::as_slice).unwrap_or(&[]) {
                    if comp == "time" {
                        continue;
                    }
                    let ms = num(cv.get("time"));
                    let mut c = PlanNode { op: comp.clone(), actual_ms: ms, ..Default::default() };
                    if phase == "process" && comp == "query" {
                        c.actual_rows = root.actual_rows;
                        c.children.extend(queries.take().unwrap_or_default());
                    } else if phase == "process" && comp == "facet" {
                        // The top entry only wraps the processors.
                        if let Some(f) = debug.get("facet-debug") {
                            match (f.get("processor"), f.get("sub-facet").and_then(J::as_arr)) {
                                (None, Some(subs)) => c.children.extend(subs.iter().map(facet_node)),
                                _ => c.children.push(facet_node(f)),
                            }
                        }
                    }
                    // Components that did nothing only add noise.
                    if ms.unwrap_or(0.0) > 0.0 || !c.children.is_empty() || comp == "query" {
                        p.children.push(c);
                    }
                }
                root.children.push(p);
            }
            if let Some(q) = queries {
                root.children.extend(q);
            }
        }
        None => root.children.extend(queries),
    }
    let actual = timing.is_some();
    let raw = J::Obj(vec![("debug".into(), debug)]);
    Plan { statement: statement.to_string(), root, actual, raw_format: "json".into(), raw: raw.pretty() }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESP: &str = r#"{"responseHeader":{"status":0,"QTime":57},
      "response":{"numFound":2,"start":0,"docs":[]},
      "debug":{"rawquerystring":"title_t:star AND year_i:[1970 TO *]","querystring":"title_t:star AND year_i:[1970 TO *]",
        "parsedquery_toString":"+title_t:star +IndexOrDocValuesQuery(indexQuery=year_i:[1970 TO 2147483647], dvQuery=year_i:[1970 TO 9223372036854775807])",
        "facet-debug":{"elapse":2,"sub-facet":[{"processor":"SimpleFacets","elapse":1,"action":"field facet",
          "sub-facet":[{"elapse":0,"appliedMethod":"FC","field":"genre_s","numBuckets":3}]}]},
        "QParser":"LuceneQParser","filter_queries":["genre_s:scifi"],"parsed_filter_queries":["genre_s:scifi"],
        "timing":{"time":57.0,
          "prepare":{"time":23.0,"query":{"time":21.0},"facet":{"time":0.0},"debug":{"time":0.0}},
          "process":{"time":29.0,"query":{"time":24.0},"facet":{"time":4.0},"highlight":{"time":0.0}}}}}"#;

    #[test]
    fn timing_tree() {
        let p = from_debug("GET /solr/films/select?q=…", "films", "select", &J::parse(RESP).unwrap(), true);
        assert!(p.actual);
        let r = &p.root;
        assert_eq!(r.actual_ms, Some(57.0));
        assert_eq!(r.actual_rows, Some(2.0));
        assert_eq!(r.children.iter().map(|c| c.op.as_str()).collect::<Vec<_>>(), ["Prepare", "Process"]);
        let process = &r.children[1];
        assert_eq!(process.children.iter().map(|c| c.op.as_str()).collect::<Vec<_>>(), ["query", "facet"]);
        let q = &process.children[0];
        assert_eq!(q.actual_ms, Some(24.0));
        assert_eq!(q.children[0].op, "BooleanQuery");
        assert_eq!(q.children[0].children[0].object.as_deref(), Some("title_t"));
        assert_eq!(q.children[1].op, "Filter Query");
        let facet = &process.children[1].children[0];
        assert_eq!(facet.op, "SimpleFacets");
        assert_eq!(facet.children[0].object.as_deref(), Some("genre_s"));
    }

    #[test]
    fn estimated_is_the_parsed_query() {
        let p = from_debug("q", "films", "select", &J::parse(RESP).unwrap(), false);
        assert!(!p.actual);
        assert_eq!(p.root.children[0].op, "BooleanQuery");
        assert_eq!(p.root.children[1].op, "Filter Query");
        assert_eq!(p.root.actual_ms, None);
    }
}
