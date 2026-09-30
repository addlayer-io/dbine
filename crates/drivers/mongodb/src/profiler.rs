//! The profiler ([`dbine_driver::profiler`]) per flavor.
//!
//! - MongoDB: the database profiler (`<db>.system.profile`), complete. When
//!   allowed, it sets the level to 2 and `sampleRate` to 1 at start and puts
//!   both back at stop (`slowms` and `filter` aren't touched). `sampleRate`
//!   is server-wide (it needs `enableProfiler` on every database), so it is
//!   only sent when it isn't 1 already: `dbAdmin` on one database is enough
//!   to raise the level. Read-only,
//!   it reads what the current level already records (level 1: only the slow
//!   operations). Where the `profile` command is off limits (level 0 and
//!   read-only, Atlas shared tiers, Azure Cosmos DB, a `mongos`), it samples
//!   `currentOp` instead.
//! - Amazon DocumentDB: its profiler writes to CloudWatch Logs, not to
//!   `system.profile`: `currentOp`, sampled.
//! - FerretDB: no `profile` command, and its `currentOp` shows only the
//!   command's name and collection (no filter or pipeline, no client or
//!   user, times to the second): not supported.

use crate::monitor::num;
use crate::{err, Flavor, MongoSession};
use chrono::{DateTime, Utc};
use dbine_driver::profiler::{Sample, Sampler, SAMPLE_EVERY, SAMPLE_FOR};
use dbine_driver::{Error, ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use mongodb::bson::{doc, Bson, Document};
use mongodb::Database;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Profile entries read per poll.
const BATCH: i32 = 1000;
/// Keys the drivers add to every command; not part of what the user wrote.
const NOISE: [&str; 9] =
    ["lsid", "$db", "$clusterTime", "$readPreference", "txnNumber", "autocommit", "startTransaction", "$audit", "apiVersion"];

pub(crate) fn supported(f: Flavor) -> bool {
    f != Flavor::Ferret
}

pub(crate) enum State {
    /// `system.profile` read past `after` (ms); `seen` are the entries
    /// already read at exactly `after`. `restore` puts the level back.
    Profile { db: Database, after: i64, seen: HashSet<String>, restore: Option<Document> },
    /// `currentOp`, looked at repeatedly; `first` is when each operation was
    /// first seen to start (so it stays the same from one look to the next).
    Sampled { sampler: Sampler, db: String, first: HashMap<String, String> },
}

pub(crate) async fn start(s: &MongoSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    if !supported(s.flavor) {
        return Err(Error::Unsupported(
            "FerretDB no tiene profiler y su currentOp no muestra el comando completo ni el cliente".into(),
        ));
    }
    let db = if opts.database.is_empty() { s.db.clone() } else { s.client.database(&opts.database) };
    let now = server_now(&db).await?;
    let sampled = |note: String| {
        let state = State::Sampled { sampler: Sampler::new(stamp(now)), db: db.name().to_string(), first: HashMap::new() };
        (state, ProfilerStarted::new(ProfilerMode::Sampled, "currentOp").note(note))
    };
    if s.flavor == Flavor::DocumentDb {
        return Ok(sampled(
            "Amazon DocumentDB envía su profiler a CloudWatch Logs: se muestran las operaciones en curso.".into(),
        ));
    }
    let status = match db.run_command(doc! { "profile": -1, "comment": s.tag.as_str() }).await {
        Ok(r) => r,
        Err(e) => {
            return Ok(sampled(format!(
                "El comando profile no está disponible ({}): se muestran las operaciones en curso.",
                err(e)
            )))
        }
    };
    let was = num(&status, &["was"]).unwrap_or(0.0) as i32;
    let rate = num(&status, &["sampleRate"]).unwrap_or(1.0);
    let slowms = num(&status, &["slowms"]).unwrap_or(100.0);
    let filtered = status.get_document("filter").is_ok();
    let profile = |restore: Option<Document>| State::Profile {
        db: db.clone(),
        after: now,
        seen: HashSet::new(),
        restore,
    };
    // Reads are the documents examined (`docsExamined`); the index keys
    // examined (`keysExamined`) stay in the detail: documents are what cost
    // I/O and what the writes count too.
    let started = ProfilerStarted::new(ProfilerMode::Complete, format!("{}.system.profile", db.name()))
        .units(Some("documentos"), Some("documentos"));
    if was == 2 && rate >= 1.0 {
        return Ok((profile(None), started));
    }
    // What the profiler records as it is.
    let partial = || {
        let what = if was == 2 {
            format!("todas las operaciones con una muestra del {} %", rate * 100.0)
        } else if filtered {
            "solo las operaciones que cumplen el filtro del profiler".to_string()
        } else {
            format!("solo las operaciones de más de {slowms} ms")
        };
        let rate = if was == 1 && rate < 1.0 { format!(" (y una muestra del {} %)", rate * 100.0) } else { String::new() };
        format!("El profiling de «{}» está en nivel {was}: se registran {what}{rate}.", db.name())
    };
    if !opts.change_server {
        if was == 0 {
            return Ok(sampled(format!(
                "El profiling de «{}» está apagado y la conexión es de solo lectura: se muestran las operaciones en curso.",
                db.name()
            )));
        }
        return Ok((profile(None), started.note(partial())));
    }
    // `sampleRate` only when it changes: setting it takes `enableProfiler` on
    // every database, the level alone only on this one.
    let sets_rate = rate < 1.0;
    let mut set = doc! { "profile": 2, "comment": s.tag.as_str() };
    if sets_rate {
        set.insert("sampleRate", 1.0);
    }
    if let Err(e) = db.run_command(set).await {
        let e = err(e);
        if was == 0 {
            return Ok(sampled(format!(
                "No se pudo activar el profiling de «{}» ({e}): se muestran las operaciones en curso.",
                db.name()
            )));
        }
        return Ok((profile(None), started.note(format!("{} No se pudo subirlo a 2 ({e}).", partial()))));
    }
    let mut started = started;
    if was != 2 {
        started = started.change(format!("Nivel de profiling de «{}» = 2 (estaba en {was})", db.name()));
    }
    let mut restore = doc! { "profile": was, "comment": s.tag.as_str() };
    if sets_rate {
        started = started.change(format!("sampleRate = 1 (estaba en {rate})"));
        restore.insert("sampleRate", rate);
    }
    Ok((
        profile(Some(restore)),
        started.note("El profiling es por servidor: en un replica set solo se ve lo que corre en el primario."),
    ))
}

pub(crate) async fn poll(s: &MongoSession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    match state {
        State::Profile { db, after, seen, .. } => {
            let own_ns = format!("{}.system.profile", db.name());
            let cmd = doc! {
                "find": "system.profile",
                "filter": {
                    "ts": { "$gte": Bson::DateTime(mongodb::bson::DateTime::from_millis(*after)) },
                    "ns": { "$ne": own_ns },
                    "command.comment": { "$ne": s.tag.as_str() },
                    // Continuations of a cursor, not statements.
                    "op": { "$nin": ["getmore", "killcursors"] },
                },
                "sort": { "ts": 1 },
                "limit": BATCH,
                "singleBatch": true,
                "comment": s.tag.as_str(),
            };
            let r = db.run_command(cmd).await.map_err(err)?;
            let rows = r.get_document("cursor").and_then(|c| c.get_array("firstBatch")).map(|a| a.as_slice()).unwrap_or(&[]);
            let mut out = Vec::new();
            for d in rows.iter().filter_map(Bson::as_document) {
                let Ok(ts) = d.get_datetime("ts").map(|t| t.timestamp_millis()) else { continue };
                let key = Bson::Document(d.clone()).into_relaxed_extjson().to_string();
                if ts == *after && seen.contains(&key) {
                    continue;
                }
                if ts != *after {
                    *after = ts;
                    seen.clear();
                }
                seen.insert(key);
                out.push(entry(d, ts));
            }
            Ok(out)
        }
        State::Sampled { sampler, db, first } => {
            let mut out = Vec::new();
            let until = Instant::now() + SAMPLE_FOR;
            loop {
                let r = s
                    .client
                    .database("admin")
                    .run_command(doc! { "currentOp": true, "active": true, "comment": s.tag.as_str() })
                    .await
                    .map_err(err)?;
                let ops: Vec<&Document> =
                    r.get_array("inprog").map(|a| a.iter().filter_map(Bson::as_document).collect()).unwrap_or_default();
                let samples: Vec<Sample> = ops.into_iter().filter_map(|op| sample(op, db, &s.tag, first)).collect();
                let live: HashSet<&str> = samples.iter().map(|s| s.session.as_str()).collect();
                first.retain(|k, _| live.contains(k.as_str()));
                out.extend(sampler.feed(samples));
                if Instant::now() + SAMPLE_EVERY > until {
                    break;
                }
                tokio::time::sleep(SAMPLE_EVERY).await;
            }
            out.sort_by(|a, b| a.time.cmp(&b.time));
            Ok(out)
        }
    }
}

/// Put back the profiling level `start` changed.
pub(crate) async fn stop(state: State) -> Result<()> {
    if let State::Profile { db, restore: Some(cmd), .. } = state {
        db.run_command(cmd).await.map_err(err)?;
    }
    Ok(())
}

/// The server's clock, in ms.
async fn server_now(db: &Database) -> Result<i64> {
    let r = db.run_command(doc! { "hello": 1 }).await.map_err(err)?;
    Ok(r.get_datetime("localTime").map(|t| t.timestamp_millis()).unwrap_or_else(|_| Utc::now().timestamp_millis()))
}

/// `YYYY-MM-DD HH:MM:SS.mmm` (UTC).
fn stamp(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|d| d.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
        .unwrap_or_default()
}

/// One `system.profile` entry (`ts` is when it ended).
fn entry(d: &Document, ts: i64) -> ProfiledStatement {
    let ms = num(d, &["millis"]);
    let ns = d.get_str("ns").unwrap_or("");
    let (database, coll) = ns.split_once('.').unwrap_or((ns, ""));
    let cmd = d.get_document("command").cloned().unwrap_or_default();
    let rows = ["nreturned", "ninserted", "nModified", "ndeleted"].iter().find_map(|k| num(d, &[k])).map(|n| n as u64);
    let error = d.get_str("errMsg").ok().map(|e| match d.get_str("errName") {
        Ok(name) => format!("{e} ({name})"),
        Err(_) => e.to_string(),
    });
    let mut detail = vec![d.get_str("op").unwrap_or("").to_string()];
    if let Ok(p) = d.get_str("planSummary") {
        detail.push(p.to_string());
    }
    if let Some(n) = num(d, &["keysExamined"]) {
        detail.push(format!("claves examinadas: {n}"));
    }
    // Documents inserted, modified, upserted or deleted; None when the
    // operation didn't write (no counter at all).
    let written: Vec<f64> = ["ninserted", "nModified", "nUpserted", "ndeleted"].iter().filter_map(|k| num(d, &[k])).collect();
    ProfiledStatement {
        time: stamp(ts - ms.unwrap_or(0.0) as i64),
        duration_ms: ms,
        text: shell_text(&cmd, coll),
        database: Some(database.to_string()).filter(|d| !d.is_empty()),
        user: d.get_str("user").ok().filter(|u| !u.is_empty()).map(str::to_string),
        client: client(d),
        rows,
        error,
        detail: Some(detail.join(" · ")).filter(|d| !d.is_empty()),
        application: application(d),
        // CPU time is only recorded on Linux (MongoDB 6.3+).
        cpu_ms: num(d, &["cpuNanos"]).map(|n| n / 1e6),
        reads: num(d, &["docsExamined"]).map(|n| n as u64),
        writes: (!written.is_empty()).then(|| written.iter().sum::<f64>() as u64),
    }
}

/// One running operation from `currentOp`, unless it's the profiler's own,
/// outside `db`, or the server's (no command).
fn sample(op: &Document, db: &str, tag: &str, first: &mut HashMap<String, String>) -> Option<Sample> {
    let cmd = op.get_document("command").ok().filter(|c| !c.is_empty())?;
    if cmd.get_str("comment") == Ok(tag) {
        return None;
    }
    // The drivers' monitors wait on `hello` for seconds at a time.
    if cmd.contains_key("maxAwaitTimeMS") && (cmd.contains_key("hello") || cmd.contains_key("isMaster") || cmd.contains_key("ismaster")) {
        return None;
    }
    let ns = op.get_str("ns").unwrap_or("");
    let (database, coll) = ns.split_once('.').unwrap_or((ns, ""));
    if !db.is_empty() && database != db {
        return None;
    }
    let opid = op.get("opid").map(|v| v.to_string())?;
    let us = num(op, &["microsecs_running"]).or_else(|| num(op, &["secs_running"]).map(|s| s * 1e6)).unwrap_or(0.0);
    let started = first
        .entry(opid.clone())
        .or_insert_with(|| {
            let now = match op.get("currentOpTime") {
                Some(Bson::DateTime(t)) => t.timestamp_millis(),
                Some(Bson::String(t)) => DateTime::parse_from_rfc3339(t).map(|t| t.timestamp_millis()).unwrap_or_default(),
                _ => Utc::now().timestamp_millis(),
            };
            stamp(now - (us / 1000.0) as i64)
        })
        .clone();
    let user = op
        .get_array("effectiveUsers")
        .ok()
        .and_then(|a| a.first())
        .and_then(Bson::as_document)
        .and_then(|u| Some(format!("{}@{}", u.get_str("user").ok()?, u.get_str("db").unwrap_or("admin"))));
    let mut detail = vec![op.get_str("op").unwrap_or("").to_string()];
    if let Ok(p) = op.get_str("planSummary") {
        detail.push(p.to_string());
    }
    Some(Sample {
        session: opid,
        started,
        text: shell_text(cmd, coll),
        running: true,
        duration_ms: Some(us / 1000.0),
        database: Some(database.to_string()).filter(|d| !d.is_empty()),
        user,
        client: client(op),
        application: application(op),
        detail: Some(detail.join(" · ")).filter(|d| !d.is_empty()),
        ..Default::default()
    })
}

/// The client's address.
fn client(d: &Document) -> Option<String> {
    d.get_str("client").or_else(|_| d.get_str("client_s")).ok().filter(|a| !a.is_empty()).map(str::to_string)
}

/// The driver's `appName`.
fn application(d: &Document) -> Option<String> {
    d.get_str("appName").ok().filter(|a| !a.is_empty()).map(str::to_string)
}

fn json(v: &Bson) -> String {
    v.clone().into_relaxed_extjson().to_string()
}

fn coll_ref(name: &str) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        format!("db.{name}")
    } else {
        format!("db.getCollection({})", serde_json::Value::String(name.into()))
    }
}

