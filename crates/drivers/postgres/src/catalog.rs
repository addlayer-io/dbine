//! Pure helpers for the catalog queries: schema filters, literals, type
//! names from `information_schema`.

use crate::Variant;
use tokio_postgres::{SimpleQueryMessage, SimpleQueryRow};

/// Schemas that belong to the server or an extension, on any variant.
const SYSTEM_SCHEMAS: &[&str] = &[
    "pg_catalog",
    "information_schema",
    // CockroachDB
    "crdb_internal",
    "pg_extension",
    // TimescaleDB
    "timescaledb_information",
    "timescaledb_experimental",
    "toolkit_experimental",
    // Greenplum
    "gp_toolkit",
    "pg_aoseg",
    "pg_bitmapindex",
    // Redshift
    "pg_internal",
    "pg_automv",
    "pg_auto_copy",
    "pg_mv",
    "pg_s3",
];

/// KingbaseES keeps its own system schemas next to PostgreSQL's.
const KINGBASE_SCHEMAS: &[&str] =
    &["sys", "sys_catalog", "sysaudit", "sysmac", "anon", "dbms_sql", "perf", "src_restrict", "xlog_record_read"];

/// The system schemas a variant adds to PostgreSQL's.
fn variant_schemas(v: Variant) -> &'static [&'static str] {
    match v {
        Variant::Kingbase => KINGBASE_SCHEMAS,
        Variant::Materialize => &["mz_catalog", "mz_internal", "mz_introspection", "mz_unsafe", "mz_catalog_unstable"],
        Variant::RisingWave => &["rw_catalog"],
        Variant::CrateDb => &["sys", "blob"],
        Variant::Edb => &["sys", "pgagent"],
        Variant::Yellowbrick => &["sys"],
        Variant::Cloudberry => &["pg_ext_aux"],
        Variant::H2 => &["INFORMATION_SCHEMA"],
        Variant::OpenGauss => &[
            "cstore",
            "pkg_service",
            "pkg_util",
            "dbe_perf",
            "dbe_pldebugger",
            "dbe_pldeveloper",
            "dbe_sql_util",
            "snapshot",
            "blockchain",
            "db4ai",
            "coverage",
            "xmltype",
            "sqladvisor",
        ],
        _ => &[],
    }
}

/// `col` isn't a system schema.
pub fn user_schema(v: Variant, col: &str) -> String {
    let mut names: Vec<&str> = SYSTEM_SCHEMAS.to_vec();
    names.extend_from_slice(variant_schemas(v));
    let list = names.iter().map(|n| format!("'{n}'")).collect::<Vec<_>>().join(", ");
    format!(
        "{col} NOT IN ({list}) AND {col} NOT LIKE 'pg\\_toast%' AND {col} NOT LIKE 'pg\\_temp\\_%' \
         AND {col} NOT LIKE '\\_timescaledb%'"
    )
}

/// `name` is one of the server's or an extension's schemas: the ones
/// [`user_schema`] leaves out, plus each engine's internal families
/// (`mz_*` on Materialize, `_timescaledb*` anywhere).
pub fn system_schema(v: Variant, name: &str) -> bool {
    SYSTEM_SCHEMAS.contains(&name)
        || variant_schemas(v).contains(&name)
        || ["pg_toast", "pg_temp_", "_timescaledb"].iter().any(|p| name.starts_with(p))
        || (v == Variant::Materialize && name.starts_with("mz_"))
}

/// Text for a `--` comment line: a server-controlled name can't end the
/// comment and turn the rest of the line into a statement. Line breaks
/// (CR, LF, NEL, U+2028/U+2029) and other control characters become `?`.
pub(crate) fn comment_text(s: &str) -> String {
    s.chars().map(|c| if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') { '?' } else { c }).collect()
}

/// A string literal for a text-protocol query. Redshift treats backslashes
/// in literals as escapes, so they are doubled there.
pub fn lit(v: Variant, s: &str) -> String {
    let quoted = s.replace('\'', "''");
    match v {
        // Its strings always read backslash escapes.
        Variant::Redshift => format!("'{}'", quoted.replace('\\', "\\\\")),
        // Engines where `standard_conforming_strings` can be off (a server,
        // database or role setting): an `E'…'` string reads backslashes the
        // same way whatever it says.
        Variant::Postgres
        | Variant::Cockroach
        | Variant::Greenplum
        | Variant::Yugabyte
        | Variant::Timescale
        | Variant::Kingbase
        | Variant::AlloyDb
        | Variant::CloudSql
        | Variant::Aurora
        | Variant::Edb
        | Variant::Fujitsu
        | Variant::OpenGauss
        | Variant::Cloudberry
        | Variant::Greengage
        | Variant::Yellowbrick => format!("E'{}'", quoted.replace('\\', "\\\\")),
        // Standard strings only: a backslash is a plain character.
        _ => format!("'{quoted}'"),
    }
}

/// The rows of a text-protocol result.
pub fn rows(msgs: Vec<SimpleQueryMessage>) -> Vec<SimpleQueryRow> {
    msgs.into_iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some(r),
            _ => None,
        })
        .collect()
}

