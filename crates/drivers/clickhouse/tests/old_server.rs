//! ClickHouse servers older than `http_write_exception_in_output_format`
//! (22.8 and before, issue #19): DBine stops asking for that setting and
//! everything works as on a current server.
//!
//! ```sh
//! docker run -d --name dbine-test-clickhouse-22 -p 25124:8123 -e CLICKHOUSE_USER=dbine \
//!   -e CLICKHOUSE_PASSWORD=dbine clickhouse/clickhouse-server:22.8
//! DBINE_TEST_CLICKHOUSE_22_URL=http://dbine:dbine@localhost:25124 \
//!   cargo test -p dbine-driver-clickhouse --test old_server -- --ignored
//! ```

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome};

fn cfg() -> Option<ConnectionConfig> {
    let url =
        reqwest::Url::parse(&std::env::var("DBINE_TEST_CLICKHOUSE_22_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "clickhouse".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

fn table(name: &str) -> ObjectRef {
    ObjectRef {
        kind: kinds::TABLE.into(),
        schema: None,
        name: name.into(),
    }
}

#[tokio::test]
#[ignore]
async fn clickhouse_22_8() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_clickhouse::drivers()
        .into_iter()
        .find(|d| d.info().id == "clickhouse")
        .unwrap();

    // The reported failure: connecting answered `Unknown setting
    // http_write_exception_in_output_format`.
    let mut s = d.connect(&c, None).await.expect("connects");
    let version = s.server_version().await.unwrap();
    assert!(version.contains("22.8"), "{version}");

    let mut out = QueryOutcome::default();
    s.execute(
        "DROP DATABASE IF EXISTS dbine_old_a; DROP DATABASE IF EXISTS dbine_old_b;
         CREATE DATABASE dbine_old_a; CREATE DATABASE dbine_old_b;
         CREATE TABLE dbine_old_a.t (id UInt64, name String) ENGINE = MergeTree ORDER BY id;
         CREATE TABLE dbine_old_b.u (id UInt64, amount Decimal(10,2)) ENGINE = MergeTree ORDER BY id;
         INSERT INTO dbine_old_a.t VALUES (1, 'uno'), (2, 'dos');
         INSERT INTO dbine_old_b.u VALUES (1, 10.5), (2, 20);",
        100,
        &mut out,
    )
    .await
    .expect("DDL and inserts run");

    // What the explorer and completion read.
    let dbs = s.list_databases().await.unwrap();
    assert!(dbs.iter().any(|x| x == "dbine_old_a") && dbs.iter().any(|x| x == "dbine_old_b"));
    let mut a = d.connect(&c, Some("dbine_old_a")).await.unwrap();
    let objs = a.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.name == "t"), "{objs:?}");
    let cols = a.columns(&table("t")).await.unwrap();
    assert_eq!(
        cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["id", "name"]
    );

    // A query in one database that reads the other (issue #20).
    let mut out = QueryOutcome::default();
    a.execute(
        "SELECT t.name, u.amount FROM t JOIN dbine_old_b.u AS u ON u.id = t.id ORDER BY t.id",
        100,
        &mut out,
    )
    .await
    .unwrap();
    let r = out.results.last().unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][0], serde_json::json!("uno"));

    // Errors still come back as errors, with the server's message.
    let mut out = QueryOutcome::default();
    let err = a
        .execute("SELECT * FROM dbine_old_b.nope", 10, &mut out)
        .await
        .expect_err("unknown table");
    assert!(err.to_string().contains("nope"), "{err}");
    // …and one in the middle of a script stops it there.
    let mut out = QueryOutcome::default();
    assert!(a
        .execute("SELECT 1; SELECT throwIf(1); SELECT 2", 10, &mut out)
        .await
        .is_err());

    let mut out = QueryOutcome::default();
    s.execute(
        "DROP DATABASE dbine_old_a; DROP DATABASE dbine_old_b",
        10,
        &mut out,
    )
    .await
    .unwrap();
}

/// The designer's vector index and BFloat16 vectors on a server that has
/// neither (22.8): the error says which version each needs, and nothing is
/// left behind.
#[tokio::test]
#[ignore]
async fn clickhouse_22_8_vectors_say_the_version() {
    use dbine_driver::{ColumnDef, DdlParts, IndexDef, TableSchema, VectorIndex};
    let Some(c) = cfg() else { return };
    let d = dbine_driver_clickhouse::drivers()
        .into_iter()
        .find(|d| d.info().id == "clickhouse")
        .unwrap();
    let mut s = d.connect(&c, None).await.expect("connects");
    let name = format!("dbine_old_vec_{}", std::process::id());
    let col = |n: &str, ty: &str| ColumnDef {
        name: n.into(),
        data_type: ty.into(),
        nullable: false,
        ..Default::default()
    };
    let with_index = TableSchema {
        schema: Some("default".into()),
        name: name.clone(),
        columns: vec![col("id", "UInt64"), col("emb", "Array(Float32)")],
        indexes: vec![IndexDef {
            name: "emb_ix".into(),
            columns: vec!["emb".into()],
            vector: Some(VectorIndex {
                algorithm: Some("hnsw".into()),
                metric: Some("cosine".into()),
                params: [("dimensions".to_string(), "3".to_string())].into(),
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    let half = TableSchema {
        columns: vec![col("id", "UInt64"), col("emb", "Array(BFloat16)")],
        indexes: Vec::new(),
        ..with_index.clone()
    };
    for (t, needs) in [(with_index, "25.8"), (half, "25.1")] {
        let ddl = d
            .table_ddl(
                &t,
                DdlParts {
                    create: true,
                    indexes: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut out = QueryOutcome::default();
        let e = s
            .execute(&ddl, 10, &mut out)
            .await
            .expect_err("22.8 has no vector index nor BFloat16")
            .to_string();
        assert!(e.contains(needs), "{e}");
    }
    let mut out = QueryOutcome::default();
    s.execute(
        &format!("SELECT count() FROM system.tables WHERE name = '{name}'"),
        10,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(0));

    // A browse of a Float64 array asks for the table's vector indexes:
    // 22.8 has none (nor the catalog column), and the rows come as always.
    let mut out = QueryOutcome::default();
    s.execute(
        &format!(
            "CREATE TABLE default.{name} (id UInt64, nums Array(Float64)) ENGINE = MergeTree ORDER BY id;
             INSERT INTO default.{name} VALUES (1, [0.5, 2]);"
        ),
        10,
        &mut out,
    )
    .await
    .unwrap();
    let browse = s.browse_query(
        &ObjectRef {
            kind: kinds::TABLE.into(),
            schema: Some("default".into()),
            name: name.clone(),
        },
        10,
    );
    let mut out = QueryOutcome::default();
    let r = s.execute(&browse, 10, &mut out).await;
    let mut drop = QueryOutcome::default();
    s.execute(&format!("DROP TABLE default.{name}"), 10, &mut drop)
        .await
        .unwrap();
    r.unwrap();
    let res = &out.results[0];
    assert!(res.columns[1].vector.is_none());
    assert_eq!(res.rows[0][1], serde_json::json!("[0.5,2]"));
}
