//! "Clonar tabla" on SQL Server, against a real server: IDENTITY with its
//! own seed and increment, named DEFAULT constraints, computed and
//! rowversion columns, a schema and names with spaces and `ñ`, constraint
//! names another table already has, and a 100-character name; sparse
//! columns with a column set, and system-versioned temporal tables;
//! partition schemes and filegroups of the table and its indexes.
//!
//! Ignored by default. Reads `DBINE_TEST_SQLSERVER_URL`
//! (`mssql://user:pass@host:port`), by default the `dbine-test-sqlserver`
//! container:
//!
//! ```sh
//! cargo test -p dbine-transfer --test clone_table_sqlserver -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};
use dbine_transfer::clone_table::{clone_table, CloneControl, CloneOptions, CloneReport, CloneRequest, ConfigEndpoints};
use std::sync::Arc;

const DEFAULT_URL: &str = "mssql://sa:Pw_12345!@localhost:25013";
const DB: &str = "dbine_clone_table";
const SCHEMA: &str = "ventas ñ";
const TABLE: &str = "clien tes-ñ";

fn config() -> ConnectionConfig {
    let url = std::env::var("DBINE_TEST_SQLSERVER_URL").unwrap_or_else(|_| DEFAULT_URL.into());
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hostport.rsplit_once(':').unwrap();
    ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    }
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

fn lit(s: &str) -> String {
    s.replace('\'', "''")
}

async fn clone(endpoints: &Arc<ConfigEndpoints>, name: &str, with_data: bool) -> Result<CloneReport, String> {
    let req = CloneRequest {
        source: ObjectRef { kind: "table".into(), schema: Some(SCHEMA.into()), name: TABLE.into() },
        new_name: name.into(),
        options: CloneOptions { with_data, with_indexes: true },
    };
    clone_table(endpoints.clone() as Arc<dyn dbine_transfer::Endpoints>, req, &CloneControl::default(), |e| println!("{e:?}"))
        .await
        .map_err(|e| e.to_string())
}

/// `(seed, increment)` of a table's IDENTITY.
async fn identity(s: &mut dyn Session, table: &str) -> Vec<String> {
    let n = lit(&format!("[{SCHEMA}].[{table}]"));
    rows(s, &format!("SELECT CAST(IDENT_SEED(N'{n}') AS nvarchar(40)), CAST(IDENT_INCR(N'{n}') AS nvarchar(40))")).await.remove(0)
}

/// The id a new row gets.
async fn next_id(s: &mut dyn Session, table: &str) -> String {
    let sql = format!("INSERT INTO [{SCHEMA}].[{table}] (codigo) VALUES (N'nuevo'); SELECT CAST(SCOPE_IDENTITY() AS nvarchar(40));");
    rows(s, &sql).await.pop().unwrap().remove(0)
}

