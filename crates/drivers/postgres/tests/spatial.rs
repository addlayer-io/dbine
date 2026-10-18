//! PostGIS values for the results' map view, against a real server: the
//! driver reads text, so geometry and geography come as PostGIS's hex EWKB
//! (no `0x`), whole. Skipped without `DBINE_TEST_POSTGIS_URL` (a server
//! with the postgis extension available, e.g. the postgis/postgis image):
//!
//! ```sh
//! DBINE_TEST_POSTGIS_URL=postgres://postgres:pw@localhost:25040/postgres \
//!   cargo test -p dbine-driver-postgres --test spatial -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_POSTGIS_URL").ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "postgres".into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    out
}

#[tokio::test]
#[ignore]
async fn postgis_values_come_as_whole_hex_ewkb() {
    let Some(cfg) = cfg() else { return };
    let d = dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == "postgres").unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, "CREATE EXTENSION IF NOT EXISTS postgis").await;
    run(&mut s, "DROP TABLE IF EXISTS dbine_zonas").await;
    run(&mut s, "CREATE TABLE dbine_zonas (id int, nombre text, lugar geography(Point, 4326), zona geometry(Polygon, 4326), web geometry(Point, 3857))").await;
    run(
        &mut s,
        "INSERT INTO dbine_zonas VALUES (1, 'Obelisco', 'SRID=4326;POINT(-58.3816 -34.6037)', \
         ST_Buffer('SRID=4326;POINT(-58.38 -34.6)'::geometry, 0.5, 60), \
         ST_Transform('SRID=4326;POINT(-58.3816 -34.6037)'::geometry, 3857))",
    )
    .await;
    let out = run(&mut s, "SELECT id, nombre, lugar, zona, web FROM dbine_zonas").await;
    let r = &out.results[0];
    for (i, name) in [(2, "lugar"), (3, "zona"), (4, "web")] {
        let v = r.rows[0][i].as_str().unwrap();
        println!("SPATIAL postgis {name} {v}");
        assert!(v.starts_with("0101000020E6100000") || v.starts_with("0103000020E6100000") || v.starts_with("0101000020110F0000"), "{name}: {}", &v[..18]);
    }
    // 241 points of 16 bytes: well past 1 KiB, whole.
    assert!(r.rows[0][3].as_str().unwrap().len() > 2 * 241 * 16);
    run(&mut s, "DROP TABLE dbine_zonas").await;
}
