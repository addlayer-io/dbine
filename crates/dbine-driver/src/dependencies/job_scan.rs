//! Server jobs in the lineage: the jobs whose steps run or name an object
//! ([`job_dependents_scan`]: a SQL Server Agent step that `EXEC`s a
//! procedure, a pg_cron command, a `DBMS_SCHEDULER` job of type
//! `STORED_PROCEDURE`, a MySQL event…), and what a job's steps use
//! ([`job_references_scan`]). Both read the jobs contract
//! ([`Session::jobs`], [`Session::job_detail`]), so every engine whose driver
//! lists its jobs gets them, through the same reading of code as views and
//! routines.
//!
//! A job runs on the server, often in a database of its own: a step's
//! `target` (or the job's `group`) is its database when it's one of the
//! server's. A step in another database counts only where it names the
//! target with the target's database (`ventas.dbo.sp_cargar`); its other
//! names are that database's objects, not the session's.

use super::{
    eq, find_mentions, mentions, names_word, resolve_names, sort, used_names, used_objects,
    Confidence, DependencyReport, DependencyScan, DependencyTarget, ExternalRef, JobDependent,
    Mention, UsedName, CONSTRAINT, MAX_MENTIONS, SCHEMA,
};
use crate::jobs::{JobDetail, JobStep};
use crate::model::ObjectRef;
use crate::sql::{code_tokens, NameToken, ScriptDialect, TokenKind};
use crate::{kinds, Error, Result, Session};

/// Jobs whose steps are read for one report, at most: each is one more
/// round trip ([`Session::job_detail`]).
pub const JOBS_READ: usize = 300;

/// What the drivers call a step that runs a stored procedure by name (a
/// `DBMS_SCHEDULER` job of type `STORED_PROCEDURE`, a Db2 `ADMIN_TASK`, a
/// HANA or Vertica scheduled procedure): the engine states what it runs.
const PROCEDURE_STEP: &str = "Procedimiento";

/// The server's databases, to tell a step's database from a warehouse, an
/// index or a destination (what [`JobStep::target`] holds on other engines).
struct Places {
    databases: Vec<String>,
}

impl Places {
    async fn read<S: Session + ?Sized>(s: &mut S) -> Places {
        Places {
            databases: s.list_databases().await.unwrap_or_default(),
        }
    }

    /// `name` when it's one of the server's databases.
    fn database<'a>(&self, name: Option<&'a str>) -> Option<&'a str> {
        name.filter(|n| self.databases.iter().any(|d| eq(d, n)))
    }
}

/// Where a step runs: its own database, else the job's, when either is one
/// of the server's databases.
fn step_database<'a>(places: &Places, job: &'a JobDetail, step: &'a JobStep) -> Option<&'a str> {
    places
        .database(step.target.as_deref())
        .or_else(|| places.database(job.job.group.as_deref()))
}

/// The generic [`Session::job_dependents`]: the server's jobs (at most
/// [`JOBS_READ`]) whose steps run or name the target. `database` is the
/// session's, the target's. Empty where the engine has no jobs.
pub async fn job_dependents_scan<S: Session + ?Sized>(
    s: &mut S,
    database: &str,
    target: &DependencyTarget,
    ctx: &DependencyScan,
) -> Result<DependencyReport> {
    let mut report = DependencyReport::default();
    let t = &target.object;
    // Jobs run objects; a column, a schema or a constraint isn't one.
    if target.column.is_some() || matches!(t.kind.as_str(), SCHEMA | kinds::INDEX | CONSTRAINT) {
        return Ok(report);
    }
    let list = match s.jobs().await {
        Ok(l) => l,
        Err(Error::Unsupported(_)) => return Ok(report),
        Err(e) => return Err(e),
    };
    let mut notes = list.notes;
    let places = Places::read(s).await;
    let total = list.jobs.len();
    for job in list.jobs.into_iter().take(JOBS_READ) {
        let detail = match s.job_detail(&job.id).await {
            Ok(d) => d,
            Err(_) => {
                report.unreadable.push(job.name);
                continue;
            }
        };
        report.scanned += 1;
        if let Some(hit) = job_hit(&places.databases, database, &detail, target, ctx) {
            report.jobs.push(hit);
        }
    }
    if total > JOBS_READ {
        notes.push(format!(
            "se leyeron los primeros {JOBS_READ} jobs del servidor, de {total}"
        ));
    }
    report.job_notes = notes;
    report.jobs.sort_by(|a, b| {
        (a.confidence, a.name.to_lowercase()).cmp(&(b.confidence, b.name.to_lowercase()))
    });
    Ok(report)
}

