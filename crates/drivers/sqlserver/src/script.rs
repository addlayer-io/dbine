//! Scripts as SSMS runs them: `GO` batches (`GO N` repeats one), and each
//! batch's whole answer in order: result sets, `PRINT` / `RAISERROR` /
//! `STATISTICS IO/TIME` messages, "(N filas afectadas)" per statement
//! (unless `SET NOCOUNT ON`), every error with its line, and `USE`.

use super::{cell, err, spatial_cell, is_desync, is_plan_column, ResultColumn, SqlServerSession};
use dbine_driver::sql::{split_script, ScriptDialect, ScriptStatement, StatementKind};
use dbine_driver::{Error, Message, MessageLevel, QueryOutcome, Result, ScriptError};
use futures::TryStreamExt;
use std::sync::atomic::Ordering;
use tiberius::{DoneKind, MessageItem, ServerMessage};

/// The column SQL Server names FOR XML / FOR JSON output, which it sends
/// cut into rows of about 2033 characters: SSMS shows them as one value.
const FOR_XML: &str = "XML_F52E2B61-18A1-11d1-B105-00805F49916B";
const FOR_JSON: &str = "JSON_F52E2B61-18A1-11d1-B105-00805F49916B";

/// Messages SSMS doesn't show: 5701 "Changed database context to…" (USE)
/// and 5703 "Changed language setting to…". Babelfish sends them with
/// numbers of its own, so their text is checked too.
const HIDDEN: [u32; 2] = [5701, 5703];
const HIDDEN_TEXT: [&str; 2] = ["Changed database context to", "Changed language setting to"];

/// Errors from this severity up end the connection (the server closes it).
const FATAL_CLASS: u8 = 20;

/// The script's `GO` batches, as SSMS and sqlcmd cut them.
pub(crate) fn batches(sql: &str) -> Vec<ScriptStatement> {
    split_script(sql, &ScriptDialect::tsql()).into_iter().filter(|u| u.kind != StatementKind::ClientCommand).collect()
}

fn affected(n: u64) -> String {
    if n == 1 {
        "(1 fila afectada)".into()
    } else {
        format!("({n} filas afectadas)")
    }
}

/// `Msg 208, Nivel 16, Estado 1[, Procedimiento p, Línea 5]`, as SSMS heads
/// an error (the script line is the message's own `line`).
/// One whole line each way (not a piece appended), so the UI translates it.
fn header(m: &ServerMessage) -> String {
    if m.procedure.is_empty() {
        format!("Msg {}, Nivel {}, Estado {}", m.number, m.class, m.state)
    } else {
        format!("Msg {}, Nivel {}, Estado {}, Procedimiento {}, Línea {}", m.number, m.class, m.state, m.procedure, m.line)
    }
}

/// The error at its line of the text the driver got: the batch's line plus
/// the server's (relative to the batch). An error raised inside a module
/// carries the module's line, so it points at the batch instead.
fn script_error(m: &ServerMessage, base: u32) -> ScriptError {
    let line = if m.procedure.is_empty() && m.line > 0 { base + m.line - 1 } else { base };
    let e = ScriptError::new(m.message.clone()).with_code(m.number.to_string()).at_line(line);
    if m.class >= FATAL_CLASS {
        e.fatal()
    } else {
        e
    }
}

/// Like `out.push_error`, but the log line carries SSMS's heading.
fn record_error(out: &mut QueryOutcome, m: &ServerMessage, mut e: ScriptError) {
    e.statement = e.statement.or(out.current_statement);
    out.message(Message {
        level: MessageLevel::Error,
        text: format!("{}\n{}", header(m), m.message),
        statement: e.statement,
        code: None,
        line: e.line,
    });
    if out.error.is_none() {
        out.error = Some(e.message.clone());
    }
    out.errors.push(e);
}

fn info(out: &mut QueryOutcome, m: &ServerMessage) {
    if HIDDEN.contains(&m.number) || HIDDEN_TEXT.iter().any(|t| m.message.starts_with(t)) {
        return;
    }
    let warning = m.message.get(..8).is_some_and(|w| w.eq_ignore_ascii_case("warning:"));
    let level = if warning { MessageLevel::Warning } else { MessageLevel::Info };
    out.message(Message { level, text: m.message.clone(), ..Default::default() });
}

/// A FOR XML / FOR JSON value being put back together.
fn flush_merged(out: &mut QueryOutcome, merged: &mut Option<String>, max_rows: usize) {
    if let Some(v) = merged.take().filter(|v| !v.is_empty()) {
        out.push_row(vec![v.into()], max_rows);
    }
}

