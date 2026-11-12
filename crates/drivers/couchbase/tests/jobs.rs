//! Server jobs (the Backup Service's repositories) against real servers:
//!
//! - Community Edition (`DBINE_TEST_COUCHBASE_URL`, initialized by
//!   tests/integration.rs) has no Backup Service: an empty list and a note.
//! - Enterprise Edition (`DBINE_TEST_COUCHBASE_EE_URL`, with
//!   `DBINE_TEST_COUCHBASE_EE_MGMT_PORT`): a cluster with the backup
//!   service, e.g. `docker run -d --name dbine-test-couchbase-ee -p
//!   25991:8091 -p 25993:8093 couchbase/server:enterprise-8.0.0`, then
//!   `POST /clusterInit` with `services=kv,n1ql,index,backup`, user
//!   Administrator / secreto1.
//!
//! ```sh
//! DBINE_TEST_COUCHBASE_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_MGMT_PORT=25891 \
//! DBINE_TEST_COUCHBASE_EE_URL=http://localhost:25993 DBINE_TEST_COUCHBASE_EE_MGMT_PORT=25991 \
//!   cargo test -p dbine-driver-couchbase --test jobs -- --ignored
//! ```
//!
//! The test makes its own plan and repository (`dbine_test_jobs_*`, the
//! archive in the server's `/tmp`), and the permission test its own users
//! (`dbine_test_jobs_*`); they are deleted at the end, also when an
//! assertion fails.

use dbine_driver::{ConnectionConfig, JobOutcome, Session};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const PLAN: &str = "dbine_test_jobs_plan";
const REPO: &str = "dbine_test_jobs_repo";

fn cfg(url_env: &str, port_env: &str) -> Option<(ConnectionConfig, String)> {
    let url = reqwest::Url::parse(&std::env::var(url_env).ok()?).expect("URL");
    let mgmt = std::env::var(port_env).unwrap_or_else(|_| "8091".into());
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("Administrator".into()),
        password: Some("secreto1".into()),
        ..Default::default()
    };
    c.options.insert("mgmt_port".into(), mgmt.clone());
    Some((
        c,
        format!("http://{}:{mgmt}/_p/backup/api/v1", url.host_str()?),
    ))
}

async fn open(c: &ConnectionConfig) -> Box<dyn Session> {
    dbine_driver_couchbase::drivers()[0]
        .connect(c, None)
        .await
        .expect("connect")
}

async fn api(base: &str, m: reqwest::Method, path: &str, body: Option<Value>) -> (u16, String) {
    let mut rq = reqwest::Client::new()
        .request(m, format!("{base}{path}"))
        .basic_auth("Administrator", Some("secreto1"));
    if let Some(b) = body {
        rq = rq.json(&b);
    }
    let r = rq.send().await.expect("http");
    let st = r.status().as_u16();
    (st, r.text().await.unwrap_or_default())
}

async fn cleanup(base: &str) {
    use reqwest::Method;
    let repo = format!("/cluster/self/repository/active/{REPO}");
    api(
        base,
        Method::POST,
        &format!("{repo}/archive"),
        Some(json!({"id": REPO})),
    )
    .await;
    api(
        base,
        Method::DELETE,
        &format!("/cluster/self/repository/archived/{REPO}?remove_repository=true"),
        None,
    )
    .await;
    api(base, Method::DELETE, &format!("/plan/{PLAN}"), None).await;
}

async fn the_job(s: &mut Box<dyn Session>) -> Option<dbine_driver::ServerJob> {
    s.jobs()
        .await
        .expect("jobs")
        .jobs
        .into_iter()
        .find(|j| j.id == format!("repo:{REPO}"))
}

