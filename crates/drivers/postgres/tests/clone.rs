//! Same-engine clone against a real server (ignored by default):
//!
//! ```sh
//! cargo test -p dbine-driver-postgres --test clone -- --ignored clone --nocapture
//! ```
//!
//! `DBINE_TEST_POSTGRES_URL`, by default the `dbine-test-postgres` container
//! (`postgres://postgres:pw@localhost:25010/postgres`). It creates and drops
//! the databases `dbine_clone_src` and `dbine_clone_dst`.

use dbine_driver::transfer::{BatchSink, BatchSource, CopySpec, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100_000, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out
}

/// Every row of a query as one line of text.
async fn lines(s: &mut Box<dyn Session>, sql: &str) -> Vec<String> {
    let out = run(s, sql).await;
    let mut v: Vec<String> = out.results[0]
        .rows
        .iter()
        .map(|r| r.iter().map(|c| c.as_str().map(str::to_string).unwrap_or_else(|| c.to_string())).collect::<Vec<_>>().join(" | "))
        .collect();
    v.sort();
    v
}

const SOURCE: &str = r#"
CREATE EXTENSION IF NOT EXISTS btree_gist;
CREATE EXTENSION IF NOT EXISTS citext;
CREATE SCHEMA app;
COMMENT ON SCHEMA app IS 'La aplicación';
CREATE COLLATION app.ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
CREATE TYPE app.mood AS ENUM ('sad', 'ok', 'happy');
COMMENT ON TYPE app.mood IS 'Ánimo';
CREATE TYPE app.addr AS (street text, zip int);
CREATE DOMAIN app.posint AS int NOT NULL DEFAULT 1 CONSTRAINT posint_pos CHECK (VALUE > 0);
COMMENT ON DOMAIN app.posint IS 'Positivo';
CREATE TYPE app.floatrange AS RANGE (subtype = float8, subtype_diff = float8mi);
CREATE SEQUENCE app.counter START 100 INCREMENT 5;
COMMENT ON SEQUENCE app.counter IS 'Códigos';
CREATE FUNCTION app.next_code() RETURNS text LANGUAGE sql AS $$ SELECT 'C' || nextval('app.counter') $$;
CREATE TABLE app.customer (
    id int GENERATED ALWAYS AS IDENTITY (START WITH 10 INCREMENT BY 2 CACHE 3) PRIMARY KEY WITH (fillfactor = 90),
    code text NOT NULL DEFAULT app.next_code(),
    name text COLLATE app.ci NOT NULL,
    email citext UNIQUE,
    mood app.mood DEFAULT 'ok',
    addr app.addr,
    qty app.posint,
    total numeric(12,2) GENERATED ALWAYS AS (qty * 1.5) STORED,
    tags text[],
    during tstzrange,
    fr app.floatrange,
    body text,
    CONSTRAINT qty_chk CHECK (qty < 1000) NOT VALID,
    CONSTRAINT no_overlap EXCLUDE USING gist (id WITH =, during WITH &&) WHERE (id > 0)
) WITH (fillfactor = 80, autovacuum_enabled = false);
ALTER TABLE app.customer ALTER COLUMN body SET STORAGE EXTERNAL;
ALTER TABLE app.customer ALTER COLUMN body SET COMPRESSION pglz;
ALTER TABLE app.customer ALTER COLUMN name SET STATISTICS 500;
ALTER TABLE app.customer SET (toast.autovacuum_enabled = false);
ALTER TABLE app.customer REPLICA IDENTITY FULL;
CREATE INDEX cust_lower_name ON app.customer USING btree (lower(name)) INCLUDE (email) WITH (fillfactor = 70);
CREATE INDEX cust_tags ON app.customer USING gin (tags);
CREATE INDEX cust_code ON app.customer USING hash (code);
CREATE INDEX cust_mood ON app.customer (mood) WHERE mood <> 'ok';
ALTER TABLE app.customer CLUSTER ON cust_lower_name;
COMMENT ON TABLE app.customer IS 'Clientes';
COMMENT ON COLUMN app.customer.name IS 'Nombre';
COMMENT ON CONSTRAINT qty_chk ON app.customer IS 'Tope';
COMMENT ON INDEX app.cust_tags IS 'Etiquetas';
CREATE TABLE app.orders (
    id bigserial,
    customer_id int NOT NULL REFERENCES app.customer (id) ON DELETE CASCADE DEFERRABLE,
    placed date NOT NULL,
    amount numeric(10,2),
    PRIMARY KEY (id, placed, customer_id)
) PARTITION BY RANGE (placed);
CREATE TABLE app.orders_2024 PARTITION OF app.orders FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
CREATE TABLE app.orders_2025 PARTITION OF app.orders FOR VALUES FROM ('2025-01-01') TO ('2026-01-01') PARTITION BY LIST (customer_id);
CREATE TABLE app.orders_2025_a PARTITION OF app.orders_2025 FOR VALUES IN (10, 12);
CREATE TABLE app.orders_2025_rest PARTITION OF app.orders_2025 DEFAULT;
CREATE TABLE app.orders_default PARTITION OF app.orders DEFAULT;
CREATE INDEX orders_amount ON app.orders (amount) WHERE amount > 0;
CREATE INDEX orders_2024_only ON app.orders_2024 (customer_id);
CREATE UNLOGGED TABLE app.scratch (k text PRIMARY KEY, v jsonb DEFAULT '{}');
CREATE TABLE app.base_log (at timestamptz NOT NULL DEFAULT now(), who text);
CREATE TABLE app.audit (msg text) INHERITS (app.base_log);
CREATE FUNCTION app.touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.body := coalesce(NEW.body, ''); RETURN NEW; END $$;
COMMENT ON FUNCTION app.touch() IS 'Toca';
CREATE TRIGGER customer_touch BEFORE INSERT OR UPDATE ON app.customer FOR EACH ROW EXECUTE FUNCTION app.touch();
CREATE TRIGGER customer_off AFTER DELETE ON app.customer FOR EACH ROW EXECUTE FUNCTION app.touch();
ALTER TABLE app.customer DISABLE TRIGGER customer_off;
COMMENT ON TRIGGER customer_touch ON app.customer IS 'Antes';
CREATE PROCEDURE app.reset_counter() LANGUAGE sql AS $$ SELECT setval('app.counter', 100) $$;
CREATE FUNCTION app.customer_count() RETURNS bigint LANGUAGE sql BEGIN ATOMIC SELECT count(*) FROM app.customer; END;
CREATE FUNCTION app.first_customer() RETURNS app.customer LANGUAGE sql AS $$ SELECT * FROM app.customer ORDER BY id LIMIT 1 $$;
CREATE VIEW app.active AS SELECT id, name, qty FROM app.customer WHERE qty > 0 WITH LOCAL CHECK OPTION;
CREATE VIEW app.active_names AS SELECT name FROM app.active;
COMMENT ON VIEW app.active IS 'Activos';
COMMENT ON COLUMN app.active.name IS 'Nombre activo';
CREATE MATERIALIZED VIEW app.totals AS SELECT customer_id, sum(amount) AS s FROM app.orders GROUP BY 1;
CREATE UNIQUE INDEX totals_pk ON app.totals (customer_id);
COMMENT ON MATERIALIZED VIEW app.totals IS 'Totales';
ALTER TABLE app.customer ENABLE ROW LEVEL SECURITY;
ALTER TABLE app.customer FORCE ROW LEVEL SECURITY;
CREATE POLICY see ON app.customer FOR SELECT TO PUBLIC USING (qty > 0);
CREATE POLICY add ON app.customer AS RESTRICTIVE FOR INSERT TO postgres WITH CHECK (qty IS NOT NULL);
COMMENT ON POLICY see ON app.customer IS 'Ver';
INSERT INTO app.customer (name, email, mood, addr, qty, tags, during, fr, body)
SELECT 'n' || i, 'e' || i || '@x', 'happy', ROW('calle ' || i, i)::app.addr, i, ARRAY['a', 'b'],
       tstzrange(now() + make_interval(days => i), now() + make_interval(days => i + 1)), '[1.5,2.5)', repeat('x', 10)