impl SqlServerSession {
    /// Run the script's batches. A batch with errors ends it, once its `GO N`
    /// repeats are done (what the app expects of a whole script; the editor sends one batch per call, see
    /// `ScriptMode::Batches`). With `plans`, showplan result sets (SHOWPLAN_XML
    /// / STATISTICS XML) are taken out of the results and collected there.
    pub(crate) async fn run_batches(
        &mut self,
        sql: &str,
        max_rows: usize,
        out: &mut QueryOutcome,
        mut plans: Option<&mut Vec<String>>,
    ) -> Result<()> {
        let units = batches(sql);
        // An invalid `GO` count: nothing runs, as in SSMS.
        if let Some(u) = units.iter().find(|u| u.error.is_some()) {
            let e = ScriptError::new(u.error.clone().unwrap_or_default()).at_line(u.line).fatal();
            out.push_error(e.clone());
            return Err(e.into());
        }
        for u in &units {
            let repeat = u.repeat.max(1);
            if repeat > 1 {
                out.info("Inicio del ciclo de ejecución");
            }
            // As sqlcmd and SSMS: an iteration with errors doesn't end the
            // repeats; only a fatal error or a cancel does. Every iteration
            // that ran counts.
            let mut done = 0u32;
            let mut failed = None;
            for _ in 0..repeat {
                // A cancel between batches: the attention found nothing running.
                if self.cancelled.load(Ordering::SeqCst) {
                    return Err(Error::Cancelled);
                }
                let r = self.run_batch(&u.text, u.line, max_rows, out, plans.as_deref_mut(), None).await;
                if matches!(r, Err(Error::Cancelled)) || self.cancelled.load(Ordering::SeqCst) {
                    return Err(Error::Cancelled);
                }
                done += 1;
                if let Err(e) = r {
                    // A fatal one is what the call returns.
                    if e.ends_script() {
                        failed = Some(e);
                        break;
                    }
                    failed.get_or_insert(e);
                }
            }
            if repeat > 1 {
                out.info(format!("Lote ejecutado {done} veces."));
            }
            if let Some(e) = failed {
                return Err(e);
            }
        }
        Ok(())
    }

