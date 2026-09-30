//! The user's approval of each MCP write (docs/mcp.md, "Aprobaciones").
//!
//! The `execute` tool registers its request here and waits: the UI shows a
//! dialog (one at a time, the rest queued) and answers with
//! `mcp_answer_approval`. Unanswered within [`APPROVAL_TIMEOUT`], the request
//! is rejected. "Approve all" is kept per client id, in memory only: it
//! lasts until DBine closes, the user removes it, or the client is revoked.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

/// How long a write waits for the user's answer before it's rejected.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// What the dialog shows. Never holds secrets.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ApprovalRequest {
    pub id: String,
    pub client_id: String,
    pub client: String,
    pub connection: String,
    pub database: String,
    /// The engine's name (PostgreSQL, MongoDB…).
    pub engine: String,
    /// The editor language (`sql`, `json`…) and SQL dialect, for highlighting.
    pub language: String,
    pub dialect: String,
    pub code: String,
    /// When it's rejected if unanswered (RFC 3339).
    pub expires_at: String,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Only this one.
    Approve,
    Reject,
    /// This one and every later one of the same client, without asking.
    ApproveAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The user approved it in the dialog.
    Approved,
    /// The client had "approve all": nobody was asked.
    AutoApproved,
    Rejected,
    TimedOut,
}

/// Told about every change of the pending list; `new` when a request was
/// just added (the app brings its window forward then).
pub type Sink = Arc<dyn Fn(&[ApprovalRequest], bool) + Send + Sync>;

#[derive(Default)]
pub struct Approvals {
    pending: Mutex<Vec<(ApprovalRequest, oneshot::Sender<Decision>)>>,
    approve_all: Mutex<HashSet<String>>,
    sink: Mutex<Option<Sink>>,
}

impl Approvals {
    pub fn set_sink(&self, sink: Sink) {
        *self.sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    }

    /// The requests waiting for an answer, oldest first.
    pub fn pending(&self) -> Vec<ApprovalRequest> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(r, _)| r.clone()).collect()
    }

    fn notify(&self, new: bool) {
        let sink = self.sink.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(sink) = sink {
            sink(&self.pending(), new);
        }
    }

    pub fn approves_all(&self, client_id: &str) -> bool {
        self.approve_all.lock().unwrap_or_else(|e| e.into_inner()).contains(client_id)
    }

    /// The clients that currently approve everything.
    pub fn approve_all_clients(&self) -> HashSet<String> {
        self.approve_all.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Ask the user and wait for the answer, at most `timeout`. If the
    /// caller goes away (the HTTP client hung up), the request is withdrawn.
    pub async fn ask(&self, req: ApprovalRequest, timeout: Duration) -> Outcome {
        if self.approves_all(&req.client_id) {
            return Outcome::AutoApproved;
        }
        let id = req.id.clone();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).push((req, tx));
        self.notify(true);
        let _withdraw = Withdraw { approvals: self, id: &id };
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Decision::Approve | Decision::ApproveAll)) => Outcome::Approved,
            Ok(Ok(Decision::Reject)) | Ok(Err(_)) => Outcome::Rejected,
            Err(_) => Outcome::TimedOut,
        }
    }

    /// The user's answer. "Approve all" also approves the same client's
    /// other pending requests: from then on it isn't asked again.
    pub fn answer(&self, id: &str, decision: Decision) -> Result<(), String> {
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            let i = pending.iter().position(|(r, _)| r.id == id).ok_or_else(|| "esa solicitud ya no está pendiente: se respondió o venció".to_string())?;
            let (req, tx) = pending.remove(i);
            let _ = tx.send(decision);
            if decision == Decision::ApproveAll {
                self.approve_all.lock().unwrap_or_else(|e| e.into_inner()).insert(req.client_id.clone());
                let mut i = 0;
                while i < pending.len() {
                    if pending[i].0.client_id == req.client_id {
                        let (_, tx) = pending.remove(i);
                        let _ = tx.send(Decision::Approve);
                    } else {
                        i += 1;
                    }
                }
            }
        }
        self.notify(false);
        Ok(())
    }

    /// Stop approving everything for a client: it's asked again.
    pub fn clear_approve_all(&self, client_id: &str) {
        self.approve_all.lock().unwrap_or_else(|e| e.into_inner()).remove(client_id);
    }

    /// A revoked client: no approve-all, and its pending requests rejected.
    pub fn forget_client(&self, client_id: &str) {
        self.clear_approve_all(client_id);
        let removed = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            let before = pending.len();
            // Dropping the sender rejects the waiting call.
            pending.retain(|(r, _)| r.client_id != client_id);
            before != pending.len()
        };
        if removed {
            self.notify(false);
        }
    }

    fn withdraw(&self, id: &str) {
        let removed = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            let before = pending.len();
            pending.retain(|(r, _)| r.id != id);
            before != pending.len()
        };
        if removed {
            self.notify(false);
        }
    }
}

