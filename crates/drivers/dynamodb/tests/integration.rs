//! Against DynamoDB Local:
//!   docker run -d --name dbine-test-dynamodb -p 25300:8000 amazon/dynamodb-local -jar DynamoDBLocal.jar -inMemory
//!   DBINE_TEST_DYNAMODB_URL=http://localhost:25300 cargo test -p dbine-driver-dynamodb -- --ignored

use dbine_driver::{kinds, ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use serde_json::json;

fn cfg(read_only: bool) -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_DYNAMODB_URL").ok()?;
    let mut c = ConnectionConfig { driver: "dynamodb".into(), read_only, ..Default::default() };
    for (k, v) in [
        ("region", "us-east-1"),
        ("auth_mode", "keys"),
        ("access_key_id", "dummy"),
        ("secret_access_key", "dummy"),
        ("endpoint_url", url.as_str()),
    ] {
        c.options.insert(k.into(), v.into());
    }
    Some(c)
}

async fn open(read_only: bool) -> Option<Box<dyn Session>> {
    let c = cfg(read_only)?;
    Some(dbine_driver_dynamodb::drivers().pop().unwrap().connect(&c, None).await.unwrap())
}

#[tokio::test]
#[ignore]
async fn round_trip() {
    let Some(mut s) = open(false).await else { return };
    // A table with a GSI, created through the SDK (PartiQL has no DDL).
    let url = std::env::var("DBINE_TEST_DYNAMODB_URL").unwrap();
    let conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .endpoint_url(url)
        .credentials_provider(aws_credential_types::Credentials::new("dummy", "dummy", None, None, "t"))
        .load()
        .await;
    let client = aws_sdk_dynamodb::Client::new(&conf);
    use aws_sdk_dynamodb::types::*;
    let _ = client.delete_table().table_name("dbine_music").send().await;
    client
        .create_table()
        .table_name("dbine_music")
        .billing_mode(BillingMode::PayPerRequest)
        .attribute_definitions(
            AttributeDefinition::builder().attribute_name("artist").attribute_type(ScalarAttributeType::S).build().unwrap(),
        )
        .attribute_definitions(
            AttributeDefinition::builder().attribute_name("song").attribute_type(ScalarAttributeType::S).build().unwrap(),
        )
        .attribute_definitions(
            AttributeDefinition::builder().attribute_name("year").attribute_type(ScalarAttributeType::N).build().unwrap(),
        )
        .key_schema(KeySchemaElement::builder().attribute_name("artist").key_type(KeyType::Hash).build().unwrap())
        .key_schema(KeySchemaElement::builder().attribute_name("song").key_type(KeyType::Range).build().unwrap())
        .global_secondary_indexes(
            GlobalSecondaryIndex::builder()
                .index_name("byYear")
                .key_schema(KeySchemaElement::builder().attribute_name("year").key_type(KeyType::Hash).build().unwrap())
                .projection(Projection::builder().projection_type(ProjectionType::All).build())
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();

    let mut out = QueryOutcome::default();
    s.execute(
        r#"INSERT INTO "dbine_music" VALUE {'artist': 'A', 'song': 's1', 'year': 1999, 'tags': ['x', 'y'], 'meta': {'k': 1}};
           INSERT INTO "dbine_music" VALUE {'artist': 'B', 'song': 's2', 'year': 2001, 'big': 12345678901234567890, 'ok': true, 'gone': null}"#,
        100,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.results.len(), 2);
    assert_eq!(out.results[0].rows_affected, Some(1));

    let objs = s.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.kind == kinds::TABLE && o.name == "dbine_music"));
    let idx = objs.iter().find(|o| o.kind == kinds::INDEX && o.name == "byYear").expect("GSI listed");
    assert_eq!(idx.parent.as_deref(), Some("dbine_music"));

    let t = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "dbine_music".into() };
    let cols = s.columns(&t).await.unwrap();
    let names: Vec<_> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(&names[..2], &["artist", "song"]);
    assert!(cols[0].primary_key && cols[1].primary_key);
    assert!(names.contains(&"year") && names.contains(&"tags"));
    let def = s.definition(&t).await.unwrap().unwrap();
    assert!(def.contains("\"GlobalSecondaryIndexes\"") && def.contains("byYear"), "{def}");

    let mut out = QueryOutcome::default();
    let q = s.browse_query(&t, 100);
    s.execute(&q, 100, &mut out).await.unwrap();
    let r = &out.results[0];
    let cols: Vec<_> = r.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(&cols[..2], &["artist", "song"]);
    assert_eq!(r.rows.len(), 2);
    let a = r.rows.iter().find(|row| row[0] == json!("A")).unwrap();
    let at = |name: &str| &a[cols.iter().position(|c| *c == name).unwrap()];
    assert_eq!(at("year"), &json!(1999));
    assert_eq!(at("tags"), &json!("[\"x\",\"y\"]"));
    assert_eq!(at("meta"), &json!("{\"k\":1}"));
    let b = r.rows.iter().find(|row| row[0] == json!("B")).unwrap();
    let bt = |name: &str| &b[cols.iter().position(|c| *c == name).unwrap()];
    assert_eq!(bt("big"), &json!("12345678901234567890"));
    assert_eq!(bt("ok"), &json!(true));
    assert_eq!(bt("gone"), &json!(null));

    // Limit and the GSI.
    let mut out = QueryOutcome::default();
    s.execute(r#"SELECT * FROM "dbine_music""#, 1, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 1);
    assert!(out.results[0].truncated);
    let i = ObjectRef { kind: kinds::INDEX.into(), schema: Some("dbine_music".into()), name: "byYear".into() };
    let mut out = QueryOutcome::default();
    s.execute(&s.browse_query(&i, 10), 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2);
    assert_eq!(s.columns(&i).await.unwrap()[0].name, "year");

    // Errors from the server come back as Query.
    let mut out = QueryOutcome::default();
    assert!(matches!(s.execute("SELECT * FROM \"nope\"", 10, &mut out).await, Err(Error::Query(_))));

    // Read-only refuses writes.
    let mut ro = open(true).await.unwrap();
    let mut out = QueryOutcome::default();
    let e = ro.execute(r#"DELETE FROM "dbine_music" WHERE artist = 'A' AND song = 's1'"#, 10, &mut out).await;
    assert!(matches!(e, Err(Error::Query(_))));

    client.delete_table().table_name("dbine_music").send().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn derived_plans() {
    let Some(mut s) = open(false).await else { return };
    let url = std::env::var("DBINE_TEST_DYNAMODB_URL").unwrap();
    let conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .endpoint_url(url)
        .credentials_provider(aws_credential_types::Credentials::new("dummy", "dummy", None, None, "t"))
        .load()
        .await;
    let client = aws_sdk_dynamodb::Client::new(&conf);
    use aws_sdk_dynamodb::types::*;
    let _ = client.delete_table().table_name("dbine_plans").send().await;
    let attr = |n: &str, t: ScalarAttributeType| AttributeDefinition::builder().attribute_name(n).attribute_type(t).build().unwrap();
    let key = |n: &str, t: KeyType| KeySchemaElement::builder().attribute_name(n).key_type(t).build().unwrap();
    client
        .create_table()
        .table_name("dbine_plans")
        .billing_mode(BillingMode::PayPerRequest)
        .attribute_definitions(attr("pk", ScalarAttributeType::S))
        .attribute_definitions(attr("sk", ScalarAttributeType::N))
        .attribute_definitions(attr("st", ScalarAttributeType::S))
        .key_schema(key("pk", KeyType::Hash))
        .key_schema(key("sk", KeyType::Range))
        .global_secondary_indexes(
            GlobalSecondaryIndex::builder()
                .index_name("bySt")
                .key_schema(key("st", KeyType::Hash))
                .projection(Projection::builder().projection_type(ProjectionType::All).build())
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        r#"INSERT INTO "dbine_plans" VALUE {'pk': 'a', 'sk': 1, 'st': 'open'};
           INSERT INTO "dbine_plans" VALUE {'pk': 'a', 'sk': 2, 'st': 'done'};
           INSERT INTO "dbine_plans" VALUE {'pk': 'b', 'sk': 1, 'st': 'open'}"#,
        10,
        &mut out,
    )
    .await
    .unwrap();

    // Estimated: nothing runs, not even the INSERT.
    let mut out = QueryOutcome::default();
    s.explain(
        r#"SELECT * FROM "dbine_plans" WHERE pk = 'a' AND sk = 1;
           SELECT * FROM "dbine_plans" WHERE pk = 'a' AND st = 'open';
           SELECT * FROM "dbine_plans" WHERE st = 'open';
           SELECT * FROM "dbine_plans"."bySt" WHERE st = 'open';
           INSERT INTO "dbine_plans" VALUE {'pk': 'z', 'sk': 9}"#,
        false,
        10,
        &mut out,
    )
    .await
    .unwrap();
    assert!(out.results.is_empty());
    let leaf = |p: &dbine_driver::Plan| {
        let mut n = &p.root;
        while let Some(c) = n.children.first() {
            n = c;
        }
        n.op.clone()
    };
    let ops: Vec<String> = out.plans.iter().map(leaf).collect();
    assert_eq!(ops, ["GetItem", "Query", "Scan", "Query", "PutItem"]);
    assert_eq!(out.plans[1].root.children[0].op, "Filter");
    let mut o = QueryOutcome::default();
    s.execute(r#"SELECT * FROM "dbine_plans" WHERE pk = 'z'"#, 10, &mut o).await.unwrap();
    assert_eq!(o.results[0].rows.len(), 0, "the estimated plan must not run the INSERT");

    // Actual: runs once, with rows and consumed capacity.
    let mut out = QueryOutcome::default();
    s.explain(r#"SELECT * FROM "dbine_plans" WHERE st = 'open'"#, true, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2);
    let p = &out.plans[0];
    assert!(p.actual);
    let mut n = &p.root;
    while let Some(c) = n.children.first() {
        n = c;
    }
    assert_eq!(n.actual_rows, Some(2.0));
    assert!(n.props.iter().any(|(k, _)| k.starts_with("Capacidad consumida")), "{:?}", n.props);
    eprintln!("capacity props: {:?}", n.props);
    client.delete_table().table_name("dbine_plans").send().await.unwrap();
}

/// The designer's table through `table_ddl` + `execute`, read back with
/// `database_schema`, rows through `insert_script`, GSIs added and dropped.
#[tokio::test]
#[ignore]
async fn ddl_and_scripts() {
    use dbine_driver::{ColumnDef, DdlParts, IndexDef, TableSchema};
    use std::collections::BTreeMap;
    let Some(mut s) = open(false).await else { return };
    let d = dbine_driver_dynamodb::drivers().pop().unwrap();
    assert_eq!(d.designer().unwrap().kind, kinds::TABLE);
    let col = |n: &str, t: &str, k: &str| ColumnDef {
        name: n.into(),
        data_type: t.into(),
        options: BTreeMap::from([("key_type".to_string(), k.to_string())]),
        ..Default::default()
    };
    let table = TableSchema {
        name: "dbine_ddl".into(),
        columns: vec![col("customer", "S", "HASH"), col("date", "N", "RANGE"), col("status", "S", "none"), col("total", "N", "none")],
        indexes: vec![
            IndexDef { name: "byStatus".into(), columns: vec!["status".into(), "date".into()], kind: Some("GSI".into()), ..Default::default() },
            IndexDef { name: "byTotal".into(), columns: vec!["total".into()], kind: Some("LSI".into()), ..Default::default() },
        ],
        options: BTreeMap::from([
            ("billing_mode".to_string(), "PROVISIONED".to_string()),
            ("read_capacity".to_string(), "3".to_string()),
            ("write_capacity".to_string(), "2".to_string()),
            ("ttl_attribute".to_string(), "exp".to_string()),
            ("stream_view_type".to_string(), "NEW_AND_OLD_IMAGES".to_string()),
        ]),
        ..Default::default()
    };
    let all = DdlParts { drop: true, if_exists: true, create: true, indexes: true, ..Default::default() };
    let ddl = d.table_ddl(&table, all).unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&ddl, 10, &mut out).await.unwrap();
    assert!(out.messages.iter().any(|m| m.contains("creada")), "{:?}", out.messages);

    let schema = s.database_schema().await.unwrap();
    let t = schema.iter().find(|t| t.name == "dbine_ddl").expect("table in schema");
    assert_eq!(t.primary_key.as_ref().unwrap().columns, ["customer", "date"]);
    assert_eq!(t.columns[0].options["key_type"], "HASH");
    assert_eq!(t.columns[1].data_type, "N");
    let idx: Vec<(&str, Option<&str>, Vec<String>)> =
        t.indexes.iter().map(|i| (i.name.as_str(), i.kind.as_deref(), i.columns.clone())).collect();
    assert!(idx.contains(&("byStatus", Some("GSI"), vec!["status".into(), "date".into()])), "{idx:?}");
    assert!(idx.contains(&("byTotal", Some("LSI"), vec!["customer".into(), "total".into()])), "{idx:?}");
    assert_eq!(t.options["billing_mode"], "PROVISIONED");
    assert_eq!(t.options["read_capacity"], "3");
    assert_eq!(t.options["stream_view_type"], "NEW_AND_OLD_IMAGES");
    assert_eq!(t.options.get("ttl_attribute").map(String::as_str), Some("exp"), "{:?}", t.options);
    // What it reads back regenerates the same table.
    let again = d.table_ddl(t, DdlParts { create: true, indexes: true, ..Default::default() }).unwrap();
    let first = d.table_ddl(&table, DdlParts { create: true, indexes: true, ..Default::default() }).unwrap();
    assert_eq!(again, first);

    // Rows through insert_script.
    let cols = vec!["customer".to_string(), "date".into(), "status".into(), "total".into(), "note".into(), "meta".into()];
    let rows = vec![
        vec![json!("ana"), json!(20240101), json!("open"), json!(10.5), json!("it's"), json!({ "tags": ["a", "b"], "n": 1 })],
        vec![json!("bob"), json!(20240102), json!("done"), json!(3), json!(null), json!(null)],
    ];
    let ins = d.insert_script(&ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "dbine_ddl".into() }, &cols, &rows).unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&ins, 10, &mut out).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(r#"SELECT * FROM "dbine_ddl"."byStatus" WHERE status = 'open'"#, 10, &mut out).await.unwrap();
    let r = &out.results[0];
    assert_eq!(r.rows.len(), 1);
    let at = |n: &str| r.rows[0][r.columns.iter().position(|c| c.name == n).unwrap()].clone();
    assert_eq!(at("note"), json!("it's"));
    assert_eq!(at("total"), json!(10.5));
    assert_eq!(at("meta"), json!("{\"n\":1,\"tags\":[\"a\",\"b\"]}"));

    // A GSI dropped and added again with the driver's own syntax.
    let mut out = QueryOutcome::default();
    s.execute(r#"DROP INDEX "byStatus" ON "dbine_ddl""#, 10, &mut out).await.unwrap();
    let only_gsi = TableSchema { indexes: vec![table.indexes[0].clone()], ..table.clone() };
    let add = d.table_ddl(&only_gsi, DdlParts { indexes: true, ..Default::default() }).unwrap();
    assert!(add.starts_with("CREATE INDEX"), "{add}");
    let mut out = QueryOutcome::default();
    s.execute(&add, 10, &mut out).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(r#"SELECT * FROM "dbine_ddl"."byStatus" WHERE status = 'done'"#, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 1);

    // Read-only refuses the extensions.
    let mut ro = open(true).await.unwrap();
    let mut out = QueryOutcome::default();
    assert!(ro.execute(r#"DROP TABLE "dbine_ddl""#, 10, &mut out).await.is_err());

    // Templates run as they are.
    for tpl in d.create_templates().iter().filter(|t| t.kind == kinds::TABLE) {
        let text = tpl.template.replace("{name}", "dbine_tpl");
        let mut out = QueryOutcome::default();
        s.execute(&text, 10, &mut out).await.unwrap();
        let mut out = QueryOutcome::default();
        s.execute(r#"DROP TABLE "dbine_tpl""#, 10, &mut out).await.unwrap();
    }

    let mut out = QueryOutcome::default();
    s.execute("DROP TABLE \"dbine_ddl\";\nDROP TABLE IF EXISTS \"dbine_ddl\"", 10, &mut out).await.unwrap();
    assert!(out.messages.iter().any(|m| m.contains("no existe")), "{:?}", out.messages);
    assert!(!d.capabilities().create_database);
}

#[tokio::test]
#[ignore]
async fn monitor() {
    let Some(mut s) = open(false).await else { return };
    let url = std::env::var("DBINE_TEST_DYNAMODB_URL").unwrap();
    let conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .endpoint_url(url)
        .credentials_provider(aws_credential_types::Credentials::new("dummy", "dummy", None, None, "t"))
        .load()
        .await;
    let client = aws_sdk_dynamodb::Client::new(&conf);
    use aws_sdk_dynamodb::types::*;
    let _ = client.delete_table().table_name("dbine_mon").send().await;
    client
        .create_table()
        .table_name("dbine_mon")
        .provisioned_throughput(ProvisionedThroughput::builder().read_capacity_units(5).write_capacity_units(3).build().unwrap())
        .attribute_definitions(AttributeDefinition::builder().attribute_name("k").attribute_type(ScalarAttributeType::S).build().unwrap())
        .attribute_definitions(AttributeDefinition::builder().attribute_name("g").attribute_type(ScalarAttributeType::S).build().unwrap())
        .key_schema(KeySchemaElement::builder().attribute_name("k").key_type(KeyType::Hash).build().unwrap())
        .global_secondary_indexes(
            GlobalSecondaryIndex::builder()
                .index_name("byG")
                .key_schema(KeySchemaElement::builder().attribute_name("g").key_type(KeyType::Hash).build().unwrap())
                .projection(Projection::builder().projection_type(ProjectionType::All).build())
                .provisioned_throughput(ProvisionedThroughput::builder().read_capacity_units(2).write_capacity_units(1).build().unwrap())
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let mut out = QueryOutcome::default();
    s.execute("INSERT INTO \"dbine_mon\" VALUE {'k': 'a', 'g': 'x', 'v': 1}", 10, &mut out).await.unwrap();

    let snap = s.monitor().await.unwrap();
    for m in &snap.metrics {
        println!("{:<18} {:?} max={:?}", m.key, m.value, m.max);
    }
    for t in &snap.tables {
        println!("[{}] {:?}", t.key, t.rows);
    }
    println!("info {:?}\nnotes {:?}", snap.info, snap.notes);
    let val = |k: &str| snap.metrics.iter().find(|m| m.key == k).unwrap().value;
    assert!(val("tables").unwrap() >= 1.0);
    assert!(val("provisioned_read").unwrap() >= 7.0);
    assert!(val("provisioned_write").unwrap() >= 4.0);
    assert!(val("items").unwrap() >= 1.0);
    let t = snap.tables.iter().find(|t| t.key == "top_objects").unwrap();
    let row = t.rows.iter().find(|r| r[0] == json!("dbine_mon")).unwrap();
    assert_eq!(row[5], json!(5));
    let ix = snap.tables.iter().find(|t| t.key == "indexes").unwrap();
    assert!(ix.rows.iter().any(|r| r[1] == json!("byG")));
    client.delete_table().table_name("dbine_mon").send().await.unwrap();
}

/// Schema sync: a table read back from `database_schema` gets its GSI
/// swapped, another is dropped and one is created; the script runs.
#[tokio::test]
#[ignore]
async fn schema_sync() {
    use dbine_driver::{IndexDef, TableChange};
    let Some(mut s) = open(false).await else { return };
    let d = dbine_driver_dynamodb::drivers().pop().unwrap();
    assert!(d.supports_schema_sync());
    let mut out = QueryOutcome::default();
    for t in ["sync_orders", "sync_gone", "sync_fresh"] {
        let _ = s.execute(&format!("DROP TABLE IF EXISTS \"{t}\""), 10, &mut out).await;
    }
    s.execute(
        r#"CREATE TABLE "sync_orders" {
             "AttributeDefinitions": [{"AttributeName": "pk", "AttributeType": "S"}, {"AttributeName": "status", "AttributeType": "S"}],
             "KeySchema": [{"AttributeName": "pk", "KeyType": "HASH"}],
             "BillingMode": "PAY_PER_REQUEST",
             "GlobalSecondaryIndexes": [{"IndexName": "by_status", "KeySchema": [{"AttributeName": "status", "KeyType": "HASH"}], "Projection": {"ProjectionType": "ALL"}}]
           };
           CREATE TABLE "sync_gone" {"AttributeDefinitions": [{"AttributeName": "id", "AttributeType": "S"}], "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}], "BillingMode": "PAY_PER_REQUEST"};"#,
        10,
        &mut out,
    )
    .await
    .unwrap();
    let schema = s.database_schema().await.unwrap();
    let old = schema.iter().find(|t| t.name == "sync_orders").unwrap().clone();
    let gone = schema.iter().find(|t| t.name == "sync_gone").unwrap().clone();
    let mut new = old.clone();
    new.columns.push(dbine_driver::ColumnDef { name: "customer".into(), data_type: "S".into(), nullable: true, options: [("key_type".to_string(), "none".to_string())].into(), ..Default::default() });
    new.indexes = vec![IndexDef { name: "by_customer".into(), columns: vec!["customer".into()], kind: Some("GSI".into()), ..Default::default() }];
    let mut created = old.clone();
    created.name = "sync_fresh".into();
    let script = d.sync_script(&[TableChange::Alter { old, new }, TableChange::Drop { table: gone }, TableChange::Create { table: created }]).unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        let mut out = QueryOutcome::default();
        s.execute(st, 10, &mut out).await.unwrap_or_else(|e| panic!("{st}: {e}"));
    }
    let after = s.database_schema().await.unwrap();
    assert!(!after.iter().any(|t| t.name == "sync_gone"));
    let fresh = after.iter().find(|t| t.name == "sync_fresh").unwrap();
    assert_eq!(fresh.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), vec!["by_status"]);
    let orders = after.iter().find(|t| t.name == "sync_orders").unwrap();
    assert_eq!(orders.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), vec!["by_customer"]);
    for t in ["sync_orders", "sync_fresh"] {
        let _ = s.execute(&format!("DROP TABLE IF EXISTS \"{t}\""), 10, &mut out).await;
    }
}

/// The data-compare delete script removes exactly the keyed items.
#[tokio::test]
#[ignore]
async fn delete_script_runs() {
    use dbine_driver::{ColumnDef, DdlParts, TableSchema};
    use std::collections::BTreeMap;
    let Some(mut s) = open(false).await else { return };
    let d = dbine_driver_dynamodb::drivers().pop().unwrap();
    let col = |n: &str, t: &str, k: &str| ColumnDef {
        name: n.into(),
        data_type: t.into(),
        options: BTreeMap::from([("key_type".to_string(), k.to_string())]),
        ..Default::default()
    };
    let table = TableSchema { name: "dbine_del".into(), columns: vec![col("pk", "S", "HASH"), col("sk", "N", "RANGE")], ..Default::default() };
    let ddl = d.table_ddl(&table, DdlParts { drop: true, if_exists: true, create: true, ..Default::default() }).unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&ddl, 10, &mut out).await.unwrap();
    let rows = vec![vec![json!("O'Brien \"Bob\""), json!(1)], vec![json!("O'Brien \"Bob\""), json!(2)], vec![json!("b"), json!(1)]];
    let obj = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "dbine_del".into() };
    let ins = d.insert_script(&obj, &["pk".to_string(), "sk".into()], &rows).unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&ins, 10, &mut out).await.unwrap();
    let keys = vec![vec![("pk".to_string(), json!("O'Brien \"Bob\"")), ("sk".to_string(), json!(1))], vec![("pk".to_string(), json!("b")), ("sk".to_string(), json!(1))]];
    let script = d.delete_script(&obj, &keys).unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&script, 10, &mut out).await.expect(&script);
    let mut out = QueryOutcome::default();
    s.execute("SELECT * FROM \"dbine_del\"", 10, &mut out).await.unwrap();
    let r = &out.results[0];
    assert_eq!(r.rows.len(), 1, "{:?}", r.rows);
    let rows = serde_json::to_string(&r.rows).unwrap();
    assert!(rows.contains("Brien") && rows.contains('2'), "{rows}");
    let mut out = QueryOutcome::default();
    s.execute("DROP TABLE \"dbine_del\";", 10, &mut out).await.ok();
}
