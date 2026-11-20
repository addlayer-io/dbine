use super::*;
use crate::meta::fake::{rowset, Fake};
use crate::HIERARCHY;
use dbine_driver::ObjectRef;
use serde_json::json;

const LINEAGE: &str = include_str!("../../tests/fixtures/tmsl_lineage.json");

fn target(kind: &str, name: &str, column: Option<&str>) -> DependencyTarget {
    DependencyTarget {
        object: ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        },
        column: column.map(str::to_string),
    }
}

/// The DBA's model through the definition tier (write permission, no
/// capacity: no `INFO.CALCDEPENDENCY`).
fn lineage() -> Fake {
    let mut f = Fake::writer();
    f.definition(LINEAGE);
    f
}

async fn refs(f: &mut Fake, kind: &str, name: &str) -> DependencyReport {
    references_in(f, &target(kind, name, None))
        .await
        .unwrap()
        .unwrap()
}

fn sql_server(schema: &str, name: &str) -> ExternalRef {
    ExternalRef {
        system: Some("SQL Server".into()),
        server: Some("sql.contoso.com".into()),
        database: Some("dw".into()),
        schema: Some(schema.into()),
        name: Some(name.into()),
        ..Default::default()
    }
}

fn without_mentions(x: &[ExternalRef]) -> Vec<ExternalRef> {
    x.iter()
        .map(|e| ExternalRef {
            mentions: Vec::new(),
            ..e.clone()
        })
        .collect()
}

#[tokio::test]
async fn a_tables_partition_reads_its_sql_server_table() {
    let mut f = lineage();
    let r = refs(&mut f, TABLE, "pbi medidas").await;
    // The DBA's case: the partition reads indicadores_directorio.medidas,
    // the server through the Servidor parameter.
    assert_eq!(
        without_mentions(&r.external),
        [sql_server("indicadores_directorio", "medidas")]
    );
    let lines: Vec<u32> = r.external[0].mentions.iter().map(|m| m.line).collect();
    assert_eq!(lines, [2, 3], "{:?}", r.external[0].mentions);
    let got: Vec<(&str, &str, Relation, Confidence)> = r
        .items
        .iter()
        .map(|d| (d.kind.as_str(), d.name.as_str(), d.relation, d.confidence))
        .collect();
    assert_eq!(
        got,
        [
            (
                TABLE,
                "Indicadores",
                Relation::ForeignKey,
                Confidence::Confirmed
            ),
            (TABLE, "Indicadores", Relation::Code, Confidence::Probable),
            (EXPRESSION, "Servidor", Relation::Code, Confidence::Probable),
        ]
    );
    // The calculated column's column, and the relationship.
    assert_eq!(r.items[1].detail.as_deref(), Some("'Indicadores'[nombre]"));
    assert_eq!(
        r.items[0].detail.as_deref(),
        Some("'pbi medidas'[indicador_id] → 'Indicadores'[id]")
    );
    assert_eq!(
        r.items[1].mentions[0].text,
        "RELATED ( 'Indicadores'[nombre] )"
    );
    assert!(r.note.is_none());
    // The same table from Databricks.
    let r = refs(&mut f, TABLE, "pbi medidas lake").await;
    assert_eq!(
        without_mentions(&r.external),
        [ExternalRef {
            system: Some("Databricks".into()),
            server: Some("adb-1234567890123456.7.azuredatabricks.net".into()),
            database: Some("hive_metastore".into()),
            schema: Some("indicadores_directorio".into()),
            name: Some("medidas".into()),
            detail: Some("/sql/1.0/warehouses/abc123".into()),
            mentions: Vec::new(),
        }]
    );
    assert!(r.items.is_empty(), "{:?}", r.items);
}

#[tokio::test]
async fn shared_expressions_and_native_queries() {
    let mut f = lineage();
    // A partition that starts from the shared DW query.
    let r = refs(&mut f, TABLE, "Indicadores").await;
    assert_eq!(
        without_mentions(&r.external),
        [sql_server("dbo", "indicadores")]
    );
    let got: Vec<(&str, &str)> = r
        .items
        .iter()
        .map(|d| (d.kind.as_str(), d.name.as_str()))
        .collect();
    assert_eq!(got, [(EXPRESSION, "DW")]);
    // A native query over two tables: their names in the detail.
    let r = refs(&mut f, TABLE, "Metas").await;
    assert_eq!(
        without_mentions(&r.external),
        [ExternalRef {
            schema: None,
            name: None,
            detail: Some("indicadores_directorio.metas, indicadores_directorio.medidas".into()),
            ..sql_server("", "")
        }]
    );
    // The shared expression itself: its source and the parameter.
    let r = refs(&mut f, EXPRESSION, "DW").await;
    assert_eq!(
        without_mentions(&r.external),
        [ExternalRef {
            schema: None,
            name: None,
            ..sql_server("", "")
        }]
    );
    assert_eq!(r.items.len(), 1);
    assert_eq!(
        (r.items[0].kind.as_str(), r.items[0].name.as_str()),
        (EXPRESSION, "Servidor")
    );
    // A parameter reads nothing.
    let r = refs(&mut f, EXPRESSION, "Servidor").await;
    assert!(r.items.is_empty() && r.external.is_empty());
}

