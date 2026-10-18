//! A notebook exported as one self-contained HTML page or as Markdown, with
//! the previews the file would store (same limits, same "don't save result").
//!
//! HTML: every text the user or a server wrote is escaped; Markdown cells go
//! through a small renderer that escapes first and only then adds headings,
//! emphasis, code, lists and http(s) links. No raw HTML gets through.

use super::{
    display_value, escape_html, html_table, preview, summary_line, CellKind, CellResult, Notebook, NotebookCell, NotebookTarget,
    Preview, PreviewLimits, StoredStatement,
};
use serde_json::Value;

const CSS: &str = r#"
:root { --bg: #ffffff; --fg: #1f2328; --muted: #656d76; --border: #d0d7de; --code: #f6f8fa; --err: #cf222e; --head: #f6f8fa; }
@media (prefers-color-scheme: dark) {
  :root { --bg: #0d1117; --fg: #e6edf3; --muted: #8d96a0; --border: #30363d; --code: #161b22; --err: #ff7b72; --head: #161b22; }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--fg); font: 15px/1.55 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; }
main { max-width: 1000px; margin: 0 auto; padding: 24px 16px 64px; }
.cell { margin: 0 0 20px; }
.meta { color: var(--muted); font-size: 12px; margin-bottom: 4px; }
pre { background: var(--code); border: 1px solid var(--border); border-radius: 6px; padding: 10px 12px; overflow-x: auto; margin: 0 0 8px; }
code, pre { font: 13px/1.45 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
p code, li code { background: var(--code); padding: 1px 4px; border-radius: 4px; }
.foreign { opacity: .65; }
.summary { color: var(--muted); font-size: 12px; margin: 4px 0; }
.scroll { overflow-x: auto; margin-bottom: 8px; }
table { border-collapse: collapse; font-size: 13px; }
th, td { border: 1px solid var(--border); padding: 3px 8px; text-align: left; vertical-align: top; white-space: pre-wrap; }
th { background: var(--head); }
td i { color: var(--muted); }
.error { color: var(--err); white-space: pre-wrap; }
.messages { color: var(--muted); }
a { color: inherit; }
"#;

fn target_text(t: &NotebookTarget) -> String {
    [&t.environment, &t.connection, &t.database].iter().filter_map(|v| v.as_deref()).filter(|v| !v.is_empty()).collect::<Vec<_>>().join(" › ")
}

fn cell_label(c: &NotebookCell, nb: &Notebook) -> String {
    let mut parts = Vec::new();
    if let Some(n) = c.execution_count {
        parts.push(format!("[{n}]"));
    }
    let target = c
        .result
        .as_ref()
        .and_then(|r| r.target_label.clone())
        .or_else(|| c.target.as_ref().or(nb.target.as_ref()).map(target_text))
        .filter(|t| !t.is_empty());
    parts.extend(target);
    parts.join(" ")
}

/// What the file would keep of a cell's result, with the user's limits.
fn stored<'a>(c: &'a NotebookCell, nb: &Notebook, limits: PreviewLimits) -> Option<(&'a CellResult, Preview, bool)> {
    let r = c.result.as_ref()?;
    let keep = nb.save_results && c.keep_result;
    Some((r, preview(r, keep, limits, c.execution_count), keep))
}

fn title(nb: &Notebook) -> String {
    nb.cells
        .iter()
        .filter(|c| c.kind == CellKind::Markdown)
        .flat_map(|c| c.source.lines())
        .find_map(|l| l.trim_start().strip_prefix('#').map(|h| h.trim_start_matches('#').trim().to_string()))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "Notebook".into())
}

/// The notebook as one HTML page.
pub fn html(nb: &Notebook, limits: PreviewLimits) -> String {
    let mut body = String::new();
    for c in &nb.cells {
        match c.kind {
            CellKind::Markdown => {
                body.push_str(&format!("<section class=\"cell md\">{}</section>\n", markdown_to_html(&c.source)));
            }
            CellKind::Foreign => {
                let lang = c.language.as_deref().unwrap_or("");
                body.push_str(&format!(
                    "<section class=\"cell foreign\"><div class=\"meta\">{}</div><pre><code>{}</code></pre></section>\n",
                    escape_html(lang),
                    escape_html(&c.source)
                ));
            }
            CellKind::Sql => {
                body.push_str("<section class=\"cell sql\">");
                let label = cell_label(c, nb);
                if !label.is_empty() {
                    body.push_str(&format!("<div class=\"meta\">{}</div>", escape_html(&label)));
                }
                body.push_str(&format!("<pre><code>{}</code></pre>", escape_html(&c.source)));
                if let Some((r, p, keep)) = stored(c, nb, limits) {
                    for s in &p.statements {
                        body.push_str(&format!("<div class=\"summary\">{}</div>", escape_html(&summary_line(s, r))));
                        if keep && !s.columns.is_empty() {
                            body.push_str(&format!("<div class=\"scroll\">{}</div>", html_table(&s.columns, &s.rows)));
                        }
                    }
                    if !p.messages.is_empty() {
                        body.push_str(&format!("<pre class=\"messages\">{}</pre>", escape_html(&p.messages.join("\n"))));
                    }
                    if let Some(e) = &p.error {
                        body.push_str(&format!("<pre class=\"error\">{}</pre>", escape_html(e)));
                    }
                }
                body.push_str("</section>\n");
            }
        }
    }
    format!(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\">\n\
         <meta name=\"generator\" content=\"DBine\">\n<title>{}</title>\n<style>{CSS}</style>\n</head>\n<body>\n<main>\n{body}</main>\n</body>\n</html>\n",
        escape_html(&title(nb))
    )
}

// ---------------------------------------------------------------------------
// Markdown → HTML, safe: escape first, then a few constructs.

fn safe_url(u: &str) -> bool {
    let l = u.to_ascii_lowercase();
    (l.starts_with("http://") || l.starts_with("https://")) && !u.chars().any(|c| c.is_whitespace() || c == '"' || c == '<' || c == '>')
}

/// `**bold**` and `*italic*` on escaped text.
fn emphasis(s: &str) -> String {
    fn wrap(s: &str, marker: &str, tag: &str) -> String {
        let mut out = String::new();
        let mut rest = s;
        while let Some(a) = rest.find(marker) {
            let after = &rest[a + marker.len()..];
            match after.find(marker) {
                Some(b) if b > 0 && !after.starts_with(' ') => {
                    out.push_str(&rest[..a]);
                    out.push_str(&format!("<{tag}>{}</{tag}>", &after[..b]));
                    rest = &after[b + marker.len()..];
                }
                _ => {
                    out.push_str(&rest[..a + marker.len()]);
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }
    wrap(&wrap(s, "**", "strong"), "*", "em")
}

/// Links (`[text](http…)`) and emphasis, on raw text (escaped here).
fn links_and_emphasis(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find("](").map(|i| open + i) else { break };
        let Some(end) = rest[close + 2..].find(')').map(|i| close + 2 + i) else { break };
        let text = &rest[open + 1..close];
        let url = &rest[close + 2..end];
        out.push_str(&emphasis(&escape_html(&rest[..open])));
        if safe_url(url) && !text.contains('[') {
            out.push_str(&format!(
                "<a href=\"{}\" rel=\"noopener noreferrer\">{}</a>",
                escape_html(url),
                emphasis(&escape_html(text))
            ));
        } else {
            out.push_str(&emphasis(&escape_html(&rest[open..=end])));
        }
        rest = &rest[end + 1..];
    }
    out.push_str(&emphasis(&escape_html(rest)));
    out
}

/// Inline: code spans first (nothing inside them is processed).
fn inline(s: &str) -> String {
    let mut segs: Vec<String> = s.split('`').map(String::from).collect();
    // An odd number of backticks: the last one is literal.
    if segs.len().is_multiple_of(2) {
        let last = segs.pop().unwrap_or_default();
        if let Some(prev) = segs.last_mut() {
            prev.push('`');
            prev.push_str(&last);
        }
    }
    segs.iter()
        .enumerate()
        .map(|(i, seg)| if i % 2 == 1 { format!("<code>{}</code>", escape_html(seg)) } else { links_and_emphasis(seg) })
        .collect()
}

enum Block {
    None,
    Para(Vec<String>),
    List(bool, Vec<String>),
}

fn flush(out: &mut String, b: &mut Block) {
    match std::mem::replace(b, Block::None) {
        Block::None => {}
        Block::Para(lines) => out.push_str(&format!("<p>{}</p>\n", lines.iter().map(|l| inline(l)).collect::<Vec<_>>().join("<br>\n"))),
        Block::List(ordered, items) => {
            let tag = if ordered { "ol" } else { "ul" };
            out.push_str(&format!("<{tag}>"));
            for it in items {
                out.push_str(&format!("<li>{}</li>", inline(&it)));
            }
            out.push_str(&format!("</{tag}>\n"));
        }
    }
}

fn list_item(line: &str) -> Option<(bool, &str)> {
    let t = line.trim_start();
    for m in ["- ", "* ", "+ "] {
        if let Some(r) = t.strip_prefix(m) {
            return Some((false, r));
        }
    }
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 {
        if let Some(r) = t[digits..].strip_prefix(". ").or_else(|| t[digits..].strip_prefix(") ")) {
            return Some((true, r));
        }
    }
    None
}

/// A Markdown cell as HTML (a safe subset; raw HTML is shown as text).
pub fn markdown_to_html(src: &str) -> String {
    let mut out = String::new();
    let mut block = Block::None;
    let mut fence: Option<(String, Vec<String>)> = None;
    for line in src.lines() {
        if let Some((marker, body)) = fence.as_mut() {
            if line.trim_start().starts_with(marker.as_str()) && line.trim().chars().all(|c| c == '`' || c == '~') {
                out.push_str(&format!("<pre><code>{}</code></pre>\n", escape_html(&body.join("\n"))));
                fence = None;
            } else {
                body.push(line.to_string());
            }
            continue;
        }
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            flush(&mut out, &mut block);
            let ch = t.chars().next().unwrap_or('`');
            let n = t.chars().take_while(|c| *c == ch).count();
            fence = Some((ch.to_string().repeat(n), Vec::new()));
            continue;
        }
        if t.is_empty() {
            flush(&mut out, &mut block);
            continue;
        }
        let hashes = t.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            flush(&mut out, &mut block);
            out.push_str(&format!("<h{hashes}>{}</h{hashes}>\n", inline(t[hashes..].trim().trim_end_matches('#').trim_end())));
            continue;
        }
        if let Some((ordered, item)) = list_item(line) {
            match &mut block {
                Block::List(o, items) if *o == ordered => items.push(item.to_string()),
                _ => {
                    flush(&mut out, &mut block);
                    block = Block::List(ordered, vec![item.to_string()]);
                }
            }
            continue;
        }
        match &mut block {
            Block::Para(lines) => lines.push(t.to_string()),
            Block::List(_, items) if line.starts_with("  ") => {
                if let Some(last) = items.last_mut() {
                    last.push(' ');
                    last.push_str(t);
                }
            }
            _ => {
                flush(&mut out, &mut block);
                block = Block::Para(vec![t.to_string()]);
            }
        }
    }
    if let Some((_, body)) = fence {
        out.push_str(&format!("<pre><code>{}</code></pre>\n", escape_html(&body.join("\n"))));
    }
    flush(&mut out, &mut block);
    out
}

// ---------------------------------------------------------------------------
// Markdown

/// A fence longer than any run of backticks inside.
fn fenced(code: &str, lang: &str) -> String {
    let mut run = 0;
    let mut max = 0;
    for c in code.chars() {
        if c == '`' {
            run += 1;
            max = max.max(run);
        } else {
            run = 0;
        }
    }
    let f = "`".repeat((max + 1).max(3));
    format!("{f}{lang}\n{}\n{f}\n\n", code.trim_end_matches('\n'))
}

/// Text that Markdown shows as it is (no emphasis, links or HTML).
fn md_text(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\\' | '*' | '_' | '[' | ']' | '`' | '|' | '#' => {
                out.push('\\');
                out.push(c);
            }
            '\r' | '\n' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn md_table(s: &StoredStatement) -> String {
    let mut t = String::new();
    let head: Vec<String> = s.columns.iter().map(|c| md_text(&c.name)).collect();
    t.push_str(&format!("| {} |\n", head.join(" | ")));
    t.push_str(&format!("|{}|\n", vec![" --- "; head.len()].join("|")));
    for r in &s.rows {
        let cells: Vec<String> = (0..s.columns.len()).map(|i| md_text(&display_value(r.get(i).unwrap_or(&Value::Null)))).collect();
        t.push_str(&format!("| {} |\n", cells.join(" | ")));
    }
    t.push('\n');
    t
}

/// The notebook as Markdown.
pub fn markdown(nb: &Notebook, limits: PreviewLimits) -> String {
    let mut out = String::new();
    for c in &nb.cells {
        match c.kind {
            CellKind::Markdown => {
                out.push_str(c.source.trim_end());
                out.push_str("\n\n");
            }
            CellKind::Foreign => out.push_str(&fenced(&c.source, c.language.as_deref().unwrap_or(""))),
            CellKind::Sql => {
                let label = cell_label(c, nb);
                if !label.is_empty() {
                    out.push_str(&format!("{}\n\n", md_text(&label)));
                }
                out.push_str(&fenced(&c.source, c.language.as_deref().unwrap_or("sql")));
                if let Some((r, p, keep)) = stored(c, nb, limits) {
                    for s in &p.statements {
                        out.push_str(&format!("*{}*\n\n", md_text(&summary_line(s, r))));
                        if keep && !s.columns.is_empty() {
                            out.push_str(&md_table(s));
                        }
                    }
                    if !p.messages.is_empty() {
                        out.push_str(&fenced(&p.messages.join("\n"), "text"));
                    }
                    if let Some(e) = &p.error {
                        out.push_str("**Error**\n\n");
                        out.push_str(&fenced(e, "text"));
                    }
                }
            }
        }
    }
    out.trim_end().to_string() + "\n"
}

#[cfg(test)]
mod tests {
    use super::super::{parse, StoredColumn};
    use super::*;
    use serde_json::json;

    fn nb_with_script() -> Notebook {
        let r = CellResult {
            results: vec![StoredStatement {
                columns: vec![StoredColumn { name: "<b>col</b>".into(), type_name: String::new() }],
                rows: vec![vec![json!("<script>alert(1)</script>")], vec![Value::Null]],
                rows_total: 2,
                rows_kept: 2,
                truncated: false,
                rows_affected: None,
                statement: None,
            }],
            messages: vec!["<script>msg</script>".into()],
            error: Some("<script>err</script>".into()),
            ran_at: "2026-10-18T14:32:05Z".into(),
            duration_ms: 41_000,
            engine: Some("postgres".into()),
            target_label: Some("<script>t</script>".into()),
            truncated: false,
        };
        let mut nb = parse(r#"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[]}"#).unwrap();
        nb.cells = vec![
            NotebookCell {
                id: "m".into(),
                kind: CellKind::Markdown,
                source: "# Hola <script>x</script>\n\n**negrita** y *it* y `<script>c</script>`\n\n- a\n- [link](https://dbine.com/?a=1&b=2)\n- [mal](javascript:alert(1))\n\n<script>alert('md')</script>\n\n```\n<script>f</script>\n```".into(),
                language: None,
                target: None,
                keep_result: true,
                chart: None,
                execution_count: None,
                result: None,
                raw: None,
            },
            NotebookCell {
                id: "c".into(),
                kind: CellKind::Sql,
                source: "SELECT '<script>s</script>'".into(),
                language: Some("sql".into()),
                target: None,
                keep_result: true,
                chart: None,
                execution_count: Some(1),
                result: Some(r),
                raw: None,
            },
        ];
        nb
    }

    #[test]
    fn html_export_escapes_everything() {
        let h = html(&nb_with_script(), PreviewLimits::default());
        assert!(!h.contains("<script"), "{h}");
        assert!(!h.contains("<b>col"));
        assert!(!h.contains("javascript:alert(1)\""));
        assert!(h.contains("<title>Hola &lt;script&gt;x&lt;/script&gt;</title>"));
        assert!(h.contains("<strong>negrita</strong> y <em>it</em> y <code>&lt;script&gt;c&lt;/script&gt;</code>"));
        assert!(h.contains("<a href=\"https://dbine.com/?a=1&amp;b=2\" rel=\"noopener noreferrer\">link</a>"));
        assert!(h.contains("<ul><li>a</li>"));
        assert!(h.contains("<pre><code>&lt;script&gt;f&lt;/script&gt;</code></pre>"));
        assert!(h.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(h.contains("<i>NULL</i>"));
        assert!(h.contains("2 / 2 rows · 2026-10-18T14:32:05Z · 41.0 s · &lt;script&gt;t&lt;/script&gt;"));
        assert!(h.contains("prefers-color-scheme: dark"));
    }

    #[test]
    fn exports_use_the_given_limits() {
        let mut nb = nb_with_script();
        let r = nb.cells[1].result.as_mut().unwrap();
        r.results[0].rows = (0..20).map(|i| vec![json!(format!("fila {i}"))]).collect();
        r.results[0].rows_total = 20;
        let one = PreviewLimits::new(Some(1), None);
        for out in [html(&nb, one), markdown(&nb, one)] {
            assert!(out.contains("fila 0") && !out.contains("fila 1"), "{out}");
        }
        assert!(html(&nb, PreviewLimits::default()).contains("fila 19"));
        // A cell's language is its fence's.
        nb.cells[1].language = Some("redis".into());
        assert!(markdown(&nb, one).contains("```redis\n"));
    }

    #[test]
    fn markdown_links_are_only_http() {
        assert_eq!(markdown_to_html("[a](https://x.y/z)"), "<p><a href=\"https://x.y/z\" rel=\"noopener noreferrer\">a</a></p>\n");
        assert_eq!(markdown_to_html("[a](javascript:alert(1))"), "<p>[a](javascript:alert(1))</p>\n");
        assert_eq!(markdown_to_html("1. uno\n2. dos"), "<ol><li>uno</li><li>dos</li></ol>\n");
        assert_eq!(markdown_to_html("## T\nx `a*b*` *c*"), "<h2>T</h2>\n<p>x <code>a*b*</code> <em>c</em></p>\n");
    }

    #[test]
    fn markdown_export_has_fences_and_tables() {
        let md = markdown(&nb_with_script(), PreviewLimits::default());
        assert!(md.starts_with("# Hola <script>x</script>"), "markdown cells go as they are");
        assert!(md.contains("```sql\nSELECT '<script>s</script>'\n```"));
        assert!(md.contains("| &lt;b&gt;col&lt;/b&gt; |"));
        assert!(md.contains("| &lt;script&gt;alert(1)&lt;/script&gt; |"));
        assert!(md.contains("*2 / 2 rows · 2026-10-18T14:32:05Z · 41.0 s · &lt;script&gt;t&lt;/script&gt;*"));
        assert!(md.contains("**Error**\n\n```text\n<script>err</script>\n```"));
        // Rows not kept: no table.
        let mut nb = nb_with_script();
        nb.cells[1].keep_result = false;
        let md = markdown(&nb, PreviewLimits::default());
        assert!(!md.contains("alert(1)&lt;") && md.contains("*0 / 2 rows"));
    }
}
