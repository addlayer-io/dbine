//! Unit tests of the scheduler designer: every script generator (create,
//! each change, delete), the escaping, and what the forms refuse.

use super::*;
use serde_json::json;

fn fv(values: Value) -> FormValues {
    FormValues {
        values: values
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        lists: BTreeMap::new(),
    }
}

fn with_props(mut v: FormValues, pairs: &[(&str, &str)]) -> FormValues {
    v.lists.insert(
        "properties".into(),
        pairs
            .iter()
            .map(|(k, x)| {
                BTreeMap::from([
                    ("key".to_string(), json!(k)),
                    ("value".to_string(), json!(x)),
                ])
            })
            .collect(),
    );
    v
}

fn err(r: Result<impl std::fmt::Debug>) -> String {
    match r {
        Err(Error::Query(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Shared pieces
// ---------------------------------------------------------------------------

#[test]
fn scripts_end_each_statement_and_frame_compound_ones() {
    let p = Plan {
        head: vec!["Crea algo\nDROP TABLE x".into()],
        steps: vec![
            "SELECT 1".into(),
            "CREATE EVENT e DO BEGIN SELECT 1; SELECT 2; END".into(),
        ],
        finally: vec!["SET x = 1".into()],
        ..Plan::default()
    };
    assert_eq!(
        p.script(),
        "-- Crea algo DROP TABLE x\nSELECT 1;\nDELIMITER $$\nCREATE EVENT e DO BEGIN SELECT 1; SELECT 2; END$$\nDELIMITER ;\nSET x = 1;\n"
    );
    // A delimiter the statement doesn't hold.
    let mut out = String::new();
    push_statement(&mut out, "DO BEGIN SELECT '$$'; END");
    assert!(out.starts_with("DELIMITER //\n"), "{out}");
    let n = Plan::nothing("db.ev", "id".into());
    assert!(n.empty && n.script().contains("no hay cambios"));
}

#[test]
fn times_from_the_form_read_as_the_engines_take_them() {
    assert_eq!(
        parse_datetime("2030-01-01T00:00:00").as_deref(),
        Some("2030-01-01 00:00:00")
    );
    assert_eq!(
        parse_datetime("2030-01-01 08:30").as_deref(),
        Some("2030-01-01 08:30:00")
    );
    assert_eq!(
        parse_datetime("2030-01-01T08:30:15.250Z").as_deref(),
        Some("2030-01-01 08:30:15")
    );
    for bad in [
        "",
        "2030-13-01T00:00:00",
        "2030-02-30T00:00:00",
        "30-01-01",
        "2030-01-01T25:00:00",
        "2030-01-01'; DROP TABLE t; --",
        "+2030-01-01",
    ] {
        assert!(parse_datetime(bad).is_none(), "{bad}");
    }
    assert_eq!(
        form_time(Some("2030-01-01 00:00:00")),
        json!("2030-01-01T00:00:00")
    );
    assert_eq!(form_time(None), json!(""));
}

#[test]
fn values_come_back_checked() {
    let raw = json!({"n": "12", "f": 1.5, "b": "true", "c": "HOUR", "x": "nope", "s": ["a"]});
    let m: BTreeMap<String, Value> = raw.as_object().unwrap().clone().into_iter().collect();
    let v = Vals(&m);
    assert_eq!(v.int("n", "N", 1, 20).unwrap(), Some(12));
    assert!(v
        .int("n", "N", 1, 5)
        .unwrap_err()
        .to_string()
        .contains("entre 1 y 5"));
    assert!(v.int("f", "F", 0, 9).is_err());
    assert!(v.bool("b", false));
    assert!(v.bool("missing", true));
    assert_eq!(v.choice("c", "C", &["HOUR", "DAY"], "DAY").unwrap(), "HOUR");
    assert_eq!(
        v.choice("missing", "C", &["HOUR", "DAY"], "DAY").unwrap(),
        "DAY"
    );
    assert!(v.choice("x", "C", &["HOUR"], "HOUR").is_err());
    assert_eq!(v.text("s"), "");
    // Names.
    let m: BTreeMap<String, Value> = BTreeMap::from([
        ("a".into(), json!("ok")),
        ("b".into(), json!("x".repeat(65))),
        ("c".into(), json!("a\nb")),
    ]);
    let v = Vals(&m);
    assert_eq!(name(v, "a", "Nombre", 64).unwrap(), "ok");
    assert!(name(v, "b", "Nombre", 64)
        .unwrap_err()
        .to_string()
        .contains("64"));
    assert!(name(v, "c", "Nombre", 64).is_err());
    assert!(name(v, "missing", "Nombre", 64)
        .unwrap_err()
        .to_string()
        .contains("obligatorio"));
    // Picks.
    let dbs = vec![("Sales".to_string(), "Sales".to_string())];
    assert_eq!(pick("sales", &dbs, |s| s.to_string()).unwrap(), "Sales");
    assert!(pick("other", &dbs, |s| format!("no {s}")).is_err());
}

#[test]
fn a_body_is_one_statement_for_the_server() {
    // One statement: no `;` but inside strings, names and comments.
    for ok in [
        "INSERT INTO t VALUES (1)",
        "INSERT INTO t VALUES (';')",
        "INSERT INTO `a;b` SELECT \"x;y\"",
        "INSERT INTO t SELECT 1 -- a; comment\n",
        "INSERT INTO t SELECT 1 /* ; */",
        "INSERT INTO t SELECT 1 # ;",
        "INSERT INTO t VALUES ('it''s; fine')",
    ] {
        assert!(one_statement(ok, false), "{ok}");
        assert!(one_statement(ok, true), "{ok}");
    }
    for bad in [
        "UPDATE t SET x = 0; DELETE FROM u",
        "SELECT 1;SELECT 2",
        "SELECT 1; -- trailing",
        // An executable comment is code.
        "SELECT 1 /*!50000 ; DELETE FROM u */",
        "SELECT 1 /*M!100100 ; DELETE FROM u */",
        // A backslash ends the string under NO_BACKSLASH_ESCAPES.
        "SELECT 'a\\'; DELETE FROM u; -- '",
        // `"` is a name under ANSI_QUOTES, and a name doesn't escape with
        // a backslash.
        "SELECT \"a\\\"; DELETE FROM u; -- \"",
        // A statement block without BEGIN … END.
        "IF x THEN DELETE FROM u; END IF",
    ] {
        assert!(!one_statement(bad, false), "{bad}");
        assert!(!one_statement(bad, true), "{bad}");
    }
    // An event's BEGIN … END holds `;`, nested blocks, loops, CASE, IF,
    // handlers, labels.
    for ok in [
        "BEGIN SELECT 1; SELECT 2; END",
        "lbl: BEGIN SELECT 1; END lbl",
        "`my lbl`: BEGIN SELECT 1; END",
        "BEGIN\n  DECLARE done INT DEFAULT 0;\n  DECLARE CONTINUE HANDLER FOR NOT FOUND SET done = 1;\n  DECLARE EXIT HANDLER FOR SQLEXCEPTION, SQLSTATE VALUE '23000', 1062 BEGIN ROLLBACK; RESIGNAL; END;\n  IF done THEN SELECT 1; ELSEIF x THEN BEGIN SELECT 2; END; ELSE SELECT 3; END IF;\n  CASE y WHEN 1 THEN SELECT CASE WHEN a THEN 1 ELSE 2 END; ELSE BEGIN END; END CASE;\n  l: LOOP LEAVE l; END LOOP l;\n  REPEAT SET x = x + 1; UNTIL x > 3 END REPEAT;\n  WHILE x > 0 DO SET x = x - 1; END WHILE;\n  SELECT IF(a, 1, 2), REPEAT('x', 2), t.begin, @begin FROM t;\nEND",
        "BEGIN FOR i IN 1..3 DO SELECT i; END FOR; END",
    ] {
        assert!(one_statement(ok, true), "{ok}");
        assert!(!one_statement(ok, false), "only in an event: {ok}");
    }
    for bad in [
        "BEGIN SELECT 1; END; DELETE FROM u",
        "BEGIN SELECT 1; END lbl; DELETE FROM u",
        // Unclosed, or closed by the wrong END.
        "BEGIN SELECT 1;",
        "BEGIN IF x THEN SELECT 1; END; END",
        "BEGIN CASE x WHEN 1 THEN SELECT 1; END; END",
        // A column named begin would count as a block the server doesn't
        // open, and hide the `;` after the block's real END.
        "BEGIN UPDATE t SET begin = 1; END; DELETE FROM u; END",
        "BEGIN SELECT CASE WHEN 1 THEN begin END FROM t; END; DELETE FROM u; END",
        "BEGIN DECLARE h CONTINUE HANDLER FOR SQLEXCEPTION SET begin = 1; END; DELETE FROM u; END",
        // A column named end closes too early: refused, not mistaken.
        "BEGIN SELECT end FROM t; END",
        // Behind an executable comment, or a string that ends sooner
        // without backslash escapes.
        "BEGIN SELECT 1; END /*! ; DELETE FROM u */",
        "BEGIN SELECT 'a\\'; END; DELETE FROM u; -- '; END",
    ] {
        assert!(!one_statement(bad, true), "{bad}");
    }
}

#[test]
fn properties_are_checked_and_diffed() {
    let v = with_props(
        FormValues::default(),
        &[("session.query_timeout", "60"), ("max_interval", "it's")],
    );
    let p = read_properties(&v).unwrap();
    assert_eq!(p[1], ("max_interval".to_string(), "it's".to_string()));
    assert_eq!(
        properties_sql(&p),
        "('session.query_timeout' = '60', 'max_interval' = 'it''s')"
    );
    for (k, x) in [("a'b", "1"), ("a b", "1"), ("k", "a\nb")] {
        let v = with_props(FormValues::default(), &[(k, x)]);
        assert!(read_properties(&v).is_err(), "{k:?} {x:?}");
    }
    let dup = with_props(FormValues::default(), &[("a", "1"), ("A", "2")]);
    assert!(err(read_properties(&dup)).contains("repetida"));
    let cur = vec![
        ("a".to_string(), "1".to_string()),
        ("b".to_string(), "2".to_string()),
    ];
    let want = vec![
        ("a".to_string(), "1".to_string()),
        ("c".to_string(), "3".to_string()),
    ];
    let (set, removed) = property_changes(&cur, &want);
    assert_eq!(set, [("c".to_string(), "3".to_string())]);
    assert_eq!(removed, ["b"]);
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

mod ev {
    use super::super::events::*;
    use super::*;
    use crate::jobs::events::Event;

    const MYSQL8: Flavor = Flavor {
        ob: false,
        replica: true,
        resets_schedule: true,
        any_definer: true,
    };
    const MARIADB: Flavor = Flavor {
        ob: false,
        replica: false,
        resets_schedule: false,
        any_definer: true,
    };
    const OB: Flavor = Flavor {
        ob: true,
        replica: false,
        resets_schedule: false,
        any_definer: true,
    };

    fn spec() -> Spec {
        Spec {
            schema: "db".into(),
            name: "ev".into(),
            definer: "root@localhost".into(),
            status: Status::Enabled,
            comment: String::new(),
            schedule: Schedule::Every {
                value: "1".into(),
                unit: "HOUR".into(),
                starts: Some("2030-01-01 00:00:00".into()),
                ends: Some("2031-01-01 00:00:00".into()),
            },
            preserve: false,
            time_zone: "SYSTEM".into(),
            sql_mode: "STRICT_TRANS_TABLES".into(),
            body: "INSERT INTO t VALUES (1)".into(),
        }
    }

    fn ctx() -> Ctx {
        Ctx {
            scheduler: "ON".into(),
            user: "app@%".into(),
            version: "8.4.11".into(),
            privileges: vec!["EVENT".into()],
            session_zone: "SYSTEM".into(),
            session_mode: "STRICT_TRANS_TABLES".into(),
            mysql: true,
            ..Ctx::default()
        }
    }

    fn dbs() -> Vec<(String, String)> {
        vec![("db".into(), "db".into()), ("db2".into(), "db2".into())]
    }

    fn definers() -> Vec<(String, String)> {
        ["app@%", "root@localhost"]
            .iter()
            .map(|d| (d.to_string(), d.to_string()))
            .collect()
    }

    fn values() -> Value {
        json!({
            "schema": "db", "name": "ev", "definer": "app@%", "status": "enable",
            "type": "every", "every": "1:30", "unit": "HOUR_MINUTE",
            "starts": "2030-01-01T00:00:00", "ends": "", "completion": "drop",
            "time_zone": "SYSTEM", "sql_mode": "STRICT_TRANS_TABLES",
            "body": "INSERT INTO t VALUES (1);", "comment": "nightly"
        })
    }

    #[test]
    fn a_new_event_is_created_under_its_mode_and_zone() {
        let mut s = spec();
        s.comment = "it's a \\ test".into();
        let p = create_plan(&s, MYSQL8);
        assert_eq!(p.id.as_deref(), Some("`db`.`ev`"));
        let sc = p.script();
        assert!(sc.starts_with("-- Crea el evento «db.ev»\nSET @dbine_sql_mode = @@session.sql_mode, @dbine_time_zone = @@session.time_zone;\nSET SESSION sql_mode = 'STRICT_TRANS_TABLES', time_zone = 'SYSTEM';\n"), "{sc}");
        assert!(sc.contains("CREATE DEFINER = `root`@`localhost` EVENT `db`.`ev`\nON SCHEDULE EVERY 1 HOUR STARTS '2030-01-01 00:00:00' ENDS '2031-01-01 00:00:00'\nON COMPLETION NOT PRESERVE\nENABLE\nCOMMENT 'it''s a \\\\ test'\nDO INSERT INTO t VALUES (1);\n"), "{sc}");
        assert!(
            sc.ends_with("SET SESSION sql_mode = @dbine_sql_mode, time_zone = @dbine_time_zone;\n"),
            "{sc}"
        );
        assert_eq!(p.finally.len(), 1);
        // Under NO_BACKSLASH_ESCAPES a backslash is itself.
        s.sql_mode = "NO_BACKSLASH_ESCAPES".into();
        assert!(create_plan(&s, MYSQL8)
            .script()
            .contains("COMMENT 'it''s a \\ test'"));
        // Once, kept, off on replicas, with a block.
        s.schedule = Schedule::At("2030-06-01 12:00:00".into());
        s.preserve = true;
        s.status = Status::ReplicaSide;
        s.body = "BEGIN\n  SELECT 1;\n  SELECT 2;\nEND".into();
        let sc = create_plan(&s, MYSQL8).script();
        assert!(
            sc.contains(
                "ON SCHEDULE AT '2030-06-01 12:00:00'\nON COMPLETION PRESERVE\nDISABLE ON REPLICA"
            ),
            "{sc}"
        );
        assert!(
            sc.contains("DELIMITER $$\nCREATE DEFINER") && sc.contains("END$$\nDELIMITER ;"),
            "{sc}"
        );
        assert!(create_plan(&s, MARIADB)
            .script()
            .contains("DISABLE ON SLAVE"));
        // A composite interval goes as a string, a role as a definer alone.
        s.schedule = Schedule::Every {
            value: "1:30".into(),
            unit: "HOUR_MINUTE".into(),
            starts: None,
            ends: None,
        };
        s.definer = "admins".into();
        let sc = create_plan(&s, MARIADB).script();
        assert!(
            sc.contains("CREATE DEFINER = `admins` EVENT")
                && sc.contains("EVERY '1:30' HOUR_MINUTE\n"),
            "{sc}"
        );
    }

    #[test]
    fn oceanbase_scripts_use_the_database_and_keep_no_zone() {
        let p = create_plan(&spec(), OB);
        assert_eq!(p.steps[0], "USE `db`");
        let sc = p.script();
        assert!(!sc.contains("time_zone"), "{sc}");
        assert!(
            sc.ends_with("SET SESSION sql_mode = @dbine_sql_mode;\n"),
            "{sc}"
        );
        let d = drop_plan("db", "ev", OB);
        assert_eq!(d.steps, ["USE `db`", "DROP EVENT `db`.`ev`"]);
        assert_eq!(
            drop_plan("my`db", "e v", MYSQL8).steps,
            ["DROP EVENT `my``db`.`e v`"]
        );
    }

    #[test]
    fn an_edit_alters_only_what_changed_and_keeps_the_definer() {
        let cur = spec();
        assert!(alter_plan(&cur, &cur, MYSQL8).is_none());
        // The comment alone.
        let mut want = cur.clone();
        want.comment = "x".into();
        let p = alter_plan(&cur, &want, MYSQL8).unwrap();
        let st = &p.steps[2];
        assert_eq!(
            st,
            "ALTER DEFINER = `root`@`localhost` EVENT `db`.`ev`\nCOMMENT 'x'"
        );
        // The end gone: the schedule whole, STARTS kept.
        let mut want = cur.clone();
        if let Schedule::Every { ends, .. } = &mut want.schedule {
            *ends = None;
        }
        let p = alter_plan(&cur, &want, MYSQL8).unwrap();
        assert_eq!(p.steps.len(), 3, "{:?}", p.steps);
        assert!(
            p.steps[2].ends_with("ON SCHEDULE EVERY 1 HOUR STARTS '2030-01-01 00:00:00'"),
            "{}",
            p.steps[2]
        );
        assert_eq!(p.undo_from, None);
        // MariaDB keeps an ENDS left out: a far one-time schedule first,
        // and if the ALTER after it fails, the old schedule back under the
        // event's zone and mode.
        let p = alter_plan(&cur, &want, MARIADB).unwrap();
        assert_eq!(p.steps.len(), 4, "{:?}", p.steps);
        assert_eq!(p.steps[2], "ALTER DEFINER = `root`@`localhost` EVENT `db`.`ev`\nON SCHEDULE AT '2037-12-31 23:59:59'");
        assert_eq!(p.undo_from, Some(2));
        assert_eq!(
            p.undo,
            [
                "SET SESSION sql_mode = 'STRICT_TRANS_TABLES', time_zone = 'SYSTEM'",
                "ALTER DEFINER = `root`@`localhost` EVENT `db`.`ev`\nON SCHEDULE EVERY 1 HOUR STARTS '2030-01-01 00:00:00' ENDS '2031-01-01 00:00:00'",
            ]
        );
        assert!(p.head.len() == 3 && p.head[2].contains("le devuelve al evento su programación"));
        // The ALTER renames it and changes the definer: the undo names the
        // event as it is, with its own definer where the login may set it,
        // else with the one the first step left.
        let moved = Spec {
            name: "ev2".into(),
            definer: "app@%".into(),
            ..want.clone()
        };
        let p = alter_plan(&cur, &moved, MARIADB).unwrap();
        assert!(p.steps[3].contains("RENAME TO `db`.`ev2`"), "{:?}", p.steps);
        assert!(p.undo[1].starts_with("ALTER DEFINER = `root`@`localhost` EVENT `db`.`ev`\n"));
        let own_only = Flavor {
            any_definer: false,
            ..MARIADB
        };
        let p = alter_plan(&cur, &moved, own_only).unwrap();
        assert!(p.undo[1].starts_with("ALTER DEFINER = `app`@`%` EVENT `db`.`ev`\n"));
        // OceanBase: from the event's database, no zone.
        let p = alter_plan(&cur, &want, OB).unwrap();
        assert_eq!(p.steps[0], "USE `db`");
        assert_eq!(p.undo_from, Some(3));
        assert_eq!(p.undo[0], "SET SESSION sql_mode = 'STRICT_TRANS_TABLES'");
        // No start: from now, said.
        if let Schedule::Every { starts, .. } = &mut want.schedule {
            *starts = None;
        }
        let p = alter_plan(&cur, &want, MYSQL8).unwrap();
        assert!(
            p.steps[2].ends_with("EVERY 1 HOUR STARTS CURRENT_TIMESTAMP"),
            "{}",
            p.steps[2]
        );
        // The zone alone moves the schedule (the event keeps the zone it's
        // set in); on OceanBase there's no zone of its own.
        let mut want = cur.clone();
        want.time_zone = "+03:00".into();
        let p = alter_plan(&cur, &want, MYSQL8).unwrap();
        assert!(
            p.steps[1].ends_with("time_zone = '+03:00'")
                && p.steps[2].contains("ON SCHEDULE EVERY"),
            "{:?}",
            p.steps
        );
        assert!(alter_plan(&cur, &want, OB).is_none());
        // Everything at once, in the grammar's order.
        let want = Spec {
            schema: "db2".into(),
            name: "ev2".into(),
            status: Status::Disabled,
            comment: "c".into(),
            preserve: true,
            sql_mode: "ANSI_QUOTES".into(),
            body: "DELETE FROM t".into(),
            ..cur.clone()
        };
        let p = alter_plan(&cur, &want, MYSQL8).unwrap();
        assert_eq!(
            p.steps[2],
            "ALTER DEFINER = `root`@`localhost` EVENT `db`.`ev`\nON COMPLETION PRESERVE\nRENAME TO `db2`.`ev2`\nDISABLE\nCOMMENT 'c'\nDO DELETE FROM t"
        );
        assert_eq!(p.id.as_deref(), Some("`db2`.`ev2`"));
        assert!(p.steps[1].contains("sql_mode = 'ANSI_QUOTES'"));
        // The SQL mode alone sends the statement again (the event keeps the
        // mode it's defined under).
        let want = Spec {
            sql_mode: "ANSI".into(),
            ..cur.clone()
        };
        assert!(alter_plan(&cur, &want, MYSQL8).unwrap().steps[2]
            .ends_with("DO INSERT INTO t VALUES (1)"));
        // The definer alone: ALTER EVENT takes a clause, the status as is.
        let want = Spec {
            definer: "app@%".into(),
            ..cur.clone()
        };
        assert_eq!(
            alter_plan(&cur, &want, MYSQL8).unwrap().steps[2],
            "ALTER DEFINER = `app`@`%` EVENT `db`.`ev`\nENABLE"
        );
        // The status off on replicas, per server.
        let want = Spec {
            status: Status::ReplicaSide,
            ..cur.clone()
        };
        assert!(alter_plan(&cur, &want, MYSQL8).unwrap().steps[2].ends_with("DISABLE ON REPLICA"));
        assert!(alter_plan(&cur, &want, MARIADB).unwrap().steps[2].ends_with("DISABLE ON SLAVE"));
        // On OceanBase, from the event's database.
        let want = Spec {
            comment: "x".into(),
            ..cur.clone()
        };
        assert_eq!(alter_plan(&cur, &want, OB).unwrap().steps[0], "USE `db`");
    }

    #[test]
    fn the_server_row_reads_back_as_the_form_writes_it() {
        let e = Event {
            schema: "db".into(),
            name: "ev".into(),
            definer: Some("root@localhost".into()),
            tz: "+03:00".into(),
            body: Some("INSERT INTO t VALUES (1);\n".into()),
            recurring: true,
            every: Some("'1:30'".into()),
            unit: Some("hour_minute".into()),
            starts: Some("2030-01-01 00:00:00".into()),
            status: "SLAVESIDE_DISABLED".into(),
            preserve: true,
            sql_mode: Some("ANSI".into()),
            altered: Some("2026-11-12 10:00:00".into()),
            ..Event::default()
        };
        let s = spec_of(&e, None);
        assert_eq!(s.status, Status::ReplicaSide);
        assert_eq!(s.body, "INSERT INTO t VALUES (1)");
        assert_eq!(
            s.schedule,
            Schedule::Every {
                value: "1:30".into(),
                unit: "HOUR_MINUTE".into(),
                starts: Some("2030-01-01 00:00:00".into()),
                ends: None
            }
        );
        assert_eq!(s.time_zone, "+03:00");
        assert_eq!(spec_of(&e, Some("+08:00")).time_zone, "+08:00");
        let v = values_of(&s, Some(&e));
        assert_eq!(v["every"], json!("1:30"));
        assert_eq!(v["starts"], json!("2030-01-01T00:00:00"));
        assert_eq!(v["status"], json!("replica"));
        assert_eq!(v["completion"], json!("preserve"));
        assert_eq!(v["altered"], json!("2026-11-12T10:00:00"));
        // A form read before the event changed is refused.
        let stale_form = fv(json!({"altered": "2026-11-12T09:59:59"}));
        assert!(err(stale(&stale_form, &e)).contains("cambió en el servidor"));
        assert!(stale(&fv(json!({"altered": "2026-11-12T10:00:00"})), &e).is_ok());
        assert!(stale(&fv(json!({})), &e).is_ok());
    }

    #[test]
    fn the_form_is_checked_before_a_script_is_written() {
        let c = ctx();
        let s = spec_from(&fv(values()), &c, &dbs(), &definers(), None).unwrap();
        assert_eq!(s.definer, "app@%");
        assert_eq!(s.body, "INSERT INTO t VALUES (1)");
        assert_eq!(
            s.schedule,
            Schedule::Every {
                value: "1:30".into(),
                unit: "HOUR_MINUTE".into(),
                starts: Some("2030-01-01 00:00:00".into()),
                ends: None
            }
        );
        for (k, bad, says) in [
            ("every", json!("1:2:3"), "no es un intervalo"),
            ("every", json!("0"), "no es un intervalo"),
            ("every", json!("1' OR '1"), "no es un intervalo"),
            ("unit", json!("FORTNIGHT"), "no es una opción válida"),
            ("starts", json!("tomorrow"), "no es una fecha"),
            ("schema", json!("nope"), "No existe la base"),
            ("definer", json!("x@y"), "no es una cuenta"),
            ("definer", json!("x@y'z"), "no es un definidor"),
            ("time_zone", json!("UTC'; DROP"), "no es una zona"),
            ("sql_mode", json!("ANSI'"), "no es un modo SQL"),
            ("body", json!(" ; "), "necesita una instrucción"),
            ("name", json!(""), "obligatorio"),
            ("status", json!("maybe"), "no es una opción válida"),
        ] {
            let mut v = values();
            v[k] = bad;
            let m = err(spec_from(&fv(v), &c, &dbs(), &definers(), None));
            assert!(m.contains(says), "{k}: {m}");
        }
        let mut v = values();
        v["ends"] = json!("2029-01-01T00:00:00");
        assert!(err(spec_from(&fv(v), &c, &dbs(), &definers(), None)).contains("posterior"));
        let mut v = values();
        v["type"] = json!("once");
        assert!(err(spec_from(&fv(v), &c, &dbs(), &definers(), None)).contains("Fecha y hora"));
        // Someone else as definer needs SET_USER_ID; a foreign one kept
        // says how to save.
        let mut v = values();
        v["definer"] = json!("root@localhost");
        assert!(
            err(spec_from(&fv(v.clone()), &c, &dbs(), &definers(), None)).contains("SET_USER_ID")
        );
        let cur = spec();
        assert!(err(spec_from(
            &fv(v.clone()),
            &c,
            &dbs(),
            &definers(),
            Some(&cur)
        ))
        .contains("elegí tu usuario"));
        let admin = Ctx {
            privileges: vec!["SET_USER_ID".into()],
            ..ctx()
        };
        assert!(spec_from(&fv(v), &admin, &dbs(), &definers(), Some(&cur)).is_ok());
        // OceanBase doesn't move events, and reads times on its zone.
        let ob = Ctx {
            ob_zone: Some("+08:00".into()),
            ob: true,
            mysql: false,
            ..ctx()
        };
        let mut v = values();
        v["schema"] = json!("db2");
        assert!(err(spec_from(&fv(v), &ob, &dbs(), &definers(), Some(&cur)))
            .contains("OceanBase no mueve"));
        let s = spec_from(&fv(values()), &ob, &dbs(), &definers(), None).unwrap();
        assert_eq!(s.time_zone, "+08:00");
        // One statement, or one BEGIN … END: a `;` outside it would end
        // the CREATE and run the rest when saving.
        for body in [
            "UPDATE t SET x = 0; DELETE FROM u",
            "BEGIN SELECT 1; END; DELETE FROM u",
            "INSERT INTO t VALUES (1); -- nightly",
        ] {
            let mut v = values();
            v["body"] = json!(body);
            let m = err(spec_from(&fv(v), &c, &dbs(), &definers(), None));
            assert!(m.contains("tiene que ser una sola"), "{body}: {m}");
        }
        let block = "BEGIN\n  INSERT INTO t VALUES (';');\n  DELETE FROM u;\nEND;";
        let mut v = values();
        v["body"] = json!(block);
        let s = spec_from(&fv(v), &c, &dbs(), &definers(), None).unwrap();
        assert_eq!(
            s.body,
            "BEGIN\n  INSERT INTO t VALUES (';');\n  DELETE FROM u;\nEND"
        );
    }

    #[test]
    fn intervals_zones_and_modes() {
        assert!(valid_interval("1", "HOUR"));
        assert!(valid_interval("1:30", "HOUR_MINUTE"));
        assert!(valid_interval("2 12", "DAY_HOUR"));
        assert!(valid_interval("1-6", "YEAR_MONTH"));
        assert!(!valid_interval("1:2:3", "HOUR_MINUTE"));
        assert!(!valid_interval("0", "DAY"));
        assert!(!valid_interval("", "DAY"));
        assert!(!valid_interval("1.5", "DAY"));
        assert!(UNITS.iter().all(|u| valid_interval("1", u)));
        for z in [
            "SYSTEM",
            "+03:00",
            "-05:30",
            "Europe/Madrid",
            "America/Argentina/Buenos_Aires",
        ] {
            assert!(valid_zone(z), "{z}");
        }
        for z in ["", "x'y", "a b", &"x".repeat(65)] {
            assert!(!valid_zone(z), "{z}");
        }
        assert!(valid_mode(""));
        assert!(valid_mode("ANSI_QUOTES,STRICT_ALL_TABLES"));
        assert!(!valid_mode("ANSI QUOTES"));
        assert_eq!(lit_in("ANSI,NO_BACKSLASH_ESCAPES", "a\\b'c"), "'a\\b''c'");
        assert_eq!(lit_in("ANSI", "a\\b'c"), "'a\\\\b''c'");
    }

    #[test]
    fn privileges_and_versions() {
        assert!(version_at_least("8.0.22", 8, 0, 22));
        assert!(version_at_least("8.4.11-log", 8, 0, 22));
        assert!(!version_at_least("8.0.21", 8, 0, 22));
        assert!(!version_at_least("5.7.44", 8, 0, 0));
        assert!(version_at_least("11.8.2-MariaDB-ubu2404", 10, 5, 0));
        let lines = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(grants_event(&lines(&[
            "GRANT USAGE ON *.* TO `u`@`%`",
            "GRANT SELECT, EVENT ON `db`.* TO `u`@`%`"
        ])));
        assert!(grants_event(&lines(&[
            "GRANT ALL PRIVILEGES ON `db`.* TO `u`@`%`"
        ])));
        assert!(!grants_event(&lines(&[
            "GRANT USAGE ON *.* TO `u`@`%`",
            "GRANT `events_role`@`%` TO `u`@`%`"
        ])));
        assert!(!grants_event(&lines(&[
            "GRANT SELECT ON `event`.* TO `u`@`%`"
        ])));
        let c = Ctx {
            privileges: vec!["SET_ANY_DEFINER".into()],
            ..ctx()
        };
        assert!(c.sets_any_definer());
        assert!(!ctx().sets_any_definer());
        assert!(ctx().may_write());
        let none = Ctx {
            privileges: vec!["USAGE".into()],
            ..ctx()
        };
        assert!(!none.may_write());
        assert!(ctx().flavor().replica && ctx().flavor().resets_schedule);
        let old = Ctx {
            version: "5.7.44".into(),
            ..ctx()
        };
        assert!(!old.flavor().replica && !old.flavor().resets_schedule);
        // DISABLE ON REPLICA came in 8.2.0: 8.0 and 8.1 (Aurora MySQL 3)
        // only take DISABLE ON SLAVE.
        for (version, replica) in [
            ("8.0.36", false),
            ("8.0.22", false),
            ("8.1.0", false),
            ("8.2.0", true),
            ("9.1.0", true),
        ] {
            let c = Ctx {
                version: version.into(),
                ..ctx()
            };
            assert_eq!(c.flavor().replica, replica, "{version}");
        }
        let c = Ctx {
            version: "8.0.36".into(),
            ..ctx()
        };
        let s = Spec {
            status: Status::ReplicaSide,
            ..spec()
        };
        let sc = create_plan(&s, c.flavor()).script();
        assert!(
            sc.contains("\nDISABLE ON SLAVE\n") && !sc.contains("REPLICA"),
            "{sc}"
        );
        // A definer-only edit of such an event repeats its status: SLAVE.
        let want = Spec {
            definer: "app@%".into(),
            ..s.clone()
        };
        assert!(alter_plan(&s, &want, c.flavor()).unwrap().steps[2].ends_with("DISABLE ON SLAVE"));
        // Who may name another definer.
        assert!(!ctx().flavor().any_definer);
        let admin = Ctx {
            privileges: vec!["SET_USER_ID".into()],
            ..ctx()
        };
        assert!(admin.flavor().any_definer);
    }

    #[test]
    fn the_log_names_the_event() {
        assert_eq!(
            log_line("Event Scheduler: [root@localhost][db.ev] Table 'db.x' doesn't exist"),
            (
                "db.ev".to_string(),
                "Table 'db.x' doesn't exist".to_string()
            )
        );
        assert_eq!(
            log_line("Event Scheduler: scheduler thread started with id 5"),
            (
                String::new(),
                "scheduler thread started with id 5".to_string()
            )
        );
    }

    #[test]
    fn the_form_has_every_part_of_an_event() {
        let keys = |s: &[dbine_driver::FormSection]| {
            s.iter()
                .flat_map(|x| x.fields.iter().map(|f| f.key.clone()))
                .collect::<Vec<_>>()
        };
        let new = keys(&sections(false, false));
        for k in [
            "schema",
            "name",
            "definer",
            "status",
            "comment",
            "type",
            "at",
            "every",
            "unit",
            "starts",
            "ends",
            "time_zone",
            "completion",
            "body",
            "sql_mode",
        ] {
            assert!(new.contains(&k.to_string()), "{k}");
        }
        assert!(!new.contains(&"created".to_string()));
        let old = sections(true, true);
        assert!(keys(&old).contains(&"last_executed".to_string()));
        let field = |k: &str| {
            old.iter()
                .flat_map(|s| s.fields.iter())
                .find(|f| f.key == k)
                .unwrap()
                .clone()
        };
        assert!(field("schema").read_only && field("time_zone").read_only);
        assert_eq!(field("at").when, Some(("type".into(), vec!["once".into()])));
    }
}

// ---------------------------------------------------------------------------
// Doris
// ---------------------------------------------------------------------------

mod doris {
    use super::super::doris::*;
    use super::*;

    fn job() -> JobSpec {
        JobSpec {
            name: "j".into(),
            database: Some("db".into()),
            enabled: true,
            comment: String::new(),
            mode: Mode::Every {
                n: 2,
                unit: "HOUR".into(),
                starts: Some("2030-01-01 00:00:00".into()),
                ends: None,
            },
            properties: Vec::new(),
            sql: "INSERT INTO dst SELECT * FROM src".into(),
        }
    }

    #[test]
    fn a_new_job_is_created_in_its_database() {
        let mut j = job();
        j.comment = "it's \\ odd".into();
        j.enabled = false;
        let p = create_plan(&j);
        assert_eq!(p.id.as_deref(), Some("job:`j`"));
        assert_eq!(
            p.steps,
            [
                "USE `db`",
                "CREATE JOB `j`\nON SCHEDULE EVERY 2 HOUR STARTS '2030-01-01 00:00:00'\nCOMMENT 'it\\'s \\\\ odd'\nDO INSERT INTO dst SELECT * FROM src",
                "PAUSE JOB WHERE jobname = 'j'",
            ]
        );
        j.mode = Mode::At("2030-06-01 12:00:00".into());
        j.comment.clear();
        j.enabled = true;
        j.database = None;
        assert_eq!(
            create_steps(&j),
            ["CREATE JOB `j`\nON SCHEDULE AT '2030-06-01 12:00:00'\nDO INSERT INTO dst SELECT * FROM src"]
        );
        j.mode = Mode::Streaming;
        j.properties = vec![("max_interval".into(), "10".into())];
        assert_eq!(
            create_steps(&j),
            ["CREATE JOB `j`\nPROPERTIES ('max_interval' = '10')\nON STREAMING\nDO INSERT INTO dst SELECT * FROM src"]
        );
        assert_eq!(comment_lit("a'b\\c"), "'a\\'b\\\\c'");
        // A name that would end the identifier stays inside it.
        j.name = "x`; DROP JOB y".into();
        assert!(create_steps(&j)[0].starts_with("CREATE JOB `x``; DROP JOB y`"));
    }

    #[test]
    fn a_scheduled_job_is_dropped_and_created_again() {
        let cur = job();
        assert!(alter_plan(&cur, &cur, true).is_none());
        // Enabled alone: in place.
        let want = JobSpec {
            enabled: false,
            ..cur.clone()
        };
        let p = alter_plan(&cur, &want, true).unwrap();
        assert_eq!(p.steps, ["PAUSE JOB WHERE jobname = 'j'"]);
        assert!(p.undo.is_empty());
        // A finished job can't be resumed: nothing to do.
        assert!(alter_plan(&cur, &want, false).is_none());
        // The schedule: dropped and created again, the old one as undo.
        let want = JobSpec {
            mode: Mode::Every {
                n: 1,
                unit: "DAY".into(),
                starts: None,
                ends: None,
            },
            comment: "c".into(),
            enabled: false,
            ..cur.clone()
        };
        let p = alter_plan(&cur, &want, true).unwrap();
        assert_eq!(p.steps[0], "USE `db`");
        assert_eq!(p.steps[1], "DROP JOB WHERE jobname = 'j'");
        assert!(p.steps[2].starts_with("CREATE JOB `j`\nON SCHEDULE EVERY 1 DAY\nCOMMENT 'c'"));
        assert_eq!(p.steps[3], "PAUSE JOB WHERE jobname = 'j'");
        assert_eq!(p.undo_from, Some(1));
        assert_eq!(p.undo[0], "USE `db`");
        assert!(p.undo[1]
            .starts_with("CREATE JOB `j`\nON SCHEDULE EVERY 2 HOUR STARTS '2030-01-01 00:00:00'"));
        assert!(p
            .head
            .iter()
            .any(|h| h.contains("lo borra y lo vuelve a crear")));
        // Renamed: the new id.
        let want = JobSpec {
            name: "k".into(),
            ..cur.clone()
        };
        let p = alter_plan(&cur, &want, true).unwrap();
        assert_eq!(p.id.as_deref(), Some("job:`k`"));
        assert!(p.steps.iter().any(|s| s.starts_with("CREATE JOB `k`")));
    }

    #[test]
    fn a_streaming_job_changes_in_place_while_paused() {
        let cur = JobSpec {
            mode: Mode::Streaming,
            properties: vec![("max_interval".into(), "10".into())],
            ..job()
        };
        let want = JobSpec {
            properties: vec![("max_interval".into(), "5".into())],
            sql: "INSERT INTO dst SELECT * FROM S3('uri'='s3://b/*')".into(),
            ..cur.clone()
        };
        let p = alter_plan(&cur, &want, true).unwrap();
        assert_eq!(
            p.steps,
            [
                "USE `db`",
                "PAUSE JOB WHERE jobname = 'j'",
                "ALTER JOB `j`\nPROPERTIES ('max_interval' = '5')\nINSERT INTO dst SELECT * FROM S3('uri'='s3://b/*')",
                "RESUME JOB WHERE jobname = 'j'",
            ]
        );
        // A failed ALTER JOB resumes the job it paused.
        assert_eq!(p.undo_from, Some(1));
        assert_eq!(p.undo, ["RESUME JOB WHERE jobname = 'j'"]);
        assert!(!recreates(&cur, &want));
        // Left paused by the change: still resumed if the ALTER fails.
        let off = JobSpec {
            enabled: false,
            ..want.clone()
        };
        let p = alter_plan(&cur, &off, true).unwrap();
        assert_eq!(p.steps.len(), 3, "no RESUME after: {:?}", p.steps);
        assert_eq!(p.undo, ["RESUME JOB WHERE jobname = 'j'"]);
        // A paused job: nothing to undo.
        let paused = JobSpec {
            enabled: false,
            ..cur.clone()
        };
        let p = alter_plan(&paused, &off, true).unwrap();
        assert_eq!(p.steps[1], "ALTER JOB `j`\nPROPERTIES ('max_interval' = '5')\nINSERT INTO dst SELECT * FROM S3('uri'='s3://b/*')");
        assert!(p.undo_from.is_none() && p.undo.is_empty());
        // A property taken away: created again (ALTER JOB can't unset).
        let want = JobSpec {
            properties: Vec::new(),
            ..cur.clone()
        };
        assert!(recreates(&cur, &want));
        let p = alter_plan(&cur, &want, true).unwrap();
        assert!(p.steps.iter().any(|s| s.starts_with("DROP JOB")));
        assert!(p.head.iter().any(|h| h.contains("hasta dónde había leído")));
        // A CDC job keeps its source.
        let cdc = JobSpec {
            sql: "FROM MYSQL ('jdbc_url' = 'x') TO DATABASE d".into(),
            ..cur.clone()
        };
        assert!(is_cdc(&cdc) && !is_cdc(&cur));
        let want = JobSpec {
            properties: vec![("max_interval".into(), "5".into())],
            ..cdc.clone()
        };
        let p = alter_plan(&cdc, &want, true).unwrap();
        assert_eq!(
            p.steps[2],
            "ALTER JOB `j`\nPROPERTIES ('max_interval' = '5')"
        );
        assert!(cdc_recreate(&cdc, &want).is_none());
        // … and is never dropped to be created again.
        for (want, says) in [
            (
                JobSpec {
                    name: "k".into(),
                    ..cdc.clone()
                },
                "no renombra",
            ),
            (
                JobSpec {
                    comment: "c".into(),
                    ..cdc.clone()
                },
                "no cambia el comentario",
            ),
            (
                JobSpec {
                    properties: Vec::new(),
                    ..cdc.clone()
                },
                "no le quita una propiedad",
            ),
            (
                JobSpec {
                    mode: Mode::Every {
                        n: 1,
                        unit: "DAY".into(),
                        starts: None,
                        ends: None,
                    },
                    ..cdc.clone()
                },
                "no cambia de tipo",
            ),
        ] {
            let m = cdc_recreate(&cdc, &want).unwrap();
            assert!(m.contains(says) && m.contains("FROM … TO DATABASE"), "{m}");
        }
        // Other jobs are created again as before.
        let want = JobSpec {
            name: "k".into(),
            ..cur.clone()
        };
        assert!(cdc_recreate(&cur, &want).is_none());
    }

    #[test]
    fn a_one_time_job_at_a_past_time_isnt_created() {
        let once = JobSpec {
            mode: Mode::At("2026-01-01 00:00:00".into()),
            ..job()
        };
        let now = Some("2026-06-01 10:00:00");
        // Its time has passed, or it ran.
        assert!(once_past(&once, "RUNNING", now));
        assert!(once_past(&once, "FINISHED", Some("2025-01-01 00:00:00")));
        assert!(once_past(&once, "FINISHED", None));
        assert!(!once_past(&once, "RUNNING", Some("2025-12-31 23:59:59")));
        assert!(!once_past(&once, "RUNNING", None));
        assert!(!once_past(&job(), "FINISHED", now));
        // A new one at a past time is refused before Doris refuses it.
        let m = err(not_past(&once, now));
        assert!(
            m.contains("ya pasó") && m.contains("2026-06-01 10:00:00"),
            "{m}"
        );
        assert!(err(not_past(&once, Some("2026-01-01 00:00:00"))).contains("ya pasó"));
        assert!(not_past(&once, Some("2025-12-31 23:59:59")).is_ok());
        assert!(not_past(&once, None).is_ok());
        assert!(not_past(&job(), now).is_ok());
    }

    #[test]
    fn the_job_form_is_checked() {
        let dbs = vec![("db".to_string(), "db".to_string())];
        let base = json!({
            "name": "j", "database": "db", "enabled": true, "type": "every", "every": 2,
            "unit": "HOUR", "starts": "2030-01-01T00:00:00", "sql": "insert into dst select * from src;"
        });
        let j = job_spec_from(&fv(base.clone()), &dbs, None, true).unwrap();
        assert_eq!(j.sql, "insert into dst select * from src");
        assert_eq!(
            j.mode,
            Mode::Every {
                n: 2,
                unit: "HOUR".into(),
                starts: Some("2030-01-01 00:00:00".into()),
                ends: None
            }
        );
        for (k, bad, says) in [
            ("unit", json!("SECOND"), "no es una opción válida"),
            ("every", json!(0), "entre 1"),
            ("sql", json!("DELETE FROM dst"), "INSERT"),
            ("sql", json!(""), "necesita una instrucción"),
            ("database", json!("nope"), "No existe la base"),
            ("ends", json!("2029-01-01T00:00:00"), "posterior"),
            ("type", json!("cron"), "no es una opción válida"),
            (
                "sql",
                json!("INSERT INTO dst SELECT * FROM src; DELETE FROM dst"),
                "tiene que ser una sola",
            ),
            (
                "sql",
                json!("INSERT INTO dst SELECT 1 /*!; DELETE FROM dst */"),
                "tiene que ser una sola",
            ),
        ] {
            let mut v = base.clone();
            v[k] = bad;
            let m = err(job_spec_from(&fv(v), &dbs, None, true));
            assert!(m.contains(says), "{k}: {m}");
        }
        let mut v = base.clone();
        v["sql"] = json!("INSERT INTO dst SELECT ';' FROM src");
        assert!(job_spec_from(&fv(v), &dbs, None, true).is_ok());
        let mut v = base.clone();
        v["type"] = json!("at");
        assert!(err(job_spec_from(&fv(v), &dbs, None, true)).contains("Fecha y hora"));
        let v = with_props(fv(base.clone()), &[("bad key", "1")]);
        assert!(job_spec_from(&v, &dbs, None, true).is_err());
        // Older servers: no streaming jobs, no job properties.
        let mut v = base.clone();
        v["type"] = json!("streaming");
        assert!(job_spec_from(&fv(v.clone()), &dbs, None, true).is_ok());
        assert!(err(job_spec_from(&fv(v), &dbs, None, false)).contains("jobs continuos"));
        let v = with_props(fv(base.clone()), &[("session.query_timeout", "60")]);
        assert!(job_spec_from(&v, &dbs, None, true).is_ok());
        assert!(err(job_spec_from(&v, &dbs, None, false)).contains("no toma propiedades"));
        assert!(job_spec_from(&fv(base), &dbs, None, false).is_ok());
    }

    #[test]
    fn a_views_refresh_reads_back_and_changes_in_place() {
        let (build, method, t) = refresh_info(
            "BUILD DEFERRED REFRESH AUTO ON SCHEDULE EVERY 1 HOUR STARTS \"2030-01-01 00:00:00\"",
        )
        .unwrap();
        assert_eq!((build.as_str(), method.as_str()), ("DEFERRED", "AUTO"));
        assert_eq!(
            t,
            Trigger::Schedule {
                n: 1,
                unit: "HOUR".into(),
                starts: Some("2030-01-01 00:00:00".into())
            }
        );
        assert_eq!(
            refresh_info("BUILD IMMEDIATE REFRESH COMPLETE ON MANUAL")
                .unwrap()
                .2,
            Trigger::Manual
        );
        assert_eq!(
            refresh_info("BUILD DEFERRED REFRESH COMPLETE ON COMMIT")
                .unwrap()
                .2,
            Trigger::Commit
        );
        assert_eq!(
            refresh_info("BUILD DEFERRED REFRESH AUTO ON SCHEDULE EVERY 2 DAY")
                .unwrap()
                .2,
            Trigger::Schedule {
                n: 2,
                unit: "DAY".into(),
                starts: None
            }
        );
        assert!(refresh_info("nonsense").is_none());
        let cur = MvSpec {
            method: "AUTO".into(),
            trigger: Trigger::Manual,
            enabled: true,
            properties: vec![("grace_period".into(), "10".into())],
        };
        assert!(mv_alter_plan("`d`.`v`", "d.v", &cur, &cur)
            .unwrap()
            .is_none());
        let want = MvSpec {
            method: "COMPLETE".into(),
            trigger: Trigger::Schedule {
                n: 1,
                unit: "DAY".into(),
                starts: Some("2030-01-01 00:00:00".into()),
            },
            enabled: false,
            properties: vec![
                ("grace_period".into(), "20".into()),
                ("workload_group".into(), "normal".into()),
            ],
        };
        let p = mv_alter_plan("`d`.`v`", "d.v", &cur, &want)
            .unwrap()
            .unwrap();
        assert_eq!(
            p.steps,
            [
                "ALTER MATERIALIZED VIEW `d`.`v` SET ('grace_period' = '20', 'workload_group' = 'normal')",
                "ALTER MATERIALIZED VIEW `d`.`v` REFRESH COMPLETE ON SCHEDULE EVERY 1 DAY STARTS '2030-01-01 00:00:00'",
                "PAUSE MATERIALIZED VIEW JOB ON `d`.`v`",
            ]
        );
        let gone = MvSpec {
            properties: Vec::new(),
            ..cur.clone()
        };
        assert!(
            err(mv_alter_plan("`d`.`v`", "d.v", &cur, &gone)).contains("no quita una propiedad")
        );
        let v = fv(json!({"method": "AUTO", "trigger": "COMMIT", "enabled": true}));
        assert_eq!(mv_spec_from(&v).unwrap().trigger, Trigger::Commit);
        let v = fv(json!({"method": "INCREMENTAL", "trigger": "COMMIT"}));
        assert!(mv_spec_from(&v).is_err());
        let v = fv(json!({"method": "AUTO", "trigger": "SCHEDULE", "unit": "SECOND", "every": 1}));
        assert!(mv_spec_from(&v).is_err());
    }

    #[test]
    fn properties_read_in_every_shape_doris_prints() {
        assert_eq!(
            parse_properties(
                "{refresh_partition_num=2, excluded_trigger_tables=a,b, grace_period=}"
            ),
            [
                ("refresh_partition_num".to_string(), "2".to_string()),
                ("excluded_trigger_tables".to_string(), "a,b".to_string()),
                ("grace_period".to_string(), String::new()),
            ]
        );
        assert_eq!(
            parse_properties(r#"{"max_interval":"10","s3.max_batch_files":256}"#),
            [
                ("max_interval".to_string(), "10".to_string()),
                ("s3.max_batch_files".to_string(), "256".to_string()),
            ]
        );
        assert_eq!(
            parse_properties("('a'='1','b'='x''y')"),
            [
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "x'y".to_string())
            ]
        );
        assert!(parse_properties("{}").is_empty());
        assert!(parse_properties("\\N").is_empty());
    }

    #[test]
    fn the_form_has_every_part_of_a_job() {
        let keys: Vec<String> = job_sections(true, true)
            .iter()
            .flat_map(|s| s.fields.iter().map(|f| f.key.clone()))
            .collect();
        for k in [
            "name", "database", "enabled", "comment", "type", "at", "every", "unit", "starts",
            "ends", "sql", "status", "error",
        ] {
            assert!(keys.contains(&k.to_string()), "{k}");
        }
        let has_props = |s: &[dbine_driver::FormSection]| {
            s.iter()
                .any(|s| s.lists.iter().any(|l| l.key == "properties"))
        };
        let types = |s: &[dbine_driver::FormSection]| {
            let f = s
                .iter()
                .flat_map(|s| s.fields.iter())
                .find(|f| f.key == "type")
                .unwrap()
                .clone();
            serde_json::to_string(&f.kind).unwrap()
        };
        assert!(has_props(&job_sections(false, true)));
        assert!(types(&job_sections(false, true)).contains("streaming"));
        // An older Doris: neither.
        assert!(!has_props(&job_sections(false, false)));
        let t = types(&job_sections(true, false));
        assert!(!t.contains("streaming") && t.contains("every"), "{t}");
    }
}

// ---------------------------------------------------------------------------
// StarRocks
// ---------------------------------------------------------------------------

mod starrocks {
    use super::super::starrocks::*;
    use super::*;
    use crate::jobs::starrocks::Task;

    fn task() -> TaskSpec {
        TaskSpec {
            name: "t".into(),
            database: "db".into(),
            every: Some(Every {
                start: Some("2030-01-01 00:00:00".into()),
                n: 1,
                unit: "HOUR".into(),
            }),
            properties: vec![("query_timeout".into(), "120".into())],
            definition: "INSERT INTO dst SELECT * FROM src".into(),
        }
    }

    #[test]
    fn a_new_task_is_submitted_in_its_database() {
        let p = create_plan(&task(), StateChange::Suspend);
        assert_eq!(p.id.as_deref(), Some("task:`t`"));
        assert_eq!(
            p.steps,
            [
                "USE `db`",
                "SUBMIT TASK `db`.`t`\nSCHEDULE START('2030-01-01 00:00:00') EVERY(INTERVAL 1 HOUR)\nPROPERTIES ('query_timeout' = '120')\nAS INSERT INTO dst SELECT * FROM src",
                "ALTER TASK `t` SUSPEND",
            ]
        );
        let once = TaskSpec {
            every: None,
            properties: Vec::new(),
            ..task()
        };
        let p = create_plan(&once, StateChange::Suspend);
        assert_eq!(
            p.steps[1],
            "SUBMIT TASK `db`.`t`\nAS INSERT INTO dst SELECT * FROM src"
        );
        assert_eq!(p.steps.len(), 2, "no schedule to suspend");
        assert!(p.head.iter().any(|h| h.contains("en cuanto se crea")));
    }

    #[test]
    fn properties_and_the_state_change_in_place_the_rest_submits_again() {
        let cur = task();
        assert!(alter_plan(&cur, &cur, StateChange::Keep).is_none());
        let want = TaskSpec {
            properties: vec![
                ("query_timeout".into(), "60".into()),
                ("session.query_timeout".into(), "70".into()),
            ],
            ..cur.clone()
        };
        let p = alter_plan(&cur, &want, StateChange::Resume).unwrap();
        assert_eq!(
            p.steps,
            [
                "ALTER TASK `t` SET ('query_timeout' = '60', 'session.query_timeout' = '70')",
                "ALTER TASK `t` RESUME",
            ]
        );
        // A property taken away, the schedule or the statement: again.
        for want in [
            TaskSpec {
                properties: Vec::new(),
                ..cur.clone()
            },
            TaskSpec {
                every: Some(Every {
                    start: None,
                    n: 2,
                    unit: "DAY".into(),
                }),
                ..cur.clone()
            },
            TaskSpec {
                definition: "INSERT INTO dst SELECT 1".into(),
                ..cur.clone()
            },
            TaskSpec {
                database: "db2".into(),
                ..cur.clone()
            },
        ] {
            let p = alter_plan(&cur, &want, StateChange::Keep).unwrap();
            assert_eq!(p.steps[0], "DROP TASK `t`");
            assert_eq!(p.steps[1], format!("USE `{}`", want.database));
            assert!(p.steps[2].starts_with("SUBMIT TASK"));
            assert_eq!(p.undo_from, Some(0));
            assert!(p.undo[1].contains("EVERY(INTERVAL 1 HOUR)"));
            assert!(p
                .head
                .iter()
                .any(|h| h.contains("la borra y la vuelve a crear")));
        }
        // Renamed: the new id; once, it runs again.
        let want = TaskSpec {
            name: "u".into(),
            every: None,
            ..cur.clone()
        };
        let p = alter_plan(&cur, &want, StateChange::Suspend).unwrap();
        assert_eq!(p.id.as_deref(), Some("task:`u`"));
        assert!(!p.steps.iter().any(|s| s.contains("SUSPEND")));
        assert!(p.head.iter().any(|h| h.contains("la ejecuta otra vez")));
        assert_eq!(p.undo_from, Some(0), "the old one had a schedule");
        // A task without a schedule already ran: a failed change leaves it
        // dropped rather than run it again.
        let once = TaskSpec {
            every: None,
            ..cur.clone()
        };
        for want in [
            TaskSpec {
                definition: "INSERT INTO dst SELECT 1".into(),
                ..once.clone()
            },
            cur.clone(),
        ] {
            let p = alter_plan(&once, &want, StateChange::Keep).unwrap();
            assert_eq!(p.steps[0], "DROP TASK `t`");
            assert!(p.undo_from.is_none() && p.undo.is_empty(), "{:?}", p.undo);
            assert!(
                p.head.iter().any(|h| h.contains("queda borrada")),
                "{:?}",
                p.head
            );
            assert!(!p.head.iter().any(|h| h.contains("DBine vuelve a crear")));
        }
    }

    #[test]
    fn a_task_reads_back_from_the_catalog() {
        let t = Task {
            name: "t".into(),
            schedule: Some("PERIODICAL START(2030-01-01T00:00) EVERY(1 HOURS)".into()),
            database: Some("db".into()),
            definition: Some("INSERT INTO dst SELECT * FROM src;".into()),
            properties: Some("('query_timeout'='120','warehouse'='default_warehouse')".into()),
            ..Task::default()
        };
        assert_eq!(spec_of(&t), task());
        assert_eq!(
            trigger_of(&t),
            Trigger::Every(Every {
                start: Some("2030-01-01 00:00:00".into()),
                n: 1,
                unit: "HOUR".into()
            })
        );
        let ev = Task {
            schedule: Some("EVENT_TRIGGERED".into()),
            ..Task::default()
        };
        assert_eq!(trigger_of(&ev), Trigger::Async);
        let manual = Task {
            schedule: Some("MANUAL".into()),
            ..Task::default()
        };
        assert_eq!(trigger_of(&manual), Trigger::Manual);
        assert!(spec_of(&manual).every.is_none());
    }

    #[test]
    fn the_task_form_is_checked() {
        let dbs = vec![("db".to_string(), "db".to_string())];
        let base = json!({
            "name": "t", "database": "db", "type": "every", "every": 1, "unit": "HOUR",
            "starts": "2030-01-01T00:00:00", "definition": "INSERT INTO dst SELECT * FROM src;",
            "state": "keep"
        });
        let v = with_props(fv(base.clone()), &[("query_timeout", "120")]);
        let (t, change) = task_spec_from(&v, &dbs).unwrap();
        assert_eq!(t, task());
        assert_eq!(change, StateChange::Keep);
        for (k, bad, says) in [
            ("unit", json!("WEEK"), "no es una opción válida"),
            ("every", json!(-1), "entre 1"),
            ("definition", json!(""), "necesita una instrucción"),
            ("database", json!("nope"), "No existe la base"),
            ("database", json!(""), "obligatorio"),
            ("state", json!("pause"), "no es una opción válida"),
        ] {
            let mut v = base.clone();
            v[k] = bad;
            let m = err(task_spec_from(&fv(v), &dbs));
            assert!(m.contains(says), "{k}: {m}");
        }
        let v = with_props(fv(base.clone()), &[("warehouse", "w")]);
        assert!(err(task_spec_from(&v, &dbs)).contains("warehouse"));
        // One statement: StarRocks would run what follows a `;` at once.
        let mut v = base.clone();
        v["definition"] = json!("INSERT INTO dst SELECT * FROM src; TRUNCATE TABLE dst");
        assert!(err(task_spec_from(&fv(v), &dbs)).contains("tiene que ser una sola"));
        let mut v = base;
        v["definition"] = json!("INSERT INTO dst SELECT ';' -- x;\n FROM src;");
        assert!(task_spec_from(&fv(v), &dbs).is_ok());
    }

    #[test]
    fn a_views_refresh_changes_in_place() {
        let create = "CREATE MATERIALIZED VIEW `v` (`k`)\nDISTRIBUTED BY HASH(`k`) BUCKETS 1 \nREFRESH MANUAL\nPROPERTIES (\n\"replicated_storage\" = \"true\",\n\"replication_num\" = \"1\",\n\"session.query_timeout\" = \"500\",\n\"excluded_trigger_tables\" = \"a,b\",\n\"mv_rewrite_staleness_second\" = \"30\"\n)\nAS SELECT k FROM src WHERE x = ')';";
        assert_eq!(
            view_properties(create),
            [
                ("session.query_timeout".to_string(), "500".to_string()),
                ("excluded_trigger_tables".to_string(), "a,b".to_string()),
                ("mv_rewrite_staleness_second".to_string(), "30".to_string()),
            ]
        );
        assert!(view_properties("CREATE MATERIALIZED VIEW v AS SELECT 1").is_empty());
        assert!(refresh_key("SESSION.query_timeout") && !refresh_key("replication_num"));
        let cur = MvSpec {
            trigger: Trigger::Manual,
            active: true,
            properties: vec![("session.query_timeout".into(), "500".into())],
        };
        assert!(mv_alter_plan("`d`.`v`", "d.v", &cur, &cur)
            .unwrap()
            .is_none());
        let want = MvSpec {
            trigger: Trigger::Every(Every {
                start: Some("2030-01-01 00:00:00".into()),
                n: 30,
                unit: "SECOND".into(),
            }),
            active: false,
            properties: vec![
                ("session.query_timeout".into(), "600".into()),
                ("partition_refresh_number".into(), "2".into()),
            ],
        };
        let p = mv_alter_plan("`d`.`v`", "d.v", &cur, &want)
            .unwrap()
            .unwrap();
        assert_eq!(
            p.steps,
            [
                "ALTER MATERIALIZED VIEW `d`.`v` SET ('session.query_timeout' = '600', 'partition_refresh_number' = '2')",
                "ALTER MATERIALIZED VIEW `d`.`v` REFRESH ASYNC START('2030-01-01 00:00:00') EVERY(INTERVAL 30 SECOND)",
                "ALTER MATERIALIZED VIEW `d`.`v` INACTIVE",
            ]
        );
        let back = MvSpec {
            trigger: Trigger::Async,
            ..cur.clone()
        };
        assert_eq!(
            mv_alter_plan("`d`.`v`", "d.v", &cur, &back)
                .unwrap()
                .unwrap()
                .steps,
            ["ALTER MATERIALIZED VIEW `d`.`v` REFRESH ASYNC"]
        );
        let gone = MvSpec {
            properties: Vec::new(),
            ..cur.clone()
        };
        assert!(
            err(mv_alter_plan("`d`.`v`", "d.v", &cur, &gone)).contains("no quita una propiedad")
        );
        let v = fv(json!({"trigger": "SCHEDULE", "every": 1, "unit": "WEEK"}));
        assert!(mv_spec_from(&v).is_err());
        let v = fv(json!({"trigger": "ASYNC", "active": false}));
        let s = mv_spec_from(&v).unwrap();
        assert_eq!((s.trigger, s.active), (Trigger::Async, false));
    }

    #[test]
    fn the_form_has_every_part_of_a_task() {
        let keys: Vec<String> = task_sections(true, &[])
            .iter()
            .flat_map(|s| s.fields.iter().map(|f| f.key.clone()))
            .collect();
        for k in [
            "name",
            "database",
            "type",
            "starts",
            "every",
            "unit",
            "state",
            "definition",
            "creator",
            "expire",
            "warehouse",
        ] {
            assert!(keys.contains(&k.to_string()), "{k}");
        }
    }
}
