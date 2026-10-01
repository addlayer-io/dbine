//! Against a real ksqlDB (it needs Kafka):
//!
//! ```sh
//! docker network create dbine-test-net
//! docker run -d --name dbine-test-kafka --network dbine-test-net --hostname kafka \
//!   -e KAFKA_NODE_ID=1 -e KAFKA_PROCESS_ROLES=broker,controller \
//!   -e KAFKA_LISTENERS=PLAINTEXT://kafka:9092,CONTROLLER://kafka:9093 \
//!   -e KAFKA_ADVERTISED_LISTENERS=PLAINTEXT://kafka:9092 -e KAFKA_CONTROLLER_LISTENER_NAMES=CONTROLLER \
//!   -e KAFKA_LISTENER_SECURITY_PROTOCOL_MAP=CONTROLLER:PLAINTEXT,PLAINTEXT:PLAINTEXT \
//!   -e KAFKA_CONTROLLER_QUORUM_VOTERS=1@kafka:9093 -e KAFKA_OFFSETS_TOPIC_REPLICATION_FACTOR=1 \
//!   -e KAFKA_TRANSACTION_STATE_LOG_REPLICATION_FACTOR=1 -e KAFKA_TRANSACTION_STATE_LOG_MIN_ISR=1 \
//!   -e CLUSTER_ID=MkU3OEVBNTcwNTJENDM2Qk confluentinc/cp-kafka:7.7.1
//! docker run -d --name dbine-test-ksqldb --network dbine-test-net -p 25188:8088 \
//!   -e KSQL_BOOTSTRAP_SERVERS=kafka:9092 -e KSQL_LISTENERS=http://0.0.0.0:8088 \
//!   -e KSQL_KSQL_INTERNAL_TOPIC_REPLICAS=1 -e KSQL_KSQL_STREAMS_REPLICATION_FACTOR=1 -e KSQL_KSQL_SINK_REPLICAS=1 \
//!   confluentinc/cp-ksqldb-server:7.7.1
//! DBINE_TEST_KSQLDB_URL=http://localhost:25188 cargo test -p dbine-driver-ksqldb -- --ignored
//! ```

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{kinds, ConnectionConfig, DdlParts, Error, ObjectRef, QueryOutcome, Session, TableSchema};
use std::time::{Duration, Instant};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_KSQLDB_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: "ksqldb".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        ..Default::default()
    };
    c.options.insert("push_timeout".into(), "10".into());
    Some(c)
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: None, name: name.into() }
}

