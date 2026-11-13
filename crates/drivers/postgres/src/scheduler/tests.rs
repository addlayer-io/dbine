//! Unit tests of the scheduler's scripts, plans and forms (no server).

use super::cron::{self, Caps as CronCaps, Flavor, Job as CronJob};
use super::pgagent::{self, Exception, Job as AgentJob, Schedule, Step};
use super::timescale::{self, Job as TsJob, Policy};
use super::*;
use dbine_driver::{FormList, SECRET_MASK};
use serde_json::json;

const V: Variant = Variant::Postgres;

fn map(v: Value) -> BTreeMap<String, Value> {
    serde_json::from_value(v).unwrap()
}

fn fv(values: Value, lists: Value) -> FormValues {
    FormValues {
        values: map(values),
        lists: serde_json::from_value(lists).unwrap(),
    }
}

// ---------------------------------------------------------------------------
// Common
// ---------------------------------------------------------------------------

#[test]
fn kinds_parse_and_unknown_ones_are_refused() {
    assert_eq!(Kind::parse("job").unwrap(), Kind::Job);
    assert_eq!(Kind::parse(" job_class ").unwrap(), Kind::JobClass);
    for p in Policy::ALL {
        assert_eq!(Kind::parse(p.key()).unwrap(), Kind::Policy(p));
        assert_eq!(p.kind().key, p.key());
    }
    let e = Kind::parse("operator'; DROP TABLE x").unwrap_err();
    assert!(e.to_string().contains("no es un tipo"), "{e}");
    for s in Source::ALL {
        assert_eq!(Source::parse(s.key()), Some(s));
        // The source's key is its jobs' id prefix, where it has its own.
        let id = match s {
            Source::Cron => JobId::Cron(1),
            Source::Task => JobId::Task(1),
            Source::Agent => JobId::Agent(1),
            Source::Timescale => JobId::Timescale(1),
            Source::CrdbFeed => JobId::CrdbJob(1),
            Source::GaussJob => JobId::GaussJob(1),
            Source::EdbJob => JobId::EdbJob(1),
            Source::EdbScheduler => JobId::EdbScheduler(Named {
                owner: None,
                name: "j".into(),
            }),
            Source::CrdbBackup | Source::CrdbChangefeed | Source::KdbJob | Source::KdbScheduler => {
                continue
            }
        };
        assert!(id.to_string().starts_with(&format!("{}:", s.key())));
    }
    assert_eq!(Source::parse("other"), None);
    for k in ["ttl", "program", "schedule"] {
        assert!(Kind::parse(k).is_ok(), "{k}");
    }
}

#[test]
fn only_the_variants_with_these_schedulers_say_so() {
    for v in Variant::ALL {
        let expected = crate::jobs::supported(v);
        assert_eq!(supported(v), expected, "{v:?}");
        assert_eq!(crate::design::capabilities(v).scheduler, expected, "{v:?}");
    }
    assert!(check(Variant::Redshift).is_err());
    assert!(check(Variant::Cockroach).is_ok());
    assert!(check(Variant::Yugabyte).is_ok());
}

#[test]
fn dates_and_times_are_checked() {
    for ok in [
        "2026-11-12T03:00:00",
        "2026-11-12 03:00",
        "2026-11-12T03:00:00Z",
        "2026-11-12T03:00:00-03:00",
        "2026-11-12T03:00:00+0530",
        "2024-02-29T00:00:00.123Z",
    ] {
        assert!(valid_datetime(ok), "{ok}");
    }
    for bad in [
        "",
        "2026-11-12",
        "2026-13-01T00:00:00",
        "2025-02-29T00:00:00",
        "2026-11-12T24:00:00",
        "2026-11-12T03:00:00+16:00",
        "2026-11-12T03:00:00'; DROP TABLE x",
        "now()",
    ] {
        assert!(!valid_datetime(bad), "{bad}");
    }
    assert!(valid_date("2000-02-29") && !valid_date("1900-02-29") && !valid_date("2026-1-1"));
    assert!(
        valid_time("23:59")
            && valid_time("00:00:59")
            && !valid_time("7:00")
            && !valid_time("12:60")
    );
    let v = map(json!({"t": "2026-02-30T00:00:00"}));
    let e = datetime_field(Vals(&v), "t", "Inicio").unwrap_err();
    assert!(e.to_string().contains("«Inicio»"), "{e}");
}

#[test]
fn values_read_as_the_fields_expect() {
    let m = map(json!({
        "a": " x ", "n": "12", "f": 1.0, "b": "true", "bn": 0, "list": ["1", 2, null, ""],
        "s": "", "secret": "pw", "z": null
    }));
    let v = Vals(&m);
    assert_eq!(v.text("a"), "x");
    assert_eq!(v.raw("a"), " x ");
    assert_eq!(v.opt("s"), None);
    assert_eq!(v.int("n", "N", 0, 20).unwrap(), Some(12));
    assert_eq!(v.int("f", "F", 0, 20).unwrap(), Some(1));
    assert!(v.int("n", "N", 0, 5).is_err());
    assert!(v.bool("b") && !v.bool("bn") && v.bool_or("missing", true) && !v.bool_or("z", false));
    assert_eq!(v.strings("list"), vec!["1", "2"]);
    assert_eq!(v.secret("secret").as_deref(), Some("pw"));
    assert_eq!(v.secret("s"), None);
    assert_eq!(v.secret("z"), None);
    assert!(name_of(v, "s", "Nombre").is_err());
    let nl = map(json!({"n": "a\nb"}));
    assert!(name_of(Vals(&nl), "n", "Nombre").is_err());
}

#[test]
fn a_script_runs_in_one_transaction() {
    let s = transaction("uno\ndos\u{0}", &["SELECT 1".into(), "SELECT 2;".into()]);
    assert_eq!(
        s,
        "-- uno\n-- dos?\nBEGIN;\nSELECT 1;\nSELECT 2;\nCOMMIT;\n"
    );
}

#[test]
fn several_schedulers_make_one_form_with_a_picker() {
    let cron = ObjectForm {
        sections: cron::fields(Flavor::Cron, &cron_caps(), None),
        ..ObjectForm::default()
    };
    let cron_name = cron.sections[0]
        .fields
        .iter()
        .find(|f| f.key == "name")
        .unwrap()
        .clone();
    let agent = ObjectForm {
        sections: vec![
            FormSection::new("general", "General").fields(vec![
                cron_name,
                FormField::new(
                    "job_class",
                    "Clase",
                    FormKind::reference("job_class", false),
                ),
            ]),
            FormSection::new("steps", "Pasos").list(FormList::new(
                "steps",
                "Pasos",
                "name",
                pgagent::step_fields("postgres"),
            )),
        ],
        values: map(json!({"job_class": "Routine Maintenance"})),
        ..ObjectForm::default()
    };
    let f = combine(vec![(Source::Cron, cron), (Source::Agent, agent)]);
    let general = &f.sections[0];
    assert_eq!(general.key, "general");
    assert_eq!(general.fields[0].key, "source");
    let field = |k: &str| {
        f.sections
            .iter()
            .flat_map(|s| s.fields.iter())
            .find(|x| x.key == k)
            .unwrap_or_else(|| panic!("{k}"))
            .clone()
    };
    // Both have the same name field: one, always shown.
    assert_eq!(field("name").when, None);
    assert_eq!(
        field("schedule").when,
        Some(("source".to_string(), vec!["pg_cron".to_string()]))
    );
    assert_eq!(
        field("job_class").when,
        Some(("source".to_string(), vec!["pgagent".to_string()]))
    );
    // Each key once.
    let keys: Vec<&str> = f
        .sections
        .iter()
        .flat_map(|s| s.fields.iter())
        .map(|x| x.key.as_str())
        .collect();
    let mut dedup = keys.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(keys.len(), dedup.len(), "{keys:?}");
    assert!(f
        .sections
        .iter()
        .any(|s| s.lists.iter().any(|l| l.key == "steps")));
    assert_eq!(f.values["source"], json!("pg_cron"));
    assert_eq!(f.values["job_class"], json!("Routine Maintenance"));
    assert!(f.notes.iter().any(|n| n.contains("pgAgent")));
    // Lists of pgAgent with another scheduler picked are refused.
    let v = fv(json!({}), json!({"steps": [{"name": "a"}]}));
    assert!(pgagent::no_lists(&v).is_err());
    assert!(pgagent::no_lists(&fv(json!({}), json!({"steps": []}))).is_ok());
}

/// The combined form's fields of `key` (its own keys included).
fn fields_named<'a>(f: &'a ObjectForm, key: &str) -> Vec<&'a FormField> {
    f.sections
        .iter()
        .flat_map(|s| s.fields.iter())
        .filter(|x| x.key == key)
        .collect()
}

