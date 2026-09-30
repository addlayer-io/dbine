//! CHECK constraints, INCLUDE columns, index options and full-text indexes
//! through the conversion: kept within a family, reported across engines.

use dbine_driver::{CheckDef, ColumnDef, IndexDef, KeyDef, TableSchema};
use dbine_schema::{convert, IssueCode, Options, Severity};

fn sample(kind: Option<&str>) -> TableSchema {
    let col = |n: &str, t: &str| ColumnDef { name: n.into(), data_type: t.into(), nullable: true, ..Default::default() };
    TableSchema {
        kind: "table".into(),
        name: "Clientes".into(),
        columns: vec![col("Id", "int"), col("Nombre", "varchar(50)"), col("Email", "varchar(80)")],
        primary_key: Some(KeyDef { name: None, columns: vec!["Id".into()] }),
        checks: vec![
            CheckDef { name: Some("CK_id".into()), expression: "([Id]>(0))".into() },
            CheckDef { name: None, expression: "([Nombre]<>'')".into() },
        ],
        indexes: vec![IndexDef {
            name: "IX_nombre".into(),
            columns: vec!["Nombre".into()],
            kind: kind.map(Into::into),
            include: vec!["Email".into()],
            options: [("fillfactor".to_string(), "80".to_string())].into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn same_family_keeps_everything() {
    let c = convert(&[sample(None)], "sqlserver", "azuresql", &Options::default()).unwrap();
    let t = &c.tables[0];
    assert_eq!(t.checks.len(), 2);
    assert_eq!(t.indexes[0].include, vec!["Email"]);
    assert_eq!(t.indexes[0].options.get("fillfactor").map(String::as_str), Some("80"));
    assert!(!c.issues.iter().any(|i| matches!(i.code, IssueCode::CheckDropped | IssueCode::OptionDropped)));
}

#[test]
fn cross_engine_drops_checks_with_a_warning_each() {
    let c = convert(&[sample(None)], "sqlserver", "mysql", &Options::default()).unwrap();
    assert!(c.tables[0].checks.is_empty());
    let w: Vec<_> = c.issues.iter().filter(|i| i.code == IssueCode::CheckDropped).collect();
    assert_eq!(w.len(), 2);
    assert!(w.iter().all(|i| i.severity == Severity::Warning));
    assert_eq!(
        w[0].message,
        "se quita la restricción CHECK «CK_id» (([Id]>(0))): su expresión es SQL de otro motor; revisala y creala a mano si hace falta"
    );
    assert!(w[1].message.contains("«([Nombre]<>'')» (([Nombre]<>''))"));
}

#[test]
fn cross_engine_drops_index_options_with_info() {
    let c = convert(&[sample(None)], "sqlserver", "mysql", &Options::default()).unwrap();
    assert!(c.tables[0].indexes[0].options.is_empty());
    let i = c.issues.iter().find(|i| i.code == IssueCode::OptionDropped && i.object.as_deref() == Some("IX_nombre")).unwrap();
    assert_eq!(i.severity, Severity::Info);
    assert_eq!(i.message, "se quitan las opciones del índice «IX_nombre»: fillfactor=80");
}

fn lower_sample(unique: bool) -> TableSchema {
    // Regular lower-case names refold to the target's case, like key columns.
    let mut t = sample(None);
    t.columns[2].name = "email".into();
    t.indexes[0].include = vec!["email".into()];
    t.indexes[0].unique = unique;
    t
}

#[test]
fn include_is_kept_where_the_target_has_it_with_renamed_columns() {
    for (to, expect) in [
        ("postgres", "email"),
        ("timescaledb", "email"),
        ("alloydb", "email"),
        ("yugabytedb", "email"),
        ("sqlserver", "email"),
        ("azuresql", "email"),
        ("babelfish", "email"),
        ("db2", "EMAIL"),
        ("db2zos", "EMAIL"),
    ] {
        // Db2 accepts INCLUDE on unique indexes only.
        let c = convert(&[lower_sample(true)], "mysql", to, &Options::default()).unwrap_or_else(|e| panic!("{to}: {e}"));
        assert_eq!(c.tables[0].indexes[0].include, vec![expect], "{to}");
        assert!(!c.issues.iter().any(|i| i.message.contains("pierde las columnas incluidas")), "{to}");
    }
}

#[test]
fn include_is_dropped_without_support_naming_the_engine() {
    let c = convert(&[sample(None)], "sqlserver", "mysql", &Options::default()).unwrap();
    assert!(c.tables[0].indexes[0].include.is_empty());
    let i = c.issues.iter().find(|i| i.message.contains("pierde las columnas incluidas")).unwrap();
    assert_eq!(i.severity, Severity::Warning);
    assert_eq!(
        i.message,
        "el índice «IX_nombre» pierde las columnas incluidas (Email): mysql no tiene INCLUDE; el índice sigue, sin esas columnas de cobertura"
    );
    // Same PostgreSQL / SQL Server dialects, engines without INCLUDE.
    for to in ["opengauss", "greengage", "greenplum", "h2", "cratedb", "fabric", "db2i"] {
        let c = convert(&[lower_sample(true)], "mysql", to, &Options::default()).unwrap_or_else(|e| panic!("{to}: {e}"));
        assert!(c.tables[0].indexes[0].include.is_empty(), "{to}");
        assert!(c.issues.iter().any(|i| i.message.contains("pierde las columnas incluidas") && i.message.contains(to)), "{to}");
    }
}

#[test]
fn db2_luw_and_zos_keep_include_only_on_unique_indexes() {
    for to in ["db2", "db2zos"] {
        let c = convert(&[lower_sample(false)], "mysql", to, &Options::default()).unwrap();
        assert!(c.tables[0].indexes[0].include.is_empty(), "{to}");
        assert!(c.issues.iter().any(|i| i.message.contains("no tiene INCLUDE en índices no únicos")), "{to}");
    }
}

#[test]
fn fulltext_indexes_are_dropped_across_engines_only() {
    let cross = convert(&[sample(Some("FULLTEXT"))], "mysql", "postgres", &Options::default()).unwrap();
    assert!(cross.tables[0].indexes.is_empty());
    let i = cross.issues.iter().find(|i| i.code == IssueCode::IndexDropped).unwrap();
    assert_eq!(i.severity, Severity::Warning);
    assert!(i.message.contains("«Clientes»") && i.message.contains("Nombre"));
    let same = convert(&[sample(Some("FULLTEXT"))], "mysql", "mysql", &Options::default()).unwrap();
    assert_eq!(same.tables[0].indexes.len(), 1);
}

#[test]
fn projections_and_exclusions_stay_in_their_engine() {
    for (from, to, kind) in [("clickhouse", "postgres", "PROJECTION"), ("postgres", "mysql", "EXCLUDE")] {
        let mut t = lower_sample(false);
        t.indexes[0].kind = Some(kind.into());
        let c = convert(&[t], from, to, &Options::default()).unwrap();
        assert!(c.tables[0].indexes.is_empty(), "{kind}");
        assert!(c.issues.iter().any(|i| i.message.contains(kind)), "{kind}: {:?}", c.issues);
    }
}