/// `(user-given?, name)` of each DEFAULT, by column.
async fn defaults(s: &mut dyn Session, table: &str) -> Vec<Vec<String>> {
    let n = lit(&format!("[{SCHEMA}].[{table}]"));
    rows(
        s,
        &format!(
            "SELECT c.name, CAST(dc.is_system_named AS int), dc.name, dc.definition FROM sys.default_constraints dc
               JOIN sys.columns c ON c.object_id = dc.parent_object_id AND c.column_id = dc.parent_column_id
              WHERE dc.parent_object_id = OBJECT_ID(N'{n}') ORDER BY c.column_id"
        ),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live SQL Server: see the top of the file"]
async fn sql_server_clone_is_faithful() {
    let driver: Arc<dyn Driver> = dbine_drivers::find("sqlserver").expect("sqlserver driver").clone();
    let cfg = config();
    let mut master = driver.connect(&cfg, Some("master")).await.unwrap();
    rows(&mut *master, &format!("IF DB_ID(N'{DB}') IS NOT NULL BEGIN ALTER DATABASE [{DB}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DB}]; END; CREATE DATABASE [{DB}];")).await;
    let endpoints = Arc::new(ConfigEndpoints { driver: driver.clone(), config: cfg.clone(), database: Some(DB.into()) });
    let mut w = driver.connect(&cfg, Some(DB)).await.unwrap();
    rows(&mut *w, &format!("EXEC (N'CREATE SCHEMA [{SCHEMA}]')")).await;
    rows(
        &mut *w,
        &format!(
            "CREATE TABLE [{SCHEMA}].[{TABLE}] (
                id int IDENTITY(1000,5) CONSTRAINT [PK_{TABLE}] PRIMARY KEY,
                codigo nvarchar(20) NULL CONSTRAINT [UQ_{TABLE}_codigo] UNIQUE,
                nombre nvarchar(50) NOT NULL CONSTRAINT DF_clien_nombre DEFAULT (N'sin nombre'),
                monto decimal(10,2) NULL CONSTRAINT DF_clien_monto DEFAULT ((0)),
                doble AS (monto * 2),
                v rowversion,
                creado datetime2 NOT NULL CONSTRAINT DF_clien_creado DEFAULT (SYSDATETIME()),
                otro int NULL DEFAULT ((7))
            );
            INSERT INTO [{SCHEMA}].[{TABLE}] (codigo, nombre, monto)
            SELECT TOP (300) CONCAT(N'c', ROW_NUMBER() OVER (ORDER BY (SELECT 1))), N'Año ñ', 1.5 FROM sys.all_objects;
            DELETE FROM [{SCHEMA}].[{TABLE}] WHERE id > 2400;"
        ),
    )
    .await;

    // The names the clone's PK and UQ would take are someone else's.
    let name = format!("{TABLE}_20260930_000000");
    rows(
        &mut *w,
        &format!("CREATE TABLE [{SCHEMA}].[otra] (a int CONSTRAINT [PK_{name}] PRIMARY KEY, b int CONSTRAINT [UQ_{name}_codigo] UNIQUE);"),
    )
    .await;

    let report = clone(&endpoints, &name, true).await.unwrap_or_else(|e| panic!("clone failed: {e}"));
    println!("{report:?}");
    assert_eq!(report.rows, 281);
    assert!(report.notes.iter().any(|n| n.contains("ya usaba otro objeto")), "{:?}", report.notes);
    // Same rows.
    let sum = |t: &str| format!("SELECT COUNT(*), CHECKSUM_AGG(CHECKSUM(id, codigo, nombre, monto, doble, creado, otro)) FROM [{SCHEMA}].[{t}]");
    assert_eq!(rows(&mut *w, &sum(&name)).await, rows(&mut *w, &sum(TABLE)).await);
    // Same seed and increment; the next row gets the same id in both.
    assert_eq!(identity(&mut *w, &name).await, vec!["1000", "5"]);
    assert_eq!(next_id(&mut *w, &name).await, next_id(&mut *w, TABLE).await);
    // Named defaults keep a name of their own (renamed like the other
    // constraints), the unnamed one stays unnamed; same expressions.
    let (a, b) = (defaults(&mut *w, TABLE).await, defaults(&mut *w, &name).await);
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(&b) {
        assert_eq!((&x[0], &x[1], &x[3]), (&y[0], &y[1], &y[3]), "{a:?}\n{b:?}");
        if x[1] == "0" {
            assert_eq!(y[2], format!("{name}_{}", x[2]), "{b:?}");
        }
    }

    // Structure only: it starts where the original started.
    let bare = format!("{TABLE}_solo");
    clone(&endpoints, &bare, false).await.unwrap_or_else(|e| panic!("clone failed: {e}"));
    assert_eq!(identity(&mut *w, &bare).await, vec!["1000", "5"]);
    assert_eq!(next_id(&mut *w, &bare).await, "1000");

    // 100 characters (200 bytes): within sysname's 128.
    let long = "ñ".repeat(100);
    clone(&endpoints, &long, false).await.unwrap_or_else(|e| panic!("clone failed: {e}"));
    let e = clone(&endpoints, &"ñ".repeat(129), false).await.unwrap_err();
    assert!(e.contains("128"), "{e}");

    drop(w);
    rows(&mut *master, &format!("ALTER DATABASE [{DB}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DB}];")).await;
}

async fn clone_of(endpoints: &Arc<ConfigEndpoints>, schema: &str, source: &str, name: &str, with_data: bool) -> Result<CloneReport, String> {
    let req = CloneRequest {
        source: ObjectRef { kind: "table".into(), schema: Some(schema.into()), name: source.into() },
        new_name: name.into(),
        options: CloneOptions { with_data, with_indexes: true },
    };
    clone_table(endpoints.clone() as Arc<dyn dbine_transfer::Endpoints>, req, &CloneControl::default(), |e| println!("{e:?}"))
        .await
        .map_err(|e| e.to_string())
}

/// A decimal(38,0) IDENTITY past bigint, NOT FOR REPLICATION, and
/// server-named constraints (`PK__…`, `CK__…`), which stay server-named.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live SQL Server: see the top of the file"]
async fn sql_server_big_identity_and_system_names() {
    const DB2: &str = "dbine_clone_table_ident";
    let driver: Arc<dyn Driver> = dbine_drivers::find("sqlserver").expect("sqlserver driver").clone();
    let cfg = config();
    let mut master = driver.connect(&cfg, Some("master")).await.unwrap();
    rows(&mut *master, &format!("IF DB_ID(N'{DB2}') IS NOT NULL BEGIN ALTER DATABASE [{DB2}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DB2}]; END; CREATE DATABASE [{DB2}];")).await;
    let endpoints = Arc::new(ConfigEndpoints { driver: driver.clone(), config: cfg.clone(), database: Some(DB2.into()) });
    let mut w = driver.connect(&cfg, Some(DB2)).await.unwrap();
    rows(
        &mut *w,
        "CREATE TABLE dbo.[big dec] (id decimal(38,0) IDENTITY(99999999999999999999999999999999,7) NOT FOR REPLICATION PRIMARY KEY, v int CHECK (v >= 0));
         INSERT INTO dbo.[big dec] (v) VALUES (1), (2), (3);
         CREATE TABLE dbo.nfr (id int IDENTITY(10,3) NOT FOR REPLICATION PRIMARY KEY, v int);
         INSERT INTO dbo.nfr (v) VALUES (1), (2);",
    )
    .await;
    // (seed, increment, NOT FOR REPLICATION, current) of the IDENTITY, and the
    // table's constraints as (type, user-named?).
    let ident = |t: &str| {
        format!(
            "SELECT CAST(IDENT_SEED(N'dbo.[{t}]') AS nvarchar(50)), CAST(IDENT_INCR(N'dbo.[{t}]') AS nvarchar(50)),
                    CAST(ic.is_not_for_replication AS nvarchar(1)), CAST(IDENT_CURRENT(N'dbo.[{t}]') AS nvarchar(50))
               FROM sys.identity_columns ic WHERE ic.object_id = OBJECT_ID(N'dbo.[{t}]')"
        )
    };
    let named = |t: &str| {
        format!(
            "SELECT type, CAST(is_system_named AS nvarchar(1)) FROM (
               SELECT type, is_system_named, parent_object_id FROM sys.key_constraints
               UNION ALL SELECT type, is_system_named, parent_object_id FROM sys.check_constraints) k
              WHERE parent_object_id = OBJECT_ID(N'dbo.[{t}]') ORDER BY type"
        )
    };
    for (source, clone_name) in [("big dec", "big dec_c"), ("nfr", "nfr_c")] {
        let report = clone_of(&endpoints, "dbo", source, clone_name, true).await.unwrap_or_else(|e| panic!("clone of {source} failed: {e}"));
        println!("{report:?}");
        let a = rows(&mut *w, &ident(source)).await;
        assert_eq!(rows(&mut *w, &ident(clone_name)).await, a, "{source}");
        assert_eq!(a[0][2], "1");
        assert_eq!(rows(&mut *w, &named(clone_name)).await, rows(&mut *w, &named(source)).await, "{source}");
        let sum = |t: &str| format!("SELECT COUNT(*), CAST(SUM(id) AS nvarchar(50)) FROM dbo.[{t}]");
        assert_eq!(rows(&mut *w, &sum(clone_name)).await, rows(&mut *w, &sum(source)).await);
        // Structure only too.
        let bare = format!("{source}_solo");
        clone_of(&endpoints, "dbo", source, &bare, false).await.unwrap_or_else(|e| panic!("clone of {source} failed: {e}"));
        let b = rows(&mut *w, &ident(&bare)).await;
        assert_eq!(b[0][..3], a[0][..3], "{source}");
    }
    // The next row continues where the original's would.
    let next = |t: &str| format!("INSERT INTO dbo.[{t}] (v) VALUES (9); SELECT CAST(SCOPE_IDENTITY() AS nvarchar(50));");
    assert_eq!(rows(&mut *w, &next("big dec_c")).await.pop(), rows(&mut *w, &next("big dec")).await.pop());

    drop(w);
    rows(&mut *master, &format!("ALTER DATABASE [{DB2}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DB2}];")).await;
}

