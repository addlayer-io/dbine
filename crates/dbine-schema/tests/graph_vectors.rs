//! Vector indexes converted into Neo4j and Memgraph, and between them (one
//! dialect family, so `convert` hands an index over as it is and the
//! dialect's `finalize` fits it to the target): the target driver's own
//! DDL takes what the conversion gives, and an index the target can't make
//! (on a sparse vector, beyond Neo4j's limits, a metric the target lacks,
//! Memgraph without a dimension) is left out by the conversion, said,
//! instead of failing the label's whole script.
//!
//! The first half works offline, with tables spelled the way each driver's
//! `database_schema` reports them. The second half (`#[ignore]`) reads real
//! Neo4j, Memgraph and pgvector schemas, converts them, creates them on the
//! other engines with the target driver's DDL and reads the indexes back:
//!
//! ```sh
//! docker start dbine-test-pgvector dbine-test-neo4j dbine-test-memgraph
//! DBINE_TEST_PGVECTOR_URL=postgres://postgres:pw@127.0.0.1:27200/postgres \
//! DBINE_TEST_NEO4J_URL=neo4j:dbine-test-pass@localhost:17687 \
//! DBINE_TEST_MEMGRAPH_URL=localhost:27687 \
//!   cargo test -p dbine-schema --test graph_vectors -- --ignored --test-threads=1
//! ```
//!
//! pgvector is optional, and so is a Neo4j 2025 Enterprise server
//! (`DBINE_TEST_NEO4J_2025_URL`, for a VECTOR property's dimension).
//!
//! The live test drops what it creates first and at the end, also when it
//! fails partway.

mod common;

use dbine_driver::{ColumnDef, IndexDef, KeyDef, QueryOutcome, Session, TableSchema, VectorIndex};
use dbine_schema::{convert, Conversion, IssueCode, Options};

// ---- offline -----------------------------------------------------------------

fn col(name: &str, ty: &str) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable: true,
        ..Default::default()
    }
}

