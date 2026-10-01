//! What the drivers return: explorer objects and query results. Serialized
//! as-is to the UI (snake_case fields, mirrored by hand in
//! `web/src/api/types.ts`).

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// An object in the explorer. `kind` is one of the driver's
/// [`crate::ObjectKindInfo::id`]s (see [`crate::kinds`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbObject {
    pub kind: String,
    /// `None` on engines without schemas.
    pub schema: Option<String>,
    pub name: String,
    /// The owner of a dependent object (a trigger's table…).
    #[serde(default)]
    pub parent: Option<String>,
}

/// A schema of the session's database, listed even when it holds no
/// objects (see [`crate::Session::list_schemas`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaInfo {
    pub name: String,
    /// Built into the engine (`sys`, `INFORMATION_SCHEMA`, `pg_catalog`,
    /// `db_owner`…): the explorer hides it while it has no objects.
    #[serde(default)]
    pub system: bool,
}

/// Which object a call is about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectRef {
    pub kind: String,
    pub schema: Option<String>,
    pub name: String,
}

impl ObjectRef {
    pub fn schema(&self) -> Option<&str> {
        self.schema.as_deref().filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
    pub auto_increment: bool,
    pub default_value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultColumn {
    pub name: String,
    /// Engine type name when the driver knows it ("" otherwise).
    pub type_name: String,
}

/// One statement's outcome: a result set (`columns` not empty) and/or a
/// count of affected rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatementResult {
    pub columns: Vec<ResultColumn>,
    /// Cells as JSON: null, bool, number (only when it fits a JS number
    /// exactly), or string (decimals, dates, binary as 0x…).
    pub rows: Vec<Vec<serde_json::Value>>,
    /// Rows the statement returned, including those dropped past the limit.
    pub total_rows: u64,
    /// More rows than the limit came back; only the first ones are in `rows`.
    pub truncated: bool,
    pub rows_affected: Option<u64>,
    /// Which statement of the script produced it (0-based, as
    /// [`crate::sql::split_script`] numbers them). Set by the app when it
    /// runs a script statement by statement; `None` when the driver got the
    /// whole script at once.
    #[serde(default)]
    pub statement: Option<usize>,
    /// Where that statement starts in the script that ran, and its line
    /// (1-based). Set by the app with `statement`.
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub line: Option<u32>,
    /// The engine's completion tag ("INSERT 0 3", "Table created",
    /// "CREATE TABLE"…), when the driver has it.
    #[serde(default)]
    pub tag: Option<String>,
    /// How long the statement took. Set by the app when it runs a script
    /// statement by statement, unless the driver set it.
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
}

/// How serious a [`Message`] is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageLevel {
    /// PRINT, NOTICE, DBMS_OUTPUT, "(3 rows affected)"…
    #[default]
    Info,
    /// A server warning (SHOW WARNINGS, PostgreSQL WARNING, SQL Server
    /// severity 10 warnings…).
    Warning,
    /// A statement failed (also in [`QueryOutcome::errors`]).
    Error,
}

/// One server or client message of a run, in the order it arrived.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub level: MessageLevel,
    pub text: String,
    /// The script statement it came from (see [`StatementResult::statement`]).
    #[serde(default)]
    pub statement: Option<usize>,
    /// The engine's code (SQL Server message number, SQLSTATE, ORA-nnnnn…).
    #[serde(default)]
    pub code: Option<String>,
    /// 1-based line in the script, when the engine says where.
    #[serde(default)]
    pub line: Option<u32>,
}