/// The primary key's clustering, key order and storage options (which
/// `KeyDef` doesn't carry), and UNIQUE constraints named by the server,
/// also as a full-text index's KEY INDEX (when full-text is installed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live SQL Server: see the top of the file"]
async fn sql_server_primary_key_options_and_server_named_uniques() {
    const DB3: &str = "dbine_clone_table_pk";
    let driver: Arc<dyn Driver> = dbine_drivers::find("sqlserver").expect("sqlserver driver").clone();
    let cfg = config();
    let mut master = driver.connect(&cfg, Some("master")).await.unwrap();
    rows(&mut *master, &format!("IF DB_ID(N'{DB3}') IS NOT NULL BEGIN ALTER DATABASE [{DB3}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DB3}]; END; CREATE DATABASE [{DB3}];")).await;
    let endpoints = Arc::new(ConfigEndpoints { driver: driver.clone(), config: cfg.clone(), database: Some(DB3.into()) });
    let mut w = driver.connect(&cfg, Some(DB3)).await.unwrap();
    for sql in [
        "CREATE TABLE dbo.pkopt (id int NOT NULL, k int NOT NULL, CONSTRAINT PK_pkopt PRIMARY KEY NONCLUSTERED (id DESC) WITH (DATA_COMPRESSION = ROW, FILLFACTOR = 80), \
         CONSTRAINT UQ_pkopt_k UNIQUE CLUSTERED (k) WITH (DATA_COMPRESSION = PAGE)); INSERT dbo.pkopt VALUES (1,1),(2,2);",
        "CREATE TABLE dbo.pkfill (id int NOT NULL CONSTRAINT PK_pkfill PRIMARY KEY WITH (FILLFACTOR = 70), v int); INSERT dbo.pkfill VALUES (1,1);",
        "CREATE TABLE dbo.pkheap (id int NOT NULL, v int NULL, CONSTRAINT PK_pkheap PRIMARY KEY NONCLUSTERED (id) WITH (PAD_INDEX = ON, FILLFACTOR = 50, \
         IGNORE_DUP_KEY = ON, STATISTICS_NORECOMPUTE = ON, ALLOW_PAGE_LOCKS = OFF, ALLOW_ROW_LOCKS = OFF)); INSERT dbo.pkheap VALUES (1,1);",
        "CREATE TABLE dbo.pkcomp (a int NOT NULL, b int NOT NULL, PRIMARY KEY CLUSTERED (a ASC, b DESC) WITH (FILLFACTOR = 90, DATA_COMPRESSION = PAGE)); \
         INSERT dbo.pkcomp VALUES (1,1),(1,2);",
        "CREATE PARTITION FUNCTION pf_pk (int) AS RANGE LEFT FOR VALUES (10, 20);",
        "CREATE PARTITION SCHEME ps_pk AS PARTITION pf_pk ALL TO ([PRIMARY]);",
        "CREATE TABLE dbo.pkpart (id int NOT NULL, CONSTRAINT PK_pkpart PRIMARY KEY CLUSTERED (id) \
         WITH (DATA_COMPRESSION = ROW ON PARTITIONS (1), DATA_COMPRESSION = PAGE ON PARTITIONS (3)) ON ps_pk(id)); INSERT dbo.pkpart VALUES (5),(15),(25);",
        "CREATE TABLE dbo.uq (id int NOT NULL UNIQUE, v int NOT NULL, CONSTRAINT UQ_uq_v UNIQUE (v DESC)); INSERT dbo.uq VALUES (1,1),(2,2);",
    ] {
        rows(&mut *w, sql).await;
    }
    // The primary key's index, name aside.
    let key = |t: &str| {
        format!(
            "SELECT i.type_desc, CAST(i.fill_factor AS nvarchar(5)), CAST(i.is_padded AS nvarchar(1)), CAST(i.ignore_dup_key AS nvarchar(1)),
                    CAST(i.allow_row_locks AS nvarchar(1)), CAST(i.allow_page_locks AS nvarchar(1)), CAST(ISNULL(st.no_recompute, 0) AS nvarchar(1)),
                    ds.name, CAST(k.is_system_named AS nvarchar(1)),
                    (SELECT c.name + CASE WHEN ic.is_descending_key = 1 THEN N' DESC' ELSE N'' END + N',' FROM sys.index_columns ic
                       JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
                      WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.key_ordinal > 0 ORDER BY ic.key_ordinal FOR XML PATH('')),
                    (SELECT p.data_compression_desc + N',' FROM sys.partitions p WHERE p.object_id = i.object_id AND p.index_id = i.index_id
                      ORDER BY p.partition_number FOR XML PATH(''))
               FROM sys.indexes i JOIN sys.key_constraints k ON k.parent_object_id = i.object_id AND k.unique_index_id = i.index_id AND k.type = 'PK'
               LEFT JOIN sys.stats st ON st.object_id = i.object_id AND st.stats_id = i.index_id
               JOIN sys.data_spaces ds ON ds.data_space_id = i.data_space_id
              WHERE i.object_id = OBJECT_ID(N'dbo.[{t}]')"
        )
    };
    for t in ["pkopt", "pkfill", "pkheap", "pkcomp", "pkpart"] {
        let c = format!("{t}_c");
        let report = clone_of(&endpoints, "dbo", t, &c, true).await.unwrap_or_else(|e| panic!("clone of {t} failed: {e}"));
        assert!(report.notes.is_empty(), "{t}: {:?}", report.notes);
        let a = rows(&mut *w, &key(t)).await;
        assert_eq!(a.len(), 1, "{t}");
        assert_eq!(rows(&mut *w, &key(&c)).await, a, "{t}");
    }
    // Server-named UNIQUE constraints stay server-named; named ones renamed.
    let uniques = |t: &str| {
        format!(
            "SELECT CAST(k.is_system_named AS nvarchar(1)), (SELECT c.name + CASE WHEN ic.is_descending_key = 1 THEN N' DESC' ELSE N'' END + N',' \
             FROM sys.index_columns ic JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
             WHERE ic.object_id = k.parent_object_id AND ic.index_id = k.unique_index_id ORDER BY ic.key_ordinal FOR XML PATH('')), \
             CASE WHEN k.is_system_named = 1 THEN N'' ELSE k.name END \
             FROM sys.key_constraints k WHERE k.parent_object_id = OBJECT_ID(N'dbo.[{t}]') AND k.type = 'UQ' ORDER BY 2"
        )
    };
    let report = clone_of(&endpoints, "dbo", "uq", "uq_c", true).await.unwrap_or_else(|e| panic!("clone of uq failed: {e}"));
    assert!(report.renames.iter().all(|r| !r.from.starts_with("UQ__")), "{:?}", report.renames);
    let mut want = rows(&mut *w, &uniques("uq")).await;
    for r in &mut want {
        if let Some(x) = report.renames.iter().find(|x| x.from == r[2]) {
            r[2] = x.to.clone();
        }
    }
    assert_eq!(report.renames.len(), 1, "{:?}", report.renames);
    assert_eq!(rows(&mut *w, &uniques("uq_c")).await, want);

    let fulltext = rows(&mut *w, "SELECT CAST(ISNULL(FULLTEXTSERVICEPROPERTY('IsFullTextInstalled'), 0) AS nvarchar(1))").await;
    if fulltext[0][0] == "1" {
        rows(
            &mut *w,
            "CREATE FULLTEXT CATALOG cat_pk; CREATE TABLE dbo.ft_uq (id int NOT NULL UNIQUE, t nvarchar(100) NULL); INSERT dbo.ft_uq VALUES (1, N'uno');
             DECLARE @uq sysname = (SELECT name FROM sys.key_constraints WHERE parent_object_id = OBJECT_ID(N'dbo.ft_uq') AND type = 'UQ');
             EXEC(N'CREATE FULLTEXT INDEX ON dbo.ft_uq (t) KEY INDEX ' + @uq + N' ON cat_pk WITH CHANGE_TRACKING OFF, NO POPULATION');",
        )
        .await;
        clone_of(&endpoints, "dbo", "ft_uq", "ft_uq_c", true).await.unwrap_or_else(|e| panic!("clone of ft_uq failed: {e}"));
        let ft = rows(
            &mut *w,
            "SELECT CAST(k.is_system_named AS nvarchar(1)), CAST((SELECT COUNT(*) FROM sys.key_constraints WHERE parent_object_id = f.object_id AND type = 'UQ') AS nvarchar(5))
               FROM sys.fulltext_indexes f JOIN sys.key_constraints k ON k.parent_object_id = f.object_id AND k.unique_index_id = f.unique_index_id
              WHERE f.object_id = OBJECT_ID(N'dbo.ft_uq_c')",
        )
        .await;
        assert_eq!(ft, vec![vec!["1".to_string(), "1".to_string()]], "the full-text index's key is the clone's server-named UNIQUE");
    } else {
        println!("sin búsqueda de texto completo: no se prueba su KEY INDEX");
    }

    drop(w);
    rows(&mut *master, &format!("ALTER DATABASE [{DB3}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DB3}];")).await;
}