#[test]
fn fields_that_differ_between_schedulers_stay_each_ones() {
    // pg_cron's time zone is cron.timezone, read-only; TimescaleDB's is the
    // job's own.
    let cron = ObjectForm {
        sections: cron::fields(Flavor::Cron, &cron_caps(), None),
        values: map(
            json!({"timezone": "GMT", "database": "postgres", "username": "postgres", "active": true}),
        ),
        ..ObjectForm::default()
    };
    let ts = timescale::blank_job_form(&timescale::caps_for_tests());
    let f = combine(vec![(Source::Cron, cron), (Source::Timescale, ts)]);
    let when = |k: &str| fields_named(&f, k)[0].when.clone();
    let only = |s: &str| Some(("source".to_string(), vec![s.to_string()]));
    let tz = fields_named(&f, "timezone");
    assert_eq!(tz.len(), 1);
    assert!(tz[0].read_only);
    assert_eq!(when("timezone"), only("pg_cron"));
    let own = fields_named(&f, "timescaledb.timezone");
    assert_eq!(own.len(), 1, "{:?}", f.sections);
    assert!(!own[0].read_only);
    assert_eq!(when("timescaledb.timezone"), only("timescaledb"));
    assert_eq!(f.values["timezone"], json!("GMT"));
    // Its own keys start with a value, so the designer sends them back.
    assert_eq!(f.values["timescaledb.timezone"], Value::Null);
    assert_eq!(f.values["timescaledb.name"], Value::Null);
    // TimescaleDB picked, its time zone left untouched: none (UTC), not
    // pg_cron's.
    let mut untouched = f.values.clone();
    untouched.insert("source".into(), json!("timescaledb"));
    let v = picked_values(
        &FormValues {
            values: untouched,
            lists: BTreeMap::new(),
        },
        Source::Timescale,
    );
    assert_eq!(Vals(&v.values).opt("timezone"), None);
    // The names differ too (pg_cron's is required): each its own.
    assert_eq!(when("name"), only("pg_cron"));
    assert_eq!(when("timescaledb.name"), only("timescaledb"));
    // Every key once.
    let mut keys: Vec<&str> = f
        .sections
        .iter()
        .flat_map(|s| s.fields.iter())
        .map(|x| x.key.as_str())
        .collect();
    let n = keys.len();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), n);
    // What the designer sends with TimescaleDB picked: its own time zone,
    // not pg_cron's.
    let mut sent = f.values.clone();
    sent.insert("source".into(), json!("timescaledb"));
    sent.insert("timescaledb.timezone".into(), json!("Europe/Madrid"));
    sent.insert("timescaledb.name".into(), json!("ts job"));
    sent.insert("name".into(), json!("cron job"));
    let v = picked_values(
        &FormValues {
            values: sent.clone(),
            lists: BTreeMap::new(),
        },
        Source::Timescale,
    );
    assert_eq!(v.values["timezone"], json!("Europe/Madrid"));
    assert_eq!(v.values["name"], json!("ts job"));
    assert!(v.values.keys().all(|k| !k.contains('.')), "{:?}", v.values);
    // Left empty: no time zone at all (UTC), not pg_cron's.
    sent.insert("timescaledb.timezone".into(), json!(""));
    let v = picked_values(
        &FormValues {
            values: sent.clone(),
            lists: BTreeMap::new(),
        },
        Source::Timescale,
    );
    assert_eq!(Vals(&v.values).opt("timezone"), None);
    // pg_cron picked: its own values, TimescaleDB's left out.
    let v = picked_values(
        &FormValues {
            values: sent,
            lists: BTreeMap::new(),
        },
        Source::Cron,
    );
    assert_eq!(v.values["timezone"], json!("GMT"));
    assert_eq!(v.values["name"], json!("cron job"));
    assert!(!v.values.contains_key("timescaledb.timezone"));
}

#[test]
fn a_when_follows_its_field_when_renamed() {
    let a = ObjectForm {
        sections: vec![
            FormSection::new("general", "General").fields(vec![FormField::new(
                "mode",
                "Modo",
                FormKind::select(&[("x", "X")]),
            )]),
        ],
        values: map(json!({"mode": "x"})),
        ..ObjectForm::default()
    };
    let b = ObjectForm {
        sections: vec![FormSection::new("general", "General").fields(vec![
            FormField::new("mode", "Modo", FormKind::select(&[("y", "Y"), ("z", "Z")])),
            FormField::new("detail", "Detalle", FormKind::Text).when("mode", &["z"]),
        ])],
        values: map(json!({"mode": "y"})),
        ..ObjectForm::default()
    };
    let f = combine(vec![(Source::Cron, a), (Source::Agent, b)]);
    assert_eq!(
        fields_named(&f, "detail")[0].when,
        Some(("pgagent.mode".to_string(), vec!["z".to_string()]))
    );
    assert_eq!(f.values["pgagent.mode"], json!("y"));
    assert_eq!(f.values["mode"], json!("x"));
    // A field with a default and no value starts with its default.
    let a = ObjectForm {
        sections: vec![
            FormSection::new("general", "General").fields(vec![FormField::new(
                "on",
                "Activo",
                FormKind::Bool,
            )
            .default(json!(false))]),
        ],
        ..ObjectForm::default()
    };
    let b = ObjectForm {
        sections: vec![
            FormSection::new("general", "General").fields(vec![FormField::new(
                "on",
                "Activo",
                FormKind::Bool,
            )
            .default(json!(true))]),
        ],
        ..ObjectForm::default()
    };
    let f = combine(vec![(Source::Cron, a), (Source::Agent, b)]);
    assert_eq!(f.values["pgagent.on"], json!(true));
}

// ---------------------------------------------------------------------------
// pg_cron and tasks
// ---------------------------------------------------------------------------

fn cron_caps() -> CronCaps {
    CronCaps {
        flavor: Some(Flavor::Cron),
        readable: true,
        name: true,
        active: true,
        runs: true,
        schedule: true,
        has_in_database: true,
        in_database: true,
        has_alter: true,
        alter: true,
        unschedule: true,
        rename: true,
        clear: true,
        seconds: true,
        superuser: true,
        user: "postgres".into(),
        database: "postgres".into(),
        timezone: Some("UTC".into()),
    }
}

/// A user who isn't pg_cron's owner and has no grants on its restricted
/// functions.
fn cron_plain_user() -> CronCaps {
    CronCaps {
        in_database: false,
        alter: false,
        rename: false,
        superuser: false,
        user: "ana".into(),
        ..cron_caps()
    }
}

fn task_caps() -> CronCaps {
    CronCaps {
        flavor: Some(Flavor::Task),
        seconds: true,
        user: "gpadmin".into(),
        database: "demo".into(),
        ..cron_caps()
    }
}

fn spec(
    name: &str,
    schedule: &str,
    command: &str,
    db: &str,
    user: &str,
    active: bool,
) -> cron::Spec {
    cron::Spec {
        name: Some(name.into()),
        schedule: schedule.into(),
        command: command.into(),
        database: db.into(),
        username: user.into(),
        active,
    }
}

#[test]
fn cron_schedules_are_checked() {
    assert_eq!(
        cron::check_schedule(" 0  3 * * * ", true).unwrap(),
        "0 3 * * *"
    );
    assert_eq!(
        cron::check_schedule("*/5 1-3,7 L * MON-FRI", true).unwrap(),
        "*/5 1-3,7 L * MON-FRI"
    );
    assert_eq!(
        cron::check_schedule("0 0 $ * *", true).unwrap(),
        "0 0 $ * *"
    );
    assert_eq!(cron::check_schedule("@daily", false).unwrap(), "@daily");
    assert_eq!(
        cron::check_schedule("30 seconds", true).unwrap(),
        "30 seconds"
    );
    assert_eq!(cron::check_schedule("1 second", true).unwrap(), "1 seconds");
    for bad in [
        "",
        "* * * *",
        "* * * * * *",
        "0 3 * * *;",
        "0 3 * * * '",
        "60 seconds",
        "0 seconds",
        "@",
    ] {
        assert!(cron::check_schedule(bad, true).is_err(), "{bad}");
    }
    let e = cron::check_schedule("10 seconds", false).unwrap_err();
    assert!(e.to_string().contains("1.5"), "{e}");
}

