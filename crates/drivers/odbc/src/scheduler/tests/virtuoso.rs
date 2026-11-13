//! OpenLink Virtuoso: DB.DBA.SYS_SCHEDULED_EVENT.

use super::*;
use crate::scheduler::virtuoso as e;

const COLS: &[&str] = &[
    "SE_NAME",
    "SE_START",
    "SE_INTERVAL",
    "SE_LAST_COMPLETED",
    "SE_SQL",
    "SE_LAST_ERROR",
    "SE_ENABLE_NOTIFY",
    "SE_NOTIFY",
    "SE_DISABLED",
];

/// Event `nightly` every 1440 minutes; `taken` exists too.
fn server() -> Fake {
    Fake::default()
        .rows_with(
            "WHERE SE_NAME = ?",
            "nightly",
            COLS,
            vec![vec![
                Some("nightly"),
                Some("2026-11-12 03:00:00"),
                Some("1440"),
                Some("2026-11-12 03:00:05"),
                Some("DB.DBA.cleanup ()"),
                None,
                Some("0"),
                None,
                Some("0"),
            ]],
        )
        .rows_with(
            "WHERE SE_NAME = ?",
            "taken",
            COLS,
            vec![vec![
                Some("taken"),
                None,
                Some("5"),
                None,
                Some("select 1"),
                None,
                None,
                None,
                None,
            ]],
        )
        .rows("SchedulerInterval", &["SI"], vec![vec![Some("10")]])
}

#[test]
fn a_new_event_is_one_insert_in_a_transaction() {
    let v = fv(
        json!({"name": "it's", "start": "2026-11-20T00:00:00", "interval": 60,
               "sql": "DB.DBA.x ('a\\b')", "enabled": false, "notify": true, "notify_to": "dba@example.com"}),
        json!({}),
    );
    assert_eq!(
        script(preset("virtuoso"), &mut server(), "job", None, &v).unwrap(),
        "-- Evento programado «it's» de Virtuoso
INSERT INTO DB.DBA.SYS_SCHEDULED_EVENT (SE_NAME, SE_START, SE_INTERVAL, SE_SQL, SE_DISABLED, SE_ENABLE_NOTIFY, SE_NOTIFY)
VALUES ('it''s', CAST('2026-11-20 00:00:00' AS DATETIME), 60, 'DB.DBA.x (''a\\\\b'')', 1, 1, 'dba@example.com');
"
    );
    let mut f = server();
    let id = save(preset("virtuoso"), &mut f, "job", None, &v).unwrap();
    assert_eq!(id.as_deref(), Some("it's"));
    assert_eq!(f.tx, ["begin", "commit"]);
    let v = fv(
        json!({"name": "later", "interval": 5, "sql": "select 1"}),
        json!({}),
    );
    assert!(script(preset("virtuoso"), &mut server(), "job", None, &v)
        .unwrap()
        .contains("VALUES ('later', now(), 5, 'select 1', 0, 0, NULL)"));
}

#[test]
fn an_edit_updates_only_what_changed() {
    let f = form(
        preset("virtuoso"),
        &mut server(),
        &ctx(),
        "job",
        Some("nightly"),
    )
    .unwrap();
    well_formed(&f);
    assert_eq!(val(&f, "interval"), 1440);
    assert_eq!(val(&f, "start"), "2026-11-12T03:00:00");
    assert_eq!(val(&f, "last_completed"), "2026-11-12 03:00:05");
    assert_eq!(
        script(
            preset("virtuoso"),
            &mut server(),
            "job",
            Some("nightly"),
            &edit(&f, json!({}))
        )
        .unwrap(),
        "-- «nightly»: no hay cambios que guardar.\n"
    );
    let v = edit(
        &f,
        json!({"name": "nightly2", "interval": 30, "enabled": false}),
    );
    assert_eq!(
        script(
            preset("virtuoso"),
            &mut server(),
            "job",
            Some("nightly"),
            &v
        )
        .unwrap(),
        "-- Evento programado «nightly» de Virtuoso
UPDATE DB.DBA.SYS_SCHEDULED_EVENT
   SET SE_NAME = 'nightly2',
       SE_INTERVAL = 30,
       SE_DISABLED = 1
 WHERE SE_NAME = 'nightly';
"
    );
    let mut fk = server();
    assert_eq!(
        save(preset("virtuoso"), &mut fk, "job", Some("nightly"), &v)
            .unwrap()
            .as_deref(),
        Some("nightly2")
    );
    let m = refused(script(
        preset("virtuoso"),
        &mut server(),
        "job",
        Some("nightly"),
        &edit(&f, json!({"name": "taken"})),
    ));
    assert!(m.contains("Ya hay"), "{m}");
}

#[test]
fn checks_delete_and_the_rest() {
    for (over, want) in [
        (json!({"notify": true, "notify_to": null}), "Falta «Correo»"),
        (json!({"notify_to": "nobody"}), "dirección de correo"),
        (json!({"interval": -1}), "«Cada (minutos)»"),
        (json!({"sql": "  "}), "Falta «SQL»."),
        (json!({"start": "ayer"}), "«Inicio»"),
    ] {
        let mut v = json!({"name": "n", "interval": 5, "sql": "select 1"});
        for (k, x) in over.as_object().unwrap() {
            v[k] = x.clone();
        }
        let m = refused(e::from_values(&fv(v, json!({}))));
        assert!(m.contains(want), "{over}: {m}");
    }
    let m = refused(script(
        preset("virtuoso"),
        &mut server(),
        "job",
        None,
        &fv(
            json!({"name": "taken", "interval": 5, "sql": "select 1"}),
            json!({}),
        ),
    ));
    assert!(m.contains("Ya hay"), "{m}");
    assert_eq!(
        delete_script(preset("virtuoso"), &mut server(), "job", "nightly").unwrap(),
        "-- Borra el evento programado «nightly» de Virtuoso\nDELETE FROM DB.DBA.SYS_SCHEDULED_EVENT WHERE SE_NAME = 'nightly';\n"
    );
    let i = info(preset("virtuoso"), &mut server(), &ctx()).unwrap();
    assert_eq!(i.kinds[0].key, "job");
    assert!(i.notes.is_empty());
    let mut off = Fake::default().rows("SchedulerInterval", &["SI"], vec![vec![Some("0")]]);
    let i = info(preset("virtuoso"), &mut off, &ctx()).unwrap();
    assert!(i.notes[0].contains("SchedulerInterval"));
    assert!(matches!(
        clear_history(preset("virtuoso"), &mut server(), "nightly"),
        Err(Error::Unsupported(_))
    ));
    assert!(refused(form(
        preset("virtuoso"),
        &mut server(),
        &ctx(),
        "job",
        Some("missing")
    ))
    .contains("no existe"));
}
