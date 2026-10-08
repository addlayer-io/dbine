//! "Comparar": each version runs on the same read-only session, once to
//! warm up and then N times, with its rows hashed as they arrive (none is
//! kept) up to a cap. A version whose result differs from the original's is
//! not equivalent. What writes is never run: it gets its estimated plan only.

use dbine_driver::{Plan, QueryOutcome, ResultColumn, RowSink, RowSinkRef, Session};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Clone, Deserialize)]
pub struct Version {
    pub id: String,
    pub sql: String,
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Measured runs, after the warm-up.
    pub runs: u32,
    /// Rows hashed at most; past it the result isn't compared.
    pub max_rows: usize,
    /// The original orders its rows: order counts.
    pub ordered: bool,
    /// Run it (a read); else only its estimated plan.
    pub execute: bool,
    pub explain: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Measure {
    pub id: String,
    pub executed: bool,
    pub error: Option<String>,
    pub runs_ms: Vec<f64>,
    pub min_ms: Option<f64>,
    pub avg_ms: Option<f64>,
    pub rows: Option<u64>,
    /// More rows than the cap: the result wasn't compared.
    pub truncated: bool,
    pub checksum: Option<String>,
    /// Same result as the original; `None`: it couldn't be checked.
    pub equivalent: Option<bool>,
    /// The estimated plan's cost (the root's), when the engine gives one.
    pub cost: Option<f64>,
    pub plans: Vec<Plan>,
    pub plan_error: Option<String>,
}

/// Hashes the rows of a run as they arrive.
struct Hasher {
    cap: usize,
    ordered: bool,
    rows: u64,
    shape: Vec<usize>,
    chain: Sha256,
    sum: u128,
    mix: u128,
}

impl Hasher {
    fn new(cap: usize, ordered: bool) -> Self {
        Self { cap, ordered, rows: 0, shape: Vec::new(), chain: Sha256::new(), sum: 0, mix: 0 }
    }

    fn checksum(&self) -> String {
        let mut h = Sha256::new();
        h.update(format!("{:?}|{}|", self.shape, self.rows));
        if self.ordered {
            h.update(self.chain.clone().finalize());
        } else {
            h.update(self.sum.to_le_bytes());
            h.update(self.mix.to_le_bytes());
        }
        h.finalize().iter().take(12).map(|b| format!("{b:02x}")).collect()
    }
}

/// A cell the same way whatever the driver made of it: 1, 1.0 and 1e0 are one number.
fn cell(v: &serde_json::Value, out: &mut String) {
    match v {
        serde_json::Value::Number(n) => match n.as_f64() {
            Some(f) if f == 0.0 => out.push('0'),
            Some(f) => out.push_str(&format!("{f}")),
            None => out.push_str(&n.to_string()),
        },
        serde_json::Value::String(s) => {
            out.push('"');
            out.push_str(s);
            out.push('"');
        }
        other => out.push_str(&other.to_string()),
    }
}

impl RowSink for Hasher {
    fn begin(&mut self, _index: usize, columns: &[ResultColumn]) -> std::io::Result<()> {
        self.shape.push(columns.len());
        Ok(())
    }

