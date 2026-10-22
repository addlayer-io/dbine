//! Spatial values for the results' map view, against real servers: a
//! geometry column comes as `0x` + MySQL's internal format (SRID + WKB),
//! whole even past the 1 KiB other binary is cut at. Each test reads
//! `DBINE_TEST_<ENGINE>_URL` and is skipped without it:
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//!   cargo test -p dbine-driver-mysql --test spatial -- --ignored --nocapture
//! ```
//!
//! `--nocapture` prints the cells (`SPATIAL <engine> <column> <value>`) for
//! the UI's parser check (web/src/composables/geo.test.mjs has the vectors).

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};

fn cfg(env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "mysql".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
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

/// A 240-vertex ring around Buenos Aires, as WKT coordinates (lon lat).
fn ring() -> String {
    let pts: Vec<String> = (0..=240)
        .map(|i| {
            let a = (i % 240) as f64 / 240.0 * std::f64::consts::TAU;
            format!("{:.6} {:.6}", -58.38 + 0.5 * a.cos(), -34.6 + 0.4 * a.sin())
        })
        .collect();
    pts.join(",")
}

async fn exercise(engine: &str, cfg: ConnectionConfig, axis: &str) {
    let d = dbine_driver_mysql::drivers()
        .into_iter()
        .find(|d| d.info().id == engine)
        .unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, "CREATE DATABASE IF NOT EXISTS dbine_spatial").await;
    run(&mut s, "DROP TABLE IF EXISTS dbine_spatial.zonas").await;
    run(&mut s, "CREATE TABLE dbine_spatial.zonas (id int PRIMARY KEY, nombre varchar(40), lugar POINT, zona GEOMETRY)").await;
    run(
        &mut s,
        &format!(
            "INSERT INTO dbine_spatial.zonas VALUES (1, 'Obelisco', ST_GeomFromText('POINT(-58.3816 -34.6037)', 4326{axis}), \
             ST_GeomFromText('POLYGON(({}))', 4326{axis}))",
            ring()
        ),
    )
    .await;
    let out = run(
        &mut s,
        "SELECT id, nombre, lugar, zona FROM dbine_spatial.zonas",
    )
    .await;
    let r = &out.results[0];
    assert_eq!(
        r.columns[2].type_name.to_ascii_lowercase(),
        "geometry",
        "{:?}",
        r.columns
    );
    for (i, name) in [(2, "lugar"), (3, "zona")] {
        let v = r.rows[0][i].as_str().unwrap();
        println!("SPATIAL {engine} {name} {v}");
        assert!(
            v.starts_with("0xE6100000"),
            "SRID 4326 prefix: {}",
            &v[..20.min(v.len())]
        );
        assert!(!v.ends_with('…'), "whole");
    }
    // SRID (4) + byte order (1) + type (4) + rings (4) + points (4) + 241 × 16
    assert_eq!(
        r.rows[0][3].as_str().unwrap().len(),
        2 + 2 * (4 + 1 + 4 + 4 + 4 + 241 * 16)
    );
    run(&mut s, "DROP DATABASE dbine_spatial").await;
}

#[tokio::test]
#[ignore]
async fn mysql_spatial_values_come_whole() {
    let Some(cfg) = cfg("DBINE_TEST_MYSQL_URL") else {
        return;
    };
    exercise("mysql", cfg, ", 'axis-order=long-lat'").await;
}

#[tokio::test]
#[ignore]
async fn mariadb_spatial_values_come_whole() {
    let Some(cfg) = cfg("DBINE_TEST_MARIADB_URL") else {
        return;
    };
    exercise("mariadb", cfg, "").await;
}