/// A primary XML index and secondary XML indexes (`USING XML INDEX … FOR
/// PATH / VALUE / PROPERTY`): the clone's secondary ones are built on the
/// clone's primary, renamed (also shortened, and when another object of
/// the schema already has the name).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live SQL Server: see the top of the file"]
async fn sql_server_xml_indexes() {
    const DB4: &str = "dbine_clone_table_xml";
    let driver: Arc<dyn Driver> = dbine_drivers::find("sqlserver").expect("sqlserver driver").clone();
    let cfg = config();
    let mut master = driver.connect(&cfg, Some("master")).await.unwrap();
    rows(&mut *master, &format!("IF DB_ID(N'{DB4}') IS NOT NULL BEGIN ALTER DATABASE [{DB4}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DB4}]; END; CREATE DATABASE [{DB4}];")).await;
    let endpoints = Arc::new(ConfigEndpoints { driver: driver.clone(), config: cfg.clone(), database: Some(DB4.into()) });
    let mut w = driver.connect(&cfg, Some(DB4)).await.unwrap();
    for sql in [
        "CREATE SCHEMA [ventas odd];",
        "CREATE TABLE [ventas odd].kinds (id int NOT NULL CONSTRAINT PK_kinds PRIMARY KEY, x xml NULL, y xml NULL);
         INSERT [ventas odd].kinds VALUES (1, N'<a><b>1</b></a>', NULL), (2, N'<a/>', N'<c/>');",
        "CREATE PRIMARY XML INDEX PXML_kinds ON [ventas odd].kinds (x);",
        "CREATE XML INDEX SXML_kinds ON [ventas odd].kinds (x) USING XML INDEX PXML_kinds FOR PATH;",
        "CREATE XML INDEX SXMLV_kinds ON [ventas odd].kinds (x) USING XML INDEX PXML_kinds FOR VALUE;",
        "CREATE PRIMARY XML INDEX PXMLY_kinds ON [ventas odd].kinds (y);",
        "CREATE XML INDEX SXMLY_kinds ON [ventas odd].kinds (y) USING XML INDEX PXMLY_kinds FOR PROPERTY;",
        // Another table of the schema already has the name the primary would get.
        "CREATE TABLE [ventas odd].otra (v int CONSTRAINT PXML_kx CHECK (v > 0));",
    ] {
        rows(&mut *w, sql).await;
    }
    // Each XML index as (type, secondary type, the column, its primary's
    // name through the renames).
    let xml = |t: &str| {
        format!(
            "SELECT x.name, ISNULL(x.secondary_type_desc, N''), c.name, ISNULL(p.name, N'') FROM sys.xml_indexes x
               JOIN sys.index_columns ic ON ic.object_id = x.object_id AND ic.index_id = x.index_id
               JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
               LEFT JOIN sys.xml_indexes p ON p.object_id = x.object_id AND p.index_id = x.using_xml_index_id
              WHERE x.object_id = OBJECT_ID(N'[ventas odd].[{}]') ORDER BY c.name, x.secondary_type_desc",
            lit(t).replace(']', "]]")
        )
    };
    let original = rows(&mut *w, &xml("kinds")).await;
    assert_eq!(original.len(), 5);
    let long = "k".repeat(124);
    for name in ["kinds_20260930_033332", "kc", "kx", long.as_str()] {
        let report = clone_of(&endpoints, "ventas odd", "kinds", name, true).await.unwrap_or_else(|e| panic!("clone {name} failed: {e}"));
        let to = |n: &str| report.renames.iter().find(|r| r.from == n).map_or(n.to_string(), |r| r.to.clone());
        let want: Vec<Vec<String>> = original.iter().map(|r| vec![to(&r[0]), r[1].clone(), r[2].clone(), if r[3].is_empty() { String::new() } else { to(&r[3]) }]).collect();
        assert_eq!(rows(&mut *w, &xml(name)).await, want, "{name}: {:?}", report.renames);
        let n = rows(&mut *w, &format!("SELECT CAST(COUNT(*) AS nvarchar(5)) FROM [ventas odd].[{name}] WHERE x.exist('/a') = 1")).await;
        assert_eq!(n[0][0], "2", "{name}");
        rows(&mut *w, &format!("DROP TABLE [ventas odd].[{name}]")).await;
    }

    drop(w);
    rows(&mut *master, &format!("ALTER DATABASE [{DB4}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DB4}];")).await;
}