    fn row(&mut self, _index: usize, row: &[serde_json::Value]) -> std::io::Result<()> {
        self.rows += 1;
        if self.rows as usize > self.cap {
            return Ok(());
        }
        let mut s = String::new();
        for v in row {
            cell(v, &mut s);
            s.push('\u{1f}');
        }
        let d = Sha256::digest(s.as_bytes());
        let mut first = [0u8; 16];
        first.copy_from_slice(&d[..16]);
        let x = u128::from_le_bytes(first);
        if self.ordered {
            self.chain.update(d);
        } else {
            // A multiset hash: the order of the rows doesn't matter.
            self.sum = self.sum.wrapping_add(x);
            self.mix = self.mix.wrapping_add(x.wrapping_mul(x | 1).rotate_left(17));
        }
        Ok(())
    }
}

struct Run {
    ms: f64,
    rows: u64,
    truncated: bool,
    checksum: String,
}

async fn run_once(session: &mut dyn Session, sql: &str, o: &Options) -> Result<Run, String> {
    let hasher = Arc::new(Mutex::new(Hasher::new(o.max_rows, o.ordered)));
    let sink: Arc<Mutex<dyn RowSink>> = hasher.clone();
    let mut out = QueryOutcome { sink: Some(RowSinkRef(sink)), ..Default::default() };
    let started = Instant::now();
    let r = session.execute(sql, o.max_rows.saturating_add(1), &mut out).await;
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    if let Err(e) = r {
        return Err(e.to_string());
    }
    if let Some(e) = out.error.clone().or_else(|| out.errors.first().map(|e| e.message.clone())) {
        return Err(e);
    }
    let h = hasher.lock().map_err(|_| "error interno".to_string())?;
    // Rows the driver kept itself (a sink it didn't use).
    let kept: u64 = out.results.iter().map(|r| r.rows.len() as u64).sum();
    if kept > 0 && h.rows == 0 {
        return Err("el driver no entregó las filas para compararlas".into());
    }
    Ok(Run { ms, rows: h.rows, truncated: h.rows as usize > o.max_rows, checksum: h.checksum() })
}

/// Measure one version. `cancelled` is checked between runs.
pub async fn measure(session: &mut dyn Session, v: &Version, o: &Options, cancelled: &(dyn Fn() -> bool + Sync)) -> Measure {
    let mut m = Measure { id: v.id.clone(), ..Default::default() };
    if o.execute {
        match run_once(session, &v.sql, o).await {
            Err(e) => m.error = Some(e),
            Ok(warm) => {
                m.executed = true;
                m.rows = Some(warm.rows);
                m.truncated = warm.truncated;
                m.checksum = Some(warm.checksum);
                for _ in 0..o.runs.max(1) {
                    if cancelled() {
                        break;
                    }
                    match run_once(session, &v.sql, o).await {
                        Ok(r) => m.runs_ms.push(r.ms),
                        Err(e) => {
                            m.error = Some(e);
                            break;
                        }
                    }
                }
                if !m.runs_ms.is_empty() {
                    m.min_ms = m.runs_ms.iter().copied().reduce(f64::min);
                    m.avg_ms = Some(m.runs_ms.iter().sum::<f64>() / m.runs_ms.len() as f64);
                }
            }
        }
    }
    if o.explain && !cancelled() {
        let mut out = QueryOutcome::default();
        match session.explain(&v.sql, false, 100, &mut out).await {
            Ok(()) => {
                let costs: Vec<f64> = out.plans.iter().filter_map(|p| p.root.total_cost).collect();
                m.cost = (!costs.is_empty()).then(|| costs.iter().sum());
                m.plans = out.plans;
            }
            Err(e) => m.plan_error = Some(e.to_string()),
        }
    }
    m
}

/// Each measure against the original's (the first): same rows, same checksum.
pub fn mark_equivalence(measures: &mut [Measure]) {
    let Some(first) = measures.first().cloned() else { return };
    let comparable = |m: &Measure| m.executed && m.error.is_none() && !m.truncated && m.checksum.is_some();
    for m in measures.iter_mut().skip(1) {
        m.equivalent = (comparable(&first) && comparable(m)).then(|| m.checksum == first.checksum && m.rows == first.rows);
    }
    if let Some(f) = measures.first_mut() {
        f.equivalent = comparable(f).then_some(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hash(rows: &[Vec<serde_json::Value>], ordered: bool) -> String {
        let mut h = Hasher::new(100, ordered);
        h.begin(0, &[ResultColumn { name: "a".into(), type_name: String::new() }, ResultColumn { name: "b".into(), type_name: String::new() }]).unwrap();
        for r in rows {
            h.row(0, r).unwrap();
        }
        h.checksum()
    }

    #[test]
    fn checksums_ignore_order_unless_asked() {
        let a = vec![vec![json!(1), json!("x")], vec![json!(2), json!(null)]];
        let b = vec![vec![json!(2), json!(null)], vec![json!(1.0), json!("x")]];
        assert_eq!(hash(&a, false), hash(&b, false));
        assert_ne!(hash(&a, true), hash(&b, true));
        // A duplicate row more is another result.
        let mut c = a.clone();
        c.push(vec![json!(1), json!("x")]);
        assert_ne!(hash(&a, false), hash(&c, false));
        // "1" (text) isn't 1 (a number), and NULL isn't 'null'.
        assert_ne!(hash(&[vec![json!("1"), json!(null)]], false), hash(&[vec![json!(1), json!("null")]], false));
    }

    #[test]
    fn equivalence_needs_both_sides_verifiable() {
        let ok = |id: &str, sum: &str, rows: u64| Measure { id: id.into(), executed: true, checksum: Some(sum.into()), rows: Some(rows), ..Default::default() };
        let mut ms = vec![ok("o", "aa", 2), ok("same", "aa", 2), ok("diff", "bb", 2), Measure { truncated: true, ..ok("cut", "aa", 2) }, Measure { id: "plan".into(), ..Default::default() }];
        mark_equivalence(&mut ms);
        let eq: Vec<Option<bool>> = ms.iter().map(|m| m.equivalent).collect();
        assert_eq!(eq, vec![Some(true), Some(true), Some(false), None, None]);
    }
}