/// The command as it would be written in the shell (`db.c.find({…})`), or
/// `db.runCommand({…})` for the rest.
fn shell_text(cmd: &Document, coll: &str) -> String {
    let mut cmd = cmd.clone();
    for k in NOISE {
        cmd.remove(k);
    }
    let name = cmd.keys().next().cloned().unwrap_or_default();
    let target = cmd.get_str(&name).map(coll_ref).unwrap_or_else(|_| coll_ref(coll));
    let empty = Bson::Document(Document::new());
    let get = |k: &str| cmd.get(k).unwrap_or(&empty);
    let docs = |k: &str| cmd.get_array(k).map(|a| a.iter().filter_map(Bson::as_document).collect::<Vec<_>>()).unwrap_or_default();
    match name.as_str() {
        "find" => {
            let mut s = format!("{target}.find({}", json(get("filter")));
            if let Some(p) = cmd.get("projection") {
                s += &format!(", {}", json(p));
            }
            s.push(')');
            for (k, m) in [("sort", "sort"), ("skip", "skip"), ("limit", "limit"), ("hint", "hint")] {
                if let Some(v) = cmd.get(k) {
                    s += &format!(".{m}({})", json(v));
                }
            }
            s
        }
        "aggregate" if cmd.get_str("aggregate").is_ok() => format!("{target}.aggregate({})", json(get("pipeline"))),
        "count" => format!("{target}.count({})", json(get("query"))),
        "distinct" => format!("{target}.distinct({}, {})", json(get("key")), json(get("query"))),
        "insert" => {
            let d = docs("documents");
            match d.as_slice() {
                [one] => format!("{target}.insertOne({})", json(&Bson::Document((*one).clone()))),
                _ => format!("{target}.insertMany({})", json(get("documents"))),
            }
        }
        "update" => docs("updates")
            .iter()
            .map(|u| {
                let m = if u.get_bool("multi").unwrap_or(false) { "updateMany" } else { "updateOne" };
                let empty = Bson::Document(Document::new());
                format!("{target}.{m}({}, {})", json(u.get("q").unwrap_or(&empty)), json(u.get("u").unwrap_or(&empty)))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        "delete" => docs("deletes")
            .iter()
            .map(|d| {
                let m = if num(d, &["limit"]) == Some(1.0) { "deleteOne" } else { "deleteMany" };
                format!("{target}.{m}({})", json(d.get("q").unwrap_or(&Bson::Document(Document::new()))))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => format!("db.runCommand({})", json(&Bson::Document(cmd))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_read_as_shell_calls() {
        let find = doc! { "find": "people", "filter": { "age": { "$gt": 30 } }, "sort": { "age": -1 }, "limit": 5, "lsid": { "id": 1 }, "$db": "x" };
        assert_eq!(shell_text(&find, "people"), r#"db.people.find({"age":{"$gt":30}}).sort({"age":-1}).limit(5)"#);
        let upd = doc! { "update": "my-coll", "updates": [{ "q": { "a": 1 }, "u": { "$set": { "b": 2 } }, "multi": true }] };
        assert_eq!(shell_text(&upd, ""), r#"db.getCollection("my-coll").updateMany({"a":1}, {"$set":{"b":2}})"#);
        let del = doc! { "delete": "c", "deletes": [{ "q": {}, "limit": 1 }] };
        assert_eq!(shell_text(&del, ""), "db.c.deleteOne({})");
        let ins = doc! { "insert": "c", "documents": [{ "a": 1 }] };
        assert_eq!(shell_text(&ins, ""), r#"db.c.insertOne({"a":1})"#);
        assert_eq!(shell_text(&doc! { "dbStats": 1, "$db": "x" }, ""), r#"db.runCommand({"dbStats":1})"#);
    }

    #[test]
    fn profile_entries_carry_cpu_reads_and_writes() {
        let ts = 1_706_708_700_000;
        let upd = doc! {
            "op": "update", "ns": "shop.orders", "millis": 12, "command": { "q": {}, "u": {} },
            "keysExamined": 4, "docsExamined": 7, "nMatched": 3, "nModified": 3, "cpuNanos": 2_500_000i64,
        };
        let e = entry(&upd, ts);
        assert_eq!((e.cpu_ms, e.reads, e.writes), (Some(2.5), Some(7), Some(3)));
        assert_eq!(e.detail.as_deref(), Some("update · claves examinadas: 4"));
        let find = doc! { "op": "query", "ns": "shop.orders", "millis": 1, "nreturned": 2, "docsExamined": 2 };
        let e = entry(&find, ts);
        assert_eq!((e.cpu_ms, e.reads, e.writes, e.rows), (None, Some(2), None, Some(2)));
    }

    #[test]
    fn stamps_keep_milliseconds() {
        assert_eq!(stamp(1_706_708_700_000), "2024-01-31 13:45:00.000");
    }
}
