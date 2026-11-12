//! The monitor per preset against canned result sets: which query feeds
//! which figure, and that a failing or empty server never breaks it.

use super::*;
use crate::presets::PRESETS;
use dbine_driver::Error;

/// Answers the first query containing a needle; any other fails.
struct Fake {
    answers: Vec<(&'static str, Option<Set>)>,
    seen: Vec<String>,
}

impl Fake {
    fn new(answers: Vec<(&'static str, Option<Set>)>) -> Fake {
        Fake {
            answers,
            seen: Vec::new(),
        }
    }
}

impl Source for Fake {
    fn query(&mut self, sql: &str) -> Result<Set> {
        self.seen.push(sql.to_string());
        match self.answers.iter().find(|(needle, _)| sql.contains(needle)) {
            Some((_, Some(set))) => Ok(set.clone()),
            Some((_, None)) => Err(Error::Query("permiso denegado".into())),
            None => Err(Error::Query("objeto inexistente".into())),
        }
    }
}

/// Every query answers an empty set.
struct Empty;

impl Source for Empty {
    fn query(&mut self, _sql: &str) -> Result<Set> {
        Ok(Set::default())
    }
}

/// Every query answers one row whose columns are the words of the query,
/// each with the value 1: every figure read by column name gets a value.
struct Echo;

impl Source for Echo {
    fn query(&mut self, sql: &str) -> Result<Set> {
        let mut cols: Vec<&str> = sql
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .filter(|w| !w.is_empty())
            .collect();
        cols.sort_unstable();
        cols.dedup();
        let row = vec![Some("1"); cols.len()];
        Ok(Set::new(&cols, vec![row]))
    }
}

/// Every query fails.
struct Down;

impl Source for Down {
    fn query(&mut self, _sql: &str) -> Result<Set> {
        Err(Error::Query("sin permiso".into()))
    }
}

fn set(cols: &[&str], rows: &[&[&str]]) -> Option<Set> {
    Some(Set::new(
        cols,
        rows.iter()
            .map(|r| r.iter().map(|c| (!c.is_empty()).then_some(*c)).collect())
            .collect(),
    ))
}

fn preset(id: &str) -> &'static Preset {
    PRESETS.iter().find(|p| p.id == id).unwrap()
}

fn metric(s: &MonitorSnapshot, key: &str) -> Option<f64> {
    s.metrics
        .iter()
        .find(|m| m.key == key)
        .and_then(|m| m.value)
}

fn table<'a>(s: &'a MonitorSnapshot, key: &str) -> &'a MonitorTable {
    s.tables
        .iter()
        .find(|t| t.key == key)
        .unwrap_or_else(|| panic!("no table {key}"))
}

fn ctx(dbms: &str) -> Ctx {
    Ctx {
        dbms: dbms.into(),
        version: format!("{dbms} 1.0"),
        database: "app".into(),
        file: None,
    }
}

#[test]
fn every_preset_survives_a_failing_or_empty_server() {
    for p in PRESETS {
        let mut down = Down;
        let snap = collect(p, &ctx("x"), &mut down);
        assert!(snap.metrics.iter().all(|m| m.value.is_none()), "{}", p.id);
        let mut empty = Empty;
        let _ = collect(p, &ctx("x"), &mut empty);
    }
}