#[test]
fn a_cron_job_is_scheduled_with_everything_it_asks_for() {
    let j = spec(
        "dbine's job",
        "0 3 * * *",
        "VACUUM; SELECT '$cmd$'",
        "app",
        "bob",
        false,
    );
    let s = cron::cron_create(V, &cron_caps(), &j).unwrap();
    assert!(
        s.starts_with("-- pg_cron: nuevo job «dbine's job»\nBEGIN;\n"),
        "{s}"
    );
    assert!(
        s.contains("SELECT cron.schedule_in_database(job_name := E'dbine''s job', schedule := E'0 3 * * *', command := $cmd1$VACUUM; SELECT '$cmd$'$cmd1$, database := E'app', username := E'bob', active := false) AS jobid;"),
        "{s}"
    );
    assert!(s.ends_with("COMMIT;\n"));
    // Active: no `active` argument.
    let s = cron::cron_create(
        V,
        &cron_caps(),
        &spec("j", "@hourly", "SELECT 1", "postgres", "postgres", true),
    )
    .unwrap();
    assert!(!s.contains("active :="), "{s}");
}

#[test]
fn without_schedule_in_database_a_job_is_this_users_in_this_database() {
    let caps = CronCaps {
        database: "postgres".into(),
        ..cron_plain_user()
    };
    let s = cron::cron_create(
        V,
        &caps,
        &spec("j", "0 3 * * *", "SELECT 1", "postgres", "ana", true),
    )
    .unwrap();
    assert!(s.contains("SELECT cron.schedule(job_name := E'j', schedule := E'0 3 * * *', command := $cmd$SELECT 1$cmd$) AS jobid;"), "{s}");
    for (db, user, active, msg) in [
        ("app", "ana", true, "otra base"),
        ("postgres", "bob", true, "otro usuario"),
        ("postgres", "ana", false, "inactivo"),
    ] {
        let e = cron::cron_create(
            V,
            &caps,
            &spec("j", "0 3 * * *", "SELECT 1", db, user, active),
        )
        .unwrap_err();
        assert!(e.to_string().contains(msg), "{e}");
    }
    // pg_cron before 1.3: no names.
    let old = CronCaps {
        name: false,
        ..caps.clone()
    };
    let unnamed = cron::Spec {
        name: None,
        ..spec("x", "0 3 * * *", "SELECT 1", "postgres", "ana", true)
    };
    let s = cron::cron_create(V, &old, &unnamed).unwrap();
    assert!(s.contains("SELECT cron.schedule(schedule := E'0 3 * * *', command := $cmd$SELECT 1$cmd$) AS jobid;"), "{s}");
}

fn cron_job() -> CronJob {
    cron::job_for_tests(
        12,
        Some("nightly"),
        "0 3 * * *",
        "VACUUM",
        "app",
        "postgres",
        true,
    )
}

#[test]
fn a_cron_job_changes_only_what_differs() {
    let cur = cron_job();
    let same = spec("nightly", "0 3 * * *", "VACUUM", "app", "postgres", true);
    assert_eq!(
        cron::cron_alter(V, &cron_caps(), &cur, &same).unwrap(),
        None
    );
    let new = spec("nightly", "0 4 * * *", "VACUUM", "app", "postgres", true);
    let s = cron::cron_alter(V, &cron_caps(), &cur, &new)
        .unwrap()
        .unwrap();
    assert!(
        s.contains("SELECT cron.alter_job(job_id := 12, schedule := E'0 4 * * *');"),
        "{s}"
    );
    let new = spec(
        "nightly",
        "0 3 * * *",
        "VACUUM FULL",
        "postgres",
        "bob",
        false,
    );
    let s = cron::cron_alter(V, &cron_caps(), &cur, &new)
        .unwrap()
        .unwrap();
    assert!(
        s.contains("SELECT cron.alter_job(job_id := 12, command := $cmd$VACUUM FULL$cmd$, database := E'postgres', username := E'bob', active := false);"),
        "{s}"
    );
    // A new name, in place where the user may.
    let new = spec(
        "nightly 'b'",
        "0 3 * * *",
        "VACUUM",
        "app",
        "postgres",
        true,
    );
    let s = cron::cron_alter(V, &cron_caps(), &cur, &new)
        .unwrap()
        .unwrap();
    assert!(
        s.contains("UPDATE cron.job SET jobname = E'nightly ''b''' WHERE jobid = 12;"),
        "{s}"
    );
    assert!(!s.contains("alter_job"), "{s}");
}

#[test]
fn a_cron_rename_without_rights_schedules_again() {
    let caps = CronCaps {
        rename: false,
        ..cron_caps()
    };
    let new = spec("renamed", "0 3 * * *", "VACUUM", "app", "postgres", true);
    let s = cron::cron_alter(V, &caps, &cron_job(), &new)
        .unwrap()
        .unwrap();
    assert!(
        s.starts_with("-- pg_cron: «nightly» pasa a llamarse «renamed»\n-- pg_cron no permite"),
        "{s}"
    );
    let schedule = s
        .find("cron.schedule_in_database(job_name := E'renamed'")
        .unwrap();
    let unschedule = s.find("SELECT cron.unschedule(12);").unwrap();
    assert!(schedule < unschedule, "{s}");
}

#[test]
fn without_alter_job_the_job_is_rescheduled_by_name() {
    let caps = cron_plain_user();
    let cur = cron::job_for_tests(
        7,
        Some("mine"),
        "0 3 * * *",
        "SELECT 1",
        "postgres",
        "ana",
        true,
    );
    let new = spec("mine", "*/10 * * * *", "SELECT 2", "postgres", "ana", true);
    let s = cron::cron_alter(V, &caps, &cur, &new).unwrap().unwrap();
    assert!(s.contains("SELECT cron.schedule(job_name := E'mine', schedule := E'*/10 * * * *', command := $cmd$SELECT 2$cmd$);"), "{s}");
    let inactive = spec("mine", "0 3 * * *", "SELECT 1", "postgres", "ana", false);
    let e = cron::cron_alter(V, &caps, &cur, &inactive).unwrap_err();
    assert!(e.to_string().contains("cron.alter_job"), "{e}");
    let unnamed = cron::job_for_tests(8, None, "0 3 * * *", "SELECT 1", "postgres", "ana", true);
    let new = cron::Spec {
        name: None,
        ..spec("", "0 4 * * *", "SELECT 1", "postgres", "ana", true)
    };
    let old = CronCaps {
        name: false,
        ..caps
    };
    assert!(cron::cron_alter(V, &old, &unnamed, &new).is_err());
}

#[test]
fn a_cron_form_reads_back_as_no_change() {
    let caps = cron_caps();
    let cur = cron_job();
    let values = FormValues {
        values: cron::values_of(&cur),
        lists: BTreeMap::new(),
    };
    let dbs = pairs(vec!["postgres".into(), "app".into()]);
    let roles = pairs(vec!["postgres".into()]);
    let j = cron::spec(&caps, &values, &dbs, &roles, Some(&cur)).unwrap();
    assert_eq!(cron::cron_alter(V, &caps, &cur, &j).unwrap(), None);
    // A database or user the server doesn't list is refused…
    let mut bad = values.clone();
    bad.values.insert("database".into(), json!("nope'"));
    let e = cron::spec(&caps, &bad, &dbs, &roles, Some(&cur)).unwrap_err();
    assert!(e.to_string().contains("nope'"), "{e}");
    let mut bad = values.clone();
    bad.values.insert("username".into(), json!("mallory"));
    assert!(cron::spec(&caps, &bad, &dbs, &roles, Some(&cur)).is_err());
    // …but the job's own stays even when it no longer is.
    let gone = cron::job_for_tests(
        12,
        Some("nightly"),
        "0 3 * * *",
        "VACUUM",
        "olddb",
        "olduser",
        true,
    );
    let values = FormValues {
        values: cron::values_of(&gone),
        lists: BTreeMap::new(),
    };
    assert!(cron::spec(&caps, &values, &dbs, &roles, Some(&gone)).is_ok());
    // Required fields.
    let mut empty = values.clone();
    empty.values.insert("command".into(), json!("   "));
    assert!(cron::spec(&caps, &empty, &dbs, &roles, Some(&gone)).is_err());
    let mut empty = values;
    empty.values.insert("name".into(), json!(""));
    assert!(cron::spec(&caps, &empty, &dbs, &roles, Some(&gone)).is_err());
}

#[test]
fn the_cron_form_lets_set_only_what_the_user_may() {
    let field = |sections: &[FormSection], k: &str| {
        sections
            .iter()
            .flat_map(|s| s.fields.iter())
            .find(|f| f.key == k)
            .unwrap()
            .clone()
    };
    let su = cron::fields(Flavor::Cron, &cron_caps(), None);
    assert!(
        !field(&su, "database").read_only
            && !field(&su, "username").read_only
            && !field(&su, "active").read_only
    );
    let plain = cron::fields(Flavor::Cron, &cron_plain_user(), None);
    assert!(
        field(&plain, "database").read_only
            && field(&plain, "username").read_only
            && field(&plain, "active").read_only
    );
    assert!(!field(&plain, "schedule").read_only && !field(&plain, "name").read_only);
    // Existing jobs: the id and the node, read-only.
    let cur = cron_job();
    let edit = cron::fields(Flavor::Cron, &cron_caps(), Some(&cur));
    assert!(field(&edit, "jobid").read_only && field(&edit, "node").read_only);
    assert!(
        matches!(field(&edit, "command").kind, FormKind::Code { ref language } if language == "sql")
    );
    assert!(matches!(field(&edit, "schedule").kind, FormKind::Cron));
    let tasks = cron::fields(Flavor::Task, &task_caps(), None);
    assert!(!field(&tasks, "database").read_only && !field(&tasks, "active").read_only);
}