async fn body(c: ConnectionConfig, base: String) {
    use reqwest::Method;
    let id = format!("repo:{REPO}");
    let (st, t) = api(
        &base,
        Method::POST,
        &format!("/plan/{PLAN}"),
        Some(json!({"description": "dbine test", "services": ["data"], "tasks": [
            {"name": "hourly", "task_type": "BACKUP", "full_backup": true,
             "schedule": {"job_type": "BACKUP", "frequency": 1, "period": "HOURS", "time": "00:00"}}
        ]})),
    )
    .await;
    assert_eq!(st, 200, "{t}");
    let (st, t) = api(
        &base,
        Method::POST,
        &format!("/cluster/self/repository/active/{REPO}"),
        Some(json!({"plan": PLAN, "archive": "/tmp/dbine_test_jobs_archive"})),
    )
    .await;
    assert_eq!(st, 200, "{t}");

    let mut s = open(&c).await;

    // Listed, with what each action allows.
    let job = the_job(&mut s).await.expect("the repository is listed");
    assert_eq!(job.kind, "Backup repository");
    assert_eq!(job.schedule.as_deref(), Some("hourly: 1 HOURS 00:00"));
    assert_eq!(job.description.as_deref(), Some("dbine test"));
    assert!(job.next_run.as_deref().is_some_and(|t| t.ends_with('Z')));
    assert!(job.enabled && job.can_run && job.can_enable && job.has_history && job.has_script);
    assert!(!job.can_stop && !job.system);

    // Detail: the plan's task as the step.
    let det = s.job_detail(&id).await.expect("detail");
    assert_eq!(det.steps.len(), 1);
    assert_eq!(det.steps[0].kind.as_deref(), Some("Backup completo"));
    assert!(det
        .properties
        .iter()
        .any(|p| p.label == "Plan" && p.value == PLAN));

    // Run now, and the new run shows in the history with its node (an
    // archive reused from an earlier run may bring older ones).
    let before: Vec<Option<String>> = s
        .job_runs(&id, 50)
        .await
        .expect("runs")
        .into_iter()
        .map(|r| r.id)
        .collect();
    s.run_job(&id).await.expect("run now");
    let start = Instant::now();
    let mut seen_running = false;
    let r = loop {
        let runs = s.job_runs(&id, 50).await.expect("runs");
        let new: Vec<_> = runs
            .into_iter()
            .filter(|r| !before.contains(&r.id))
            .collect();
        seen_running |= new.iter().any(|r| r.outcome == JobOutcome::Running);
        if let Some(r) = new.into_iter().find(|r| r.outcome == JobOutcome::Succeeded) {
            break r;
        }
        assert!(
            start.elapsed() < Duration::from_secs(90),
            "the new run never finished"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    eprintln!("run {:?} (seen running: {seen_running})", r.id);
    assert!(r.id.as_deref().is_some_and(|i| i.starts_with("BACKUP-")));
    assert_eq!(r.trigger.as_deref(), Some("manual"));
    assert!(r.started.is_some() && r.duration_ms.is_some());
    assert!(!r.steps.is_empty() && r.steps[0].name.starts_with("Nodo "));
    let job = the_job(&mut s).await.unwrap();
    assert!(job.last_started.is_some());

    // Disable (pause) and enable (resume).
    s.set_job_enabled(&id, false).await.expect("pause");
    assert!(!the_job(&mut s).await.unwrap().enabled);
    s.set_job_enabled(&id, true).await.expect("resume");
    assert!(the_job(&mut s).await.unwrap().enabled);

    // Script: the plan and the repository.
    let script = s.job_script(&id).await.expect("script");
    assert!(script.contains(&format!("POST /api/v1/plan/{PLAN}")));
    assert!(script.contains(&format!(
        "POST /api/v1/cluster/self/repository/active/{REPO}"
    )));
    assert!(script.contains("/tmp/dbine_test_jobs_archive"));

    // No stop.
    assert!(s.stop_job(&id).await.is_err());

    // A malicious or unknown id is refused before anything runs.
    for bad in [
        "repo:../../plan/dbine_test_jobs_plan",
        "repo:dbine_test_jobs_repo/pause",
        "repo:a b",
        "repo:dbine_test_jobs_nope",
        "dbine_test_jobs_repo",
        "",
    ] {
        assert!(s.job_detail(bad).await.is_err(), "{bad:?}");
        assert!(s.run_job(bad).await.is_err(), "{bad:?}");
        assert!(s.set_job_enabled(bad, false).await.is_err(), "{bad:?}");
        assert!(s.job_script(bad).await.is_err(), "{bad:?}");
    }
    assert!(
        the_job(&mut s).await.unwrap().enabled,
        "the bad ids touched nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn backup_repositories_on_enterprise() {
    let Some((c, base)) = cfg(
        "DBINE_TEST_COUCHBASE_EE_URL",
        "DBINE_TEST_COUCHBASE_EE_MGMT_PORT",
    ) else {
        eprintln!("DBINE_TEST_COUCHBASE_EE_URL not set; skipping");
        return;
    };
    cleanup(&base).await;
    let r = tokio::spawn(body(c, base.clone())).await;
    cleanup(&base).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}

/// Users made for the permission test: one that reads data but not the
/// backup service, one that can't even read the cluster (`/pools/default`).
const LIMITED: [(&str, &str); 2] = [
    ("dbine_test_jobs_reader", "data_reader[*]"),
    ("dbine_test_jobs_stats", "external_stats_reader"),
];

async fn users(mgmt: &str, create: bool) {
    for (user, roles) in LIMITED {
        let url = format!("{mgmt}/settings/rbac/users/local/{user}");
        let c = reqwest::Client::new();
        let rq = if create {
            c.put(url)
                .form(&[("password", "secreto1"), ("roles", roles)])
        } else {
            c.delete(url)
        };
        let r = rq
            .basic_auth("Administrator", Some("secreto1"))
            .send()
            .await
            .expect("http");
        if create {
            assert_eq!(r.status().as_u16(), 200, "user {user}");
        }
    }
}

async fn limited_body(c: ConnectionConfig) {
    for (user, _) in LIMITED {
        let mut c = c.clone();
        c.username = Some(user.into());
        let mut s = open(&c).await;
        let list = s.jobs().await.expect("a note, not an error");
        assert!(list.jobs.is_empty(), "{user}: {list:?}");
        assert!(
            list.notes
                .iter()
                .any(|n| n.contains("no puede ver el servicio de backup")),
            "{user}: {list:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn a_user_without_backup_roles_gets_a_note() {
    let Some((c, base)) = cfg(
        "DBINE_TEST_COUCHBASE_EE_URL",
        "DBINE_TEST_COUCHBASE_EE_MGMT_PORT",
    ) else {
        eprintln!("DBINE_TEST_COUCHBASE_EE_URL not set; skipping");
        return;
    };
    let mgmt = base.trim_end_matches("/_p/backup/api/v1").to_string();
    users(&mgmt, false).await;
    users(&mgmt, true).await;
    let r = tokio::spawn(limited_body(c)).await;
    users(&mgmt, false).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn community_edition_has_no_backup_service() {
    let Some((c, _)) = cfg("DBINE_TEST_COUCHBASE_URL", "DBINE_TEST_COUCHBASE_MGMT_PORT") else {
        eprintln!("DBINE_TEST_COUCHBASE_URL not set; skipping");
        return;
    };
    let mut s = open(&c).await;
    let list = s.jobs().await.expect("jobs");
    assert!(list.jobs.is_empty());
    assert!(
        list.notes.iter().any(|n| n.contains("Community Edition")),
        "{list:?}"
    );
    assert!(s.job_detail("repo:x").await.is_err());
}
