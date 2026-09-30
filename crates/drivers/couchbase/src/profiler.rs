//! The profiler ([`dbine_driver::profiler`]): `system:completed_requests`,
//! complete. The Query service keeps there the requests that took longer
//! than `queryCompletedThreshold` (1000 ms by default), the last
//! `queryCompletedLimit` of them (4000) on every query node. When allowed,
//! the profiler sets the threshold to 0 for the whole cluster (the cluster
//! manager's `/settings/querySettings`) and puts it back at stop; read-only,
//! it shows what the threshold already lets through.
//!
//! A request names no bucket: it's scoped by its `query_context` or by the
//! bucket's name in the statement. The profiler's own requests carry a
//! `client_context_id` of their own and are left out.
//!
//! Each request carries its CPU time (`cpuTime`), the documents it fetched
//! from the data service (`phaseCounts.fetch`: reads) and the ones it
//! changed (`mutations`: writes). A query covered by an index fetches none.

use crate::{context_id, text, CbSession};
use dbine_driver::{ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use serde_json::{json, Value};
use std::collections::HashSet;

/// The profiler's requests' `client_context_id` starts with it.
const OWN: &str = "dbine-profiler-";

pub(crate) struct State {
    /// Server time (ms) when profiling began.
    since: i64,
    bucket: Option<String>,
    /// Requests already reported, among those still in the list.
    seen: HashSet<String>,
    /// The threshold to put back at stop.
    restore: Option<i64>,
}

async fn post(s: &CbSession, stmt: &str, args: Value) -> Result<Vec<Value>> {
    let mut body = json!({ "statement": stmt, "client_context_id": format!("{OWN}{}", context_id()) });
    for (k, v) in args.as_object().into_iter().flatten() {
        body[format!("${k}")] = v.clone();
    }
    let v = s.cancel.run(s.conn.post_query(&body)).await?;
    Ok(v.get("results").and_then(Value::as_array).cloned().unwrap_or_default())
}

async fn set_threshold(s: &CbSession, ms: i64) -> Result<()> {
    let url = format!("{}/settings/querySettings", s.conn.mgmt);
    s.conn.mgmt_send(s.conn.http.post(url).form(&[("queryCompletedThreshold", ms.to_string())])).await.map(|_| ())
}

pub(crate) async fn start(s: &CbSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    s.cancel.flag.store(false, std::sync::atomic::Ordering::SeqCst);
    let bucket = Some(opts.database.trim()).filter(|b| !b.is_empty()).map(str::to_string).or_else(|| s.bucket.clone());
    let since = post(s, "SELECT RAW NOW_MILLIS()", json!({})).await?.first().and_then(Value::as_f64).unwrap_or(0.0) as i64;
    let mut started =
        ProfilerStarted::new(ProfilerMode::Complete, "system:completed_requests").units(Some("documentos"), Some("documentos"));
    let settings = s.cancel.run(s.conn.mgmt_get("/settings/querySettings")).await;
    let was = settings.as_ref().ok().and_then(|v| v.get("queryCompletedThreshold")).and_then(Value::as_i64);
    let mut restore = None;
    let partial = |ms: i64| format!("Couchbase solo registra las consultas de más de {ms} ms (queryCompletedThreshold).");
    match (was, &settings) {
        (Some(0), _) => {}
        (Some(ms), _) if opts.change_server => match set_threshold(s, 0).await {
            Ok(()) => {
                started = started.change(format!("queryCompletedThreshold = 0 (estaba en {ms} ms)"));
                restore = Some(ms);
            }
            Err(e) => started = started.note(format!("{} No se pudo bajar a 0: {e}", partial(ms))),
        },
        (Some(ms), _) => started = started.note(partial(ms)),
        (None, Err(e)) => {
            started = started.note(format!(
                "No se pudo leer la configuración del servicio de consultas ({e}): solo se ven las consultas que superan su umbral."
            ))
        }
        (None, Ok(_)) => {}
    }
    let state = State { since, bucket, seen: HashSet::new(), restore };
    Ok((state, started))
}

pub(crate) async fn poll(s: &CbSession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    // Every request since the start still in the list (a long one lands
    // there when it ends, after shorter ones that began later); `seen`
    // leaves out those already reported.
    let scope = if state.bucket.is_some() {
        "AND (CONTAINS(IFMISSINGORNULL(r.queryContext, \"\"), $bucket) OR CONTAINS(r.statement, $bucket))"
    } else {
        ""
    };
    let stmt = format!(
        "SELECT r.requestId AS id, STR_TO_MILLIS(r.requestTime) AS ms, r.elapsedTime AS elapsed, \
                IFMISSINGORNULL(r.statement, r.preparedText) AS stmt, r.queryContext AS ctx, r.users AS usr, \
                r.remoteAddr AS addr, r.userAgent AS agent, r.resultCount AS res, r.mutations AS mut, \
                r.errors AS errors, r.statementType AS kind, r.state AS state, r.node AS node, \
                r.cpuTime AS cpu, r.phaseCounts AS phases \
         FROM system:completed_requests r \
         WHERE STR_TO_MILLIS(r.requestTime) >= $since \
           AND (r.clientContextID IS MISSING OR r.clientContextID IS NULL OR NOT r.clientContextID LIKE \"{OWN}%\") \
           {scope} \
         ORDER BY STR_TO_MILLIS(r.requestTime)"
    );
    let rows = post(s, &stmt, json!({ "since": state.since, "bucket": state.bucket })).await?;
    let mut out = Vec::new();
    let mut present = HashSet::new();
    for r in rows {
        let id = text(r.get("id").unwrap_or(&Value::Null));
        present.insert(id.clone());
        if !state.seen.insert(id) {
            continue;
        }
        out.push(statement(&r));
    }
    // Requests that fell off the end of the list won't come back.
    state.seen.retain(|id| present.contains(id));
    Ok(out)
}

/// Put the threshold back.
pub(crate) async fn stop(s: &CbSession, state: State) -> Result<()> {
    if let Some(ms) = state.restore {
        s.cancel.flag.store(false, std::sync::atomic::Ordering::SeqCst);
        set_threshold(s, ms).await?;
    }
    Ok(())
}

fn statement(r: &Value) -> ProfiledStatement {
    let get = |k: &str| r.get(k).filter(|v| !v.is_null()).map(text).filter(|t| !t.is_empty());
    let ms = r.get("ms").and_then(Value::as_f64).unwrap_or(0.0) as i64;
    let errors: Vec<String> = r
        .get("errors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|e| e.get("msg").map(text).unwrap_or_else(|| e.to_string()))
        .collect();
    let database = get("ctx").map(|c| {
        // `default:`bucket`.`scope`` → bucket.scope
        let c = c.strip_prefix("default:").unwrap_or(&c).replace('`', "");
        c.trim_end_matches("._default").to_string()
    });
    let phase = |k: &str| r.get("phases").and_then(|p| p.get(k)).and_then(Value::as_u64);
    // Index entries scanned stay in the detail; reads are the documents.
    let scanned = match (phase("indexScan"), phase("primaryScan")) {
        (None, None) => None,
        (i, p) => Some(format!("entradas de índice: {}", i.unwrap_or(0) + p.unwrap_or(0))),
    };
    let detail: Vec<String> =
        [get("kind"), get("state").filter(|s| s != "completed"), scanned, get("node")].into_iter().flatten().collect();
    ProfiledStatement {
        time: stamp(ms),
        duration_ms: get("elapsed").as_deref().and_then(go_duration),
        text: get("stmt").unwrap_or_default(),
        database,
        user: get("usr").map(|u| u.trim_start_matches("builtin:").trim_start_matches("local:").to_string()),
        client: get("addr"),
        rows: r.get("mut").and_then(Value::as_u64).or_else(|| r.get("res").and_then(Value::as_u64)),
        error: Some(errors.join("; ")).filter(|e| !e.is_empty()),
        detail: Some(detail.join(" · ")).filter(|d| !d.is_empty()),
        application: get("agent"),
        cpu_ms: get("cpu").as_deref().and_then(go_duration),
        reads: phase("fetch"),
        writes: r.get("mut").and_then(Value::as_u64),
    }
}

/// A Go duration (`1m2.5s`, `307.666µs`, `12ms`) in ms.
fn go_duration(s: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut rest = s.trim();
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let n = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
        let value: f64 = rest[..n].parse().ok()?;
        rest = &rest[n..];
        let u = rest.find(|c: char| c.is_ascii_digit() || c == '.').unwrap_or(rest.len());
        let factor = match &rest[..u] {
            "h" => 3_600_000.0,
            "m" => 60_000.0,
            "s" => 1000.0,
            "ms" => 1.0,
            "µs" | "us" | "μs" => 0.001,
            "ns" => 0.000_001,
            _ => return None,
        };
        total += value * factor;
        rest = &rest[u..];
    }
    Some(total)
}

/// Unix ms as `YYYY-MM-DD HH:MM:SS.mmm` (UTC).
fn stamp(ms: i64) -> String {
    let (secs, milli) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{milli:03}", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(go_duration("307.666µs"), Some(0.307666));
        assert_eq!(go_duration("1m2.5s"), Some(62_500.0));
        assert_eq!(go_duration("12ms"), Some(12.0));
        assert_eq!(go_duration("x"), None);
    }

    #[test]
    fn stamps() {
        assert_eq!(stamp(1_706_708_700_123), "2024-01-31 13:45:00.123");
        assert_eq!(stamp(951_782_400_000), "2000-02-29 00:00:00.000");
    }

    #[test]
    fn requests() {
        let r = json!({
            "id": "a", "ms": 1_706_708_700_123i64, "elapsed": "1.5ms", "stmt": "SELECT 1", "ctx": "default:`b`.`_default`",
            "usr": "builtin:Administrator", "addr": "10.0.0.1:1", "agent": "curl", "res": 1, "kind": "SELECT", "state": "completed",
            "errors": [{ "code": 1, "msg": "boom" }]
        });
        let s = statement(&r);
        assert_eq!((s.database.as_deref(), s.user.as_deref(), s.error.as_deref()), (Some("b"), Some("Administrator"), Some("boom")));
        assert_eq!((s.duration_ms, s.rows, s.client.as_deref()), (Some(1.5), Some(1), Some("10.0.0.1:1")));
        assert_eq!(s.application.as_deref(), Some("curl"));
        assert_eq!((s.cpu_ms, s.reads, s.writes), (None, None, None));
    }

    #[test]
    fn request_costs() {
        let r = json!({
            "id": "b", "ms": 0, "stmt": "SELECT * FROM b WHERE i > 10", "kind": "SELECT", "res": 39, "cpu": "2ms",
            "phases": { "fetch": 50, "filter": 39, "primaryScan": 50, "primaryScan.GSI": 50 }
        });
        let s = statement(&r);
        assert_eq!((s.cpu_ms, s.reads, s.writes, s.rows), (Some(2.0), Some(50), None, Some(39)));
        assert_eq!(s.detail.as_deref(), Some("SELECT · entradas de índice: 50"));
        let del = json!({ "id": "c", "ms": 0, "stmt": "DELETE FROM b", "mut": 50, "res": 0, "phases": { "indexScan": 50 } });
        let s = statement(&del);
        assert_eq!((s.reads, s.writes, s.rows), (None, Some(50), Some(50)));
    }
}
