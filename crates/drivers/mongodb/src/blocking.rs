//! Blocking chains and ending an operation (the Monitor's "Bloqueos").
//!
//! MongoDB: `lockInfo` lists every lock resource with its granted and
//! pending requests; a pending request carries the waiting operation's
//! `opid`, a granted one either the running operation's `opid` or, for a
//! transaction between statements (its locks are stashed), its session id
//! in `debugInfo`. Who blocks whom is then the lock-mode conflict matrix:
//! a pending request waits for the conflicting granted ones, or for the
//! conflicting requests queued before it. `$currentOp` (with idle
//! sessions) fills in the user, client, statement and times.
//!
//! A plain write to a document an open transaction changed doesn't wait on
//! a lock: it retries on write conflicts until the transaction ends. Such
//! an operation (active, `writeConflicts` > 0) is shown waiting for the
//! oldest transaction holding locks on its collection.
//!
//! Ids: an operation is its `opid` ("845", or "shard:845" through mongos),
//! killed with `killOp`; an idle transaction is `lsid:<uuid>`, killed with
//! `killSessions` (its transaction aborts).
//!
//! Amazon DocumentDB: `$currentOp` reports `WaitState` and `blockedOn` (the
//! blocking opid, or "INTERNAL"), killed with `killOp`. FerretDB has no lock
//! waits to report nor `killOp`.

use crate::monitor::{compact, num};
use crate::{err, Flavor};
use dbine_driver::{BlockedSession, Error, Result};
use futures::TryStreamExt;
use mongodb::bson::{doc, Bson, Document};
use mongodb::Client;
use std::collections::BTreeSet;

const TXN_PREFIX: &str = "lsid:";

/// Who holds or waits for a lock.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Who {
    Op(String),
    Txn(String),
}

impl Who {
    fn id(&self) -> String {
        match self {
            Who::Op(o) => o.clone(),
            Who::Txn(u) => format!("{TXN_PREFIX}{u}"),
        }
    }
}

struct Request {
    mode: String,
    who: Option<Who>,
}

struct Resource {
    kind: String,
    /// The namespace or database, when the resource names one.
    name: Option<String>,
    granted: Vec<Request>,
    pending: Vec<Request>,
}