#[test]
fn a_task_is_created_with_its_database_and_user() {
    let caps = task_caps();
    let j = spec(
        "dbine task",
        "5 seconds",
        "SELECT 'x'",
        "demo",
        "gpadmin",
        true,
    );
    let s = cron::task_create(V, &caps, &j);
    assert!(
        s.contains("CREATE TASK \"dbine task\" SCHEDULE E'5 seconds' DATABASE \"demo\" USER \"gpadmin\" AS $cmd$SELECT 'x'$cmd$;"),
        "{s}"
    );
    assert!(!s.contains("NOT ACTIVE"));
    // Inactive, for another user: switched off as that user.
    let j = spec("t", "0 3 * * *", "SELECT 1", "demo", "bob\"x", false);
    let s = cron::task_create(V, &caps, &j);
    assert!(
        s.contains(
            "SET LOCAL ROLE \"bob\"\"x\";\nALTER TASK \"t\" NOT ACTIVE;\nRESET ROLE;\nCOMMIT;"
        ),
        "{s}"
    );
}

#[test]
fn a_task_changes_in_place_or_is_created_again() {
    let caps = task_caps();
    let cur = cron::job_for_tests(
        9,
        Some("t"),
        "0 3 * * *",
        "SELECT 1",
        "demo",
        "gpadmin",
        true,
    );
    assert_eq!(
        cron::task_alter(
            V,
            &caps,
            &cur,
            &spec("t", "0 3 * * *", "SELECT 1", "demo", "gpadmin", true)
        ),
        None
    );
    let s = cron::task_alter(
        V,
        &caps,
        &cur,
        &spec("t", "0 4 * * *", "SELECT 2", "postgres", "gpadmin", false),
    )
    .unwrap();
    assert!(
        s.contains("ALTER TASK \"t\" SCHEDULE E'0 4 * * *' DATABASE \"postgres\" NOT ACTIVE AS $cmd$SELECT 2$cmd$;"),
        "{s}"
    );
    // Its own user, changed by a superuser in place.
    let s = cron::task_alter(
        V,
        &caps,
        &cur,
        &spec("t", "0 3 * * *", "SELECT 1", "demo", "bob", true),
    )
    .unwrap();
    assert!(s.contains("ALTER TASK \"t\" USER \"bob\";"), "{s}");
    // Another user's task: as that user.
    let other = cron::job_for_tests(9, Some("t"), "0 3 * * *", "SELECT 1", "demo", "bob", true);
    let s = cron::task_alter(
        V,
        &caps,
        &other,
        &spec("t", "0 5 * * *", "SELECT 1", "demo", "bob", true),
    )
    .unwrap();
    assert!(
        s.contains("SET LOCAL ROLE \"bob\";\nALTER TASK \"t\" SCHEDULE E'0 5 * * *';\nRESET ROLE;"),
        "{s}"
    );
    // A new name: dropped and created again.
    let s = cron::task_alter(
        V,
        &caps,
        &cur,
        &spec("t2", "0 3 * * *", "SELECT 1", "demo", "gpadmin", true),
    )
    .unwrap();
    assert!(
        s.contains("pasa a llamarse «t2»") && s.contains("otro id, sin su historial"),
        "{s}"
    );
    let drop = s.find("DROP TASK \"t\";").unwrap();
    let create = s.find("CREATE TASK \"t2\"").unwrap();
    assert!(drop < create, "{s}");
    // Another user's task given to a third one: the same.
    let s = cron::task_alter(
        V,
        &caps,
        &other,
        &spec("t", "0 3 * * *", "SELECT 1", "demo", "carol", true),
    )
    .unwrap();
    assert!(
        s.contains("SET LOCAL ROLE \"bob\";\nDROP TASK \"t\";\nRESET ROLE;\nCREATE TASK \"t\""),
        "{s}"
    );
}

// ---------------------------------------------------------------------------
// pgAgent
// ---------------------------------------------------------------------------

#[test]
fn passwords_come_out_of_connection_strings_and_go_back_in() {
    for (conn, without, pw) in [
        (
            "host=a password=secret user=u",
            "host=a user=u",
            Some("secret"),
        ),
        (
            "host=a password='a b\\'c\\\\' user=u",
            "host=a user=u",
            Some("a b'c\\"),
        ),
        ("host=a user=u", "host=a user=u", None),
        (
            "postgresql://u:p%40ss@h:5432/db?sslmode=require",
            "postgresql://u@h:5432/db?sslmode=require",
            Some("p@ss"),
        ),
        (
            "postgres://u@h/db?password=x%2Fy&sslmode=disable",
            "postgres://u@h/db?sslmode=disable",
            Some("x/y"),
        ),
    ] {
        let (w, p) = pgagent::split_password(conn);
        assert_eq!(w, without, "{conn}");
        if let Some(pw) = pw {
            assert_eq!(p.as_deref(), Some(pw), "{conn}");
        }
    }
    let kv = pgagent::with_password("host=a user=u", Some("a'b\\c"), false);
    assert_eq!(kv, "host=a user=u password='a\\'b\\\\c'");
    assert_eq!(pgagent::split_password(&kv).1.as_deref(), Some("a'b\\c"));
    let uri = pgagent::with_password("postgresql://u@h/db", Some("p@ss w"), false);
    assert_eq!(uri, "postgresql://u@h/db?password=p%40ss%20w");
    assert_eq!(pgagent::split_password(&uri).1.as_deref(), Some("p@ss w"));
    assert_eq!(
        pgagent::with_password("host=a", Some("secret"), true),
        format!("host=a password={SECRET_MASK}")
    );
    assert_eq!(pgagent::with_password("host=a", None, false), "host=a");
}

fn agent_job() -> AgentJob {
    AgentJob {
        id: 3,
        name: "nightly".into(),
        description: "".into(),
        host_agent: "".into(),
        enabled: true,
        class_id: 1,
        class: "Routine Maintenance".into(),
        changed_at: "1794488199.123456".into(),
        steps: vec![
            Step {
                id: 10,
                name: "01 vacuum".into(),
                enabled: true,
                kind: 's',
                code: "VACUUM".into(),
                dbname: "app".into(),
                on_error: 'f',
                ..Step::default()
            },
            Step {
                id: 11,
                name: "02 remote".into(),
                enabled: true,
                kind: 's',
                code: "SELECT 1".into(),
                connstr: "host=db2 user=etl password='s3cret'".into(),
                on_error: 'i',
                ..Step::default()
            },
        ],
        schedules: vec![Schedule {
            id: 20,
            name: "daily".into(),
            enabled: true,
            start: "2026-01-01T00:00:00Z".into(),
            end: None,
            minutes: {
                let mut m = vec![false; 60];
                m[30] = true;
                m
            },
            hours: {
                let mut h = vec![false; 24];
                h[3] = true;
                h
            },
            weekdays: vec![false; 7],
            monthdays: vec![false; 32],
            months: vec![false; 12],
            description: "".into(),
        }],
        exceptions: vec![Exception {
            id: 30,
            schedule: 20,
            date: Some("2026-12-25".into()),
            time: None,
        }],
        ..AgentJob::default()
    }
}

/// The values the job's form carries.
fn agent_values(j: &AgentJob) -> FormValues {
    FormValues {
        values: map(json!({
            "name": j.name, "enabled": j.enabled, "job_class": j.class, "host_agent": j.host_agent,
            "description": j.description, "changed_at": j.changed_at
        })),
        lists: BTreeMap::from([
            (
                "steps".to_string(),
                j.steps.iter().map(pgagent::step_row).collect(),
            ),
            (
                "schedules".to_string(),
                j.schedules.iter().map(pgagent::schedule_row).collect(),
            ),
            (
                "exceptions".to_string(),
                j.exceptions
                    .iter()
                    .map(|e| pgagent::exception_row(e, &j.schedules))
                    .collect(),
            ),
        ]),
    }
}

fn classes() -> Vec<(i32, String)> {
    vec![
        (1, "Routine Maintenance".into()),
        (5, "Miscellaneous".into()),
    ]
}

fn dbs() -> Vec<(String, String)> {
    pairs(vec!["postgres".into(), "app".into()])
}

