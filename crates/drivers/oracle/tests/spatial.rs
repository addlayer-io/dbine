//! SDO_GEOMETRY for the results' map against a real server (it needs
//! Spatial / Locator: the `full` image, not `slim`): a table's data selects
//! its geometries as EWKT, and a free query with an SDO_GEOMETRY column (a
//! join with ORDER BY, duplicate names) runs again converted. Skipped
//! without the variable:
//!
//! ```sh
//! docker run -d --name dbine-test-oracle-spatial -p 25602:1521 -e ORACLE_PASSWORD=Secret123 \
//!   -e APP_USER=dbine -e APP_USER_PASSWORD=Dbine123 gvenzl/oracle-free:23-full
//! DBINE_TEST_ORACLE_SPATIAL_URL=oracle://dbine:Dbine123@localhost:25602/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test spatial -- --ignored
//! ```

use dbine_driver::{ColumnFilter, ConnectionConfig, FilterOp, ObjectRef, QueryOutcome, Session, SortKey, StatementResult};
use serde_json::{json, Value};

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_ORACLE_SPATIAL_URL").ok()?;
    let rest = url.strip_prefix("oracle://")?;
    let (cred, addr) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (hostport, service) = addr.split_once('/')?;
    let (host, port) = hostport.split_once(':')?;
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    Some(cfg)
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    if let Some(e) = out.error.take() {
        panic!("{sql}: {e}");
    }
    out
}

fn result(out: &QueryOutcome) -> &StatementResult {
    out.results.iter().find(|r| !r.columns.is_empty()).expect("a result set")
}

fn column(r: &StatementResult, name: &str) -> Vec<Value> {
    let i = r.columns.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("column {name}"));
    r.rows.iter().map(|row| row[i].clone()).collect()
}

