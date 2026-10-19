//! A notebook exported as one self-contained HTML page or as Markdown, with
//! the previews the file would store (same limits, same "don't save result").
//!
//! HTML: every text the user or a server wrote is escaped; Markdown cells go
//! through a small renderer that escapes first and only then adds headings,
//! emphasis, code, lists and http(s) links. No raw HTML gets through.
//!
//! Charts: the UI renders each cell's charts to PNG and passes them in; they
//! are checked to be PNG and re-encoded here. HTML embeds them as data: URIs;
//! Markdown references them in a `<name>_files/` folder next to the file,
//! with names DBine makes (`chart-<cell>-<n>.png`). A cell whose result isn't
//! kept ("No guardar el resultado", or a notebook that saves no results)
//! exports no charts either: they show its data.

use super::{
    display_value, escape_html, html_table, preview, summary_line, CellKind, CellResult, Notebook, NotebookCell, NotebookTarget,
    Preview, PreviewLimits, StoredStatement, write_cell_text, write_summary_line,
};
use base64::Engine;
use serde_json::Value;
use std::collections::HashMap;

const CSS: &str = r#"
:root { --bg: #ffffff; --fg: #1f2328; --muted: #656d76; --border: #d0d7de; --code: #f6f8fa; --err: #cf222e; --head: #f6f8fa; }
@media (prefers-color-scheme: dark) {
  :root { --bg: #0d1117; --fg: #e6edf3; --muted: #8d96a0; --border: #30363d; --code: #161b22; --err: #ff7b72; --head: #161b22; }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--fg); font: 15px/1.55 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "PingFang SC", "Hiragino Sans", "Hiragino Kaku Gothic ProN", "Apple SD Gothic Neo", "Microsoft YaHei", "Yu Gothic UI", "Meiryo", "Malgun Gothic", "Noto Sans CJK SC", "Noto Sans CJK JP", "Noto Sans CJK KR", sans-serif; }
main { max-width: 1000px; margin: 0 auto; padding: 24px 16px 64px; }
.cell { margin: 0 0 20px; }
.meta { color: var(--muted); font-size: 12px; margin-bottom: 4px; }
pre { background: var(--code); border: 1px solid var(--border); border-radius: 6px; padding: 10px 12px; overflow-x: auto; margin: 0 0 8px; }
code, pre { font: 13px/1.45 ui-monospace, SFMono-Regular, Menlo, Consolas, "PingFang SC", "Hiragino Sans", "Hiragino Kaku Gothic ProN", "Apple SD Gothic Neo", "Microsoft YaHei", "Yu Gothic UI", "Meiryo", "Malgun Gothic", "Noto Sans CJK SC", "Noto Sans CJK JP", "Noto Sans CJK KR", monospace; }
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
figure.chart { margin: 4px 0 12px; }
figure.chart img { display: block; max-width: 100%; height: auto; border: 1px solid var(--border); border-radius: 6px; background: #fff; }
figure.chart figcaption { color: var(--muted); font-size: 12px; margin-top: 4px; }
a { color: inherit; }
"#;

/// A chart rendered by the UI: its title and PNG bytes (checked).
#[derive(Debug, Clone)]
pub struct ChartImage {
    pub title: String,
    pub png: Vec<u8>,
}

/// Each cell's charts by cell id, in the order they show.
pub type CellImages = HashMap<String, Vec<ChartImage>>;

/// One chart image at most this big, and at most this many per cell.
pub const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_IMAGES_PER_CELL: usize = 20;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// A chart image from the UI (base64, with or without a `data:image/png;base64,`
/// prefix): the bytes when they're a PNG within the size cap.
pub fn decode_png(b64: &str) -> Result<Vec<u8>, String> {
    let raw = b64.strip_prefix("data:image/png;base64,").unwrap_or(b64);
    if raw.len() > MAX_IMAGE_BYTES / 3 * 4 + 4 {
        return Err("la imagen del gráfico es demasiado grande".into());
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(raw.trim()).map_err(|e| format!("imagen de gráfico inválida: {e}"))?;
    if !bytes.starts_with(PNG_SIGNATURE) || bytes.len() > MAX_IMAGE_BYTES {
        return Err("la imagen del gráfico no es un PNG válido".into());
    }
    Ok(bytes)
}

/// The folder a Markdown export's images go in, next to it: `<stem>_files`.
pub fn files_dir_name(md_file_name: &str) -> String {
    let stem = md_file_name.rsplit_once('.').map(|(s, _)| s).filter(|s| !s.is_empty()).unwrap_or(md_file_name);
    format!("{stem}_files")
}

/// A path segment for a Markdown link: everything but unreserved characters
/// percent-encoded (spaces, parentheses, brackets…).
fn url_segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The images a cell may export: none when its result isn't kept.
fn cell_images<'a>(c: &NotebookCell, nb: &Notebook, images: &'a CellImages) -> &'a [ChartImage] {
    if c.kind != CellKind::Sql || !(nb.save_results && c.keep_result) {
        return &[];
    }
    images.get(&c.id).map(|v| &v[..v.len().min(MAX_IMAGES_PER_CELL)]).unwrap_or(&[])
}

/// A Markdown export's files: (name inside the `_files` folder, PNG bytes).
pub type ExportFiles = Vec<(String, Vec<u8>)>;

fn target_text(t: &NotebookTarget) -> String {
    if t.results {
        return "DataFrame SQL".into();
    }
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

/// The notebook as one HTML page (without charts).
#[cfg(test)]
pub fn html(nb: &Notebook, limits: PreviewLimits) -> String {
    html_with(nb, limits, &CellImages::new())
}

/// The notebook as one HTML page, each cell's charts embedded (data: URIs).
pub fn html_with(nb: &Notebook, limits: PreviewLimits, images: &CellImages) -> String {
    html_with_note(nb, limits, images, None)
}

/// A cell with a chart but no image for it (a scheduled run renders no
/// charts): `chart_note` under its result, when given.
fn chart_missing<'a>(c: &NotebookCell, nb: &Notebook, images: &CellImages, chart_note: Option<&'a str>) -> Option<&'a str> {
    let kept = nb.save_results && c.keep_result;
    (kept && c.chart.is_some() && c.result.is_some() && cell_images(c, nb, images).is_empty()).then_some(chart_note).flatten()
}

/// [`html_with`], with `chart_note` where a chart has no image.
pub fn html_with_note(nb: &Notebook, limits: PreviewLimits, images: &CellImages, chart_note: Option<&str>) -> String {
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
            CellKind::Write => {
                body.push_str("<section class=\"cell write\">");
                body.push_str(&format!("<pre><code>{}</code></pre>", escape_html(&write_cell_text(&c.write.clone().unwrap_or_default()))));
                if let Some(r) = &c.write_result {
                    let class = if r.error.is_some() { "error" } else { "summary" };
                    body.push_str(&format!("<div class=\"{class}\">{}</div>", escape_html(&write_summary_line(r))));
                }
                body.push_str("</section>\n");
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
                if let Some(note) = chart_missing(c, nb, images, chart_note) {
                    body.push_str(&format!("<p class=\"summary\">{}</p>", escape_html(note)));
                }
                for img in cell_images(c, nb, images) {
                    let data = base64::engine::general_purpose::STANDARD.encode(&img.png);
                    let caption = if img.title.is_empty() { String::new() } else { format!("<figcaption>{}</figcaption>", escape_html(&img.title)) };
                    body.push_str(&format!(
                        "<figure class=\"chart\"><img alt=\"{}\" src=\"data:image/png;base64,{data}\">{caption}</figure>",
                        escape_html(&img.title)
                    ));
                }
                body.push_str("</section>\n");
            }
        }
    }
    format!(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src data:\">\n\
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
    markdown_with(nb, limits, &CellImages::new(), "").0
}

/// The notebook as Markdown, each cell's charts as `![title](<dir>/chart-<cell>-<n>.png)`;
/// with the files to write in `dir` (a folder next to the .md file).
pub fn markdown_with(nb: &Notebook, limits: PreviewLimits, images: &CellImages, dir: &str) -> (String, ExportFiles) {
    markdown_with_note(nb, limits, images, dir, None)
}

/// [`markdown_with`], with `chart_note` where a chart has no image.
pub fn markdown_with_note(nb: &Notebook, limits: PreviewLimits, images: &CellImages, dir: &str, chart_note: Option<&str>) -> (String, ExportFiles) {
    let mut out = String::new();
    let mut files = ExportFiles::new();
    for (ci, c) in nb.cells.iter().enumerate() {
        match c.kind {
            CellKind::Markdown => {
                out.push_str(c.source.trim_end());
                out.push_str("\n\n");
            }
            CellKind::Foreign => out.push_str(&fenced(&c.source, c.language.as_deref().unwrap_or(""))),
            CellKind::Write => {
                out.push_str(&fenced(&write_cell_text(&c.write.clone().unwrap_or_default()), "text"));
                if let Some(r) = &c.write_result {
                    out.push_str(&format!("*{}*\n\n", md_text(&write_summary_line(r))));
                }
            }
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
                if let Some(note) = chart_missing(c, nb, images, chart_note) {
                    out.push_str(&format!("*{}*\n\n", md_text(note)));
                }
                for (k, img) in cell_images(c, nb, images).iter().enumerate() {
                    let name = format!("chart-{}-{}.png", ci + 1, k + 1);
                    out.push_str(&format!("![{}]({}/{name})\n\n", md_text(&img.title), url_segment(dir)));
                    files.push((name, img.png.clone()));
                }
            }
        }
    }
    (out.trim_end().to_string() + "\n", files)
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
            params: None,
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
                name: None,
                write: None,
                write_result: None,
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
                name: None,
                write: None,
                write_result: None,
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

    const PNG_1PX: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    fn images(cell: &str, titles: &[&str]) -> CellImages {
        let png = decode_png(PNG_1PX).unwrap();
        CellImages::from([(cell.to_string(), titles.iter().map(|t| ChartImage { title: t.to_string(), png: png.clone() }).collect())])
    }

    #[test]
    fn chart_images_must_be_png() {
        let png = decode_png(PNG_1PX).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        assert_eq!(decode_png(&format!("data:image/png;base64,{PNG_1PX}")).unwrap(), png);
        // Not base64, or not a PNG (an SVG, HTML…): refused.
        assert!(decode_png("%%%").is_err());
        let svg = base64::engine::general_purpose::STANDARD.encode("<svg onload=alert(1)>");
        assert!(decode_png(&svg).is_err());
        assert!(decode_png(&format!("data:image/svg+xml;base64,{svg}")).is_err());
    }

    #[test]
    fn html_embeds_each_cells_charts() {
        let nb = nb_with_script();
        let h = html_with(&nb, PreviewLimits::default(), &images("c", &["Ventas <b>2026</b>", ""]));
        assert_eq!(h.matches("<img ").count(), 2);
        assert!(h.contains(&format!("src=\"data:image/png;base64,{PNG_1PX}\"")));
        assert!(h.contains("<figcaption>Ventas &lt;b&gt;2026&lt;/b&gt;</figcaption>"));
        assert!(!h.contains("<b>2026"));
        // The page's policy lets data: images in (and nothing else).
        assert!(h.contains("default-src 'none'; style-src 'unsafe-inline'; img-src data:"));
        // Images of another (or no) cell: ignored. A cell whose result isn't kept: no charts.
        assert!(!html_with(&nb, PreviewLimits::default(), &images("m", &["x"])).contains("<img"));
        let mut dropped = nb_with_script();
        dropped.cells[1].keep_result = false;
        assert!(!html_with(&dropped, PreviewLimits::default(), &images("c", &["x"])).contains("<img"));
        let mut none = nb_with_script();
        none.save_results = false;
        assert!(!html_with(&none, PreviewLimits::default(), &images("c", &["x"])).contains("<img"));
    }

    #[test]
    fn markdown_links_charts_in_the_files_folder() {
        let nb = nb_with_script();
        let dir = files_dir_name("Informe (final) v2.md");
        assert_eq!(dir, "Informe (final) v2_files");
        assert_eq!(files_dir_name("notas"), "notas_files");
        assert_eq!(files_dir_name(".md"), ".md_files");
        let (md, files) = markdown_with(&nb, PreviewLimits::default(), &images("c", &["Por [mes]", "otro"]), &dir);
        // The SQL cell is the second one: chart-2-<n>.png, linked relatively.
        assert!(md.contains("![Por \\[mes\\]](Informe%20%28final%29%20v2_files/chart-2-1.png)"), "{md}");
        assert!(md.contains("![otro](Informe%20%28final%29%20v2_files/chart-2-2.png)"));
        assert_eq!(files.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["chart-2-1.png", "chart-2-2.png"]);
        assert!(files.iter().all(|(n, b)| !n.contains('/') && !n.contains("..") && b.starts_with(b"\x89PNG")));
        // Without images: exactly the plain export.
        assert_eq!(markdown_with(&nb, PreviewLimits::default(), &CellImages::new(), &dir).0, markdown(&nb, PreviewLimits::default()));
        // At most MAX_IMAGES_PER_CELL per cell.
        let many: Vec<String> = (0..MAX_IMAGES_PER_CELL + 5).map(|i| i.to_string()).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(markdown_with(&nb, PreviewLimits::default(), &images("c", &refs), &dir).1.len(), MAX_IMAGES_PER_CELL);
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