/// Every figure of every preset says what it measures and where it comes
/// from, whatever the server answered; and a key appears once.
#[test]
fn every_metric_has_help_and_source() {
    // The branches that depend on the database (Ingres' imadb, HeavyDB's
    // information_schema) or on a file (Access, dBase) too.
    struct Gone(std::path::PathBuf);
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let dir = std::env::temp_dir().join(format!("dbine-monitor-help-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let _gone = Gone(dir.clone());
    let file = dir.join("T.dbf");
    std::fs::write(&file, vec![0u8; 10]).unwrap();
    let mut seen = 0;
    for p in PRESETS {
        for dbms in ["x", "Microsoft SQL Server", "Informix", "DB2/LINUXX8664"] {
            let contexts = [
                ctx(dbms),
                Ctx {
                    database: "imadb".into(),
                    ..ctx(dbms)
                },
                Ctx {
                    database: "information_schema".into(),
                    ..ctx(dbms)
                },
                Ctx {
                    file: Some(file.to_string_lossy().into_owned()),
                    ..ctx(dbms)
                },
            ];
            let snaps: Vec<MonitorSnapshot> = contexts
                .iter()
                .flat_map(|c| {
                    [
                        collect(p, c, &mut Down),
                        collect(p, c, &mut Empty),
                        collect(p, c, &mut Echo),
                    ]
                })
                .collect();
            for snap in &snaps {
                let mut keys: Vec<&str> = snap.metrics.iter().map(|m| m.key.as_str()).collect();
                keys.sort_unstable();
                keys.dedup();
                assert_eq!(keys.len(), snap.metrics.len(), "{}: duplicate keys", p.id);
                for m in &snap.metrics {
                    seen += 1;
                    assert!(
                        m.help.as_deref().is_some_and(|h| !h.trim().is_empty()),
                        "{}: {} has no help",
                        p.id,
                        m.key
                    );
                    assert!(
                        m.source.as_deref().is_some_and(|s| !s.trim().is_empty()),
                        "{}: {} has no source",
                        p.id,
                        m.key
                    );
                }
            }
        }
    }
    assert!(seen > 500, "{seen}");
}

#[test]
fn monitor_capability_per_preset() {
    for p in PRESETS {
        let expected = !matches!(p.id, "spark" | "kyuubi" | "zen" | "netsuite" | "mimer");
        assert_eq!(supports(p), expected, "{}", p.id);
        assert_eq!(crate::design::capabilities(p).monitor, expected, "{}", p.id);
    }
}

#[test]
fn cells_keep_numbers_and_cut_long_text() {
    assert_eq!(cell(Some("42")), Value::from(42));
    assert_eq!(cell(Some("0.5")), serde_json::json!(0.5));
    assert_eq!(cell(Some("007")), Value::from("007"));
    assert_eq!(cell(Some("  ")), Value::Null);
    assert_eq!(cell(Some("1e3x")), Value::from("1e3x"));
    let long = "x".repeat(MAX_TEXT + 50);
    assert_eq!(
        cell(Some(&long)).as_str().unwrap().chars().count(),
        MAX_TEXT + 1
    );
    assert_eq!(parse("1,024"), Some(1024.0));
    assert_eq!(hit_ratio(Some(5.0), Some(100.0)), Some(95.0));
    assert_eq!(hit_ratio(Some(5.0), Some(0.0)), None);
}

#[test]
fn generic_unknown_engine_reports_only_info() {
    let mut f = Fake::new(vec![]);
    let s = collect(preset("odbc"), &ctx("PostgreSQL"), &mut f);
    assert!(f.seen.is_empty());
    assert!(s.metrics.is_empty());
    assert!(s
        .info
        .iter()
        .any(|(l, v)| l == "Motor" && v == "PostgreSQL"));
    assert!(s.notes[0].contains("PostgreSQL"));
}

#[test]
fn generic_routes_by_dbms_name() {
    assert_eq!(eng_from_dbms("Microsoft SQL Server"), Some(Eng::SqlServer));
    assert_eq!(eng_from_dbms("DB2/LINUXX8664"), Some(Eng::Db2));
    assert_eq!(eng_from_dbms("DSN12015"), Some(Eng::Db2zos));
    assert_eq!(eng_from_dbms("Adaptive Server Enterprise"), Some(Eng::Ase));
    assert_eq!(eng_from_dbms("Informix"), Some(Eng::Informix));
    assert_eq!(eng_from_dbms("Spark SQL"), Some(Eng::Spark));
    assert_eq!(eng_from_dbms("MonetDB"), Some(Eng::MonetDb));
    assert_eq!(eng_from_dbms("PostgreSQL"), None);
}

#[test]
fn sql_server_through_the_generic_preset() {
    let mut f = Fake::new(vec![
        (
            "dm_os_sys_info",
            set(
                &[
                    "cpu_count",
                    "physical_memory_kb",
                    "committed_kb",
                    "committed_target_kb",
                    "uptime_s",
                ],
                &[&["8", "16777216", "1048576", "2097152", "3600"]],
            ),
        ),
        (
            "RING_BUFFER",
            set(&["sql_cpu", "total_cpu"], &[&["12", "30"]]),
        ),
        (
            "dm_os_performance_counters",
            set(
                &["name", "value"],
                &[
                    &["Batch Requests/sec", "1000"],
                    &["User Connections", "7"],
                    &["Buffer cache hit ratio", "99"],
                    &["Buffer cache hit ratio base", "100"],
                    &["Database pages", "10"],
                ],
            ),
        ),
        (
            "dm_exec_sessions",
            set(
                &[
                    "session_id",
                    "login_name",
                    "db",
                    "host_name",
                    "status",
                    "secs",
                    "sql_text",
                ],
                &[
                    &["51", "sa", "master", "mac", "running", "0", "SELECT 1"],
                    &["52", "app", "db", "pc", "sleeping", "10", ""],
                ],
            ),
        ),
    ]);
    let s = collect(preset("odbc"), &ctx("Microsoft SQL Server"), &mut f);
    assert_eq!(metric(&s, "uptime"), Some(3600.0));
    assert_eq!(metric(&s, "cpu"), Some(30.0));
    assert_eq!(metric(&s, "mem_used"), Some(1048576.0 * 1024.0));
    assert_eq!(
        s.metrics.iter().find(|m| m.key == "mem_used").unwrap().max,
        Some(2097152.0 * 1024.0)
    );
    assert_eq!(metric(&s, "connections"), Some(7.0));
    assert_eq!(metric(&s, "active_sessions"), Some(1.0));
    assert_eq!(metric(&s, "cache_hit"), Some(99.0));
    assert_eq!(metric(&s, "mem_cache"), Some(81920.0));
    assert!(
        s.metrics
            .iter()
            .find(|m| m.key == "queries")
            .unwrap()
            .counter
    );
    assert_eq!(table(&s, "sessions").rows.len(), 2);
}

#[test]
fn db2_luw_sums_members_and_builds_waits() {
    let mut f = Fake::new(vec![
        (
            "ENV_GET_SYSTEM_RESOURCES",
            set(
                &[
                    "HOST_NAME",
                    "CPU_USAGE_TOTAL",
                    "MEMORY_TOTAL",
                    "OS_NAME",
                    "CPU_ONLINE",
                ],
                &[&["db2srv", "40", "8192", "Linux", "4"]],
            ),
        ),
        (
            "MON_GET_MEMORY_POOL",
            set(&["USED_KB", "BP_KB"], &[&["2048", "1024"]]),
        ),
        (
            "SELECT * FROM TABLE(MON_GET_DATABASE",
            set(
                &[
                    "APPLS_CUR_CONS",
                    "TOTAL_CPU_TIME",
                    "POOL_DATA_L_READS",
                    "POOL_DATA_P_READS",
                    "POOL_INDEX_L_READS",
                    "POOL_INDEX_P_READS",
                    "LOCK_WAIT_TIME",
                    "TOTAL_APP_COMMITS",
                ],
                &[
                    &["3", "2000000", "900", "9", "100", "1", "15", "5"],
                    &["2", "1000000", "", "", "", "", "5", "5"],
                ],
            ),
        ),
        ("MON_GET_HADR", set(&["HADR_ROLE"], &[])),
        ("TIMESTAMPDIFF", set(&["UP"], &[&["86400"]])),
        (
            "MON_GET_TABLESPACE",
            set(
                &["TBSP_NAME", "TBSP_TYPE", "USED", "TOTAL"],
                &[&["USERSPACE1", "DMS", "100", "400"]],
            ),
        ),
    ]);
    let s = collect(preset("db2"), &ctx("DB2/LINUXX8664"), &mut f);
    assert_eq!(metric(&s, "cpu"), Some(40.0));
    assert_eq!(metric(&s, "connections"), Some(5.0));
    // 3 s of CPU over the 4 CPUs online: 75, so 100 is all of them busy.
    assert_eq!(metric(&s, "cpu_time"), Some(75.0));
    let cpu = s.metrics.iter().find(|m| m.key == "cpu_time").unwrap();
    assert!(cpu.counter && cpu.max == Some(100.0));
    assert_eq!(metric(&s, "cache_hit"), Some(99.0));
    assert_eq!(metric(&s, "mem_used"), Some(2048.0 * 1024.0));
    assert_eq!(
        s.metrics.iter().find(|m| m.key == "mem_used").unwrap().max,
        Some(8192.0 * 1024.0 * 1024.0)
    );
    assert_eq!(metric(&s, "uptime"), Some(86400.0));
    assert_eq!(metric(&s, "storage_used"), Some(100.0));
    let waits = table(&s, "waits");
    assert_eq!(
        waits.rows[0],
        vec![Value::from("Bloqueos"), serde_json::json!(20.0)]
    );
    // No HADR: no replication table.
    assert!(!s.tables.iter().any(|t| t.key == "replication"));
    assert!(
        s.notes.iter().any(|n| n.starts_with("Sesiones")),
        "{:?}",
        s.notes
    );
}

#[test]
fn sybase_without_mda_notes_it() {
    let mut f = Fake::new(vec![
        (
            "@@servername",
            set(
                &["srv", "maxc", "pg", "busy", "tt", "rd", "wr"],
                &[&["ASE1", "1024", "4096", "1000", "100000", "5", "6"]],
            ),
        ),
        (
            "syscurconfigs",
            set(
                &["name", "value"],
                &[
                    &["total logical memory", "51200"],
                    &["max memory", "102400"],
                    &["number of user connections", "50"],
                ],
            ),
        ),
        (
            "COUNT(*) AS n",
            set(&["n", "act", "blk"], &[&["10", "2", "1"]]),
        ),
        ("tempdb", set(&["up"], &[&["7200"]])),
        ("sysengines", set(&["engines"], &[&["4"]])),
        ("monState", None),
        (
            "curunreservedpgs",
            set(
                &["name", "total", "free"],
                &[&["master", "1000", "400"], &["pubs2", "2000", "600"]],
            ),
        ),
    ]);
    let s = collect(preset("sybase"), &ctx("Adaptive Server Enterprise"), &mut f);
    // 100 s of CPU over 4 engines: 2500, so 100 is all of them busy.
    assert_eq!(metric(&s, "cpu_time"), Some(2500.0));
    assert_eq!(metric(&s, "mem_used"), Some(51200.0 * 2048.0));
    assert_eq!(metric(&s, "connections"), Some(10.0));
    assert_eq!(
        s.metrics
            .iter()
            .find(|m| m.key == "connections")
            .unwrap()
            .max,
        Some(50.0)
    );
    assert_eq!(metric(&s, "locks_waiting"), Some(1.0));
    assert_eq!(metric(&s, "uptime"), Some(7200.0));
    assert_eq!(metric(&s, "storage_used"), Some(2000.0));
    assert!(s.notes.iter().any(|n| n.contains("mon_role")));
}

#[test]
fn informix_profile_counters() {
    let mut f = Fake::new(vec![
        ("sysshmvals", set(&["up"], &[&["500"]])),
        (
            "sysprofile",
            set(
                &["name", "value"],
                &[
                    &["dskreads", "10"],
                    &["bufreads", "1000"],
                    &["commits", "7"],
                    &["rollbacks", "3"],
                    &["deadlks", "1"],
                ],
            ),
        ),
        (
            "syssessions",
            set(
                &["sid", "username", "hostname", "secs"],
                &[&["1", "informix", "h", "60"]],
            ),
        ),
    ]);
    let s = collect(preset("informix"), &ctx("Informix"), &mut f);
    assert_eq!(metric(&s, "cache_hit"), Some(99.0));
    assert_eq!(metric(&s, "transactions"), Some(10.0));
    assert_eq!(metric(&s, "deadlocks"), Some(1.0));
    assert_eq!(metric(&s, "connections"), Some(1.0));
    assert_eq!(metric(&s, "uptime"), Some(500.0));
}

#[test]
fn teradata_falls_back_to_session_info() {
    let mut f = Fake::new(vec![
        (
            "DBCInfoV",
            set(&["InfoKey", "InfoData"], &[&["VERSION", "17.20"]]),
        ),
        ("MonitorSession", None),
        (
            "SessionInfoV",
            set(
                &["SessionNo", "UserName", "DefaultDataBase", "LogonSource"],
                &[&["1", "dbc", "dbc", "x"], &["2", "app", "app", "y"]],
            ),
        ),
        (
            "SUM(CurrentPerm) AS used",
            set(&["used", "mx"], &[&["10", "100"]]),
        ),
    ]);
    let s = collect(preset("teradata"), &ctx("Teradata"), &mut f);
    assert_eq!(metric(&s, "connections"), Some(2.0));
    assert_eq!(metric(&s, "storage_used"), Some(10.0));
    assert!(s.info.iter().any(|(l, v)| l == "VERSION" && v == "17.20"));
    assert!(s.notes.iter().any(|n| n.contains("MONITOR SESSION")));
    assert_eq!(table(&s, "sessions").columns[0], "ID");
}

#[test]
fn exasol_without_dba_uses_own_sessions() {
    let mut f = Fake::new(vec![
        (
            "EXA_MONITOR_LAST_DAY",
            set(
                &["MEASURE_TIME", "CPU", "LOAD", "TEMP_DB_RAM"],
                &[&["t", "35.5", "1.2", "10"]],
            ),
        ),
        ("EXA_DBA_SESSIONS", None),
        (
            "EXA_ALL_SESSIONS",
            set(
                &["SESSION_ID", "USER_NAME", "STATUS"],
                &[&["1", "SYS", "EXECUTE SQL"], &["2", "APP", "IDLE"]],
            ),
        ),
    ]);
    let s = collect(preset("exasol"), &ctx("EXASolution"), &mut f);
    assert_eq!(metric(&s, "cpu"), Some(35.5));
    assert_eq!(metric(&s, "connections"), Some(2.0));
    assert_eq!(metric(&s, "active_sessions"), Some(1.0));
    assert_eq!(
        table(&s, "sessions").columns,
        vec!["ID", "Usuario", "Estado"]
    );
}

/// "Memoria usada" is the RAM in use, against the database's RAM; the
/// compressed size of the data is a figure of its own.
#[test]
fn exasol_ram_in_use_is_not_the_data_size() {
    let mut f = Fake::new(vec![
        (
            "EXA_MONITOR_LAST_DAY",
            set(
                &["MEASURE_TIME", "TEMP_DB_RAM", "PERSISTENT_DB_RAM"],
                &[&["t", "1024", "3072"]],
            ),
        ),
        (
            "EXA_DB_SIZE_LAST_DAY",
            set(&["STORAGE_SIZE", "MEM_OBJECT_SIZE"], &[&["2000", "1000"]]),
        ),
        (
            "EXA_SYSTEM_EVENTS ORDER BY",
            set(&["DB_RAM_SIZE", "NODES"], &[&["256", "4"]]),
        ),
    ]);
    let s = collect(preset("exasol"), &ctx("EXASolution"), &mut f);
    let mem = s.metrics.iter().find(|m| m.key == "mem_used").unwrap();
    assert_eq!(mem.value, Some(4.0 * GB));
    assert_eq!(mem.max, Some(256.0 * GB));
    let data = s.metrics.iter().find(|m| m.key == "data_mem_size").unwrap();
    assert_eq!(data.value, Some(1000.0 * GB));
    assert_eq!(data.group, "Almacenamiento");
}

#[test]
fn vertica_memory_from_host_resources() {
    let mut f = Fake::new(vec![(
        "host_resources",
        set(
            &[
                "host_name",
                "total_memory_bytes",
                "total_memory_free_bytes",
                "disk_space_used_mb",
                "disk_space_total_mb",
            ],
            &[
                &["n1", "1000", "400", "1", "2"],
                &["n2", "1000", "600", "1", "2"],
            ],
        ),
    )]);
    let s = collect(preset("vertica"), &ctx("Vertica Database"), &mut f);
    assert_eq!(metric(&s, "mem_used"), Some(1000.0));
    assert_eq!(metric(&s, "storage_used"), Some(2.0 * MB));
}

/// The OS's buffers and file cache aren't memory in use; the session
/// ceiling is per node.
#[test]
fn vertica_leaves_the_os_cache_out_and_scales_the_session_ceiling() {
    let mut f = Fake::new(vec![
        (
            "host_resources",
            set(
                &[
                    "total_memory_bytes",
                    "total_memory_free_bytes",
                    "total_buffer_memory_bytes",
                    "total_memory_cache_bytes",
                ],
                &[&["1000", "100", "50", "600"], &["1000", "100", "50", "600"]],
            ),
        ),
        ("MaxClientSessions", set(&["v"], &[&["50"]])),
        (
            "v_monitor.sessions",
            set(&["session_id", "state"], &[&["1", "activa"]]),
        ),
    ]);
    let s = collect(preset("vertica"), &ctx("Vertica Database"), &mut f);
    assert_eq!(metric(&s, "mem_used"), Some(500.0));
    assert_eq!(metric(&s, "mem_cache"), Some(1300.0));
    let conns = s.metrics.iter().find(|m| m.key == "connections").unwrap();
    assert_eq!(conns.max, Some(100.0));
}

#[test]
fn openedge_reads_hyphenated_vst_columns() {
    let mut f = Fake::new(vec![
        (
            "_ActSummary",
            set(
                &[
                    "_Summary-Uptime",
                    "_Summary-Commits",
                    "_Summary-Undos",
                    "_Summary-DbReads",
                    "_Summary-DbAccesses",
                ],
                &[&["100", "8", "2", "5", "500"]],
            ),
        ),
        (
            "_DbStatus",
            set(
                &[
                    "_DbStatus-DbBlkSize",
                    "_DbStatus-HiWater",
                    "_DbStatus-TotalBlks",
                ],
                &[&["8192", "10", "20"]],
            ),
        ),
        (
            "_Connect",
            set(
                &["_Connect-Usr", "_Connect-Name", "_Connect-Wait"],
                &[&["5", "app", " -- "], &["6", "batch", "REC"]],
            ),
        ),
    ]);
    let s = collect(preset("openedge"), &ctx("OpenEdge"), &mut f);
    assert_eq!(metric(&s, "uptime"), Some(100.0));
    assert_eq!(metric(&s, "transactions"), Some(10.0));
    assert_eq!(metric(&s, "cache_hit"), Some(99.0));
    assert_eq!(metric(&s, "storage_used"), Some(81920.0));
    assert_eq!(metric(&s, "connections"), Some(2.0));
    assert_eq!(metric(&s, "locks_waiting"), Some(1.0));
}

#[test]
fn heavydb_sorts_tables_by_size() {
    let mut f = Fake::new(vec![
        (
            "SHOW TABLE DETAILS",
            set(
                &[
                    "table_name",
                    "max_rows",
                    "total_data_file_size",
                    "total_metadata_file_size",
                ],
                &[&["small", "1", "10", "1"], &["big", "2", "1000", "5"]],
            ),
        ),
        (
            "SHOW QUERIES",
            set(
                &["query_session_id", "current_status"],
                &[&["a", "RUNNING"], &["b", "PENDING_EXECUTOR"]],
            ),
        ),
    ]);
    let s = collect(preset("heavydb"), &ctx("HeavyDB"), &mut f);
    assert_eq!(metric(&s, "storage_used"), Some(1016.0));
    assert_eq!(metric(&s, "active_sessions"), Some(1.0));
    assert_eq!(table(&s, "top_objects").rows[0][0], Value::from("big"));
    assert!(s.notes.iter().any(|n| n.contains("superusuario")));
    assert!(s.notes.iter().any(|n| n.contains("information_schema")));
}

#[test]
fn dbase_folder_sizes() {
    let dir = std::env::temp_dir().join(format!("dbine-dbase-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("CLIENTES.DBF"), vec![0u8; 300]).unwrap();
    std::fs::write(dir.join("CLIENTES.MDX"), vec![0u8; 50]).unwrap();
    std::fs::write(dir.join("VENTAS.dbf"), vec![0u8; 100]).unwrap();
    std::fs::write(dir.join("leeme.txt"), vec![0u8; 999]).unwrap();
    let c = Ctx {
        file: Some(dir.join("VENTAS.dbf").to_string_lossy().into_owned()),
        ..ctx("")
    };
    let s = collect(preset("dbase"), &c, &mut Down);
    assert_eq!(metric(&s, "storage_used"), Some(450.0));
    let t = table(&s, "top_objects");
    assert_eq!(
        t.rows[0],
        vec![
            Value::from("CLIENTES"),
            serde_json::json!(300.0),
            serde_json::json!(50.0)
        ]
    );
    assert_eq!(t.rows.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);

    let file = std::env::temp_dir().join(format!("dbine-access-{}.accdb", std::process::id()));
    std::fs::write(&file, vec![0u8; 4096]).unwrap();
    let c = Ctx {
        file: Some(file.to_string_lossy().into_owned()),
        ..ctx("ACCESS")
    };
    let s = collect(preset("access"), &c, &mut Down);
    assert_eq!(metric(&s, "storage_used"), Some(4096.0));
    assert!(s
        .info
        .iter()
        .any(|(l, v)| l == "Modificado" && v.ends_with("UTC")));
    let _ = std::fs::remove_file(&file);
}

#[test]
fn utc_dates_without_a_date_crate() {
    let t: chrono_lite::Time =
        (std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_706_708_700)).into();
    assert_eq!(t.0, "2024-01-31 13:45:00 UTC");
}
