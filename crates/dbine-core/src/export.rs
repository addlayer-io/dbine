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
    /// SQL: how the source engine reads '…' strings, which decides how the
    /// string literals are written (see [`SourceStrings`]). Never taken from
    /// the UI: the backend sets it from the connection's driver
    /// ([`SourceStrings::of`]); without a connection it stays `Unknown`.
    #[serde(skip)]
    pub source: SourceStrings,
    /// SQL: the user says the script goes to an engine that reads strings
    /// the standard way, so backslashes are written as they are. It only
    /// matters when the source is `Unknown` (no connection, or an engine
    /// without an exact form): off, a backslash is written doubled, so the
    /// literal ends in the same place whichever engine reads it (Redshift,
    /// Snowflake and ClickHouse read `\'` as an escaped quote). Sources that
    /// read backslash escapes and backtick quoting always double them; the
    /// standard engines DBine knows (PostgreSQL, SQL Server, Oracle, SQLite,
    /// DuckDB) always get their exact form, without backslashes.
    pub standard_strings: bool,
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
            source: SourceStrings::Unknown,
            standard_strings: false,
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
    let trigger = |b: u8| matches!(b, b'=' | b'+' | b'-' | b'@' | b'\t' | b'\r');
    let number = s.len() > 1 && s.bytes().all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)) && s.parse::<f64>().is_ok();
    if number {
        return s;
    }
    // A spreadsheet whose list separator isn't the file's (`;` in es, pt, fr
    // and it; `,` in en) splits an unquoted field at `,` or `;` too: every
    // piece that would start a formula gets its `'` as well.
    let mut out = String::with_capacity(s.len() + 1);
    let mut piece_start = true;
    for c in s.chars() {
        if piece_start && c.is_ascii() && trigger(c as u8) {
            out.push('\'');
        }
        out.push(c);
        piece_start = matches!(c, ',' | ';');
    }
    out
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
                // formulas. Every text cell goes through the guard, whatever
                // the column's declared type (SQLite keeps any text in an
                // INTEGER column); a number written as text stays as is.
                let safe = self.opts.formula_safe;
                let rec: Vec<String> = row
                    .iter()
                    .map(|v| match (text(v), v) {
                        (None, _) => null.clone(),
                        (Some(t), Value::String(_)) if safe => formula_safe(t),
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
                let lit = Literal::pick(self.opts.source, q, self.opts.standard_strings);
                let values: Vec<String> = row.iter().zip(&self.numeric).map(|(v, &num)| sql_literal(v, num, lit)).collect();
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

/// How the source engine reads `'…'` strings: it decides how an SQL export
/// writes them, so the script gives back the same value on the source
/// engine and can't be broken by a value on any other.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SourceStrings {
    /// No connection (rows of a multi-database grid, a connection gone) or
    /// an engine without an exact form here: the dual-safe form (or the
    /// standard one, if the user asks for it with `standard_strings`).
    #[default]
    Unknown,
    /// Reads backslash escapes in strings (MySQL, MariaDB, ClickHouse,
    /// BigQuery, Snowflake, Hive, Spark… and Redshift): the dual-safe form,
    /// exact there.
    Backslash,
    /// PostgreSQL and its wire-compatible engines but Redshift: `chr(92)`.
    Postgres,
    /// SQL Server, Azure SQL, Fabric, Babelfish: `CONCAT(…, CHAR(92), …)`.
    SqlServer,
    /// Oracle (and engines with its dialect): `CHR(92)`.
    Oracle,
    /// SQLite and libSQL: `char(92)`.
    Sqlite,
    /// DuckDB: `chr(92)`.
    DuckDb,
}

impl SourceStrings {
    /// From the connection's driver.
    pub fn of(driver: &dyn dbine_driver::Driver) -> Self {
        let info = driver.info();
        Self::from_parts(info.id, info.dialect, driver.script_dialect().backslash_escapes)
    }

    /// From the driver id, its dialect hint and whether its scripts read
    /// backslash escapes.
    pub fn from_parts(id: &str, dialect: &str, backslash_escapes: bool) -> Self {
        // Redshift shares PostgreSQL's lexer but its strings read `\'` as an
        // escaped quote.
        if backslash_escapes || id == "redshift" {
            return Self::Backslash;
        }
        match (id, dialect) {
            ("duckdb" | "duckdb_files", _) => Self::DuckDb,
            (_, "postgres") => Self::Postgres,
            (_, "mssql") => Self::SqlServer,
            (_, "oracle") => Self::Oracle,
            (_, "sqlite") => Self::Sqlite,
            _ => Self::Unknown,
        }
    }
}

/// How the INSERT script writes a string value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Literal {
    /// `'…'` with the quote doubled, backslashes as they are.
    Standard,
    /// The quote and the backslash doubled (see [`dual_safe_literal`]).
    DualSafe,
    /// Standard pieces joined with the engine's character function where the
    /// value has a backslash or a NUL (see [`spliced_literal`]).
    Spliced(SourceStrings),
}