#[tokio::test]
#[ignore]
async fn ksqldb() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_ksqldb::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    assert!(s.server_version().await.unwrap().starts_with("ksqlDB 7"));
    assert_eq!(s.list_databases().await.unwrap(), vec!["default".to_string()]);

    let mut out = QueryOutcome::default();
    s.execute(
        "DROP TABLE IF EXISTS DBINE_T DELETE TOPIC; DROP STREAM IF EXISTS DBINE_S DELETE TOPIC;",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE STREAM DBINE_S (ID INT KEY, NAME VARCHAR, TAGS ARRAY<VARCHAR>)
           WITH (KAFKA_TOPIC='dbine_s', VALUE_FORMAT='JSON', PARTITIONS=1);
         INSERT INTO DBINE_S (ID, NAME, TAGS) VALUES (1, 'a', ARRAY['x']);
         INSERT INTO DBINE_S (ID, NAME, TAGS) VALUES (2, 'b', ARRAY['y']);
         INSERT INTO DBINE_S (ID, NAME, TAGS) VALUES (3, 'c', ARRAY['z']);
         CREATE TABLE DBINE_T AS SELECT ID, COUNT(*) AS N FROM DBINE_S GROUP BY ID EMIT CHANGES;",
        10,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.results[1].rows_affected, Some(1));
    assert!(out.messages.iter().any(|m| m.contains("Stream created")), "{:?}", out.messages);

    let objs = s.list_objects().await.unwrap();
    let has = |k: &str, n: &str| objs.iter().any(|o| o.kind == k && o.name == n);
    assert!(has(kinds::STREAM, "DBINE_S") && has(kinds::TABLE, "DBINE_T") && has(kinds::TOPIC, "dbine_s"), "{objs:?}");
    assert!(!objs.iter().any(|o| o.name == "KSQL_PROCESSING_LOG"));

    let cols = s.columns(&obj(kinds::STREAM, "DBINE_S")).await.unwrap();
    assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["ID", "NAME", "TAGS"]);
    assert!(cols[0].primary_key && cols[2].data_type == "ARRAY<STRING>", "{cols:?}");
    let def = s.definition(&obj(kinds::STREAM, "DBINE_S")).await.unwrap().unwrap();
    assert!(def.starts_with("CREATE STREAM"), "{def}");

    // Push query through browse: stops at max_rows.
    let q = s.browse_query(&obj(kinds::STREAM, "DBINE_S"), 100);
    let t = Instant::now();
    let mut out = QueryOutcome::default();
    s.execute(&q, 2, &mut out).await.unwrap();
    let r = &out.results[0];
    assert_eq!(r.columns.len(), 3);
    assert_eq!((r.rows.len(), r.truncated), (2, true), "{r:?}");
    assert_eq!(r.rows[0][2], serde_json::json!("[\"x\"]"));
    assert!(t.elapsed() < Duration::from_secs(10));

    // Push query with fewer rows than the limit: the time cap ends it.
    let mut out = QueryOutcome::default();
    s.execute("SELECT * FROM DBINE_S EMIT CHANGES", 100, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 3);
    assert!(out.messages.iter().any(|m| m.contains("se detuvo")), "{:?}", out.messages);

    // Pull query on the materialized table (may take a moment to fill).
    let mut rows = 0;
    for _ in 0..20 {
        let mut out = QueryOutcome::default();
        s.execute("SELECT * FROM DBINE_T;", 100, &mut out).await.unwrap();
        rows = out.results[0].rows.len();
        if rows == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_eq!(rows, 3);

    // PRINT a topic.
    let q = s.browse_query(&obj(kinds::TOPIC, "dbine_s"), 2);
    let mut out = QueryOutcome::default();
    s.execute(&q, 2, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2, "{:?}", out.results);

    // SHOW / DESCRIBE come back as tables; SET is kept client-side.
    let mut out = QueryOutcome::default();
    s.execute("SET 'auto.offset.reset' = 'latest'; SHOW STREAMS; DESCRIBE DBINE_S;", 100, &mut out).await.unwrap();
    assert!(out.results[1].rows.iter().any(|r| r[0] == serde_json::json!("DBINE_S")));
    assert_eq!(out.results[2].rows.len(), 3);

    // Error mid-script.
    let mut out = QueryOutcome::default();
    let e = s.execute("SHOW TOPICS; SELECT * FROM NOPE; SHOW STREAMS", 10, &mut out).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");
    assert_eq!(e.to_script_error().offset, Some(13), "the failing statement's place");
    assert_eq!(out.results.len(), 1);

    // Cancel a push query that would wait forever for new events.
    let mut c2 = c.clone();
    c2.options.insert("push_timeout".into(), "600".into());
    let mut s2 = d.connect(&c2, None).await.unwrap();
    let stop = s2.interrupter().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        stop();
    });
    let t = Instant::now();
    let mut out = QueryOutcome::default();
    let r = s2.execute("SET 'auto.offset.reset' = 'latest'; SELECT * FROM DBINE_S EMIT CHANGES;", 100, &mut out).await;
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(10));

    // Read-only (the registry wraps SQL sessions).
    let mut ro = ReadOnlySession::new(d.connect(&c, None).await.unwrap());
    let mut out = QueryOutcome::default();
    assert!(ro.execute("DROP STREAM DBINE_S", 10, &mut out).await.is_err());
    ro.execute("SHOW STREAMS", 10, &mut out).await.unwrap();

    let mut out = QueryOutcome::default();
    s.execute("DROP TABLE DBINE_T DELETE TOPIC; DROP STREAM DBINE_S DELETE TOPIC;", 10, &mut out).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn plans() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_ksqldb::drivers().remove(0);
    assert!(d.supports_explain());
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP STREAM IF EXISTS DBINE_P2 DELETE TOPIC; DROP STREAM IF EXISTS DBINE_P DELETE TOPIC;
         CREATE STREAM DBINE_P (ID INT KEY, NAME VARCHAR) WITH (KAFKA_TOPIC='dbine_p', VALUE_FORMAT='JSON', PARTITIONS=1);",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let csas = "CREATE STREAM DBINE_P2 AS SELECT ID, UCASE(NAME) AS N FROM DBINE_P WHERE ID > 1 EMIT CHANGES";

    let mut out = QueryOutcome::default();
    s.explain(&format!("SELECT * FROM DBINE_P EMIT CHANGES; {csas}"), false, 10, &mut out).await.unwrap();
    assert!(out.results.is_empty(), "nothing ran");
    assert_eq!(out.plans.len(), 2, "{:?}", out.messages);
    let p = &out.plans[1];
    println!("{}\n{:#?}", p.raw, p.root);
    assert!(!p.actual);
    assert_eq!(p.root.op, "SINK");
    assert!(!p.root.children.is_empty());
    let mut out = QueryOutcome::default();
    s.execute("SHOW STREAMS", 10, &mut out).await.unwrap();
    assert!(!format!("{:?}", out.results).contains("DBINE_P2"), "the estimated plan must not create the stream");

    // Run + plan: the statement runs once.
    let mut out = QueryOutcome::default();
    s.explain(csas, true, 10, &mut out).await.unwrap();
    assert_eq!(out.plans.len(), 1);
    let mut out = QueryOutcome::default();
    s.execute("SHOW STREAMS", 10, &mut out).await.unwrap();
    assert!(format!("{:?}", out.results).contains("DBINE_P2"));

    let mut out = QueryOutcome::default();
    s.execute(
        "TERMINATE ALL; DROP STREAM IF EXISTS DBINE_P2 DELETE TOPIC; DROP STREAM IF EXISTS DBINE_P DELETE TOPIC;",
        10,
        &mut out,
    )
    .await
    .unwrap();
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{e:?}\n{sql}"));
}