/// `jobs` (a [`Session::job_dependents`] report) into `report` (the
/// object's dependents): its jobs, their notes and the unreadable ones. A job that is
/// already one of the objects (a ClickHouse refreshable materialized view, a
/// task) isn't added again.
pub fn merge_jobs(report: &mut DependencyReport, jobs: DependencyReport) {
    for j in jobs.jobs {
        let shown = report.items.iter().any(|d| {
            eq(&d.name, &j.name)
                && match (d.schema.as_deref(), j.group.as_deref()) {
                    (Some(a), Some(b)) => eq(a, b),
                    _ => true,
                }
        });
        if !shown {
            report.jobs.push(j);
        }
    }
    report.unreadable.extend(jobs.unreadable);
    report.job_notes.extend(jobs.job_notes);
}

/// The job, when one of its steps runs or names the target. `databases`:
/// the server's ([`Session::list_databases`]); `database`: the target's.
pub fn job_hit(
    databases: &[String],
    database: &str,
    detail: &JobDetail,
    target: &DependencyTarget,
    ctx: &DependencyScan,
) -> Option<JobDependent> {
    let places = Places {
        databases: databases.to_vec(),
    };
    let mut steps = Vec::new();
    let mut found: Vec<Mention> = Vec::new();
    let mut best: Option<Confidence> = None;
    for step in &detail.steps {
        let Some(cmd) = step.command.as_deref().filter(|c| !c.trim().is_empty()) else {
            continue;
        };
        let elsewhere = step_database(&places, detail, step)
            .filter(|d| !database.is_empty() && !eq(d, database))
            .is_some();
        let hit = if elsewhere {
            mentions_from_elsewhere(cmd, &ctx.dialect, target, database, ctx.has_schemas)
        } else {
            find_mentions(cmd, &ctx.dialect, target)
        };
        let Some((mut confidence, ms)) = hit else {
            continue;
        };
        if step.kind.as_deref() == Some(PROCEDURE_STEP)
            && called(cmd).is_some_and(|(_, name)| eq(&name, &target.object.name))
        {
            confidence = Confidence::Confirmed;
        }
        best = Some(best.map_or(confidence, |b| b.min(confidence)));
        steps.push(step.name.clone());
        let room = MAX_MENTIONS.saturating_sub(found.len());
        found.extend(ms.into_iter().take(room));
    }
    Some(JobDependent {
        id: detail.job.id.clone(),
        name: detail.job.name.clone(),
        kind: detail.job.kind.clone(),
        group: detail.job.group.clone(),
        steps,
        confidence: best?,
        mentions: found,
    })
}

/// Where a step that runs in another database names the target: only with
/// the target's database before it (`ventas.dbo.sp`, `ventas..sp`,
/// `ventas.sp` without schemas). Inside a string, the database and the name
/// both as words: `Review`.
fn mentions_from_elsewhere(
    cmd: &str,
    dialect: &ScriptDialect,
    target: &DependencyTarget,
    database: &str,
    has_schemas: bool,
) -> Option<(Confidence, Vec<Mention>)> {
    let t = &target.object;
    let toks = code_tokens(cmd, dialect);
    let mut hits: Vec<(usize, bool)> = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        match tok.kind {
            TokenKind::Name if eq(tok.text, &t.name) && !super::at_variable(&toks, i, dialect) => {
                let quals = qualifiers(&toks, i);
                if in_database(&quals, database, t.schema(), has_schemas) {
                    hits.push((tok.start, false));
                }
            }
            TokenKind::String
                if names_word(tok.text, &t.name) && names_word(tok.text, database) =>
            {
                hits.push((tok.start, true));
            }
            _ => {}
        }
    }
    if hits.is_empty() {
        return None;
    }
    let confidence = if hits.iter().any(|&(_, dynamic)| !dynamic) {
        Confidence::Probable
    } else {
        Confidence::Review
    };
    Some((confidence, mentions(cmd, &hits)))
}

