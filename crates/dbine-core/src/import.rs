//! Reading data files to import: CSV (any delimiter), TSV, JSON (array of
//! objects), JSON Lines, Excel (.xlsx/.xls/.ods) and XML (a root element
//! with one child per row, one grandchild per field — what the XML export
//! writes). Values come out typed: numbers, booleans, text, NULL.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImportFormat {
    Auto,
    Csv,
    CsvSemicolon,
    Tsv,
    Json,
    JsonLines,
    Xlsx,
    Xml,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ImportOptions {
    /// One character; empty = the format's.
    pub delimiter: String,
    /// First row holds the column names (CSV/TSV/Excel).
    pub header: bool,
    /// Excel sheet (`None` / empty = the first).
    pub sheet: Option<String>,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self { delimiter: String::new(), header: true, sheet: None }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PreviewColumn {
    pub name: String,
    /// integer | number | boolean | date | datetime | text
    pub inferred_type: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    pub format: ImportFormat,
    pub columns: Vec<PreviewColumn>,
    pub rows: Vec<Vec<Value>>,
    pub sheets: Vec<String>,
}

fn bad(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

pub fn detect(path: &Path, format: ImportFormat) -> ImportFormat {
    if format != ImportFormat::Auto {
        return format;
    }
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("tsv" | "tab") => ImportFormat::Tsv,
        Some("json") => ImportFormat::Json,
        Some("jsonl" | "ndjson") => ImportFormat::JsonLines,
        Some("xlsx" | "xlsm" | "xls" | "xlsb" | "ods") => ImportFormat::Xlsx,
        Some("xml") => ImportFormat::Xml,
        _ => ImportFormat::Csv,
    }
}

/// A text cell typed by its looks: integers, decimals, true/false; the
/// empty string is NULL.
fn typed(s: &str) -> Value {
    let t = s.trim();
    if t.is_empty() {
        return Value::Null;
    }
    if let Ok(i) = t.parse::<i64>() {
        // Leading zeros (codes, zip codes) stay text.
        if !(t.len() > 1 && t.starts_with('0')) && !(t.len() > 2 && t.starts_with("-0")) {
            return dbine_driver::json_i64(i);
        }
    }
    if t.contains('.') && !t.starts_with('.') {
        if let Ok(f) = t.parse::<f64>() {
            if f.is_finite() {
                return dbine_driver::json_f64(f);
            }
        }
    }
    match t.to_ascii_lowercase().as_str() {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => Value::String(s.to_string()),
    }
}

/// Rows of a file: the column names and an iterator of rows aligned to them.
pub struct Reader {
    pub columns: Vec<String>,
    pub sheets: Vec<String>,
    pub format: ImportFormat,
    rows: Box<dyn Iterator<Item = io::Result<Vec<Value>>> + Send>,
}

impl Iterator for Reader {
    type Item = io::Result<Vec<Value>>;
    fn next(&mut self) -> Option<Self::Item> {
        self.rows.next()
    }
}

fn objects_to_rows(objs: Vec<Map<String, Value>>) -> (Vec<String>, Vec<Vec<Value>>) {
    let mut cols: Vec<String> = Vec::new();
    for o in &objs {
        for k in o.keys() {
            if !cols.contains(k) {
                cols.push(k.clone());
            }
        }
    }
    let rows = objs
        .into_iter()
        .map(|mut o| {
            cols.iter()
                .map(|c| match o.remove(c) {
                    Some(v @ (Value::Object(_) | Value::Array(_))) => Value::String(v.to_string()),
                    Some(v) => v,
                    None => Value::Null,
                })
                .collect()
        })
        .collect();
    (cols, rows)
}

pub fn open(path: &Path, format: ImportFormat, opts: &ImportOptions) -> io::Result<Reader> {
    let format = detect(path, format);
    match format {
        ImportFormat::Csv | ImportFormat::CsvSemicolon | ImportFormat::Tsv | ImportFormat::Auto => {
            let delim = opts.delimiter.bytes().next().unwrap_or(match format {
                ImportFormat::Tsv => b'\t',
                ImportFormat::CsvSemicolon => b';',
                _ => sniff_delimiter(path).unwrap_or(b','),
            });
            let rdr = csv::ReaderBuilder::new()
                .delimiter(delim)
                .has_headers(false)
                .flexible(true)
                .from_reader(BomSkip::new(File::open(path)?));
            let mut records = rdr.into_records();
            let first: Vec<String> = match records.next() {
                Some(r) => r.map_err(bad)?.iter().map(str::to_string).collect(),
                None => Vec::new(),
            };
            let (columns, pending) = if opts.header {
                (first, None)
            } else {
                ((1..=first.len()).map(|i| format!("columna{i}")).collect(), Some(first))
            };
            let width = columns.len();
            let pending = pending.map(|r| Ok(r.iter().map(|s| typed(s)).collect::<Vec<_>>()));
            let rest = records.map(move |r| {
                let r = r.map_err(bad)?;
                let mut v: Vec<Value> = r.iter().map(typed).collect();
                v.resize(width.max(v.len()), Value::Null);
                v.truncate(width);
                Ok(v)
            });
            Ok(Reader { columns, sheets: Vec::new(), format, rows: Box::new(pending.into_iter().chain(rest)) })
        }
        ImportFormat::Json => {
            let v: Value = serde_json::from_reader(BufReader::new(File::open(path)?)).map_err(bad)?;
            let arr = match v {
                Value::Array(a) => a,
                // { "rows": [...] } / { "data": [...] }: the first array inside.
                Value::Object(o) => o.into_iter().find_map(|(_, v)| v.as_array().cloned()).unwrap_or_default(),
                other => vec![other],
            };
            let objs: Vec<Map<String, Value>> = arr
                .into_iter()
                .map(|v| match v {
                    Value::Object(o) => o,
                    other => Map::from_iter([("valor".to_string(), other)]),
                })
                .collect();
            let (columns, rows) = objects_to_rows(objs);
            Ok(Reader { columns, sheets: Vec::new(), format, rows: Box::new(rows.into_iter().map(Ok)) })
        }
        ImportFormat::JsonLines => {
            // Columns are the union of the keys: read the whole file once.
            let mut objs = Vec::new();
            for line in BufReader::new(File::open(path)?).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line).map_err(bad)? {
                    Value::Object(o) => objs.push(o),
                    other => objs.push(Map::from_iter([("valor".to_string(), other)])),
                }
            }
            let (columns, rows) = objects_to_rows(objs);
            Ok(Reader { columns, sheets: Vec::new(), format, rows: Box::new(rows.into_iter().map(Ok)) })
        }
        ImportFormat::Xlsx => {
            use calamine::{open_workbook_auto, Data, Reader as _};
            let mut wb = open_workbook_auto(path).map_err(bad)?;
            let sheets = wb.sheet_names();
            let name = match opts.sheet.as_deref().filter(|s| !s.is_empty()) {
                Some(s) => s.to_string(),
                None => sheets.first().cloned().unwrap_or_default(),
            };
            let range = wb.worksheet_range(&name).map_err(bad)?;
            let cell = |d: &Data| -> Value {
                match d {
                    Data::Empty => Value::Null,
                    Data::Int(i) => dbine_driver::json_i64(*i),
                    Data::Float(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => dbine_driver::json_i64(*f as i64),
                    Data::Float(f) => dbine_driver::json_f64(*f),
                    Data::String(s) => typed(s),
                    Data::Bool(b) => Value::Bool(*b),
                    Data::DateTime(dt) => match dt.as_datetime() {
                        Some(t) if t.time() == chrono::NaiveTime::MIN => t.format("%Y-%m-%d").to_string().into(),
                        Some(t) => t.format("%Y-%m-%d %H:%M:%S").to_string().into(),
                        None => Value::Null,
                    },
                    Data::DateTimeIso(s) | Data::DurationIso(s) => Value::String(s.clone()),
                    Data::Error(e) => Value::String(format!("#{e:?}")),
                }
            };
            let mut all = range.rows();
            let first: Vec<Value> = all.next().map(|r| r.iter().map(cell).collect()).unwrap_or_default();
            let (columns, pending) = if opts.header {
                (first.iter().enumerate().map(|(i, v)| match v { Value::Null => format!("columna{}", i + 1), v => v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()) }).collect(), None)
            } else {
                ((1..=first.len()).map(|i| format!("columna{i}")).collect::<Vec<_>>(), Some(first))
            };
            let rows: Vec<Vec<Value>> = pending.into_iter().chain(all.map(|r| r.iter().map(cell).collect())).collect();
            Ok(Reader { columns, sheets, format, rows: Box::new(rows.into_iter().map(Ok)) })
        }
        ImportFormat::Xml => {
            let text = std::fs::read_to_string(path)?;
            let doc = roxmltree::Document::parse(&text).map_err(bad)?;
            let objs: Vec<Map<String, Value>> = doc
                .root_element()
                .children()
                .filter(|n| n.is_element())
                .map(|row| {
                    let mut m = Map::new();
                    // Attributes of the row element count as fields too.
                    for a in row.attributes() {
                        m.insert(a.name().to_string(), typed(a.value()));
                    }
                    for f in row.children().filter(|n| n.is_element()) {
                        let v = if f.attribute("null") == Some("true") { Value::Null } else { typed(f.text().unwrap_or("")) };
                        m.insert(f.tag_name().name().to_string(), v);
                    }
                    m
                })
                .collect();
            let (columns, rows) = objects_to_rows(objs);
            Ok(Reader { columns, sheets: Vec::new(), format, rows: Box::new(rows.into_iter().map(Ok)) })
        }
    }
}

