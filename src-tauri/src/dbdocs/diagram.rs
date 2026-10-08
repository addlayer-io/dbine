//! The ER diagram of the HTML document: inline SVG, laid out in layers.
//! A table goes one layer right of every table it references (referenced
//! tables on the left); cycles stop growing after a few rounds. A layer
//! taller than a column wraps into several. Each box links to its table.

use super::html::esc;
use dbine_driver::TableSchema;
use std::fmt::Write;

const CHAR_W: f64 = 6.6;
const ROW_H: f64 = 16.0;
const HEAD_H: f64 = 22.0;
const PAD: f64 = 8.0;
const GAP_X: f64 = 70.0;
const GAP_Y: f64 = 26.0;
const MIN_W: f64 = 120.0;
const MAX_W: f64 = 300.0;
/// Columns drawn per box; the rest is "+N".
const SHOWN_COLUMNS: usize = 12;
/// Layering rounds (and layers) at most: cycles stop here.
const MAX_LAYERS: usize = 30;

struct Node {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

fn cut(s: &str, max: usize) -> String {
    if chars(s) <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
    }
}

/// Index of the table `fk` points at, schema-aware.
fn target(tables: &[&TableSchema], from: &TableSchema, ref_schema: Option<&str>, ref_table: &str) -> Option<usize> {
    let sc = ref_schema.or(from.schema.as_deref()).unwrap_or("");
    tables
        .iter()
        .position(|t| t.name.eq_ignore_ascii_case(ref_table) && t.schema.as_deref().unwrap_or("").eq_ignore_ascii_case(sc))
        .or_else(|| ref_schema.is_none().then(|| tables.iter().position(|t| t.name.eq_ignore_ascii_case(ref_table))).flatten())
}