fn vix(algorithm: Option<&str>, metric: &str, params: &[(&str, &str)]) -> VectorIndex {
    VectorIndex {
        algorithm: algorithm.map(str::to_string),
        metric: Some(metric.into()),
        params: params
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

fn index(name: &str, column: &str, kind: &str, v: VectorIndex) -> IndexDef {
    IndexDef {
        name: name.into(),
        columns: vec![column.into()],
        kind: Some(kind.into()),
        vector: Some(v),
        ..Default::default()
    }
}

/// The DDL the target driver writes (panics if it refuses a label).
fn ddl(driver: &str, tables: &[TableSchema]) -> String {
    common::target_ddl(driver, tables).join("\n")
}

/// Whether the conversion left out the vector index `name`, saying `why`.
fn dropped(c: &Conversion, name: &str, why: &str) -> bool {
    c.tables
        .iter()
        .all(|t| t.indexes.iter().all(|i| i.name != name))
        && c.issues.iter().any(|i| {
            i.code == IssueCode::IndexDropped
                && i.object.as_deref() == Some(name)
                && i.message.contains(why)
        })
}

/// pgvector's table with a dense and a sparse vector, both with HNSW.
fn pg_docs() -> TableSchema {
    TableSchema {
        schema: Some("public".into()),
        name: "docs".into(),
        columns: vec![
            col("id", "integer"),
            col("emb", "vector(3)"),
            col("sp", "sparsevec(5)"),
        ],
        primary_key: Some(KeyDef {
            name: Some("docs_pkey".into()),
            columns: vec!["id".into()],
        }),
        indexes: vec![
            index(
                "docs_emb",
                "emb",
                "hnsw",
                vix(
                    Some("hnsw"),
                    "cosine",
                    &[("m", "16"), ("ef_construction", "64")],
                ),
            ),
            index("docs_sp", "sp", "hnsw", vix(Some("hnsw"), "ip", &[])),
        ],
        ..Default::default()
    }
}

/// From another family: the dense vector's index takes the column's
/// dimension; the sparse one (text in a graph) is left out; Oracle's
/// 8192 dimensions are beyond Neo4j's 4096, not Memgraph's.
#[test]
fn into_neo4j_and_memgraph_the_script_runs() {
    for (to, dims) in [
        ("neo4j", "`vector.dimensions`: 3"),
        ("memgraph", "\"dimension\": 3"),
    ] {
        let c = convert(&[pg_docs()], "postgres", to, &Options::default()).unwrap();
        let script = ddl(to, &c.tables);
        assert!(
            script.contains("docs_emb") && script.contains(dims),
            "{to}: {script}"
        );
        assert!(dropped(&c, "docs_sp", "disperso"), "{to}: {:?}", c.issues);
        assert!(!script.contains("docs_sp"), "{to}: {script}");
    }
    let wide = TableSchema {
        name: "WIDE".into(),
        columns: vec![col("ID", "NUMBER(10)"), col("EMB", "VECTOR(8192, FLOAT32)")],
        indexes: vec![index(
            "WIDE_EMB",
            "EMB",
            "VECTOR",
            vix(Some("hnsw"), "cosine", &[]),
        )],
        ..Default::default()
    };
    let neo = convert(
        std::slice::from_ref(&wide),
        "oracle",
        "neo4j",
        &Options::default(),
    )
    .unwrap();
    assert!(dropped(&neo, "WIDE_EMB", "4096"), "{:?}", neo.issues);
    let script = ddl("neo4j", &neo.tables);
    assert!(!script.contains("VECTOR INDEX"), "{script}");
    let mg = convert(&[wide], "oracle", "memgraph", &Options::default()).unwrap();
    assert!(ddl("memgraph", &mg.tables).contains("\"dimension\": 8192"));
}

/// A Neo4j label as `database_schema` reads it: a Neo4j 2025 VECTOR
/// property searched by an index that doesn't state the dimension (5.23+),
/// and a list searched by another.
fn neo_doc() -> TableSchema {
    TableSchema {
        kind: "label".into(),
        name: "Doc".into(),
        columns: vec![col("emb", "VECTOR<FLOAT32>(3)"), col("old", "LIST<FLOAT>")],
        indexes: vec![
            index(
                "doc_emb",
                "emb",
                "VECTOR",
                vix(Some("hnsw"), "cosine", &[("m", "24")]),
            ),
            index("doc_old", "old", "VECTOR", vix(Some("hnsw"), "l2", &[])),
        ],
        ..Default::default()
    }
}

/// A Memgraph label: a Memgraph-only metric, and cosine with Memgraph's
/// own settings.
fn mg_doc() -> TableSchema {
    TableSchema {
        kind: "label".into(),
        name: "Doc".into(),
        columns: vec![col("emb", "LIST<FLOAT>"), col("emb2", "LIST<FLOAT>")],
        indexes: vec![
            index(
                "doc_pearson",
                "emb",
                "VECTOR",
                vix(None, "pearson", &[("dimensions", "3"), ("capacity", "100")]),
            ),
            index(
                "doc_cos",
                "emb2",
                "VECTOR",
                vix(
                    None,
                    "cosine",
                    &[
                        ("dimensions", "3"),
                        ("capacity", "100"),
                        ("resize_coefficient", "2"),
                    ],
                ),
            ),
        ],
        ..Default::default()
    }
}

/// Between the two: Memgraph gets the VECTOR property's dimension (and
/// leaves out the index nothing gives one to); Neo4j leaves out the
/// Memgraph-only metric and drops Memgraph's own settings. Within one
/// engine everything stays.
#[test]
fn between_neo4j_and_memgraph_the_script_runs() {
    let o = Options::default();
    let mg = convert(&[neo_doc()], "neo4j", "memgraph", &o).unwrap();
    let script = ddl("memgraph", &mg.tables);
    assert!(
        script.contains("CREATE VECTOR INDEX doc_emb") && script.contains("\"dimension\": 3"),
        "{script}"
    );
    assert!(dropped(&mg, "doc_old", "dimensión"), "{:?}", mg.issues);
    let same = convert(&[neo_doc()], "neo4j", "neo4j", &o).unwrap();
    assert_eq!(same.tables[0].indexes, neo_doc().indexes);

    let neo = convert(&[mg_doc()], "memgraph", "neo4j", &o).unwrap();
    assert!(dropped(&neo, "doc_pearson", "pearson"), "{:?}", neo.issues);
    let script = ddl("neo4j", &neo.tables);
    assert!(
        script.contains("CREATE VECTOR INDEX doc_cos")
            && script.contains("`vector.dimensions`: 3")
            && !script.contains("capacity"),
        "{script}"
    );
    let same = convert(&[mg_doc()], "memgraph", "memgraph", &o).unwrap();
    assert_eq!(same.tables[0].indexes, mg_doc().indexes);
    let script = ddl("memgraph", &same.tables);
    assert!(
        script.contains("\"metric\": 'pearson'") && script.contains("\"resize_coefficient\": 2"),
        "{script}"
    );
}

// ---- against real servers ---------------------------------------------------

type S = Box<dyn Session>;

const NEO_LABEL: &str = "DbineGvNeo";
const NEO25_LABEL: &str = "DbineGvNeo25";
const MG_LABEL: &str = "DbineGvMg";
const PG_TABLE: &str = "zz_gv_docs";
const LABELS: &[&str] = &[NEO_LABEL, NEO25_LABEL, MG_LABEL, PG_TABLE];
const INDEXES: &[&str] = &[
    "dbine_gv_neo_emb",
    "dbine_gv_neo_old",
    "dbine_gv_neo25_emb",
    "dbine_gv_mg_pearson",
    "dbine_gv_mg_cos",
    "zz_gv_docs_emb",
    "zz_gv_docs_sp",
];

async fn run(s: &mut S, q: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    if let Some(e) = &out.error {
        panic!("{q}: {e}");
    }
    out
}

/// The target driver's script for the converted labels (a label's alone
/// is only a comment).
async fn create(s: &mut S, driver: &str, c: &Conversion) {
    for script in common::target_ddl(driver, &c.tables) {
        if script
            .lines()
            .any(|l| !l.trim().is_empty() && !l.trim().starts_with("//"))
        {
            run(s, &script).await;
        }
    }
}

/// The vector index `name` as the driver lists it on `label`.
async fn listed(s: &mut S, label: &str, name: &str) -> Option<VectorIndex> {
    let t = common::read(s, &[label]).await.into_iter().next()?;
    t.indexes
        .into_iter()
        .find(|i| i.name == name)
        .and_then(|i| i.vector)
}

/// Drops the test's indexes, constraints (pgvector's primary key becomes a
/// uniqueness constraint) and nodes on a Cypher server, whatever is there.
async fn cypher_cleanup(driver: &str, env: &str) {
    let Some(mut s) = common::session(driver, env).await else {
        return;
    };
    let memgraph = driver == "memgraph";
    let drops: Vec<String> = INDEXES
        .iter()
        .map(|i| match memgraph {
            true => format!("DROP VECTOR INDEX {i}"),
            false => format!("DROP INDEX {i} IF EXISTS"),
        })
        .chain([match memgraph {
            true => format!("DROP CONSTRAINT ON (n:{PG_TABLE}) ASSERT n.id IS UNIQUE"),
            false => "DROP CONSTRAINT zz_gv_docs_pkey IF EXISTS".to_string(),
        }])
        .chain(
            LABELS
                .iter()
                .map(|l| format!("MATCH (n:{l}) DETACH DELETE n")),
        )
        .collect();
    common::exec_quiet(
        &mut s,
        &drops.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
}

async fn cleanup() {
    cypher_cleanup("neo4j", "DBINE_TEST_NEO4J_URL").await;
    cypher_cleanup("neo4j", "DBINE_TEST_NEO4J_2025_URL").await;
    cypher_cleanup("memgraph", "DBINE_TEST_MEMGRAPH_URL").await;
    if let Some(mut pg) = common::session("postgres", "DBINE_TEST_PGVECTOR_URL").await {
        common::exec_quiet(&mut pg, &[&format!("DROP TABLE IF EXISTS {PG_TABLE}")]).await;
    }
}

async fn scenario() {
    let mut neo = common::session("neo4j", "DBINE_TEST_NEO4J_URL")
        .await
        .unwrap();
    let mut mg = common::session("memgraph", "DBINE_TEST_MEMGRAPH_URL")
        .await
        .unwrap();
    let o = Options::default();

    // 1. Neo4j → Memgraph: the index stating its dimension is made; the one
    // without (5.23+, on a list) is left out, said, and the script runs.
    run(
        &mut neo,
        &format!("CREATE (:{NEO_LABEL} {{emb: [1.0, 0.0, 0.0], old: [0.5, 0.5]}})"),
    )
    .await;
    run(
        &mut neo,
        &format!(
            "CREATE VECTOR INDEX dbine_gv_neo_emb FOR (n:{NEO_LABEL}) ON (n.emb) \
             OPTIONS {{indexConfig: {{`vector.dimensions`: 3, `vector.similarity_function`: 'COSINE', `vector.hnsw.m`: 24}}}}"
        ),
    )
    .await;
    run(
        &mut neo,
        &format!(
            "CREATE VECTOR INDEX dbine_gv_neo_old FOR (n:{NEO_LABEL}) ON (n.old) \
             OPTIONS {{indexConfig: {{`vector.similarity_function`: 'EUCLIDEAN'}}}}"
        ),
    )
    .await;
    run(&mut neo, "CALL db.awaitIndexes(60)").await;
    let src = common::read(&mut neo, &[NEO_LABEL]).await;
    let c = convert(&src, "neo4j", "memgraph", &o).unwrap();
    assert!(
        dropped(&c, "dbine_gv_neo_old", "dimensión"),
        "{:?}",
        c.issues
    );
    create(&mut mg, "memgraph", &c).await;
    run(
        &mut mg,
        &format!("CREATE (:{NEO_LABEL} {{emb: [1.0, 0.0, 0.0]}})"),
    )
    .await;
    let v = listed(&mut mg, NEO_LABEL, "dbine_gv_neo_emb")
        .await
        .expect("Memgraph lists the converted index");
    assert_eq!(v.metric.as_deref(), Some("cosine"), "{v:?}");
    assert_eq!(v.params.get("dimensions").map(String::as_str), Some("3"));

    // 2. Memgraph → Neo4j: the Memgraph-only metric is left out, said; the
    // cosine index is made with the dimension.
    run(
        &mut mg,
        &format!("CREATE (:{MG_LABEL} {{emb: [1.0, 0.0, 0.0], emb2: [0.0, 1.0, 0.0]}})"),
    )
    .await;
    run(
        &mut mg,
        &format!(
            "CREATE VECTOR INDEX dbine_gv_mg_pearson ON :{MG_LABEL}(emb) \
             WITH CONFIG {{\"dimension\": 3, \"capacity\": 100, \"metric\": \"pearson\"}}"
        ),
    )
    .await;
    run(
        &mut mg,
        &format!(
            "CREATE VECTOR INDEX dbine_gv_mg_cos ON :{MG_LABEL}(emb2) \
             WITH CONFIG {{\"dimension\": 3, \"capacity\": 100, \"metric\": \"cos\", \"resize_coefficient\": 2}}"
        ),
    )
    .await;
    let src = common::read(&mut mg, &[MG_LABEL]).await;
    let c = convert(&src, "memgraph", "neo4j", &o).unwrap();
    assert!(
        dropped(&c, "dbine_gv_mg_pearson", "pearson"),
        "{:?}",
        c.issues
    );
    create(&mut neo, "neo4j", &c).await;
    run(
        &mut neo,
        &format!("CREATE (:{MG_LABEL} {{emb2: [0.0, 1.0, 0.0]}})"),
    )
    .await;
    run(&mut neo, "CALL db.awaitIndexes(60)").await;
    let v = listed(&mut neo, MG_LABEL, "dbine_gv_mg_cos")
        .await
        .expect("Neo4j lists the converted index");
    assert_eq!(v.metric.as_deref(), Some("cosine"), "{v:?}");
    assert_eq!(v.params.get("dimensions").map(String::as_str), Some("3"));

    // 3. pgvector → both: the dense vector's index with its column's
    // dimension; the sparse one left out, said (it'd index text).
    if let Some(mut pg) = common::session("postgres", "DBINE_TEST_PGVECTOR_URL").await {
        run(&mut pg, "CREATE EXTENSION IF NOT EXISTS vector").await;
        run(
            &mut pg,
            &format!(
                "CREATE TABLE {PG_TABLE} (id int PRIMARY KEY, emb vector(3), sp sparsevec(5)); \
                 CREATE INDEX zz_gv_docs_emb ON {PG_TABLE} USING hnsw (emb vector_cosine_ops); \
                 CREATE INDEX zz_gv_docs_sp ON {PG_TABLE} USING hnsw (sp sparsevec_ip_ops)"
            ),
        )
        .await;
        let src = common::read(&mut pg, &[PG_TABLE]).await;
        assert_eq!(src.len(), 1, "pgvector: {PG_TABLE} not read");
        for (driver, s) in [("neo4j", &mut neo), ("memgraph", &mut mg)] {
            let c = convert(&src, "postgres", driver, &o).unwrap();
            assert!(
                dropped(&c, "zz_gv_docs_sp", "disperso"),
                "{driver}: {:?}",
                c.issues
            );
            create(s, driver, &c).await;
            run(
                s,
                &format!("CREATE (:{PG_TABLE} {{id: 1, emb: [1.0, 0.0, 0.0]}})"),
            )
            .await;
            if driver == "neo4j" {
                run(s, "CALL db.awaitIndexes(60)").await;
            }
            let v = listed(s, PG_TABLE, "zz_gv_docs_emb")
                .await
                .unwrap_or_else(|| panic!("{driver} lists the converted index"));
            assert_eq!(v.metric.as_deref(), Some("cosine"), "{driver}: {v:?}");
            assert_eq!(
                v.params.get("dimensions").map(String::as_str),
                Some("3"),
                "{driver}"
            );
        }
    }

    // 4. Neo4j 2025 (Enterprise) → Memgraph: a VECTOR<FLOAT32>(3)
    // property's dimension goes into the index that doesn't state one.
    if let Some(mut neo25) = common::session("neo4j", "DBINE_TEST_NEO4J_2025_URL").await {
        run(
            &mut neo25,
            &format!(
                "CYPHER 25 CREATE (:{NEO25_LABEL} {{emb: vector([1.0, 0.0, 0.0], 3, FLOAT32)}})"
            ),
        )
        .await;
        run(
            &mut neo25,
            &format!(
                "CREATE VECTOR INDEX dbine_gv_neo25_emb FOR (n:{NEO25_LABEL}) ON (n.emb) \
                 OPTIONS {{indexConfig: {{`vector.similarity_function`: 'COSINE'}}}}"
            ),
        )
        .await;
        run(&mut neo25, "CALL db.awaitIndexes(60)").await;
        let src = common::read(&mut neo25, &[NEO25_LABEL]).await;
        let t = src.first().expect("Neo4j 2025 reads the label");
        assert_eq!(common::col(t, "emb").data_type, "VECTOR<FLOAT32>(3)");
        let ix = t
            .indexes
            .iter()
            .find(|i| i.name == "dbine_gv_neo25_emb")
            .expect("the vector index");
        assert!(
            !ix.vector
                .as_ref()
                .unwrap()
                .params
                .contains_key("dimensions"),
            "{ix:?}"
        );
        let c = convert(&src, "neo4j", "memgraph", &o).unwrap();
        create(&mut mg, "memgraph", &c).await;
        run(
            &mut mg,
            &format!("CREATE (:{NEO25_LABEL} {{emb: [1.0, 0.0, 0.0]}})"),
        )
        .await;
        let v = listed(&mut mg, NEO25_LABEL, "dbine_gv_neo25_emb")
            .await
            .expect("Memgraph lists the converted index");
        assert_eq!(v.params.get("dimensions").map(String::as_str), Some("3"));
    }
}

#[tokio::test]
#[ignore]
async fn live_vector_indexes_into_and_between_cypher_engines() {
    if std::env::var("DBINE_TEST_NEO4J_URL").is_err()
        || std::env::var("DBINE_TEST_MEMGRAPH_URL").is_err()
    {
        eprintln!("DBINE_TEST_NEO4J_URL and DBINE_TEST_MEMGRAPH_URL not set; skipping");
        return;
    }
    cleanup().await;
    let outcome = tokio::spawn(scenario()).await;
    cleanup().await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}