impl Literal {
    fn pick(source: SourceStrings, quote: Quote, standard_strings: bool) -> Self {
        // Backtick identifiers are MySQL's (and its kin's): the script goes
        // to an engine that reads backslash escapes and has neither `chr` nor
        // `||` as concatenation.
        if quote == Quote::Backtick {
            return Self::DualSafe;
        }
        match source {
            SourceStrings::Backslash => Self::DualSafe,
            SourceStrings::Unknown if standard_strings => Self::Standard,
            SourceStrings::Unknown => Self::DualSafe,
            known => Self::Spliced(known),
        }
    }
}

/// A value as a literal of the INSERT script.
fn sql_literal(v: &Value, numeric_col: bool, lit: Literal) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "1".into() } else { "0".into() },
        Value::Number(n) => n.to_string(),
        // Only finite numbers go bare: "NaN" or "inf" would be identifiers.
        Value::String(s) if numeric_col && s.trim().parse::<f64>().is_ok_and(f64::is_finite) => s.trim().to_string(),
        Value::String(s) => string_literal(s, lit),
        other => string_literal(&other.to_string(), lit),
    }
}

fn string_literal(s: &str, lit: Literal) -> String {
    match lit {
        Literal::Standard => standard_literal(s),
        Literal::DualSafe => dual_safe_literal(s),
        Literal::Spliced(src) => spliced_literal(s, src),
    }
}

/// `'…'` the standard way: only the quote doubles.
fn standard_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Parts joined in one call or `||` chain at most: SQL Server's CONCAT takes
/// up to 254 arguments, SQLite nests 1000 expressions at most and
/// PostgreSQL has a stack limit, so a value with many backslashes is joined
/// in groups of groups (but on Oracle, see [`spliced_literal`]).
const SPLICE_GROUP: usize = 50;

