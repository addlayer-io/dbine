//! "Comparar esquemas" against real servers: the database is read as the
//! source (full-text index over two labels with its analyzer, vector index
//! with its dimensions and similarity, point index with its bounds, text,
//! range and relationship indexes, constraints), then made into a target
//! that differs, and the sync script from target to source must make it
//! read the same again. (Community editions have one database, so both
//! sides are the same database at two moments.)
//!
//! ```sh
//! DBINE_TEST_NEO4J_URL=neo4j:dbine-test-pass@localhost:17687 DBINE_TEST_MEMGRAPH_URL=localhost:27687 \
//!   cargo test -p dbine-driver-neo4j --test compare -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session, TableChange, TableSchema};

fn cfg(driver: &str, url: &str) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').map_or((None, url), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':').unwrap();
    let (user, pass) = auth.and_then(|a| a.split_once(':')).map_or((None, None), |(u, p)| (Some(u.to_string()), Some(p.to_string())));
    ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().unwrap(), username: user, password: pass, ..Default::default() }
}

async fn exec(s: &mut Box<dyn Session>, q: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out).await.map_err(|e| format!("{q}: {e}"))?;
    out.error.map_or(Ok(()), |e| Err(format!("{q}: {e}")))
}

const LABELS: &[&str] = &["CmpDoc", "CmpNote", "CMP_LINK"];