    /// One batch, starting at line `line` of the text the driver got. Every
    /// error goes into `out`; the call fails with the first (or the one
    /// that ended the connection).
    /// `params`: the batch runs through `sp_executesql` with `@P1`, `@P2`…
    /// bound to them (notebook parameters, see [`Self::run_bound`]).
    pub(crate) async fn run_batch(
        &mut self,
        text: &str,
        line: u32,
        max_rows: usize,
        out: &mut QueryOutcome,
        mut plans: Option<&mut Vec<String>>,
        params: Option<&[&dyn tiberius::ToSql]>,
    ) -> Result<()> {
        let variant = self.variant;
        let mut first: Option<ScriptError> = None;
        let mut fatal: Option<ScriptError> = None;
        let mut broken: Option<tiberius::error::Error> = None;
        let mut database: Option<String> = None;
        let mut had_result = false;
        let unsent = 'read: {
            let sent = match params {
                Some(p) => self.client.query_messages(text, p).await,
                None => self.client.simple_query_messages(text).await,
            };
            let mut stream = match sent {
                Ok(s) => s,
                Err(e) => break 'read Some(e),
            };
            let mut in_plan = false;
            // A result set started since the last statement ended: its count
            // is the grid's, not "(N filas afectadas)".
            let mut in_result = false;
            let mut merged: Option<String> = None;
            // The result's geometry / geography columns (their values go whole).
            let mut spatial: Vec<bool> = Vec::new();
            loop {
                let item = match stream.try_next().await {
                    Ok(Some(item)) => item,
                    Ok(None) => break,
                    Err(e) => {
                        broken = Some(e);
                        break;
                    }
                };
                match item {
                    MessageItem::Metadata(meta) => {
                        flush_merged(out, &mut merged, max_rows);
                        in_result = true;
                        let cols = meta.columns();
                        in_plan = plans.is_some() && cols.len() == 1 && is_plan_column(variant, cols[0].name());
                        if in_plan {
                            // SQL Server sends one XML row per plan; Babelfish
                            // one row per line of the text plan.
                            if let Some(p) = plans.as_deref_mut() {
                                p.push(String::new());
                            }
                            continue;
                        }
                        had_result = true;
                        if cols.len() == 1 && [FOR_XML, FOR_JSON].iter().any(|n| cols[0].name().eq_ignore_ascii_case(n)) {
                            merged = Some(String::new());
                        }
                        spatial = cols.iter().map(|c| matches!(c.udt_name(), Some("geometry" | "geography"))).collect();
                        out.begin_result(
                            cols.iter()
                                .map(|c| ResultColumn {
                                    name: c.name().to_string(),
                                    // A UDT by its name: `geometry`, `geography`, `hierarchyid`.
                                    type_name: c.udt_name().map_or_else(|| format!("{:?}", c.column_type()), str::to_string),
                                })
                                .collect(),
                        );
                    }
                    MessageItem::Row(row) if in_plan => {
                        if let (Some(last), Some(text)) =
                            (plans.as_deref_mut().and_then(|p| p.last_mut()), row.try_get::<&str, _>(0).ok().flatten())
                        {
                            last.push_str(text);
                            last.push('\n');
                        }
                    }
                    MessageItem::Row(row) => match merged.as_mut() {
                        Some(buf) => {
                            if let Some(serde_json::Value::String(s)) = row.into_iter().next().map(cell) {
                                buf.push_str(&s);
                            }
                        }
                        None => out.push_row(
                            row.into_iter().enumerate().map(|(i, d)| spatial_cell(d, spatial.get(i).copied().unwrap_or(false))).collect(),
                            max_rows,
                        ),
                    },
                    MessageItem::Info(m) => info(out, &m),
                    MessageItem::Error(m) => {
                        let e = script_error(&m, line);
                        record_error(out, &m, e.clone());
                        if e.fatal {
                            fatal.get_or_insert(e.clone());
                        }
                        first.get_or_insert(e);
                    }
                    MessageItem::Done(d) => {
                        flush_merged(out, &mut merged, max_rows);
                        // A procedure's own DONE repeats what its statements
                        // (DONEINPROC) already counted.
                        if d.kind != DoneKind::DoneProc {
                            if let (Some(n), false, false) = (d.rows, d.error, in_result) {
                                out.info(affected(n));
                            }
                            in_result = false;
                        }
                    }
                    MessageItem::Database(db) => database = Some(db),
                    MessageItem::Transaction(_) => {}
                }
            }
            flush_merged(out, &mut merged, max_rows);
            None
        };
        match unsent {
            Some(e) if is_desync(&e) || is_io(&e) => return Err(self.connection_lost(out, e).await),
            Some(e) => {
                let e = ScriptError::new(err(e).to_string()).at_line(line);
                out.push_error(e.clone());
                return Err(e.into());
            }
            None => {}
        }
        if let Some(db) = database {
            self.database = Some(db.clone());
            out.database = Some(db);
        }
        match broken {
            // Stopped by the interrupter (Babelfish's KILL breaks the
            // connection): the app decides what happens to the session.
            Some(tiberius::error::Error::Cancelled) => return Err(Error::Cancelled),
            Some(_) if self.cancelled.load(Ordering::SeqCst) => return Err(Error::Cancelled),
            // The server closed the connection after a severity 20+ error:
            // that error says why.
            Some(e) if fatal.is_none() && (is_desync(&e) || is_io(&e)) => return Err(self.connection_lost(out, e).await),
            Some(e) if fatal.is_none() => {
                let e = ScriptError::new(err(e).to_string()).at_line(line);
                out.push_error(e.clone());
                return Err(e.into());
            }
            _ => {}
        }
        if let Some(e) = fatal {
            // The session is gone on the server: a new one for what follows.
            match self.reconnect().await {
                Ok(()) => out.warning(
                    "El servidor cerró la conexión por un error de nivel 20 o más. Se abrió una nueva: \
                     se perdió el estado de la sesión (transacción abierta, tablas #temp y SET).",
                ),
                Err(re) => out.warning(format!("El servidor cerró la conexión y no se pudo abrir otra: {re}")),
            }
            return Err(e.into());
        }
        if let Some(e) = first {
            return Err(e.into());
        }
        if !had_result {
            // The batch just shows as done ("completada"); its counts, if
            // any, are in the messages.
            out.results.push(Default::default());
        }
        Ok(())
    }

    /// One batch with its values bound (`@P1`, `@P2`…), run through
    /// `sp_executesql`. What `sp_executesql` would undo when it ends (a
    /// `USE`, a session `SET`, a `#temp` table the next cells read) stays
    /// out: such a batch, `GO` lines or more than one batch answer
    /// `Unsupported` before anything runs, and the app sends literals.
    pub(crate) async fn run_bound(&mut self, st: &dbine_driver::BoundStatement, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let sql = st.numbered("@P");
        let units = batches(&sql);
        if units.len() != 1 || units[0].repeat > 1 || units[0].error.is_some() {
            return Err(Error::Unsupported("SQL Server recibe los parámetros de un lote por vez".into()));
        }
        if changes_session(&units[0].text) {
            return Err(Error::Unsupported("el lote cambia la sesión (USE, SET o una tabla #temporal): va con los valores escritos en el texto".into()));
        }
        let data: Vec<tiberius::ColumnData<'static>> = st.values.iter().map(bind_value).collect();
        let refs: Vec<&dyn tiberius::ToSql> = data.iter().map(|d| d as &dyn tiberius::ToSql).collect();
        let text = units[0].text.clone();
        self.run_batch(&text, units[0].line, max_rows, out, None, Some(&refs)).await
    }

    /// The connection broke under a batch: a new one is opened for the next
    /// run and the script stops (its session state is gone).
    async fn connection_lost(&mut self, out: &mut QueryOutcome, e: tiberius::error::Error) -> Error {
        tracing::debug!("sqlserver connection lost: {e}");
        let lost = self.lost_connection().await;
        let e = ScriptError::new(lost.to_string()).fatal();
        out.push_error(e.clone());
        e.into()
    }
}

