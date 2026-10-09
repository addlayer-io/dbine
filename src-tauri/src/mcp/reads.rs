//! How `run_query` and `explain` read (docs/mcp.md, "Lecturas"). DBine's
//! lexical guard (`dbine_driver::read_only`) is defense in depth, not the
//! boundary: a read runs without asking only when the server enforces it
//! (`Session::run_read_only`: one statement in a read-only transaction).
//! An engine that can't (or a driver host published before the call)
//! answers `Unsupported`, and then the user approves the exact query first:
//! MCP clients in DBine's dialog (like `execute`), DBine's assistant in the
//! chat. Only after that it runs on the read-only session (guard plus the
//! driver's read-only mode).

use std::future::Future;

/// How a read was allowed to run: the activity log says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum How {
    /// The server enforced it as read-only; nobody was asked.
    Enforced,
    /// The user approved it in DBine's dialog.
    Approved,
    /// The client had "approve all reads".
    AutoApproved,
    /// DBine's assistant: the user approved it in the chat (or chose to
    /// approve the conversation's reads).
    ApprovedInChat,
}

impl How {
    /// The log's step (`run_query:enforced`…).
    pub fn phase(self) -> &'static str {
        match self {
            How::Enforced => "enforced",
            How::Approved => "approved",
            How::AutoApproved => "auto_approved",
            How::ApprovedInChat => "approved_in_chat",
        }
    }
}

/// The server-enforced attempt.
pub(crate) enum Attempt<T> {
    Done(T),
    /// `Unsupported`: the engine (or its host) can't enforce the read.
    NotEnforced,
}

/// Why a read didn't run or failed, and the log's step for it.
#[derive(Debug, PartialEq)]
pub(crate) struct Refused {
    pub message: String,
    pub phase: &'static str,
}

/// The routing. Futures are lazy: `approve` runs only when the read isn't
/// enforced, and `guarded` only after the approval.
pub(crate) async fn route<T>(
    enforced: impl Future<Output = Result<Attempt<T>, String>>,
    approve: impl Future<Output = Result<How, Refused>>,
    guarded: impl Future<Output = Result<T, String>>,
) -> Result<(T, How), Refused> {
    match enforced.await {
        Ok(Attempt::Done(out)) => return Ok((out, How::Enforced)),
        Ok(Attempt::NotEnforced) => {}
        Err(message) => return Err(Refused { message, phase: How::Enforced.phase() }),
    }
    let how = approve.await?;
    match guarded.await {
        Ok(out) => Ok((out, how)),
        Err(message) => Err(Refused { message, phase: how.phase() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// What ran, in order.
    #[derive(Default)]
    struct Trace(Mutex<Vec<&'static str>>);
    impl Trace {
        fn push(&self, s: &'static str) {
            self.0.lock().unwrap().push(s);
        }
        fn get(&self) -> Vec<&'static str> {
            self.0.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn mcp_read_enforced_runs_without_asking() {
        let t = Trace::default();
        let r = route(
            async {
                t.push("enforced");
                Ok(Attempt::Done(3))
            },
            async {
                t.push("approve");
                Ok(How::Approved)
            },
            async {
                t.push("guarded");
                Ok(4)
            },
        )
        .await;
        assert_eq!(r.unwrap(), (3, How::Enforced));
        assert_eq!(t.get(), ["enforced"]);
    }

    #[tokio::test]
    async fn mcp_read_not_enforced_needs_approval_first() {
        let t = Trace::default();
        let r = route(
            async {
                t.push("enforced");
                Ok(Attempt::<i32>::NotEnforced)
            },
            async {
                t.push("approve");
                Ok(How::Approved)
            },
            async {
                t.push("guarded");
                Ok(4)
            },
        )
        .await;
        assert_eq!(r.unwrap(), (4, How::Approved));
        // Nothing ran on the guarded path before the approval.
        assert_eq!(t.get(), ["enforced", "approve", "guarded"]);
    }

    #[tokio::test]
    async fn mcp_read_rejected_runs_nothing() {
        let t = Trace::default();
        let r = route(
            async {
                t.push("enforced");
                Ok(Attempt::<i32>::NotEnforced)
            },
            async {
                t.push("approve");
                Err(Refused { message: "rechazado".into(), phase: "rejected" })
            },
            async {
                t.push("guarded");
                Ok(4)
            },
        )
        .await;
        assert_eq!(r.unwrap_err(), Refused { message: "rechazado".into(), phase: "rejected" });
        assert_eq!(t.get(), ["enforced", "approve"]);
    }

    #[tokio::test]
    async fn mcp_read_enforced_error_is_not_retried_unguarded() {
        // A guard refusal or a server error on the enforced path ends there:
        // it never falls back to asking and running another way.
        let t = Trace::default();
        let r = route(
            async {
                t.push("enforced");
                Err::<Attempt<i32>, _>("Conexión de solo lectura: se bloqueó una sentencia DELETE.".to_string())
            },
            async {
                t.push("approve");
                Ok(How::Approved)
            },
            async {
                t.push("guarded");
                Ok(4)
            },
        )
        .await;
        assert_eq!(r.unwrap_err().phase, "enforced");
        assert_eq!(t.get(), ["enforced"]);
    }

    #[tokio::test]
    async fn mcp_read_approved_but_failing_says_how_it_was_approved() {
        let r = route(async { Ok(Attempt::<i32>::NotEnforced) }, async { Ok(How::ApprovedInChat) }, async { Err("no existe la tabla".to_string()) }).await;
        assert_eq!(r.unwrap_err(), Refused { message: "no existe la tabla".into(), phase: "approved_in_chat" });
    }
}