/// Sparse columns and a column set (`database_schema` reports them as
/// plain columns), and system-versioned temporal tables: the clone has
/// them as the original (SPARSE, COLUMN_SET, PERIOD with GENERATED ALWAYS
/// and HIDDEN columns, SYSTEM_VERSIONING with a history table of its own
/// holding the original's history rows, the retention).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live SQL Server: see the top of the file"]
async fn sql_server_sparse_columns_and_temporal_tables() {
    const DB5: &str = "dbine_clone_table_temporal";
    let driver: Arc<dyn Driver> = dbine_drivers::find("sqlserver").expect("sqlserver driver").clone();
    let cfg = config();
    let mut master = driver.connect(&cfg, Some("master")).await.unwrap();
    rows(&mut *master, &format!("IF DB_ID(N'{DB5}') IS NOT NULL BEGIN ALTER DATABASE [{DB5}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DB5}]; END; CREATE DATABASE [{DB5}];")).await;
    let endpoints = Arc::new(ConfigEndpoints { driver: driver.clone(), config: cfg.clone(), database: Some(DB5.into()) });
    let mut w = driver.connect(&cfg, Some(DB5)).await.unwrap();
    for sql in [
        "CREATE SCHEMA [ventas odd];",
        "CREATE TABLE [ventas odd].esparsa (id int PRIMARY KEY, s1 int SPARSE NULL, s2 nvarchar(20) COLLATE Latin1_General_CS_AS SPARSE NULL,
           cs xml COLUMN_SET FOR ALL_SPARSE_COLUMNS, u AS (id * 2));
         INSERT INTO [ventas odd].esparsa (id, s1, s2) VALUES (1, 1, N'a'), (2, NULL, N'b'), (3, NULL, NULL);",
        "CREATE TABLE [ventas odd].temporal (id int PRIMARY KEY, v nvarchar(20) NULL,
           vf datetime2 GENERATED ALWAYS AS ROW START HIDDEN NOT NULL, vt datetime2 GENERATED ALWAYS AS ROW END NOT NULL,
           PERIOD FOR SYSTEM_TIME (vf, vt))
         WITH (SYSTEM_VERSIONING = ON (HISTORY_TABLE = [ventas odd].temporal_hist, HISTORY_RETENTION_PERIOD = 6 MONTHS));",
        "INSERT INTO [ventas odd].temporal (id, v) VALUES (1, N'a'), (2, N'b');",
        "UPDATE [ventas odd].temporal SET v = N'a2' WHERE id = 1;",
        "DELETE FROM [ventas odd].temporal WHERE id = 2;",
        // History named by the server.
        "CREATE TABLE dbo.auto_h (id int PRIMARY KEY, v int, vf datetime2 GENERATED ALWAYS AS ROW START NOT NULL,
           vt datetime2 GENERATED ALWAYS AS ROW END NOT NULL, PERIOD FOR SYSTEM_TIME (vf, vt)) WITH (SYSTEM_VERSIONING = ON);
         INSERT INTO dbo.auto_h (id, v) VALUES (1, 1);",
        // A period without versioning.
        "CREATE TABLE dbo.periodo (id int PRIMARY KEY, vf datetime2 GENERATED ALWAYS AS ROW START NOT NULL,
           vt datetime2 GENERATED ALWAYS AS ROW END NOT NULL, PERIOD FOR SYSTEM_TIME (vf, vt));
         INSERT INTO dbo.periodo (id) VALUES (1);",
        // The name the clone's history would take is someone else's.
        "CREATE TABLE [ventas odd].temporal_t_hist (x int);",
    ] {
        rows(&mut *w, sql).await;
    }
    let obj = |s: &str, t: &str| format!("[{}].[{}]", s.replace(']', "]]"), t.replace(']', "]]")).replace('\'', "''");

    // Sparse: the flags, and the column set follows the sparse columns.
    let sparse = |t: &str| {
        format!(
            "SELECT name, CAST(is_sparse AS nvarchar(1)) + CAST(is_column_set AS nvarchar(1)) FROM sys.columns
              WHERE object_id = OBJECT_ID(N'{}') ORDER BY column_id",
            obj("ventas odd", t)
        )
    };
    let want = rows(&mut *w, &sparse("esparsa")).await;
    for (name, with_data) in [("esparsa_c", true), ("esparsa_s", false)] {
        let report = clone_of(&endpoints, "ventas odd", "esparsa", name, with_data).await.unwrap_or_else(|e| panic!("clone {name} failed: {e}"));
        println!("{report:?}");
        assert_eq!(rows(&mut *w, &sparse(name)).await, want, "{name}");
    }
    let cs = |t: &str| format!("SELECT CAST(id AS nvarchar(5)), ISNULL(CAST(cs AS nvarchar(max)), N'-') FROM [ventas odd].[{t}] ORDER BY id");
    assert_eq!(rows(&mut *w, &cs("esparsa_c")).await, rows(&mut *w, &cs("esparsa")).await);
    rows(&mut *w, "UPDATE [ventas odd].esparsa_c SET s1 = 9 WHERE id = 3").await;
    assert_eq!(rows(&mut *w, &cs("esparsa_c")).await[2][1], "<s1>9</s1>");

    // Temporal: (temporal type, retention, each column's GENERATED ALWAYS
    // and HIDDEN), and the rows FOR SYSTEM_TIME ALL.
    let temporal = |s: &str, t: &str| {
        let o = obj(s, t);
        format!(
            "SELECT t.temporal_type_desc COLLATE DATABASE_DEFAULT,
                    ISNULL(CAST(t.history_retention_period AS nvarchar(10)) + t.history_retention_period_unit_desc, N'-') COLLATE DATABASE_DEFAULT
               FROM sys.tables t WHERE t.object_id = OBJECT_ID(N'{o}')
             UNION ALL SELECT c.name COLLATE DATABASE_DEFAULT, (c.generated_always_type_desc + CAST(c.is_hidden AS nvarchar(1))) COLLATE DATABASE_DEFAULT
               FROM sys.columns c WHERE c.object_id = OBJECT_ID(N'{o}')"
        )
    };
    let history = |s: &str, t: &str| {
        format!(
            "SELECT OBJECT_SCHEMA_NAME(history_table_id), OBJECT_NAME(history_table_id) FROM sys.tables WHERE object_id = OBJECT_ID(N'{}')",
            obj(s, t)
        )
    };
    let all = |s: &str, t: &str| {
        format!(
            "SELECT CAST(id AS nvarchar(5)), ISNULL(CAST(v AS nvarchar(20)), N'-'), CONVERT(nvarchar(30), vf, 121), CONVERT(nvarchar(30), vt, 121)
               FROM {} FOR SYSTEM_TIME ALL ORDER BY id, vf",
            obj(s, t).replace("''", "'")
        )
    };
    let want = rows(&mut *w, &temporal("ventas odd", "temporal")).await;
    assert_eq!(want[0], ["SYSTEM_VERSIONED_TEMPORAL_TABLE", "6MONTH"]);
    let report = clone_of(&endpoints, "ventas odd", "temporal", "temporal_c", true).await.unwrap_or_else(|e| panic!("clone failed: {e}"));
    println!("{report:?}");
    assert_eq!(rows(&mut *w, &temporal("ventas odd", "temporal_c")).await, want);
    assert_eq!(rows(&mut *w, &history("ventas odd", "temporal_c")).await, [["ventas odd", "temporal_c_hist"]]);
    assert_eq!(rows(&mut *w, &all("ventas odd", "temporal_c")).await, rows(&mut *w, &all("ventas odd", "temporal")).await);
    assert_eq!(rows(&mut *w, &all("ventas odd", "temporal_c")).await.len(), 3);
    // Versioned from then on.
    rows(&mut *w, "UPDATE [ventas odd].temporal_c SET v = N'a3' WHERE id = 1").await;
    assert_eq!(rows(&mut *w, "SELECT CAST(COUNT(*) AS nvarchar(5)) FROM [ventas odd].temporal_c_hist").await[0][0], "3");
    // Structure only: versioned, empty history; the history's name taken.
    clone_of(&endpoints, "ventas odd", "temporal", "temporal_t", false).await.unwrap_or_else(|e| panic!("clone failed: {e}"));
    assert_eq!(rows(&mut *w, &temporal("ventas odd", "temporal_t")).await, want);
    assert_eq!(rows(&mut *w, &history("ventas odd", "temporal_t")).await, [["ventas odd", "temporal_t_hist_2"]]);
    assert!(rows(&mut *w, &all("ventas odd", "temporal_t")).await.is_empty());
    // History named by the server: the clone's too.
    let want = rows(&mut *w, &temporal("dbo", "auto_h")).await;
    clone_of(&endpoints, "dbo", "auto_h", "auto_h_c", true).await.unwrap_or_else(|e| panic!("clone failed: {e}"));
    assert_eq!(rows(&mut *w, &temporal("dbo", "auto_h_c")).await, want);
    assert!(rows(&mut *w, &history("dbo", "auto_h_c")).await[0][1].starts_with("MSSQL_TemporalHistoryFor_"));
    // A period without versioning.
    let want = rows(&mut *w, &temporal("dbo", "periodo")).await;
    clone_of(&endpoints, "dbo", "periodo", "periodo_c", true).await.unwrap_or_else(|e| panic!("clone failed: {e}"));
    assert_eq!(rows(&mut *w, &temporal("dbo", "periodo_c")).await, want);
    assert!(want.iter().any(|r| r[1] == "AS_ROW_START0"));

    drop(w);
    rows(&mut *master, &format!("ALTER DATABASE [{DB5}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DB5}];")).await;
}

