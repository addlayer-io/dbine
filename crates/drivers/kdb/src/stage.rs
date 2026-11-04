//! The staging table of a sync by rows and of a notebook's write cell:
//! where the rows go before the target changes, in one step.
//!
//! kdb+ has no temporary tables: it's a variable of the namespace
//! [`NAMESPACE`] with a name of its own (`dbine_w_…`), created empty with
//! the target's columns and types (`0#`), loaded as a bulk load is (typed
//! IPC tables applied to [`load::LOAD`], never q text built from values)
//! and removed afterwards, whatever happened.

use crate::ipc::{self, K};
use crate::read::symbol_list;
use crate::{column_types, edit, load, KdbSession, PARTITIONED, SPLAYED};
use dbine_driver::transfer::{BatchSource, Cell, Progress};
use dbine_driver::{Error, ObjectRef, Result};

/// Where staging tables live.
pub const NAMESPACE: &str = ".dbine_stage";

/// `.Q.qp` of table `x`: `1b` partitioned, `0b` splayed, `0` in memory.
const ON_DISK: &str = "{@[.Q.qp;get x;0]}";

/// `s`: an empty table with the columns `c` of table `n`, of their types.
const CREATE: &str = "{[n;s;c] s set 0#?[0!get n;();0b;c!c]; count c}";

/// Removes staging table `x` of [`NAMESPACE`], and the namespace itself once
/// it holds no other (another write's, in flight): nothing of DBine's is
/// left in the process.
const DROP: &str = "{![`.dbine_stage;();0b;enlist x]; if[0=count (key `.dbine_stage) except `; ![`.;();0b;enlist `dbine_stage]]}";

/// A staging table on the server.
pub(crate) struct Stage {
    /// Its name in [`NAMESPACE`].
    base: String,
}

impl Stage {
    /// Its whole name (`.dbine_stage.dbine_w_…`).
    pub fn name(&self) -> String {
        format!("{NAMESPACE}.{}", self.base)
    }
}

/// The target is a table in memory (refused on disk, as the grid does),
/// checked on the server.
async fn in_memory(s: &mut KdbSession, table: &str) -> Result<()> {
    let kind = match s
        .request(&ipc::apply(ON_DISK, vec![K::Symbol(table.into())]))
        .await?
    {
        K::Bool(true) => PARTITIONED,
        K::Bool(false) => SPLAYED,
        _ => return Ok(()),
    };
    crate::writable(&ObjectRef {
        kind: kind.into(),
        schema: None,
        name: table.into(),
    })
}

/// A new staging table with `columns` of `table` (a table in memory).
pub(crate) async fn create(s: &mut KdbSession, table: &str, columns: &[String]) -> Result<Stage> {
    in_memory(s, table).await?;
    let stage = Stage {
        base: dbine_driver::write::stage_name(),
    };
    s.request(&ipc::apply(
        CREATE,
        vec![
            K::Symbol(table.into()),
            K::Symbol(stage.name()),
            symbol_list(columns),
        ],
    ))
    .await?;
    Ok(stage)
}

/// Load `source`'s rows (cells in the order of `columns`) into `stage`,
/// typed as `table`'s columns; returns the rows staged.
pub(crate) async fn load(
    s: &mut KdbSession,
    stage: &Stage,
    table: &str,
    columns: &[String],
    source: &mut dyn BatchSource,
    progress: Progress<'_>,
) -> Result<u64> {
    let meta = s
        .q(&format!("exec c!t from 0!meta {}", edit::qref(table)))
        .await?;
    let types = column_types(&meta, columns);
    let name = stage.name();
    let (max_rows, max_bytes) = (load::MESSAGE_ROWS as usize, load::MESSAGE_BYTES as usize);
    let mut total = 0u64;
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut bytes = 0usize;
    let mut done = false;
    while !done {
        match source.next().await {
            Some(batch) => {
                bytes += batch.rows.iter().flatten().map(Cell::size).sum::<usize>();
                rows.extend(batch.rows);
                if rows.len() < max_rows && bytes < max_bytes {
                    continue;
                }
            }
            None => done = true,
        }
        if rows.is_empty() {
            continue;
        }
        let sent = rows.len() as u64;
        let t = load::table(columns, &types, &rows);
        rows.clear();
        bytes = 0;
        let msg = ipc::apply(load::LOAD, vec![K::Symbol(name.clone()), t]);
        total += match s.request(&msg).await? {
            K::Long(n) if n >= 0 => n as u64,
            _ => sent,
        };
        progress(total);
    }
    Ok(total)
}

/// Remove `stage` (on a new connection if the last one was cancelled). A
/// staging table that can't be removed (the server is gone) is only
/// memory of a process that's no longer there.
pub(crate) async fn drop(s: &mut KdbSession, stage: &Stage) {
    crate::read::fresh(s);
    let _ = s
        .request(&ipc::apply(DROP, vec![K::Symbol(stage.base.clone())]))
        .await;
}

/// Refused on a read-only connection (before anything is sent).
pub(crate) fn writes(s: &KdbSession) -> Result<()> {
    if s.read_only {
        return Err(Error::Query(format!(
            "Conexión de solo lectura: se bloqueó {}. Solo se permiten lecturas.",
            "set"
        )));
    }
    Ok(())
}

/// What a staged change tells the user (the cell's result, the run's log).
pub(crate) const ONE_STEP: &str = "kdb+ no tiene transacciones: DBine cargó las filas aparte y cambió la tabla en un solo paso, así que un error la deja como estaba. El cambio queda en la memoria del proceso: para conservarlo después de reiniciarlo hay que guardar la tabla (save, set).";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_tables_have_names_of_their_own_in_one_namespace() {
        let a = Stage {
            base: dbine_driver::write::stage_name(),
        };
        assert!(
            a.name().starts_with(".dbine_stage.dbine_w_"),
            "{}",
            a.name()
        );
        assert!(edit::plain(&a.name()));
        // Created empty from the target's columns, typed as they are.
        assert!(CREATE.contains("s set 0#?[0!get n;();0b;c!c]"));
        assert!(DROP.contains("![`.dbine_stage;();0b;enlist x]"));
        assert!(DROP.contains("![`.;();0b;enlist `dbine_stage]"));
        assert_eq!(NAMESPACE, ".dbine_stage");
    }
}