/// `;` when the first line has more of them than commas (Spanish-locale CSV).
fn sniff_delimiter(path: &Path) -> Option<u8> {
    let mut line = String::new();
    BufReader::new(File::open(path).ok()?).read_line(&mut line).ok()?;
    let count = |c: char| line.matches(c).count();
    Some(if count(';') > count(',') { b';' } else if count('\t') > count(',') { b'\t' } else { b',' })
}

/// Drops a UTF-8 BOM at the start of the stream.
struct BomSkip<R> {
    inner: R,
    checked: bool,
}

impl<R: io::Read> BomSkip<R> {
    fn new(inner: R) -> Self {
        Self { inner, checked: false }
    }
}

impl<R: io::Read> io::Read for BomSkip<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if !self.checked {
            self.checked = true;
            if n >= 3 && buf[..3] == [0xEF, 0xBB, 0xBF] {
                buf.copy_within(3..n, 0);
                return Ok(n - 3);
            }
        }
        Ok(n)
    }
}

/// The type a column looks like from sample values.
pub fn infer_type(values: &[&Value]) -> &'static str {
    let vals: Vec<&&Value> = values.iter().filter(|v| !v.is_null()).collect();
    if vals.is_empty() {
        return "text";
    }
    let all = |f: &dyn Fn(&Value) -> bool| vals.iter().all(|v| f(v));
    if all(&|v| v.is_boolean()) {
        "boolean"
    } else if all(&|v| v.is_i64() || v.is_u64()) {
        "integer"
    } else if all(&|v| v.is_number()) {
        "number"
    } else if all(&|v| v.as_str().is_some_and(|s| s.len() == 10 && chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok())) {
        "date"
    } else if all(&|v| {
        v.as_str().is_some_and(|s| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").is_ok()
                || chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").is_ok()
                || chrono::DateTime::parse_from_rfc3339(s).is_ok()
        })
    }) {
        "datetime"
    } else {
        "text"
    }
}