#[tokio::test]
async fn measures_and_roles_by_their_dax() {
    let mut f = lineage();
    let r = refs(&mut f, MEASURE, "Valor promedio").await;
    let got: Vec<(&str, &str, Option<&str>, Confidence)> = r
        .items
        .iter()
        .map(|d| {
            (
                d.kind.as_str(),
                d.name.as_str(),
                d.parent.as_deref(),
                d.confidence,
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (TABLE, "pbi medidas", None, Confidence::Probable),
            (
                MEASURE,
                "Total valor",
                Some("pbi medidas"),
                Confidence::Probable
            ),
        ]
    );
    assert_eq!(r.items[1].mentions[0].text, "[Total valor],");
    assert!(r.external.is_empty());
    let r = refs(&mut f, MEASURE, "Total valor").await;
    assert_eq!(r.items[0].detail.as_deref(), Some("'pbi medidas'[valor]"));
    // An RLS filter's bare column is the filtered table's.
    let r = refs(&mut f, ROLE, "Directorio").await;
    assert_eq!(r.items.len(), 1);
    assert_eq!(r.items[0].name, "pbi medidas");
    assert_eq!(r.items[0].detail.as_deref(), Some("'pbi medidas'[area]"));
    // A column: its relationship and, if calculated, its DAX.
    let r = references_in(&mut f, &target(TABLE, "pbi medidas", Some("Indicador")))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.items.len(), 1);
    assert_eq!(r.items[0].detail.as_deref(), Some("'Indicadores'[nombre]"));
    assert!(r.external.is_empty());
    let r = references_in(&mut f, &target(TABLE, "pbi medidas", Some("indicador_id")))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.items.len(), 1);
    assert_eq!(r.items[0].relation, Relation::ForeignKey);
    // Hierarchies, relationships and perspectives: the generic scan.
    assert!(references_in(&mut f, &target(HIERARCHY, "x", None))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn the_statistics_tier_says_why_there_is_nothing() {
    let mut f = Fake::new();
    f.dax_answer(
        "COLUMNSTATISTICS()",
        vec![rowset(
            &[
                "[Table Name]",
                "[Column Name]",
                "[Min]",
                "[Max]",
                "[Cardinality]",
                "[Max Length]",
            ],
            vec![],
        )],
    );
    let r = refs(&mut f, TABLE, "Sales").await;
    assert!(r.items.is_empty() && r.external.is_empty());
    assert_eq!(r.note.as_deref(), Some(NO_METADATA));
}

fn calc_rows(rows: Vec<serde_json::Value>) -> crate::query::Rowset {
    rowset(
        &[
            "[DATABASE_NAME]",
            "[OBJECT_TYPE]",
            "[TABLE]",
            "[OBJECT]",
            "[EXPRESSION]",
            "[REFERENCED_OBJECT_TYPE]",
            "[REFERENCED_TABLE]",
            "[REFERENCED_OBJECT]",
        ],
        rows,
    )
}

/// The INFO tier, a writer on capacity.
fn info_writer() -> Fake {
    let mut f = Fake::writer();
    f.arrow = true;
    crate::meta::info::tests::viewer_answers(&mut f);
    crate::meta::info::tests::admin_answers(&mut f);
    f
}

#[tokio::test]
async fn writers_on_capacity_get_the_engines_dependencies() {
    let mut f = info_writer();
    f.dax_answer(
        "INFO.CALCDEPENDENCY()",
        vec![calc_rows(vec![
            json!([
                "db",
                "MEASURE",
                "Sales",
                "Total Sales",
                "SUM(Sales[Amount])",
                "COLUMN",
                "Sales",
                "Amount"
            ]),
            json!([
                "db",
                "MEASURE",
                "Sales",
                "Total Sales",
                "SUM(Sales[Amount])",
                "TABLE",
                "Customer",
                "Customer"
            ]),
            // Another object's row: left out.
            json!([
                "db",
                "MEASURE",
                "Sales",
                "Other",
                "SUM(Sales[Cost])",
                "COLUMN",
                "Customer",
                "Cost"
            ]),
        ])],
    );
    let r = refs(&mut f, MEASURE, "Total Sales").await;
    assert!(
        f.dax_log.contains(
            &"EVALUATE FILTER(INFO.CALCDEPENDENCY(), [OBJECT_TYPE] = \"MEASURE\" && [OBJECT] = \"Total Sales\")"
                .to_string()
        ),
        "{:?}",
        f.dax_log
    );
    let got: Vec<(&str, &str, Option<&str>, Confidence)> = r
        .items
        .iter()
        .map(|d| {
            (
                d.kind.as_str(),
                d.name.as_str(),
                d.detail.as_deref(),
                d.confidence,
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (TABLE, "Customer", None, Confidence::Confirmed),
            (
                TABLE,
                "Sales",
                Some("'Sales'[Amount]"),
                Confidence::Confirmed
            ),
        ]
    );
    assert!(r.note.is_none());
}

#[tokio::test]
async fn without_the_engines_dependencies_hidden_dax_is_said() {
    let mut f = info_writer();
    f.dax_refuse("INFO.CALCDEPENDENCY()");
    // INFO.VIEW hides the measure's DAX: nothing to read, and the note says why.
    let r = refs(&mut f, MEASURE, "Total Sales").await;
    assert!(r.items.is_empty());
    assert_eq!(r.note.as_deref(), Some(NO_EXPRESSIONS));
    assert_eq!(r.unreadable, ["'Sales'[Total Sales]"]);
    // The table's partition (INFO.PARTITIONS) still gives its source.
    let r = refs(&mut f, TABLE, "Sales").await;
    assert_eq!(
        without_mentions(&r.external),
        [ExternalRef {
            schema: None,
            name: None,
            ..sql_server("", "")
        }]
    );
}

#[test]
fn the_filters_sent_per_kind() {
    let q = |kind: &str, name: &str, column: Option<&str>| calc_query(&target(kind, name, column));
    assert_eq!(
        q(TABLE, "pbi \"medidas\"", None).as_deref(),
        Some("EVALUATE FILTER(INFO.CALCDEPENDENCY(), ([TABLE] = \"pbi \"\"medidas\"\"\" && [OBJECT_TYPE] IN {\"CALC_COLUMN\", \"CALC_TABLE\"}) || ([OBJECT] = \"pbi \"\"medidas\"\"\" && [OBJECT_TYPE] = \"CALC_TABLE\"))")
    );
    assert_eq!(
        q(TABLE, "Sales", Some("Margin")).as_deref(),
        Some("EVALUATE FILTER(INFO.CALCDEPENDENCY(), [OBJECT_TYPE] = \"CALC_COLUMN\" && [TABLE] = \"Sales\" && [OBJECT] = \"Margin\")")
    );
    assert_eq!(
        q(CALCULATION_GROUP, "Time Intelligence", None).as_deref(),
        Some("EVALUATE FILTER(INFO.CALCDEPENDENCY(), [OBJECT_TYPE] = \"CALCULATION_ITEM\" && [TABLE] = \"Time Intelligence\")")
    );
    assert_eq!(
        q(ROLE, "West", None).as_deref(),
        Some("EVALUATE FILTER(INFO.CALCDEPENDENCY(), [OBJECT_TYPE] = \"ROWS_ALLOWED\" && [OBJECT] = \"West\")")
    );
    assert_eq!(
        q(FUNCTION, "AddTax", None).as_deref(),
        Some("EVALUATE FILTER(INFO.CALCDEPENDENCY(), CONTAINSSTRING([OBJECT_TYPE], \"FUNCTION\") && [OBJECT] = \"AddTax\")")
    );
    assert!(q(EXPRESSION, "DW", None).is_none());
}

#[test]
fn calculated_items_and_columns_from_the_sample_model() {
    let meta = crate::objects::tests::sample();
    // A calculated column's bare [Amount] is its own table's; RELATED's is Customer's.
    let uses = dax_uses("[Amount] - RELATED('Customer'[Cost])", Some("Sales"), &meta);
    let got: Vec<Use> = uses.into_iter().map(|(u, _)| u).collect();
    assert_eq!(
        got,
        [
            Use::Column("Sales".into(), "Amount".into()),
            Use::Column("Customer".into(), "Cost".into()),
        ]
    );
    // Measures bare or qualified, functions called, strings and comments never.
    let got: Vec<Use> = dax_uses(
        "AddTax([Total Sales]) + 'Sales'[Total Sales] // Customer\n+ \"Sales\"",
        None,
        &meta,
    )
    .into_iter()
    .map(|(u, _)| u)
    .collect();
    assert_eq!(
        got,
        [
            Use::Function("AddTax".into()),
            Use::Measure("Total Sales".into()),
            Use::Measure("Total Sales".into()),
        ]
    );
}