/// The first cell of the first row.
pub fn first_cell(msgs: &[SimpleQueryMessage]) -> Option<String> {
    msgs.iter().find_map(|m| match m {
        SimpleQueryMessage::Row(r) => r.get(0).map(str::to_string),
        _ => None,
    })
}

/// A cell by column name (text protocol), `None` when null or missing.
pub fn cell(r: &SimpleQueryRow, name: &str) -> Option<String> {
    r.try_get(name).ok().flatten().map(str::to_string)
}

/// `character varying(50)`, `numeric(18,2)` from `information_schema.columns`.
pub fn info_type(data_type: &str, char_len: Option<&str>, precision: Option<&str>, scale: Option<&str>) -> String {
    match data_type {
        "character varying" | "character" | "varchar" | "char" | "bpchar" | "nvarchar" | "nchar" => match char_len {
            Some(n) if !n.is_empty() => format!("{data_type}({n})"),
            _ => data_type.to_string(),
        },
        "numeric" | "decimal" => match (precision, scale) {
            (Some(p), Some(s)) if !p.is_empty() => format!("{data_type}({p},{s})"),
            _ => data_type.to_string(),
        },
        _ => data_type.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_escape_quotes_and_backslashes() {
        // E'…' reads the same with standard_conforming_strings on or off.
        assert_eq!(lit(Variant::Postgres, "o'k\\"), "E'o''k\\\\'");
        assert_eq!(lit(Variant::Postgres, "x\\'; drop table t; --"), "E'x\\\\''; drop table t; --'");
        assert_eq!(lit(Variant::Redshift, "o'k\\"), "'o''k\\\\'");
        assert_eq!(lit(Variant::Materialize, "o'k\\"), "'o''k\\'");
    }

    #[test]
    fn schema_filter_adds_kingbase_schemas_only_there() {
        assert!(!user_schema(Variant::Postgres, "n.nspname").contains("'sys_catalog'"));
        assert!(user_schema(Variant::Kingbase, "n.nspname").contains("'sys_catalog'"));
        assert!(user_schema(Variant::Postgres, "x").starts_with("x NOT IN ('pg_catalog'"));
    }

    #[test]
    fn system_schemas_by_engine() {
        for name in ["pg_catalog", "information_schema", "pg_toast", "pg_toast_temp_3", "pg_temp_3", "crdb_internal", "pg_extension", "_timescaledb_catalog", "timescaledb_information"] {
            assert!(system_schema(Variant::Postgres, name), "{name}");
        }
        for name in ["public", "ventas", "pg_ventas", "mz_ventas", "sys"] {
            assert!(!system_schema(Variant::Postgres, name), "{name}");
        }
        assert!(system_schema(Variant::Materialize, "mz_catalog_unstable") && system_schema(Variant::Materialize, "mz_whatever"));
        assert!(system_schema(Variant::RisingWave, "rw_catalog") && system_schema(Variant::Kingbase, "sys_catalog"));
        assert!(system_schema(Variant::OpenGauss, "dbe_perf") && system_schema(Variant::H2, "INFORMATION_SCHEMA"));
        assert!(!system_schema(Variant::H2, "PUBLIC") && !system_schema(Variant::CrateDb, "doc"));
    }

    #[test]
    fn info_types_carry_length_and_precision() {
        assert_eq!(info_type("character varying", Some("50"), None, None), "character varying(50)");
        assert_eq!(info_type("numeric", None, Some("18"), Some("2")), "numeric(18,2)");
        assert_eq!(info_type("numeric", None, None, None), "numeric");
        assert_eq!(info_type("integer", None, Some("32"), Some("0")), "integer");
    }
}
