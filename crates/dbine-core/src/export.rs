//! Result exports: JSON, JSON Lines, SQL INSERTs, CSV (comma, semicolon,
//! Excel-flavored), TSV, Excel (.xlsx) and XML.
//!
//! An [`Exporter`] is a [`RowSink`]: rows are written as they arrive, so a
//! re-run of the query streams straight to the file whatever its size. It
//! writes one result set (`target`) and ignores the others.

use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{ResultColumn, RowSink};
use serde::Deserialize;
use serde_json::Value;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Json,
    JsonLines,
    Sql,
    Csv,
    CsvSemicolon,
    CsvExcel,
    Tsv,
    Xlsx,
    Xml,
}

/// Options of every format; each format reads the ones it uses.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ExportOptions {
    pub format: Format,
    /// Column names as the first row (CSV/TSV/Excel).
    pub header: bool,
    /// How NULL is written in CSV/TSV (empty by default).
    pub null_text: String,
    /// CSV/TSV delimiter override (one character); empty = the format's.
    pub delimiter: String,
    /// Always quote CSV fields (else only when needed).
    pub quote_all: bool,
    /// CSV/TSV: text cells and column names that a spreadsheet would read
    /// as a formula (starting with `=`, `+`, `-`, `@`, tab or CR) get a `'`
    /// in front (CWE-1236). On by default, also when the options come
    /// without it (scheduled tasks); numbers are never touched. Off gives
    /// the values exactly as they are, for files read by other programs.
    pub formula_safe: bool,
    /// Windows line endings (CSV for Excel uses them anyway).
    pub crlf: bool,
    /// UTF-8 byte order mark (so Excel reads accents right).
    pub bom: bool,
    /// Pretty-printed JSON array.
    pub pretty: bool,
    /// SQL: target table (as typed: may be `schema.table`).
    pub table: String,
    /// SQL: rows per INSERT statement.
    pub rows_per_insert: usize,
    /// SQL: identifier quoting ("double", "bracket", "backtick").
    pub quote: String,
    /// SQL: the source engine reads backslash escapes in '…' strings
    /// ([`dbine_driver::ScriptDialect::backslash_escapes`]: MySQL,
    /// ClickHouse, BigQuery, Hive, Spark…). Never taken from the UI: the
    /// backend sets it from the connection's driver. Backtick quoting (the
    /// identifiers of those engines) turns the escaping on as well.
    #[serde(skip)]
    pub backslash_escapes: bool,
    /// Excel: sheet name.
    pub sheet: String,
    /// XML: element names.
    pub xml_root: String,
    pub xml_row: String,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            format: Format::Csv,
            header: true,
            null_text: String::new(),
            delimiter: String::new(),
            quote_all: false,
            formula_safe: true,
            crlf: false,
            bom: false,
            pretty: true,
            table: "tabla".into(),
            rows_per_insert: 100,
            quote: "double".into(),
            backslash_escapes: false,
            sheet: "Resultado".into(),
            xml_root: "rows".into(),
            xml_row: "row".into(),
        }
    }
}

/// Text of a cell for text formats (`None` = NULL).
fn text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        other => Some(other.to_string()),
    }
}

/// A text cell a spreadsheet would run as a formula, with a `'` in front
/// (the OWASP CSV-injection advice). A plain number written as text
/// (`-5`, `+3.2`, `-1e3`) is left alone: it can't be a formula.
fn formula_safe(s: String) -> String {
    let risky = matches!(s.as_bytes().first(), Some(b'=' | b'+' | b'-' | b'@' | b'\t' | b'\r'));
    let number = s.len() > 1 && s.bytes().all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)) && s.parse::<f64>().is_ok();
    if risky && !number { format!("'{s}") } else { s }
}

