//! "Clonar tabla" on PostgreSQL's family, against real servers: identity
//! `ALWAYS` with its own start, step and current value, `serial` columns
//! and a shared sequence, collations, generated / computed columns,
//! names another object of the schema already has, partitioned and
//! inherited tables, TimescaleDB hypertables and names past 63 bytes.
//!
//! Ignored by default. By default the `dbine-test-*` containers:
//!
//! ```sh
//! cargo test -p dbine-transfer --test clone_table_pg -- --ignored --nocapture --test-threads 1
//! ```
//!
//! - `DBINE_TEST_PG_URL` (`postgres://user:pass@host:port/db`, default
//!   `dbine-test-postgres`);
//! - `DBINE_TEST_TIMESCALE_URL` (default `dbine-test-timescale`);
//! - `DBINE_TEST_COCKROACH_URL` (default `dbine-test-cockroach`, insecure).

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session};
use dbine_transfer::clone_table::{clone_table, CloneControl, CloneOptions, CloneReport, CloneRequest, ConfigEndpoints};
use dbine_transfer::Endpoints;
use std::sync::Arc;

const SCHEMA: &str = "clonefix";

fn config(driver: &str, var: &str, default: &str) -> (ConnectionConfig, String) {
    let url = std::env::var(var).unwrap_or_else(|_| default.into());
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap_or((auth, ""));
    let (hostport, db) = hostport.split_once('/').unwrap_or((hostport, "postgres"));
    let (host, port) = hostport.rsplit_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().unwrap(),
        database: db.into(),
        username: Some(user.into()),
        password: (!pass.is_empty()).then(|| pass.to_string()),
        ..Default::default()
    };
    (cfg, db.to_string())
}

fn endpoints(driver: &str, var: &str, default: &str) -> Arc<ConfigEndpoints> {
    let (config, db) = config(driver, var, default);
    let d = dbine_drivers::find(driver).unwrap_or_else(|| panic!("no driver {driver}")).clone();
    Arc::new(ConfigEndpoints { driver: d, config, database: Some(db) })
}

async fn rows(s: &mut dyn Session, sql: &str) -> Vec<Vec<String>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100_000, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{sql}"));
    if let Some(e) = out.error {
        panic!("{e}\n{sql}");
    }
    out.results
        .iter()
        .flat_map(|r| r.rows.iter())
        .map(|r| r.iter().map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).collect())
        .collect()
}

async fn one(s: &mut dyn Session, sql: &str) -> String {
    rows(s, sql).await.remove(0).remove(0)
}

async fn clone(e: &Arc<ConfigEndpoints>, table: &str, name: &str, with_data: bool) -> Result<CloneReport, String> {
    clone_with(e, table, name, CloneOptions { with_data, with_indexes: true }).await
}

async fn clone_with(e: &Arc<ConfigEndpoints>, table: &str, name: &str, options: CloneOptions) -> Result<CloneReport, String> {
    let req = CloneRequest {
        source: ObjectRef { kind: "table".into(), schema: Some(SCHEMA.into()), name: table.into() },
        new_name: name.into(),
        options,
    };
    let r = clone_table(e.clone() as Arc<dyn Endpoints>, req, &CloneControl::default(), |_| {}).await.map_err(|e| e.to_string());
    println!("{table} → {name}: {r:?}");
    r
}

/// `column → (identity_generation, is_generated, collation_name, default)`.
async fn columns(s: &mut dyn Session, table: &str) -> Vec<Vec<String>> {
    rows(
        s,
        &format!(
            "SELECT column_name, COALESCE(identity_generation, ''), COALESCE(is_generated, ''), COALESCE(collation_name, ''),
                    COALESCE(column_default, '')
             FROM information_schema.columns WHERE table_schema = '{SCHEMA}' AND table_name = '{table}' ORDER BY ordinal_position"
        ),
    )
    .await
}

async fn no_table(s: &mut dyn Session, name: &str) {
    let n = one(s, &format!("SELECT count(*) FROM information_schema.tables WHERE table_schema = '{SCHEMA}' AND table_name = '{name}'")).await;
    assert_eq!(n, "0", "{name} must not exist");
}