/// The exact form for a standard engine DBine knows: the value as standard
/// `'…'` pieces, and each backslash (and NUL, which a script can't carry
/// raw) as the engine's character function, joined in parentheses:
/// `('a' || chr(92) || 'b')`, or `CONCAT('a', CHAR(92), 'b')` on SQL Server.
/// No backslash is left anywhere in the literal, so it ends in the same
/// place whether the engine that reads it takes backslash escapes or not,
/// and the source engine reads back exactly the value. A value without
/// backslash or NUL is just the standard literal.
///
/// NUL: PostgreSQL text can't hold it (`chr(0)` fails, which beats cutting
/// the value); the others store it.
fn spliced_literal(s: &str, src: SourceStrings) -> String {
    if !s.contains(['\\', '\0']) {
        return standard_literal(s);
    }
    let char_fn = |c: char| match src {
        SourceStrings::SqlServer => format!("CHAR({})", c as u32),
        SourceStrings::Oracle => format!("CHR({})", c as u32),
        SourceStrings::Sqlite => format!("char({})", c as u32),
        _ => format!("chr({})", c as u32),
    };
    let mut parts = Vec::new();
    let mut piece = String::new();
    for c in s.chars() {
        if matches!(c, '\\' | '\0') {
            if !piece.is_empty() {
                parts.push(standard_literal(&std::mem::take(&mut piece)));
            }
            parts.push(char_fn(c));
        } else {
            piece.push(c);
        }
    }
    if !piece.is_empty() {
        parts.push(standard_literal(&piece));
    }
    // CONCAT without a MAX argument cuts its result at 8000 bytes.
    let long = src == SourceStrings::SqlServer && s.len() > 4000;
    // Oracle (23ai) misreads a nested `((…) || (…))` in a VALUES list of
    // more than one row (ORA-00907); its strings stop at 4000 bytes (32767
    // extended) anyway, so the chain stays flat there.
    let group = if src == SourceStrings::Oracle { usize::MAX } else { SPLICE_GROUP };
    while parts.len() > 1 {
        parts = parts
            .chunks(group)
            .map(|g| match src {
                SourceStrings::SqlServer if long => format!("CONCAT(CAST('' AS varchar(max)), {})", g.join(", ")),
                SourceStrings::SqlServer if g.len() > 1 => format!("CONCAT({})", g.join(", ")),
                _ if g.len() > 1 => format!("({})", g.join(" || ")),
                _ => g[0].clone(),
            })
            .collect();
    }
    parts.pop().unwrap_or_default()
}

