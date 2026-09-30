//! The original's triggers. They're objects of their own that name the
//! table in their body (and often a sequence or generator the original
//! uses): the clone doesn't get them, and the report says which ones, so a
//! classic auto-increment (a generator plus a BEFORE INSERT trigger) never
//! surprises on the clone's first insert.

use super::strings;
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{Driver, Session, TableSchema};

/// The note for the report, when the original has triggers (best effort:
/// engines DBine knows how to ask; nothing when the question fails).
pub(super) async fn note(driver: &dyn Driver, s: &mut dyn Session, t: &TableSchema) -> Option<String> {
    let names = triggers_of(driver, s, t).await;
    (!names.is_empty()).then(|| {
        format!(
            "los triggers de la tabla original no se clonan ({}): el clon no los tiene. Si alguno completa valores al insertar \
             (un autoincremental con generador, por ejemplo), las filas nuevas del clon tienen que traerlos, o creale al clon sus propios triggers",
            names.join(", ")
        )
    })
}

/// The catalog query for `t`'s triggers, on engines that have them.
fn query(driver: &dyn Driver, t: &TableSchema) -> Option<String> {
    let info = driver.info();
    let lit = |s: &str| s.replace('\'', "''");
    let name = lit(&t.name);
    let schema = t.schema.as_deref().filter(|s| !s.is_empty());
    let and_schema = |col: &str| schema.map(|s| format!(" AND {col} = '{}'", lit(s))).unwrap_or_default();
    Some(match (info.id, info.dialect) {
        ("sqlite" | "libsql", _) => format!(
            "SELECT name FROM {}sqlite_master WHERE type = 'trigger' AND tbl_name = '{name}'",
            schema.map(|s| format!("{}.", qualified_name(Quote::Double, None, s))).unwrap_or_default()
        ),
        ("firebird", _) => {
            format!("SELECT TRIM(RDB$TRIGGER_NAME) FROM RDB$TRIGGERS WHERE RDB$RELATION_NAME = '{name}' AND COALESCE(RDB$SYSTEM_FLAG, 0) = 0")
        }
        (_, "postgres") => format!(
            "SELECT tgname FROM pg_trigger WHERE NOT tgisinternal AND tgrelid = '{}'::regclass",
            lit(&qualified_name(Quote::Double, schema, &t.name))
        ),
        (_, "mysql") => format!(
            "SELECT trigger_name FROM information_schema.triggers WHERE event_object_table = '{name}'{}",
            if schema.is_some() { and_schema("event_object_schema") } else { " AND event_object_schema = DATABASE()".into() }
        ),
        (_, "mssql") => format!("SELECT name FROM sys.triggers WHERE parent_id = OBJECT_ID(N'{}')", lit(&qualified_name(Quote::Bracket, schema, &t.name))),
        (_, "oracle") => format!("SELECT trigger_name FROM all_triggers WHERE table_name = '{name}'{}", and_schema("table_owner")),
        (_, "db2") => format!("SELECT TRIM(trigname) FROM syscat.triggers WHERE tabname = '{name}'{}", and_schema("tabschema")),
        _ => return None,
    })
}

async fn triggers_of(driver: &dyn Driver, s: &mut dyn Session, t: &TableSchema) -> Vec<String> {
    let Some(sql) = query(driver, t) else { return Vec::new() };
    strings(s, &sql).await.map(|r| r.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{async_trait, ConnectionConfig, DriverInfo, Error, Family, Language, Result};

    struct D(DriverInfo);

    #[async_trait]
    impl Driver for D {
        fn info(&self) -> &DriverInfo {
            &self.0
        }
        async fn connect(&self, _: &ConnectionConfig, _: Option<&str>) -> Result<Box<dyn Session>> {
            Err(Error::Unsupported("test".into()))
        }
    }

    fn d(id: &'static str, dialect: &'static str) -> D {
        D(DriverInfo {
            id,
            name: id,
            family: Family::Relational,
            language: Language::Sql,
            dialect,
            default_port: 0,
            fields: vec![],
            databases_label: "",
            has_schemas: true,
            object_kinds: vec![],
        })
    }

    #[test]
    fn asks_each_engine_its_way() {
        let t = TableSchema { kind: "table".into(), schema: Some("app".into()), name: "o'k".into(), ..Default::default() };
        let q = |id, dialect| query(&d(id, dialect), &t).unwrap_or_default();
        assert!(q("firebird", "standard").contains("RDB$RELATION_NAME = 'o''k'"));
        assert!(q("sqlite", "sqlite").starts_with("SELECT name FROM \"app\".sqlite_master"));
        assert!(q("postgres", "postgres").contains("'\"app\".\"o''k\"'::regclass"));
        assert!(q("sqlserver", "mssql").contains("OBJECT_ID(N'[app].[o''k]')"));
        assert!(q("mysql", "mysql").contains("event_object_schema = 'app'"));
        // DuckDB has no triggers.
        assert!(query(&d("duckdb", "standard"), &t).is_none());
    }
}