/// Takes a request off the list however `ask` ends (answer, timeout, or
/// the future dropped).
struct Withdraw<'a> {
    approvals: &'a Approvals,
    id: &'a str,
}

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        self.approvals.withdraw(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(id: &str, client: &str) -> ApprovalRequest {
        ApprovalRequest {
            id: id.into(),
            client_id: client.into(),
            client: client.into(),
            connection: "c".into(),
            database: "db".into(),
            engine: "PostgreSQL".into(),
            language: "sql".into(),
            dialect: "postgres".into(),
            code: "delete from t".into(),
            expires_at: String::new(),
            timeout_secs: 1,
        }
    }

    /// Wait until `n` requests are pending.
    async fn pending(a: &Approvals, n: usize) {
        for _ in 0..200 {
            if a.pending().len() == n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("expected {n} pending, got {}", a.pending().len());
    }

    #[tokio::test]
    async fn mcp_approval_approve_reject_timeout() {
        let a = Arc::new(Approvals::default());
        let events = Arc::new(Mutex::new(Vec::<(usize, bool)>::new()));
        let ev = events.clone();
        a.set_sink(Arc::new(move |list, new| ev.lock().unwrap().push((list.len(), new))));

        let t = tokio::spawn({
            let a = a.clone();
            async move { a.ask(req("1", "claude"), Duration::from_secs(5)).await }
        });
        pending(&a, 1).await;
        a.answer("1", Decision::Approve).unwrap();
        assert_eq!(t.await.unwrap(), Outcome::Approved);
        assert!(a.pending().is_empty());
        assert!(!a.approves_all("claude"));
        // Answering twice fails: it's gone.
        assert!(a.answer("1", Decision::Approve).is_err());

        let t = tokio::spawn({
            let a = a.clone();
            async move { a.ask(req("2", "claude"), Duration::from_secs(5)).await }
        });
        pending(&a, 1).await;
        a.answer("2", Decision::Reject).unwrap();
        assert_eq!(t.await.unwrap(), Outcome::Rejected);

        assert_eq!(a.ask(req("3", "claude"), Duration::from_millis(30)).await, Outcome::TimedOut);
        assert!(a.pending().is_empty());
        // The UI heard of each new request and of each removal.
        let ev = events.lock().unwrap().clone();
        assert!(ev.contains(&(1, true)) && ev.contains(&(0, false)), "{ev:?}");
    }

    #[tokio::test]
    async fn mcp_approval_approve_all_is_per_client_and_revocable() {
        let a = Arc::new(Approvals::default());
        let first = tokio::spawn({
            let a = a.clone();
            async move { a.ask(req("1", "claude"), Duration::from_secs(5)).await }
        });
        pending(&a, 1).await;
        let queued = tokio::spawn({
            let a = a.clone();
            async move { a.ask(req("2", "claude"), Duration::from_secs(5)).await }
        });
        let other = tokio::spawn({
            let a = a.clone();
            async move { a.ask(req("3", "codex"), Duration::from_secs(5)).await }
        });
        pending(&a, 3).await;
        a.answer("1", Decision::ApproveAll).unwrap();
        assert_eq!(first.await.unwrap(), Outcome::Approved);
        // The same client's queued request goes through; another client's waits.
        assert_eq!(queued.await.unwrap(), Outcome::Approved);
        assert_eq!(a.pending().len(), 1);
        assert!(a.approves_all("claude") && !a.approves_all("codex"));
        // Later requests aren't asked.
        assert_eq!(a.ask(req("4", "claude"), Duration::from_millis(10)).await, Outcome::AutoApproved);

        // Removing approve-all asks again.
        a.clear_approve_all("claude");
        assert_eq!(a.ask(req("5", "claude"), Duration::from_millis(10)).await, Outcome::TimedOut);

        // Revoking a client clears approve-all and rejects what it has pending.
        a.answer("3", Decision::ApproveAll).unwrap();
        assert_eq!(other.await.unwrap(), Outcome::Approved);
        assert!(a.approves_all("codex"));
        a.forget_client("codex");
        assert!(!a.approves_all("codex"));
        let waiting = tokio::spawn({
            let a = a.clone();
            async move { a.ask(req("6", "codex"), Duration::from_secs(5)).await }
        });
        pending(&a, 1).await;
        a.forget_client("codex");
        assert_eq!(waiting.await.unwrap(), Outcome::Rejected);
        assert!(a.pending().is_empty());
    }

    #[tokio::test]
    async fn mcp_approval_withdrawn_when_the_caller_goes_away() {
        let a = Arc::new(Approvals::default());
        let t = tokio::spawn({
            let a = a.clone();
            async move { a.ask(req("1", "claude"), Duration::from_secs(5)).await }
        });
        pending(&a, 1).await;
        t.abort();
        let _ = t.await;
        assert!(a.pending().is_empty());
    }
}
