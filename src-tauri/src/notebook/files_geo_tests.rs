//! Tests for geographic files (`st_read`) and database files
//! (`sqlite_scan`, `read_duckdb`) in results cells (`notebook::files`). The
//! pure ones always run; the `st_read` ones need DuckDB's `spatial`
//! extension (fetched once) and say so when they can't get it.

use super::*;
use crate::notebook::access::{Access, Gate, Policy};
use crate::notebook::local::{
    existing_tables, has_spatial, open_sandboxed, open_sandboxed_with, prepare, uses_spatial,
    Fetched, LiveRows, LoadedTable, LocalSource, Prepared,
};
use dbine_driver::ResultColumn;
use serde_json::{json, Value};
use std::collections::HashMap;

fn temp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbine-nbgeo-{tag}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn write(path: &Path, text: &str) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

fn plan_err(sql: &str, dir: &Path) -> String {
    match plan(sql, Some(dir), None, 100) {
        Ok(r) => panic!(
            "{sql}: planned {:?}",
            r.iter().map(|r| &r.text).collect::<Vec<_>>()
        ),
        Err(e) => e,
    }
}

/// Every file of `dir` with its size, modification time and SHA-256.
fn snapshot(dir: &Path) -> Vec<(String, u64, std::time::SystemTime, String)> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.path().is_file())
        .map(|e| {
            let m = e.metadata().unwrap();
            (
                e.file_name().to_string_lossy().to_string(),
                m.len(),
                m.modified().unwrap(),
                crate::notebook::access::hash_file(&e.path()).unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

// -- st_read: what is refused before anything runs ------------------------------------------------

#[test]
fn st_read_refuses_virtual_paths_urls_and_indirection_formats() {
    let dir = temp("refuse");
    write(&dir.join("zonas.geojson"), "{}");
    write(&dir.join("capas.vrt"), "<OGRVRTDataSource/>");
    write(&dir.join("capas.gml"), "<x/>");
    write(&dir.join("zonas.zip"), "PK");
    write(&dir.join("notas.txt"), "x");
    for (sql, want) in [
        (
            "SELECT * FROM st_read('/vsizip/datos.zip/zonas.shp')",
            "ruta virtual de GDAL",
        ),
        (
            "SELECT * FROM st_read('/vsicurl/https://example.com/z.geojson')",
            "ruta virtual de GDAL",
        ),
        (
            "SELECT * FROM st_read('/VSIS3/bucket/z.geojson')",
            "ruta virtual de GDAL",
        ),
        (
            "SELECT * FROM st_read('/vsimem/z.geojson')",
            "ruta virtual de GDAL",
        ),
        (
            "SELECT * FROM st_read('vsigzip/z.geojson')",
            "ruta virtual de GDAL",
        ),
        (
            "SELECT * FROM st_read('http://example.com/z.geojson')",
            "sin cifrar",
        ),
        (
            "SELECT * FROM st_read('s3://bucket/z.geojson')",
            "solo https://",
        ),
        (
            "SELECT * FROM st_read('WFS:http://example.com/wfs')",
            "ni una URL",
        ),
        (
            "SELECT * FROM read_csv('/vsicurl/x.csv')",
            "ruta virtual de GDAL",
        ),
        (
            "SELECT * FROM st_read('capas.vrt')",
            "apuntan a otros archivos",
        ),
        (
            "SELECT * FROM st_read('capas.gml')",
            "apuntan a otros archivos",
        ),
        ("SELECT * FROM st_read('zonas.zip')", "descomprimí"),
        ("SELECT * FROM st_read('notas.txt')", "no lee «.txt»"),
        (
            "SELECT * FROM st_read(['zonas.geojson'])",
            "un solo archivo",
        ),
        ("SELECT * FROM st_read('*.geojson')", "un solo archivo"),
    ] {
        let e = plan_err(sql, &dir);
        assert!(e.contains(want), "{sql}: {e}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn st_read_takes_only_the_parameters_it_allows() {
    let dir = temp("params");
    write(&dir.join("zonas.csv"), "wkt,name\n\"POINT (1 1)\",a\n");
    for (sql, want) in [
        ("SELECT * FROM st_read('zonas.csv', allowed_drivers = ['OGR_VRT'])", "no acepta «allowed_drivers»"),
        ("SELECT * FROM st_read('zonas.csv', sibling_files = ['/etc/passwd'])", "no acepta «sibling_files»"),
        ("SELECT * FROM st_read('zonas.csv', spatial_filter_box = x)", "no acepta «spatial_filter_box»"),
        ("SELECT * FROM st_read('zonas.csv', 'otra')", "solo opciones con nombre"),
        ("SELECT * FROM st_read('zonas.csv', open_options = ['PRELUDE_STATEMENTS=ATTACH ''/x'' AS y'])", "«PRELUDE_STATEMENTS» no se puede usar"),
        ("SELECT * FROM st_read('zonas.csv', open_options = ['FEATURE_SERVER_PAGING=YES'])", "no se puede usar"),
        ("SELECT * FROM st_read('zonas.csv', open_options = ['ENCODING=/vsicurl/http'])", "no puede ser una ruta"),
        ("SELECT * FROM st_read('zonas.csv', open_options = ['X_POSSIBLE_NAMES=C:x'])", "no puede ser una ruta"),
        ("SELECT * FROM st_read('zonas.csv', layer = lower('x'))", "tiene que ser un texto"),
        ("SELECT * FROM st_read('zonas.csv', layer = 3)", "no lleva ese valor"),
    ] {
        let e = plan_err(sql, &dir);
        assert!(e.contains(want), "{sql}: {e}");
    }
    let r = plan("SELECT * FROM st_read('zonas.csv', open_options = ['GEOM_POSSIBLE_NAMES=wkt', 'KEEP_GEOM_COLUMNS=NO'], layer := 'zonas', keep_wkb = false)", Some(&dir), None, 100).unwrap();
    let path = lit(&dir.join("zonas.csv").to_string_lossy());
    let drivers = GEO_DRIVERS
        .iter()
        .map(|d| lit(d))
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(
        r[0].text,
        format!("st_read({path}, open_options = ['GEOM_POSSIBLE_NAMES=wkt', 'KEEP_GEOM_COLUMNS=NO'], layer = 'zonas', keep_wkb = false, allowed_drivers = [{drivers}])")
    );
    assert_eq!(
        r[0].kind,
        ReadKind::Duck {
            extension: Some("spatial")
        }
    );
    assert!(!GEO_DRIVERS.iter().any(|d| matches!(
        *d,
        "OGR_VRT" | "GML" | "SQLite" | "MapInfo File" | "OSM" | "WFS" | "OAPIF"
    )));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_shapefile_brings_its_sidecars_and_they_are_part_of_its_key() {
    let dir = temp("sidecars");
    for ext in ["shp", "shx", "dbf", "prj", "CPG", "qix"] {
        write(&dir.join(format!("zonas.{ext}")), ext);
    }
    write(&dir.join("zonas_viejo.dbf"), "x");
    write(&dir.join("otra.shx"), "x");
    let r = plan("SELECT * FROM st_read('zonas.shp')", Some(&dir), None, 100).unwrap();
    let names: Vec<String> = r[0].files[0]
        .sidecars
        .iter()
        .map(|f| {
            Path::new(&f.path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    assert_eq!(
        names,
        [
            "zonas.CPG",
            "zonas.dbf",
            "zonas.prj",
            "zonas.qix",
            "zonas.shx"
        ]
    );
    assert_eq!(r[0].display, "zonas.shp");
    let paths = reader_paths(&r[0]);
    assert_eq!(paths.len(), 6);
    assert!(!paths
        .iter()
        .any(|p| p.contains("otra") || p.contains("viejo")));
    // A sidecar that changes reads the file again.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(&dir.join("zonas.dbf"), "dbf, otra vez");
    let again = plan("SELECT * FROM st_read('zonas.shp')", Some(&dir), None, 100).unwrap();
    assert_ne!(r[0].key, again[0].key);
    assert_eq!(r[0].table, again[0].table);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sidecars_are_approved_with_their_file_and_blocked_like_it() {
    let dir = temp("approve");
    let home = dir.join("home");
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    let data = dir.join("data");
    for ext in ["shp", "shx", "dbf"] {
        write(&data.join(format!("zonas.{ext}")), ext);
    }
    let r = plan("SELECT * FROM st_read('zonas.shp')", Some(&data), None, 100).unwrap();
    let f = r[0].files[0].clone();
    let all: Vec<String> = f.all().map(|x| x.path.clone()).collect();
    // Nobody allowed them: one question, the file with its sidecars.
    let Err(Gate::Ask(asked)) = Access::default().gate(&r[0].files) else {
        panic!()
    };
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].sidecars.len(), 2);
    // The file alone approved: still asked.
    let only = Access {
        approved: vec![f.path.clone()],
        ..Default::default()
    };
    assert!(matches!(only.gate(&r[0].files), Err(Gate::Ask(_))));
    let ok = Access {
        approved: all.clone(),
        ..Default::default()
    };
    assert!(ok.gate(&r[0].files).is_ok());
    // "Permitir siempre" grants each one.
    let targets = crate::notebook::access::grant_targets(&r[0].files);
    assert_eq!(
        targets.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>(),
        all
    );
    // A sidecar that is a link into a blocked folder blocks the read.
    std::fs::remove_file(data.join("zonas.dbf")).unwrap();
    write(&home.join(".ssh/zonas.dbf"), "secret");
    #[cfg(unix)]
    std::os::unix::fs::symlink(home.join(".ssh/zonas.dbf"), data.join("zonas.dbf")).unwrap();
    #[cfg(unix)]
    {
        let r = plan("SELECT * FROM st_read('zonas.shp')", Some(&data), None, 100).unwrap();
        let access = Access {
            policy: Policy::new(Some(&home), &[], &[], vec![]),
            approved: r[0].files[0].all().map(|x| x.path.clone()).collect(),
            ..Default::default()
        };
        let Err(Gate::Blocked(e)) = access.gate(&r[0].files) else {
            panic!("not blocked")
        };
        assert!(e.contains(".ssh/zonas.dbf") && e.contains("~/.ssh"), "{e}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// -- database files: what is refused --------------------------------------------------------------

#[test]
fn database_readers_check_their_table_names_and_arguments() {
    let dir = temp("dbargs");
    write(&dir.join("a.db"), "");
    write(&dir.join("b.duckdb"), "");
    for (sql, want) in [
        (
            "SELECT * FROM sqlite_scan('a.db')",
            "lleva el archivo y el nombre de la tabla",
        ),
        (
            "SELECT * FROM sqlite_scan('a.db', 't\"; DROP TABLE x; --')",
            "no es un nombre de tabla válido",
        ),
        (
            "SELECT * FROM sqlite_scan('a.db', 'main.t')",
            "no es un nombre de tabla válido",
        ),
        (
            "SELECT * FROM sqlite_scan('a.db', 't', 'u')",
            "lleva el archivo y el nombre de la tabla",
        ),
        (
            "SELECT * FROM sqlite_scan('a.db', lower('t'))",
            "lleva el archivo y el nombre de la tabla",
        ),
        (
            "SELECT * FROM sqlite_scan(['a.db'], 't')",
            "un solo archivo",
        ),
        ("SELECT * FROM read_duckdb('b.duckdb')", "necesita la tabla"),
        (
            "SELECT * FROM read_duckdb('b.duckdb', 'a.b.c')",
            "no es un nombre de tabla válido",
        ),
        (
            "SELECT * FROM read_duckdb('b.duckdb', table_name = 'x''y')",
            "no es un nombre de tabla válido",
        ),
        (
            "SELECT * FROM read_duckdb('b.duckdb', 't', hive_partitioning = true)",
            "lleva el archivo y la tabla",
        ),
        (
            "SELECT * FROM read_duckdb('https://x/b.duckdb', 't')",
            "solo archivos locales",
        ),
        (
            "SELECT * FROM sqlite_scan('s3://bucket/a.db', 't')",
            "solo archivos locales",
        ),
    ] {
        let e = plan_err(sql, &dir);
        assert!(e.contains(want), "{sql}: {e}");
    }
    let r = plan("SELECT * FROM sqlite_scan('a.db', 'Clientes 2024'), read_duckdb('b.duckdb', 'ventas.t'), read_duckdb('b.duckdb', table_name = 't')", Some(&dir), None, 100).unwrap();
    assert_eq!(
        r[0].kind,
        ReadKind::Sqlite {
            table: "Clientes 2024".into()
        }
    );
    let b = lit(&dir.join("b.duckdb").to_string_lossy());
    assert_eq!(
        r[1].text,
        format!("read_duckdb({b}, table_name = 't', schema_name = 'ventas')")
    );
    assert_eq!(r[2].text, format!("read_duckdb({b}, table_name = 't')"));
    // DuckDB looks for the file's write-ahead log: its path is granted (it isn't there).
    assert_eq!(
        reader_paths(&r[2]),
        vec![
            dir.join("b.duckdb").to_string_lossy().to_string(),
            format!("{}.wal", dir.join("b.duckdb").display())
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dbines_own_databases_are_never_read() {
    let dir = temp("own");
    let app = dir.join("dbine-data");
    write(&app.join("state.db"), "");
    write(&app.join("cache.db"), "");
    let policy = Policy::new(None, std::slice::from_ref(&app), &[], vec![]);
    for sql in [
        "SELECT * FROM sqlite_scan('dbine-data/state.db', 'connections')",
        "SELECT * FROM sqlite_scan('dbine-data/cache.db', 'x')",
        "SELECT * FROM read_duckdb('dbine-data/state.db', 'x')",
    ] {
        let r = plan(sql, Some(&dir), None, 100).unwrap();
        let access = Access {
            policy: policy.clone(),
            approved: vec![r[0].files[0].path.clone()],
            ..Default::default()
        };
        assert!(
            matches!(access.gate(&r[0].files), Err(Gate::Blocked(_))),
            "{sql}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// -- DuckDB ---------------------------------------------------------------------------------------

async fn rows_of(s: &mut dyn Session, sql: &str) -> Vec<Vec<Value>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10_000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out.results
        .into_iter()
        .find(|r| !r.columns.is_empty())
        .map(|r| r.rows)
        .unwrap_or_default()
}

async fn columns_of(s: &mut dyn Session, sql: &str) -> Vec<ResultColumn> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out.results
        .into_iter()
        .find(|r| !r.columns.is_empty())
        .map(|r| r.columns)
        .unwrap_or_default()
}

/// A DuckDB session that can write files (the test's setup, not a cell).
async fn writer() -> Box<dyn Session> {
    let mut cfg = ConnectionConfig {
        driver: "duckdb".into(),
        host: ":memory:".into(),
        ..Default::default()
    };
    cfg.options.insert("isolated".into(), "true".into());
    dbine_drivers::open_session(&cfg, None)
        .await
        .expect("DuckDB")
}

/// Runs `sql` on the setup session; `false` (and says why) when it fails.
async fn setup(w: &mut dyn Session, sql: &str) -> bool {
    let mut out = QueryOutcome::default();
    let r = w.execute(sql, 1, &mut out).await;
    if r.is_err() || out.error.is_some() {
        eprintln!("skipped: {sql}: {r:?} {:?}", out.error);
        return false;
    }
    true
}

fn never(_: usize) -> std::future::Ready<CommandResult<Fetched>> {
    std::future::ready(Err(CommandError::Internal("no".into())))
}

/// A cell's rows: sales with their coordinates.
fn ventas() -> LocalSource {
    LocalSource {
        name: "ventas".into(),
        local: false,
        sql: "SELECT * FROM ventas".into(),
        connection_id: Some("conn".into()),
        database: None,
        run: Some(1),
        rows: Some(LiveRows {
            columns: vec![
                ResultColumn {
                    name: "lon".into(),
                    type_name: "float8".into(),
                },
                ResultColumn {
                    name: "lat".into(),
                    type_name: "float8".into(),
                },
                ResultColumn {
                    name: "total".into(),
                    type_name: "int4".into(),
                },
                ResultColumn {
                    name: "cliente".into(),
                    type_name: "int4".into(),
                },
            ],
            rows: vec![
                vec![json!(1.0), json!(1.0), json!(10), json!(1)],
                vec![json!(5.0), json!(5.0), json!(5), json!(2)],
                vec![json!(2.0), json!(-3.0), json!(7), json!(1)],
            ],
        }),
        files_key: String::new(),
    }
}

/// [`prepare`] as a user who approves every file it asks about.
async fn prep(
    s: &mut dyn Session,
    tables: &mut HashMap<String, crate::notebook::local::Loaded>,
    sql: &str,
    sources: &[LocalSource],
    dir: &Path,
) -> CommandResult<(String, Vec<LoadedTable>)> {
    let present = existing_tables(s).await.unwrap();
    let mut access = Access::default();
    for _ in 0..2 {
        match prepare(
            s,
            tables,
            &present,
            sql,
            sources,
            1_000,
            Some(dir),
            &access,
            &mut QueryOutcome::default(),
            None,
            never,
        )
        .await?
        {
            Prepared::Ready(sql, loaded) => return Ok((sql, loaded)),
            Prepared::Ask(files) => {
                access.approved = files
                    .iter()
                    .flat_map(|f| f.all())
                    .map(|f| f.path.clone())
                    .collect()
            }
        }
    }
    panic!("asked twice")
}

const ZONAS: &str = "SELECT 'Norte' AS name, 'POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))'::GEOMETRY AS geom \
                     UNION ALL SELECT 'Sur', 'POLYGON((0 -10, 10 -10, 10 0, 0 0, 0 -10))'::GEOMETRY";

#[tokio::test]
async fn notebook_geo_geojson_and_shapefile_join_a_cell_table_with_st_contains() {
    let dir = temp("join");
    let d = dir.to_string_lossy().to_string();
    std::fs::create_dir_all(dir.join("geo")).unwrap();
    {
        let mut w = writer().await;
        if !setup(w.as_mut(), "INSTALL spatial; LOAD spatial").await {
            return;
        }
        assert!(
            setup(
                w.as_mut(),
                &format!(
                    "COPY ({ZONAS}) TO {} WITH (FORMAT gdal, DRIVER 'GeoJSON')",
                    lit(&format!("{d}/zonas.geojson"))
                )
            )
            .await
        );
        assert!(setup(w.as_mut(), &format!("COPY ({ZONAS}) TO {} WITH (FORMAT gdal, DRIVER 'ESRI Shapefile', SRS 'EPSG:4326')", lit(&format!("{d}/geo/zonas.shp")))).await);
    }
    assert!(dir.join("geo/zonas.dbf").exists() && dir.join("geo/zonas.shx").exists());
    let before = snapshot(&dir.join("geo"));
    assert!(before.len() >= 3, "{before:?}");
    let mut s = open_sandboxed_with(true)
        .await
        .expect("DuckDB with spatial");
    assert!(has_spatial(s.as_mut()).await);
    let mut tables = HashMap::new();
    let sources = vec![ventas()];
    for file in ["zonas.geojson", "geo/zonas.shp"] {
        let sql = format!("SELECT z.name, sum(v.total) AS total FROM st_read('{file}') z JOIN ventas v ON ST_Contains(z.geom, ST_Point(v.lon, v.lat)) GROUP BY ALL ORDER BY 1");
        assert!(uses_spatial(&sql));
        let (run, loaded) = prep(s.as_mut(), &mut tables, &sql, &sources, &dir)
            .await
            .unwrap();
        assert!(!run.contains("st_read"), "{run}");
        assert_eq!(
            rows_of(s.as_mut(), &run).await,
            vec![
                vec![json!("Norte"), json!(15)],
                vec![json!("Sur"), json!(7)]
            ],
            "{file}"
        );
        let f = &loaded.iter().find(|l| l.origin == "file").unwrap().files[0];
        assert_eq!(
            loaded.iter().find(|l| l.origin == "file").unwrap().name,
            file
        );
        if file.ends_with(".shp") {
            let names: Vec<String> = f
                .sidecars
                .iter()
                .map(|c| {
                    Path::new(&c.path)
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string()
                })
                .collect();
            assert!(
                ["zonas.dbf", "zonas.shx", "zonas.prj"]
                    .iter()
                    .all(|n| names.iter().any(|x| x == n)),
                "{names:?}"
            );
        }
        // The geometry came in as GEOMETRY: spatial functions and the map view work on it.
        let q =
            format!("SELECT name, geom, ST_AsText(geom) AS wkt FROM st_read('{file}') ORDER BY 1");
        let geo_sql = rewrite(&q, &plan(&q, Some(&dir), None, 1_000).unwrap());
        let cols = columns_of(s.as_mut(), &geo_sql).await;
        assert_eq!(cols[1].type_name, "GEOMETRY");
        let rows = rows_of(s.as_mut(), &geo_sql).await;
        assert!(
            rows[0][1].as_str().is_some_and(|h| h.starts_with("0x0103")),
            "{:?}",
            rows[0][1]
        );
        assert!(
            rows[0][2]
                .as_str()
                .is_some_and(|w| w.starts_with("POLYGON")),
            "{:?}",
            rows[0][2]
        );
    }
    // Read, not written: the shapefile and its sidecars are as they were.
    assert_eq!(snapshot(&dir.join("geo")), before);
    // The notebook's session still reads no file itself.
    let shp = lit(&format!("{d}/geo/zonas.shp"));
    let mut out = QueryOutcome::default();
    let r = s
        .execute(&format!("SELECT * FROM st_read({shp})"), 10, &mut out)
        .await;
    assert!(
        r.is_err() || out.error.is_some(),
        "the notebook's session read a file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn notebook_geo_gdal_opens_no_file_it_was_not_granted() {
    let dir = temp("vrt");
    let d = dir.to_string_lossy().to_string();
    std::fs::create_dir_all(dir.join("otro")).unwrap();
    {
        let mut w = writer().await;
        if !setup(w.as_mut(), "INSTALL spatial; LOAD spatial").await {
            return;
        }
        assert!(setup(w.as_mut(), &format!("COPY (SELECT 'secreto' AS name, 'POINT(1 1)'::GEOMETRY AS geom) TO {} WITH (FORMAT gdal, DRIVER 'GeoJSON')", lit(&format!("{d}/otro/secreto.geojson")))).await);
    }
    // A VRT (XML pointing GDAL at another file) under a GeoJSON name.
    let vrt = format!("<OGRVRTDataSource><OGRVRTLayer name=\"x\"><SrcDataSource>{d}/otro/secreto.geojson</SrcDataSource></OGRVRTLayer></OGRVRTDataSource>");
    write(&dir.join("zonas.geojson"), &vrt);
    write(&dir.join("zonas.vrt"), &vrt);
    let mut s = open_sandboxed_with(true).await.unwrap();
    let e = prep(
        s.as_mut(),
        &mut HashMap::new(),
        "SELECT * FROM st_read('zonas.geojson')",
        &[],
        &dir,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("zonas.geojson") && !e.contains("secreto"), "{e}");
    assert!(existing_tables(s.as_mut()).await.unwrap().is_empty());
    // And the VRT itself is refused before anything runs.
    let e = prep(
        s.as_mut(),
        &mut HashMap::new(),
        "SELECT * FROM st_read('zonas.vrt')",
        &[],
        &dir,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("apuntan a otros archivos"), "{e}");
    // A shapefile without its sidecars: its reader session gets the .shp alone and GDAL can't
    // find the .shx anywhere else.
    write(&dir.join("solo/zonas.shp"), "x");
    let r = plan(
        "SELECT * FROM st_read('solo/zonas.shp')",
        Some(&dir),
        None,
        10,
    )
    .unwrap();
    assert_eq!(
        reader_paths(&r[0]),
        vec![dir.join("solo/zonas.shp").to_string_lossy().to_string()]
    );
    let mut reader = open_reader(&reader_paths(&r[0]), Some("spatial"))
        .await
        .unwrap();
    let mut out = QueryOutcome::default();
    let r = reader
        .execute(
            &format!(
                "SELECT * FROM read_text({})",
                lit(&format!("{d}/otro/secreto.geojson"))
            ),
            10,
            &mut out,
        )
        .await;
    let e = r
        .err()
        .map(|e| e.to_string())
        .or(out.error)
        .unwrap_or_default();
    assert!(e.contains("disabled"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn notebook_geo_spatial_functions_need_the_extension_in_the_notebook_session() {
    assert!(uses_spatial("SELECT ST_Point(1, 2)"));
    assert!(!uses_spatial("SELECT 'ST_Point(1, 2)' -- st_x\n"));
    assert!(!uses_spatial("SELECT \"st_x\" FROM t"));
    let mut plain = open_sandboxed().await.unwrap();
    assert!(!has_spatial(plain.as_mut()).await);
    // Rebuilding a geometry needs no extension (ST_GeomFromWKB is DuckDB's own).
    assert_eq!(
        rows_of(
            plain.as_mut(),
            "SELECT ST_GeomFromWKB(unhex('0101000000000000000000F03F0000000000000040'))"
        )
        .await,
        vec![vec![json!("0x0101000000000000000000F03F0000000000000040")]]
    );
    let mut out = QueryOutcome::default();
    let r = plain.execute("LOAD spatial", 1, &mut out).await;
    assert!(
        r.is_err() || out.error.is_some(),
        "an extension loaded after the lockdown"
    );
}

#[tokio::test]
async fn notebook_geo_a_geometry_from_a_cell_round_trips() {
    // Rows as DuckDB's driver gives them for a GEOMETRY column: WKB hex.
    let src = LocalSource {
        name: "puntos".into(),
        local: false,
        sql: "SELECT * FROM puntos".into(),
        connection_id: Some("conn".into()),
        database: None,
        run: Some(1),
        rows: Some(LiveRows {
            columns: vec![
                ResultColumn {
                    name: "id".into(),
                    type_name: "INTEGER".into(),
                },
                ResultColumn {
                    name: "g".into(),
                    type_name: "GEOMETRY".into(),
                },
            ],
            rows: vec![
                vec![
                    json!(1),
                    json!("0x0101000000000000000000F03F0000000000000040"),
                ],
                vec![json!(2), Value::Null],
            ],
        }),
        files_key: String::new(),
    };
    let mut s = open_sandboxed().await.unwrap();
    let dir = temp("roundtrip");
    let (run, loaded) = prep(
        s.as_mut(),
        &mut HashMap::new(),
        "SELECT id, g FROM puntos ORDER BY id",
        std::slice::from_ref(&src),
        &dir,
    )
    .await
    .unwrap();
    assert!(!loaded[0].as_text);
    assert_eq!(columns_of(s.as_mut(), &run).await[1].type_name, "GEOMETRY");
    assert_eq!(
        rows_of(s.as_mut(), &run).await,
        vec![
            vec![
                json!(1),
                json!("0x0101000000000000000000F03F0000000000000040")
            ],
            vec![json!(2), Value::Null]
        ]
    );
    // Something that isn't WKB stays text.
    let mut bad = src.clone();
    bad.name = "malos".into();
    bad.rows.as_mut().unwrap().rows[0][1] = json!("0x01FF0000000000000000");
    let (run, loaded) = prep(
        s.as_mut(),
        &mut HashMap::new(),
        "SELECT g FROM malos",
        &[bad],
        &dir,
    )
    .await
    .unwrap();
    assert!(loaded[0].as_text, "{loaded:?}");
    assert_eq!(columns_of(s.as_mut(), &run).await[0].type_name, "VARCHAR");
    let _ = std::fs::remove_dir_all(&dir);
}

// -- SQLite and DuckDB files ------------------------------------------------------------------------

#[tokio::test]
async fn notebook_db_sqlite_files_are_read_without_touching_them() {
    let dir = temp("sqlite");
    {
        let c = rusqlite::Connection::open(dir.join("crm.db")).unwrap();
        c.execute_batch("CREATE TABLE clientes(id INTEGER, nombre TEXT, saldo REAL, foto BLOB); INSERT INTO clientes VALUES (1, 'Ana', 1.5, x'CAFE'), (2, 'Luis', NULL, NULL);").unwrap();
        // A database in WAL mode, closed: no -wal next to it.
        let c = rusqlite::Connection::open(dir.join("wal.db")).unwrap();
        c.execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE t(id INTEGER); INSERT INTO t VALUES (5);",
        )
        .unwrap();
    }
    let before = snapshot(&dir);
    assert_eq!(
        before.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(),
        ["crm.db", "wal.db"]
    );
    let mut s = open_sandboxed().await.unwrap();
    let mut tables = HashMap::new();
    let sql = "SELECT c.nombre, c.saldo, c.foto, sum(v.total) AS total FROM sqlite_scan('crm.db', 'clientes') c JOIN ventas v ON v.cliente = c.id GROUP BY ALL ORDER BY 1";
    let (run, loaded) = prep(s.as_mut(), &mut tables, sql, &[ventas()], &dir)
        .await
        .unwrap();
    assert_eq!(
        rows_of(s.as_mut(), &run).await,
        vec![
            vec![json!("Ana"), json!(1.5), json!("0xCAFE"), json!(17)],
            vec![json!("Luis"), Value::Null, Value::Null, json!(5)]
        ]
    );
    let f = loaded.iter().find(|l| l.origin == "file").unwrap();
    assert_eq!(
        (f.name.as_str(), f.rows, f.files[0].sidecars.len()),
        ("crm.db", 2, 0)
    );
    let (run, _) = prep(
        s.as_mut(),
        &mut tables,
        "SELECT * FROM sqlite_scan('wal.db', 't')",
        &[],
        &dir,
    )
    .await
    .unwrap();
    assert_eq!(rows_of(s.as_mut(), &run).await, vec![vec![json!(5)]]);
    // Nothing was created or changed next to them (no -wal, -shm or -journal).
    assert_eq!(snapshot(&dir), before);
    // A missing table says so.
    let e = prep(
        s.as_mut(),
        &mut tables,
        "SELECT * FROM sqlite_scan('crm.db', 'nada')",
        &[],
        &dir,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("«nada»") && e.contains("no such table"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn notebook_db_a_sqlite_file_with_a_pending_wal_is_read_from_a_copy() {
    let dir = temp("sqlitewal");
    // Open in another program, with rows only in its -wal.
    let other = rusqlite::Connection::open(dir.join("vivo.db")).unwrap();
    other.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE t(id INTEGER); INSERT INTO t VALUES (7);").unwrap();
    assert!(std::fs::metadata(dir.join("vivo.db-wal")).unwrap().len() > 0);
    let before = snapshot(&dir);
    let r = plan(
        "SELECT * FROM sqlite_scan('vivo.db', 't')",
        Some(&dir),
        None,
        100,
    )
    .unwrap();
    assert_eq!(
        r[0].files[0]
            .sidecars
            .iter()
            .map(|f| f.display.as_str())
            .collect::<Vec<_>>(),
        ["vivo.db-wal"]
    );
    let mut s = open_sandboxed().await.unwrap();
    let (run, _) = prep(
        s.as_mut(),
        &mut HashMap::new(),
        "SELECT * FROM sqlite_scan('vivo.db', 't')",
        &[],
        &dir,
    )
    .await
    .unwrap();
    assert_eq!(rows_of(s.as_mut(), &run).await, vec![vec![json!(7)]]);
    assert_eq!(
        snapshot(&dir),
        before,
        "the database, its -wal or its -shm changed"
    );
    drop(other);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn notebook_db_duckdb_files_are_read_read_only() {
    let dir = temp("duckfile");
    let db = dir.join("ventas.duckdb");
    {
        let mut w = writer().await;
        assert!(
            setup(
                w.as_mut(),
                &format!(
                    "ATTACH {} AS dd; CREATE TABLE dd.clientes AS SELECT * FROM (VALUES (1, 'Ana'), (2, 'Luis')) v(id, nombre); \
                     CREATE SCHEMA dd.arch; CREATE TABLE dd.arch.zonas AS SELECT * FROM (VALUES (1, 'Norte'), (2, 'Sur')) v(cliente, zona); DETACH dd",
                    lit(&db.to_string_lossy())
                )
            )
            .await
        );
    }
    let before = snapshot(&dir);
    assert_eq!(before.len(), 1, "{before:?}");
    let mut s = open_sandboxed().await.unwrap();
    let sql = "SELECT c.nombre, z.zona, sum(v.total) AS total FROM read_duckdb('ventas.duckdb', 'clientes') c \
               JOIN read_duckdb('ventas.duckdb', table_name = 'zonas', schema_name = 'arch') z ON z.cliente = c.id \
               JOIN ventas v ON v.cliente = c.id GROUP BY ALL ORDER BY 1";
    let (run, loaded) = prep(s.as_mut(), &mut HashMap::new(), sql, &[ventas()], &dir)
        .await
        .unwrap();
    assert_eq!(
        rows_of(s.as_mut(), &run).await,
        vec![
            vec![json!("Ana"), json!("Norte"), json!(17)],
            vec![json!("Luis"), json!("Sur"), json!(5)]
        ]
    );
    assert_eq!(loaded.iter().filter(|l| l.origin == "file").count(), 2);
    // Unchanged, and no .wal left next to it.
    assert_eq!(snapshot(&dir), before);
    // The notebook's session never attached it.
    let dbs = rows_of(
        s.as_mut(),
        "SELECT count(*) FROM duckdb_databases() WHERE NOT internal AND path IS NOT NULL",
    )
    .await;
    assert_eq!(dbs, vec![vec![json!(0)]]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A GeoPackage is a SQLite file: one with a SQLite virtual table over
/// another file (GDAL's `VirtualOGR`) doesn't read that file.
#[tokio::test]
async fn notebook_geo_a_geopackage_cannot_point_gdal_at_another_file() {
    let dir = temp("gpkg");
    let d = dir.to_string_lossy().to_string();
    std::fs::create_dir_all(dir.join("otro")).unwrap();
    {
        let mut w = writer().await;
        if !setup(w.as_mut(), "INSTALL spatial; LOAD spatial").await {
            return;
        }
        assert!(setup(w.as_mut(), &format!("COPY (SELECT 'secreto' AS name, 'POINT(1 1)'::GEOMETRY AS geom) TO {} WITH (FORMAT gdal, DRIVER 'GeoJSON')", lit(&format!("{d}/otro/secreto.geojson")))).await);
        assert!(setup(w.as_mut(), &format!("COPY (SELECT 'z' AS name, 'POINT(1 1)'::GEOMETRY AS geom) TO {} WITH (FORMAT gdal, DRIVER 'GPKG')", lit(&format!("{d}/z.gpkg")))).await);
    }
    {
        let c = rusqlite::Connection::open(dir.join("z.gpkg")).unwrap();
        c.execute_batch(&format!(
            "PRAGMA writable_schema=ON; INSERT INTO sqlite_master(type, name, tbl_name, rootpage, sql) \
             VALUES ('table', 'x', 'x', 0, 'CREATE VIRTUAL TABLE x USING VirtualOGR(''{d}/otro/secreto.geojson'')'); PRAGMA writable_schema=OFF; \
             INSERT INTO gpkg_contents(table_name, data_type, identifier) VALUES ('x', 'attributes', 'x');"
        ))
        .unwrap();
    }
    let before = snapshot(&dir);
    let mut s = open_sandboxed_with(true).await.unwrap();
    let (run, _) = prep(
        s.as_mut(),
        &mut HashMap::new(),
        "SELECT name FROM st_read('z.gpkg')",
        &[],
        &dir,
    )
    .await
    .unwrap();
    assert_eq!(rows_of(s.as_mut(), &run).await, vec![vec![json!("z")]]);
    for sql in [
        "SELECT * FROM st_read('z.gpkg', layer = 'x')",
        "SELECT * FROM st_read('z.gpkg', open_options = ['LIST_ALL_TABLES=YES'], layer = 'x')",
    ] {
        let got = match prep(s.as_mut(), &mut HashMap::new(), sql, &[], &dir).await {
            Ok((run, _)) => format!("{:?}", rows_of(s.as_mut(), &run).await),
            Err(e) => e.to_string(),
        };
        assert!(!got.contains("secreto"), "{sql}: {got}");
    }
    assert_eq!(snapshot(&dir), before);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A GeoPackage open in another program (WAL mode, changes only in its
/// `-wal`): the `-wal` is a sidecar, the file is read from a private copy
/// with it applied, and the originals stay byte- and time-identical.
#[tokio::test]
async fn notebook_geo_a_geopackage_with_a_pending_wal_is_read_from_a_copy() {
    let dir = temp("gpkgwal");
    let d = dir.to_string_lossy().to_string();
    {
        let mut w = writer().await;
        if !setup(w.as_mut(), "INSTALL spatial; LOAD spatial").await {
            return;
        }
        assert!(setup(w.as_mut(), &format!("COPY (SELECT 'z' AS name, 'POINT(1 1)'::GEOMETRY AS geom) TO {} WITH (FORMAT gdal, DRIVER 'GPKG', LAYER_NAME 'z')", lit(&format!("{d}/z.gpkg")))).await);
    }
    // Another program: WAL mode, no checkpoints, a new layer only in the -wal.
    let other = rusqlite::Connection::open(dir.join("z.gpkg")).unwrap();
    other
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
             CREATE TABLE extra(fid INTEGER PRIMARY KEY, name TEXT); INSERT INTO extra(name) VALUES ('desde el wal');
             INSERT INTO gpkg_contents(table_name, data_type, identifier) VALUES ('extra', 'attributes', 'extra');",
        )
        .unwrap();
    assert!(std::fs::metadata(dir.join("z.gpkg-wal")).unwrap().len() > 0);
    let before = snapshot(&dir);
    let r = plan(
        "SELECT * FROM st_read('z.gpkg', layer = 'extra')",
        Some(&dir),
        None,
        100,
    )
    .unwrap();
    assert_eq!(
        r[0].files[0]
            .sidecars
            .iter()
            .map(|f| f.display.as_str())
            .collect::<Vec<_>>(),
        ["z.gpkg-wal"]
    );
    assert!(r[0].gpkg_copy);
    let mut s = open_sandboxed_with(true).await.unwrap();
    let mut tables = HashMap::new();
    let (run, loaded) = prep(
        s.as_mut(),
        &mut tables,
        "SELECT name FROM st_read('z.gpkg', layer = 'extra')",
        &[],
        &dir,
    )
    .await
    .unwrap();
    assert_eq!(
        rows_of(s.as_mut(), &run).await,
        vec![vec![json!("desde el wal")]]
    );
    assert_eq!(loaded[0].files[0].sidecars.len(), 1);
    let (run, _) = prep(
        s.as_mut(),
        &mut tables,
        "SELECT name FROM st_read('z.gpkg')",
        &[],
        &dir,
    )
    .await
    .unwrap();
    assert_eq!(rows_of(s.as_mut(), &run).await, vec![vec![json!("z")]]);
    assert_eq!(
        snapshot(&dir),
        before,
        "the GeoPackage, its -wal or its -shm changed"
    );
    // Approving the file without its -wal isn't enough.
    let wal = r[0].files[0].sidecars[0].path.clone();
    let access = Access {
        approved: vec![r[0].files[0].path.clone()],
        ..Default::default()
    };
    assert!(matches!(access.gate(&r[0].files), Err(Gate::Ask(_))));
    let access = Access {
        approved: vec![r[0].files[0].path.clone(), wal],
        ..Default::default()
    };
    assert!(access.gate(&r[0].files).is_ok());
    drop(other);
    let _ = std::fs::remove_dir_all(&dir);
}