/// A failed statement, with what the engine says about where and why.
/// Drivers return it as [`crate::Error::Statement`]; `offset` and `line`
/// are then relative to the text the driver got, and the app moves them to
/// the script and fills `statement`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptError {
    pub message: String,
    /// The engine's error code: SQL Server's message number, MySQL's error
    /// number, `ORA-00942`, `Neo.ClientError…`.
    #[serde(default)]
    pub code: Option<String>,
    /// The SQLSTATE, when the engine has one.
    #[serde(default)]
    pub sqlstate: Option<String>,
    /// Which statement of the script failed (set by the app).
    #[serde(default)]
    pub statement: Option<usize>,
    /// Where it failed: byte offset (into the driver's text; the app turns
    /// it into an offset of the script the UI sent).
    #[serde(default)]
    pub offset: Option<usize>,
    /// 1-based line (same convention as `offset`).
    #[serde(default)]
    pub line: Option<u32>,
    /// The script can't go on after it, even when it continues on errors
    /// (SQL Server severity 20 and up, a lost connection…).
    #[serde(default)]
    pub fatal: bool,
}

impl ScriptError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), ..Default::default() }
    }
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
    pub fn with_sqlstate(mut self, sqlstate: impl Into<String>) -> Self {
        self.sqlstate = Some(sqlstate.into());
        self
    }
    /// Byte offset of the error in the text the driver ran.
    pub fn at_offset(mut self, offset: usize) -> Self {
        self.offset = Some(offset);
        self
    }
    /// 1-based line of the error in the text the driver ran.
    pub fn at_line(mut self, line: u32) -> Self {
        self.line = Some(line);
        self
    }
    pub fn fatal(mut self) -> Self {
        self.fatal = true;
        self
    }
}

/// Whether the session is inside a transaction (see
/// [`crate::Session::transaction_state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxState {
    /// No transaction open: autocommit, or nothing ran since the last
    /// commit / rollback.
    Idle,
    /// A transaction is open, with changes not yet committed.
    Open,
    /// A transaction is open but failed: only a rollback ends it
    /// (PostgreSQL's "current transaction is aborted").
    Failed,
}

/// Receives the messages of a run as they arrive (see
/// [`QueryOutcome::message_sink`]), so the UI shows them live.
#[derive(Clone)]
pub struct MessageSinkRef(pub Arc<dyn Fn(&Message) + Send + Sync>);

impl std::fmt::Debug for MessageSinkRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MessageSink")
    }
}

/// A statement ended inside one `execute` call of a driver that splits the
/// script itself (`Whole`): what it produced, for the editor's live
/// progress (see [`QueryOutcome::progress_sink`]). `offset` and `line` are
/// its place in the text the driver got.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatementEnd {
    pub statement: usize,
    pub offset: usize,
    pub line: u32,
    pub elapsed_ms: u64,
    #[serde(default)]
    pub results: Vec<StatementResult>,
    #[serde(default)]
    pub log: Vec<Message>,
    #[serde(default)]
    pub errors: Vec<ScriptError>,
}

/// Receives each [`StatementEnd`] of a run (see
/// [`QueryOutcome::progress_sink`]).
#[derive(Clone)]
pub struct ProgressSinkRef(pub Arc<dyn Fn(&StatementEnd) + Send + Sync>);

impl std::fmt::Debug for ProgressSinkRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProgressSink")
    }
}