/// Column names made unique ("id", "id_2"…): JSON keys and XML elements
/// can't repeat.
fn unique_names(cols: &[ResultColumn]) -> Vec<String> {
    let mut seen = std::collections::HashMap::<String, usize>::new();
    cols.iter()
        .map(|c| {
            let base = if c.name.is_empty() { "columna".to_string() } else { c.name.clone() };
            let n = seen.entry(base.clone()).or_insert(0);
            *n += 1;
            if *n == 1 { base } else { format!("{base}_{n}") }
        })
        .collect()
}

fn numeric_type(t: &str) -> bool {
    let t = t.to_ascii_lowercase();
    ["int", "dec", "num", "float", "double", "real", "money", "serial"].iter().any(|k| t.contains(k))
}

/// A cell that is a number in the result (or a numeric string of a
/// numeric column, as decimals arrive).
fn as_number(v: &Value, numeric_col: bool) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) if numeric_col => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

enum Writer {
    Text(BufWriter<File>),
    Csv(csv::Writer<BufWriter<File>>),
    Xlsx(Box<rust_xlsxwriter::Workbook>),
}

pub struct Exporter {
    opts: ExportOptions,
    path: PathBuf,
    target: usize,
    writer: Option<Writer>,
    names: Vec<String>,
    numeric: Vec<bool>,
    rows: u64,
    /// SQL: rows in the INSERT being written.
    in_batch: usize,
    /// Called with the running row count every so often (progress).
    progress: Option<Box<dyn FnMut(u64) + Send>>,
}

const XLSX_MAX_ROWS: u64 = 1_048_575;

impl Exporter {
    /// Export result set `target` of a run to `path`.
    pub fn new(path: &Path, target: usize, opts: ExportOptions) -> Self {
        Self {
            opts,
            path: path.to_path_buf(),
            target,
            writer: None,
            names: Vec::new(),
            numeric: Vec::new(),
            rows: 0,
            in_batch: 0,
            progress: None,
        }
    }

    pub fn on_progress(mut self, f: impl FnMut(u64) + Send + 'static) -> Self {
        self.progress = Some(Box::new(f));
        self
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    fn nl(&self) -> &'static str {
        if self.opts.crlf || self.opts.format == Format::CsvExcel { "\r\n" } else { "\n" }
    }

    fn text_out(&mut self) -> &mut BufWriter<File> {
        match self.writer.as_mut() {
            Some(Writer::Text(w)) => w,
            _ => unreachable!("text writer"),
        }
    }

    fn start(&mut self, columns: &[ResultColumn]) -> io::Result<()> {
        self.names = unique_names(columns);
        self.numeric = columns.iter().map(|c| numeric_type(&c.type_name)).collect();
        let o = &self.opts;
        match o.format {
            Format::Csv | Format::CsvSemicolon | Format::CsvExcel | Format::Tsv => {
                let mut f = BufWriter::new(File::create(&self.path)?);
                if o.bom || o.format == Format::CsvExcel {
                    f.write_all(b"\xEF\xBB\xBF")?;
                }
                let delim = o.delimiter.bytes().next().unwrap_or(match o.format {
                    Format::Tsv => b'\t',
                    Format::CsvSemicolon | Format::CsvExcel => b';',
                    _ => b',',
                });
                let mut w = csv::WriterBuilder::new()
                    .delimiter(delim)
                    .quote_style(if o.quote_all { csv::QuoteStyle::Always } else { csv::QuoteStyle::Necessary })
                    .terminator(if self.nl() == "\r\n" { csv::Terminator::CRLF } else { csv::Terminator::Any(b'\n') })
                    .from_writer(f);
                if o.header {
                    let safe = o.formula_safe;
                    w.write_record(self.names.iter().map(|n| if safe { formula_safe(n.clone()) } else { n.clone() }))?;
                }
                self.writer = Some(Writer::Csv(w));
            }
            Format::Json | Format::JsonLines | Format::Sql | Format::Xml => {
                let mut f = BufWriter::new(File::create(&self.path)?);
                if o.bom {
                    f.write_all(b"\xEF\xBB\xBF")?;
                }
                match o.format {
                    Format::Json => f.write_all(b"[")?,
                    Format::Xml => write!(
                        f,
                        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<{}>\n",
                        xml_name(&o.xml_root)
                    )?,
                    _ => {}
                }
                self.writer = Some(Writer::Text(f));
            }
            Format::Xlsx => {
                let mut wb = rust_xlsxwriter::Workbook::new();
                let bold = rust_xlsxwriter::Format::new().set_bold();
                let ws = wb.add_worksheet_with_constant_memory();
                let sheet: String = o.sheet.chars().filter(|c| !"[]:*?/\\".contains(*c)).take(31).collect();
                ws.set_name(if sheet.is_empty() { "Resultado" } else { &sheet }).map_err(xlsx_err)?;
                for (i, n) in self.names.iter().enumerate() {
                    let col = i as u16;
                    ws.set_column_width(col, (n.chars().count() as f64 + 4.0).clamp(10.0, 60.0)).map_err(xlsx_err)?;
                    if o.header {
                        ws.write_string_with_format(0, col, n, &bold).map_err(xlsx_err)?;
                    }
                }
                if o.header {
                    ws.set_freeze_panes(1, 0).map_err(xlsx_err)?;
                }
                self.writer = Some(Writer::Xlsx(Box::new(wb)));
            }
        }
        Ok(())
    }