const POINT: &str = "SRID=4326;POINT (-58.3816 -34.6037)";
const POLYGON: &str = "SRID=4326;POLYGON ((-59.0 -35.0, -58.0 -35.0, -58.0 -34.0, -59.0 -34.0, -59.0 -35.0))";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn oracle_geometries_reach_the_map() {
    let Some(cfg) = cfg() else { return };
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.unwrap();
    for t in ["DBINE_GEO_MAP", "DBINE_GEO_LBL"] {
        run(&mut s, &format!("DROP TABLE IF EXISTS {t}")).await;
    }
    run(&mut s, "CREATE TABLE DBINE_GEO_MAP (ID NUMBER PRIMARY KEY, NAME VARCHAR2(20), \"Geom\" SDO_GEOMETRY)").await;
    run(&mut s, "CREATE TABLE DBINE_GEO_LBL (ID NUMBER PRIMARY KEY, LABEL VARCHAR2(20))").await;
    run(
        &mut s,
        "INSERT INTO DBINE_GEO_MAP VALUES (1, 'point', SDO_GEOMETRY(2001, 4326, SDO_POINT_TYPE(-58.3816, -34.6037, NULL), NULL, NULL))",
    )
    .await;
    // 8307: Oracle's own WGS 84, sent as 4326.
    run(
        &mut s,
        "INSERT INTO DBINE_GEO_MAP VALUES (2, 'polygon', SDO_GEOMETRY(2003, 8307, NULL, SDO_ELEM_INFO_ARRAY(1, 1003, 1), \
         SDO_ORDINATE_ARRAY(-59, -35, -58, -35, -58, -34, -59, -34, -59, -35)))",
    )
    .await;
    run(&mut s, "INSERT INTO DBINE_GEO_MAP VALUES (3, 'null', NULL)").await;
    run(&mut s, "INSERT INTO DBINE_GEO_MAP VALUES (4, 'no srid', SDO_GEOMETRY(2001, NULL, SDO_POINT_TYPE(10, 20, NULL), NULL, NULL))").await;
    // A 6,000-vertex line: its WKT is past the 64 KB other text is cut at.
    run(
        &mut s,
        "DECLARE o SDO_ORDINATE_ARRAY := SDO_ORDINATE_ARRAY(); BEGIN
           FOR i IN 0 .. 5999 LOOP o.EXTEND(2); o(2 * i + 1) := -60 + i / 1000; o(2 * i + 2) := -30 - i / 4000; END LOOP;
           INSERT INTO DBINE_GEO_MAP VALUES (5, 'line', SDO_GEOMETRY(2002, 4326, NULL, SDO_ELEM_INFO_ARRAY(1, 2, 1), o));
         END;",
    )
    .await;
    run(&mut s, "INSERT INTO DBINE_GEO_LBL VALUES (1, 'uno'), (2, 'dos'), (3, 'tres'), (4, 'cuatro'), (5, 'cinco')").await;

    // Table data: the browse query converts the geometry column, sorted
    // and filtered as the grid does.
    let obj = ObjectRef { kind: "table".into(), schema: None, name: "DBINE_GEO_MAP".into() };
    let browse = s.browse_query(&obj, 100);
    assert!(browse.contains("SDO_UTIL.TO_WKTGEOMETRY(t.\"Geom\") AS \"Geom\""), "{browse}");
    let sorted = d.sorted_browse(&browse, &[SortKey { column: "ID".into(), descending: false }]).unwrap();
    let out = run(&mut s, &sorted).await;
    let r = result(&out);
    let types: Vec<(&str, &str)> = r.columns.iter().map(|c| (c.name.as_str(), c.type_name.as_str())).collect();
    assert_eq!(types[2], ("Geom", "SDO_GEOMETRY"), "{types:?}");
    let geoms = column(r, "Geom");
    assert_eq!(geoms[..4], [json!(POINT), json!(POLYGON), Value::Null, json!("POINT (10.0 20.0)")]);
    let line = geoms[4].as_str().unwrap();
    assert!(line.len() > 64 * 1024, "{}", line.len());
    assert!(line.starts_with("SRID=4326;LINESTRING (-60.0 -30.0, -59.999 -30.00025, ") && line.ends_with(')'), "{}", &line[..80]);
    assert_eq!(line.matches(',').count(), 5999);
    let filter = [ColumnFilter { column: "ID".into(), op: FilterOp::Eq, values: vec![json!(2)], sql: None }];
    let filtered = d.filtered_browse(&browse, &filter).unwrap();
    assert_eq!(column(result(&run(&mut s, &filtered).await), "Geom"), [json!(POLYGON)]);
    // A table without geometries keeps SELECT *.
    let plain = s.browse_query(&ObjectRef { kind: "table".into(), schema: None, name: "DBINE_GEO_LBL".into() }, 10);
    assert!(plain.starts_with("SELECT *\nFROM"), "{plain}");

    // A free query with the raw column: run again converted, the join's
    // ORDER BY kept, both ID columns kept.
    let out = run(
        &mut s,
        "SELECT g.ID, l.ID, l.LABEL, g.\"Geom\" FROM DBINE_GEO_MAP g JOIN DBINE_GEO_LBL l ON l.ID = g.ID WHERE g.ID < 5 ORDER BY g.ID DESC",
    )
    .await;
    let r = result(&out);
    let names: Vec<&str> = r.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["ID", "ID", "LABEL", "Geom"]);
    assert_eq!(r.columns[3].type_name, "SDO_GEOMETRY");
    assert_eq!(column(r, "LABEL"), [json!("cuatro"), json!("tres"), json!("dos"), json!("uno")]);
    assert_eq!(column(r, "Geom"), [json!("POINT (10.0 20.0)"), Value::Null, json!(POLYGON), json!(POINT)]);
    assert!(out.log.iter().any(|m| m.text.contains("EWKT")) || out.messages.iter().any(|m| m.contains("EWKT")), "{:?} {:?}", out.log, out.messages);

    // SELECT * and a script: each statement on its own.
    let out = run(&mut s, "SELECT * FROM DBINE_GEO_MAP WHERE ID = 1;\nSELECT COUNT(*) N FROM DBINE_GEO_MAP;").await;
    let sets: Vec<&StatementResult> = out.results.iter().filter(|r| !r.columns.is_empty()).collect();
    assert_eq!(column(sets[0], "Geom"), [json!(POINT)]);
    assert_eq!(column(sets[1], "N"), [json!(5)]);

    // An object type that isn't a geometry: the client's error, with the hint.
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT SDO_POINT_TYPE(1, 2, 3) P FROM dual", 10, &mut out).await;
    assert!(e.is_err() || out.error.is_some());
    assert!(out.log.iter().any(|m| m.text.contains("SDO_UTIL.TO_WKTGEOMETRY")) || out.messages.iter().any(|m| m.contains("SDO_UTIL.TO_WKTGEOMETRY")), "{:?} {:?}", out.log, out.messages);
    // The connection is still good.
    assert_eq!(column(result(&run(&mut s, "SELECT 1 X FROM dual").await), "X"), [json!(1)]);

    for t in ["DBINE_GEO_MAP", "DBINE_GEO_LBL"] {
        run(&mut s, &format!("DROP TABLE {t}")).await;
    }
}