/// Everything a script produced.
///
/// A driver's `execute` stops at a failing statement: the results before it
/// stay and the `Err` says what failed. When the app runs a script
/// statement by statement (see [`crate::Driver::script_mode`]) it may go on
/// after an error: every failure is then in `errors`, in order, and
/// `error` keeps the first one's text (what older UIs show).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryOutcome {
    pub results: Vec<StatementResult>,
    /// Server messages (PRINT, notices, warnings…) as plain text. Drivers
    /// add them with [`Self::info`] / [`Self::warning`] / [`Self::message`]
    /// (which also fill `log`); text pushed here directly still works and
    /// joins `log` as `Info` (see [`Self::adopt_plain_messages`]).
    pub messages: Vec<String>,
    pub error: Option<String>,
    pub elapsed_ms: u64,
    /// Execution plans, when the run asked for them ([`crate::Session::explain`]).
    #[serde(default)]
    pub plans: Vec<Plan>,
    /// Every message with its level and statement, errors included, in the
    /// order they happened. Empty in outcomes of drivers built before it:
    /// read `messages` and `error` then.
    #[serde(default)]
    pub log: Vec<Message>,
    /// Every statement that failed, in order.
    #[serde(default)]
    pub errors: Vec<ScriptError>,
    /// The session's transaction after the run, when the driver tracks it
    /// ([`crate::Session::transaction_state`]); set by the app.
    #[serde(default)]
    pub transaction: Option<TxState>,
    /// The session's database after the run, set by the driver when a
    /// statement switched it (T-SQL / MySQL `USE`, Mongo `use db`…): the
    /// editor tab then follows it on the same session. `None`: unchanged,
    /// or the driver doesn't track it (also what older hosts send).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Gets each message as it's added (set by the app, never by drivers).
    #[serde(skip)]
    pub message_sink: Option<MessageSinkRef>,
    /// Editor runs of a driver that splits the script itself (`Whole`):
    /// `Some(true)` goes on after a failed statement, as the engine's shell
    /// does, recording each failure with [`Self::push_error`]. `None` or
    /// `Some(false)`: stop at the first (every caller but the editor). Set
    /// by the app, never by drivers.
    #[serde(skip)]
    pub continue_on_error: Option<bool>,
    /// Gets each statement as it ends, on such drivers' editor runs (set
    /// by the app, never by drivers).
    #[serde(skip)]
    pub progress_sink: Option<ProgressSinkRef>,
    /// The script statement running now: messages added without one get
    /// it. Set by the app.
    #[serde(skip)]
    pub current_statement: Option<usize>,
    /// How many of `messages` are already in `log`.
    #[serde(skip)]
    pub logged: usize,
    /// Where rows go instead of `rows` (exports): set by the app, never by
    /// drivers. Drivers don't notice: `begin_result` / `push_row` route
    /// through it, so every engine streams to a file without buffering.
    #[serde(skip)]
    pub sink: Option<RowSinkRef>,
    /// First error the sink returned; rows after it are dropped.
    #[serde(skip)]
    pub sink_error: Option<String>,
    /// Result sets before this outcome's first one (see [`Self::fork`]).
    #[serde(skip)]
    pub sink_base: usize,
}

/// Receives the rows of a run as they arrive (see [`QueryOutcome::sink`]).
pub trait RowSink: Send {
    /// Result set `index` (0-based, in this run) starts.
    fn begin(&mut self, index: usize, columns: &[ResultColumn]) -> std::io::Result<()>;
    fn row(&mut self, index: usize, row: &[serde_json::Value]) -> std::io::Result<()>;
}

/// A shared [`RowSink`] (the outcome is `Clone`/`Debug`; sinks aren't).
#[derive(Clone)]
pub struct RowSinkRef(pub Arc<Mutex<dyn RowSink>>);

impl std::fmt::Debug for RowSinkRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RowSink")
    }
}

/// One statement's execution plan as a tree of operators, the way SSMS
/// draws it: the root is the statement's last operator (the one returning
/// rows), children feed it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Plan {
    /// The statement the plan is for.
    pub statement: String,
    pub root: PlanNode,
    /// Actual figures (the statement ran) or estimates only.
    pub actual: bool,
    /// The engine's own plan, as it came: "showplan_xml", "json", "text"…
    pub raw_format: String,
    pub raw: String,
}

/// An operator. Every figure is optional: engines report different ones.
/// Costs are in the engine's own units; the UI turns `self_cost` into a
/// share of the root's `total_cost`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanNode {
    /// Operator ("Clustered Index Seek", "Hash Join", "Seq Scan"…).
    pub op: String,
    /// Refinement of the operator (logical op, join type, access type…).
    #[serde(default)]
    pub detail: String,
    /// Table / index it reads, if any.
    #[serde(default)]
    pub object: Option<String>,
    /// Cost of this operator and everything below it.
    #[serde(default)]
    pub total_cost: Option<f64>,
    /// Cost of this operator alone; when `None` the UI derives it from
    /// `total_cost` minus the children's.
    #[serde(default)]
    pub self_cost: Option<f64>,
    /// Estimated rows **per execution** (as engines report them).
    #[serde(default)]
    pub est_rows: Option<f64>,
    /// Actual rows **in total**, over all executions.
    #[serde(default)]
    pub actual_rows: Option<f64>,
    /// How many times it ran (loops / executions).
    #[serde(default)]
    pub executions: Option<f64>,
    #[serde(default)]
    pub actual_ms: Option<f64>,
    /// Things worth a warning icon (missing index, spill, implicit
    /// conversion, no join predicate…).
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Everything else, for the properties panel.
    #[serde(default)]
    pub props: Vec<(String, String)>,
    #[serde(default)]
    pub children: Vec<PlanNode>,
}