    fn write_row(&mut self, row: &[Value]) -> io::Result<()> {
        let n = self.rows;
        self.rows += 1;
        match self.opts.format {
            Format::Csv | Format::CsvSemicolon | Format::CsvExcel | Format::Tsv => {
                let null = self.opts.null_text.clone();
                // Only text is neutralized: numbers and booleans can't be
                // formulas, and a negative number must stay a number.
                let safe = self.opts.formula_safe;
                let numeric = &self.numeric;
                let rec: Vec<String> = row
                    .iter()
                    .enumerate()
                    .map(|(i, v)| match (text(v), v) {
                        (None, _) => null.clone(),
                        (Some(t), Value::String(_)) if safe && !numeric.get(i).copied().unwrap_or(false) => formula_safe(t),
                        (Some(t), _) => t,
                    })
                    .collect();
                if let Some(Writer::Csv(w)) = self.writer.as_mut() {
                    w.write_record(&rec)?;
                }
            }
            Format::Json | Format::JsonLines => {
                let pretty = self.opts.pretty && self.opts.format == Format::Json;
                let mut obj = String::from("{");
                for (i, (name, v)) in self.names.iter().zip(row).enumerate() {
                    if i > 0 {
                        obj.push_str(if pretty { ",\n    " } else { "," });
                    } else if pretty {
                        obj.push_str("\n    ");
                    }
                    obj.push_str(&serde_json::to_string(name)?);
                    obj.push_str(if pretty { ": " } else { ":" });
                    obj.push_str(&serde_json::to_string(v)?);
                }
                obj.push_str(if pretty { "\n  }" } else { "}" });
                let json = self.opts.format == Format::Json;
                let sep = match (json, n, pretty) {
                    (true, 0, true) => "\n  ",
                    (true, 0, false) => "",
                    (true, _, true) => ",\n  ",
                    (true, _, false) => ",",
                    (false, _, _) => "",
                };
                let w = self.text_out();
                w.write_all(sep.as_bytes())?;
                w.write_all(obj.as_bytes())?;
                if !json {
                    w.write_all(b"\n")?;
                }
            }
            Format::Sql => {
                let q = match self.opts.quote.as_str() {
                    "bracket" => Quote::Bracket,
                    "backtick" => Quote::Backtick,
                    _ => Quote::Double,
                };
                let per = self.opts.rows_per_insert.max(1);
                // The script may be read by the source engine or by the one
                // the backtick quoting is meant for: escape for either.
                let bs = self.opts.backslash_escapes || q == Quote::Backtick;
                let values: Vec<String> = row.iter().zip(&self.numeric).map(|(v, &num)| sql_literal(v, num, bs)).collect();
                let head = if self.in_batch == 0 {
                    let table = sql_table(&self.opts.table, q);
                    let cols: Vec<String> = self.names.iter().map(|c| quote_ident(q, c)).collect();
                    format!("INSERT INTO {table} ({}) VALUES\n  ", cols.join(", "))
                } else {
                    ",\n  ".to_string()
                };
                self.in_batch += 1;
                let tail = if self.in_batch >= per {
                    self.in_batch = 0;
                    ";\n"
                } else {
                    ""
                };
                let line = format!("{head}({}){tail}", values.join(", "));
                self.text_out().write_all(line.as_bytes())?;
            }
            Format::Xml => {
                let nl = self.nl();
                let mut out = format!("  <{}>{nl}", xml_name(&self.opts.xml_row));
                for (name, v) in self.names.iter().zip(row) {
                    let tag = xml_name(name);
                    match text(v) {
                        None => out.push_str(&format!("    <{tag} null=\"true\"/>{nl}")),
                        Some(t) => out.push_str(&format!("    <{tag}>{}</{tag}>{nl}", xml_escape(&t))),
                    }
                }
                out.push_str(&format!("  </{}>{nl}", xml_name(&self.opts.xml_row)));
                self.text_out().write_all(out.as_bytes())?;
            }
            Format::Xlsx => {
                let r = n + u64::from(self.opts.header);
                if r > XLSX_MAX_ROWS {
                    return Err(io::Error::other("Excel admite hasta 1.048.576 filas por hoja; exportá en CSV"));
                }
                let numeric = self.numeric.clone();
                if let Some(Writer::Xlsx(wb)) = self.writer.as_mut() {
                    let ws = wb.worksheet_from_index(0).map_err(xlsx_err)?;
                    for (i, v) in row.iter().enumerate() {
                        let col = i as u16;
                        let row = r as u32;
                        match v {
                            Value::Null => {}
                            Value::Bool(b) => {
                                ws.write_boolean(row, col, *b).map_err(xlsx_err)?;
                            }
                            other => match as_number(other, numeric.get(i).copied().unwrap_or(false)) {
                                // Past 15 digits Excel rounds: keep those as text.
                                Some(f) if f.abs() < 1e15 => {
                                    ws.write_number(row, col, f).map_err(xlsx_err)?;
                                }
                                _ => {
                                    let t = text(other).unwrap_or_default();
                                    ws.write_string(row, col, t.chars().take(32_767).collect::<String>()).map_err(xlsx_err)?;
                                }
                            },
                        }
                    }
                }
            }
        }
        if n % 5_000 == 0 {
            if let Some(p) = self.progress.as_mut() {
                p(self.rows);
            }
        }
        Ok(())
    }

