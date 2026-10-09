//! The Markdown document: headings, tables and fenced code, for a
//! repository or a wiki. Anchors are explicit (`<a id="t1"></a>`) so links
//! work whatever the renderer does with headings. Names and comments are
//! escaped ([`md`]), code goes in a fence longer than any it contains.

use super::html::esc;
use super::{key_marks, qualified, Anchors, Doc, Labels, ObjectDoc, TableDoc};
use dbine_driver::{ColumnDef, TableSchema};
use std::fmt::Write;

/// Inline text: Markdown's punctuation backslash-escaped, `<`, `>` and `&`
/// as entities (no HTML gets through), line breaks as spaces.
pub fn md(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '#' | '|' | '!' | '~' | '{' | '}' | '+' | '-' | '.' => {
                out.push('\\');
                out.push(c);
            }
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\r' => {}
            c if c.is_control() => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// A name as an inline code span inside a table cell: on one line (no line
/// break or control character can end the cell or the span), no backtick to
/// close it early, `|` escaped for the table, and `<`, `>`, `&` as entities
/// like [`md`] (no HTML gets through, whatever the renderer does with spans).
fn code(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('`');
    for c in s.chars() {
        match c {
            '`' => out.push('\''),
            '|' => out.push_str("\\|"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\r' => {}
            c if c.is_control() => out.push(' '),
            _ => out.push(c),
        }
    }
    out.push('`');
    out
}

/// A code fence that `text` can't close.
fn fence(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat(longest.max(2) + 1)
}

fn row(out: &mut String, cells: &[String]) {
    out.push('|');
    for c in cells {
        let _ = write!(out, " {} |", if c.is_empty() { " " } else { c });
    }
    out.push('\n');
}

fn header(out: &mut String, l: &Labels, keys: &[&str]) {
    row(out, &keys.iter().map(|k| md(l.get(k))).collect::<Vec<_>>());
    out.push('|');
    for _ in keys {
        out.push_str(" --- |");
    }
    out.push('\n');
}

fn link(anchors: &Anchors, schema: Option<&str>, default_schema: Option<&str>, name: &str) -> String {
    let text = md(&qualified(schema, name));
    match anchors.table(schema, default_schema, name) {
        Some(id) => format!("[{text}](#{id})"),
        None => text,
    }
}

fn yes_no(l: &Labels, v: bool) -> String {
    md(l.get(if v { "yes" } else { "no" }))
}

pub fn render(doc: &Doc, l: &Labels) -> String {
    let anchors = Anchors::new(doc);
    let mut out = String::with_capacity(32 * 1024);
    let _ = writeln!(out, "# {} · {}\n", md(l.get("title")), md(&doc.database));
    let engine = match &doc.version {
        Some(v) => format!("{} · {}", doc.engine, v),
        None => doc.engine.clone(),
    };
    for (k, v) in [("database", &doc.database), ("connection", &doc.connection), ("engine", &engine), ("generated", &doc.generated_at)] {
        if !v.is_empty() {
            let _ = writeln!(out, "- **{}:** {}", md(l.get(k)), md(v));
        }
    }
    out.push('\n');
    if !doc.notes.is_empty() {
        let _ = writeln!(out, "> **{}**", md(l.get("notes")));
        for n in &doc.notes {
            let _ = writeln!(out, "> - {}", md(n));
        }
        out.push('\n');
    }
    let schema_title = |name: &Option<String>| match name {
        Some(n) => format!("{} {}", l.get("schema"), n),
        None => l.get("noSchema").to_string(),
    };

    // Contents.
    let _ = writeln!(out, "## {}\n", md(l.get("contents")));
    let mut n = 0;
    for (si, s) in doc.schemas.iter().enumerate() {
        let _ = writeln!(out, "- [{}](#s{})", md(&schema_title(&s.name)), si + 1);
        for t in &s.tables {
            let id = anchors.table(t.table.schema.as_deref(), None, &t.table.name).unwrap_or("");
            let _ = writeln!(out, "  - [{}](#{id})", md(&t.table.name));
        }
        for g in &s.groups {
            for o in &g.items {
                n += 1;
                let _ = writeln!(out, "  - [{}](#o{n}) · {}", md(&o.name), md(&g.label));
            }
        }
    }
    out.push('\n');

    let mut n = 0;
    for (si, s) in doc.schemas.iter().enumerate() {
        let _ = writeln!(out, "<a id=\"s{}\"></a>\n\n## {}\n", si + 1, md(&schema_title(&s.name)));
        let mut last_kind = "";
        for t in &s.tables {
            if t.kind_label != last_kind {
                let _ = writeln!(out, "### {}\n", md(&t.kind_label));
                last_kind = &t.kind_label;
            }
            table(&mut out, doc, t, &anchors, l);
        }
        for g in &s.groups {
            let _ = writeln!(out, "### {}\n", md(&g.label));
            for o in &g.items {
                n += 1;
                object(&mut out, doc, o, &format!("o{n}"), l);
            }
        }
    }
    out
}

fn columns(out: &mut String, cols: &[ColumnDef], t: Option<&TableSchema>, anchors: &Anchors, l: &Labels) {
    if cols.is_empty() {
        let _ = writeln!(out, "_{}_\n", md(l.get("noColumns")));
        return;
    }
    let with_comment = cols.iter().any(|c| c.comment.as_deref().is_some_and(|x| !x.trim().is_empty()));
    let mut keys = vec!["column", "type", "nullable", "default"];
    if t.is_some() {
        keys.push("key");
    }
    if with_comment {
        keys.push("comment");
    }
    header(out, l, &keys);
    for c in cols {
        let mut ty = md(&c.data_type);
        if c.auto_increment {
            let _ = write!(ty, " ({})", md(l.get("autoIncrement")));
        }
        let mut cells = vec![code(&c.name), ty, yes_no(l, c.nullable), md(c.default_value.as_deref().unwrap_or(""))];
        if let Some(t) = t {
            let mut cell = String::new();
            if key_marks(t, &c.name).starts_with("PK") {
                cell.push_str("**PK**");
            }
            for fk in t.foreign_keys.iter().filter(|f| f.columns.iter().any(|x| x.eq_ignore_ascii_case(&c.name))) {
                if !cell.is_empty() {
                    cell.push(' ');
                }
                let _ = write!(cell, "**FK** → {}", link(anchors, fk.ref_schema.as_deref(), t.schema.as_deref(), &fk.ref_table));
            }
            cells.push(cell);
        }
        if with_comment {
            cells.push(md(c.comment.as_deref().unwrap_or("")));
        }
        row(out, &cells);
    }
    out.push('\n');
}

fn table(out: &mut String, doc: &Doc, td: &TableDoc, anchors: &Anchors, l: &Labels) {
    let t = &td.table;
    let id = anchors.table(t.schema.as_deref(), None, &t.name).unwrap_or("");
    let _ = writeln!(out, "<a id=\"{}\"></a>\n\n#### {}\n", esc(id), md(&qualified(t.schema.as_deref(), &t.name)));
    if let Some(c) = t.comment.as_deref().filter(|c| !c.trim().is_empty()) {
        let _ = writeln!(out, "{}\n", md(c));
    }
    if let Some(rows) = td.rows {
        let _ = writeln!(out, "{}: {}\n", md(l.get("rows")), super::thousands(rows));
    }
    columns(out, &t.columns, Some(t), anchors, l);
    if let Some(pk) = &t.primary_key {
        let name = pk.name.as_deref().map(|n| format!("{} ", md(n))).unwrap_or_default();
        let _ = writeln!(out, "**{}:** {name}({})\n", md(l.get("primaryKey")), md(&pk.columns.join(", ")));
    }
    if doc.foreign_keys && !t.foreign_keys.is_empty() {
        let _ = writeln!(out, "**{}**\n", md(l.get("foreignKeys")));
        header(out, l, &["name", "columns", "references", "onDelete", "onUpdate"]);
        for fk in &t.foreign_keys {
            row(
                out,
                &[
                    md(fk.name.as_deref().unwrap_or("")),
                    md(&fk.columns.join(", ")),
                    format!("{} ({})", link(anchors, fk.ref_schema.as_deref(), t.schema.as_deref(), &fk.ref_table), md(&fk.ref_columns.join(", "))),
                    md(fk.on_delete.as_deref().unwrap_or("")),
                    md(fk.on_update.as_deref().unwrap_or("")),
                ],
            );
        }
        out.push('\n');
    }
    if doc.indexes && !t.indexes.is_empty() {
        let _ = writeln!(out, "**{}**\n", md(l.get("indexes")));
        header(out, l, &["name", "columns", "unique", "type", "included", "filter"]);
        for ix in &t.indexes {
            row(
                out,
                &[
                    md(&ix.name),
                    md(&ix.columns.join(", ")),
                    yes_no(l, ix.unique),
                    md(ix.kind.as_deref().unwrap_or("")),
                    md(&ix.include.join(", ")),
                    md(ix.filter.as_deref().unwrap_or("")),
                ],
            );
        }
        out.push('\n');
    }
    if !t.checks.is_empty() {
        let _ = writeln!(out, "**{}**\n", md(l.get("checks")));
        header(out, l, &["name", "condition"]);
        for ck in &t.checks {
            row(out, &[md(ck.name.as_deref().unwrap_or("")), md(&ck.expression)]);
        }
        out.push('\n');
    }
    if !t.options.is_empty() {
        let _ = writeln!(out, "**{}**\n", md(l.get("options")));
        for (k, v) in &t.options {
            let _ = writeln!(out, "- {}: {}", md(k), md(v));
        }
        out.push('\n');
    }
    if !td.triggers.is_empty() {
        let _ = writeln!(out, "**{}:** {}\n", md(l.get("triggers")), md(&td.triggers.join(", ")));
    }
    if doc.dependencies && !td.used_by.is_empty() {
        let _ = writeln!(out, "**{}**\n", md(l.get("usedBy")));
        for u in &td.used_by {
            let name = if u.table { link(anchors, u.schema.as_deref(), None, &u.name) } else { md(&qualified(u.schema.as_deref(), &u.name)) };
            let _ = writeln!(out, "- {name} · {} · {}", md(&u.kind_label), md(&u.how));
        }
        out.push('\n');
    }
}

fn object(out: &mut String, doc: &Doc, o: &ObjectDoc, id: &str, l: &Labels) {
    let _ = writeln!(out, "<a id=\"{id}\"></a>\n\n#### {}\n", md(&qualified(o.schema.as_deref(), &o.name)));
    if let Some(p) = &o.parent {
        let _ = writeln!(out, "{}: {}\n", md(l.get("parent")), md(p));
    }
    if let Some(c) = o.comment.as_deref() {
        let _ = writeln!(out, "{}\n", md(c));
    }
    if !o.columns.is_empty() {
        columns(out, &o.columns, None, &Anchors { tables: Default::default() }, l);
    }
    if let Some(src) = o.source.as_deref().filter(|_| doc.source) {
        let f = fence(src);
        let _ = writeln!(out, "{f}sql\n{}\n{f}\n", src.trim_end());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbdocs::tests::sample;
    use std::collections::BTreeMap;

    #[test]
    fn escapes_inline_text() {
        assert_eq!(md("a|b *c* <x> [l](u)\nz"), r"a\|b \*c\* &lt;x&gt; \[l\]\(u\) z");
        assert_eq!(md("a\tb\u{0b}c\u{1b}d"), "a b c d");
        assert_eq!(fence("no ticks"), "```");
        assert_eq!(fence("a ``` b ```` c"), "`````");
    }

    #[test]
    fn renders_tables_links_and_code() {
        let text = render(&sample(), &Labels::new(&BTreeMap::new()));
        assert!(!text.contains("<script>") && !text.contains("<img") && !text.contains("<b>"));
        assert!(text.contains("**FK** → [app\\.users&lt;script&gt;](#t2)"), "{text}");
        assert!(!text.contains("`name|x`") && text.contains("`name\\|x`"));
        // The view's code can't close its fence.
        assert!(text.contains("````sql\nSELECT * FROM orders WHERE x < 1 -- ``` </pre>\n````"));
        assert!(text.contains("<a id=\"t1\"></a>") && text.contains("<a id=\"o1\"></a>"));
    }

    #[test]
    fn column_names_stay_inside_their_cell() {
        assert_eq!(code("a`b|c"), r"`a'b\|c`");
        assert_eq!(code("x\r\n<script>alert(1)</script>&"), "`x &lt;script&gt;alert(1)&lt;/script&gt;&amp;`");
        let mut doc = sample();
        let t = &mut doc.schemas[0].tables[0].table;
        t.columns[0].name = "id\n\n<script>alert(1)</script>\r\n| x | y |".into();
        let text = render(&doc, &Labels::new(&BTreeMap::new()));
        assert!(!text.contains("<script>"), "{text}");
        let line = text.lines().find(|l| l.contains("alert(1)")).unwrap();
        assert!(line.starts_with("| `id  &lt;script&gt;alert(1)&lt;/script&gt; \\| x \\| y \\|` |"), "{line}");
    }
}