impl QueryOutcome {
    /// An empty outcome that continues this one: drivers that fill a local
    /// outcome (on a blocking thread, per statement…) must start it with
    /// `fork` and hand it back with [`Self::merge`], so rows still reach the
    /// sink with the right result-set numbers.
    pub fn fork(&self) -> QueryOutcome {
        QueryOutcome {
            sink: self.sink.clone(),
            sink_base: self.sink_base + self.results.len(),
            message_sink: self.message_sink.clone(),
            continue_on_error: self.continue_on_error,
            progress_sink: self.progress_sink.clone(),
            current_statement: self.current_statement,
            ..Default::default()
        }
    }

    /// Take back what a [`Self::fork`] collected.
    pub fn merge(&mut self, child: QueryOutcome) {
        let sink_error = child.sink_error.clone();
        self.absorb(child);
        if self.sink_error.is_none() {
            self.sink_error = sink_error;
        }
    }

    /// Add `other`'s results, messages, plans, log and errors after this
    /// outcome's (its `error`, `elapsed_ms` and `transaction` are left to the
    /// caller). `other` may come from an older driver (no `log`) or over the
    /// wire (its `logged` count is lost): plain messages join `log` as `Info`
    /// unless `log` already has its messages. Its log entries and errors
    /// without a statement get the running one.
    pub fn absorb(&mut self, mut other: QueryOutcome) {
        self.adopt_plain_messages();
        // `messages[..logged]` are the log's non-error entries, in order
        // ([`Self::message`] adopts earlier plain ones first): count them,
        // as the count itself doesn't cross the wire. Older hosts send no
        // log, so all their messages are plain.
        other.logged = other.log.iter().filter(|m| m.level != MessageLevel::Error).count().min(other.messages.len());
        let statement = other.current_statement.or(self.current_statement);
        for m in &mut other.log {
            m.statement = m.statement.or(statement);
        }
        for e in &mut other.errors {
            e.statement = e.statement.or(statement);
        }
        // Its plain messages reach our live sink as they join the log.
        other.message_sink = self.message_sink.clone();
        other.current_statement = statement;
        other.adopt_plain_messages();
        if other.database.is_some() {
            self.database = other.database.take();
        }
        self.results.extend(other.results);
        self.messages.extend(other.messages);
        self.logged = self.messages.len();
        self.plans.extend(other.plans);
        self.log.extend(other.log);
        self.errors.extend(other.errors);
    }

    /// Add a message: to `log` (stamped with the running statement), to
    /// `messages` unless it's an error, and to the live sink.
    pub fn message(&mut self, mut m: Message) {
        self.adopt_plain_messages();
        if m.statement.is_none() {
            m.statement = self.current_statement;
        }
        if m.level != MessageLevel::Error {
            self.messages.push(m.text.clone());
            self.logged = self.messages.len();
        }
        if let Some(sink) = &self.message_sink {
            (sink.0)(&m);
        }
        self.log.push(m);
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.message(Message { level: MessageLevel::Info, text: text.into(), ..Default::default() });
    }

    pub fn warning(&mut self, text: impl Into<String>) {
        self.message(Message { level: MessageLevel::Warning, text: text.into(), ..Default::default() });
    }

