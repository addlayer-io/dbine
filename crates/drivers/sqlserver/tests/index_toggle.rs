//! "Deshabilitar / Habilitar índice" against a real server. Reads
//! `DBINE_TEST_SQLSERVER_URL` like `integration.rs`.

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session};

fn parse_url(url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap();
}

async fn disabled(s: &mut Box<dyn Session>, t: &ObjectRef, name: &str) -> bool {
    let r = s.index_usage(t).await.unwrap().unwrap();
    r.indexes.iter().find(|i| i.name == name).unwrap().disabled
}

#[tokio::test]
#[ignore]
async fn disable_and_enable() {
    let Ok(url) = std::env::var("DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let cfg = parse_url(&url);
    let d = dbine_driver_sqlserver::drivers().remove(0);
    assert!(d.supports_index_toggle());
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    run(
        &mut admin,
        "IF DB_ID('dbine_toggle') IS NOT NULL BEGIN ALTER DATABASE dbine_toggle SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_toggle; END
         GO
         CREATE DATABASE dbine_toggle",
    )
    .await;
    let mut s = d.connect(&cfg, Some("dbine_toggle")).await.unwrap();
    run(&mut s, "CREATE TABLE dbo.Pedidos (id int CONSTRAINT PK_Pedidos PRIMARY KEY, fecha date)\nGO\nCREATE INDEX IX_Fecha ON dbo.Pedidos (fecha)").await;
    let t = ObjectRef { kind: kinds::TABLE.into(), schema: Some("dbo".into()), name: "Pedidos".into() };
    assert!(!disabled(&mut s, &t, "IX_Fecha").await);

    let ix = s.index_usage(&t).await.unwrap().unwrap().indexes.into_iter().find(|i| i.name == "IX_Fecha").unwrap();
    for st in d.index_toggle_script(&t, &ix, false).unwrap().statements {
        run(&mut s, &st).await;
    }
    assert!(disabled(&mut s, &t, "IX_Fecha").await);
    // The table still reads: only the clustered index takes it offline.
    run(&mut s, "SELECT COUNT(*) FROM dbo.Pedidos WHERE fecha > '2026-01-01'").await;

    for st in d.index_toggle_script(&t, &ix, true).unwrap().statements {
        run(&mut s, &st).await;
    }
    assert!(!disabled(&mut s, &t, "IX_Fecha").await);

    drop(s);
    run(&mut admin, "ALTER DATABASE dbine_toggle SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_toggle").await;
}
