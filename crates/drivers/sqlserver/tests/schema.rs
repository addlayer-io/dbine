//! Structure, DDL round trip and database create/drop against a real
//! server. Reads `DBINE_TEST_SQLSERVER_URL` like `integration.rs`:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:26010' \
//!   cargo test -p dbine-driver-sqlserver --test schema -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, DdlParts, ObjectRef, QueryOutcome, Session};
use serde_json::json;

fn parse_url(url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port,
        username: Some(user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{e}\n---\n{sql}"));
}

const SOURCE: &str = "
CREATE SCHEMA ventas
GO
CREATE TABLE ventas.clientes (
    id int IDENTITY(1,1) CONSTRAINT PK_clientes PRIMARY KEY,
    email nvarchar(120) NOT NULL,
    nombre nvarchar(50) NOT NULL DEFAULT N'sin nombre',
    alta datetime2(3) NOT NULL DEFAULT SYSDATETIME()
);
CREATE TABLE ventas.pedidos (
    id bigint IDENTITY(1,1) CONSTRAINT PK_pedidos PRIMARY KEY,
    cliente_id int NOT NULL CONSTRAINT FK_pedidos_clientes REFERENCES ventas.clientes (id) ON DELETE CASCADE,
    estado varchar(20) NULL,
    total decimal(18,2) NOT NULL DEFAULT 0,
    doble AS (total * 2)
);
CREATE TABLE ventas.items (
    pedido_id bigint NOT NULL,
    linea int NOT NULL,
    cliente_id int NULL,
    producto nvarchar(max) NULL,
    CONSTRAINT PK_items PRIMARY KEY (pedido_id, linea),
    CONSTRAINT FK_items_pedidos FOREIGN KEY (pedido_id) REFERENCES ventas.pedidos (id) ON DELETE CASCADE,
    CONSTRAINT FK_items_clientes FOREIGN KEY (cliente_id) REFERENCES ventas.clientes (id)
);
CREATE UNIQUE INDEX UX_clientes_email ON ventas.clientes (email);
CREATE INDEX IX_pedidos_estado ON ventas.pedidos (estado, total) WHERE estado IS NOT NULL;
EXEC sys.sp_addextendedproperty N'MS_Description', N'Clientes de la tienda', N'SCHEMA', N'ventas', N'TABLE', N'clientes';
EXEC sys.sp_addextendedproperty N'MS_Description', N'Correo único', N'SCHEMA', N'ventas', N'TABLE', N'clientes', N'COLUMN', N'email';
";

#[tokio::test]
#[ignore]
async fn schema_round_trip() {
    let Ok(url) = std::env::var("DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let cfg = parse_url(&url);
    let d = dbine_driver_sqlserver::drivers().remove(0);
    assert!(d.capabilities().create_database && d.capabilities().foreign_keys);
    let mut admin = d.connect(&cfg, Some("master")).await.expect("connect");
    for db in ["dbine_ddl_src", "dbine_ddl_dst"] {
        let _ = admin.drop_database(db).await;
        admin.create_database(db).await.expect("create_database");
    }
    assert!(admin.list_databases().await.unwrap().contains(&"dbine_ddl_src".to_string()));

    let mut src = d.connect(&cfg, Some("dbine_ddl_src")).await.unwrap();
    run(&mut src, SOURCE).await;
    let schema = src.database_schema().await.unwrap();
    let names: Vec<&str> = schema.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["clientes", "items", "pedidos"]);
    let clientes = &schema[0];
    assert_eq!(clientes.comment.as_deref(), Some("Clientes de la tienda"));
    assert_eq!(clientes.columns[1].comment.as_deref(), Some("Correo único"));
    assert!(clientes.columns[0].auto_increment);
    assert_eq!(clientes.columns[1].data_type, "nvarchar(120)");
    assert_eq!(clientes.primary_key.as_ref().unwrap().name.as_deref(), Some("PK_clientes"));
    assert!(clientes.indexes.iter().any(|i| i.name == "UX_clientes_email" && i.unique));
    let items = &schema[1];
    assert_eq!(items.primary_key.as_ref().unwrap().columns, ["pedido_id", "linea"]);
    assert_eq!(items.foreign_keys.len(), 2);
    let fk = items.foreign_keys.iter().find(|f| f.name.as_deref() == Some("FK_items_pedidos")).unwrap();
    assert_eq!((fk.ref_table.as_str(), fk.on_delete.as_deref()), ("pedidos", Some("CASCADE")));
    let pedidos = &schema[2];
    let ix = pedidos.indexes.iter().find(|i| i.name == "IX_pedidos_estado").unwrap();
    assert_eq!(ix.columns, ["estado", "total"]);
    assert_eq!(ix.filter.as_deref(), Some("[estado] IS NOT NULL"));
    assert_eq!(pedidos.columns[4].data_type, "AS ([total]*(2))");

    // Round trip: every CREATE, then indexes and foreign keys, into a fresh database.
    let mut script = vec!["CREATE SCHEMA ventas".to_string()];
    for t in &schema {
        script.push(d.table_ddl(t, DdlParts { create: true, if_exists: true, ..Default::default() }).unwrap());
    }
    for t in &schema {
        script.push(d.table_ddl(t, DdlParts { indexes: true, foreign_keys: true, ..Default::default() }).unwrap());
    }
    let script = script.join(&format!("\n{}\n", d.script_separator()));
    eprintln!("{script}");
    let mut dst = d.connect(&cfg, Some("dbine_ddl_dst")).await.unwrap();
    run(&mut dst, &script).await;
    let copy = dst.database_schema().await.unwrap();
    assert_eq!(copy, schema);

    // Data through insert_script, and the FK cascade works on the copy.
    let target = |n: &str| ObjectRef { kind: "table".into(), schema: Some("ventas".into()), name: n.into() };
    let ins = d
        .insert_script(&target("clientes"), &["email".into(), "nombre".into()], &[vec![json!("a@x.com"), json!("Año O'Brien")], vec![json!("b@x.com"), json!("Beto")]])
        .unwrap();
    run(&mut dst, &ins).await;
    let ins = d.insert_script(&target("pedidos"), &["cliente_id".into(), "estado".into(), "total".into()], &[vec![json!(1), json!("nuevo"), json!(10.5)]]).unwrap();
    run(&mut dst, &ins).await;
    let mut out = QueryOutcome::default();
    dst.execute("SELECT nombre FROM ventas.clientes WHERE id = 1", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], json!("Año O'Brien"));

    // The session's own database can't be dropped from it.
    assert!(dst.drop_database("dbine_ddl_dst").await.is_err());
    drop(src);
    // The destination session is still open: the drop closes it.
    for db in ["dbine_ddl_src", "dbine_ddl_dst"] {
        admin.drop_database(db).await.expect("drop_database");
    }
    assert!(!admin.list_databases().await.unwrap().contains(&"dbine_ddl_dst".to_string()));
}