FROM generate_series(1, 20) i;
INSERT INTO app.orders (customer_id, placed, amount) SELECT 10 + 2 * (i % 20), DATE '2024-06-01' + i * 7, i FROM generate_series(1, 80) i;
INSERT INTO app.scratch VALUES ('a', '{"x": 1}');
INSERT INTO app.audit (who, msg) VALUES ('yo', 'hola');
REFRESH MATERIALIZED VIEW app.totals;
"#;

/// The catalog of schema `app`, one line per fact.
async fn catalog(s: &mut Box<dyn Session>) -> Vec<(String, Vec<String>)> {
    let queries = [
        ("relaciones", "SELECT c.relname, c.relkind, c.relpersistence, array_to_string(c.reloptions, ','), pg_get_partkeydef(c.oid),
                pg_get_expr(c.relpartbound, c.oid), c.relrowsecurity, c.relforcerowsecurity, array_to_string(t.reloptions, ','),
                c.relreplident, (SELECT string_agg(p.relname, ',') FROM pg_inherits i JOIN pg_class p ON p.oid = i.inhparent WHERE i.inhrelid = c.oid)
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace LEFT JOIN pg_class t ON t.oid = c.reltoastrelid
         WHERE n.nspname = 'app' AND c.relkind IN ('r', 'p', 'v', 'm', 'S')"),
        ("columnas", "SELECT c.relname, a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull, pg_get_expr(d.adbin, d.adrelid),
                a.attidentity, a.attgenerated, co.collname, a.attstorage, a.attcompression, a.attstattarget, a.attislocal
         FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum LEFT JOIN pg_collation co ON co.oid = a.attcollation
         WHERE n.nspname = 'app' AND c.relkind IN ('r', 'p', 'v', 'm') AND a.attnum > 0 AND NOT a.attisdropped"),
        ("restricciones", "SELECT conrelid::regclass::text, contypid::regtype::text, conname, pg_get_constraintdef(oid), convalidated,
                (SELECT array_to_string(reloptions, ',') FROM pg_class WHERE oid = conindid AND contype IN ('p', 'u', 'x'))
         FROM pg_constraint WHERE connamespace = 'app'::regnamespace AND contype <> 'n'"),
        ("índices", "SELECT pg_get_indexdef(i.indexrelid), i.indisclustered, i.indisreplident FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
         WHERE c.relnamespace = 'app'::regnamespace"),
        ("triggers", "SELECT pg_get_triggerdef(oid), tgenabled FROM pg_trigger WHERE NOT tgisinternal AND tgrelid IN
         (SELECT oid FROM pg_class WHERE relnamespace = 'app'::regnamespace)"),
        ("políticas", "SELECT tablename, policyname, permissive, roles::text, cmd, qual, with_check FROM pg_policies WHERE schemaname = 'app'"),
        ("funciones", "SELECT pg_get_functiondef(p.oid) FROM pg_proc p WHERE p.pronamespace = 'app'::regnamespace"),
        ("vistas", "SELECT c.relname, pg_get_viewdef(c.oid), c.relispopulated FROM pg_class c WHERE c.relnamespace = 'app'::regnamespace AND c.relkind IN ('v', 'm')"),
        ("tipos", "SELECT t.typname, t.typtype, format_type(t.typbasetype, t.typtypmod), t.typnotnull, t.typdefault,
                (SELECT string_agg(enumlabel, ',' ORDER BY enumsortorder) FROM pg_enum WHERE enumtypid = t.oid),
                (SELECT string_agg(attname || ' ' || format_type(atttypid, atttypmod), ',' ORDER BY attnum) FROM pg_attribute WHERE attrelid = t.typrelid AND t.typtype = 'c'),
                (SELECT format_type(rngsubtype, NULL) || rngsubdiff::text FROM pg_range WHERE rngtypid = t.oid)
         FROM pg_type t LEFT JOIN pg_class c ON c.oid = t.typrelid
         WHERE t.typnamespace = 'app'::regnamespace AND t.typtype IN ('e', 'c', 'd', 'r') AND (c.relkind IS NULL OR c.relkind = 'c')"),
        ("secuencias", "SELECT sequencename, data_type::text, start_value, min_value, max_value, increment_by, cycle, cache_size, last_value
         FROM pg_sequences WHERE schemaname = 'app'"),
        ("colaciones", "SELECT collname, collprovider, coalesce(to_jsonb(c) ->> 'colllocale', to_jsonb(c) ->> 'colliculocale'), collisdeterministic FROM pg_collation c WHERE collnamespace = 'app'::regnamespace"),
        ("comentarios", "SELECT pg_describe_object(classoid, objoid, objsubid), description FROM pg_description
         WHERE classoid <> 'pg_extension'::regclass AND pg_describe_object(classoid, objoid, objsubid) LIKE '%app%'"),
        ("extensiones", "SELECT extname FROM pg_extension"),
        ("filas", "SELECT 'customer', count(*) FROM app.customer UNION ALL SELECT 'orders', count(*) FROM app.orders
         UNION ALL SELECT 'orders_2025_a', count(*) FROM app.orders_2025_a UNION ALL SELECT 'scratch', count(*) FROM app.scratch
         UNION ALL SELECT 'audit', count(*) FROM app.audit UNION ALL SELECT 'totals', count(*) FROM app.totals"),
    ];
    let mut out = Vec::new();
    for (name, sql) in queries {
        out.push((name.to_string(), lines(s, sql).await));
    }
    out
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: "table".into(), schema: Some("app".into()), name: name.into() }
}

/// Every leaf partition (the rows live there) with its columns.
async fn leaves(s: &mut Box<dyn Session>, root: &str) -> Vec<(String, Vec<String>)> {
    let sql = format!(
        "SELECT c.relname, string_agg(quote_ident(a.attname), ',' ORDER BY a.attnum) FROM pg_partition_tree('app.{root}') p
         JOIN pg_class c ON c.oid = p.relid JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
         AND a.attgenerated = '' WHERE p.isleaf GROUP BY c.relname"
    );
    lines(s, &sql)
        .await
        .into_iter()
        .map(|l| {
            let (n, cols) = l.split_once(" | ").unwrap();
            (n.to_string(), cols.split(',').map(|c| c.trim_matches('"').to_string()).collect())
        })
        .collect()
}

struct Collect(Vec<RowBatch>);
impl BatchSink for Collect {
    fn begin(&mut self, _: &[TransferColumn]) -> std::io::Result<()> {
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
        self.0.push(b);
        Ok(())
    }
}

struct Batches(std::vec::IntoIter<RowBatch>);
#[dbine_driver::async_trait]
impl BatchSource for Batches {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.next()
    }
}

/// Typed read + `COPY` load (user types travel as text); small tables only.
async fn read_and_load(src: &mut Box<dyn Session>, dst: &mut Box<dyn Session>, spec: &CopySpec) {
    let sink = Arc::new(Mutex::new(Collect(Vec::new())));
    src.read_batches(&spec.source, sink.clone()).await.expect("read_batches");
    let batches = std::mem::take(&mut sink.lock().unwrap().0);
    dst.bulk_load(&spec.target, &[], &mut Batches(batches.into_iter()), &|_| {}).await.expect("bulk_load");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn clone_postgres_every_feature() {
    let url = std::env::var("DBINE_TEST_POSTGRES_URL").unwrap_or_else(|_| "postgres://postgres:pw@localhost:25010/postgres".into());
    let cfg = parse_url("postgres", &url);
    let d: Arc<dyn Driver> = dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == "postgres").unwrap();
    assert!(d.supports_clone());
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    for db in ["dbine_clone_src", "dbine_clone_dst"] {
        run(&mut admin, &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")).await;
        run(&mut admin, &format!("CREATE DATABASE {db}")).await;
    }
    let mut src = d.connect(&cfg, Some("dbine_clone_src")).await.unwrap();
    let mut dst = d.connect(&cfg, Some("dbine_clone_dst")).await.unwrap();
    run(&mut src, SOURCE).await;

    let listed = [table("customer"), table("orders"), table("scratch"), table("base_log"), table("audit")];
    let t0 = Instant::now();
    let script = d.clone_script(src.as_mut(), dst.as_mut(), &listed).await.expect("clone_script");
    println!("script en {:?}: {} antes, {} tablas, {} después", t0.elapsed(), script.before.len(), script.tables.len(), script.after.len());
    for n in &script.notes {
        println!("nota: {n}");
    }

    // Twice: the second pass proves every statement can run again.
    for pass in 0..2 {
        for sql in &script.before {
            run(&mut dst, sql).await;
        }
        for t in &script.tables {
            run(&mut dst, &t.create).await;
            for sql in &t.before_data {
                run(&mut dst, sql).await;
            }
            if pass == 0 {
                // The data: every leaf of a partitioned table, or the table (ONLY its own rows).
                let leaves_of = leaves(&mut src, &t.table.name).await;
                let targets = if leaves_of.is_empty() {
                    let cols = lines(
                        &mut src,
                        &format!(
                            "SELECT a.attname FROM pg_attribute a WHERE a.attrelid = 'app.{}'::regclass AND a.attnum > 0
                             AND NOT a.attisdropped AND a.attgenerated = ''",
                            t.table.name
                        ),
                    )
                    .await;
                    vec![(t.table.name.clone(), cols)]
                } else {
                    leaves_of
                };
                for (leaf, cols) in targets {
                    let spec = CopySpec {
                        source: ReadSpec { table: table(&leaf), columns: Some(cols.clone()), filter: None },
                        target: LoadSpec {
                            table: table(&leaf),
                            columns: cols,
                            table_lock: false,
                            keep_identity: true,
                            commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
                            commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
                        },
                    };
                    // Inherited tables: only the parent's own rows.
                    let mut spec = spec;
                    if t.table.name == "base_log" {
                        spec.source.filter = Some("tableoid = 'app.base_log'::regclass".into());
                    }
                    match d.copy_native(src.as_mut(), dst.as_mut(), &spec, &|_| {}).await {
                        Ok(_) => {}
                        Err(dbine_driver::Error::Unsupported(_)) => read_and_load(&mut src, &mut dst, &spec).await,
                        Err(e) => panic!("copy_native: {e}"),
                    }
                }
            }
            for sql in &t.after_data {
                run(&mut dst, sql).await;
            }
        }
        for sql in &script.after {
            run(&mut dst, sql).await;
        }
    }

    let a = catalog(&mut src).await;
    let b = catalog(&mut dst).await;
    let mut diffs = 0;
    for ((name, x), (_, y)) in a.iter().zip(&b) {
        for l in x.iter().filter(|l| !y.contains(l)) {
            println!("[{name}] solo en el origen: {l}");
            diffs += 1;
        }
        for l in y.iter().filter(|l| !x.contains(l)) {
            println!("[{name}] solo en el destino: {l}");
            diffs += 1;
        }
    }
    println!("{} hechos comparados, {diffs} diferencias", a.iter().map(|x| x.1.len()).sum::<usize>());

    // One partition alone: its parents come first, what points elsewhere is noted.
    let part = d.clone_script(src.as_mut(), dst.as_mut(), &[table("orders_2025_a")]).await.expect("clone_script");
    assert!(part.before.iter().any(|s| s.contains("TABLE IF NOT EXISTS \"app\".\"orders\" (")), "{:?}", part.before);
    let parent_at = part.before.iter().position(|s| s.contains("\"app\".\"orders\" (")).unwrap();
    let mid_at = part.before.iter().position(|s| s.contains("\"app\".\"orders_2025\" PARTITION OF")).unwrap();
    assert!(parent_at < mid_at);
    assert!(part.tables[0].create.contains("PARTITION OF \"app\".\"orders_2025\" FOR VALUES IN (10, 12)"));
    for n in &part.notes {
        println!("nota (partición): {n}");
    }
    assert!(part.notes.iter().any(|n| n.contains("clave foránea")));
    assert!(part.notes.iter().any(|n| n.contains("vistas")));
    drop(src);
    drop(dst);
    for db in ["dbine_clone_src", "dbine_clone_dst"] {
        run(&mut admin, &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")).await;
    }
    assert_eq!(diffs, 0);
}
