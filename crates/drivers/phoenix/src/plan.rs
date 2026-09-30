//! Phoenix `EXPLAIN`: rows with a `PLAN` step each (`CLIENT … SCAN OVER T`,
//! indented `SERVER FILTER BY …`, `CLIENT MERGE SORT`…) plus
//! `EST_BYTES_READ` / `EST_ROWS_READ`. Steps run top to bottom, each one
//! feeding the next, so the tree is that chain upside down: the last step
//! is the root and the first scan the leaf.

use dbine_driver::{Plan, PlanNode};

pub fn from_rows(statement: &str, steps: &[String], est_rows: Option<f64>, est_bytes: Option<f64>) -> Plan {
    let mut chain: Option<PlanNode> = None;
    for s in steps {
        let mut n = step(s);
        if let Some(prev) = chain.take() {
            n.children.push(prev);
        }
        chain = Some(n);
    }
    let mut root = chain.unwrap_or_else(|| PlanNode { op: "PLAN".into(), ..Default::default() });
    if let Some(r) = est_rows {
        root.est_rows = Some(r);
        root.props.push(("Filas a leer (estimado)".into(), fmt(r)));
    }
    if let Some(b) = est_bytes {
        root.props.push(("Bytes a leer (estimado)".into(), fmt(b)));
    }
    Plan {
        statement: statement.to_string(),
        root,
        actual: false,
        raw_format: "text".into(),
        raw: steps.join("\n"),
    }
}

fn fmt(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        v.to_string()
    }
}

/// `CLIENT 1-CHUNK PARALLEL 1-WAY ROUND ROBIN FULL SCAN OVER T` →
/// op `CLIENT FULL SCAN`, object `T`, the rest as detail.
fn step(line: &str) -> PlanNode {
    let text = line.trim();
    let mut n = PlanNode { op: text.to_string(), ..Default::default() };
    let words: Vec<&str> = text.split_whitespace().collect();
    let side = words.first().copied().filter(|w| matches!(*w, "CLIENT" | "SERVER"));
    if let Some(i) = words.iter().position(|w| *w == "OVER") {
        // `SCHEMA:TABLE` when schemas map to HBase namespaces.
        let object = words.get(i + 1).map(|s| s.replacen(':', ".", 1));
        // The scan kind: the words right before OVER that aren't sizes.
        let kind: Vec<&str> =
            words[..i].iter().copied().skip(1).filter(|w| !w.contains('-') && !w.chars().any(|c| c.is_ascii_digit()) && !matches!(*w, "ROWS" | "BYTES"))
            .collect();
        let kind = kind.join(" ");
        n.op = match side {
            Some(s) => format!("{s} {}", if kind.is_empty() { "SCAN" } else { &kind }),
            None => kind,
        };
        n.object = object;
        let rest = words.get(i + 2..).map(|w| w.join(" ")).unwrap_or_default();
        let sizes: Vec<&str> = words[1..i].iter().copied().filter(|w| w.contains('-')).collect();
        n.detail = [sizes.join(" "), rest].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
    } else if let Some(s) = side {
        // SERVER FILTER BY x = 1 → op SERVER FILTER BY, detail the rest.
        let cut = words.iter().position(|w| matches!(*w, "BY" | "INTO" | "WITH" | "LIMIT")).map(|p| p + 1);
        if let Some(p) = cut.filter(|p| *p < words.len()) {
            n.op = words[..p].join(" ");
            n.detail = words[p..].join(" ");
        } else {
            n.op = format!("{s} {}", words[1..].join(" "));
        }
    }
    if text.contains("FULL SCAN") {
        n.warnings.push("Recorre la tabla completa".into());
    }
    if text.contains("SKIP SCAN") {
        n.props.push(("Acceso".into(), "Skip scan sobre la clave".into()));
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_become_a_chain() {
        let rows = [
            "CLIENT 1-CHUNK PARALLEL 1-WAY ROUND ROBIN FULL SCAN OVER DBINE.T",
            "    SERVER FILTER BY X = 1",
            "    SERVER AGGREGATE INTO DISTINCT ROWS BY [Y]",
            "CLIENT MERGE SORT",
        ]
        .map(String::from);
        let p = from_rows("select y, count(*) from dbine.t where x = 1 group by y", &rows, Some(10.0), Some(1024.0));
        assert_eq!(p.root.op, "CLIENT MERGE SORT");
        assert_eq!(p.root.est_rows, Some(10.0));
        let agg = &p.root.children[0];
        assert_eq!(agg.op, "SERVER AGGREGATE INTO");
        assert_eq!(agg.detail, "DISTINCT ROWS BY [Y]");
        let filter = &agg.children[0];
        assert_eq!(filter.op, "SERVER FILTER BY");
        assert_eq!(filter.detail, "X = 1");
        let scan = &filter.children[0];
        assert_eq!(scan.op, "CLIENT PARALLEL ROUND ROBIN FULL SCAN");
        assert_eq!(scan.object.as_deref(), Some("DBINE.T"));
        assert_eq!(scan.detail, "1-CHUNK 1-WAY");
        assert_eq!(scan.warnings.len(), 1);
    }

    #[test]
    fn point_lookups() {
        let rows = ["CLIENT 1-CHUNK 1 ROWS 205 BYTES PARALLEL 1-WAY ROUND ROBIN POINT LOOKUP ON 1 KEY OVER T".to_string()];
        let p = from_rows("select * from t where id = 1", &rows, None, None);
        assert_eq!(p.root.object.as_deref(), Some("T"));
        assert!(p.root.warnings.is_empty());
    }
}