/// The qualifiers written before the name at `i`, nearest first; `db..x`
/// (T-SQL's default schema) gives `["", "db"]`.
fn qualifiers(toks: &[NameToken<'_>], i: usize) -> Vec<String> {
    let dot = |k: usize| toks[k].kind == TokenKind::Punct && toks[k].text == ".";
    let mut quals = Vec::new();
    let mut k = i;
    loop {
        if k >= 2 && dot(k - 1) && toks[k - 2].kind == TokenKind::Name {
            quals.push(toks[k - 2].text.to_string());
            k -= 2;
        } else if k >= 3 && dot(k - 1) && dot(k - 2) && toks[k - 3].kind == TokenKind::Name {
            quals.push(String::new());
            quals.push(toks[k - 3].text.to_string());
            k -= 3;
        } else {
            return quals;
        }
    }
}

/// `quals` put the name in `database`: the database before the schema with
/// schemas (and a schema that's the target's, or none, or T-SQL's empty
/// default), the first qualifier without.
fn in_database(quals: &[String], database: &str, schema: Option<&str>, has_schemas: bool) -> bool {
    if !has_schemas {
        return quals.first().is_some_and(|q| eq(q, database));
    }
    let Some(db) = quals.get(1) else { return false };
    let schema_ok = match (schema, quals.first()) {
        (Some(s), Some(q)) => {
            q.is_empty() || super::match_qualifiers(&quals[..1], s) != super::Qualified::Other
        }
        _ => true,
    };
    eq(db, database) && schema_ok
}

/// The routine a procedure step runs: its command's first name, after
/// `CALL` or `EXEC` (`APP.LOAD`, `CALL ventas.cargar(1)`, `p(args)`), as
/// its qualifiers (nearest first) and the name.
fn called(cmd: &str) -> Option<(Vec<String>, String)> {
    let mut rest = cmd.trim();
    for kw in ["CALL ", "EXECUTE ", "EXEC "] {
        if rest
            .get(..kw.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(kw))
        {
            rest = rest[kw.len()..].trim_start();
            break;
        }
    }
    let end = rest
        .find(|c: char| c == '(' || c == ';' || c.is_whitespace())
        .unwrap_or(rest.len());
    let mut parts: Vec<String> = rest[..end]
        .split('.')
        .map(|p| {
            p.trim_matches(|c| matches!(c, '"' | '[' | ']' | '`'))
                .to_string()
        })
        .filter(|p| !p.is_empty())
        .collect();
    let name = parts.pop()?;
    let ident = |c: char| c.is_alphanumeric() || matches!(c, '_' | '$' | '#');
    if !name.chars().all(ident) {
        return None;
    }
    parts.reverse();
    Some((parts, name))
}

/// The generic [`Session::job_references`]: what the job's steps use. Names
/// of `database` (the session's) resolve to its objects, as a routine's do
/// ([`super::references_scan`]); a step that runs in another database, and a
/// name qualified with another of the server's databases, go to `external`
/// with that database. A procedure step's routine is `Confirmed`.
pub async fn job_references_scan<S: Session + ?Sized>(
    s: &mut S,
    database: &str,
    job_id: &str,
    ctx: &DependencyScan,
) -> Result<DependencyReport> {
    let detail = s.job_detail(job_id).await?;
    let places = Places::read(s).await;
    let me = ObjectRef {
        kind: "job".into(),
        schema: detail.job.group.clone(),
        name: detail.job.name.clone(),
    };
    let mut report = DependencyReport::default();
    for step in &detail.steps {
        let Some(cmd) = step.command.as_deref().filter(|c| !c.trim().is_empty()) else {
            continue;
        };
        report.scanned += 1;
        let runs_in = step_database(&places, &detail, step);
        let mut used = used_names(cmd, &ctx.dialect);
        let procedure = (step.kind.as_deref() == Some(PROCEDURE_STEP))
            .then(|| called(cmd))
            .flatten();
        if let Some((quals, name)) = &procedure {
            if !used.iter().any(|u| eq(&u.name, name)) {
                let at = cmd
                    .to_ascii_lowercase()
                    .find(&name.to_ascii_lowercase())
                    .unwrap_or(0);
                used.push(UsedName {
                    quals: quals.clone(),
                    name: name.clone(),
                    at,
                    call: true,
                });
            }
        }
        let mut local = Vec::new();
        for u in used {
            let named_db = if ctx.has_schemas {
                u.quals.get(1)
            } else {
                u.quals.first()
            }
            .map(String::as_str)
            .filter(|q| !q.is_empty());
            let other = match named_db {
                Some(q) if !eq(q, database) && places.database(Some(q)).is_some() => {
                    Some(q.to_string())
                }
                Some(_) => None,
                None => runs_in
                    .filter(|d| !database.is_empty() && !eq(d, database))
                    .map(str::to_string),
            };
            match other {
                Some(db) => add_external(&mut report, cmd, step, &u, db, ctx.has_schemas),
                None => local.push(u),
            }
        }
        if local.is_empty() {
            continue;
        }
        let objects = resolve_names(s, &local).await?;
        for mut d in used_objects(cmd, &local, &objects, &me) {
            if procedure.as_ref().is_some_and(|(_, n)| eq(n, &d.name)) {
                d.confidence = Confidence::Confirmed;
            }
            match report
                .items
                .iter_mut()
                .find(|x| x.kind == d.kind && x.schema == d.schema && eq(&x.name, &d.name))
            {
                Some(x) => {
                    x.confidence = x.confidence.min(d.confidence);
                    let room = MAX_MENTIONS.saturating_sub(x.mentions.len());
                    x.mentions.extend(d.mentions.into_iter().take(room));
                }
                None => report.items.push(d),
            }
        }
    }
    sort(&mut report.items);
    Ok(report)
}

/// A name the step uses in another database, as a source outside (once per
/// database, schema and name).
fn add_external(
    report: &mut DependencyReport,
    cmd: &str,
    step: &JobStep,
    u: &UsedName,
    database: String,
    has_schemas: bool,
) {
    let schema = if has_schemas {
        u.quals.first().filter(|q| !q.is_empty()).cloned()
    } else {
        None
    };
    let line = mentions(cmd, &[(u.at, false)]);
    if let Some(x) = report.external.iter_mut().find(|x| {
        x.database.as_deref().is_some_and(|d| eq(d, &database))
            && x.schema.as_deref().map(str::to_lowercase)
                == schema.as_deref().map(str::to_lowercase)
            && x.name.as_deref().is_some_and(|n| eq(n, &u.name))
    }) {
        if x.mentions.len() < MAX_MENTIONS {
            x.mentions.extend(line);
        }
        return;
    }
    report.external.push(ExternalRef {
        system: None,
        server: None,
        database: Some(database),
        schema,
        name: Some(u.name.clone()),
        detail: Some(step.name.clone()),
        mentions: line,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{JobList, ServerJob};
    use crate::model::DbObject;

    /// A server with jobs and a few objects in `ventas`.
    struct Fake {
        jobs: Vec<JobDetail>,
        objects: Vec<DbObject>,
        details_read: u32,
        unsupported: bool,
    }

    #[async_trait::async_trait]
    impl Session for Fake {
        async fn server_version(&mut self) -> Result<String> {
            Ok(String::new())
        }
        async fn list_databases(&mut self) -> Result<Vec<String>> {
            Ok(vec!["ventas".into(), "stage".into(), "master".into()])
        }
        async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
            Ok(self.objects.clone())
        }
        async fn columns(&mut self, _: &ObjectRef) -> Result<Vec<crate::ColumnInfo>> {
            Ok(vec![])
        }
        async fn definition(&mut self, _: &ObjectRef) -> Result<Option<String>> {
            Ok(None)
        }
        fn browse_query(&self, _: &ObjectRef, _: u32) -> String {
            String::new()
        }
        async fn execute(&mut self, _: &str, _: usize, _: &mut crate::QueryOutcome) -> Result<()> {
            Ok(())
        }
        async fn jobs(&mut self) -> Result<JobList> {
            if self.unsupported {
                return Err(Error::Unsupported("no".into()));
            }
            Ok(JobList {
                jobs: self.jobs.iter().map(|j| j.job.clone()).collect(),
                notes: vec!["se ven solo los jobs de este usuario".into()],
            })
        }
        async fn job_detail(&mut self, id: &str) -> Result<JobDetail> {
            self.details_read += 1;
            self.jobs
                .iter()
                .find(|j| j.job.id == id)
                .cloned()
                .ok_or_else(|| Error::Query("no existe".into()))
        }
    }

    fn job(
        id: &str,
        name: &str,
        group: Option<&str>,
        steps: &[(&str, Option<&str>, Option<&str>, &str)],
    ) -> JobDetail {
        JobDetail {
            job: ServerJob {
                id: id.into(),
                name: name.into(),
                kind: "SQL Server Agent".into(),
                group: group.map(str::to_string),
                ..Default::default()
            },
            steps: steps
                .iter()
                .map(|(n, kind, target, cmd)| JobStep {
                    name: n.to_string(),
                    kind: kind.map(str::to_string),
                    target: target.map(str::to_string),
                    command: Some(cmd.to_string()),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn obj(kind: &str, schema: &str, name: &str) -> DbObject {
        DbObject {
            kind: kind.into(),
            schema: Some(schema.into()),
            name: name.into(),
            parent: None,
        }
    }

    fn proc_target() -> DependencyTarget {
        DependencyTarget {
            object: ObjectRef {
                kind: "procedure".into(),
                schema: Some("dbo".into()),
                name: "sp_cargar".into(),
            },
            column: None,
        }
    }

    fn ctx() -> DependencyScan {
        DependencyScan {
            source_kinds: vec!["procedure".into()],
            dialect: ScriptDialect::tsql(),
            foreign_keys: true,
            written_kinds: vec!["procedure".into()],
            has_schemas: true,
        }
    }

    fn ready<T>(f: impl std::future::Future<Output = T>) -> T {
        let mut f = std::pin::pin!(f);
        match f
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        {
            std::task::Poll::Ready(v) => v,
            std::task::Poll::Pending => panic!("waited"),
        }
    }

    fn server() -> Fake {
        Fake {
            jobs: vec![
                // Runs it in its own database.
                job(
                    "1",
                    "Carga nocturna",
                    None,
                    &[
                        (
                            "Limpiar",
                            Some("T-SQL"),
                            Some("ventas"),
                            "TRUNCATE TABLE dbo.stage_ventas;",
                        ),
                        (
                            "Cargar ventas",
                            Some("T-SQL"),
                            Some("ventas"),
                            "EXEC dbo.sp_cargar @dia = 1;\nSELECT 1;",
                        ),
                    ],
                ),
                // From another database, by its three-part name.
                job(
                    "2",
                    "Desde stage",
                    None,
                    &[(
                        "Paso 1",
                        Some("T-SQL"),
                        Some("stage"),
                        "EXEC ventas.dbo.sp_cargar;\nEXEC ventas..sp_cargar;",
                    )],
                ),
                // Another database's own sp_cargar: not this one.
                job(
                    "3",
                    "Otra base",
                    None,
                    &[(
                        "Paso 1",
                        Some("T-SQL"),
                        Some("stage"),
                        "EXEC dbo.sp_cargar;",
                    )],
                ),
                // Only in dynamic SQL.
                job(
                    "4",
                    "Dinamico",
                    None,
                    &[(
                        "Paso 1",
                        Some("T-SQL"),
                        Some("ventas"),
                        "EXEC('EXEC dbo.sp_cargar');",
                    )],
                ),
                // A comment doesn't count; a variable neither.
                job(
                    "5",
                    "Comentario",
                    None,
                    &[(
                        "Paso 1",
                        Some("T-SQL"),
                        Some("ventas"),
                        "-- sp_cargar\nSELECT @sp_cargar = 1;",
                    )],
                ),
                // A schema other than the target's.
                job(
                    "6",
                    "Otro esquema",
                    None,
                    &[(
                        "Paso 1",
                        Some("T-SQL"),
                        Some("ventas"),
                        "EXEC ventas_hist.sp_cargar;",
                    )],
                ),
                // A procedure step: the engine says what it runs.
                job(
                    "7",
                    "Programa",
                    Some("ventas"),
                    &[("Programa", Some("Procedimiento"), None, "dbo.sp_cargar")],
                ),
            ],
            objects: vec![
                obj("procedure", "dbo", "sp_cargar"),
                obj("table", "dbo", "stage_ventas"),
                obj("view", "dbo", "v_ventas"),
            ],
            details_read: 0,
            unsupported: false,
        }
    }

    #[test]
    fn jobs_that_run_a_procedure_in_its_database_or_by_its_full_name() {
        let mut s = server();
        let r = ready(job_dependents_scan(
            &mut s,
            "ventas",
            &proc_target(),
            &ctx(),
        ))
        .unwrap();
        let got: Vec<(&str, Confidence, Vec<&str>)> = r
            .jobs
            .iter()
            .map(|j| {
                (
                    j.name.as_str(),
                    j.confidence,
                    j.steps.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("Programa", Confidence::Confirmed, vec!["Programa"]),
                (
                    "Carga nocturna",
                    Confidence::Probable,
                    vec!["Cargar ventas"]
                ),
                ("Desde stage", Confidence::Probable, vec!["Paso 1"]),
                ("Dinamico", Confidence::Review, vec!["Paso 1"]),
            ]
        );
        let nightly = &r.jobs[1];
        assert_eq!(nightly.id, "1");
        assert_eq!(nightly.kind, "SQL Server Agent");
        assert_eq!(nightly.mentions.len(), 1);
        assert_eq!(nightly.mentions[0].line, 1);
        assert_eq!(nightly.mentions[0].text, "EXEC dbo.sp_cargar @dia = 1;");
        // Both lines of the three-part and the `..` names.
        assert_eq!(r.jobs[2].mentions.len(), 2);
        assert_eq!(r.scanned, 7);
        assert_eq!(r.job_notes, ["se ven solo los jobs de este usuario"]);
        assert!(r.note.is_none());
    }

    #[test]
    fn no_jobs_on_engines_without_them_nor_for_a_column() {
        let mut s = server();
        s.unsupported = true;
        let r = ready(job_dependents_scan(
            &mut s,
            "ventas",
            &proc_target(),
            &ctx(),
        ))
        .unwrap();
        assert!(r.jobs.is_empty() && r.job_notes.is_empty());
        let mut s = server();
        let mut col = proc_target();
        col.column = Some("id".into());
        let r = ready(job_dependents_scan(&mut s, "ventas", &col, &ctx())).unwrap();
        assert!(r.jobs.is_empty());
        assert_eq!(s.details_read, 0);
    }

    #[test]
    fn past_the_cap_the_rest_isnt_read_and_the_note_says_so() {
        let mut s = server();
        let template = s.jobs[1].clone();
        s.jobs = (0..JOBS_READ + 5)
            .map(|i| {
                let mut j = template.clone();
                j.job.id = i.to_string();
                j.job.name = format!("job {i}");
                j
            })
            .collect();
        let r = ready(job_dependents_scan(
            &mut s,
            "ventas",
            &proc_target(),
            &ctx(),
        ))
        .unwrap();
        assert_eq!(s.details_read as usize, JOBS_READ);
        assert_eq!(r.jobs.len(), JOBS_READ);
        assert_eq!(
            r.job_notes.last().map(String::as_str),
            Some(
                format!(
                    "se leyeron los primeros {JOBS_READ} jobs del servidor, de {}",
                    JOBS_READ + 5
                )
                .as_str()
            )
        );
    }

    #[test]
    fn without_schemas_the_database_is_the_first_qualifier() {
        // A MySQL event in `stage` that calls `ventas.sp_cargar`.
        let mut ctx = ctx();
        ctx.has_schemas = false;
        ctx.dialect = ScriptDialect::mysql();
        let target = DependencyTarget {
            object: ObjectRef {
                kind: "procedure".into(),
                schema: None,
                name: "sp_cargar".into(),
            },
            column: None,
        };
        let detail = job(
            "e",
            "ev_carga",
            Some("stage"),
            &[(
                "ev_carga",
                Some("SQL"),
                None,
                "CALL ventas.sp_cargar(); CALL sp_cargar();",
            )],
        );
        let dbs = vec!["ventas".to_string(), "stage".to_string()];
        let hit = job_hit(&dbs, "ventas", &detail, &target, &ctx).unwrap();
        assert_eq!(hit.mentions.len(), 1);
        // The same event, in `ventas`: both calls are the target.
        let detail = job(
            "e",
            "ev_carga",
            Some("ventas"),
            &[(
                "ev_carga",
                Some("SQL"),
                None,
                "CALL ventas.sp_cargar();\nCALL sp_cargar();",
            )],
        );
        let hit = job_hit(&dbs, "ventas", &detail, &target, &ctx).unwrap();
        assert_eq!(hit.mentions.len(), 2);
    }

    #[test]
    fn what_a_job_uses_here_and_in_other_databases() {
        let mut s = server();
        s.jobs.push(job("9", "Todo", None, &[
            ("Cargar", Some("T-SQL"), Some("ventas"), "EXEC dbo.sp_cargar;\nSELECT * FROM dbo.v_ventas JOIN stage.dbo.tmp t ON 1 = 1;"),
            ("Archivar", Some("T-SQL"), Some("stage"), "INSERT INTO dbo.archivo SELECT * FROM dbo.tmp;"),
            ("Programa", Some("Procedimiento"), None, "dbo.sp_cargar"),
        ]));
        let r = ready(job_references_scan(&mut s, "ventas", "9", &ctx())).unwrap();
        let items: Vec<(String, Confidence, usize)> = r
            .items
            .iter()
            .map(|d| {
                (
                    format!("{} {}", d.kind, d.name),
                    d.confidence,
                    d.mentions.len(),
                )
            })
            .collect();
        assert_eq!(
            items,
            [
                ("procedure sp_cargar".to_string(), Confidence::Confirmed, 2),
                ("view v_ventas".to_string(), Confidence::Probable, 1),
            ]
        );
        let ext: Vec<String> = r
            .external
            .iter()
            .map(|x| {
                format!(
                    "{}.{}.{} ({})",
                    x.database.as_deref().unwrap_or(""),
                    x.schema.as_deref().unwrap_or(""),
                    x.name.as_deref().unwrap_or(""),
                    x.detail.as_deref().unwrap_or("")
                )
            })
            .collect();
        assert_eq!(
            ext,
            ["stage.dbo.tmp (Cargar)", "stage.dbo.archivo (Archivar)"]
        );
        assert_eq!(r.scanned, 3);
    }

    #[test]
    fn a_job_thats_already_an_object_isnt_added_twice() {
        let mut s = server();
        let r = ready(job_dependents_scan(
            &mut s,
            "ventas",
            &proc_target(),
            &ctx(),
        ))
        .unwrap();
        let mut deps = DependencyReport {
            items: vec![super::super::Dependent {
                kind: "materialized_view".into(),
                schema: None,
                name: "carga nocturna".into(),
                parent: None,
                relation: super::super::Relation::Code,
                confidence: Confidence::Probable,
                detail: None,
                mentions: vec![],
            }],
            note: Some("una nota".into()),
            ..Default::default()
        };
        merge_jobs(&mut deps, r);
        let names: Vec<&str> = deps.jobs.iter().map(|j| j.name.as_str()).collect();
        assert_eq!(names, ["Programa", "Desde stage", "Dinamico"]);
        assert_eq!(deps.note.as_deref(), Some("una nota"));
        assert_eq!(deps.job_notes, ["se ven solo los jobs de este usuario"]);
    }

    #[test]
    fn a_procedure_steps_routine() {
        assert_eq!(
            called("APP.LOAD_PROG"),
            Some((vec!["APP".into()], "LOAD_PROG".into()))
        );
        assert_eq!(
            called("CALL ventas.cargar(1, 'x')"),
            Some((vec!["ventas".into()], "cargar".into()))
        );
        assert_eq!(
            called("exec [dbo].[sp] 1"),
            Some((vec!["dbo".into()], "sp".into()))
        );
        assert_eq!(called("p(args)"), Some((vec![], "p".into())));
        assert_eq!(called("'texto'"), None);
    }
}
