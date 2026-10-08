//! The HTML document: one file with its CSS and script inline, an index on
//! the side with a search box, light and dark themes (the system's, or the
//! reader's choice), and a print layout (the index hides, code opens).
//! Every text from the database goes through [`esc`].

use super::{diagram, key_marks, qualified, thousands, Anchors, Doc, Labels, ObjectDoc, TableDoc, DIAGRAM_MAX};
use dbine_driver::{ColumnDef, TableSchema};
use std::fmt::Write;

/// Escape for HTML text and attribute values.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// DBine's logo, icon and wordmark (scripts/brand/), inline so the file
/// stands alone; the wordmark's colors follow the theme.
const LOGO: &str = include_str!("../../../web/public/brand/dbine-logo-horizontal.svg");

/// What the search box matches, lowercased.
fn search_text<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    esc(&parts.into_iter().collect::<Vec<_>>().join(" ").to_lowercase())
}

const CSS: &str = r#"
:root{--dbine-deep:#2448c8;--dbine-lite:#4f9df5;--bg:#ffffff;--bg2:#f5f6f8;--fg:#1f2328;--dim:#646c76;--border:#d8dce1;--accent:#2563eb;--code:#f1f3f5;--pk:#b45309;--fk:#2563eb;--edge:#8a94a3}
@media (prefers-color-scheme: dark){:root:not([data-theme=light]){--dbine-deep:#6d98ff;--dbine-lite:#a6d3ff;--bg:#1e1f22;--bg2:#26282c;--fg:#e3e5e8;--dim:#9aa1ab;--border:#3a3d43;--accent:#6ea8fe;--code:#2b2d31;--pk:#f0b45a;--fk:#6ea8fe;--edge:#7b8491}}
:root[data-theme=dark]{--dbine-deep:#6d98ff;--dbine-lite:#a6d3ff;--bg:#1e1f22;--bg2:#26282c;--fg:#e3e5e8;--dim:#9aa1ab;--border:#3a3d43;--accent:#6ea8fe;--code:#2b2d31;--pk:#f0b45a;--fk:#6ea8fe;--edge:#7b8491}
*{box-sizing:border-box}
html,body{margin:0;background:var(--bg);color:var(--fg);font:14px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif}
a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}
aside{position:fixed;top:0;left:0;bottom:0;width:290px;overflow:auto;border-right:1px solid var(--border);background:var(--bg2);padding:14px 12px}
aside .product{margin-bottom:14px}aside .product>svg{height:44px;width:auto;display:block}aside .brand{font-weight:600;margin-bottom:8px;word-break:break-word}
aside input{width:100%;padding:6px 8px;border:1px solid var(--border);border-radius:4px;background:var(--bg);color:var(--fg);margin-bottom:10px}
aside ul{list-style:none;margin:0 0 6px;padding:0 0 0 10px}
aside li{white-space:nowrap;overflow:hidden;text-overflow:ellipsis;font-size:13px}
aside .toc-schema{font-weight:600;margin-top:8px;font-size:13px}
aside .toc-kind{color:var(--dim);font-size:12px;margin-top:4px}
main{margin-left:290px;padding:24px 32px 60px;max-width:1200px}
header h1{margin:0 0 6px;font-size:24px}
header dl{display:grid;grid-template-columns:max-content 1fr;gap:2px 14px;margin:0 0 10px;color:var(--dim)}
header dt{font-weight:600}header dd{margin:0}
.tools{display:flex;gap:8px;margin-bottom:12px}
.tools button{border:1px solid var(--border);background:var(--bg2);color:var(--fg);border-radius:4px;padding:4px 10px;cursor:pointer;font:inherit;font-size:12px}
h2{font-size:20px;border-bottom:2px solid var(--border);padding-bottom:4px;margin-top:36px}
h3{font-size:16px;margin:26px 0 8px;color:var(--dim);text-transform:uppercase;letter-spacing:.04em}
h4{font-size:16px;margin:0 0 6px}
.obj{border:1px solid var(--border);border-radius:6px;padding:12px 16px;margin:12px 0;scroll-margin-top:12px}
.obj h5{font-size:13px;margin:12px 0 4px;color:var(--dim)}
.comment{white-space:pre-wrap;margin:4px 0 8px}
.dim{color:var(--dim)}
table{border-collapse:collapse;width:100%;font-size:13px}
th,td{border:1px solid var(--border);padding:3px 8px;text-align:left;vertical-align:top}
th{background:var(--bg2);font-weight:600}
td.pre{white-space:pre-wrap}
.pk{color:var(--pk);font-weight:600}.fk{color:var(--fk);font-weight:600}
code,pre{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:12.5px}
pre{background:var(--code);border:1px solid var(--border);border-radius:4px;padding:10px;overflow:auto;white-space:pre}
details summary{cursor:pointer;color:var(--accent);margin:8px 0 4px}
.notes{border-left:3px solid var(--pk);padding:4px 12px;background:var(--bg2)}
.erd{overflow:auto;border:1px solid var(--border);border-radius:6px;background:var(--bg2);margin:10px 0}
.erd-rect{fill:var(--bg);stroke:var(--border)}.erd-top{fill:var(--accent);opacity:.16}
.erd-name{font:600 12px sans-serif;fill:var(--fg)}.erd-col{font:11px sans-serif;fill:var(--fg)}
.erd-pk{fill:var(--pk);font-weight:600}.erd-fk{fill:var(--fk)}.erd-type{font:10.5px sans-serif;fill:var(--dim)}
.erd-edge{fill:none;stroke:var(--edge);stroke-width:1.2}.erd-head{fill:var(--edge)}
.erd a:hover .erd-rect{stroke:var(--accent)}
[hidden]{display:none!important}
@media (max-width:800px){aside{position:static;width:auto;border-right:0;border-bottom:1px solid var(--border)}main{margin-left:0;padding:16px}}
@media print{aside,.tools{display:none}main{margin:0;max-width:none;padding:0}.obj{break-inside:avoid}.erd{overflow:visible}}
"#;

const SCRIPT: &str = r#"(function(){
var root=document.documentElement,q=document.getElementById('q');
try{var saved=localStorage.getItem('dbdocs-theme');if(saved)root.setAttribute('data-theme',saved);}catch(e){}
q.addEventListener('input',function(){
var v=q.value.trim().toLowerCase();
document.querySelectorAll('[data-s]').forEach(function(e){e.hidden=v!==''&&e.getAttribute('data-s').indexOf(v)<0;});
document.querySelectorAll('.toc-group').forEach(function(g){g.hidden=v!==''&&!g.querySelector('li:not([hidden])');});
});
document.getElementById('theme').addEventListener('click',function(){
var cur=root.getAttribute('data-theme');
var dark=cur?cur==='dark':window.matchMedia('(prefers-color-scheme: dark)').matches;
var next=dark?'light':'dark';root.setAttribute('data-theme',next);
try{localStorage.setItem('dbdocs-theme',next);}catch(e){}
});
document.getElementById('print').addEventListener('click',function(){window.print();});
window.addEventListener('beforeprint',function(){document.querySelectorAll('details').forEach(function(d){d.open=true;});});
})();"#;

pub fn render(doc: &Doc, l: &Labels) -> String {
    let anchors = Anchors::new(doc);
    let mut obj_ids: Vec<Vec<Vec<String>>> = Vec::new();
    let mut n = 0;
    for s in &doc.schemas {
        obj_ids.push(
            s.groups
                .iter()
                .map(|g| {
                    g.items
                        .iter()
                        .map(|_| {
                            n += 1;
                            format!("o{n}")
                        })
                        .collect()
                })
                .collect(),
        );
    }
    let schema_title = |name: &Option<String>| match name {
        Some(n) => format!("{} {}", l.get("schema"), n),
        None => l.get("noSchema").to_string(),
    };

    let mut out = String::with_capacity(64 * 1024);
    let title = format!("{} · {}", l.get("title"), doc.database);
    let _ = write!(
        out,
        "<!DOCTYPE html>\n<html lang=\"{}\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><meta name=\"generator\" content=\"DBine\"><title>{}</title><style>{CSS}</style></head><body>\n",
        esc(l.get("lang")),
        esc(&title)
    );

    // The index.
    let logo = LOGO.trim();
    let _ = write!(out, "<aside><div class=\"product\">{logo}</div><div class=\"brand\">{}</div><input id=\"q\" type=\"search\" placeholder=\"{}\" aria-label=\"{}\"><nav>", esc(&doc.database), esc(l.get("search")), esc(l.get("search")));
    let diagrams = diagram_sets(doc);
    if doc.diagram && !diagrams.0.is_empty() {
        let _ = write!(out, "<ul><li><a href=\"#diagram\">{}</a></li></ul>", esc(l.get("diagram")));
    }
    for (si, s) in doc.schemas.iter().enumerate() {
        let _ = write!(out, "<div class=\"toc-group\"><div class=\"toc-schema\"><a href=\"#s{}\">{}</a></div>", si + 1, esc(&schema_title(&s.name)));
        let mut last_kind = "";
        for t in &s.tables {
            if t.kind_label != last_kind {
                if !last_kind.is_empty() {
                    out.push_str("</ul>");
                }
                let _ = write!(out, "<div class=\"toc-kind\">{}</div><ul>", esc(&t.kind_label));
                last_kind = &t.kind_label;
            }
            let id = anchors.table(t.table.schema.as_deref(), None, &t.table.name).unwrap_or("");
            let _ = write!(out, "<li data-s=\"{}\"><a href=\"#{}\">{}</a></li>", table_search(&t.table), esc(id), esc(&t.table.name));
        }
        if !last_kind.is_empty() {
            out.push_str("</ul>");
        }
        for (gi, g) in s.groups.iter().enumerate() {
            let _ = write!(out, "<div class=\"toc-kind\">{}</div><ul>", esc(&g.label));
            for (ii, o) in g.items.iter().enumerate() {
                let _ = write!(out, "<li data-s=\"{}\"><a href=\"#{}\">{}</a></li>", object_search(o), obj_ids[si][gi][ii], esc(&o.name));
            }
            out.push_str("</ul>");
        }
        out.push_str("</div>");
    }
    out.push_str("</nav></aside>\n<main>");

    // Header.
    let engine = match &doc.version {
        Some(v) => format!("{} · {}", doc.engine, v),
        None => doc.engine.clone(),
    };
    let _ = write!(out, "<header><h1>{}</h1><dl>", esc(&title));
    for (k, v) in [("database", &doc.database), ("connection", &doc.connection), ("engine", &engine), ("generated", &doc.generated_at)] {
        if !v.is_empty() {
            let _ = write!(out, "<dt>{}</dt><dd>{}</dd>", esc(l.get(k)), esc(v));
        }
    }
    let _ = write!(
        out,
        "</dl><div class=\"tools\"><button id=\"theme\" type=\"button\">{}</button><button id=\"print\" type=\"button\">{}</button></div></header>\n",
        esc(l.get("theme")),
        esc(l.get("print"))
    );
    if !doc.notes.is_empty() {
        let _ = write!(out, "<div class=\"notes\"><strong>{}</strong><ul>", esc(l.get("notes")));
        for note in &doc.notes {
            let _ = write!(out, "<li>{}</li>", esc(note));
        }
        out.push_str("</ul></div>");
    }

    // The diagram(s).
    if doc.diagram {
        let (sets, skipped) = diagrams;
        if !sets.is_empty() || !skipped.is_empty() {
            let _ = write!(out, "<h2 id=\"diagram\">{}</h2>", esc(l.get("diagram")));
        }
        let link = |t: &TableSchema| anchors.table(t.schema.as_deref(), None, &t.name).map(str::to_string);
        for (i, (name, tables)) in sets.iter().enumerate() {
            if let Some(n) = name {
                let _ = write!(out, "<h5>{}</h5>", esc(&l.fill("diagramFor", &[("name", n.clone())])));
            }
            let _ = write!(out, "<div class=\"erd\">{}</div>", diagram::svg(tables, &format!("erd{}", i + 1), &link));
        }
        for count in skipped {
            let _ = write!(out, "<p class=\"dim\">{}</p>", esc(&l.fill("diagramSkipped", &[("count", count.to_string()), ("max", DIAGRAM_MAX.to_string())])));
        }
    }

    // Schemas.
    for (si, s) in doc.schemas.iter().enumerate() {
        let _ = write!(out, "<h2 id=\"s{}\">{}</h2>", si + 1, esc(&schema_title(&s.name)));
        let mut last_kind = "";
        for t in &s.tables {
            if t.kind_label != last_kind {
                let _ = write!(out, "<h3>{}</h3>", esc(&t.kind_label));
                last_kind = &t.kind_label;
            }
            table(&mut out, doc, t, &anchors, l);
        }
        for (gi, g) in s.groups.iter().enumerate() {
            let _ = write!(out, "<h3>{}</h3>", esc(&g.label));
            for (ii, o) in g.items.iter().enumerate() {
                object(&mut out, doc, o, &obj_ids[si][gi][ii], l);
            }
        }
    }
    let _ = write!(out, "</main><script>{SCRIPT}</script></body></html>\n");
    out
}

/// Diagrams to draw (`None` name: the whole database; else per schema),
/// and the table counts of what's too big to draw.
fn diagram_sets(doc: &Doc) -> (Vec<(Option<String>, Vec<&TableSchema>)>, Vec<usize>) {
    let all: Vec<&TableSchema> = doc.schemas.iter().flat_map(|s| s.tables.iter().map(|t| &t.table)).collect();
    if !doc.diagram || all.is_empty() {
        return (Vec::new(), Vec::new());
    }
    if all.len() <= DIAGRAM_MAX {
        return (vec![(None, all)], Vec::new());
    }
    let mut sets = Vec::new();
    let mut skipped = Vec::new();
    for s in &doc.schemas {
        let tables: Vec<&TableSchema> = s.tables.iter().map(|t| &t.table).collect();
        if tables.is_empty() {
            continue;
        }
        if tables.len() <= DIAGRAM_MAX && doc.schemas.len() > 1 {
            sets.push((Some(s.name.clone().unwrap_or_default()), tables));
        } else {
            skipped.push(tables.len());
        }
    }
    (sets, skipped)
}

fn table_search(t: &TableSchema) -> String {
    search_text(std::iter::once(t.name.as_str()).chain(t.schema.as_deref()).chain(t.columns.iter().map(|c| c.name.as_str())))
}

fn object_search(o: &ObjectDoc) -> String {
    search_text(std::iter::once(o.name.as_str()).chain(o.schema.as_deref()).chain(o.parent.as_deref()).chain(o.columns.iter().map(|c| c.name.as_str())))
}

fn yes_no(l: &Labels, v: bool) -> &str {
    l.get(if v { "yes" } else { "no" })
}

/// A link to a table when it's in the document, else its name.
fn table_link(anchors: &Anchors, schema: Option<&str>, default_schema: Option<&str>, name: &str) -> String {
    let text = esc(&qualified(schema, name));
    match anchors.table(schema, default_schema, name) {
        Some(id) => format!("<a href=\"#{}\">{text}</a>", esc(id)),
        None => text,
    }
}

fn columns_table(out: &mut String, cols: &[ColumnDef], t: Option<&TableSchema>, anchors: &Anchors, l: &Labels) {
    if cols.is_empty() {
        let _ = write!(out, "<p class=\"dim\">{}</p>", esc(l.get("noColumns")));
        return;
    }
    let with_key = t.is_some();
    let with_comment = cols.iter().any(|c| c.comment.as_deref().is_some_and(|x| !x.trim().is_empty()));
    let _ = write!(out, "<table><thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th>", esc(l.get("column")), esc(l.get("type")), esc(l.get("nullable")), esc(l.get("default")));
    if with_key {
        let _ = write!(out, "<th>{}</th>", esc(l.get("key")));
    }
    if with_comment {
        let _ = write!(out, "<th>{}</th>", esc(l.get("comment")));
    }
    out.push_str("</tr></thead><tbody>");
    for c in cols {
        let mut ty = esc(&c.data_type);
        if c.auto_increment {
            let _ = write!(ty, " <span class=\"dim\">({})</span>", esc(l.get("autoIncrement")));
        }
        let _ = write!(out, "<tr><td><code>{}</code></td><td>{ty}</td><td>{}</td><td>{}</td>", esc(&c.name), esc(yes_no(l, c.nullable)), esc(c.default_value.as_deref().unwrap_or("")));
        if let Some(t) = t {
            let marks = key_marks(t, &c.name);
            let mut cell = String::new();
            if marks.starts_with("PK") {
                cell.push_str("<span class=\"pk\">PK</span>");
            }
            for fk in t.foreign_keys.iter().filter(|f| f.columns.iter().any(|x| x.eq_ignore_ascii_case(&c.name))) {
                if !cell.is_empty() {
                    cell.push(' ');
                }
                let _ = write!(cell, "<span class=\"fk\">FK</span> → {}", table_link(anchors, fk.ref_schema.as_deref(), t.schema.as_deref(), &fk.ref_table));
            }
            let _ = write!(out, "<td>{cell}</td>");
        }
        if with_comment {
            let _ = write!(out, "<td class=\"pre\">{}</td>", esc(c.comment.as_deref().unwrap_or("")));
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table>");
}

fn table(out: &mut String, doc: &Doc, td: &TableDoc, anchors: &Anchors, l: &Labels) {
    let t = &td.table;
    let id = anchors.table(t.schema.as_deref(), None, &t.name).unwrap_or("");
    let _ = write!(out, "<section class=\"obj\" id=\"{}\" data-s=\"{}\"><h4>{}</h4>", esc(id), table_search(t), esc(&qualified(t.schema.as_deref(), &t.name)));
    if let Some(c) = t.comment.as_deref().filter(|c| !c.trim().is_empty()) {
        let _ = write!(out, "<p class=\"comment\">{}</p>", esc(c));
    }
    if let Some(rows) = td.rows {
        let _ = write!(out, "<p class=\"dim\">{}: {}</p>", esc(l.get("rows")), thousands(rows));
    }
    let _ = write!(out, "<h5>{}</h5>", esc(l.get("columns")));
    columns_table(out, &t.columns, Some(t), anchors, l);

    if let Some(pk) = &t.primary_key {
        let name = pk.name.as_deref().map(|n| format!("<code>{}</code> ", esc(n))).unwrap_or_default();
        let _ = write!(out, "<h5>{}</h5><p>{name}({})</p>", esc(l.get("primaryKey")), esc(&pk.columns.join(", ")));
    }
    if doc.foreign_keys && !t.foreign_keys.is_empty() {
        let _ = write!(
            out,
            "<h5>{}</h5><table><thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>",
            esc(l.get("foreignKeys")),
            esc(l.get("name")),
            esc(l.get("columns")),
            esc(l.get("references")),
            esc(l.get("onDelete")),
            esc(l.get("onUpdate"))
        );
        for fk in &t.foreign_keys {
            let _ = write!(
                out,
                "<tr><td><code>{}</code></td><td>{}</td><td>{} ({})</td><td>{}</td><td>{}</td></tr>",
                esc(fk.name.as_deref().unwrap_or("")),
                esc(&fk.columns.join(", ")),
                table_link(anchors, fk.ref_schema.as_deref(), t.schema.as_deref(), &fk.ref_table),
                esc(&fk.ref_columns.join(", ")),
                esc(fk.on_delete.as_deref().unwrap_or("")),
                esc(fk.on_update.as_deref().unwrap_or(""))
            );
        }
        out.push_str("</tbody></table>");
    }
    if doc.indexes && !t.indexes.is_empty() {
        let _ = write!(
            out,
            "<h5>{}</h5><table><thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>",
            esc(l.get("indexes")),
            esc(l.get("name")),
            esc(l.get("columns")),
            esc(l.get("unique")),
            esc(l.get("type")),
            esc(l.get("included")),
            esc(l.get("filter"))
        );
        for ix in &t.indexes {
            let _ = write!(
                out,
                "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&ix.name),
                esc(&ix.columns.join(", ")),
                esc(yes_no(l, ix.unique)),
                esc(ix.kind.as_deref().unwrap_or("")),
                esc(&ix.include.join(", ")),
                esc(ix.filter.as_deref().unwrap_or(""))
            );
        }
        out.push_str("</tbody></table>");
    }
    if !t.checks.is_empty() {
        let _ = write!(out, "<h5>{}</h5><table><thead><tr><th>{}</th><th>{}</th></tr></thead><tbody>", esc(l.get("checks")), esc(l.get("name")), esc(l.get("condition")));
        for ck in &t.checks {
            let _ = write!(out, "<tr><td><code>{}</code></td><td><code>{}</code></td></tr>", esc(ck.name.as_deref().unwrap_or("")), esc(&ck.expression));
        }
        out.push_str("</tbody></table>");
    }
    if !t.options.is_empty() {
        let _ = write!(out, "<h5>{}</h5><table><tbody>", esc(l.get("options")));
        for (k, v) in &t.options {
            let _ = write!(out, "<tr><th>{}</th><td>{}</td></tr>", esc(k), esc(v));
        }
        out.push_str("</tbody></table>");
    }
    if !td.triggers.is_empty() {
        let _ = write!(out, "<h5>{}</h5><p>{}</p>", esc(l.get("triggers")), esc(&td.triggers.join(", ")));
    }
    if doc.dependencies && !td.used_by.is_empty() {
        let _ = write!(out, "<h5>{}</h5><ul>", esc(l.get("usedBy")));
        for u in &td.used_by {
            let name = if u.table { table_link(anchors, u.schema.as_deref(), None, &u.name) } else { esc(&qualified(u.schema.as_deref(), &u.name)) };
            let _ = write!(out, "<li>{name} <span class=\"dim\">· {} · {}</span></li>", esc(&u.kind_label), esc(&u.how));
        }
        out.push_str("</ul>");
    }
    out.push_str("</section>\n");
}

fn object(out: &mut String, doc: &Doc, o: &ObjectDoc, id: &str, l: &Labels) {
    let _ = write!(out, "<section class=\"obj\" id=\"{}\" data-s=\"{}\"><h4>{}</h4>", esc(id), object_search(o), esc(&qualified(o.schema.as_deref(), &o.name)));
    if let Some(p) = &o.parent {
        let _ = write!(out, "<p class=\"dim\">{}: {}</p>", esc(l.get("parent")), esc(p));
    }
    if let Some(c) = o.comment.as_deref() {
        let _ = write!(out, "<p class=\"comment\">{}</p>", esc(c));
    }
    if !o.columns.is_empty() {
        let _ = write!(out, "<h5>{}</h5>", esc(l.get("columns")));
        columns_table(out, &o.columns, None, &Anchors { tables: Default::default() }, l);
    }
    if let Some(src) = o.source.as_deref().filter(|_| doc.source) {
        let _ = write!(out, "<details><summary>{}</summary><pre><code>{}</code></pre></details>", esc(l.get("source")), esc(src.trim_end()));
    }
    out.push_str("</section>\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbdocs::tests::sample;
    use std::collections::BTreeMap;

    #[test]
    fn escapes_everything() {
        assert_eq!(esc(r#"<a href="x">'&'</a>"#), "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;");
        let html = render(&sample(), &Labels::new(&BTreeMap::new()));
        for raw in ["users<script>", "<img src=x", "<b>bold</b>", "shop</title>", "<i>"] {
            assert!(!html.contains(raw), "unescaped {raw}");
        }
        assert!(html.contains("users&lt;script&gt;"));
        assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
        // Only our own script and style tags.
        assert_eq!(html.matches("<script").count(), 1);
        assert_eq!(html.matches("</pre>").count(), 1);
    }

    #[test]
    fn links_tables_and_draws_the_diagram() {
        let html = render(&sample(), &Labels::new(&BTreeMap::new()));
        // orders' FK points at users (t2), from the column and the FK table.
        assert!(html.contains("<span class=\"fk\">FK</span> → <a href=\"#t2\">app.users&lt;script&gt;</a>"));
        assert!(html.contains("id=\"t1\"") && html.contains("id=\"t2\"") && html.contains("id=\"o1\""));
        assert!(html.contains("<svg class=\"erd-svg\""));
        assert!(html.contains("<details><summary>Código</summary>"));
        assert!(html.contains("Usada por"));
        assert!(html.contains("<th>Comentario</th>"));
        assert!(html.contains("ix_orders_user") && html.contains("id &gt; 0 AND id &lt; 10"));
    }

    #[test]
    fn big_databases_get_a_diagram_per_schema_or_none() {
        let mut doc = sample();
        let many = |schema: &str, n: usize| -> Vec<TableDoc> {
            (0..n)
                .map(|i| TableDoc { table: TableSchema { kind: "table".into(), schema: Some(schema.into()), name: format!("t{i}"), ..Default::default() }, kind_label: "Tablas".into(), ..Default::default() })
                .collect()
        };
        doc.schemas = vec![
            crate::dbdocs::SchemaDoc { name: Some("a".into()), tables: many("a", 100), groups: vec![] },
            crate::dbdocs::SchemaDoc { name: Some("b".into()), tables: many("b", 200), groups: vec![] },
        ];
        let (sets, skipped) = diagram_sets(&doc);
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].0.as_deref(), Some("a"));
        assert_eq!(skipped, vec![200]);
        let html = render(&doc, &Labels::new(&BTreeMap::new()));
        assert!(html.contains("hay 200 tablas"));
    }
}
