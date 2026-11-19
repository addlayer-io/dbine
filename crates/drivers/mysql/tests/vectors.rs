//! Vectors (embeddings) against real servers, one test per engine: a
//! table with a vector column and a vector index made from a `TableSchema`
//! through the driver's own DDL (as the designer would), rows inserted with
//! `insert_script`, read back (query and table browse) with the column
//! marked as vectors and the cells whole, the describe and the index read
//! back, a similarity query, an `update_script`, and a transfer (same
//! engine, and pgvector-shaped text cells as another family's source).
//!
//! Each test reads `DBINE_TEST_<ENGINE>_URL` (`mysql://user:pass@host:port`)
//! and is skipped without it; what it creates is dropped first and at the
//! end, also when it fails half-way.
//!
//! ```sh
//! docker start dbine-test-mysql9 dbine-test-mariadb dbine-test-tidb8
//! DBINE_TEST_MYSQL9_URL=mysql://root:pw@127.0.0.1:27201 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@127.0.0.1:25012 \
//! DBINE_TEST_TIDB8_URL=mysql://root@127.0.0.1:25044 \
//!   cargo test -p dbine-driver-mysql --test vectors -- --ignored --nocapture
//! ```

use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    kinds, ColumnDef, ConnectionConfig, DdlParts, Driver, IndexDef, KeyDef, ObjectRef,
    QueryOutcome, RowChange, Session, TableSchema, VectorIndex,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};

fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport
        .rsplit_once(':')
        .map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_mysql::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10_000, &mut out)
        .await
        .map_err(|e| e.to_string())?;
    match out.error.take() {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

async fn must(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    run(s, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn table(name: &str) -> ObjectRef {
    ObjectRef {
        kind: kinds::TABLE.into(),
        schema: None,
        name: name.into(),
    }
}

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    batches: Vec<RowBatch>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        self.batches.push(b);
        Ok(())
    }
}

struct Source(VecDeque<RowBatch>);

#[dbine_driver::async_trait]
impl BatchSource for Source {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.pop_front()
    }
}

async fn read_cells(s: &mut Box<dyn Session>, name: &str) -> (Vec<TransferColumn>, Vec<Vec<Cell>>) {
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(
        &ReadSpec {
            table: table(name),
            columns: None,
            filter: None,
        },
        sink.clone(),
    )
    .await
    .expect("read_batches");
    let c = std::mem::take(&mut *sink.lock().unwrap());
    let mut rows: Vec<Vec<Cell>> = c.batches.into_iter().flat_map(|b| b.rows).collect();
    rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        Cell::UInt(u) => u as i64,
        _ => 0,
    });
    (c.columns, rows)
}