    /// Close the file. A run that returned no result set still leaves a
    /// valid (empty) file.
    pub fn finish(&mut self) -> io::Result<u64> {
        if self.writer.is_none() {
            self.start(&[])?;
        }
        let nl = self.nl();
        match self.writer.take() {
            Some(Writer::Csv(mut w)) => w.flush()?,
            Some(Writer::Text(mut f)) => {
                match self.opts.format {
                    Format::Json => f.write_all(if self.opts.pretty && self.rows > 0 { b"\n]\n" } else { b"]\n" })?,
                    Format::Sql if self.in_batch > 0 => f.write_all(b";\n")?,
                    Format::Xml => write!(f, "</{}>{nl}", xml_name(&self.opts.xml_root))?,
                    _ => {}
                }
                f.flush()?;
            }
            Some(Writer::Xlsx(mut wb)) => {
                if self.opts.header && !self.names.is_empty() {
                    let last_row = self.rows as u32;
                    let last_col = (self.names.len() - 1) as u16;
                    wb.worksheet_from_index(0).map_err(xlsx_err)?.autofilter(0, 0, last_row, last_col).map_err(xlsx_err)?;
                }
                wb.save(&self.path).map_err(xlsx_err)?;
            }
            None => {}
        }
        if let Some(p) = self.progress.as_mut() {
            p(self.rows);
        }
        Ok(self.rows)
    }
}