/// The first rows and the inferred column types.
pub fn preview(path: &Path, format: ImportFormat, opts: &ImportOptions, limit: usize) -> io::Result<Preview> {
    let mut r = open(path, format, opts)?;
    let columns = r.columns.clone();
    let rows: Vec<Vec<Value>> = r.by_ref().take(limit.max(200)).collect::<io::Result<_>>()?;
    let cols = columns
        .iter()
        .enumerate()
        .map(|(i, name)| PreviewColumn {
            name: name.clone(),
            inferred_type: infer_type(&rows.iter().filter_map(|row| row.get(i)).collect::<Vec<_>>()),
        })
        .collect();
    Ok(Preview { format: r.format, columns: cols, rows: rows.into_iter().take(limit).collect(), sheets: r.sheets })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file(name: &str, content: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(name);
        std::fs::write(&p, content).unwrap();
        (d, p)
    }

    #[test]
    fn csv_types_bom_and_sniffed_semicolons() {
        let (_d, p) = file("a.csv", "\u{feff}id;nombre;monto;activo;cp\n1;Ana;10.5;true;01234\n2;;;false;5000\n".as_bytes());
        let pv = preview(&p, ImportFormat::Auto, &ImportOptions::default(), 50).unwrap();
        assert_eq!(pv.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "nombre", "monto", "activo", "cp"]);
        assert_eq!(pv.rows[0], vec![json!(1), json!("Ana"), json!(10.5), json!(true), json!("01234")]);
        assert_eq!(pv.rows[1][1], Value::Null);
        let types: Vec<&str> = pv.columns.iter().map(|c| c.inferred_type).collect();
        assert_eq!(types, ["integer", "text", "number", "boolean", "text"]);
    }

    #[test]
    fn json_and_lines_union_their_keys() {
        let (_d, p) = file("a.json", br#"[{"a":1,"b":{"x":1}},{"c":"2026-01-31"}]"#);
        let pv = preview(&p, ImportFormat::Auto, &ImportOptions::default(), 50).unwrap();
        assert_eq!(pv.columns.len(), 3);
        assert_eq!(pv.rows[0][1], json!("{\"x\":1}"), "nested values as JSON text");
        assert_eq!(pv.columns[2].inferred_type, "date");
        let (_d2, p2) = file("a.jsonl", b"{\"a\":1}\n\n{\"a\":2,\"b\":true}\n");
        let r = open(&p2, ImportFormat::Auto, &ImportOptions::default()).unwrap();
        assert_eq!(r.columns, ["a", "b"]);
        assert_eq!(r.count(), 2);
    }

    #[test]
    fn xml_as_exported() {
        let (_d, p) = file("a.xml", br#"<?xml version="1.0"?><rows><row><id>1</id><n null="true"/></row><row><id>2</id><n>x</n></row></rows>"#);
        let rows: Vec<_> = open(&p, ImportFormat::Auto, &ImportOptions::default()).unwrap().collect::<io::Result<_>>().unwrap();
        assert_eq!(rows, vec![vec![json!(1), Value::Null], vec![json!(2), json!("x")]]);
    }

    #[test]
    fn xlsx_round_trips_with_the_exporter() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.xlsx");
        let cols = vec![
            dbine_driver::ResultColumn { name: "id".into(), type_name: "int".into() },
            dbine_driver::ResultColumn { name: "nombre".into(), type_name: String::new() },
        ];
        crate::export::export_rows(&p, crate::export::ExportOptions { format: crate::export::Format::Xlsx, ..Default::default() }, &cols, &[vec![json!(7), json!("Ana")]]).unwrap();
        let pv = preview(&p, ImportFormat::Auto, &ImportOptions::default(), 50).unwrap();
        assert_eq!(pv.columns[0].name, "id");
        assert_eq!(pv.rows[0], vec![json!(7), json!("Ana")]);
        assert_eq!(pv.sheets, ["Resultado"]);
    }
}