/// Where the table and its indexes are stored: a partitioned heap with a
/// unique clustered index (not the key) on the scheme and per-partition
/// compression, an aligned nonclustered index, one on another filegroup
/// and one left on PRIMARY; a clustered columnstore index on the scheme; a
/// partitioned heap with compression and LOB columns; a table on another
/// filegroup with its LOB columns on PRIMARY and a partitioned index; a
/// nonclustered key on PRIMARY on a partitioned table. The clone's
/// `sys.indexes` / `sys.partitions` / `sys.data_spaces` are the original's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live SQL Server: see the top of the file"]
async fn sql_server_partitions_and_filegroups() {
    const DB7: &str = "dbine_clone_table_layout";
    let driver: Arc<dyn Driver> = dbine_drivers::find("sqlserver").expect("sqlserver driver").clone();
    let cfg = config();
    let mut master = driver.connect(&cfg, Some("master")).await.unwrap();
    rows(&mut *master, &format!("IF DB_ID(N'{DB7}') IS NOT NULL BEGIN ALTER DATABASE [{DB7}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DB7}]; END; CREATE DATABASE [{DB7}];")).await;
    rows(
        &mut *master,
        &format!(
            "ALTER DATABASE [{DB7}] ADD FILEGROUP FG2; \
             ALTER DATABASE [{DB7}] ADD FILE (NAME = N'{DB7}_fg2', FILENAME = N'/var/opt/mssql/data/{DB7}_fg2.ndf') TO FILEGROUP FG2;"
        ),
    )
    .await;
    let endpoints = Arc::new(ConfigEndpoints { driver: driver.clone(), config: cfg.clone(), database: Some(DB7.into()) });
    let mut w = driver.connect(&cfg, Some(DB7)).await.unwrap();
    for sql in [
        "CREATE PARTITION FUNCTION pf_r3 (int) AS RANGE LEFT FOR VALUES (10, 20);",
        "CREATE PARTITION SCHEME ps_r3 AS PARTITION pf_r3 ALL TO ([PRIMARY]);",
        "CREATE TABLE dbo.cx (id int NOT NULL, v int NOT NULL) ON ps_r3(id);
         CREATE UNIQUE CLUSTERED INDEX CX ON dbo.cx (id, v) WITH (DATA_COMPRESSION = PAGE ON PARTITIONS (2)) ON ps_r3(id);
         CREATE INDEX IX_cx_al ON dbo.cx (v);
         CREATE INDEX IX_cx_fg ON dbo.cx (v) INCLUDE (id) ON FG2;
         CREATE INDEX IX_cx_pri ON dbo.cx (v DESC) ON [PRIMARY];
         INSERT dbo.cx VALUES (5, 1), (15, 2), (16, 3), (25, 4);",
        "CREATE TABLE dbo.cci (id int NOT NULL, v int NULL) ON ps_r3(id);
         CREATE CLUSTERED COLUMNSTORE INDEX CCI ON dbo.cci ON ps_r3(id);
         INSERT dbo.cci VALUES (5, 1), (15, 2), (25, 3), (26, 4);",
        "CREATE TABLE dbo.heap (id int NOT NULL, v int NULL, t nvarchar(max) NULL) ON ps_r3(id) WITH (DATA_COMPRESSION = ROW ON PARTITIONS (2));
         INSERT dbo.heap VALUES (1, 1, N'a'), (11, 2, N'b'), (21, 3, NULL);",
        "CREATE TABLE dbo.fgtab (id int NOT NULL CONSTRAINT PK_fgtab PRIMARY KEY NONCLUSTERED, t nvarchar(max) NULL) ON FG2 TEXTIMAGE_ON [PRIMARY];
         CREATE INDEX IX_fgtab_ps ON dbo.fgtab (id) ON ps_r3(id);
         INSERT dbo.fgtab VALUES (1, N'x'), (12, N'y');",
        "CREATE TABLE dbo.pkp (id int NOT NULL CONSTRAINT PK_pkp PRIMARY KEY NONCLUSTERED ON [PRIMARY], v int NULL) ON ps_r3(id);
         INSERT dbo.pkp VALUES (3, 1), (13, 2), (23, 3);",
    ] {
        rows(&mut *w, sql).await;
    }
    // Each index, name aside: its kind, keys, where it is (and on which
    // column), and each partition's compression and rows; the LOB columns'
    // place.
    let layout = |t: &str| {
        format!(
            "SELECT i.type_desc COLLATE DATABASE_DEFAULT, CAST(i.is_unique AS nvarchar(1)), CAST(i.is_primary_key AS nvarchar(1)),
                    ISNULL((SELECT c.name + CASE WHEN ic.is_descending_key = 1 THEN N' DESC' ELSE N'' END + N',' FROM sys.index_columns ic
                      JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
                     WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id AND (ic.key_ordinal > 0 OR ic.is_included_column = 1)
                     ORDER BY ic.is_included_column, ic.key_ordinal, c.name FOR XML PATH('')), N'') COLLATE DATABASE_DEFAULT,
                    ds.name COLLATE DATABASE_DEFAULT, ds.type COLLATE DATABASE_DEFAULT,
                    ISNULL((SELECT c.name FROM sys.index_columns ic JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
                     WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.partition_ordinal = 1), N'-') COLLATE DATABASE_DEFAULT,
                    (SELECT p.data_compression_desc + N':' + CAST(p.rows AS nvarchar(10)) + N',' FROM sys.partitions p
                      WHERE p.object_id = i.object_id AND p.index_id = i.index_id ORDER BY p.partition_number FOR XML PATH('')) COLLATE DATABASE_DEFAULT
               FROM sys.indexes i JOIN sys.data_spaces ds ON ds.data_space_id = i.data_space_id
              WHERE i.object_id = OBJECT_ID(N'dbo.[{t}]')
             UNION ALL SELECT N'LOB', N'', N'', N'', ISNULL(ds.name, N'-') COLLATE DATABASE_DEFAULT, ISNULL(ds.type, N'-') COLLATE DATABASE_DEFAULT, N'', N''
               FROM sys.tables t LEFT JOIN sys.data_spaces ds ON ds.data_space_id = t.lob_data_space_id
              WHERE t.object_id = OBJECT_ID(N'dbo.[{t}]')
              ORDER BY 1, 4, 5"
        )
    };
    for t in ["cx", "cci", "heap", "fgtab", "pkp"] {
        let c = format!("{t}_c");
        let report = clone_of(&endpoints, "dbo", t, &c, true).await.unwrap_or_else(|e| panic!("clone of {t} failed: {e}"));
        println!("{t}: {:?}", report.notes);
        let want = rows(&mut *w, &layout(t)).await;
        println!("{t}: {want:?}");
        assert_eq!(rows(&mut *w, &layout(&c)).await, want, "{t}");
        let sum = |x: &str| format!("SELECT CAST(COUNT(*) AS nvarchar(10)), CAST(SUM(CAST(id AS bigint)) AS nvarchar(20)) FROM dbo.[{x}]");
        assert_eq!(rows(&mut *w, &sum(&c)).await, rows(&mut *w, &sum(t)).await, "{t}");
    }
    // The cases the fix is about: 3 partitions on the scheme, not PRIMARY.
    for t in ["cx_c", "cci_c"] {
        let p = rows(
            &mut *w,
            &format!(
                "SELECT ds.name, CAST(COUNT(*) AS nvarchar(5)) FROM sys.indexes i JOIN sys.data_spaces ds ON ds.data_space_id = i.data_space_id
                   JOIN sys.partitions p ON p.object_id = i.object_id AND p.index_id = i.index_id
                  WHERE i.object_id = OBJECT_ID(N'dbo.[{t}]') AND i.index_id = 1 GROUP BY ds.name"
            ),
        )
        .await;
        assert_eq!(p, [["ps_r3", "3"]], "{t}");
    }
    // Structure only: the heap where the original's table is.
    clone_of(&endpoints, "dbo", "cx", "cx_s", false).await.unwrap_or_else(|e| panic!("clone of cx failed: {e}"));
    let heap = rows(&mut *w, "SELECT ds.name FROM sys.indexes i JOIN sys.data_spaces ds ON ds.data_space_id = i.data_space_id WHERE i.object_id = OBJECT_ID(N'dbo.cx_s') AND i.index_id IN (0, 1)").await;
    assert_eq!(heap, [["ps_r3"]]);

    drop(w);
    rows(&mut *master, &format!("ALTER DATABASE [{DB7}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DB7}];")).await;
}