/// Whether a batch changes the session in a way `sp_executesql` would undo
/// at its end: `USE`, a `SET` of a session option (not `SET @v = …`), or a
/// `#temp` table it creates (`CREATE TABLE #t`, `SELECT … INTO #t`).
pub(crate) fn changes_session(batch: &str) -> bool {
    use dbine_driver::sql::{name_tokens, TokenKind};
    let toks = name_tokens(batch, &ScriptDialect::tsql());
    let word = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.kind == TokenKind::Name && t.text.eq_ignore_ascii_case(w) && !batch[t.start..].starts_with('['));
    let starts = |i: usize| i == 0 || toks[i - 1].text == ";" || word(i - 1, "begin") || word(i - 1, "end") || word(i - 1, "go");
    let temp_after = |i: usize| toks.get(i + 1).is_some_and(|t| t.text.starts_with('#') || batch[t.start..].starts_with('#'));
    (0..toks.len()).any(|i| {
        (word(i, "use") && starts(i))
            || (word(i, "set") && starts(i) && toks.get(i + 1).is_some_and(|t| t.kind == TokenKind::Name && !t.text.starts_with('@') && !batch[t.start..].starts_with('@')))
            || ((word(i, "into") || word(i, "table")) && temp_after(i))
    })
}

/// A parameter as tiberius sends it: text as NVARCHAR, integers as BIGINT,
/// other numbers as DECIMAL (exact), dates as DATE / DATETIME2, booleans
/// as BIT.
fn bind_value(v: &dbine_driver::ParamValue) -> tiberius::ColumnData<'static> {
    use dbine_driver::ParamValue as P;
    use tiberius::ColumnData as C;
    use tiberius::IntoSql;
    match v {
        P::Null => C::String(None),
        P::Text(s) => C::String(Some(s.clone().into())),
        P::Number(n) => match n.parse::<i64>() {
            Ok(i) => C::I64(Some(i)),
            Err(_) => decimal(n).map(|d| C::Numeric(Some(d))).unwrap_or_else(|| C::F64(n.parse().ok())),
        },
        P::Bool(b) => C::Bit(Some(*b)),
        P::Date(d) => match chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d") {
            Ok(d) => d.into_sql(),
            Err(_) => C::String(Some(d.clone().into())),
        },
        P::DateTime(d) => match chrono::NaiveDateTime::parse_from_str(d, "%Y-%m-%dT%H:%M:%S%.f") {
            Ok(d) => d.into_sql(),
            Err(_) => C::String(Some(d.clone().into())),
        },
    }
}

/// `-12.345` as a DECIMAL (no exponent; at most 38 digits).
fn decimal(n: &str) -> Option<tiberius::numeric::Numeric> {
    if n.contains(['e', 'E']) {
        return None;
    }
    let (int, frac) = n.split_once('.').unwrap_or((n, ""));
    let digits = format!("{int}{frac}");
    if digits.trim_start_matches('-').len() > 38 {
        return None;
    }
    let value: i128 = digits.parse().ok()?;
    Some(tiberius::numeric::Numeric::new_with_scale(value, frac.len() as u8))
}