/// An opid as text (a number on mongod, "shard:n" through mongos).
fn opid(v: &Bson) -> Option<String> {
    match v {
        Bson::Int32(i) => Some(i.to_string()),
        Bson::Int64(i) => Some(i.to_string()),
        Bson::Double(f) if f.fract() == 0.0 => Some((*f as i64).to_string()),
        Bson::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn hyphenated(bytes: &[u8]) -> Option<String> {
    if bytes.len() != 16 {
        return None;
    }
    let h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Some(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

/// The session id (UUID text) of an operation or idle session.
fn lsid(op: &Document) -> Option<String> {
    match op.get_document("lsid").ok()?.get("id")? {
        Bson::Binary(b) => hyphenated(&b.bytes),
        _ => None,
    }
}

/// `lsid: { id: UUID("7ef3…"), uid: … }` in a lock request's `debugInfo`.
fn lsid_in_debug(s: &str) -> Option<String> {
    let at = s.find("UUID(\"")? + 6;
    let u = s.get(at..at + 36)?;
    valid_uuid(u).then(|| u.to_ascii_lowercase())
}

fn valid_uuid(u: &str) -> bool {
    u.len() == 36
        && u.char_indices().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() })
}

/// "{7382713655222838518: Collection, 465184627581756662, t.c}" → ("Collection", Some("t.c")).
fn resource_id(s: &str) -> (String, Option<String>) {
    let inner = s.trim().trim_start_matches('{').trim_end_matches('}');
    let rest = inner.split_once(':').map_or(inner, |(_, r)| r).trim();
    let parts: Vec<&str> = rest.split(", ").collect();
    let kind = parts.first().map(|k| k.trim().to_string()).unwrap_or_default();
    let name = (parts.len() > 2).then(|| parts[2..].join(", ").trim().to_string()).filter(|n| !n.is_empty());
    (kind, name)
}

fn request(d: &Document) -> Request {
    let who = d
        .get_document("clientInfo")
        .ok()
        .and_then(|c| c.get("opid"))
        .and_then(opid)
        .map(Who::Op)
        .or_else(|| d.get_str("debugInfo").ok().and_then(lsid_in_debug).map(Who::Txn));
    Request { mode: d.get_str("mode").unwrap_or("").to_string(), who }
}

fn resources(lock_info: &Document) -> Vec<Resource> {
    let list = lock_info.get_array("lockInfo").map(|a| a.as_slice()).unwrap_or(&[]);
    list.iter()
        .filter_map(Bson::as_document)
        .map(|r| {
            let (kind, name) = resource_id(r.get_str("resourceId").unwrap_or(""));
            let reqs = |k: &str| r.get_array(k).map(|a| a.iter().filter_map(Bson::as_document).map(request).collect()).unwrap_or_default();
            Resource { kind, name, granted: reqs("granted"), pending: reqs("pending") }
        })
        .collect()
}

/// Whether two lock modes can't be held together.
fn conflicts(a: &str, b: &str) -> bool {
    match (a, b) {
        ("IS", x) | (x, "IS") => x == "X",
        ("IX", "IX") => false,
        ("S", "S") => false,
        _ => true,
    }
}

/// An edge of the chain: `waiter` waits for `holder` (if known).
struct Wait {
    waiter: Who,
    holder: Option<Who>,
    what: String,
    object: Option<String>,
}

fn lock_waits(res: &[Resource]) -> Vec<Wait> {
    let mut out: Vec<Wait> = Vec::new();
    for r in res {
        for (i, p) in r.pending.iter().enumerate() {
            let Some(w) = p.who.clone() else { continue };
            if out.iter().any(|e| e.waiter == w) {
                continue; // One edge per waiter: the first resource it waits on.
            }
            let other = |q: &&Request| q.who.as_ref() != Some(&w) && conflicts(&p.mode, &q.mode);
            let holder = r
                .granted
                .iter()
                .filter(other)
                .find_map(|q| q.who.clone())
                .or_else(|| r.pending[..i].iter().filter(other).find_map(|q| q.who.clone()));
            let internal = holder.is_none() && r.granted.iter().any(|q| q.who.is_none() && conflicts(&p.mode, &q.mode));
            out.push(Wait {
                waiter: w,
                holder,
                what: format!(
                    "Esperando un bloqueo {} ({}){}",
                    p.mode,
                    r.kind,
                    if internal { " de una tarea interna" } else { "" }
                ),
                object: r.name.clone(),
            });
        }
    }
    out
}

/// The chain for MongoDB from `$currentOp` (with idle sessions) and `lockInfo`.
fn mongo_chain(ops: &[Document], lock_info: &Document) -> Vec<BlockedSession> {
    let res = resources(lock_info);
    let mut waits = lock_waits(&res);

    // Writes retrying on a document an open transaction holds.
    for op in ops {
        let Some(id) = op.get("opid").and_then(opid) else { continue };
        let conflicts_seen = num(op, &["writeConflicts"]).unwrap_or(0.0);
        if !op.get_bool("active").unwrap_or(false) || conflicts_seen <= 0.0 || waits.iter().any(|w| w.waiter == Who::Op(id.clone())) {
            continue;
        }
        let ns = op.get_str("ns").unwrap_or("");
        let own = lsid(op);
        let open_for = |u: &str| {
            ops.iter()
                .find(|o| lsid(o).as_deref() == Some(u))
                .and_then(|o| num(o, &["transaction", "timeOpenMicros"]))
                .unwrap_or(0.0)
        };
        let holder = res
            .iter()
            .filter(|r| r.kind == "Collection" && r.name.as_deref() == Some(ns))
            .flat_map(|r| r.granted.iter())
            .filter_map(|q| match &q.who {
                Some(Who::Txn(u)) if own.as_deref() != Some(u.as_str()) => Some(u.clone()),
                _ => None,
            })
            .max_by(|a, b| open_for(a).total_cmp(&open_for(b)));
        if let Some(u) = holder {
            waits.push(Wait {
                waiter: Who::Op(id),
                holder: Some(Who::Txn(u)),
                what: format!("Conflicto de escritura con una transacción abierta ({conflicts_seen} reintentos)"),
                object: Some(ns.to_string()).filter(|n| !n.is_empty()),
            });
        }
    }

    let find = |who: &Who| -> Option<&Document> {
        match who {
            Who::Op(o) => ops.iter().find(|d| d.get("opid").and_then(opid).as_deref() == Some(o.as_str())),
            // The idle session, or the operation the transaction is running.
            Who::Txn(u) => ops
                .iter()
                .filter(|d| lsid(d).as_deref() == Some(u.as_str()))
                .max_by_key(|d| d.get_str("type") == Ok("idleSession")),
        }
    };
    let mut out = Vec::new();
    let waiters: BTreeSet<Who> = waits.iter().map(|w| w.waiter.clone()).collect();
    let mut heads: BTreeSet<Who> = BTreeSet::new();
    for w in &waits {
        let op = find(&w.waiter);
        let mut s = describe(w.waiter.id(), op);
        s.blocked_by = w.holder.as_ref().map(Who::id);
        s.wait = Some(w.what.clone());
        s.object = w.object.clone().or(s.object);
        s.waited_ms = op.and_then(|o| num(o, &["microsecs_running"])).map(|us| (us / 1000.0) as u64).or(s.waited_ms);
        out.push(s);
        if let Some(h) = &w.holder {
            if !waiters.contains(h) {
                heads.insert(h.clone());
            }
        }
    }
    for h in &heads {
        let op = find(h);
        let mut s = describe(h.id(), op);
        if s.wait.is_none() {
            s.wait = Some(match h {
                Who::Txn(_) => "Transacción abierta".into(),
                Who::Op(_) => "Retiene el bloqueo".into(),
            });
        }
        out.push(s);
    }
    out
}

/// The facts `$currentOp` gives about an operation or idle session.
fn describe(id: String, op: Option<&Document>) -> BlockedSession {
    let mut s = BlockedSession { id, ..Default::default() };
    let Some(op) = op else { return s };
    s.user = op
        .get_array("effectiveUsers")
        .ok()
        .and_then(|a| a.first())
        .and_then(Bson::as_document)
        .and_then(|u| u.get_str("user").ok())
        .map(str::to_string);
    let client = op.get_str("client").or_else(|_| op.get_str("client_s")).ok();
    let app = op.get_str("appName").ok().filter(|a| !a.is_empty());
    s.client = match (client, app) {
        (Some(c), Some(a)) => Some(format!("{c} · {a}")),
        (c, a) => c.or(a).map(str::to_string),
    };
    let ns = op.get_str("ns").unwrap_or("");
    s.object = Some(ns.to_string()).filter(|n| n.contains('.'));
    s.database = ns
        .split('.')
        .next()
        .filter(|d| !d.is_empty())
        .map(str::to_string)
        .or_else(|| op.get_document("command").ok().and_then(|c| c.get_str("$db").ok()).map(str::to_string));
    if op.get_str("type") == Ok("idleSession") {
        s.wait = Some("inactiva con transacción abierta".into());
        s.waited_ms = num(op, &["transaction", "timeOpenMicros"]).map(|us| (us / 1000.0) as u64);
    } else {
        s.wait = op.get_str("WaitState").ok().map(str::to_string);
        s.waited_ms = num(op, &["microsecs_running"])
            .map(|us| (us / 1000.0) as u64)
            .or_else(|| num(op, &["secs_running"]).map(|v| (v * 1000.0) as u64));
        s.sql = op.get("command").filter(|c| c.as_document().is_some_and(|d| !d.is_empty())).map(|c| compact(c, 2000));
    }
    s
}

/// The chain for Amazon DocumentDB: `blockedOn` names the blocking opid.
fn documentdb_chain(ops: &[Document]) -> Vec<BlockedSession> {
    let by_id = |id: &str| ops.iter().find(|d| d.get("opid").and_then(opid).as_deref() == Some(id));
    let mut out = Vec::new();
    let mut heads = BTreeSet::new();
    let mut waiters = BTreeSet::new();
    for op in ops {
        let Some(id) = op.get("opid").and_then(opid) else { continue };
        let Some(on) = op.get("blockedOn").and_then(opid).filter(|b| by_id(b).is_some() && *b != id) else { continue };
        let mut s = describe(id.clone(), Some(op));
        s.wait = Some(op.get_str("WaitState").map(|w| format!("Esperando: {w}")).unwrap_or_else(|_| "Esperando un bloqueo".into()));
        s.blocked_by = Some(on.clone());
        heads.insert(on);
        waiters.insert(id);
        out.push(s);
    }
    for h in heads.difference(&waiters) {
        out.push(describe(h.clone(), by_id(h)));
    }
    out
}

async fn current_ops(client: &Client, idle_sessions: bool) -> Result<Vec<Document>> {
    let stage = if idle_sessions {
        doc! { "$currentOp": { "allUsers": true, "idleSessions": true } }
    } else {
        doc! { "$currentOp": { "allUsers": true } }
    };
    let cur = client.database("admin").aggregate([stage]).await.map_err(err)?;
    cur.try_collect().await.map_err(err)
}

pub async fn blocking(client: &Client, flavor: Flavor) -> Result<Vec<BlockedSession>> {
    match flavor {
        Flavor::Mongo => {
            let lock_info = client.database("admin").run_command(doc! { "lockInfo": 1 }).await.map_err(err)?;
            let ops = current_ops(client, true).await?;
            Ok(mongo_chain(&ops, &lock_info))
        }
        Flavor::DocumentDb => Ok(documentdb_chain(&current_ops(client, false).await?)),
        Flavor::Ferret => Err(unsupported_ferret()),
    }
}

fn unsupported_ferret() -> Error {
    Error::Unsupported("FerretDB no informa esperas por bloqueos ni permite terminar operaciones (no tiene killOp)".into())
}

/// An opid as `blocking` gives it: digits, or "shard:digits" through mongos.
fn valid_opid(id: &str) -> bool {
    let n = id.rsplit_once(':').map_or(id, |(shard, n)| {
        if !shard.is_empty() && shard.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
            n
        } else {
            ""
        }
    });
    !n.is_empty() && n.len() <= 19 && n.chars().all(|c| c.is_ascii_digit())
}

pub async fn kill(client: &Client, flavor: Flavor, id: &str) -> Result<()> {
    if flavor == Flavor::Ferret {
        return Err(unsupported_ferret());
    }
    let id = id.trim();
    let admin = client.database("admin");
    if let Some(u) = id.strip_prefix(TXN_PREFIX).filter(|_| flavor == Flavor::Mongo) {
        if !valid_uuid(u) {
            return Err(Error::Query(format!("«{id}» no es un id de sesión de MongoDB")));
        }
        // killSessions needs the session's user hash too: take the lsid as
        // $currentOp reports it.
        let ops = current_ops(client, true).await?;
        let Some(lsid) = ops.iter().find(|o| lsid(o).as_deref() == Some(&u.to_ascii_lowercase())).and_then(|o| o.get_document("lsid").ok())
        else {
            return Err(Error::Query("La transacción ya no está abierta.".into()));
        };
        admin.run_command(doc! { "killSessions": [lsid.clone()] }).await.map_err(err)?;
        return Ok(());
    }
    if !valid_opid(id) {
        return Err(Error::Query(format!("«{id}» no es un id de operación (opid)")));
    }
    let op = if id.contains(':') {
        Bson::String(id.to_string())
    } else {
        let n: i64 = id.parse().map_err(|_| Error::Query(format!("«{id}» no es un id de operación (opid)")))?;
        i32::try_from(n).map(Bson::Int32).unwrap_or(Bson::Int64(n))
    };
    admin.run_command(doc! { "killOp": 1, "op": op }).await.map_err(err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::spec::BinarySubtype;
    use mongodb::bson::Binary;

    const U: &str = "7ef3ba81-ed26-4753-9edd-e14bedcaf5c0";

    fn uuid_bin() -> Bson {
        let bytes: Vec<u8> = (0..16).map(|i| u8::from_str_radix(&U.replace('-', "")[i * 2..i * 2 + 2], 16).unwrap()).collect();
        Bson::Binary(Binary { subtype: BinarySubtype::Uuid, bytes })
    }

    fn lock_info() -> Document {
        let granted_txn = doc! { "mode": "IX", "debugInfo": format!("lsid: {{ id: UUID(\"{U}\"), uid: BinData(0, E3B0) }}") };
        doc! { "lockInfo": [
            { "resourceId": "{1: Collection, 2, t.c}",
              "granted": [granted_txn],
              "pending": [
                { "mode": "X", "clientInfo": { "desc": "conn12", "opid": 845 } },
                { "mode": "IX", "clientInfo": { "desc": "conn18", "opid": 855 } },
              ] },
            { "resourceId": "{3: Database, 4, t}", "granted": [ { "mode": "IX", "clientInfo": { "opid": 845 } } ], "pending": [] },
        ] }
    }

    #[test]
    fn parses_helpers() {
        assert_eq!(resource_id("{7382: Collection, 4651, t.c}"), ("Collection".into(), Some("t.c".into())));
        assert_eq!(resource_id("{1: Global, 1}"), ("Global".into(), None));
        assert_eq!(lsid_in_debug(&format!("lsid: {{ id: UUID(\"{U}\") }}")).as_deref(), Some(U));
        assert!(conflicts("X", "IS") && conflicts("IX", "S") && !conflicts("IX", "IX") && !conflicts("IS", "IX"));
        assert!(valid_opid("845") && valid_opid("shard01:845"));
        assert!(!valid_opid("845; drop") && !valid_opid("") && !valid_opid(":1") && !valid_opid("a b:1"));
        assert!(valid_uuid(U) && !valid_uuid("7ef3ba81"));
    }

    #[test]
    fn chains_lock_waits() {
        let ops = vec![
            doc! { "type": "op", "opid": 845, "active": true, "ns": "t.c", "microsecs_running": 2_500_000_i64,
                   "command": { "createIndexes": "c" }, "client": "127.0.0.1:1", "appName": "mongosh" },
            doc! { "type": "op", "opid": 855, "active": true, "ns": "t.c", "microsecs_running": 2_000_000_i64,
                   "command": { "q": { "_id": 1 } } },
            doc! { "type": "idleSession", "lsid": { "id": uuid_bin() }, "client": "127.0.0.1:2",
                   "transaction": { "timeOpenMicros": 6_000_000_i64 } },
        ];
        let c = mongo_chain(&ops, &lock_info());
        let get = |id: &str| c.iter().find(|s| s.id == id).unwrap_or_else(|| panic!("{id} in {c:?}"));
        let head = format!("lsid:{U}");
        assert_eq!(get("845").blocked_by.as_deref(), Some(head.as_str()));
        assert_eq!(get("845").waited_ms, Some(2500));
        assert!(get("845").wait.as_deref().unwrap().contains("X (Collection)"));
        assert_eq!(get("855").blocked_by.as_deref(), Some("845"), "queued behind the X request");
        let h = get(&head);
        assert_eq!(h.blocked_by, None);
        assert_eq!(h.waited_ms, Some(6000));
        assert_eq!(h.wait.as_deref(), Some("inactiva con transacción abierta"));
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn write_conflicts_wait_for_the_transaction() {
        let li = doc! { "lockInfo": [ { "resourceId": "{1: Collection, 2, t.c}", "granted": [
            { "mode": "IX", "clientInfo": { "opid": 900 } },
            { "mode": "IX", "debugInfo": format!("lsid: {{ id: UUID(\"{U}\") }}") },
        ], "pending": [] } ] };
        let ops = vec![
            doc! { "type": "op", "opid": 900, "active": true, "ns": "t.c", "writeConflicts": 213, "microsecs_running": 3_000_000_i64 },
            doc! { "type": "idleSession", "lsid": { "id": uuid_bin() } },
        ];
        let c = mongo_chain(&ops, &li);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].id, "900");
        assert_eq!(c[0].blocked_by.as_deref(), Some(format!("lsid:{U}").as_str()));
        assert!(c[0].wait.as_deref().unwrap().contains("Conflicto de escritura"));
        // Nothing blocked: nothing listed.
        assert!(mongo_chain(&[doc! { "opid": 1, "active": true }], &doc! { "lockInfo": [] }).is_empty());
    }

    #[test]
    fn documentdb_blocked_on() {
        let ops = vec![
            doc! { "opid": 75, "ns": "db.c", "WaitState": "CollectionLock", "blockedOn": 74, "command": { "find": "c" } },
            doc! { "opid": 74, "ns": "db.c", "secs_running": 30 },
            doc! { "opid": 76, "WaitState": "IO", "blockedOn": "INTERNAL" },
        ];
        let c = documentdb_chain(&ops);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].blocked_by.as_deref(), Some("74"));
        assert_eq!(c[1].id, "74");
        assert_eq!(c[1].waited_ms, Some(30_000));
    }
}