async fn read(s: &mut Box<dyn Session>) -> Vec<TableSchema> {
    let mut v: Vec<TableSchema> = s.database_schema().await.unwrap().into_iter().filter(|t| LABELS.contains(&t.name.as_str())).collect();
    for t in &mut v {
        t.indexes.sort_by(|a, b| (&a.name, &a.columns).cmp(&(&b.name, &b.columns)));
        t.columns.clear();
    }
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

async fn compare_and_sync(id: &str, env: &str, clean: &[&str], source: &[&str], target: &[&str]) {
    let Ok(url) = std::env::var(env) else { return };
    let d = dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    let mut s = d.connect(&cfg(id, &url), None).await.unwrap();
    for q in clean {
        let _ = exec(&mut s, q).await;
    }
    exec(&mut s, "CREATE (a:CmpDoc {title: 't', n: 1})-[:CMP_LINK {w: 1}]->(:CmpNote {title: 'n'})").await.unwrap();
    for q in source {
        exec(&mut s, q).await.unwrap();
    }
    let src = read(&mut s).await;
    println!("{id} source: {src:#?}");
    for q in clean.iter().skip(1) {
        let _ = exec(&mut s, q).await;
    }
    for q in target {
        exec(&mut s, q).await.unwrap();
    }
    let dst = read(&mut s).await;
    assert_ne!(src, dst);
    let changes: Vec<TableChange> = dst.iter().cloned().zip(src.iter().cloned()).map(|(old, new)| TableChange::Alter { old, new }).collect();
    let script = d.sync_script(&changes).unwrap();
    println!("{id}: {script:#?}");
    for q in &script.statements {
        exec(&mut s, q).await.unwrap();
    }
    let after = read(&mut s).await;
    assert_eq!(after, src);
    let changes: Vec<TableChange> = after.into_iter().zip(src).map(|(old, new)| TableChange::Alter { old, new }).collect();
    assert!(d.sync_script(&changes).unwrap().statements.is_empty());
    for q in clean {
        let _ = exec(&mut s, q).await;
    }
}

#[tokio::test]
#[ignore]
async fn neo4j_compare() {
    let clean = [
        "MATCH (n) WHERE n:CmpDoc OR n:CmpNote DETACH DELETE n",
        "DROP INDEX cmp_ft IF EXISTS",
        "DROP INDEX cmp_vec IF EXISTS",
        "DROP INDEX cmp_pt IF EXISTS",
        "DROP INDEX cmp_txt IF EXISTS",
        "DROP INDEX cmp_rel IF EXISTS",
        "DROP INDEX cmp_old IF EXISTS",
        "DROP CONSTRAINT cmp_u IF EXISTS",
    ];
    let source = [
        "CREATE FULLTEXT INDEX cmp_ft FOR (e:CmpDoc|CmpNote) ON EACH [e.title, e.body] OPTIONS {indexConfig: {`fulltext.analyzer`: 'spanish', `fulltext.eventually_consistent`: true}}",
        "CREATE VECTOR INDEX cmp_vec FOR (e:CmpDoc) ON (e.emb) OPTIONS {indexConfig: {`vector.dimensions`: 4, `vector.similarity_function`: 'euclidean'}}",
        "CREATE POINT INDEX cmp_pt FOR (e:CmpDoc) ON (e.loc) OPTIONS {indexConfig: {`spatial.cartesian.min`: [-100.0, -100.0], `spatial.cartesian.max`: [100.0, 100.0]}}",
        "CREATE TEXT INDEX cmp_txt FOR (e:CmpDoc) ON (e.code)",
        "CREATE INDEX cmp_rel FOR ()-[e:CMP_LINK]-() ON (e.w)",
        "CREATE CONSTRAINT cmp_u FOR (e:CmpDoc) REQUIRE e.code2 IS UNIQUE",
    ];
    let target = [
        "CREATE FULLTEXT INDEX cmp_ft FOR (e:CmpDoc) ON EACH [e.title]",
        "CREATE VECTOR INDEX cmp_vec FOR (e:CmpDoc) ON (e.emb) OPTIONS {indexConfig: {`vector.dimensions`: 4, `vector.similarity_function`: 'cosine'}}",
        "CREATE POINT INDEX cmp_pt FOR (e:CmpDoc) ON (e.loc)",
        "CREATE INDEX cmp_old FOR (e:CmpDoc) ON (e.x)",
    ];
    compare_and_sync("neo4j", "DBINE_TEST_NEO4J_URL", &clean, &source, &target).await;
}

#[tokio::test]
#[ignore]
async fn memgraph_compare() {
    let clean = [
        "MATCH (n) WHERE n:CmpDoc OR n:CmpNote DETACH DELETE n",
        "DROP INDEX ON :CmpDoc(title)",
        "DROP INDEX ON :CmpDoc(x)",
        "DROP EDGE INDEX ON :CMP_LINK(w)",
        "DROP POINT INDEX ON :CmpDoc(loc)",
        "DROP CONSTRAINT ON (n:CmpDoc) ASSERT n.code IS UNIQUE",
        "DROP CONSTRAINT ON (n:CmpDoc) ASSERT EXISTS (n.title)",
        "DROP CONSTRAINT ON (n:CmpDoc) ASSERT n.n IS TYPED INTEGER",
        "DROP VECTOR INDEX cmp_v",
    ];
    let source = [
        "CREATE INDEX ON :CmpDoc(title)",
        "CREATE EDGE INDEX ON :CMP_LINK(w)",
        "CREATE POINT INDEX ON :CmpDoc(loc)",
        "CREATE CONSTRAINT ON (n:CmpDoc) ASSERT n.code IS UNIQUE",
        "CREATE CONSTRAINT ON (n:CmpDoc) ASSERT EXISTS (n.title)",
        "CREATE CONSTRAINT ON (n:CmpDoc) ASSERT n.n IS TYPED INTEGER",
        "CREATE VECTOR INDEX cmp_v ON :CmpDoc(emb) WITH CONFIG {\"dimension\": 2, \"capacity\": 100, \"metric\": \"l2sq\"}",
    ];
    let target = ["CREATE INDEX ON :CmpDoc(x)", "CREATE VECTOR INDEX cmp_v ON :CmpDoc(emb) WITH CONFIG {\"dimension\": 2, \"capacity\": 100, \"metric\": \"cos\"}"];
    compare_and_sync("memgraph", "DBINE_TEST_MEMGRAPH_URL", &clean, &source, &target).await;
}
