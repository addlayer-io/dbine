//! Which errors are worth a retry, and how long to wait.

use dbine_driver::Error;
use std::io::ErrorKind;
use std::time::Duration;

/// Longest wait between retries.
const MAX_BACKOFF_MS: u64 = 30_000;

/// Phrases of server and network errors that usually go away on their own
/// (lowercase). Whole phrases, never bare words: a message that only names
/// a column or constraint like `session_timeout` or `UQ_connection_id` is
/// not transient.
const TRANSIENT: &[&str] = &[
    "deadlocked on",
    "deadlock detected",
    "deadlock found",
    "deadlock victim",
    "timed out",
    "timeout expired",
    "lock wait timeout exceeded",
    "connection reset",
    "connection refused",
    "connection aborted",
    "connection closed",
    "connection was closed",
    "connection is closed",
    "connection lost",
    "lost connection",
    "closed the connection",
    "forcibly closed",
    "broken pipe",
    "transport-level error",
    "transport error",
    "network-related",
    "server is busy",
    // SQL Server sync by rows: the staging table collided with a schema
    // change (error 539), and the target was left untouched.
    "mientras se creaba la tabla de paso",
];

/// A connection drop, a timeout, a deadlock… Anything else is not retried
/// (when in doubt, it isn't). The engine's own errors (`Error::State`: a
/// column mismatch, a table with rows, a panic) never are.
pub fn is_transient(e: &Error) -> bool {
    match e {
        Error::Connect(_) => true,
        Error::Query(m) => transient_text(m),
        Error::Io(io) => {
            matches!(
                io.kind(),
                ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe | ErrorKind::TimedOut
            ) || transient_text(&io.to_string())
        }
        _ => false,
    }
}

fn transient_text(m: &str) -> bool {
    let m = m.to_lowercase();
    TRANSIENT.iter().any(|t| m.contains(t))
}

/// The wait before retry number `retry` (1-based): `base_ms`, doubling,
/// capped at 30 s.
pub fn backoff(base_ms: u64, retry: u32) -> Duration {
    let shift = retry.saturating_sub(1).min(8);
    Duration::from_millis(base_ms.saturating_mul(1 << shift).min(MAX_BACKOFF_MS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies() {
        assert!(is_transient(&Error::Connect("refused".into())));
        assert!(is_transient(&Error::Query("Transaction was deadlocked on lock resources".into())));
        assert!(is_transient(&Error::Query("Connection reset by peer".into())));
        assert!(is_transient(&Error::Io(std::io::Error::from(ErrorKind::BrokenPipe))));
        assert!(!is_transient(&Error::Query("Violation of PRIMARY KEY constraint".into())));
        assert!(is_transient(&Error::Query("Execution Timeout Expired".into())));
        assert!(is_transient(&Error::Query("A transport-level error has occurred when receiving results".into())));
        assert!(is_transient(&Error::Query("deadlock detected".into())));
        assert!(is_transient(&Error::Query("server closed the connection unexpectedly".into())));
    }

    #[test]
    fn identifiers_and_engine_errors_are_not_transient() {
        assert!(!is_transient(&Error::Query("Cannot insert the value NULL into column 'session_timeout'".into())));
        assert!(!is_transient(&Error::Query("Violation of UNIQUE KEY constraint 'UQ_connection_id'".into())));
        assert!(!is_transient(&Error::Query("null value in column \"connection_id\" violates not-null constraint".into())));
        assert!(!is_transient(&Error::Query("insert or update violates foreign key constraint \"fk_transport_deadlock\"".into())));
        assert!(!is_transient(&Error::State("la tabla de destino no coincide: falta la columna «connection_string»".into())));
        assert!(!is_transient(&Error::State("error inesperado: connection reset".into())));
        assert!(!is_transient(&Error::AuthFailed("login failed".into())));
        assert!(!is_transient(&Error::Unsupported("x".into())));
        assert!(!is_transient(&Error::Cancelled));
    }

    #[test]
    fn doubles_up_to_thirty_seconds() {
        assert_eq!(backoff(1_000, 1), Duration::from_secs(1));
        assert_eq!(backoff(1_000, 2), Duration::from_secs(2));
        assert_eq!(backoff(1_000, 3), Duration::from_secs(4));
        assert_eq!(backoff(1_000, 6), Duration::from_secs(30));
        assert_eq!(backoff(1_000, 60), Duration::from_secs(30));
    }
}
