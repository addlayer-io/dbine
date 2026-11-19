//! "Clonar tabla" of tables with vectors on PostgreSQL's family, against
//! real servers: the vector columns keep their dimension and the vector
//! indexes their algorithm, distance and parameters.
//!
//! Ignored by default; each test is skipped without its URL:
//!
//! ```sh
//! DBINE_TEST_PGVECTOR_URL=postgres://postgres:pw@localhost:27200/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//!   cargo test -p dbine-transfer --test clone_vectors_pg -- --ignored --nocapture --test-threads 1
//! ```
//!
//! Each makes the schema `clonevec` (dropped first and at the end).

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session};
use dbine_transfer::clone_table::{
    clone_table, CloneControl, CloneOptions, CloneRequest, ConfigEndpoints,
};
use dbine_transfer::Endpoints;
use std::sync::Arc;

const SCHEMA: &str = "clonevec";

fn endpoints(driver: &str, url: &str) -> Arc<ConfigEndpoints> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap_or((auth, ""));
    let (hostport, db) = hostport.split_once('/').unwrap_or((hostport, "postgres"));
    let (host, port) = hostport.rsplit_once(':').unwrap();
    let config = ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().unwrap(),
        database: db.into(),
        username: Some(user.into()),
        password: (!pass.is_empty()).then(|| pass.to_string()),
        ..Default::default()
    };
    Arc::new(ConfigEndpoints {
        driver: dbine_drivers::find(driver).unwrap().clone(),
        config,
        database: Some(db.to_string()),
    })
}

async fn rows(s: &mut dyn Session, sql: &str) -> Vec<Vec<String>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100_000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{e}\n{sql}"));
    if let Some(e) = out.error {
        panic!("{e}\n{sql}");
    }
    out.results
        .iter()
        .flat_map(|r| r.rows.iter())
        .map(|r| {
            r.iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string())
                })
                .collect()
        })
        .collect()
}

/// Clones `items` as `items_c` and compares the copy's columns, vector
/// indexes and rows with the original's.
async fn clone_round_trip(e: Arc<ConfigEndpoints>, setup: &str, drop: &str) {
    let mut w = e.open_target().await.unwrap();
    let s = &mut *w;
    let _ = rows(s, drop).await;
    let body = async {
        rows(s, setup).await;
        let req = CloneRequest {
            source: ObjectRef {
                kind: "table".into(),
                schema: Some(SCHEMA.into()),
                name: "items".into(),
            },
            new_name: "items_c".into(),
            options: CloneOptions {
                with_data: true,
                with_indexes: true,
            },
        };
        let r = clone_table(
            e.clone() as Arc<dyn Endpoints>,
            req,
            &CloneControl::default(),
            |_| {},
        )
        .await
        .unwrap_or_else(|e| panic!("clone: {e}"));
        println!("{r:?}");
        assert_eq!(r.rows, 2);
        let schema = s.database_schema().await.unwrap();
        let find = |name: &str| {
            schema
                .iter()
                .find(|t| t.schema.as_deref() == Some(SCHEMA) && t.name == name)
                .unwrap_or_else(|| panic!("{name}"))
                .clone()
        };
        let (a, b) = (find("items"), find("items_c"));
        let types = |t: &dbine_driver::TableSchema| {
            t.columns
                .iter()
                .map(|c| (c.name.clone(), c.data_type.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(types(&a), types(&b));
        let vectors = |t: &dbine_driver::TableSchema| {
            let mut v: Vec<_> = t
                .indexes
                .iter()
                .filter_map(|i| Some((i.columns.clone(), i.vector.clone()?)))
                .collect();
            v.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
            v
        };
        assert!(!vectors(&a).is_empty());
        assert_eq!(vectors(&a), vectors(&b));
        let data = |t: &str| format!("SELECT id, v::text FROM {SCHEMA}.{t} ORDER BY id");
        assert_eq!(
            rows(s, &data("items")).await,
            rows(s, &data("items_c")).await
        );
    };
    let res = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(body)).await;
    let _ = rows(s, drop).await;
    if let Err(p) = res {
        std::panic::resume_unwind(p);
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pgvector_clone_keeps_vectors() {
    let Ok(url) = std::env::var("DBINE_TEST_PGVECTOR_URL") else {
        eprintln!("DBINE_TEST_PGVECTOR_URL not set; skipping");
        return;
    };
    let e = endpoints("postgres", &url);
    let setup = format!(
        "CREATE EXTENSION IF NOT EXISTS vector; CREATE SCHEMA {SCHEMA};
         CREATE TABLE {SCHEMA}.items (id int PRIMARY KEY, v vector(3), h halfvec(3), s sparsevec(5));
         CREATE INDEX items_v ON {SCHEMA}.items USING hnsw (v vector_cosine_ops) WITH (m = 12, ef_construction = 40);
         CREATE INDEX items_h ON {SCHEMA}.items USING ivfflat (h halfvec_ip_ops) WITH (lists = 1);
         CREATE INDEX items_s ON {SCHEMA}.items USING hnsw (s sparsevec_l1_ops);
         CREATE INDEX items_q ON {SCHEMA}.items USING hnsw ((binary_quantize(v)::bit(3)) bit_hamming_ops);
         INSERT INTO {SCHEMA}.items VALUES (1, '[1,2.5,-3]', '[1,2,3]', '{{1:1}}/5'), (2, '[0,1,0]', '[0,1,0]', '{{2:2}}/5');"
    );
    clone_round_trip(
        e,
        &setup,
        &format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cockroach_clone_keeps_vectors() {
    let Ok(url) = std::env::var("DBINE_TEST_COCKROACH_URL") else {
        eprintln!("DBINE_TEST_COCKROACH_URL not set; skipping");
        return;
    };
    let e = endpoints("cockroachdb", &url);
    let setup = format!(
        "CREATE SCHEMA {SCHEMA};
         CREATE TABLE {SCHEMA}.items (id INT8 PRIMARY KEY, uid INT8, v VECTOR(3));
         CREATE VECTOR INDEX items_v ON {SCHEMA}.items (v vector_cosine_ops);
         CREATE VECTOR INDEX items_uv ON {SCHEMA}.items (uid, v) WITH (min_partition_size = 16);
         INSERT INTO {SCHEMA}.items VALUES (1, 7, '[1,2.5,-3]'), (2, 8, '[0,1,0]');"
    );
    clone_round_trip(
        e,
        &setup,
        &format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE"),
    )
    .await;
}