#[test]
fn a_pgagent_form_reads_back_as_no_change() {
    let j = agent_job();
    let values = agent_values(&j);
    let row = &values.lists["steps"][1];
    assert_eq!(row["kind"], json!("remote"));
    assert_eq!(
        row["connstr"],
        json!("host=db2 user=etl"),
        "no password in the form"
    );
    assert_eq!(row["password"], Value::Null);
    assert_eq!(values.lists["schedules"][0]["minutes"], json!(["30"]));
    assert_eq!(values.lists["exceptions"][0]["schedule"], json!("daily"));
    let spec = pgagent::spec(&values, Some(&j), &classes(), &dbs()).unwrap();
    assert_eq!(pgagent::alter_script(V, &j, &spec, false), None);
}

#[test]
fn a_pgagent_job_is_created_in_one_block() {
    let values = fv(
        json!({"name": "dbine 'job'", "enabled": true, "job_class": "Miscellaneous", "host_agent": "", "description": "d"}),
        json!({
            "steps": [
                {"name": "a", "kind": "sql", "dbname": "app", "code": "SELECT '$cmd$'", "on_error": "s"},
                {"name": "b", "kind": "remote", "connstr": "host=x user=u", "password": "p'w", "code": "SELECT 1"},
                {"name": "c", "kind": "batch", "code_batch": "echo hi", "enabled": false}
            ],
            "schedules": [
                {"name": "s1", "start": "2026-11-12T03:00:00Z", "minutes": ["0", "30"], "hours": ["3"],
                 "weekdays": ["1", "5"], "monthdays": ["32"], "months": ["12"]}
            ],
            "exceptions": [{"schedule": "s1", "date": "2026-12-25", "time": "10:00"}]
        }),
    );
    let j = pgagent::spec(&values, None, &classes(), &dbs()).unwrap();
    assert_eq!(j.class_id, 5);
    let shown = pgagent::create_script(V, &j, true);
    assert!(
        shown.starts_with("-- pgAgent: nuevo job «dbine 'job'»\nDO $job$"),
        "{shown}"
    );
    assert!(
        shown.contains(
            "VALUES (5, E'dbine ''job''', E'd', E'', true)\n    RETURNING jobid INTO jid;"
        ),
        "{shown}"
    );
    assert!(
        shown.contains(
            "VALUES (jid, E'a', E'', true, E's', $cmd1$SELECT '$cmd$'$cmd1$, E'', E'app', E's');"
        ),
        "{shown}"
    );
    assert!(
        shown.contains(&format!("E'host=x user=u password={SECRET_MASK}'")),
        "{shown}"
    );
    assert!(!shown.contains("p'w") && !shown.contains("p''w"), "{shown}");
    assert!(
        shown.contains("VALUES (jid, E'c', E'', false, E'b', $cmd$echo hi$cmd$, E'', E'', E'f');"),
        "{shown}"
    );
    let minutes = format!(
        "'{{{}}}'::boolean[]",
        (0..60)
            .map(|m| if m == 0 || m == 30 { "t" } else { "f" })
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(shown.contains(&minutes), "{shown}");
    let monthdays = format!(
        "'{{{}}}'::boolean[]",
        (1..=32)
            .map(|d| if d == 32 { "t" } else { "f" })
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(shown.contains(&monthdays), "{shown}");
    assert!(
        shown.contains("E'2026-11-12T03:00:00Z'::timestamptz, NULL"),
        "{shown}"
    );
    assert!(shown.contains("RETURNING jscid INTO scid;\n    INSERT INTO pgagent.pga_exception (jexscid, jexdate, jextime) VALUES (scid, E'2026-12-25'::date, E'10:00:00'::time);"), "{shown}");
    assert!(shown.trim_end().ends_with("$job$;"), "{shown}");
    let real = pgagent::create_script(V, &j, false);
    assert!(real.contains("password=''p\\\\''w''"), "{real}");
}

#[test]
fn a_pgagent_job_changes_only_what_differs() {
    let cur = agent_job();
    let mut values = agent_values(&cur);
    values.values.insert("enabled".into(), json!(false));
    values
        .values
        .insert("job_class".into(), json!("Miscellaneous"));
    // Steps: the first edited, the remote one removed, a new one.
    let steps = values.lists.get_mut("steps").unwrap();
    steps[0].insert("code".into(), json!("VACUUM ANALYZE"));
    steps.remove(1);
    steps.push(map(
        json!({"name": "03 new", "kind": "sql", "dbname": "postgres", "code": "SELECT 3"}),
    ));
    // Schedules: the one there changed, a new one with an exception.
    let schedules = values.lists.get_mut("schedules").unwrap();
    schedules[0].insert("hours".into(), json!(["4"]));
    schedules.push(map(json!({"name": "weekly", "weekdays": ["0"]})));
    // Exceptions: the one there moved to the new schedule, a new one.
    let exceptions = values.lists.get_mut("exceptions").unwrap();
    exceptions[0].insert("schedule".into(), json!("weekly"));
    exceptions.push(map(json!({"schedule": "daily", "time": "03:30:00"})));
    let spec = pgagent::spec(&values, Some(&cur), &classes(), &dbs()).unwrap();
    let s = pgagent::alter_script(V, &cur, &spec, true).unwrap();
    assert!(s.contains("UPDATE pgagent.pga_job SET jobenabled = false, jobjclid = 5, jobchanged = now() WHERE jobid = 3;"), "{s}");
    assert!(
        s.contains("DELETE FROM pgagent.pga_exception WHERE jexid = 30;"),
        "{s}"
    );
    assert!(
        s.contains("UPDATE pgagent.pga_schedule SET jschours = '{f,f,f,f,t,"),
        "{s}"
    );
    assert!(s.contains("WHERE jscid = 20 AND jscjobid = 3;"), "{s}");
    assert!(
        s.contains("VALUES (3, E'weekly', E'', true, now(), NULL,"),
        "{s}"
    );
    // The moved exception goes in with its new schedule.
    let weekly = s.find("E'weekly'").unwrap();
    let moved = s.find("VALUES (scid, E'2026-12-25'::date, NULL);").unwrap();
    assert!(weekly < moved, "{s}");
    assert!(s.contains("VALUES (20, NULL, E'03:30:00'::time);"), "{s}");
    assert!(
        s.contains("DELETE FROM pgagent.pga_jobstep WHERE jstid = 11 AND jstjobid = 3;"),
        "{s}"
    );
    assert!(s.contains("UPDATE pgagent.pga_jobstep SET jstcode = $cmd$VACUUM ANALYZE$cmd$ WHERE jstid = 10 AND jstjobid = 3;"), "{s}");
    assert!(
        s.contains(
            "VALUES (3, E'03 new', E'', true, E's', $cmd$SELECT 3$cmd$, E'', E'postgres', E'f');"
        ),
        "{s}"
    );
}

#[test]
fn a_remote_steps_password_is_kept_changed_or_removed() {
    let cur = agent_job();
    let base = agent_values(&cur);
    // Kept: the string unchanged, no update.
    let spec = pgagent::spec(&base, Some(&cur), &classes(), &dbs()).unwrap();
    assert_eq!(pgagent::alter_script(V, &cur, &spec, false), None);
    // Kept with another host: the server's password goes with it.
    let mut v = base.clone();
    v.lists.get_mut("steps").unwrap()[1].insert("connstr".into(), json!("host=db3 user=etl"));
    let spec = pgagent::spec(&v, Some(&cur), &classes(), &dbs()).unwrap();
    let real = pgagent::alter_script(V, &cur, &spec, false).unwrap();
    assert!(
        real.contains("jstconnstr = E'host=db3 user=etl password=''s3cret'''"),
        "{real}"
    );
    let shown = pgagent::alter_script(V, &cur, &spec, true).unwrap();
    assert!(
        shown.contains(&format!(
            "jstconnstr = E'host=db3 user=etl password={SECRET_MASK}'"
        )),
        "{shown}"
    );
    assert!(!shown.contains("s3cret"), "{shown}");
    // A new one.
    let mut v = base.clone();
    v.lists.get_mut("steps").unwrap()[1].insert("password".into(), json!("n3w"));
    let spec = pgagent::spec(&v, Some(&cur), &classes(), &dbs()).unwrap();
    let real = pgagent::alter_script(V, &cur, &spec, false).unwrap();
    assert!(real.contains("password=''n3w''"), "{real}");
    assert!(!pgagent::alter_script(V, &cur, &spec, true)
        .unwrap()
        .contains("n3w"));
    // Removed.
    let mut v = base;
    v.lists.get_mut("steps").unwrap()[1].insert("drop_password".into(), json!(true));
    let spec = pgagent::spec(&v, Some(&cur), &classes(), &dbs()).unwrap();
    let real = pgagent::alter_script(V, &cur, &spec, false).unwrap();
    assert!(
        real.contains("jstconnstr = E'host=db2 user=etl' WHERE jstid = 11"),
        "{real}"
    );
}

/// `agent_job` with the remote step's string as another tool wrote it.
fn agent_job_with(connstr: &str) -> AgentJob {
    let mut j = agent_job();
    j.steps[1].connstr = connstr.into();
    j
}

#[test]
fn a_connection_string_written_elsewhere_is_left_alone() {
    // pgAdmin's way: unquoted, the password in the middle.
    for stored in [
        "host=db2 password=s3cret user=etl",
        "password = s3cret host=db2 user=etl",
        "postgresql://etl:s3cret@db2/app?sslmode=require",
    ] {
        let cur = agent_job_with(stored);
        let v = agent_values(&cur);
        assert!(
            !v.lists["steps"][1]["connstr"]
                .as_str()
                .unwrap()
                .contains("s3cret"),
            "{stored}"
        );
        let spec = pgagent::spec(&v, Some(&cur), &classes(), &dbs()).unwrap();
        assert_eq!(
            pgagent::alter_script(V, &cur, &spec, false),
            None,
            "{stored}"
        );
    }
    // Changed: rewritten, the server's password kept.
    let cur = agent_job_with("host=db2 password=s3cret user=etl");
    let mut v = agent_values(&cur);
    v.lists.get_mut("steps").unwrap()[1].insert("connstr".into(), json!("host=db3 user=etl"));
    let spec = pgagent::spec(&v, Some(&cur), &classes(), &dbs()).unwrap();
    let real = pgagent::alter_script(V, &cur, &spec, false).unwrap();
    assert!(
        real.contains("jstconnstr = E'host=db3 user=etl password=''s3cret'''"),
        "{real}"
    );
}

#[test]
fn a_client_keys_passphrase_is_a_secret_too() {
    let cur =
        agent_job_with("host=db2 user=etl sslkey=/k/etl.key sslpassword=k3y password='s3cret'");
    let v = agent_values(&cur);
    let shown_conn = v.lists["steps"][1]["connstr"].as_str().unwrap().to_string();
    assert_eq!(shown_conn, "host=db2 user=etl sslkey=/k/etl.key");
    let spec = pgagent::spec(&v, Some(&cur), &classes(), &dbs()).unwrap();
    assert_eq!(pgagent::alter_script(V, &cur, &spec, false), None);
    // Another host: both secrets go back, masked in the script shown.
    let mut v2 = v.clone();
    v2.lists.get_mut("steps").unwrap()[1].insert(
        "connstr".into(),
        json!("host=db3 user=etl sslkey=/k/etl.key"),
    );
    let spec = pgagent::spec(&v2, Some(&cur), &classes(), &dbs()).unwrap();
    let real = pgagent::alter_script(V, &cur, &spec, false).unwrap();
    assert!(
        real.contains(
            "host=db3 user=etl sslkey=/k/etl.key sslpassword=''k3y'' password=''s3cret''"
        ),
        "{real}"
    );
    let shown = pgagent::alter_script(V, &cur, &spec, true).unwrap();
    assert!(
        !shown.contains("k3y") && !shown.contains("s3cret"),
        "{shown}"
    );
    assert!(
        shown.contains(&format!("sslpassword={SECRET_MASK} password={SECRET_MASK}")),
        "{shown}"
    );
    // Typed into the string: taken as typed, masked when shown.
    let mut v3 = v;
    v3.lists.get_mut("steps").unwrap()[1].insert(
        "connstr".into(),
        json!("host=db2 user=etl sslkey=/k/etl.key sslpassword=n3w"),
    );
    let spec = pgagent::spec(&v3, Some(&cur), &classes(), &dbs()).unwrap();
    let real = pgagent::alter_script(V, &cur, &spec, false).unwrap();
    assert!(real.contains("sslpassword=''n3w''"), "{real}");
    assert!(!pgagent::alter_script(V, &cur, &spec, true)
        .unwrap()
        .contains("n3w"));
    assert_eq!(
        pgagent::ssl_password("postgresql://u@h/db?sslpassword=a%20b").as_deref(),
        Some("a b")
    );
}

#[test]
fn a_pgagent_form_with_problems_is_refused() {
    let cur = agent_job();
    let base = agent_values(&cur);
    let refused = |f: &dyn Fn(&mut FormValues), needle: &str| {
        let mut v = base.clone();
        f(&mut v);
        let e = pgagent::spec(&v, Some(&cur), &classes(), &dbs()).unwrap_err();
        assert!(e.to_string().contains(needle), "{needle}: {e}");
    };
    refused(
        &|v| {
            v.values.insert("job_class".into(), json!("Nope"));
        },
        "clase",
    );
    refused(
        &|v| {
            v.lists.get_mut("steps").unwrap()[1].insert("name".into(), json!("01 vacuum"));
        },
        "dos pasos",
    );
    refused(
        &|v| {
            v.lists.get_mut("steps").unwrap()[0].insert("step_id".into(), json!(999));
        },
        "no es de este job",
    );
    refused(
        &|v| {
            v.lists.get_mut("steps").unwrap()[0].insert("step_id".into(), json!("10; DROP"));
        },
        "no es de este job",
    );
    refused(
        &|v| {
            v.lists.get_mut("steps").unwrap()[0].insert("kind".into(), json!("x"));
        },
        "tipo de paso",
    );
    refused(
        &|v| {
            v.lists.get_mut("steps").unwrap()[0].insert("code".into(), json!(" "));
        },
        "no tiene código",
    );
    refused(
        &|v| {
            v.lists.get_mut("steps").unwrap()[1].insert("connstr".into(), json!(""));
        },
        "cadena de conexión",
    );
    refused(
        &|v| {
            v.lists.get_mut("steps").unwrap()[0].insert("dbname".into(), json!("other"));
        },
        "La base «other»",
    );
    refused(
        &|v| {
            v.lists.get_mut("steps").unwrap()[0].insert("on_error".into(), json!("z"));
        },
        "acción",
    );
    refused(
        &|v| {
            v.lists.get_mut("schedules").unwrap()[0].insert("minutes".into(), json!(["60"]));
        },
        "Minutos",
    );
    refused(
        &|v| {
            v.lists.get_mut("schedules").unwrap()[0].insert("monthdays".into(), json!(["0"]));
        },
        "Días del mes",
    );
    refused(
        &|v| {
            v.lists.get_mut("schedules").unwrap()[0].insert("start".into(), json!("mañana"));
        },
        "Inicio",
    );
    refused(
        &|v| {
            v.lists.get_mut("schedules").unwrap()[0].insert("schedule_id".into(), json!(21));
        },
        "no es de este job",
    );
    refused(
        &|v| {
            v.lists
                .get_mut("schedules")
                .unwrap()
                .push(map(json!({"name": "daily"})));
        },
        "dos programaciones",
    );
    refused(
        &|v| {
            v.lists.get_mut("exceptions").unwrap()[0].insert("schedule".into(), json!("other"));
        },
        "que el job no tiene",
    );
    refused(
        &|v| {
            let e = &mut v.lists.get_mut("exceptions").unwrap()[0];
            e.insert("date".into(), json!(""));
            e.insert("time".into(), json!(""));
        },
        "ni hora",
    );
    refused(
        &|v| {
            v.lists.get_mut("exceptions").unwrap()[0].insert("date".into(), json!("2026-02-30"));
        },
        "Fecha",
    );
    refused(
        &|v| {
            v.lists.get_mut("exceptions").unwrap()[0].insert("time".into(), json!("25:00"));
        },
        "Hora",
    );
    refused(
        &|v| {
            let e = v.lists.get_mut("exceptions").unwrap();
            let dup = e[0].clone();
            let mut dup = dup;
            dup.remove("exception_id");
            e.push(dup);
        },
        "misma fecha",
    );
}

#[test]
fn the_steps_order_has_to_be_the_one_pgagent_runs() {
    let step = |name: &str| pgagent::StepSpec {
        id: None,
        name: name.into(),
        description: String::new(),
        enabled: true,
        kind: 's',
        code: "SELECT 1".into(),
        connstr: String::new(),
        password: pgagent::Password::Keep,
        ssl_password: None,
        dbname: "postgres".into(),
        on_error: 'f',
    };
    let steps = vec![step("load"), step("clean")];
    // The server sorts them "clean" (2nd listed) first.
    let m = pgagent::order_problem(&steps, &[2, 1]).unwrap();
    assert!(m.contains("«clean» iría antes que «load»"), "{m}");
    assert_eq!(pgagent::order_problem(&steps, &[1, 2]), None);
}

#[test]
fn class_ids_are_plain_numbers() {
    assert_eq!(pgagent::class_id("5").unwrap(), 5);
    for bad in ["", "0", "-1", "05", "5 ", "x", "5; DROP"] {
        assert!(pgagent::class_id(bad).is_err(), "{bad}");
    }
}

// ---------------------------------------------------------------------------
// TimescaleDB
// ---------------------------------------------------------------------------

fn ts_job() -> TsJob {
    TsJob {
        id: 1000,
        name: "dbine job".into(),
        proc_schema: "public".into(),
        proc_name: "my_proc".into(),
        owner: "postgres".into(),
        every: "01:00:00".into(),
        max_runtime: "00:00:00".into(),
        max_retries: -1,
        retry_period: "00:05:00".into(),
        scheduled: true,
        fixed_schedule: Some(true),
        initial_start: Some("2026-11-12T00:00:00Z".into()),
        next_start: Some("2026-11-12T05:00:00Z".into()),
        config: Some("{\"a\": 1}".into()),
        check: None,
        timezone: Some("Europe/Madrid".into()),
        target: None,
        mine: true,
    }
}

fn ts_spec(j: &TsJob) -> timescale::JobSpec {
    timescale::JobSpec {
        proc: "\"public\".\"my_proc\"".into(),
        config: j
            .config
            .as_deref()
            .map(|c| serde_json::from_str(c).unwrap()),
        check: j.check.clone(),
        sched: timescale::Sched {
            every: Some(j.every.clone()),
            fixed_schedule: j.fixed_schedule.unwrap_or(false),
            initial_start: j.initial_start.clone(),
            timezone: j.timezone.clone(),
            next_start: j.next_start.clone(),
            next_start_was: j.next_start.clone(),
            max_runtime: Some(j.max_runtime.clone()),
            max_retries: Some(j.max_retries),
            retry_period: Some(j.retry_period.clone()),
            scheduled: j.scheduled,
            name: Some(j.name.clone()),
        },
    }
}

#[test]
fn a_timescaledb_job_is_added_with_what_it_asks_for() {
    let caps = timescale::caps_for_tests();
    let j = ts_job();
    let mut spec = ts_spec(&j);
    spec.sched.max_runtime = None;
    spec.sched.max_retries = None;
    spec.sched.retry_period = None;
    let s = timescale::job_create(V, &caps, &spec);
    assert!(
        s.contains("SELECT \"public\".add_job(proc => E'\"public\".\"my_proc\"'::regproc, schedule_interval => INTERVAL E'01:00:00', config => E'{\"a\":1}'::jsonb, initial_start => E'2026-11-12T00:00:00Z'::timestamptz, fixed_schedule => true, timezone => E'Europe/Madrid', job_name => E'dbine job') AS job_id;"),
        "{s}"
    );
    // Execution settings and a disabled job: alter_job around it, the time
    // zone given again.
    spec.sched.max_retries = Some(3);
    spec.sched.scheduled = false;
    let s = timescale::job_create(V, &caps, &spec);
    assert!(
        s.contains("SELECT job_id FROM \"public\".alter_job(\"public\".add_job("),
        "{s}"
    );
    assert!(
        s.ends_with(
            "), max_retries => 3, scheduled => false, timezone => E'Europe/Madrid');\nCOMMIT;\n"
        ),
        "{s}"
    );
}

#[test]
fn a_timescaledb_job_changes_in_place_keeping_its_time_zone() {
    let caps = timescale::caps_for_tests();
    let cur = ts_job();
    assert_eq!(timescale::job_alter(V, &caps, &cur, &ts_spec(&cur)), None);
    let mut spec = ts_spec(&cur);
    spec.sched.every = Some("02:00:00".into());
    spec.sched.max_retries = Some(5);
    let s = timescale::job_alter(V, &caps, &cur, &spec).unwrap();
    assert!(
        s.contains("SELECT job_id FROM \"public\".alter_job(1000, schedule_interval => INTERVAL E'02:00:00', max_retries => 5, timezone => E'Europe/Madrid');"),
        "{s}"
    );
    // The time zone itself.
    let mut spec = ts_spec(&cur);
    spec.sched.timezone = Some("UTC".into());
    let s = timescale::job_alter(V, &caps, &cur, &spec).unwrap();
    assert!(s.contains("alter_job(1000, timezone => E'UTC');"), "{s}");
    // Config emptied, a check function, a new name, the next start moved.
    let mut spec = ts_spec(&cur);
    spec.config = None;
    spec.check = Some("\"public\".\"chk\"".into());
    spec.sched.name = Some("renamed".into());
    spec.sched.next_start = Some("2027-01-01T00:00:00Z".into());
    let s = timescale::job_alter(V, &caps, &cur, &spec).unwrap();
    for part in [
        "next_start => E'2027-01-01T00:00:00Z'::timestamptz",
        "job_name => E'renamed'",
        "config => 'null'::jsonb",
        "check_config => E'\"public\".\"chk\"'::regproc",
    ] {
        assert!(s.contains(part), "{part}: {s}");
    }
    // The check removed.
    let with_check = TsJob {
        check: Some("\"public\".\"chk\"".into()),
        ..cur.clone()
    };
    let mut spec = ts_spec(&with_check);
    spec.check = None;
    let s = timescale::job_alter(V, &caps, &with_check, &spec).unwrap();
    assert!(s.contains("check_config => 0"), "{s}");
    // The next start moved by a run since the form was read: left alone.
    let mut spec = ts_spec(&cur);
    spec.sched.next_start_was = Some("2026-11-12T04:00:00Z".into());
    spec.sched.next_start = Some("2026-11-12T04:00:00Z".into());
    assert_eq!(timescale::job_alter(V, &caps, &cur, &spec), None);
}

#[test]
fn another_procedure_adds_the_job_again() {
    let caps = timescale::caps_for_tests();
    let cur = ts_job();
    let mut spec = ts_spec(&cur);
    spec.proc = "\"public\".\"other\"".into();
    let s = timescale::job_alter(V, &caps, &cur, &spec).unwrap();
    assert!(s.contains("no permite cambiar el procedimiento"), "{s}");
    let delete = s.find("SELECT \"public\".delete_job(1000);").unwrap();
    let add = s.find("add_job(proc => E'\"public\".\"other\"'").unwrap();
    assert!(delete < add, "{s}");
}

fn policy_job(p: Policy, config: &str) -> TsJob {
    let proc_name = match p {
        Policy::Compression => "policy_compression",
        Policy::Retention => "policy_retention",
        Policy::Refresh => "policy_refresh_continuous_aggregate",
        Policy::Reorder => "policy_reorder",
        Policy::Compaction => "policy_compaction",
    };
    TsJob {
        id: 1001,
        name: "Columnstore Policy".into(),
        proc_schema: "_timescaledb_functions".into(),
        proc_name: proc_name.into(),
        every: "12:00:00".into(),
        max_runtime: "00:00:00".into(),
        max_retries: -1,
        retry_period: "01:00:00".into(),
        scheduled: true,
        fixed_schedule: Some(false),
        initial_start: None,
        next_start: None,
        config: Some(config.into()),
        check: None,
        timezone: None,
        target: Some("\"public\".\"m1\"".into()),
        owner: "postgres".into(),
        mine: true,
    }
}

fn sched_of(j: &TsJob) -> timescale::Sched {
    ts_spec(j).sched
}

#[test]
fn policies_are_added_with_their_settings() {
    let caps = timescale::caps_for_tests();
    let blank = timescale::Sched {
        scheduled: true,
        ..timescale::Sched::default()
    };
    let ps = timescale::PolicySpec {
        target: "\"public\".\"m1\"".into(),
        params: vec![
            ("compress_after".into(), json!("7 days")),
            ("compress_created_before".into(), Value::Null),
        ],
        sched: timescale::Sched {
            every: Some("1 day".into()),
            ..blank.clone()
        },
    };
    let s = timescale::policy_create(V, &caps, Policy::Compression, &ps);
    assert!(
        s.contains("SELECT \"public\".add_compression_policy(E'\"public\".\"m1\"', compress_after => INTERVAL E'7 days', schedule_interval => INTERVAL E'1 day') AS job_id;"),
        "{s}"
    );
    // Integer time: a number.
    let ps = timescale::PolicySpec {
        params: vec![("drop_after".into(), json!(100000))],
        sched: blank.clone(),
        ..ps
    };
    let s = timescale::policy_create(V, &caps, Policy::Retention, &ps);
    assert!(
        s.contains("add_retention_policy(E'\"public\".\"m1\"', drop_after => 100000) AS job_id;"),
        "{s}"
    );
    // Refresh: both offsets always named (they have no default; an open
    // end is NULL), the rest only when given.
    let ps = timescale::PolicySpec {
        target: "\"public\".\"cagg\"".into(),
        params: vec![
            ("start_offset".into(), Value::Null),
            ("end_offset".into(), json!("01:00:00")),
            ("buckets_per_batch".into(), json!(2)),
            ("max_batches_per_execution".into(), Value::Null),
            ("refresh_newest_first".into(), json!(false)),
        ],
        sched: timescale::Sched {
            every: Some("01:00:00".into()),
            scheduled: false,
            ..blank.clone()
        },
    };
    let s = timescale::policy_create(V, &caps, Policy::Refresh, &ps);
    assert!(
        s.contains("SELECT job_id FROM \"public\".alter_job(\"public\".add_continuous_aggregate_policy(E'\"public\".\"cagg\"', start_offset => NULL, end_offset => INTERVAL E'01:00:00', buckets_per_batch => 2, refresh_newest_first => false, schedule_interval => INTERVAL E'01:00:00'), scheduled => false);"),
        "{s}"
    );
    let open = timescale::PolicySpec {
        params: vec![
            ("start_offset".into(), Value::Null),
            ("end_offset".into(), Value::Null),
        ],
        sched: timescale::Sched {
            every: Some("01:00:00".into()),
            ..blank.clone()
        },
        ..ps
    };
    assert!(
        timescale::policy_create(V, &caps, Policy::Refresh, &open).contains("add_continuous_aggregate_policy(E'\"public\".\"cagg\"', start_offset => NULL, end_offset => NULL, schedule_interval => INTERVAL E'01:00:00') AS job_id;")
    );
    // Other policies leave an empty parameter to the function's default.
    let ps = timescale::PolicySpec {
        target: "\"public\".\"m1\"".into(),
        params: vec![
            ("max_chunks".into(), Value::Null),
            ("inactive_for".into(), json!("02:00:00")),
        ],
        sched: blank.clone(),
    };
    assert!(timescale::policy_create(V, &caps, Policy::Compaction, &ps)
        .contains("add_compaction_policy(E'\"public\".\"m1\"', inactive_for => INTERVAL E'02:00:00') AS job_id;"));
    // Reorder: the index as text, its interval through alter_job.
    let ps = timescale::PolicySpec {
        target: "\"public\".\"m1\"".into(),
        params: vec![("index_name".into(), json!("m1_ix'"))],
        sched: timescale::Sched {
            every: Some("1 day".into()),
            ..blank
        },
    };
    let s = timescale::policy_create(V, &caps, Policy::Reorder, &ps);
    assert!(
        s.contains("alter_job(\"public\".add_reorder_policy(E'\"public\".\"m1\"', index_name => E'm1_ix'''), schedule_interval => INTERVAL E'1 day');"),
        "{s}"
    );
}

#[test]
fn a_policys_settings_change_in_its_config() {
    let caps = timescale::caps_for_tests();
    let cur = policy_job(
        Policy::Compression,
        "{\"hypertable_id\": 1, \"compress_after\": \"7 days\"}",
    );
    let same = timescale::PolicySpec {
        target: "\"public\".\"m1\"".into(),
        params: vec![
            ("compress_after".into(), json!("7 days")),
            ("compress_created_before".into(), Value::Null),
        ],
        sched: sched_of(&cur),
    };
    assert_eq!(
        timescale::policy_alter(V, &caps, Policy::Compression, &cur, &same),
        None
    );
    // From "after" to "created before": the other key leaves the config.
    let ps = timescale::PolicySpec {
        params: vec![
            ("compress_after".into(), Value::Null),
            ("compress_created_before".into(), json!("10 days")),
        ],
        ..same.clone()
    };
    let s = timescale::policy_alter(V, &caps, Policy::Compression, &cur, &ps).unwrap();
    assert!(
        s.contains("alter_job(1001, config => E'{\"compress_created_before\":\"10 days\",\"hypertable_id\":1}'::jsonb);"),
        "{s}"
    );
    // Another hypertable: removed and added again.
    let ps = timescale::PolicySpec {
        target: "\"public\".\"m2\"".into(),
        ..same
    };
    let s = timescale::policy_alter(V, &caps, Policy::Compression, &cur, &ps).unwrap();
    let remove = s
        .find("SELECT \"public\".remove_compression_policy(E'\"public\".\"m1\"');")
        .unwrap();
    let add = s
        .find("add_compression_policy(E'\"public\".\"m2\"'")
        .unwrap();
    assert!(remove < add, "{s}");
    // A refresh window opened at its start keeps the key, as null.
    let cur = policy_job(
        Policy::Refresh,
        "{\"start_offset\": \"3 days\", \"end_offset\": \"01:00:00\", \"mat_hypertable_id\": 7}",
    );
    let ps = timescale::PolicySpec {
        target: "\"public\".\"m1\"".into(),
        params: vec![
            ("start_offset".into(), Value::Null),
            ("end_offset".into(), json!("01:00:00")),
        ],
        sched: sched_of(&cur),
    };
    let s = timescale::policy_alter(V, &caps, Policy::Refresh, &cur, &ps).unwrap();
    assert!(s.contains("\"start_offset\":null"), "{s}");
}

#[test]
fn a_refresh_policys_interval_is_required() {
    let caps = timescale::caps_for_tests();
    let keys = |p: Option<Policy>, existing: bool| -> Option<bool> {
        timescale::schedule_sections_for_tests(&caps, existing, p)
            .iter()
            .flat_map(|s| s.fields.iter())
            .find(|f| f.key == "schedule_interval")
            .map(|f| f.required)
    };
    assert_eq!(keys(Some(Policy::Refresh), false), Some(true));
    assert_eq!(keys(Some(Policy::Compression), false), Some(false));
    assert_eq!(keys(Some(Policy::Compression), true), Some(true));
    assert_eq!(keys(None, false), Some(true));
}

#[test]
fn what_an_older_timescaledb_lacks_isnt_asked_for_or_written() {
    // Before 2.9: no fixed schedules, no initial start in alter_job; before
    // 2.7: no check function.
    let caps = timescale::Caps {
        fixed_schedule: false,
        check_config: false,
        alter_initial_start: false,
        timezone: false,
        ..timescale::caps_for_tests()
    };
    let f = timescale::blank_job_form(&caps);
    let keys: Vec<&str> = f
        .sections
        .iter()
        .flat_map(|s| s.fields.iter().map(|x| x.key.as_str()))
        .collect();
    for k in ["fixed_schedule", "check_config", "timezone"] {
        assert!(!keys.contains(&k), "{k}: {keys:?}");
    }
    assert!(keys.contains(&"initial_start"), "{keys:?}");
    assert!(!f.values.contains_key("fixed_schedule"));
    let j = ts_job();
    let mut spec = ts_spec(&j);
    spec.check = Some("\"public\".\"chk\"".into());
    let s = timescale::job_create(V, &caps, &spec);
    assert!(
        !s.contains("fixed_schedule") && !s.contains("check_config") && !s.contains("timezone"),
        "{s}"
    );
    assert!(s.contains("initial_start => "), "{s}");
    // An existing job: the initial start isn't changed, nor the check.
    let old = TsJob {
        fixed_schedule: None,
        initial_start: None,
        timezone: None,
        ..ts_job()
    };
    let mut spec = ts_spec(&old);
    spec.sched.initial_start = Some("2027-01-01T00:00:00Z".into());
    spec.check = Some("\"public\".\"chk\"".into());
    assert_eq!(timescale::job_alter(V, &caps, &old, &spec), None);
}

#[test]
fn only_the_policies_this_version_has_are_kinds() {
    let mut caps = timescale::caps_for_tests();
    assert_eq!(caps.policies().len(), 5);
    caps.compaction = false;
    assert!(!caps.policies().contains(&Policy::Compaction));
}

#[test]
fn passwords_never_come_back_in_an_error() {
    let m = "ERROR: new row violates check constraint\nDetalle: Failing row contains (12, 3, 02 remote, , t, s, SELECT 2, host=h password='pw\\'x y' user=u, , i, null).";
    let r = redact_passwords(m);
    assert!(
        !r.contains("pw") && r.contains(&format!("password={SECRET_MASK} user=u")),
        "{r}"
    );
    let r =
        redact_passwords("x PASSWORD=abc,next postgresql://u:s3cr%40t@h:5432/db?password=zz&a=b");
    assert!(
        !r.contains("abc") && !r.contains("s3cr") && !r.contains("zz"),
        "{r}"
    );
    assert!(
        r.contains(&format!(
            "postgresql://u:{SECRET_MASK}@h:5432/db?password={SECRET_MASK}&a=b"
        )),
        "{r}"
    );
    assert_eq!(
        redact_passwords("nada que ocultar: ñandú"),
        "nada que ocultar: ñandú"
    );
    assert_eq!(redact_passwords("postgres://u@h/db"), "postgres://u@h/db");
}
