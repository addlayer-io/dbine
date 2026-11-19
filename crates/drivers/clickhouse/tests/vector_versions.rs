//! Vector indexes on the ClickHouse versions around the index's changes:
//! 25.3 LTS (24.8–25.7: experimental, no dimension, behind
//! `allow_experimental_vector_similarity_index`) and 25.8 (the dimension
//! form DBine writes; `dotProduct` only from 26.4).
//!
//! ```sh
//! docker run -d --name dbine-test-clickhouse-25-3 -p 25125:8123 -e CLICKHOUSE_USER=dbine \
//!   -e CLICKHOUSE_PASSWORD=dbine clickhouse/clickhouse-server:25.3
//! docker run -d --name dbine-test-clickhouse-25-8 -p 25126:8123 -e CLICKHOUSE_USER=dbine \
//!   -e CLICKHOUSE_PASSWORD=dbine clickhouse/clickhouse-server:25.8
//! DBINE_TEST_CLICKHOUSE_25_3_URL=http://dbine:dbine@localhost:25125 \
//! DBINE_TEST_CLICKHOUSE_25_8_URL=http://dbine:dbine@localhost:25126 \
//!   cargo test -p dbine-driver-clickhouse --test vector_versions -- --ignored
//! ```
//!
//! Each test's database is dropped first and at the end, even when an
//! assertion fails (the body runs in a task of its own).

use dbine_driver::{
    kinds, ColumnDef, ConnectionConfig, DdlParts, Driver, IndexDef, KeyDef, ObjectRef,
    QueryOutcome, Session, TableChange, TableSchema, VectorIndex,
};
use serde_json::json;
use std::sync::Arc;

fn cfg(var: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(var).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "clickhouse".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_clickhouse::drivers()
        .into_iter()
        .find(|d| d.info().id == "clickhouse")
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}\n{e}"));
    out
}

async fn fails(s: &mut Box<dyn Session>, sql: &str) -> String {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out)
        .await
        .expect_err(sql)
        .to_string()
}

fn table(db: &str, name: &str) -> ObjectRef {
    ObjectRef {
        kind: kinds::TABLE.into(),
        schema: Some(db.into()),
        name: name.into(),
    }
}

fn col(name: &str, ty: &str) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable: false,
        ..Default::default()
    }
}