async fn load(
    s: &mut Box<dyn Session>,
    name: &str,
    columns: &[TransferColumn],
    rows: Vec<Vec<Cell>>,
) {
    let spec = LoadSpec {
        table: table(name),
        columns: columns.iter().map(|c| c.name.clone()).collect(),
        table_lock: false,
        keep_identity: true,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let bytes = rows.iter().flatten().map(Cell::size).sum();
    let mut source = Source([RowBatch { rows, bytes }].into_iter().collect());
    s.bulk_load(&spec, columns, &mut source, &|_| {})
        .await
        .unwrap_or_else(|e| panic!("bulk_load {name}: {e}"));
}

/// One engine's case.
struct Case {
    id: &'static str,
    env: &'static str,
    /// The database to work in (created and dropped), `None`: the server's
    /// only namespace (Manticore).
    db: Option<&'static str>,
    /// The vector column's type, as the designer writes it and the
    /// describe reads it back.
    column_type: &'static str,
    described: &'static str,
    /// The vector index the designer would make (`None`: the engine has
    /// none, or this server can't build it).
    index: Option<IndexDef>,
    /// What the catalog reads back of it.
    index_back: Option<VectorIndex>,
    /// What the DDL made from the read-back table must have (the engine's
    /// vector index syntax).
    marker: &'static str,
    /// Ordered by similarity to [1,0,0], ids in order.
    similarity: &'static str,
    /// The result header's type of the vector column.
    header: &'static str,
    /// Table options the engine needs (key model, time index…).
    options: Vec<(&'static str, &'static str)>,
    /// `update_script` works (GreptimeDB has no UPDATE).
    update: bool,
    /// Bulk load works (Manticore has none).
    transfer: bool,
}

fn vindex(
    name: &str,
    algorithm: Option<&str>,
    metric: Option<&str>,
    params: &[(&str, &str)],
) -> IndexDef {
    IndexDef {
        name: name.into(),
        columns: vec!["v".into()],
        vector: Some(VectorIndex {
            algorithm: algorithm.map(Into::into),
            metric: metric.map(Into::into),
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }),
        ..Default::default()
    }
}

fn vi(algorithm: Option<&str>, metric: Option<&str>, params: &[(&str, &str)]) -> VectorIndex {
    vindex("", algorithm, metric, params).vector.unwrap()
}

const ROWS: [(i64, [f64; 3]); 3] = [
    (1, [1.0, 0.0, 0.0]),
    (2, [0.5, 0.5, 0.0]),
    (3, [-1.0, 2.5, 0.125]),
];

fn schema(case: &Case, name: &str, with_index: bool) -> TableSchema {
    let mut cols = vec![
        ColumnDef {
            name: "id".into(),
            data_type: if case.id == "manticore" {
                "bigint"
            } else {
                "INT"
            }
            .into(),
            nullable: false,
            ..Default::default()
        },
        ColumnDef {
            name: "v".into(),
            data_type: case.column_type.into(),
            nullable: false,
            ..Default::default()
        },
    ];
    if case.id == "greptimedb" {
        cols.push(ColumnDef {
            name: "ts".into(),
            data_type: "TIMESTAMP(3)".into(),
            nullable: false,
            default_value: Some("'2024-01-01 00:00:00'".into()),
            ..Default::default()
        });
    }
    if case.id == "manticore" {
        // `id` is implicit.
        cols.remove(0);
    }
    TableSchema {
        kind: kinds::TABLE.into(),
        name: name.into(),
        columns: cols,
        primary_key: (!matches!(case.id, "manticore" | "greptimedb")).then(|| KeyDef {
            name: None,
            columns: vec!["id".into()],
        }),
        indexes: case
            .index
            .iter()
            .filter(|_| with_index)
            .cloned()
            .map(|mut ix| {
                if case.id == "manticore" {
                    ix.name = "v".into();
                }
                ix
            })
            .collect(),
        options: case
            .options
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    }
}

async fn cleanup(admin: &mut Box<dyn Session>, s: Option<&mut Box<dyn Session>>, case: &Case) {
    match case.db {
        Some(db) => {
            let _ = run(admin, &format!("DROP DATABASE IF EXISTS {db}")).await;
        }
        None => {
            if let Some(s) = s {
                for t in ["vec_items", "vec_copy", "vec_pg"] {
                    let _ = run(s, &format!("DROP TABLE IF EXISTS {t}")).await;
                }
            }
        }
    }
}

async fn exercise(case: Case) {
    let Ok(url) = std::env::var(case.env) else {
        eprintln!("{} not set; skipping", case.env);
        return;
    };
    let d = driver(case.id);
    let cfg = parse_url(case.id, &url);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    eprintln!(
        "{}: {}",
        case.id,
        admin.server_version().await.unwrap_or_default()
    );
    cleanup(&mut admin, None, &case).await;
    if let Some(db) = case.db {
        must(&mut admin, &format!("CREATE DATABASE {db}")).await;
    }
    let mut s = d
        .connect(&cfg, case.db)
        .await
        .expect("connect to the test database");
    if case.db.is_none() {
        cleanup(&mut admin, Some(&mut s), &case).await;
    }
    // Whatever fails below, the database goes.
    let outcome = tokio::spawn(steps(case.id, url.clone(), case.db, CaseData::from(&case))).await;
    cleanup(&mut admin, Some(&mut s), &case).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

/// What `steps` needs of a case (it runs in its own task, so that a
/// failure still leaves the cleanup to the caller).
struct CaseData {
    schema: TableSchema,
    copy: TableSchema,
    described: String,
    index_back: Option<VectorIndex>,
    marker: String,
    similarity: String,
    header: String,
    update: bool,
    transfer: bool,
}

impl From<&Case> for CaseData {
    fn from(c: &Case) -> Self {
        CaseData {
            schema: schema(c, "vec_items", true),
            copy: schema(c, "vec_copy", false),
            described: c.described.into(),
            index_back: c.index_back.clone(),
            marker: c.marker.into(),
            similarity: c.similarity.into(),
            header: c.header.into(),
            update: c.update,
            transfer: c.transfer,
        }
    }
}

async fn steps(id: &'static str, url: String, db: Option<&'static str>, c: CaseData) {
    let d = driver(id);
    let cfg = parse_url(id, &url);
    let mut s = d.connect(&cfg, db).await.expect("connect");
    let all = DdlParts {
        create: true,
        indexes: true,
        ..Default::default()
    };

    // 1. The table and its vector index, from the designer's schema.
    let ddl = d.table_ddl(&c.schema, all).expect("table_ddl");
    eprintln!("{id}: {ddl}");
    assert!(
        !ddl.contains("CREATE INDEX `iv` ON `vec_items` (`v`);"),
        "{ddl}"
    );
    for stmt in ddl.split(";\n") {
        let stmt = stmt.trim().trim_end_matches(';');
        if !stmt.is_empty() && !stmt.starts_with("--") {
            must(&mut s, stmt).await;
        }
    }

    // 2. Rows from insert_script, vectors as JSON arrays.
    let mut columns: Vec<String> = vec!["id".into(), "v".into()];
    let rows: Vec<Vec<Value>> = ROWS.iter().map(|(i, v)| vec![json!(i), json!(v)]).collect();
    if id == "greptimedb" {
        columns.push("ts".into());
    }
    let rows: Vec<Vec<Value>> = rows
        .into_iter()
        .enumerate()
        .map(|(n, mut r)| {
            if id == "greptimedb" {
                r.push(json!(format!("2024-01-01 00:00:0{n}")));
            }
            r
        })
        .collect();
    let ins = d
        .insert_script(&table("vec_items"), &columns, &rows)
        .expect("insert_script");
    eprintln!("{id}: {ins}");
    for stmt in ins.split(";\n") {
        let stmt = stmt.trim().trim_end_matches(';');
        if !stmt.is_empty() {
            must(&mut s, stmt).await;
        }
    }
    if matches!(id, "doris" | "starrocks") {
        // Inserts land asynchronously on the OLAP engines' visibility.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    let expect = ["[1,0,0]", "[0.5,0.5,0]", "[-1,2.5,0.125]"];

    // 3. Read back: a query and the table browse.
    for sql in [
        // Manticore doesn't sort by a quoted name; GreptimeDB needs `id` quoted.
        if id == "manticore" {
            "SELECT id, v FROM vec_items ORDER BY id ASC".to_string()
        } else {
            "SELECT `id`, `v` FROM `vec_items` ORDER BY `id`".to_string()
        },
        s.browse_query(&table("vec_items"), 100),
    ] {
        let out = must(&mut s, &sql).await;
        let r = &out.results[0];
        let vi = r
            .columns
            .iter()
            .position(|c| c.name == "v")
            .expect("v column");
        let ii = r
            .columns
            .iter()
            .position(|c| c.name == "id")
            .expect("id column");
        let col = &r.columns[vi];
        let meta = col
            .vector
            .as_ref()
            .unwrap_or_else(|| panic!("{sql}: {col:?}"));
        assert!(!meta.sparse);
        assert_eq!(
            col.type_name.to_ascii_lowercase(),
            c.header.to_ascii_lowercase(),
            "{sql}"
        );
        let mut cells: Vec<(String, Value)> = r
            .rows
            .iter()
            .map(|row| (row[ii].to_string(), row[vi].clone()))
            .collect();
        cells.sort_by(|a, b| a.0.cmp(&b.0));
        let cells: Vec<Value> = cells.into_iter().map(|(_, v)| v).collect();
        assert_eq!(cells, expect.map(|e| json!(e)), "{sql}");
    }

    // 4. The describe keeps the type with its dimension; 5. the index, its
    // vector settings.
    let schema = s.database_schema().await.expect("database_schema");
    let t = schema
        .iter()
        .find(|t| t.name == "vec_items")
        .expect("table described");
    let v = t.columns.iter().find(|c| c.name == "v").unwrap();
    assert_eq!(
        v.data_type.to_ascii_lowercase(),
        c.described.to_ascii_lowercase()
    );
    if let Some(back) = &c.index_back {
        let ix = t
            .indexes
            .iter()
            .find(|i| i.vector.is_some())
            .unwrap_or_else(|| panic!("no vector index read back: {:?}", t.indexes));
        assert_eq!(ix.columns, ["v"]);
        assert_eq!(ix.vector.as_ref(), Some(back), "{ix:?}");
        // The DDL made from what was read is the same index again.
        let again = d.table_ddl(t, all).unwrap();
        assert!(again.contains(&c.marker), "{again}");
        assert!(!again.contains("\n-- "), "{again}");
    }

    // 6. Similarity.
    let out = must(&mut s, &c.similarity).await;
    let ids: Vec<i64> = out.results[0]
        .rows
        .iter()
        .map(|r| {
            r[0].as_i64()
                .or_else(|| r[0].as_str().and_then(|x| x.parse().ok()))
                .unwrap()
        })
        .collect();
    assert_eq!(ids.first(), Some(&1), "{ids:?}");

    // 7. update_script.
    if c.update {
        let up = d
            .update_script(
                &table("vec_items"),
                &[RowChange {
                    key: vec![("id".into(), json!(2))],
                    set: vec![("v".into(), json!([0.25, -4, 8]))],
                    row: Vec::new(),
                }],
            )
            .expect("update_script");
        eprintln!("{id}: {up}");
        must(&mut s, up.trim_end_matches(';')).await;
        if matches!(id, "doris" | "starrocks") {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let out = must(&mut s, "SELECT `v` FROM `vec_items` WHERE `id` = 2").await;
        assert_eq!(out.results[0].rows[0][0], json!("[0.25,-4,8]"));
    }

    // 8. Transfer: read, load into a copy, read the copy; then text cells
    // as pgvector's reader sends them.
    // Manticore has no bulk load (its moves write insert_script): its read
    // is checked.
    if !c.transfer {
        let (cols, rows) = read_cells(&mut s, "vec_items").await;
        let vpos = cols.iter().position(|c| c.name == "v").unwrap();
        assert!(
            matches!(&rows[0][vpos], Cell::Text(t) if t == "[1,0,0]"),
            "{:?}",
            rows[0][vpos]
        );
    }
    if c.transfer {
        let (cols, rows) = read_cells(&mut s, "vec_items").await;
        let vpos = cols.iter().position(|c| c.name == "v").unwrap();
        assert!(
            matches!(&rows[0][vpos], Cell::Text(t) if t == "[1,0,0]"),
            "{:?}",
            rows[0][vpos]
        );
        let ddl = d
            .table_ddl(
                &c.copy,
                DdlParts {
                    create: true,
                    ..Default::default()
                },
            )
            .unwrap();
        for stmt in ddl.split(";\n") {
            let stmt = stmt.trim().trim_end_matches(';');
            if !stmt.is_empty() {
                must(&mut s, stmt).await;
            }
        }
        load(&mut s, "vec_copy", &cols, rows.clone()).await;
        if matches!(id, "doris" | "starrocks") {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let (_, back) = read_cells(&mut s, "vec_copy").await;
        assert_eq!(back, rows);
        // Another family's source: pgvector's text, with its spacing.
        must(&mut s, "DELETE FROM `vec_copy` WHERE `id` > 0").await;
        if matches!(id, "doris" | "starrocks") {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let pg: Vec<Vec<Cell>> = rows
            .iter()
            .enumerate()
            .map(|(n, r)| {
                let mut r = r.clone();
                r[vpos] = Cell::Text(["[1,0,0]", "[0.5, 0.5, 0]", "[-1,2.5,0.125]"][n].into());
                r
            })
            .collect();
        load(&mut s, "vec_copy", &cols, pg).await;
        if matches!(id, "doris" | "starrocks") {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let (_, back) = read_cells(&mut s, "vec_copy").await;
        let got: Vec<&Cell> = back.iter().map(|r| &r[vpos]).collect();
        let want: Vec<Cell> = ["[1,0,0]", "[0.5,0.5,0]", "[-1,2.5,0.125]"]
            .iter()
            .map(|t| Cell::Text(t.to_string()))
            .collect();
        assert_eq!(got, want.iter().collect::<Vec<_>>());
    }
}

#[tokio::test]
#[ignore]
async fn mysql9_vectors() {
    exercise(Case {
        id: "mysql",
        env: "DBINE_TEST_MYSQL9_URL",
        db: Some("dbine_vec"),
        column_type: "vector(3)",
        described: "vector(3)",
        // MySQL Community has no vector index.
        index: None,
        index_back: None,
        marker: "`v` vector(3) NOT NULL",
        // Community has no distance function: a dot product over the
        // vector's text.
        similarity: "SELECT id FROM vec_items, JSON_TABLE(VECTOR_TO_STRING(v), '$[*]' \
                     COLUMNS(n FOR ORDINALITY, x DOUBLE PATH '$')) e GROUP BY id \
                     ORDER BY SUM(x * JSON_EXTRACT('[1,0,0]', CONCAT('$[', n - 1, ']'))) DESC",
        header: "vector(3)",
        options: vec![],
        update: true,
        transfer: true,
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn mariadb_vectors() {
    exercise(Case {
        id: "mariadb",
        env: "DBINE_TEST_MARIADB_URL",
        db: Some("dbine_vec"),
        column_type: "vector(3)",
        described: "vector(3)",
        index: Some(vindex("iv", None, Some("cosine"), &[("m", "8")])),
        index_back: Some(vi(None, Some("cosine"), &[("m", "8")])),
        marker: "CREATE VECTOR INDEX `iv` ON `vec_items` (`v`) M=8 DISTANCE=cosine;",
        similarity: "SELECT id FROM vec_items ORDER BY VEC_DISTANCE_COSINE(v, VEC_FromText('[1,0,0]')) LIMIT 3",
        header: "vector(3)",
        options: vec![],
        update: true,
        transfer: true,
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn tidb_vectors() {
    exercise(Case {
        id: "tidb",
        env: "DBINE_TEST_TIDB8_URL",
        db: Some("dbine_vec"),
        column_type: "vector(3)",
        described: "vector(3)",
        // The container has no TiFlash, which vector indexes need: their
        // DDL is covered by the unit tests.
        index: None,
        index_back: None,
        marker: "`v` vector(3) NOT NULL",
        similarity: "SELECT id FROM vec_items ORDER BY VEC_COSINE_DISTANCE(v, '[1,0,0]') LIMIT 3",
        header: "vector(3)",
        options: vec![],
        update: true,
        transfer: true,
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn greptimedb_vectors() {
    exercise(Case {
        id: "greptimedb",
        env: "DBINE_TEST_GREPTIMEDB_URL",
        db: Some("dbine_vec"),
        column_type: "VECTOR(3)",
        described: "vector(3)",
        index: Some(vindex("iv", None, Some("cosine"), &[("m", "16")])),
        // Its defaults come back written out.
        index_back: Some(vi(
            None,
            Some("cosine"),
            &[("m", "16"), ("ef_construction", "128"), ("ef_search", "64")],
        )),
        marker: "VECTOR INDEX WITH(metric = 'cosine', expansion_add = '128', expansion_search = '64', connectivity = '16')",
        similarity: "SELECT `id` FROM vec_items ORDER BY vec_cos_distance(v, '[1,0,0]') LIMIT 3",
        header: "vector(3)",
        options: vec![],
        // No UPDATE: a row is replaced by inserting it again.
        update: false,
        transfer: true,
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn manticore_vectors() {
    exercise(Case {
        id: "manticore",
        env: "DBINE_TEST_MANTICORE_URL",
        db: None,
        column_type: "float_vector",
        described: "float_vector",
        index: Some(vindex("v", Some("hnsw"), Some("l2"), &[("knn_dims", "3")])),
        index_back: Some(vi(Some("hnsw"), Some("l2"), &[("knn_dims", "3")])),
        marker: "`v` float_vector knn_type='hnsw' knn_dims='3' hnsw_similarity='L2'",
        similarity: "SELECT id FROM vec_items WHERE knn(v, 3, (1,0,0))",
        header: "float_vector",
        options: vec![],
        update: true,
        transfer: false,
    })
    .await;
}