async fn ours(s: &mut Box<dyn Session>) -> Vec<TableSchema> {
    s.database_schema().await.unwrap().into_iter().filter(|t| t.name.starts_with("DBINE_DDL")).collect()
}

#[tokio::test]
#[ignore]
async fn schema_and_ddl() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_ksqldb::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let cleanup = "DROP TABLE IF EXISTS DBINE_DDL_T DELETE TOPIC; DROP STREAM IF EXISTS DBINE_DDL_S DELETE TOPIC;";
    run(&mut s, cleanup).await;
    run(
        &mut s,
        "CREATE STREAM DBINE_DDL_S (ID INT KEY, NAME VARCHAR, TAGS ARRAY<VARCHAR>, ST STRUCT<A INT, `b` VARCHAR>,
            M MAP<VARCHAR, INT>, AMT DECIMAL(10,2), TS TIMESTAMP)
           WITH (KAFKA_TOPIC='dbine_ddl_s', VALUE_FORMAT='JSON', PARTITIONS=2, TIMESTAMP='TS');
         CREATE TABLE DBINE_DDL_T (ID VARCHAR PRIMARY KEY, N BIGINT)
           WITH (KAFKA_TOPIC='dbine_ddl_t', KEY_FORMAT='JSON', VALUE_FORMAT='JSON', PARTITIONS=1);",
    )
    .await;

    let src = ours(&mut s).await;
    assert_eq!(src.iter().map(|t| (t.name.as_str(), t.kind.as_str())).collect::<Vec<_>>(), [("DBINE_DDL_S", "stream"), ("DBINE_DDL_T", "table")]);
    let st = &src[0];
    assert_eq!(st.primary_key.as_ref().unwrap().columns, ["ID"]);
    assert_eq!(st.columns.iter().map(|c| c.data_type.as_str()).collect::<Vec<_>>(), [
        "INTEGER", "STRING", "ARRAY<STRING>", "STRUCT<`A` INTEGER, `b` STRING>", "MAP<STRING, INTEGER>", "DECIMAL(10, 2)", "TIMESTAMP"
    ]);
    let o = |t: &TableSchema, k: &str| t.options.get(k).cloned().unwrap_or_default();
    assert_eq!([o(st, "object"), o(st, "KAFKA_TOPIC"), o(st, "VALUE_FORMAT"), o(st, "KEY_FORMAT"), o(st, "PARTITIONS"), o(st, "TIMESTAMP")],
        ["STREAM", "dbine_ddl_s", "JSON", "KAFKA", "2", "TS"]);
    let tb = &src[1];
    assert_eq!(tb.primary_key.as_ref().unwrap().columns, ["ID"]);
    assert_eq!([o(tb, "object"), o(tb, "KEY_FORMAT"), o(tb, "PARTITIONS")], ["TABLE", "JSON", "1"]);

    // Round trip: drop the sources (the topics stay) and create them again from the DDL.
    for t in &src {
        run(&mut s, &d.table_ddl(t, DdlParts { drop: true, if_exists: true, ..Default::default() }).unwrap()).await;
    }
    assert!(ours(&mut s).await.is_empty());
    let mut script = Vec::new();
    for t in &src {
        script.push(d.table_ddl(t, DdlParts { create: true, if_exists: true, indexes: true, foreign_keys: true, ..Default::default() }).unwrap());
    }
    let script = script.join("\n");
    println!("{script}");
    run(&mut s, &script).await;
    assert_eq!(ours(&mut s).await, src);

    // Inserts.
    let target = obj(kinds::STREAM, "DBINE_DDL_S");
    let ins = d
        .insert_script(
            &target,
            &["ID".into(), "NAME".into(), "TAGS".into(), "AMT".into(), "TS".into()],
            &[
                vec![1.into(), "O'Brien".into(), serde_json::json!(["x", "y"]), 1.5.into(), "2024-01-31T10:00:00".into()],
                vec![2.into(), serde_json::Value::Null, serde_json::Value::Null, 2.into(), "2024-01-31T11:00:00".into()],
            ],
        )
        .unwrap();
    println!("{ins}");
    run(&mut s, &ins).await;
    let mut out = QueryOutcome::default();
    s.execute("SELECT NAME FROM DBINE_DDL_S EMIT CHANGES LIMIT 2;", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2, "{:?}", out.results);
    assert!(out.results[0].rows.iter().any(|r| r[0] == serde_json::json!("O'Brien")));

    assert!(!d.capabilities().create_database);
    assert!(matches!(s.create_database("X").await, Err(Error::Unsupported(_))));
    run(&mut s, cleanup).await;
}