/// The designer's table: `emb` with a vector index of `metric`, in the
/// form DBine writes (with the dimension).
fn designed(db: &str, name: &str, ty: &str, metric: &str) -> TableSchema {
    TableSchema {
        kind: kinds::TABLE.into(),
        schema: Some(db.into()),
        name: name.into(),
        columns: vec![col("id", "UInt64"), col("emb", ty)],
        primary_key: Some(KeyDef {
            name: None,
            columns: vec!["id".into()],
        }),
        indexes: vec![IndexDef {
            name: "emb_ix".into(),
            columns: vec!["emb".into()],
            vector: Some(VectorIndex {
                algorithm: Some("hnsw".into()),
                metric: Some(metric.into()),
                params: [("dimensions".to_string(), "3".to_string())].into(),
            }),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn create(d: &Arc<dyn Driver>, t: &TableSchema) -> String {
    d.table_ddl(
        t,
        DdlParts {
            create: true,
            indexes: true,
            ..Default::default()
        },
    )
    .unwrap()
}

/// 25.3: an index of its own form (no dimension) is read with its settings,
/// written back in that form (the setting first) for a clone and a sync,
/// and searched; the Índices view carries it. The form DBine writes for a
/// new index says it needs 25.8.
#[tokio::test]
#[ignore]
async fn clickhouse_25_3_experimental_indexes() {
    let Some(c) = cfg("DBINE_TEST_CLICKHOUSE_25_3_URL") else {
        return;
    };
    let d = driver();
    let db = format!("dbine_vec_legacy_{}", std::process::id());
    let mut s = d.connect(&c, None).await.expect("connects");
    let version = s.server_version().await.unwrap();
    assert!(version.contains(" 25.3"), "{version}");
    run(&mut s, &format!("DROP DATABASE IF EXISTS `{db}`")).await;
    run(&mut s, &format!("CREATE DATABASE `{db}`")).await;
    let r = {
        let (d, c, db) = (d.clone(), c.clone(), db.clone());
        tokio::spawn(async move {
            let mut s = d.connect(&c, Some(&db)).await.unwrap();
            // Made by hand, as 25.3 takes it: two and five arguments.
            run(
                &mut s,
                &format!(
                    "SET allow_experimental_vector_similarity_index = 1;
                     CREATE TABLE `{db}`.t (id UInt64, emb Array(Float32), wide Array(Float64),
                       INDEX emb_ix emb TYPE vector_similarity('hnsw', 'cosineDistance') GRANULARITY 100000000,
                       INDEX wide_ix wide TYPE vector_similarity('hnsw', 'L2Distance', 'bf16', 32, 128) GRANULARITY 2)
                     ENGINE = MergeTree ORDER BY id;"
                ),
            )
            .await;
            let all = s.database_schema().await.unwrap();
            let t = all.iter().find(|t| t.name == "t").unwrap().clone();
            let ix = |n: &str| t.indexes.iter().find(|i| i.name == n).unwrap().clone();
            let e = ix("emb_ix");
            assert_eq!(e.kind.as_deref(), Some("vector_similarity"));
            assert_eq!(
                e.vector,
                Some(VectorIndex {
                    algorithm: Some("hnsw".into()),
                    metric: Some("cosine".into()),
                    params: Default::default(),
                })
            );
            let w = ix("wide_ix");
            assert_eq!(w.kind.as_deref(), Some("vector_similarity GRANULARITY 2"));
            let v = w.vector.as_ref().unwrap();
            assert_eq!(v.metric.as_deref(), Some("l2"));
            assert_eq!(
                (
                    v.params.get("quantization").map(String::as_str),
                    v.params.get("m").map(String::as_str),
                    v.params.get("ef_construction").map(String::as_str),
                    v.params.get("dimensions")
                ),
                (Some("bf16"), Some("32"), Some("128"), None)
            );

            // Written back in its own form, after the setting: a clone.
            let mut clone = t.clone();
            clone.name = "c".into();
            let ddl = create(&d, &clone);
            assert!(
                ddl.contains("SET allow_experimental_vector_similarity_index = 1;")
                    && ddl.contains("vector_similarity('hnsw', 'cosineDistance')")
                    && ddl.contains("vector_similarity('hnsw', 'L2Distance', 'bf16', 32, 128)"),
                "{ddl}"
            );
            run(&mut s, &ddl).await;
            let all = s.database_schema().await.unwrap();
            let c2 = all.iter().find(|t| t.name == "c").unwrap();
            for n in ["emb_ix", "wide_ix"] {
                let a = c2.indexes.iter().find(|i| i.name == n).unwrap();
                assert_eq!(
                    (&a.kind, &a.vector, &a.options),
                    (&ix(n).kind, &ix(n).vector, &ix(n).options),
                    "{n}"
                );
            }

            // A sync that adds a column keeps both indexes.
            let mut new = t.clone();
            new.columns.push(col("note", "String"));
            let sync = d
                .sync_script(&[TableChange::Alter {
                    old: t.clone(),
                    new: new.clone(),
                }])
                .unwrap();
            for st in &sync.statements {
                run(&mut s, st).await;
            }
            // And one that changes the cosine index's metric makes it again,
            // in the form 25.3 takes.
            let mut l2 = new.clone();
            l2.indexes
                .iter_mut()
                .find(|i| i.name == "emb_ix")
                .unwrap()
                .vector
                .as_mut()
                .unwrap()
                .metric = Some("l2".into());
            let sync = d
                .sync_script(&[TableChange::Alter {
                    old: new.clone(),
                    new: l2.clone(),
                }])
                .unwrap();
            let all_sql = sync.statements.join("\n");
            assert!(all_sql.contains("DROP INDEX"), "{all_sql}");
            for st in &sync.statements {
                run(&mut s, st).await;
            }
            let all = s.database_schema().await.unwrap();
            let t2 = all.iter().find(|t| t.name == "t").unwrap();
            assert!(t2.columns.iter().any(|c| c.name == "note"));
            let e2 = t2.indexes.iter().find(|i| i.name == "emb_ix").unwrap();
            assert_eq!(
                e2.vector.as_ref().unwrap().metric.as_deref(),
                Some("l2"),
                "{all_sql}"
            );
            assert_eq!(
                t2.indexes
                    .iter()
                    .find(|i| i.name == "wide_ix")
                    .unwrap()
                    .vector,
                w.vector
            );

            // Rows, and a search the index serves.
            let ins = d
                .insert_script(
                    &table(&db, "t"),
                    &["id".into(), "emb".into(), "wide".into(), "note".into()],
                    &[
                        vec![json!(1), json!([1, 0, 0]), json!([0.5, 1]), json!("a")],
                        vec![json!(2), json!([0, 1, 0]), json!([2, 1e-7]), json!("b")],
                    ],
                )
                .unwrap();
            run(&mut s, &ins).await;
            // (On `wide`: with two L2 indexes, 25.3 searches `emb` through
            // `wide`'s and fails on the dimension, its own bug.)
            let out = run(
                &mut s,
                &format!("SELECT id FROM `{db}`.t ORDER BY L2Distance(wide, [2, 0]) LIMIT 1"),
            )
            .await;
            assert_eq!(out.results[0].rows[0][0], json!(2));
            // The browse marks the Float64 one its index searches (no
            // dimension in this form).
            let browse = s.browse_query(&table(&db, "t"), 10);
            let out = run(&mut s, &browse).await;
            let r = &out.results[0];
            let wi = r.columns.iter().position(|c| c.name == "wide").unwrap();
            assert_eq!(
                r.columns[wi].vector,
                Some(dbine_driver::VectorMeta::dense(
                    None,
                    dbine_driver::VectorElement::F64
                ))
            );
            // The Índices view.
            let usage = s.index_usage(&table(&db, "t")).await.unwrap().unwrap();
            let u = usage.indexes.iter().find(|u| u.name == "wide_ix").unwrap();
            assert_eq!(u.vector, w.vector);

            // The form DBine writes for a new index is 25.8's: refused, and
            // the error says so.
            let e = fails(
                &mut s,
                &create(&d, &designed(&db, "n", "Array(Float32)", "cosine")),
            )
            .await;
            assert!(e.contains("25.8"), "{e}");
        })
    };
    let r = r.await;
    run(&mut s, &format!("DROP DATABASE IF EXISTS `{db}`")).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}

/// 25.8: the designer's index (with the dimension) and BFloat16 vectors
/// work; inner product (`dotProduct`, 26.4) and an index of 24.8–25.7
/// (without the dimension) are refused, and the error says what each
/// needs.
#[tokio::test]
#[ignore]
async fn clickhouse_25_8_dimension_form() {
    let Some(c) = cfg("DBINE_TEST_CLICKHOUSE_25_8_URL") else {
        return;
    };
    let d = driver();
    let db = format!("dbine_vec_258_{}", std::process::id());
    let mut s = d.connect(&c, None).await.expect("connects");
    let version = s.server_version().await.unwrap();
    assert!(version.contains(" 25.8"), "{version}");
    run(&mut s, &format!("DROP DATABASE IF EXISTS `{db}`")).await;
    run(&mut s, &format!("CREATE DATABASE `{db}`")).await;
    let r = {
        let (d, c, db) = (d.clone(), c.clone(), db.clone());
        tokio::spawn(async move {
            let mut s = d.connect(&c, Some(&db)).await.unwrap();
            for (name, ty, metric) in [
                ("f32", "Array(Float32)", "cosine"),
                ("bf16", "Array(BFloat16)", "l2"),
            ] {
                let t = designed(&db, name, ty, metric);
                run(&mut s, &create(&d, &t)).await;
                let all = s.database_schema().await.unwrap();
                let back = all.iter().find(|x| x.name == name).unwrap();
                assert_eq!(back.indexes[0].vector, t.indexes[0].vector, "{name}");
                assert_eq!(back.columns[1].data_type, ty);
            }
            let e = fails(
                &mut s,
                &create(&d, &designed(&db, "ip", "Array(Float32)", "ip")),
            )
            .await;
            assert!(e.contains("26.4"), "{e}");
            // An index of 24.8–25.7 (no dimension, its type in the options),
            // written back in its form: 25.8 refuses it, and the error says
            // to give it its dimension.
            let mut old = designed(&db, "old", "Array(Float32)", "l2");
            old.indexes[0].vector.as_mut().unwrap().params.clear();
            old.indexes[0].options.insert(
                "type".into(),
                "vector_similarity('hnsw', 'L2Distance')".into(),
            );
            let sql = create(&d, &old);
            assert!(
                sql.starts_with("SET allow_experimental_vector_similarity_index = 1;"),
                "{sql}"
            );
            let e = fails(&mut s, &sql).await;
            assert!(e.contains("«dimensions»"), "{e}");
        })
    };
    let r = r.await;
    run(&mut s, &format!("DROP DATABASE IF EXISTS `{db}`")).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}