/// Graph tables (AS NODE / AS EDGE) are refused up front in Spanish,
/// leaving nothing behind; a temporal table's history in a schema of its
/// own, with an index added to it: the clone's history is in that schema
/// and has that index, with its options.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live SQL Server: see the top of the file"]
async fn sql_server_graph_tables_and_history_indexes() {
    const DB6: &str = "dbine_clone_table_graph";
    let driver: Arc<dyn Driver> = dbine_drivers::find("sqlserver").expect("sqlserver driver").clone();
    let cfg = config();
    let mut master = driver.connect(&cfg, Some("master")).await.unwrap();
    rows(&mut *master, &format!("IF DB_ID(N'{DB6}') IS NOT NULL BEGIN ALTER DATABASE [{DB6}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DB6}]; END; CREATE DATABASE [{DB6}];")).await;
    let endpoints = Arc::new(ConfigEndpoints { driver: driver.clone(), config: cfg.clone(), database: Some(DB6.into()) });
    let mut w = driver.connect(&cfg, Some(DB6)).await.unwrap();
    for sql in [
        "CREATE SCHEMA [ventas odd];",
        "CREATE SCHEMA [hist ñ];",
        "CREATE TABLE [ventas odd].[nodo v4] (id int PRIMARY KEY, nombre nvarchar(20)) AS NODE;",
        "CREATE TABLE [ventas odd].[arista v4] (peso int, CONSTRAINT [EC_arista] CONNECTION ([ventas odd].[nodo v4] TO [ventas odd].[nodo v4])) AS EDGE;",
        "INSERT INTO [ventas odd].[nodo v4] (id, nombre) VALUES (1, N'a'), (2, N'b');",
        "CREATE TABLE [ventas odd].tv3 (id int NOT NULL PRIMARY KEY, code nvarchar(10) NOT NULL,
           vf datetime2(3) GENERATED ALWAYS AS ROW START HIDDEN NOT NULL, vt datetime2(3) GENERATED ALWAYS AS ROW END HIDDEN NOT NULL,
           PERIOD FOR SYSTEM_TIME (vf, vt)) WITH (SYSTEM_VERSIONING = ON (HISTORY_TABLE = [hist ñ].[tv3 hist]));",
        "CREATE INDEX [IX_tv3_hist_extra] ON [hist ñ].[tv3 hist] (code) WITH (DATA_COMPRESSION = ROW);",
        "INSERT INTO [ventas odd].tv3 (id, code) VALUES (1, N'aa'), (2, N'bb');",
        "UPDATE [ventas odd].tv3 SET code = N'a2' WHERE id = 1;",
    ] {
        rows(&mut *w, sql).await;
    }

    for (t, what) in [("nodo v4", "AS NODE"), ("arista v4", "AS EDGE")] {
        let e = clone_of(&endpoints, "ventas odd", t, &format!("{t} c"), true).await.unwrap_err();
        assert!(e.contains("no se puede clonar") && e.contains(what), "{e}");
        let left = rows(&mut *w, &format!("SELECT CAST(COUNT(*) AS nvarchar(5)) FROM sys.tables WHERE name = N'{}'", lit(&format!("{t} c")))).await;
        assert_eq!(left[0][0], "0", "{t}");
    }

    let report = clone_of(&endpoints, "ventas odd", "tv3", "tv3 c", true).await.unwrap_or_else(|e| panic!("clone failed: {e}"));
    println!("{report:?}");
    let history = rows(&mut *w, "SELECT OBJECT_SCHEMA_NAME(history_table_id), OBJECT_NAME(history_table_id) FROM sys.tables WHERE object_id = OBJECT_ID(N'[ventas odd].[tv3 c]')").await;
    assert_eq!(history, [["hist ñ", "tv3 c hist"]]);
    assert!(report.notes.iter().any(|n| n.contains("«hist ñ.tv3 c hist»")), "{:?}", report.notes);
    let indexes = |t: &str| {
        format!(
            "SELECT i.type_desc COLLATE DATABASE_DEFAULT, CAST(i.is_unique AS nvarchar(1)), p.data_compression_desc COLLATE DATABASE_DEFAULT,
                    (SELECT c.name + ',' FROM sys.index_columns ic JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
                      WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id ORDER BY ic.key_ordinal FOR XML PATH(''))
               FROM sys.indexes i JOIN sys.partitions p ON p.object_id = i.object_id AND p.index_id = i.index_id
              WHERE i.object_id = OBJECT_ID(N'[hist ñ].[{t}]') ORDER BY 1, 4"
        )
    };
    let want = rows(&mut *w, &indexes("tv3 hist")).await;
    assert_eq!(want.len(), 2);
    assert_eq!(rows(&mut *w, &indexes("tv3 c hist")).await, want);
    assert_eq!(rows(&mut *w, "SELECT CAST(COUNT(*) AS nvarchar(5)) FROM [hist ñ].[tv3 c hist]").await[0][0], "1");

    drop(w);
    rows(&mut *master, &format!("ALTER DATABASE [{DB6}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DB6}];")).await;
}