/// `'…'` that ends in the same place under both reading rules.
///
/// Engines that read backslash escapes (MySQL, ClickHouse, Hive…) would take
/// a stored `\'` as an escaped quote and let the rest of the value run as
/// SQL, so every backslash is doubled. The script may still be run on
/// another engine than the source's (the user picks the quoting), so the
/// literal has to end in the same place under both reading rules:
/// - the quote is always doubled (`''`), never `\'`: a standard engine
///   (PostgreSQL, SQL Server, Oracle, MySQL with NO_BACKSLASH_ESCAPES) reads
///   `\'` as a backslash plus the closing quote, which would let the value
///   inject SQL. `''` is a quote for standard engines, for MySQL in both
///   modes and for ClickHouse.
/// - `\\`, `\0` and `\Z` are plain text under the standard rule and the
///   escaped byte under the backslash rule; none of them contains a quote, so
///   neither rule can end the literal there. NUL and Ctrl-Z (which mysql on
///   Windows reads as the end of the file) are escaped; line breaks stay as
///   they are, which every engine reads the same inside a literal.
///
/// Exact on the engines that read backslash escapes; a standard engine keeps
/// the doubled backslashes (the standard engines DBine knows get
/// [`spliced_literal`] instead). BigQuery and Spark don't read `''` as a
/// quote: there the statement fails or concatenates two literals, which
/// never runs the value as SQL.
fn dual_safe_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("''"),
            '\0' => out.push_str("\\0"),
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
            vec![json!("Ana;=cmd|' /C calc'!A0"), json!("x,=WEBSERVICE(B2)"), json!("3")],
            vec![json!("a; +cmd"), json!("1,2"), json!("4")],
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
            // A piece after `,` or `;` that a spreadsheet with another list
            // separator would make its own cell.
            assert!(!s.contains(";=") && !s.contains(",=") && !s.contains(";+cmd"), "{f:?}: {s}");
            // Numbers stay numbers: a JSON number, a number written as text
            // and any value of a numeric column.
            assert!(!s.contains("'-5") && !s.contains("'-12.50") && !s.contains("'-1"), "{f:?}: {s}");
        }
        // Off: the values exactly as they are.
        let raw = out(Format::Csv, Some(false));
        assert!(raw.starts_with("\"=HYPERLINK(\"\"x\"\")\",n,amount\n=1+1,-5,-12.50\n"), "{raw}");
        assert!(!raw.contains("'=") && !raw.contains("'@") && raw.contains("\n+cmd"), "{raw}");
        // The column's declared type doesn't exempt text: SQLite keeps any
        // text in an INTEGER column.
        let typed = vec![ResultColumn { name: "id".into(), type_name: "INTEGER".into() }];
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("typed.csv");
        let o = ExportOptions { format: Format::Csv, ..Default::default() };
        export_rows(&p, o, &typed, &[vec![json!("=HYPERLINK(\"http://x\")")], vec![json!("-5")], vec![json!(-7)]]).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("'=HYPERLINK"), "{s}");
        assert!(s.contains("\n-5\n") && !s.contains("'-5") && s.ends_with("-7\n"), "{s}");
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

    /// Values that have broken or bent string literals.
    const TRICKY: [&str; 13] = [
        "a\\b",
        "\\",
        "x\\'",
        "x\\');DROP TABLE users;--",
        EXPLOIT,
        "ends with\\",
        "\\\\",
        "it's",
        "line\nbreak\r\n",
        "",
        "a\0b",
        "x'); DROP TABLE users; --",
        "C:\\temp\\new\\'' \\0 \\Z",
    ];

    const SOURCES: [SourceStrings; 7] = [
        SourceStrings::Unknown,
        SourceStrings::Backslash,
        SourceStrings::Postgres,
        SourceStrings::SqlServer,
        SourceStrings::Oracle,
        SourceStrings::Sqlite,
        SourceStrings::DuckDb,
    ];

    #[test]
    fn source_strings_from_the_driver() {
        use SourceStrings::*;
        let cases = [
            (("postgres", "postgres", false), Postgres),
            (("cockroachdb", "postgres", false), Postgres),
            (("dsql", "postgres", false), Postgres),
            (("redshift", "postgres", false), Backslash),
            (("sqlserver", "mssql", false), SqlServer),
            (("babelfish", "mssql", false), SqlServer),
            (("oracle", "oracle", false), Oracle),
            (("oracle_adb", "oracle", false), Oracle),
            (("sqlite", "sqlite", false), Sqlite),
            (("libsql", "sqlite", false), Sqlite),
            (("duckdb", "standard", false), DuckDb),
            (("duckdb_files", "standard", false), DuckDb),
            (("firebird", "standard", false), Unknown),
            (("mysql", "mysql", true), Backslash),
            (("snowflake", "snowflake", true), Backslash),
            // Backslash escapes win over the dialect hint.
            (("x", "postgres", true), Backslash),
        ];
        for ((id, dialect, bs), want) in cases {
            assert_eq!(SourceStrings::from_parts(id, dialect, bs), want, "{id}");
        }
    }

    #[test]
    fn sql_literals_follow_the_source() {
        use SourceStrings::*;
        let lit = |v: &str, src, q, std| sql_literal(&json!(v), false, Literal::pick(src, q, std));
        let dual = "'x\\\\'');DROP TABLE users;#'";
        // No connection: doubled backslashes, unless the user says the target
        // reads strings the standard way.
        assert_eq!(lit(EXPLOIT, Unknown, Quote::Double, false), dual);
        assert_eq!(lit(EXPLOIT, Unknown, Quote::Double, true), "'x\\'');DROP TABLE users;#'");
        // A source that reads backslash escapes, or backtick quoting: always
        // doubled, whatever the user says.
        assert_eq!(lit(EXPLOIT, Backslash, Quote::Double, true), dual);
        assert_eq!(lit(EXPLOIT, Postgres, Quote::Backtick, false), dual);
        assert_eq!(lit(EXPLOIT, SqlServer, Quote::Backtick, true), dual);
        // Standard engines DBine knows: the backslash as the engine's
        // character function, whatever the user says.
        for std in [false, true] {
            assert_eq!(lit(EXPLOIT, Postgres, Quote::Double, std), "('x' || chr(92) || ''');DROP TABLE users;#')");
            assert_eq!(lit(EXPLOIT, DuckDb, Quote::Double, std), "('x' || chr(92) || ''');DROP TABLE users;#')");
            assert_eq!(lit(EXPLOIT, Oracle, Quote::Double, std), "('x' || CHR(92) || ''');DROP TABLE users;#')");
            assert_eq!(lit(EXPLOIT, Sqlite, Quote::Double, std), "('x' || char(92) || ''');DROP TABLE users;#')");
            assert_eq!(lit(EXPLOIT, SqlServer, Quote::Bracket, std), "CONCAT('x', CHAR(92), ''');DROP TABLE users;#')");
        }
        assert_eq!(lit("\\", Postgres, Quote::Double, false), "chr(92)");
        assert_eq!(lit("\\", SqlServer, Quote::Bracket, false), "CHAR(92)");
        assert_eq!(lit("\\\\", SqlServer, Quote::Bracket, false), "CONCAT(CHAR(92), CHAR(92))");
        assert_eq!(lit("a\\", Sqlite, Quote::Double, false), "('a' || char(92))");
        assert_eq!(lit("a\0b", SqlServer, Quote::Bracket, false), "CONCAT('a', CHAR(0), 'b')");
        // Without a backslash it's the plain standard literal.
        assert_eq!(lit("it's\nok", Postgres, Quote::Double, false), "'it''s\nok'");
        assert_eq!(lit("", SqlServer, Quote::Bracket, false), "''");
        // Line breaks stay as they are in the dual-safe form too.
        assert_eq!(lit("a\0b\nc\rd\u{1a}e", Unknown, Quote::Double, false), "'a\\0b\nc\rd\\Ze'");
        // JSON values go through the same literal.
        let json_lit = |src| sql_literal(&json!({"k": "it's \\ ok"}), false, Literal::pick(src, Quote::Double, false));
        assert_eq!(json_lit(Unknown), "'{\"k\":\"it''s \\\\\\\\ ok\"}'");
        assert_eq!(json_lit(Postgres), "('{\"k\":\"it''s ' || chr(92) || chr(92) || ' ok\"}')");
        let standard = Literal::Standard;
        assert_eq!(sql_literal(&json!("NaN"), true, standard), "'NaN'");
        assert_eq!(sql_literal(&json!(" 1e3 "), true, standard), "1e3");
    }

    #[test]
    fn many_backslashes_are_joined_in_groups() {
        let v = "\\".repeat(5000);
        let depth = |s: &str| {
            let (mut d, mut max) = (0i32, 0i32);
            for c in s.chars() {
                d += i32::from(c == '(') - i32::from(c == ')');
                max = max.max(d);
            }
            max
        };
        let oracle = spliced_literal(&v, SourceStrings::Oracle);
        assert_eq!(eval_spliced(&oracle), v);
        assert_eq!(depth(&oracle), 2, "one flat chain of CHR(92) on Oracle");
        for src in [SourceStrings::Postgres, SourceStrings::Sqlite, SourceStrings::SqlServer] {
            let s = spliced_literal(&v, src);
            assert_eq!(eval_spliced(&s), v, "{src:?}");
            // 5000 parts: three levels of groups, each at most 50 wide.
            assert!(depth(&s) <= 8, "{src:?}: {}", depth(&s));
            if src == SourceStrings::SqlServer {
                // Past 4000 characters CONCAT has to return varchar(max).
                assert!(s.starts_with("CONCAT(CAST('' AS varchar(max)), CONCAT(CAST('' AS varchar(max)), "), "{}", &s[..80]);
            }
        }
    }

    /// Where a `'…'` literal that starts at `start` ends, read the way the
    /// engine reads it (the end of the text if it never ends).
    fn literal_end(s: &str, start: usize, backslash: bool) -> usize {
        let b = s.as_bytes();
        let mut i = start + 1;
        while i < b.len() {
            match b[i] {
                b'\\' if backslash => i += 2,
                b'\'' if b.get(i + 1) == Some(&b'\'') => i += 2,
                b'\'' => return i,
                _ => i += 1,
            }
        }
        b.len()
    }

    /// The script with every literal as `L`, read with one rule.
    fn skeleton(s: &str, backslash: bool) -> String {
        let b = s.as_bytes();
        let (mut out, mut i) = (Vec::new(), 0);
        while i < b.len() {
            if b[i] == b'\'' {
                out.push(b'L');
                i = literal_end(s, i, backslash) + 1;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The text inside a literal, read with one rule.
    fn decode(content: &str, backslash: bool) -> String {
        let mut out = String::new();
        let mut it = content.chars().peekable();
        while let Some(c) = it.next() {
            match c {
                '\\' if backslash => match it.next() {
                    Some('0') => out.push('\0'),
                    Some('Z') => out.push('\u{1a}'),
                    Some(o) => out.push(o),
                    None => {}
                },
                '\'' => {
                    it.next();
                    out.push('\'');
                }
                c => out.push(c),
            }
        }
        out
    }

    /// What the engine reads from a spliced literal: the pieces in order,
    /// each character function as its character.
    fn eval_spliced(expr: &str) -> String {
        let expr = expr.replace("CAST('' AS varchar(max))", "");
        let mut out = String::new();
        let mut i = 0;
        while i < expr.len() {
            let rest = &expr[i..];
            if rest.starts_with('\'') {
                let end = literal_end(&expr, i, false);
                out.push_str(&decode(&expr[i + 1..end], false));
                i = end + 1;
            } else if let Some(p) = ["chr(", "CHR(", "char(", "CHAR("].iter().find(|p| rest.starts_with(**p)) {
                let close = rest.find(')').unwrap();
                out.push(char::from_u32(rest[p.len()..close].parse().unwrap()).unwrap());
                i += close + 1;
            } else {
                assert!("()|, CONCAT".contains(&rest[..1]), "unexpected {rest}");
                i += 1;
            }
        }
        out
    }

    #[test]
    fn sql_export_cannot_be_escaped_by_a_value() {
        let one = |src: SourceStrings, quote: &str, std: bool, value: &str| {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("out.sql");
            let o = ExportOptions {
                format: Format::Sql,
                table: "t".into(),
                quote: quote.into(),
                source: src,
                standard_strings: std,
                ..Default::default()
            };
            let cols = [ResultColumn { name: "v".into(), type_name: "varchar".into() }];
            export_rows(&p, o, &cols, &[vec![json!(value)]]).unwrap();
            std::fs::read_to_string(&p).unwrap()
        };
        for src in SOURCES {
            for quote in ["backtick", "double", "bracket"] {
                for std in [false, true] {
                    let q = match quote {
                        "backtick" => Quote::Backtick,
                        "bracket" => Quote::Bracket,
                        _ => Quote::Double,
                    };
                    let lit = Literal::pick(src, q, std);
                    for value in TRICKY {
                        let script = one(src, quote, std, value);
                        let ctx = format!("src={src:?} quote={quote} std={std} lit={lit:?}: {script:?}");
                        let head = script.find("VALUES\n  (").unwrap() + "VALUES\n  (".len();
                        assert!(script.ends_with(");\n"), "{ctx}");
                        let expr = &script[head..script.len() - 3];
                        // The script may run on a standard engine or on one
                        // that reads backslash escapes: the literals must end
                        // in the same place under both rules, and the value
                        // never reaches the SQL around them. The standard
                        // form is exact for standard engines only, so there
                        // the backslash rule is checked only on a value
                        // without backslashes.
                        let mut rules = vec![false];
                        if lit != Literal::Standard || !value.contains('\\') {
                            rules.push(true);
                        }
                        let shapes: Vec<String> = rules.iter().map(|&r| skeleton(&script, r)).collect();
                        for shape in &shapes {
                            assert_eq!(shape, &shapes[0], "{ctx}");
                            assert!(!shape.contains("DROP") && shape.matches(';').count() == 1, "{ctx}: {shape}");
                        }
                        // The source engine reads back exactly the value.
                        match lit {
                            Literal::Spliced(_) => {
                                assert!(!script.contains('\\'), "no backslash in a spliced script: {ctx}");
                                assert_eq!(eval_spliced(expr), value, "{ctx}");
                            }
                            Literal::DualSafe => assert_eq!(decode(&expr[1..expr.len() - 1], true), value, "{ctx}"),
                            Literal::Standard => assert_eq!(decode(&expr[1..expr.len() - 1], false), value, "{ctx}"),
                        }
                    }
                }
            }
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
