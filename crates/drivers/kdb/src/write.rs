//! A notebook's write cell ([`Session::write_rows`]) on kdb+, in every
//! mode of [`WriteMode`].
//!
//! The rows go to a staging table first ([`stage`]: typed IPC tables, never
//! q text built from values); then one request applies them to the target
//! ([`WRITE`]), a table in memory (partitioned and splayed ones are
//! refused, as in the grid):
//!
//! - **Insert:** `n insert st`, one `insert` of every staged row. On a
//!   keyed table a key already there, or one the rows repeat, is refused
//!   first (kdb+'s `insert` checks only the first).
//! - **Update:** the target's rows whose key (the cell's, any of the
//!   columns) matches a staged row get its other columns.
//! - **Upsert:** that update, then the staged rows whose key the table
//!   doesn't have, appended.
//! - **Replace:** the staged rows become the table (`0#` of it, so it keeps
//!   its columns, types and key).
//!
//! Update, Upsert and Replace build the new table and assign it in one step
//! (`set`), keyed again when it was and with its key checked unique. kdb+
//! has no transactions, but a failure before that step (or `insert`'s
//! single append) leaves the table as it was, and kdb+ runs one request at
//! a time: the result says `atomic`, and its note says why and that the
//! change lives in the process's memory.
//!
//! [`Session::write_rows`]: dbine_driver::Session::write_rows

use crate::delta::{counts, dup_key};
use crate::ipc::{self, K};
use crate::read::{fresh, symbol_list};
use crate::stage::{self, ONE_STEP};
use crate::sync::column_name;
use crate::KdbSession;
use dbine_driver::transfer::{BatchSource, Progress, TransferColumn};
use dbine_driver::{Error, Result, WriteMode, WriteResult, WriteSpec};

/// `[n;s;k;c;md]`: the staged rows of `s` (columns `c`) written into table
/// `n` by mode `md` and key `k`; answers the rows inserted, updated and
/// deleted.
pub const WRITE: &str = concat!(
    "{[n;s;k;c;md] ky:keys n; t:0!get n; st:get s; nt:count t; ns:count st;",
    " if[md=`insert; if[count ky; kv:?[st;();0b;ky!ky]; if[(count distinct kv)<ns; ",
    dup_key!(),
    "]; if[any kv in key get n; '\"la tabla ya tiene una fila con esa clave\"]]; n insert st; :(ns;0;0)];",
    " if[md=`replace; t:(0#t) upsert st; if[count ky; t:ky xkey t; if[count[t]>count distinct key t; ",
    dup_key!(),
    "]]; n set t; :(ns;0;nt)];",
    " cc:c except k; tk:?[t;();0b;k!k]; sk:?[st;();0b;k!k]; m:sk?tk; mw:where m<ns;",
    " if[count[cc]&count mw; t:@[t;cc;:;{[x;y;i;j] @[x;i;:;y j]}[;;mw;m mw]'[t cc;st cc]]];",
    " a:$[md=`upsert; where nt=tk?sk; `long$()];",
    " if[count a; t:t upsert st a];",
    " if[count ky; t:ky xkey t; if[count[t]>count distinct key t; ",
    dup_key!(),
    "]];",
    " if[count[mw]+count a; n set t];",
    " (count a; count mw; 0)}"
);

/// Every mode: kdb+ does all four.
pub fn modes() -> Vec<WriteMode> {
    WriteMode::ALL.to_vec()
}

fn mode_symbol(m: WriteMode) -> &'static str {
    match m {
        WriteMode::Insert => "insert",
        WriteMode::Update => "update",
        WriteMode::Upsert => "upsert",
        WriteMode::Replace => "replace",
    }
}

pub(crate) async fn write_rows(
    s: &mut KdbSession,
    spec: &WriteSpec,
    _columns: &[TransferColumn],
    source: &mut dyn BatchSource,
    progress: Progress<'_>,
) -> Result<WriteResult> {
    stage::writes(s)?;
    spec.check()?;
    crate::writable(&spec.table)?;
    for c in spec.columns.iter().chain(&spec.key) {
        column_name(c)?;
    }
    // The key as the table spells it (the check compares it ignoring case).
    let key: Vec<String> = spec
        .key
        .iter()
        .map(|k| {
            spec.columns
                .iter()
                .find(|c| c.eq_ignore_ascii_case(k))
                .cloned()
                .ok_or_else(|| {
                    Error::Query(format!(
                        "la clave «{k}» no está entre las columnas que se escriben"
                    ))
                })
        })
        .collect::<Result<_>>()?;
    fresh(s);
    let table = spec.table.name.clone();
    let st = stage::create(s, &table, &spec.columns).await?;
    let r = async {
        stage::load(s, &st, &table, &spec.columns, source, progress).await?;
        let msg = ipc::apply(
            WRITE,
            vec![
                K::Symbol(table.clone()),
                K::Symbol(st.name()),
                symbol_list(&key),
                symbol_list(&spec.columns),
                K::Symbol(mode_symbol(spec.mode).into()),
            ],
        );
        s.request(&msg).await
    }
    .await;
    stage::drop(s, &st).await;
    let (inserted, updated, deleted) = counts(&r?)?;
    Ok(WriteResult {
        inserted,
        updated,
        deleted,
        atomic: true,
        notes: vec![ONE_STEP.into()],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_and_its_q() {
        assert_eq!(modes(), WriteMode::ALL.to_vec());
        for m in WriteMode::ALL {
            assert!(WRITE.contains(&format!("`{}", mode_symbol(m))) || m == WriteMode::Update);
        }
        // Insert appends in one insert, after its key checks.
        assert!(WRITE.contains("n insert st; :(ns;0;0)"));
        // Replace keeps the table's columns and key.
        assert!(WRITE.contains("t:(0#t) upsert st"));
        // The rest assign once, at the end, only when something changed.
        assert!(WRITE.contains(" if[count[mw]+count a; n set t];"));
        assert_eq!(WRITE.matches("n set t").count(), 2);
        assert!(WRITE.contains("la tabla tiene clave y las filas repiten"));
    }
}