/// The SVG for `tables`. `id` keeps marker ids unique on the page;
/// `link` gives a table's anchor.
pub fn svg(tables: &[&TableSchema], id: &str, link: &dyn Fn(&TableSchema) -> Option<String>) -> String {
    let n = tables.len();
    if n == 0 {
        return String::new();
    }
    // Edges: (child, child row, parent).
    let mut edges: Vec<(usize, Option<usize>, usize)> = Vec::new();
    for (i, t) in tables.iter().enumerate() {
        for fk in &t.foreign_keys {
            if let Some(j) = target(tables, t, fk.ref_schema.as_deref(), &fk.ref_table) {
                let row = fk.columns.first().and_then(|c| t.columns.iter().position(|col| col.name.eq_ignore_ascii_case(c))).filter(|r| *r < SHOWN_COLUMNS);
                edges.push((i, row, j));
            }
        }
    }

    // Layers.
    let mut layer = vec![0usize; n];
    for _ in 0..MAX_LAYERS.min(n) {
        let mut changed = false;
        for &(c, _, p) in &edges {
            if c != p && layer[c] < layer[p] + 1 && layer[p] + 1 < MAX_LAYERS {
                layer[c] = layer[p] + 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Sizes.
    let mut nodes: Vec<Node> = tables
        .iter()
        .map(|t| {
            let name_w = chars(&t.name) as f64 * (CHAR_W + 0.6);
            let col_w = t.columns.iter().take(SHOWN_COLUMNS).map(|c| (chars(&cut(&c.name, 24)) + chars(&cut(&c.data_type, 16)) + 3) as f64 * CHAR_W).fold(0.0, f64::max);
            let w = (name_w.max(col_w) + 2.0 * PAD).clamp(MIN_W, MAX_W);
            let rows = t.columns.len().min(SHOWN_COLUMNS) + usize::from(t.columns.len() > SHOWN_COLUMNS);
            Node { x: 0.0, y: 0.0, w, h: HEAD_H + rows.max(1) as f64 * ROW_H + 4.0 }
        })
        .collect();

    // Columns: each layer in order, the layer sorted by its parents' place.
    let per_col = ((n as f64).sqrt() * 1.3).ceil().max(6.0) as usize;
    let layers = layer.iter().copied().max().unwrap_or(0) + 1;
    let mut place = vec![0f64; n];
    let mut x = 0.0;
    let mut height: f64 = 0.0;
    for l in 0..layers {
        let mut members: Vec<usize> = (0..n).filter(|&i| layer[i] == l).collect();
        let weight = |i: usize| {
            let parents: Vec<f64> = edges.iter().filter(|e| e.0 == i && e.2 != i && layer[e.2] < l).map(|e| place[e.2]).collect();
            if parents.is_empty() { f64::MAX } else { parents.iter().sum::<f64>() / parents.len() as f64 }
        };
        members.sort_by(|&a, &b| weight(a).total_cmp(&weight(b)).then_with(|| tables[a].name.to_lowercase().cmp(&tables[b].name.to_lowercase())));
        for chunk in members.chunks(per_col) {
            let w = chunk.iter().map(|&i| nodes[i].w).fold(0.0, f64::max);
            let mut y = 0.0;
            for (k, &i) in chunk.iter().enumerate() {
                nodes[i].x = x;
                nodes[i].y = y;
                place[i] = k as f64;
                y += nodes[i].h + GAP_Y;
            }
            height = height.max(y - GAP_Y);
            x += w + GAP_X;
        }
    }
    let margin = 12.0;
    let width = x - GAP_X + 2.0 * margin + 40.0;
    let height = height + 2.0 * margin;

    let mut out = String::new();
    let _ = write!(
        out,
        r#"<svg class="erd-svg" xmlns="http://www.w3.org/2000/svg" width="{w:.0}" height="{h:.0}" viewBox="0 0 {w:.0} {h:.0}" role="img"><defs><marker id="{id}-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z" class="erd-head"/></marker></defs><g transform="translate({m},{m})">"#,
        w = width,
        h = height,
        id = esc(id),
        m = margin
    );

    // Edges under the boxes.
    for &(c, row, p) in &edges {
        let (a, b) = (&nodes[c], &nodes[p]);
        let cy = a.y + row.map_or(HEAD_H / 2.0, |r| HEAD_H + (r as f64 + 0.5) * ROW_H);
        let py = b.y + HEAD_H / 2.0;
        let d = if c == p {
            let x0 = a.x + a.w;
            format!("M{x0:.1},{cy:.1} C{:.1},{cy:.1} {:.1},{py:.1} {x0:.1},{py:.1}", x0 + 36.0, x0 + 36.0)
        } else if b.x + b.w <= a.x {
            let (x0, x1) = (a.x, b.x + b.w);
            let mid = (x0 - x1) / 2.0;
            format!("M{x0:.1},{cy:.1} C{:.1},{cy:.1} {:.1},{py:.1} {x1:.1},{py:.1}", x0 - mid, x1 + mid)
        } else if a.x + a.w <= b.x {
            let (x0, x1) = (a.x + a.w, b.x);
            let mid = (x1 - x0) / 2.0;
            format!("M{x0:.1},{cy:.1} C{:.1},{cy:.1} {:.1},{py:.1} {x1:.1},{py:.1}", x0 + mid, x1 - mid)
        } else {
            // Same column: around the right side.
            let x0 = a.x + a.w;
            let x1 = b.x + b.w;
            let bulge = x0.max(x1) + 30.0;
            format!("M{x0:.1},{cy:.1} C{bulge:.1},{cy:.1} {bulge:.1},{py:.1} {x1:.1},{py:.1}")
        };
        let _ = write!(out, r#"<path class="erd-edge" d="{d}" marker-end="url(#{}-arrow)"/>"#, esc(id));
    }

    for (i, t) in tables.iter().enumerate() {
        let nd = &nodes[i];
        let open = match link(t) {
            Some(a) => format!(r##"<a href="#{}">"##, esc(&a)),
            None => String::new(),
        };
        let _ = write!(out, r#"{open}<g class="erd-box" transform="translate({:.1},{:.1})">"#, nd.x, nd.y);
        let title = match &t.comment {
            Some(c) if !c.trim().is_empty() => format!("{}: {}", super::qualified(t.schema.as_deref(), &t.name), c),
            _ => super::qualified(t.schema.as_deref(), &t.name),
        };
        let _ = write!(out, "<title>{}</title>", esc(&title));
        let _ = write!(out, r#"<rect class="erd-rect" width="{:.1}" height="{:.1}" rx="4"/>"#, nd.w, nd.h);
        let _ = write!(out, r#"<rect class="erd-top" width="{:.1}" height="{HEAD_H}" rx="4"/>"#, nd.w);
        let max_name = ((nd.w - 2.0 * PAD) / (CHAR_W + 0.6)) as usize;
        let _ = write!(out, r#"<text class="erd-name" x="{PAD}" y="15">{}</text>"#, esc(&cut(&t.name, max_name)));
        let pk = |c: &str| t.primary_key.as_ref().is_some_and(|k| k.columns.iter().any(|x| x.eq_ignore_ascii_case(c)));
        let fk = |c: &str| t.foreign_keys.iter().any(|f| f.columns.iter().any(|x| x.eq_ignore_ascii_case(c)));
        for (r, c) in t.columns.iter().take(SHOWN_COLUMNS).enumerate() {
            let y = HEAD_H + (r as f64 + 1.0) * ROW_H - 4.0;
            let class = if pk(&c.name) { "erd-col erd-pk" } else if fk(&c.name) { "erd-col erd-fk" } else { "erd-col" };
            let _ = write!(out, r#"<text class="{class}" x="{PAD}" y="{y:.1}">{}</text>"#, esc(&cut(&c.name, 24)));
            let _ = write!(out, r#"<text class="erd-type" x="{:.1}" y="{y:.1}" text-anchor="end">{}</text>"#, nd.w - PAD, esc(&cut(&c.data_type, 16)));
        }
        if t.columns.len() > SHOWN_COLUMNS {
            let y = HEAD_H + (SHOWN_COLUMNS as f64 + 1.0) * ROW_H - 4.0;
            let _ = write!(out, r#"<text class="erd-type" x="{PAD}" y="{y:.1}">+{}</text>"#, t.columns.len() - SHOWN_COLUMNS);
        }
        out.push_str("</g>");
        if !open.is_empty() {
            out.push_str("</a>");
        }
    }
    out.push_str("</g></svg>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ColumnDef, ForeignKeyDef};

    fn table(name: &str, refs: &[&str]) -> TableSchema {
        TableSchema {
            kind: "table".into(),
            name: name.into(),
            columns: vec![ColumnDef { name: "id".into(), data_type: "int".into(), ..Default::default() }, ColumnDef { name: "ref_id".into(), data_type: "int".into(), ..Default::default() }],
            foreign_keys: refs
                .iter()
                .map(|r| ForeignKeyDef { name: None, columns: vec!["ref_id".into()], ref_schema: None, ref_table: r.to_string(), ref_columns: vec!["id".into()], on_delete: None, on_update: None })
                .collect(),
            ..Default::default()
        }
    }

    fn x_of(svg: &str, name: &str) -> f64 {
        // The box's translate comes right before its title.
        let at = svg.find(&format!("<title>{name}</title>")).unwrap();
        let head = &svg[..at];
        let t = head.rfind("translate(").unwrap();
        head[t + 10..].split(',').next().unwrap().parse().unwrap()
    }

    #[test]
    fn referenced_tables_go_left() {
        let (a, b, c) = (table("orders", &["users"]), table("users", &[]), table("lines", &["orders", "lines"]));
        let svg = svg(&[&a, &b, &c], "d1", &|t| Some(format!("t-{}", t.name)));
        assert!(x_of(&svg, "users") < x_of(&svg, "orders"));
        assert!(x_of(&svg, "orders") < x_of(&svg, "lines"));
        assert_eq!(svg.matches("erd-edge").count(), 3);
        assert!(svg.contains(r##"<a href="#t-users">"##));
    }

    #[test]
    fn cycles_end_and_names_are_escaped() {
        let (a, b) = (table("a<x>", &["b&y"]), table("b&y", &["a<x>"]));
        let svg = svg(&[&a, &b], "d\"2", &|_| None);
        assert!(svg.contains("a&lt;x&gt;") && svg.contains("b&amp;y"));
        assert!(!svg.contains("a<x>") && !svg.contains("d\"2"));
    }
}
