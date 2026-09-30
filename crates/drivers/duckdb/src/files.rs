//! "Archivos CSV / Parquet / JSON": a folder opened as a database. DuckDB
//! runs in memory and every data file in the folder becomes a view over
//! `read_csv_auto` / `read_parquet` / `read_json_auto`, so the files are
//! read in place (a query always sees their current contents) and never
//! copied. The folder is also the `file_search_path`, so `FROM 'otro.csv'`
//! works in queries.

use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{kinds, DriverInfo, Family, Field, FieldKind, Language, ObjectKindInfo};
use std::path::{Path, PathBuf};

/// Files per folder turned into views (the rest can be queried by path).
const MAX_FILES: usize = 500;

pub fn info() -> DriverInfo {
    DriverInfo {
        id: "duckdb_files",
        name: "Archivos CSV / Parquet / JSON",
        family: Family::Analytical,
        language: Language::Sql,
        dialect: "standard",
        default_port: 0,
        fields: vec![
            Field::new("host", "Carpeta", FieldKind::File)
                .required()
                .placeholder("/ruta/a/los/archivos")
                .help(
                    "La carpeta con los archivos (podés elegir cualquier archivo de adentro). Cada .csv, .tsv, \
                     .parquet, .json, .jsonl o .ndjson (también comprimidos con .gz) queda como una vista.",
                ),
            Field::new("recursive", "Incluir subcarpetas", FieldKind::Bool)
                .help("Las vistas de las subcarpetas se llaman carpeta/archivo.")
                .advanced(),
            Field::read_only(),
        ],
        databases_label: "",
        has_schemas: true,
        object_kinds: vec![
            ObjectKindInfo::new(kinds::VIEW, "Archivos", true, true, true),
            ObjectKindInfo::tables(),
            ObjectKindInfo::new(kinds::FUNCTION, "Macros", false, false, true),
        ],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    Tsv,
    Parquet,
    Json,
}

/// The format of a data file by its extension (`.csv.gz` counts as CSV).
pub fn format_of(path: &Path) -> Option<(Format, String)> {
    let name = path.file_name()?.to_str()?;
    let lower = name.to_ascii_lowercase();
    let base = lower.strip_suffix(".gz").or_else(|| lower.strip_suffix(".zst")).unwrap_or(&lower);
    let (stem_len, format) = [
        (".csv", Format::Csv),
        (".tsv", Format::Tsv),
        (".parquet", Format::Parquet),
        (".json", Format::Json),
        (".jsonl", Format::Json),
        (".ndjson", Format::Json),
    ]
    .iter()
    .find(|(ext, _)| base.ends_with(ext))
    .map(|(ext, f)| (base.len() - ext.len(), *f))?;
    (stem_len > 0).then(|| (format, name[..stem_len].to_string()))
}

/// The folder to open: the path itself, or the folder of a picked file.
pub fn folder(path: &str) -> PathBuf {
    let p = Path::new(path.trim());
    if p.is_file() {
        p.parent().map(Path::to_path_buf).unwrap_or_default()
    } else {
        p.to_path_buf()
    }
}

/// (view name, file, format) for every data file, sorted; a name taken
/// twice (a.csv and a.parquet) gets its format appended.
pub fn scan(dir: &Path, recursive: bool) -> std::io::Result<Vec<(String, PathBuf, Format)>> {
    fn walk(dir: &Path, prefix: &str, recursive: bool, out: &mut Vec<(String, PathBuf, Format)>) -> std::io::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if out.len() >= MAX_FILES {
                break;
            }
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                if recursive {
                    walk(&path, &format!("{prefix}{name}/"), recursive, out)?;
                }
            } else if let Some((format, stem)) = format_of(&path) {
                out.push((format!("{prefix}{stem}"), path, format));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, "", recursive, &mut out)?;
    let names: Vec<String> = out.iter().map(|(n, _, _)| n.to_lowercase()).collect();
    for (i, (name, _, format)) in out.iter_mut().enumerate() {
        if names.iter().filter(|n| **n == names[i]).count() > 1 {
            name.push_str(match format {
                Format::Csv => "_csv",
                Format::Tsv => "_tsv",
                Format::Parquet => "_parquet",
                Format::Json => "_json",
            });
        }
    }
    Ok(out)
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `CREATE OR REPLACE VIEW` over the file.
pub fn view_sql(name: &str, path: &Path, format: Format) -> String {
    let p = lit(&path.to_string_lossy());
    let reader = match format {
        Format::Csv => format!("read_csv_auto({p})"),
        Format::Tsv => format!("read_csv_auto({p}, delim = '\\t')"),
        Format::Parquet => format!("read_parquet({p})"),
        Format::Json => format!("read_json_auto({p})"),
    };
    format!("CREATE OR REPLACE VIEW {} AS SELECT * FROM {reader}", quote_ident(Quote::Double, name))
}

/// Settings for a session on the folder.
pub fn session_sql(dir: &Path) -> String {
    format!("SET file_search_path = {}", lit(&dir.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_names() {
        assert_eq!(format_of(Path::new("/d/ventas.csv")), Some((Format::Csv, "ventas".into())));
        assert_eq!(format_of(Path::new("/d/Ventas.CSV.gz")), Some((Format::Csv, "Ventas".into())));
        assert_eq!(format_of(Path::new("/d/x.parquet")), Some((Format::Parquet, "x".into())));
        assert_eq!(format_of(Path::new("/d/log.ndjson")), Some((Format::Json, "log".into())));
        assert_eq!(format_of(Path::new("/d/notas.txt")), None);
        assert_eq!(format_of(Path::new("/d/.csv")), None);
    }

    #[test]
    fn scanning_a_folder() {
        let dir = std::env::temp_dir().join(format!("dbine-files-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        for f in ["a.csv", "a.parquet", "b.json", "leeme.txt", "sub/c.tsv", ".oculto.csv"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        let names = |r: bool| scan(&dir, r).unwrap().into_iter().map(|(n, _, _)| n).collect::<Vec<_>>();
        assert_eq!(names(false), ["a_csv", "a_parquet", "b"]);
        assert_eq!(names(true), ["a_csv", "a_parquet", "b", "sub/c"]);
        assert_eq!(folder(&dir.join("b.json").to_string_lossy()), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn views_quote_names_and_paths() {
        let s = view_sql("sub/o'k", Path::new("/d/it's.tsv"), Format::Tsv);
        assert_eq!(s, "CREATE OR REPLACE VIEW \"sub/o'k\" AS SELECT * FROM read_csv_auto('/d/it''s.tsv', delim = '\\t')");
    }
}