/// PostgreSQL and TimescaleDB (same catalog).
async fn postgres_family(e: Arc<ConfigEndpoints>, timescale: bool) {
    let mut w = e.open_target().await.unwrap();
    let s = &mut *w;
    rows(
        s,
        &format!(
            "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA};
             CREATE SEQUENCE {SCHEMA}.compartida START 1000;
             CREATE TABLE {SCHEMA}.ser (
                 id int GENERATED ALWAYS AS IDENTITY (START WITH 100 INCREMENT BY 5) PRIMARY KEY,
                 n serial,
                 k bigint DEFAULT nextval('{SCHEMA}.compartida'),
                 s text COLLATE \"C\" CHECK (length(s) < 50),
                 g int GENERATED ALWAYS AS (id * 2) STORED
             );
             CREATE INDEX ser_s_idx ON {SCHEMA}.ser (s);
             INSERT INTO {SCHEMA}.ser (s) SELECT 'v' || g FROM generate_series(1, 40) g;
             DELETE FROM {SCHEMA}.ser WHERE id > 250;
             CREATE TABLE {SCHEMA}.otra (x int);
             CREATE INDEX ser_c2_pkey ON {SCHEMA}.otra (x);
             CREATE TABLE {SCHEMA}.part (id int, d date) PARTITION BY RANGE (d);
             CREATE TABLE {SCHEMA}.part_a PARTITION OF {SCHEMA}.part FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
             CREATE TABLE {SCHEMA}.part_b PARTITION OF {SCHEMA}.part FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
             INSERT INTO {SCHEMA}.part VALUES (1, '2025-05-01'), (2, '2026-05-01');
             CREATE TABLE {SCHEMA}.padre (id int);
             CREATE TABLE {SCHEMA}.hija (extra int) INHERITS ({SCHEMA}.padre);"
        ),
    )
    .await;

    // Everything kept; the index name another table has is avoided.
    let r = clone(&e, "ser", "ser_c2", true).await.unwrap();
    assert_eq!(r.rows, 31);
    assert!(r.notes.iter().any(|n| n.contains("ser_c2_pkey")), "{:?}", r.notes);
    assert!(r.notes.iter().any(|n| n.contains("compartida")), "{:?}", r.notes);
    let (a, b) = (columns(s, "ser").await, columns(s, "ser_c2").await);
    println!("{a:?}\n{b:?}");
    assert_eq!(b[0][1], "ALWAYS", "identity ALWAYS");
    assert_eq!(b[3][3], "C", "collation");
    assert_eq!(b[4][2], "ALWAYS", "generated");
    assert_eq!(b[2][4], a[2][4], "shared sequence");
    assert!(b[1][4].contains("ser_c2_n_seq"), "serial with its own sequence: {}", b[1][4]);
    let data = |t: &str| format!("SELECT id, n, k, s, g FROM {SCHEMA}.{t} ORDER BY id");
    assert_eq!(rows(s, &data("ser")).await, rows(s, &data("ser_c2")).await);
    // The identity continues where the original's does (300, after the
    // deletes), stepping by 5; the serial after 40.
    let next = rows(s, &format!("INSERT INTO {SCHEMA}.ser_c2 (s) VALUES ('nuevo') RETURNING id, n")).await;
    assert_eq!(next, vec![vec!["300".to_string(), "41".to_string()]]);
    let pk = one(s, &format!("SELECT conname FROM pg_constraint WHERE conrelid = '{SCHEMA}.ser_c2'::regclass AND contype = 'p'")).await;
    assert_ne!(pk, "ser_c2_pkey");

    // Without rows: the counters start over, with the original's options.
    clone(&e, "ser", "ser_c3", false).await.unwrap();
    let next = rows(s, &format!("INSERT INTO {SCHEMA}.ser_c3 (s) VALUES ('x') RETURNING id, n")).await;
    assert_eq!(next, vec![vec!["100".to_string(), "1".to_string()]]);

    // A name an index already has: refused in Spanish, nothing created.
    let err = clone(&e, "ser", "ser_s_idx", true).await.unwrap_err();
    assert!(err.contains("ya existe") && err.contains("ser_s_idx"), "{err}");

    // Partitioned, a partition, an inheritance parent: refused.
    let err = clone(&e, "part", "part_c", true).await.unwrap_err();
    assert!(err.contains("particionada"), "{err}");
    let err = clone(&e, "part_a", "part_a_c", true).await.unwrap_err();
    assert!(err.contains("partición"), "{err}");
    let err = clone(&e, "padre", "padre_c", true).await.unwrap_err();
    assert!(err.contains("heredan"), "{err}");
    for n in ["part_c", "part_a_c", "padre_c"] {
        no_table(s, n).await;
    }

    // 64 bytes in 32 characters: the message says bytes.
    let err = clone(&e, "ser", &"ñ".repeat(32), true).await.unwrap_err();
    assert!(err.contains("63 bytes") && err.contains("ñ"), "{err}");

    // CHECKs NOT VALID (rows that break it: the original's own) and NO
    // INHERIT, a foreign key NOT VALID, UNLOGGED: kept as they are.
    rows(
        s,
        &format!(
            "CREATE UNLOGGED TABLE {SCHEMA}.nv (id int PRIMARY KEY, x int, p int, r int4range);
             INSERT INTO {SCHEMA}.nv VALUES (1, -5, 99, '[1,5)'), (2, 3, 1, '[5,9)');
             ALTER TABLE {SCHEMA}.nv ADD CONSTRAINT ck_x CHECK (x > 0) NOT VALID;
             ALTER TABLE {SCHEMA}.nv ADD CONSTRAINT ck_ok CHECK (x < 100) NOT VALID;
             ALTER TABLE {SCHEMA}.nv ADD CONSTRAINT ck_ni CHECK (id > 0) NO INHERIT;
             ALTER TABLE {SCHEMA}.nv ADD CONSTRAINT fk_p FOREIGN KEY (p) REFERENCES {SCHEMA}.nv (id) NOT VALID;
             ALTER TABLE {SCHEMA}.nv ADD CONSTRAINT ex_r EXCLUDE USING gist (r WITH &&);"
        ),
    )
    .await;
    let defs = |t: &str| {
        format!(
            "SELECT contype::text, pg_get_constraintdef(oid), convalidated::text FROM pg_constraint
             WHERE conrelid = '{SCHEMA}.{t}'::regclass AND contype IN ('c', 'f', 'x') ORDER BY 1, 2"
        )
    };
    let persistence = |t: &str| format!("SELECT relpersistence::text FROM pg_class WHERE oid = '{SCHEMA}.{t}'::regclass");
    clone(&e, "nv", "nv_c2", true).await.unwrap();
    let (a, b) = (rows(s, &defs("nv")).await, rows(s, &defs("nv_c2")).await);
    assert_eq!(a.len(), 5);
    for (x, y) in a.iter().zip(&b) {
        assert_eq!((&x[0], &x[2]), (&y[0], &y[2]), "{a:?}\n{b:?}");
        assert_eq!(x[1].replace("nv(", "nv_c2("), y[1], "{a:?}\n{b:?}");
    }
    assert_eq!(one(s, &persistence("nv_c2")).await, "u");
    let data = |t: &str| format!("SELECT id, x, p, r::text FROM {SCHEMA}.{t} ORDER BY id");
    assert_eq!(rows(s, &data("nv")).await, rows(s, &data("nv_c2")).await);
    // Without indexes: the EXCLUDE goes too, and the notes say so.
    let r = clone_with(&e, "nv", "nv_c3", CloneOptions { with_data: false, with_indexes: false }).await.unwrap();
    assert!(r.notes.iter().any(|n| n.contains("EXCLUDE") && n.contains("ex_r")), "{:?}", r.notes);
    let ix = one(s, &format!("SELECT count(*) FROM pg_index WHERE indrelid = '{SCHEMA}.nv_c3'::regclass")).await;
    assert_eq!(ix, "1");

    // Comments on the table, a column, an index and constraints: all on
    // the clone's renamed ones; without indexes, a note for the index's.
    rows(
        s,
        &format!(
            "CREATE TABLE {SCHEMA}.adv3 (id int CONSTRAINT pk_adv3 PRIMARY KEY, n int, m int, CONSTRAINT ck_multi CHECK (n < m));
             CREATE INDEX ix_inc ON {SCHEMA}.adv3 (n) INCLUDE (m);
             INSERT INTO {SCHEMA}.adv3 VALUES (1, 1, 2);
             COMMENT ON TABLE {SCHEMA}.adv3 IS 'tabla';
             COMMENT ON COLUMN {SCHEMA}.adv3.n IS 'col n';
             COMMENT ON INDEX {SCHEMA}.ix_inc IS 'comentario idx';
             COMMENT ON CONSTRAINT ck_multi ON {SCHEMA}.adv3 IS 'ck com';
             COMMENT ON CONSTRAINT pk_adv3 ON {SCHEMA}.adv3 IS 'la pk''s';"
        ),
    )
    .await;
    let r = clone(&e, "adv3", "adv3_k", true).await.unwrap();
    assert!(r.notes.is_empty(), "{:?}", r.notes);
    let com = |t: &str| {
        format!(
            "SELECT 'i ' || ic.relname, obj_description(ic.oid, 'pg_class') FROM pg_index x JOIN pg_class ic ON ic.oid = x.indexrelid
             WHERE x.indrelid = '{SCHEMA}.{t}'::regclass AND obj_description(ic.oid, 'pg_class') IS NOT NULL
             UNION ALL SELECT 'c ' || conname, obj_description(oid, 'pg_constraint') FROM pg_constraint
             WHERE conrelid = '{SCHEMA}.{t}'::regclass AND obj_description(oid, 'pg_constraint') IS NOT NULL
             UNION ALL SELECT 't', obj_description('{SCHEMA}.{t}'::regclass, 'pg_class')
             UNION ALL SELECT 'n', col_description('{SCHEMA}.{t}'::regclass, 2) ORDER BY 1"
        )
    };
    let renamed: Vec<Vec<String>> = rows(s, &com("adv3"))
        .await
        .into_iter()
        .map(|r| r.into_iter().map(|v| v.replacen("i ix_", "i adv3_k_ix_", 1).replacen("c ck_", "c adv3_k_ck_", 1).replacen("c pk_adv3", "c pk_adv3_k", 1)).collect())
        .collect();
    let mut got = rows(s, &com("adv3_k")).await;
    got.sort();
    assert_eq!(got.len(), 5, "{got:?}");
    let mut renamed = renamed;
    renamed.sort();
    assert_eq!(renamed, got);
    let r = clone_with(&e, "adv3", "adv3_n", CloneOptions { with_data: true, with_indexes: false }).await.unwrap();
    assert!(r.notes.iter().any(|n| n.contains("comentarios") && n.contains("ix_inc")), "{:?}", r.notes);
    let ck = one(s, &format!("SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint WHERE conrelid = '{SCHEMA}.adv3_n'::regclass AND contype = 'c'")).await;
    assert_eq!(ck, "ck com");

    // Row-level security (its flags and policies), the table's storage
    // options and the columns' STORAGE / STATISTICS / COMPRESSION: the
    // clone's are the original's (they used to be dropped silently).
    rows(
        s,
        &format!(
            "CREATE TABLE {SCHEMA}.sec (id int PRIMARY KEY, owner text, b bytea, n int, t text COMPRESSION pglz)
                 WITH (fillfactor=60, autovacuum_enabled=false, toast.autovacuum_enabled=false);
             ALTER TABLE {SCHEMA}.sec ALTER COLUMN n SET STATISTICS 500, ALTER COLUMN b SET STORAGE EXTERNAL;
             INSERT INTO {SCHEMA}.sec VALUES (-1, 'a', NULL, 1, 'x'), (1, 'b', NULL, 2, 'y');
             ALTER TABLE {SCHEMA}.sec ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {SCHEMA}.sec FORCE ROW LEVEL SECURITY;
             CREATE POLICY p1 ON {SCHEMA}.sec USING (id > 0);
             CREATE POLICY \"p 2\" ON {SCHEMA}.sec AS RESTRICTIVE FOR UPDATE USING (owner = current_user) WITH CHECK (id < 100);"
        ),
    )
    .await;
    let r = clone(&e, "sec", "sec_c", true).await.unwrap();
    assert_eq!(r.rows, 2);
    assert!(r.notes.is_empty(), "{:?}", r.notes);
    let table = |t: &str| {
        format!(
            "SELECT relrowsecurity::text, relforcerowsecurity::text, array_to_string(ARRAY(SELECT unnest(reloptions) ORDER BY 1), ','),
                    (SELECT array_to_string(reloptions, ',') FROM pg_class x WHERE x.oid = c.reltoastrelid)
             FROM pg_class c WHERE oid = '{SCHEMA}.{t}'::regclass"
        )
    };
    let sec = rows(s, &table("sec")).await;
    assert_eq!(sec, vec![vec!["true", "true", "autovacuum_enabled=false,fillfactor=60", "autovacuum_enabled=false"]]);
    assert_eq!(sec, rows(s, &table("sec_c")).await);
    let policies = |t: &str| {
        format!("SELECT policyname, permissive, cmd, roles::text, COALESCE(qual, ''), COALESCE(with_check, '') FROM pg_policies WHERE schemaname = '{SCHEMA}' AND tablename = '{t}' ORDER BY 1")
    };
    assert_eq!(rows(s, &policies("sec")).await.len(), 2);
    assert_eq!(rows(s, &policies("sec")).await, rows(s, &policies("sec_c")).await);
    let attrs = |t: &str| {
        format!("SELECT attname, attstorage::text, attstattarget::text, attcompression::text FROM pg_attribute WHERE attrelid = '{SCHEMA}.{t}'::regclass AND attnum > 0 ORDER BY attnum")
    };
    assert_eq!(rows(s, &attrs("sec")).await, rows(s, &attrs("sec_c")).await);
    assert_eq!(rows(s, &format!("SELECT count(*) FROM {SCHEMA}.sec_c")).await, vec![vec!["2".to_string()]]);

    // REPLICA IDENTITY, CLUSTER ON, extended statistics and a policy that
    // reads the table itself: the clone's are the original's, on its
    // renamed index and statistics, the policy reading the clone (all of
    // it was dropped silently, and the policy kept reading the original).
    rows(
        s,
        &format!(
            "CREATE TABLE {SCHEMA}.b1 (id int PRIMARY KEY, a int, bb int, p int);
             CREATE INDEX b1_ix ON {SCHEMA}.b1 (a);
             INSERT INTO {SCHEMA}.b1 VALUES (1, 1, 1, 1), (2, 2, 1, -1);
             ALTER TABLE {SCHEMA}.b1 REPLICA IDENTITY FULL;
             ALTER TABLE {SCHEMA}.b1 CLUSTER ON b1_ix;
             CREATE STATISTICS {SCHEMA}.b1_st (dependencies, ndistinct) ON a, bb FROM {SCHEMA}.b1;
             CREATE STATISTICS {SCHEMA}.b1_e ON (a + bb), p FROM {SCHEMA}.b1;
             ALTER STATISTICS {SCHEMA}.b1_e SET STATISTICS 50;
             COMMENT ON STATISTICS {SCHEMA}.b1_st IS 'st com';
             ALTER TABLE {SCHEMA}.b1 ENABLE ROW LEVEL SECURITY;
             CREATE POLICY p_self ON {SCHEMA}.b1 USING (id IN (SELECT id FROM {SCHEMA}.b1 WHERE p > 0));
             CREATE POLICY p_self2 ON {SCHEMA}.b1 USING (EXISTS (SELECT 1 FROM {SCHEMA}.b1 x WHERE x.id = b1.a));
             CREATE TABLE {SCHEMA}.b2 (id int PRIMARY KEY, u int NOT NULL, CONSTRAINT b2_u UNIQUE (u));
             ALTER TABLE {SCHEMA}.b2 REPLICA IDENTITY USING INDEX b2_u;
             ALTER TABLE {SCHEMA}.b2 CLUSTER ON b2_pkey;"
        ),
    )
    .await;
    let r = clone(&e, "b1", "b1_c", true).await.unwrap();
    assert_eq!(r.rows, 2);
    assert!(r.notes.iter().all(|n| !n.contains("CLUSTER") && !n.contains("réplica")), "{:?}", r.notes);
    let rel = |t: &str| {
        format!(
            "SELECT c.relreplident::text, COALESCE((SELECT ic.relname::text FROM pg_index x JOIN pg_class ic ON ic.oid = x.indexrelid
                     WHERE x.indrelid = c.oid AND x.indisclustered), ''),
                    COALESCE((SELECT ic.relname::text FROM pg_index x JOIN pg_class ic ON ic.oid = x.indexrelid
                     WHERE x.indrelid = c.oid AND x.indisreplident), '')
             FROM pg_class c WHERE c.oid = '{SCHEMA}.{t}'::regclass"
        )
    };
    assert_eq!(rows(s, &rel("b1_c")).await, vec![vec!["f".to_string(), "b1_c_ix".to_string(), String::new()]]);
    let stats = |t: &str| {
        format!(
            "SELECT stxname::text, replace(pg_get_statisticsobjdef(oid), '{t}', 'T'), COALESCE(stxstattarget::text, '-1'),
                    COALESCE(obj_description(oid, 'pg_statistic_ext'), '')
             FROM pg_statistic_ext WHERE stxrelid = '{SCHEMA}.{t}'::regclass ORDER BY 1"
        )
    };
    let renamed: Vec<Vec<String>> =
        rows(s, &stats("b1")).await.into_iter().map(|r| r.into_iter().map(|v| v.replace("b1_", "b1_c_")).collect()).collect();
    assert_eq!(renamed.len(), 2);
    assert_eq!(rows(s, &stats("b1_c")).await, renamed);
    let pol = format!("SELECT policyname::text, qual FROM pg_policies WHERE schemaname = '{SCHEMA}' AND tablename = 'b1_c' ORDER BY 1");
    for q in rows(s, &pol).await {
        assert!(q[1].contains("b1_c"), "{q:?}");
    }
    // The clone's policy depends on the clone, not on the original.
    let deps = format!(
        "SELECT count(*) FROM pg_depend d JOIN pg_policy p ON p.oid = d.objid
         WHERE d.classid = 'pg_policy'::regclass AND p.polrelid = '{SCHEMA}.b1_c'::regclass AND d.refobjid = '{SCHEMA}.b1'::regclass"
    );
    assert_eq!(one(s, &deps).await, "0");
    // USING INDEX (a UNIQUE constraint's) and CLUSTER ON the primary key.
    clone(&e, "b2", "b2_c", false).await.unwrap();
    assert_eq!(rows(s, &rel("b2_c")).await, vec![vec!["i".to_string(), "b2_c_pkey".to_string(), "b2_c_u".to_string()]]);
    // Without indexes: what needs one stays out, and the notes say so.
    let r = clone_with(&e, "b1", "b1_n", CloneOptions { with_data: false, with_indexes: false }).await.unwrap();
    assert!(r.notes.iter().any(|n| n.contains("CLUSTER") && n.contains("b1_ix")), "{:?}", r.notes);
    assert_eq!(rows(s, &rel("b1_n")).await, vec![vec!["f".to_string(), String::new(), String::new()]]);
    let r = clone_with(&e, "b2", "b2_n", CloneOptions { with_data: false, with_indexes: false }).await.unwrap();
    assert!(r.notes.iter().any(|n| n.contains("REPLICA IDENTITY USING INDEX b2_u")), "{:?}", r.notes);
    assert_eq!(rows(s, &rel("b2_n")).await, vec![vec!["d".to_string(), "b2_n_pkey".to_string(), String::new()]]);

    if timescale {
        rows(
            s,
            &format!(
                "CREATE EXTENSION IF NOT EXISTS timescaledb;
                 CREATE TABLE {SCHEMA}.metrics (t timestamptz NOT NULL, v float8);
                 SELECT create_hypertable('{SCHEMA}.metrics', 't');
                 INSERT INTO {SCHEMA}.metrics SELECT now() - g * interval '1 day', g FROM generate_series(1, 20) g;
                 CREATE TABLE {SCHEMA}.metrics_vacia (t timestamptz NOT NULL, v float8);
                 SELECT create_hypertable('{SCHEMA}.metrics_vacia', 't');"
            ),
        )
        .await;
        for t in ["metrics", "metrics_vacia"] {
            let err = clone(&e, t, &format!("{t}_c"), true).await.unwrap_err();
            assert!(err.contains("hypertable"), "{err}");
            no_table(s, &format!("{t}_c")).await;
        }
    }
    rows(s, &format!("DROP SCHEMA {SCHEMA} CASCADE")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs dbine-test-postgres"]
async fn postgres() {
    postgres_family(endpoints("postgres", "DBINE_TEST_PG_URL", "postgres://postgres:pw@localhost:25010/postgres"), false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs dbine-test-timescale"]
async fn timescale() {
    postgres_family(endpoints("timescaledb", "DBINE_TEST_TIMESCALE_URL", "postgres://postgres:pw@localhost:25015/postgres"), true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs dbine-test-cockroach"]
async fn cockroach() {
    let e = endpoints("cockroachdb", "DBINE_TEST_COCKROACH_URL", "postgresql://root@localhost:26014/defaultdb");
    let mut w = e.open_target().await.unwrap();
    let s = &mut *w;
    rows(
        s,
        &format!(
            "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA};
             CREATE SEQUENCE {SCHEMA}.compartida START 1000;
             CREATE TABLE {SCHEMA}.cc (
                 id INT8 GENERATED ALWAYS AS IDENTITY (START 100 INCREMENT 5) PRIMARY KEY,
                 a INT8,
                 b INT8 AS (a * 2) STORED,
                 v INT8 AS (a + 1) VIRTUAL,
                 c INT8 AS (7) STORED,
                 s STRING COLLATE en,
                 k INT8 DEFAULT nextval('{SCHEMA}.compartida')
             );
             INSERT INTO {SCHEMA}.cc (a, s) SELECT g, 'x' || g::STRING FROM generate_series(1, 10) g;
             SET serial_normalization = 'sql_sequence';
             CREATE TABLE {SCHEMA}.sr (id SERIAL8 PRIMARY KEY, x INT8);
             INSERT INTO {SCHEMA}.sr (x) SELECT g FROM generate_series(1, 5) g;"
        ),
    )
    .await;

    let r = clone(&e, "cc", "cc_c2", true).await.unwrap();
    assert_eq!(r.rows, 10);
    let (a, b) = (columns(s, "cc").await, columns(s, "cc_c2").await);
    println!("{a:?}\n{b:?}");
    assert_eq!(b[0][1], "ALWAYS", "identity ALWAYS");
    for i in [2, 3, 4] {
        assert_eq!(b[i][2], "ALWAYS", "computed {}", b[i][0]);
    }
    assert_eq!(b[5][3], "en", "collation");
    assert_eq!(b[6][4], a[6][4], "shared sequence");
    let data = |t: &str| format!("SELECT id, a, b, v, c, s, k FROM {SCHEMA}.{t} ORDER BY id");
    assert_eq!(rows(s, &data("cc")).await, rows(s, &data("cc_c2")).await);
    let next = one(s, &format!("INSERT INTO {SCHEMA}.cc_c2 (a) VALUES (1) RETURNING id")).await;
    assert_eq!(next, "150");

    let r = clone(&e, "sr", "sr_c2", true).await.unwrap();
    assert_eq!(r.rows, 5);
    let b = columns(s, "sr_c2").await;
    assert!(b[0][4].contains("sr_c2_id_seq"), "{:?}", b[0]);
    let next = one(s, &format!("INSERT INTO {SCHEMA}.sr_c2 (x) VALUES (6) RETURNING id")).await;
    assert_eq!(next, "6");

    // Index names are per table here: another table's is a valid name.
    // A CHECK NOT VALID that the rows break is kept as it is.
    rows(
        s,
        &format!(
            "CREATE INDEX ix_v ON {SCHEMA}.sr (x);
             ALTER TABLE {SCHEMA}.sr ADD CONSTRAINT ck_x CHECK (x > 3) NOT VALID;"
        ),
    )
    .await;
    let r = clone(&e, "sr", "ix_v", true).await.unwrap();
    assert_eq!(r.rows, 5);
    let ck = |t: &str| {
        format!("SELECT pg_get_constraintdef(oid), convalidated::text FROM pg_constraint WHERE conrelid = '{SCHEMA}.{t}'::regclass AND contype = 'c' AND conname NOT LIKE 'check_crdb%'")
    };
    assert_eq!(rows(s, &ck("sr")).await, rows(s, &ck("ix_v")).await);

    // Comments on an index and a constraint: on the clone's renamed ones
    // (not on a NOT VALID one: CockroachDB stores it with constraint id 0
    // and `pg_description` fails for the whole cluster until it's gone).
    rows(
        s,
        &format!(
            "ALTER TABLE {SCHEMA}.sr ADD CONSTRAINT ck_ok CHECK (x < 100);
             COMMENT ON INDEX {SCHEMA}.sr@ix_v IS 'comentario idx';
             COMMENT ON CONSTRAINT ck_ok ON {SCHEMA}.sr IS 'ck com';"
        ),
    )
    .await;
    let r = clone(&e, "sr", "sr_k", true).await.unwrap();
    assert!(r.notes.is_empty(), "{:?}", r.notes);
    let com = format!(
        "SELECT d.description FROM pg_description d JOIN pg_class c ON c.oid = d.objoid WHERE c.relname = 'sr_k_ix_v'
         UNION ALL SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint
         WHERE conrelid = '{SCHEMA}.sr_k'::regclass AND conname = 'sr_k_ck_ok' ORDER BY 1"
    );
    assert_eq!(rows(s, &com).await, vec![vec!["ck com".to_string()], vec!["comentario idx".to_string()]]);

    // Hash-sharded indexes and primary key (on a hidden shard column the
    // clone gets from the index), column families, zone configurations and
    // row-level security: the clone's are the original's.
    rows(
        s,
        &format!(
            "CREATE TABLE {SCHEMA}.cadv (id INT8 PRIMARY KEY, t STRING, j JSONB, n INT8, g INT8,
                 INDEX cadv_hs (n) USING HASH, UNIQUE INDEX cadv_uh (g) USING HASH WITH (bucket_count = 8), INDEX cadv_t (t),
                 FAMILY f1 (id, t), FAMILY f2 (j, n, g));
             ALTER TABLE {SCHEMA}.cadv CONFIGURE ZONE USING gc.ttlseconds = 600;
             ALTER INDEX {SCHEMA}.cadv@cadv_t CONFIGURE ZONE USING gc.ttlseconds = 700;
             INSERT INTO {SCHEMA}.cadv VALUES (1, 'a', '{{}}', 1, 1), (-2, 'b', NULL, 2, 2);
             ALTER TABLE {SCHEMA}.cadv ENABLE ROW LEVEL SECURITY;
             CREATE POLICY p1 ON {SCHEMA}.cadv USING (id > 0);
             CREATE TABLE {SCHEMA}.hpk (id INT8 PRIMARY KEY USING HASH WITH (bucket_count = 4), v INT8);
             INSERT INTO {SCHEMA}.hpk VALUES (1, 1), (2, 2);"
        ),
    )
    .await;
    let create = |t: &str| format!("SELECT create_statement FROM [SHOW CREATE TABLE {SCHEMA}.{t}]");
    let r = clone(&e, "cadv", "cadv_c", true).await.unwrap();
    assert_eq!(r.rows, 2);
    assert!(r.notes.is_empty(), "{:?}", r.notes);
    let got = one(s, &create("cadv_c")).await;
    println!("{got}");
    for part in ["FAMILY f1 (id, t)", "FAMILY f2 (j, n, g)", "INDEX cadv_c_hs (n ASC) USING HASH WITH (bucket_count=16)", "UNIQUE INDEX cadv_c_uh (g ASC) USING HASH WITH (bucket_count=8)", "gc.ttlseconds = 600", "cadv_c@cadv_c_t CONFIGURE ZONE USING\n\tgc.ttlseconds = 700", "ENABLE ROW LEVEL SECURITY", "CREATE POLICY p1 ON"] {
        assert!(got.contains(part), "{part}: {got}");
    }
    let r = clone_with(&e, "cadv", "cadv_n", CloneOptions { with_data: true, with_indexes: false }).await.unwrap();
    assert!(r.notes.iter().any(|n| n.contains("zona") && n.contains("cadv_t")), "{:?}", r.notes);
    let got = one(s, &create("cadv_n")).await;
    assert!(got.contains("FAMILY f2 (j, n, g)") && got.contains("gc.ttlseconds = 600") && !got.contains("USING HASH"), "{got}");
    let r = clone(&e, "hpk", "hpk_c", true).await.unwrap();
    assert_eq!(r.rows, 2);
    let got = one(s, &create("hpk_c")).await;
    assert!(got.contains("CONSTRAINT hpk_c_pkey PRIMARY KEY (id ASC) USING HASH WITH (bucket_count=4)"), "{got}");

    // Row-level TTL (its expiration column hidden, the rows' own values),
    // a NOT VISIBLE column and ON UPDATE: kept, with and without indexes
    // (the TTL's options failed after the CREATE with an English error;
    // the hidden column was lost and the clone refused after the work).
    rows(
        s,
        &format!(
            "CREATE TABLE {SCHEMA}.tt (id INT8 PRIMARY KEY, v STRING, u TIMESTAMPTZ DEFAULT now() ON UPDATE now())
                 WITH (ttl_expire_after = '30 days', ttl_job_cron = '@daily');
             INSERT INTO {SCHEMA}.tt (id, v, crdb_internal_expiration) VALUES (1, 'a', '2030-01-01'), (2, 'b', '2031-01-01');
             CREATE TABLE {SCHEMA}.k4 (id INT8 PRIMARY KEY, j JSONB, a INT8, b INT8, secret STRING NOT VISIBLE,
                 INVERTED INDEX (j), INDEX (a) STORING (b), UNIQUE INDEX (b) WHERE a > 0, INDEX ((a + b)), CHECK (a >= 0));
             COMMENT ON COLUMN {SCHEMA}.k4.secret IS 'oculta';
             INSERT INTO {SCHEMA}.k4 (id, j, a, b, secret) VALUES (1, '{{\"k\": 1}}', 1, 1, 's1'), (2, NULL, 0, 2, NULL);"
        ),
    )
    .await;
    let hidden = |t: &str| {
        format!(
            "SELECT column_name, is_hidden, COALESCE(column_default, ''), COALESCE(column_on_update, ''), is_nullable
             FROM information_schema.columns WHERE table_schema = '{SCHEMA}' AND table_name = '{t}' ORDER BY ordinal_position"
        )
    };
    let options = |t: &str| format!("SELECT array_to_string(reloptions, ',') FROM pg_class WHERE oid = '{SCHEMA}.{t}'::regclass");
    for (name, with_indexes) in [("tt_c", true), ("tt_n", false)] {
        let r = clone_with(&e, "tt", name, CloneOptions { with_data: true, with_indexes }).await.unwrap();
        assert_eq!(r.rows, 2);
        assert_eq!(rows(s, &hidden("tt")).await, rows(s, &hidden(name)).await);
        assert_eq!(one(s, &options("tt")).await, one(s, &options(name)).await);
        let data = |t: &str| format!("SELECT id, v, crdb_internal_expiration::STRING FROM {SCHEMA}.{t} ORDER BY id");
        assert_eq!(rows(s, &data("tt")).await, rows(s, &data(name)).await);
    }
    for (name, with_indexes) in [("k4_c", true), ("k4_n", false)] {
        let r = clone_with(&e, "k4", name, CloneOptions { with_data: true, with_indexes }).await.unwrap();
        assert_eq!(r.rows, 2);
        assert_eq!(rows(s, &hidden("k4")).await, rows(s, &hidden(name)).await);
        let data = |t: &str| format!("SELECT id, j::STRING, a, b, secret FROM {SCHEMA}.{t} ORDER BY id");
        assert_eq!(rows(s, &data("k4")).await, rows(s, &data(name)).await);
        let com = format!("SELECT col_description('{SCHEMA}.{name}'::regclass, 5)");
        assert_eq!(one(s, &com).await, "oculta");
    }
    let got = one(s, &create("k4_c")).await;
    for part in ["secret STRING NOT VISIBLE NULL", "INVERTED INDEX k4_c_j_idx (j)", "INDEX k4_c_a_idx (a ASC) STORING (b)", "UNIQUE INDEX k4_c_b_key (b ASC) WHERE a > 0:::INT8", "((a + b) ASC)", "CHECK (a >= 0:::INT8)"] {
        assert!(got.contains(part), "{part}: {got}");
    }

    rows(s, &format!("DROP SCHEMA {SCHEMA} CASCADE")).await;
}