fn is_io(e: &tiberius::error::Error) -> bool {
    matches!(e, tiberius::error::Error::Io { .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_that_change_the_session_are_not_bound() {
        for yes in ["USE ventas; SELECT @P1", "SET NOCOUNT ON; SELECT @P1", "SELECT * INTO #t FROM x WHERE a = @P1", "CREATE TABLE #t (a int)", "begin use x end"] {
            assert!(changes_session(yes), "{yes}");
        }
        for no in ["SELECT * FROM t WHERE a = @P1", "DECLARE @x int; SET @x = @P1; SELECT @x", "SELECT 'USE x; SET NOCOUNT ON' AS t", "UPDATE t SET a = @P1", "SELECT [use] FROM t", "-- USE x\nSELECT 1"] {
            assert!(!changes_session(no), "{no}");
        }
    }

    #[test]
    fn decimals_bind_exactly() {
        let n = decimal("-12.345").unwrap();
        assert_eq!((n.value(), n.scale()), (-12345, 3));
        assert_eq!(decimal(".5").map(|n| (n.value(), n.scale())), Some((5, 1)));
        assert!(decimal("1e3").is_none());
    }

    fn msg(number: u32, class: u8, line: u32, procedure: &str) -> ServerMessage {
        ServerMessage {
            number,
            state: 1,
            class,
            message: "Invalid object name 'x'.".into(),
            server: "srv".into(),
            procedure: procedure.into(),
            line,
        }
    }

    #[test]
    fn go_batches_use_the_shared_lexer() {
        let b = batches("select 1\nGO\n  go  \n/* \nGO\n*/ select 2\nGO 3 -- three\nselect 'a\nGO\nb'\n");
        let texts: Vec<_> = b.iter().map(|u| (u.text.as_str(), u.line, u.repeat)).collect();
        assert_eq!(texts, vec![("select 1", 1, 1), ("select 2", 6, 3), ("select 'a\nGO\nb'", 8, 1)]);
    }

    #[test]
    fn errors_carry_ssms_heading_and_script_line() {
        let m = msg(208, 16, 3, "");
        assert_eq!(header(&m), "Msg 208, Nivel 16, Estado 1");
        let e = script_error(&m, 10);
        assert_eq!((e.line, e.code.as_deref(), e.fatal), (Some(12), Some("208"), false));
        // Inside a module: the module's line in the heading, the batch's line.
        let p = msg(50000, 16, 5, "dbo.p");
        assert_eq!(header(&p), "Msg 50000, Nivel 16, Estado 1, Procedimiento dbo.p, Línea 5");
        assert_eq!(script_error(&p, 10).line, Some(10));
        assert!(script_error(&msg(2745, 20, 1, ""), 1).fatal);
    }

    #[test]
    fn errors_and_messages_go_to_the_log_in_order() {
        let mut out = QueryOutcome::default();
        info(&mut out, &ServerMessage { message: "Changed database context to 'x'.".into(), ..msg(5701, 0, 1, "") });
        info(&mut out, &ServerMessage { message: "hola".into(), ..msg(0, 0, 1, "") });
        info(&mut out, &ServerMessage { message: "Warning: Null value is eliminated by an aggregate or other SET operation.".into(), ..msg(8153, 10, 1, "") });
        let m = msg(208, 16, 2, "");
        record_error(&mut out, &m, script_error(&m, 1));
        out.info(affected(1));
        out.info(affected(3));
        let log: Vec<_> = out.log.iter().map(|m| (m.level, m.text.as_str())).collect();
        assert_eq!(
            log,
            vec![
                (MessageLevel::Info, "hola"),
                (MessageLevel::Warning, "Warning: Null value is eliminated by an aggregate or other SET operation."),
                (MessageLevel::Error, "Msg 208, Nivel 16, Estado 1\nInvalid object name 'x'."),
                (MessageLevel::Info, "(1 fila afectada)"),
                (MessageLevel::Info, "(3 filas afectadas)"),
            ]
        );
        // The error itself keeps the server's words (what other screens show).
        assert_eq!(out.error.as_deref(), Some("Invalid object name 'x'."));
        assert_eq!(out.errors[0].line, Some(2));
    }

    #[test]
    fn an_invalid_go_count_is_reported() {
        let b = batches("select 1\nGO 0\n");
        assert!(b.iter().any(|u| u.error.is_some()));
    }
}
