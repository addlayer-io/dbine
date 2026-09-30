//! DBine's bulk transfer engine: copies tables between databases with the
//! engines' native bulk loads, several tables at once, memory bounded, and
//! resumable after a cut. See `docs/transferencia-masiva.md`.
//!
//! It knows nothing of any engine: the app describes each table as a
//! [`TransferJob`], opens connections through [`Endpoints`] and runs an
//! [`Engine`]. Per table, the copy takes the first path available:
//! 1. the driver's own copy ([`dbine_driver::Driver::copy_native`]) when
//!    both ends are the same driver;
//! 2. a reader ([`dbine_driver::Session::read_batches`]) and a writer
//!    ([`dbine_driver::Session::bulk_load`], or the driver's
//!    `insert_script` batches) joined by a channel of at most
//!    [`CHANNEL_BATCHES`] batches.
//!
//! Each table is all or nothing: its state lives in SQLite ([`Store`]); a
//! table left half copied is emptied and copied again, and a table already
//! copied is never emptied (only its `post` statements run again).
//!
//! A job in [`TransferMode::Delta`] syncs by rows instead ([`delta`]): only
//! the rows that differ are merged, and the table is never emptied.

mod check;
pub mod clone_table;
mod copy;
pub mod delta;
mod engine;
mod event;
mod job;
mod retry;
mod slots;
pub mod state;

pub use check::column_differences;
pub use copy::CHANNEL_BATCHES;
pub use engine::{Control, Endpoints, Engine, RunReport, PROGRESS_EVERY};
pub use event::{Bottleneck, CopyPath, CopyStats, Event, LogLevel, Phase, RunSummary};
pub use job::{CopyOrder, RunOptions, TransferJob, TransferMode};
pub use retry::{backoff, is_transient};
pub use slots::MAX_PARALLEL;
pub use state::{RunSpec, RunState, RunStatus, Store, TableState, TableStatus};

/// Lock a mutex, taking the data even if a panicking thread poisoned it
/// (a table's panic must not take the run down).
pub(crate) fn lock<T: ?Sized>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A panic's message, for the table's error.
pub(crate) fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "sin detalle".to_string()
    }
}