    /// Record a failed statement: in `errors`, in `log` as an error, and in
    /// `error` when it's the first.
    pub fn push_error(&mut self, mut e: ScriptError) {
        if e.statement.is_none() {
            e.statement = self.current_statement;
        }
        self.message(Message { level: MessageLevel::Error, text: e.message.clone(), statement: e.statement, code: e.code.clone(), line: e.line });
        if self.error.is_none() {
            self.error = Some(e.message.clone());
        }
        self.errors.push(e);
    }

    /// Messages pushed straight to `messages` (drivers written before
    /// `log`) join `log` as `Info`, stamped with the running statement.
    pub fn adopt_plain_messages(&mut self) {
        while self.logged < self.messages.len() {
            let m = Message { level: MessageLevel::Info, text: self.messages[self.logged].clone(), statement: self.current_statement, ..Default::default() };
            self.logged += 1;
            if let Some(sink) = &self.message_sink {
                (sink.0)(&m);
            }
            self.log.push(m);
        }
    }

    /// Start a new result set with these columns.
    pub fn begin_result(&mut self, columns: Vec<ResultColumn>) {
        if let Some(sink) = &self.sink {
            let index = self.sink_base + self.results.len();
            if self.sink_error.is_none() {
                if let Err(e) = sink.0.lock().expect("sink").begin(index, &columns) {
                    self.sink_error = Some(e.to_string());
                }
            }
        }
        self.results.push(StatementResult { columns, ..Default::default() });
    }

    /// Add a row to the current result set, keeping at most `max_rows`.
    pub fn push_row(&mut self, row: Vec<serde_json::Value>, max_rows: usize) {
        if self.results.is_empty() {
            self.begin_result(Vec::new());
        }
        let index = self.sink_base + self.results.len() - 1;
        let r = self.results.last_mut().expect("a result set");
        r.total_rows += 1;
        if let Some(sink) = &self.sink {
            if self.sink_error.is_none() {
                if let Err(e) = sink.0.lock().expect("sink").row(index, &row) {
                    self.sink_error = Some(e.to_string());
                }
            }
            return;
        }
        if r.rows.len() < max_rows {
            r.rows.push(row);
        } else {
            r.truncated = true;
        }
    }

    /// Record a statement that returned no rows.
    pub fn push_affected(&mut self, n: u64) {
        self.results.push(StatementResult { rows_affected: Some(n), ..Default::default() });
    }
}

/// Integers past ±2^53 go as strings so JS doesn't round them.
pub fn json_i64(v: i64) -> serde_json::Value {
    const MAX_SAFE: i64 = (1 << 53) - 1;
    if (-MAX_SAFE..=MAX_SAFE).contains(&v) {
        v.into()
    } else {
        v.to_string().into()
    }
}

pub fn json_u64(v: u64) -> serde_json::Value {
    match i64::try_from(v) {
        Ok(i) => json_i64(i),
        Err(_) => v.to_string().into(),
    }
}

pub fn json_f64(v: f64) -> serde_json::Value {
    serde_json::Number::from_f64(v).map_or_else(|| v.to_string().into(), serde_json::Value::Number)
}

