//! Spatial values for the results' map view, against a real server: a
//! geometry / geography column is typed by its name and comes as `0x` +
//! SQL Server's own serialization, whole even past the 1 KiB other binary
//! is cut at. Skipped without `DBINE_TEST_SQLSERVER_URL`:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
//!   cargo test -p dbine-driver-sqlserver --test spatial -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_SQLSERVER_URL").ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    out
}

/// A 240-vertex ring around Buenos Aires, counter-clockwise (geography's
/// exterior ring orientation), as WKT coordinates (lon lat).
fn ring() -> String {
    let pts: Vec<String> = (0..=240)
        .map(|i| {
            let a = (i % 240) as f64 / 240.0 * std::f64::consts::TAU;
            format!("{:.6} {:.6}", -58.38 + 0.5 * a.cos(), -34.6 + 0.4 * a.sin())
        })
        .collect();
    pts.join(",")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn geometry_and_geography_come_whole_and_typed() {
    let Some(cfg) = cfg() else { return };
    let d = dbine_driver_sqlserver::drivers()
        .into_iter()
        .find(|d| d.info().id == "sqlserver")
        .unwrap();
    let mut s = d.connect(&cfg, Some("master")).await.unwrap();
    run(
        &mut s,
        "IF OBJECT_ID('tempdb..#zonas') IS NOT NULL DROP TABLE #zonas",
    )
    .await;
    run(&mut s, "CREATE TABLE #zonas (id int, nombre nvarchar(40), lugar geography, zona geometry, area geography, h hierarchyid)").await;
    run(
        &mut s,
        &format!(
            "INSERT INTO #zonas VALUES (1, N'Obelisco', geography::Point(-34.6037, -58.3816, 4326), \
             geometry::STGeomFromText('POLYGON(({r}))', 4326), geography::STGeomFromText('POLYGON(({r}))', 4326), \
             hierarchyid::Parse('/1/2/'))",
            r = ring()
        ),
    )
    .await;
    let out = run(
        &mut s,
        "SELECT id, nombre, lugar, zona, area, h FROM #zonas",
    )
    .await;
    let r = &out.results[0];
    let types: Vec<&str> = r.columns.iter().map(|c| c.type_name.as_str()).collect();
    assert_eq!(
        &types[2..],
        ["geography", "geometry", "geography", "hierarchyid"]
    );
    for (i, name) in [(2, "lugar"), (3, "zona"), (4, "area")] {
        let v = r.rows[0][i].as_str().unwrap();
        println!("SPATIAL sqlserver {name} {v}");
        assert!(v.starts_with("0xE6100000"), "SRID 4326: {}", &v[..20]);
        assert!(!v.ends_with('…'), "whole");
    }
    // SRID, version, flags, point count, 241 points, then figures and shapes.
    assert!(r.rows[0][3].as_str().unwrap().len() > 2 + 2 * 241 * 16);
}