#[tokio::test]
#[ignore]
async fn monitor() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_ksqldb::drivers().remove(0);
    assert!(d.capabilities().monitor);
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE STREAM IF NOT EXISTS MON_S (ID INT, V VARCHAR) WITH (KAFKA_TOPIC='mon_s', PARTITIONS=1, VALUE_FORMAT='JSON');
         CREATE TABLE IF NOT EXISTS MON_T AS SELECT V, COUNT(*) AS C FROM MON_S GROUP BY V EMIT CHANGES;
         INSERT INTO MON_S (ID, V) VALUES (1, 'a'); INSERT INTO MON_S (ID, V) VALUES (2, 'b');",
        10,
        &mut out,
    )
    .await
    .unwrap();
    // The persistent query needs a moment to consume what was inserted.
    let deadline = Instant::now() + Duration::from_secs(60);
    let snap = loop {
        let snap = s.monitor().await.unwrap();
        let consumed = snap.metrics.iter().find(|m| m.key == "messages_in").and_then(|m| m.value).unwrap_or(0.0);
        if consumed > 0.0 || Instant::now() > deadline {
            break snap;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    let v = |k: &str| snap.metrics.iter().find(|m| m.key == k).and_then(|m| m.value);
    eprintln!(
        "{:?}\ninfo {:?}\nnotes {:?}\ntables {:?}",
        snap.metrics.iter().map(|m| (m.key.as_str(), m.value)).collect::<Vec<_>>(),
        snap.info,
        snap.notes,
        snap.tables.iter().map(|t| (t.key.as_str(), t.rows.len())).collect::<Vec<_>>()
    );
    assert!(v("active_sessions").unwrap() >= 1.0);
    assert!(v("messages_in").unwrap() > 0.0);
    assert!(v("streams").unwrap() >= 1.0 && v("tables").unwrap() >= 1.0);
    let q = snap.tables.iter().find(|t| t.key == "queries").unwrap();
    assert!(q.rows.iter().any(|r| r[0].as_str().is_some_and(|id| id.contains("MON_T"))));
    assert!(snap.tables.iter().any(|t| t.key == "sources" && t.rows.iter().any(|r| r[0] == "MON_S")));
    assert!(snap.info.iter().any(|(k, _)| k == "Versión"));
}

/// Schema sync: a stream read back from `database_schema` gets columns
/// added (ALTER STREAM … ADD COLUMN); another is dropped and one created.
#[tokio::test]
#[ignore]
async fn schema_sync() {
    use dbine_driver::{ColumnDef, TableChange};
    let Some(c) = cfg() else { return };
    let d = dbine_driver_ksqldb::drivers().remove(0);
    assert!(d.supports_schema_sync());
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    for t in ["SYNC_CLICKS", "SYNC_GONE", "SYNC_FRESH"] {
        let _ = s.execute(&format!("DROP STREAM IF EXISTS {t} DELETE TOPIC;"), 10, &mut out).await;
    }
    s.execute(
        "CREATE STREAM SYNC_CLICKS (ID STRING KEY, URL STRING) WITH (KAFKA_TOPIC='sync_clicks', PARTITIONS=1, VALUE_FORMAT='JSON');
         CREATE STREAM SYNC_GONE (ID STRING KEY, X INT) WITH (KAFKA_TOPIC='sync_gone', PARTITIONS=1, VALUE_FORMAT='JSON');",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let schema = s.database_schema().await.unwrap();
    let old = schema.iter().find(|t| t.name == "SYNC_CLICKS").unwrap().clone();
    let gone = schema.iter().find(|t| t.name == "SYNC_GONE").unwrap().clone();
    let mut new = old.clone();
    new.columns.push(ColumnDef { name: "USER_ID".into(), data_type: "BIGINT".into(), nullable: true, ..Default::default() });
    let mut created = old.clone();
    created.name = "SYNC_FRESH".into();
    created.options.insert("KAFKA_TOPIC".into(), "sync_fresh".into());
    let script = d.sync_script(&[TableChange::Alter { old, new }, TableChange::Drop { table: gone }, TableChange::Create { table: created }]).unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        let mut out = QueryOutcome::default();
        s.execute(st, 10, &mut out).await.unwrap_or_else(|e| panic!("{st}: {e}"));
    }
    let after = s.database_schema().await.unwrap();
    assert!(!after.iter().any(|t| t.name == "SYNC_GONE"));
    assert!(after.iter().any(|t| t.name == "SYNC_FRESH"));
    let clicks = after.iter().find(|t| t.name == "SYNC_CLICKS").unwrap();
    assert!(clicks.columns.iter().any(|c| c.name == "USER_ID" && c.data_type == "BIGINT"), "{:?}", clicks.columns);
    for t in ["SYNC_CLICKS", "SYNC_GONE", "SYNC_FRESH"] {
        let _ = s.execute(&format!("DROP STREAM IF EXISTS {t} DELETE TOPIC;"), 10, &mut out).await;
    }
}