/// Binary shown as `0x…` hex, cut at 1 KiB.
pub fn json_bytes(b: &[u8]) -> serde_json::Value {
    const MAX: usize = 1024;
    let mut s = String::with_capacity(2 + b.len().min(MAX) * 2);
    s.push_str("0x");
    for byte in b.iter().take(MAX) {
        s.push_str(&format!("{byte:02X}"));
    }
    if b.len() > MAX {
        s.push('…');
    }
    s.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_info_reads_back_and_defaults_to_user() {
        let s = SchemaInfo { name: "ventas".into(), system: true };
        let back: SchemaInfo = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        let bare: SchemaInfo = serde_json::from_value(serde_json::json!({ "name": "dbo" })).unwrap();
        assert!(!bare.system);
    }

    #[test]
    fn rows_past_the_limit_are_counted_not_kept() {
        let mut o = QueryOutcome::default();
        o.begin_result(vec![]);
        for i in 0..5 {
            o.push_row(vec![i.into()], 3);
        }
        assert_eq!(o.results[0].rows.len(), 3);
        assert_eq!(o.results[0].total_rows, 5);
        assert!(o.results[0].truncated);
    }

    #[test]
    fn a_sink_takes_the_rows_instead_of_memory() {
        struct Count(Vec<(usize, usize)>);
        impl RowSink for Count {
            fn begin(&mut self, index: usize, cols: &[ResultColumn]) -> std::io::Result<()> {
                self.0.push((index, cols.len()));
                Ok(())
            }
            fn row(&mut self, index: usize, _: &[serde_json::Value]) -> std::io::Result<()> {
                self.0.push((index, 99));
                Ok(())
            }
        }
        let sink = Arc::new(Mutex::new(Count(Vec::new())));
        let mut o = QueryOutcome { sink: Some(RowSinkRef(sink.clone())), ..Default::default() };
        o.begin_result(vec![ResultColumn { name: "a".into(), type_name: String::new() }]);
        o.push_row(vec![1.into()], 1);
        o.push_row(vec![2.into()], 1);
        assert!(o.results[0].rows.is_empty(), "nothing buffered");
        assert_eq!(o.results[0].total_rows, 2);
        assert!(!o.results[0].truncated);
        assert_eq!(sink.lock().unwrap().0, vec![(0, 1), (0, 99), (0, 99)]);

        // A fork (a driver's local outcome) numbers its result sets after
        // the parent's, and merging brings them back.
        let mut child = o.fork();
        child.begin_result(vec![]);
        child.push_row(vec![3.into()], 1);
        o.merge(child);
        assert_eq!(o.results.len(), 2);
        assert_eq!(sink.lock().unwrap().0[3..], [(1, 0), (1, 99)]);
    }

    #[test]
    fn outcomes_of_older_drivers_still_read() {
        // What a host published before the script fields sends.
        let old = serde_json::json!({
            "results": [{ "columns": [], "rows": [], "total_rows": 0, "truncated": false, "rows_affected": 3 }],
            "messages": ["hola"],
            "error": "falló",
            "elapsed_ms": 5,
        });
        let o: QueryOutcome = serde_json::from_value(old).unwrap();
        assert_eq!(o.results[0].rows_affected, Some(3));
        assert_eq!(o.results[0].statement, None);
        assert!(o.log.is_empty() && o.errors.is_empty() && o.transaction.is_none());
        // Absorbed (as the plugin client does), its plain messages join the log.
        let mut app = QueryOutcome { current_statement: Some(2), ..Default::default() };
        app.absorb(o);
        assert_eq!(app.messages, vec!["hola"]);
        assert_eq!(app.log, vec![Message { text: "hola".into(), statement: Some(2), ..Default::default() }]);
    }

    #[test]
    fn host_outcomes_join_the_running_statement() {
        // A plugin host's outcome: a logged message, then a plain one.
        let mut host = QueryOutcome::default();
        host.messages.push("plain before".into());
        host.info("logged");
        host.messages.push("plain after".into());
        let wire: QueryOutcome = serde_json::from_value(serde_json::to_value(&host).unwrap()).unwrap();
        let mut app = QueryOutcome { current_statement: Some(3), ..Default::default() };
        app.absorb(wire);
        let log: Vec<_> = app.log.iter().map(|m| (m.text.as_str(), m.statement)).collect();
        assert_eq!(log, vec![("plain before", Some(3)), ("logged", Some(3)), ("plain after", Some(3))]);
        assert_eq!(app.messages, vec!["plain before", "logged", "plain after"]);
        // Its errors too.
        let mut host = QueryOutcome::default();
        host.push_error(ScriptError::new("falló"));
        let wire: QueryOutcome = serde_json::from_value(serde_json::to_value(&host).unwrap()).unwrap();
        app.absorb(wire);
        assert_eq!((app.errors[0].statement, app.log[3].statement), (Some(3), Some(3)));
    }

    #[test]
    fn new_fields_round_trip() {
        let mut o = QueryOutcome::default();
        o.results.push(StatementResult { statement: Some(1), offset: Some(10), line: Some(2), tag: Some("INSERT 0 1".into()), elapsed_ms: Some(4), ..Default::default() });
        o.warning("cuidado");
        o.push_error(ScriptError::new("no existe").with_code("208").with_sqlstate("42S02").at_line(3));
        o.transaction = Some(TxState::Open);
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v["log"][0]["level"], "warning");
        assert_eq!(v["log"][1]["level"], "error");
        assert_eq!(v["transaction"], "open");
        let back: QueryOutcome = serde_json::from_value(v).unwrap();
        assert_eq!(back.results[0].tag.as_deref(), Some("INSERT 0 1"));
        assert_eq!(back.errors[0].sqlstate.as_deref(), Some("42S02"));
        assert_eq!(back.error.as_deref(), Some("no existe"));
        // Errors aren't plain messages (older UIs show `error`).
        assert_eq!(back.messages, vec!["cuidado"]);
        // Read back over the wire, nothing is logged twice.
        let mut app = QueryOutcome::default();
        app.absorb(back);
        assert_eq!(app.log.len(), 2);
    }

    #[test]
    fn messages_keep_their_order_and_reach_the_sink() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let mut o = QueryOutcome {
            message_sink: Some(MessageSinkRef(Arc::new(move |m: &Message| s2.lock().unwrap().push(m.text.clone())))),
            current_statement: Some(0),
            ..Default::default()
        };
        o.info("a");
        o.messages.push("b".into()); // an older driver
        o.current_statement = Some(1);
        o.warning("c");
        let mut child = o.fork();
        child.messages.push("d".into());
        child.info("e");
        o.merge(child);
        let texts: Vec<_> = o.log.iter().map(|m| (m.text.as_str(), m.statement)).collect();
        assert_eq!(texts, vec![("a", Some(0)), ("b", Some(1)), ("c", Some(1)), ("d", Some(1)), ("e", Some(1))]);
        assert_eq!(o.messages, vec!["a", "b", "c", "d", "e"]);
        assert_eq!(*seen.lock().unwrap(), vec!["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn statement_errors_keep_their_details() {
        let e: crate::Error = ScriptError::new("syntax").with_code("102").at_offset(7).into();
        assert!(e.is_query() && !e.ends_script());
        assert_eq!(e.to_string(), "syntax");
        assert_eq!(e.to_script_error().offset, Some(7));
        assert!(crate::Error::Connect("x".into()).to_script_error().fatal);
        assert!(crate::Error::from(ScriptError::new("x").fatal()).ends_script());
    }

    #[test]
    fn big_integers_become_strings() {
        assert_eq!(json_i64(42), serde_json::json!(42));
        assert_eq!(json_i64(i64::MAX), serde_json::json!("9223372036854775807"));
        assert_eq!(json_u64(u64::MAX), serde_json::json!("18446744073709551615"));
    }

    #[test]
    fn binary_is_hex() {
        assert_eq!(json_bytes(&[0xde, 0xad]), serde_json::json!("0xDEAD"));
    }
}

/// One row changed in the results grid: which row (`key`: its primary key
/// values, or every original value when there's no key) and the new values
/// (`set`). [`crate::Driver::update_script`] turns a list of them into code.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RowChange {
    pub key: Vec<(String, serde_json::Value)>,
    pub set: Vec<(String, serde_json::Value)>,
    /// The whole row as it was (every column of the result), for engines
    /// that rewrite a row whole (ksqlDB tables, TDengine…): the new row is
    /// this with `set` applied. May be empty.
    #[serde(default)]
    pub row: Vec<(String, serde_json::Value)>,
}

impl RowChange {
    /// The row after the change: `row` with `set` applied (columns only in
    /// `set` go at the end). Empty when `row` is.
    pub fn new_row(&self) -> Vec<(String, serde_json::Value)> {
        if self.row.is_empty() {
            return Vec::new();
        }
        let mut out = self.row.clone();
        for (k, v) in &self.set {
            match out.iter_mut().find(|(c, _)| c == k) {
                Some(slot) => slot.1 = v.clone(),
                None => out.push((k.clone(), v.clone())),
            }
        }
        out
    }
}