impl RowSink for Exporter {
    fn begin(&mut self, index: usize, columns: &[ResultColumn]) -> io::Result<()> {
        if index == self.target {
            self.start(columns)?;
        }
        Ok(())
    }

    fn row(&mut self, index: usize, row: &[Value]) -> io::Result<()> {
        if index == self.target && self.writer.is_some() {
            self.write_row(row)?;
        }
        Ok(())
    }
}

fn xlsx_err(e: rust_xlsxwriter::XlsxError) -> io::Error {
    io::Error::other(e.to_string())
}

/// A value as a literal of the INSERT script. `backslash`: the engine reads
/// backslash escapes in strings (see [`string_literal`]).
fn sql_literal(v: &Value, numeric_col: bool, backslash: bool) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "1".into() } else { "0".into() },
        Value::Number(n) => n.to_string(),
        // Only finite numbers go bare: "NaN" or "inf" would be identifiers.
        Value::String(s) if numeric_col && s.trim().parse::<f64>().is_ok_and(f64::is_finite) => s.trim().to_string(),
        Value::String(s) => string_literal(s, backslash),
        other => string_literal(&other.to_string(), backslash),
    }
}

/// `'…'` for the engine. Standard SQL only doubles the quote. Engines that
/// read backslash escapes (MySQL, ClickHouse, BigQuery, Hive, Spark…) would
/// take a stored `\'` as an escaped quote and let the rest of the value run
/// as SQL, so there every backslash and quote is escaped with a backslash
/// (`\'` is the form all of them accept; BigQuery and Spark don't read
/// `''`), and so are the bytes that cut a script in a client: NUL, line
/// breaks and Ctrl-Z (mysql on Windows reads it as the end of the file).
fn string_literal(s: &str, backslash: bool) -> String {
    if !backslash {
        return format!("'{}'", s.replace('\'', "''"));
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\0' => out.push_str("\\0"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{1a}' => out.push_str("\\Z"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// `schema.table` quoted part by part; empty → "tabla".
fn sql_table(name: &str, q: Quote) -> String {
    let name = if name.trim().is_empty() { "tabla" } else { name.trim() };
    name.split('.').map(|p| quote_ident(q, p.trim())).collect::<Vec<_>>().join(".")
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            // Control characters aren't allowed in XML 1.0.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}

/// A valid XML element name from a column name.
fn xml_name(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '_' | '-' | '.') { c } else { '_' })
        .collect();
    if out.is_empty() || !out.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') || out.to_lowercase().starts_with("xml") {
        out.insert(0, '_');
    }
    out
}

/// Export rows already in memory (the grid's) to a file.
pub fn export_rows(path: &Path, opts: ExportOptions, columns: &[ResultColumn], rows: &[Vec<Value>]) -> io::Result<u64> {
    let mut e = Exporter::new(path, 0, opts);
    e.begin(0, columns)?;
    for r in rows {
        e.row(0, r)?;
    }
    e.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cols() -> Vec<ResultColumn> {
        vec![
            ResultColumn { name: "id".into(), type_name: "int".into() },
            ResultColumn { name: "nombre".into(), type_name: "varchar".into() },
            ResultColumn { name: "total".into(), type_name: "decimal".into() },
            ResultColumn { name: "id".into(), type_name: "int".into() },
        ]
    }
    fn rows() -> Vec<Vec<Value>> {
        vec![
            vec![json!(1), json!("Pérez; \"el\" <1>"), json!("10.50"), json!(7)],
            vec![json!(2), Value::Null, json!("3"), Value::Null],
        ]
    }
    fn run(format: Format, tweak: impl FnOnce(&mut ExportOptions)) -> String {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("out");
        let mut o = ExportOptions { format, ..Default::default() };
        tweak(&mut o);
        export_rows(&p, o, &cols(), &rows()).unwrap();
        String::from_utf8_lossy(&std::fs::read(&p).unwrap()).to_string()
    }

    #[test]
    fn csv_variants() {
        assert_eq!(run(Format::Csv, |_| {}), "id,nombre,total,id_2\n1,\"Pérez; \"\"el\"\" <1>\",10.50,7\n2,,3,\n");
        let excel = run(Format::CsvExcel, |_| {});
        assert!(excel.starts_with('\u{feff}'), "BOM so Excel reads UTF-8");
        assert!(excel.contains("id;nombre;total;id_2\r\n"));
        assert!(run(Format::Tsv, |o| o.null_text = "NULL".into()).contains("2\tNULL\t3\tNULL\n"));
    }

    #[test]
    fn csv_cells_cannot_become_formulas() {
        let cols = vec![
            ResultColumn { name: "=HYPERLINK(\"x\")".into(), type_name: "varchar".into() },
            ResultColumn { name: "n".into(), type_name: "varchar".into() },
            ResultColumn { name: "amount".into(), type_name: "decimal".into() },
        ];
        let rows = vec![
            vec![json!("=1+1"), json!(-5), json!("-12.50")],
            vec![json!("+cmd|' /C calc'!A0"), json!("-5"), json!("-1")],
            vec![json!("@SUM(A1)"), json!("-2+3"), Value::Null],
            vec![json!("\t=1"), json!("\r=1"), json!("1")],
            vec![json!("-"), json!("ok"), json!("2")],
        ];
        let out = |f: Format, safe: Option<bool>| {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("out");
            let mut o = ExportOptions { format: f, ..Default::default() };
            if let Some(v) = safe {
                o.formula_safe = v;
            }
            export_rows(&p, o, &cols, &rows).unwrap();
            String::from_utf8_lossy(&std::fs::read(&p).unwrap()).to_string()
        };
        for f in [Format::CsvExcel, Format::Csv, Format::CsvSemicolon, Format::Tsv] {
            let s = out(f, None);
            assert!(s.contains("'=HYPERLINK"), "{f:?}: {s}");
            assert!(s.contains("'=1+1"), "{f:?}: {s}");
            assert!(s.contains("'+cmd"), "{f:?}: {s}");
            assert!(s.contains("'@SUM(A1)"), "{f:?}: {s}");
            assert!(s.contains("'\t=1") && s.contains("'\r=1"), "{f:?}: {s}");
            assert!(s.contains("'-2+3") && s.contains("'-"), "{f:?}: {s}");
            // Numbers stay numbers: a JSON number, a number written as text
            // and any value of a numeric column.
            assert!(!s.contains("'-5") && !s.contains("'-12.50") && !s.contains("'-1"), "{f:?}: {s}");
        }
        // Off: the values exactly as they are.
        let raw = out(Format::Csv, Some(false));
        assert!(raw.starts_with("\"=HYPERLINK(\"\"x\"\")\",n,amount\n=1+1,-5,-12.50\n"), "{raw}");
        assert!(!raw.contains("'=") && !raw.contains("'@") && raw.contains("\n+cmd"), "{raw}");
        // Scheduled tasks send the options as JSON, often without the field.
        let o: ExportOptions = serde_json::from_value(json!({ "format": "csv" })).unwrap();
        assert!(o.formula_safe);
    }

    #[test]
    fn json_and_lines_keep_column_order_and_nulls() {
        let j: Value = serde_json::from_str(&run(Format::Json, |_| {})).unwrap();
        assert_eq!(j[1]["nombre"], Value::Null);
        assert_eq!(j[0]["id_2"], json!(7));
        let compact = run(Format::Json, |o| o.pretty = false);
        assert!(compact.starts_with("[{\"id\":1,\"nombre\":"));
        let lines = run(Format::JsonLines, |_| {});
        assert_eq!(lines.lines().count(), 2);
        assert!(lines.lines().all(|l| serde_json::from_str::<Value>(l).is_ok()));
    }

    #[test]
    fn sql_inserts_in_batches() {
        let s = run(Format::Sql, |o| {
            o.table = "dbo.clientes".into();
            o.quote = "bracket".into();
            o.rows_per_insert = 1;
        });
        assert!(s.contains("INSERT INTO [dbo].[clientes] ([id], [nombre], [total], [id_2]) VALUES\n  (1, 'Pérez; \"el\" <1>', 10.50, 7);"));
        assert!(s.contains("(2, NULL, 3, NULL);"));
        let batched = run(Format::Sql, |_| {});
        assert_eq!(batched.matches("INSERT INTO").count(), 1);
        assert!(batched.trim_end().ends_with(';'));
    }

    const EXPLOIT: &str = "x\\');DROP TABLE users;#";

    #[test]
    fn sql_literals_follow_the_dialect() {
        let v = json!(EXPLOIT);
        // Standard SQL: a backslash is just a character, the quote doubles.
        assert_eq!(sql_literal(&v, false, false), "'x\\'');DROP TABLE users;#'");
        // Backslash engines: `\'` must not close the string.
        assert_eq!(sql_literal(&v, false, true), "'x\\\\\\');DROP TABLE users;#'");
        assert_eq!(sql_literal(&json!("a\0b\nc\rd\u{1a}e"), false, true), "'a\\0b\\nc\\rd\\Ze'");
        assert_eq!(sql_literal(&json!({"k": "it's"}), false, true), "'{\"k\":\"it\\'s\"}'");
        assert_eq!(sql_literal(&json!("NaN"), true, false), "'NaN'");
        assert_eq!(sql_literal(&json!(" 1e3 "), true, false), "1e3");
    }

    /// Where a `'…'` literal that starts at `start` ends, read the way the
    /// engine reads it.
    fn literal_end(s: &str, start: usize, backslash: bool) -> usize {
        let b = s.as_bytes();
        let mut i = start + 1;
        loop {
            match b[i] {
                b'\\' if backslash => i += 2,
                b'\'' if b.get(i + 1) == Some(&b'\'') => i += 2,
                b'\'' => return i,
                _ => i += 1,
            }
        }
    }

    #[test]
    fn sql_export_cannot_be_escaped_by_a_value() {
        let one = |tweak: fn(&mut ExportOptions)| {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("out.sql");
            let mut o = ExportOptions { format: Format::Sql, table: "t".into(), ..Default::default() };
            tweak(&mut o);
            let cols = [ResultColumn { name: "v".into(), type_name: "varchar".into() }];
            export_rows(&p, o, &cols, &[vec![json!(EXPLOIT)]]).unwrap();
            std::fs::read_to_string(&p).unwrap()
        };
        // From the source connection's driver, from the backtick quoting,
        // and the standard escaping.
        for (script, backslash) in [
            (one(|o| o.backslash_escapes = true), true),
            (one(|o| o.quote = "backtick".into()), true),
            (one(|_| {}), false),
        ] {
            let start = script.find('\'').unwrap();
            let end = literal_end(&script, start, backslash);
            assert_eq!(&script[end + 1..], ");\n", "the value ends where it should: {script}");
        }
    }

    #[test]
    fn xml_escapes_and_marks_nulls() {
        let x = run(Format::Xml, |_| {});
        assert!(x.contains("<nombre>Pérez; &quot;el&quot; &lt;1&gt;</nombre>"));
        assert!(x.contains("<nombre null=\"true\"/>"));
        assert!(x.trim_end().ends_with("</rows>"));
    }

    #[test]
    fn xlsx_is_a_real_workbook() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("out.xlsx");
        export_rows(&p, ExportOptions { format: Format::Xlsx, ..Default::default() }, &cols(), &rows()).unwrap();
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(&bytes[..2], b"PK", "a zip (xlsx) file");
    }

    #[test]
    fn only_the_target_result_set_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("out.csv");
        let mut e = Exporter::new(&p, 1, ExportOptions::default());
        e.begin(0, &cols()).unwrap();
        e.row(0, &rows()[0]).unwrap();
        e.begin(1, &cols()[..1]).unwrap();
        e.row(1, &[json!(42)]).unwrap();
        assert_eq!(e.finish().unwrap(), 1);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "id\n42\n");
    }
}
