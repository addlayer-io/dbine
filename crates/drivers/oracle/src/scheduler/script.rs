//! The PL/SQL block a change runs, written once and shown two ways: as the
//! script the designer shows (secrets masked, ending in `/`) and as the
//! block that runs (secrets bound as `:1`, `:2`…).

use super::no_open_transaction;
use crate::jobs::{self, JobId};
use crate::{db_code, quote};
use dbine_driver::{Error, Result, SECRET_MASK};
use oracledb::{Connection, ToDbValue};
use std::fmt::Write as _;

/// Where a secret goes in the body until it's rendered. Text from the app
/// never carries it into the body: [`lit`] writes the character as
/// `CHR(1)`.
const MARK: char = '\u{1}';

/// The error a block raises when a call after its disable fails: the
/// object stays disabled (the calls before it are already committed).
const LEFT_DISABLED: usize = 20991;

/// The note of a block whose calls commit one by one.
const NOT_ATOMIC: &str =
    "Oracle confirma cada llamada: si una falla, las anteriores ya quedaron hechas.";

/// A call's argument.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum A {
    Null,
    /// Text: a literal.
    S(String),
    I(i64),
    B(bool),
    /// PL/SQL as is: built here (a timestamp, a constant), never the app's.
    R(String),
    /// An object's id, written `'"OWNER"."NAME"'`.
    Name(String),
    /// A password: masked in the script, bound in the block that runs.
    Secret(String),
}

impl A {
    pub(crate) fn opt(v: Option<String>) -> A {
        v.map_or(A::Null, A::S)
    }

    /// Seconds as an `INTERVAL DAY TO SECOND`.
    pub(crate) fn secs(n: Option<i64>) -> A {
        n.map_or(A::Null, |n| A::R(format!("NUMTODSINTERVAL({n}, 'SECOND')")))
    }

    pub(crate) fn int(n: Option<i64>) -> A {
        n.map_or(A::Null, A::I)
    }

    pub(crate) fn is_null(&self) -> bool {
        *self == A::Null
    }
}

/// Text for a `--` comment: one line.
pub(crate) fn comment_text(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// A line SQL*Plus (and DBine's editor) would read as the end of the
/// block (`/`) or of its buffer (`.`).
fn ends_block(line: &str) -> bool {
    matches!(line.trim(), "/" | ".")
}

/// A control character written as `CHR(n)`: all but tab and line breaks
/// (U+0001 is where the block keeps a secret until it's rendered).
fn odd(c: char) -> bool {
    c.is_ascii_control() && !matches!(c, '\t' | '\n' | '\r')
}

/// One line as `'text'`, its control characters as `|| CHR(n) ||`.
fn line_lit(s: &str) -> String {
    if !s.contains(odd) {
        return jobs::lit(s);
    }
    let mut parts = Vec::new();
    let mut run = String::new();
    for c in s.chars() {
        if odd(c) {
            if !run.is_empty() {
                parts.push(jobs::lit(&std::mem::take(&mut run)));
            }
            parts.push(format!("CHR({})", c as u32));
        } else {
            run.push(c);
        }
    }
    if !run.is_empty() {
        parts.push(jobs::lit(&run));
    }
    parts.join(" || ")
}

/// `'text'`, quotes doubled. Text with a line that would end the block is
/// written line by line, joined with `CHR(10)`, so no such line is left;
/// control characters (but tab and line breaks) go as `CHR(n)`.
pub(crate) fn lit(s: &str) -> String {
    if !s.lines().any(ends_block) {
        if !s.contains(odd) {
            return jobs::lit(s);
        }
        return format!("({})", line_lit(s));
    }
    let lines: Vec<String> = s.split('\n').map(line_lit).collect();
    format!("({})", lines.join(" || CHR(10) || "))
}

/// `'"OWNER"."NAME"'` of an id the plan already checked.
pub(crate) fn name_lit(id: &str) -> String {
    match jobs::parse_id(id) {
        Ok(JobId::Scheduler { owner, name }) => lit(&format!("{}.{}", quote(&owner), quote(&name))),
        _ => lit(id),
    }
}

/// A call or a statement of the block.
#[derive(Debug, Default)]
pub(crate) struct Block {
    head: Vec<String>,
    body: String,
    declares: Vec<String>,
    /// Run when a call after the create fails (the object is dropped).
    undo: Option<String>,
    /// The object the block disabled first: a call that fails after the
    /// disable leaves it disabled, and the error says so (enabled again,
    /// it would run half changed).
    disabled: Option<String>,
    /// `DBMS_JOB` is transactional: rolled back on failure.
    rollback: bool,
    /// Ends handing back `j`, the new `DBMS_JOB` job's number: `:1` in the
    /// block that runs, printed in the script.
    returns_job: bool,
    secrets: Vec<String>,
    calls: usize,
}

impl Block {
    pub(crate) fn new(comment: &str) -> Block {
        Block {
            head: vec![comment_text(comment)],
            ..Block::default()
        }
    }

    /// Another comment line at the top.
    pub(crate) fn note(&mut self, text: &str) {
        self.head.push(comment_text(text));
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls
    }

    /// All or nothing: one call, a create the block drops again on a
    /// failure, or `DBMS_JOB`'s calls in one transaction. Otherwise the
    /// script says each call commits on its own.
    pub(crate) fn atomic(&self) -> bool {
        self.calls <= 1 || self.undo.is_some() || self.rollback
    }

    pub(crate) fn declare(&mut self, d: &str) {
        if !self.declares.iter().any(|x| x == d) {
            self.declares.push(d.to_string());
        }
    }

    pub(crate) fn rollback_on_error(&mut self) {
        self.rollback = true;
    }

    pub(crate) fn returns_job(&mut self) {
        self.returns_job = true;
    }

    fn render(&mut self, a: A) -> String {
        match a {
            A::Null => "NULL".into(),
            A::S(s) => lit(&s),
            A::I(n) => n.to_string(),
            A::B(b) => if b { "TRUE" } else { "FALSE" }.into(),
            A::R(r) => r,
            A::Name(id) => name_lit(&id),
            A::Secret(s) => {
                self.secrets.push(s);
                format!("{MARK}{}{MARK}", self.secrets.len())
            }
        }
    }

    fn push_call(&mut self, proc_name: &str, args: Vec<String>) {
        let one = format!("{proc_name}({});", args.join(", "));
        if one.len() <= 110 && !one.contains('\n') {
            let _ = writeln!(self.body, "  {one}");
        } else {
            let _ = writeln!(
                self.body,
                "  {proc_name}(\n      {});",
                args.join(",\n      ")
            );
        }
        self.calls += 1;
    }

    /// `proc(name => value, …);`
    pub(crate) fn call(&mut self, proc_name: &str, args: Vec<(&str, A)>) {
        let rendered = args
            .into_iter()
            .map(|(k, a)| format!("{k} => {}", self.render(a)))
            .collect();
        self.push_call(proc_name, rendered);
    }

    /// A call that only checks (it changes nothing): written like
    /// [`Block::call`], not counted among the changes.
    pub(crate) fn check(&mut self, proc_name: &str, args: Vec<(&str, A)>) {
        self.call(proc_name, args);
        self.calls -= 1;
    }

    /// `proc(value, …);`
    pub(crate) fn pcall(&mut self, proc_name: &str, args: Vec<A>) {
        let rendered = args.into_iter().map(|a| self.render(a)).collect();
        self.push_call(proc_name, rendered);
    }

    /// `SET_ATTRIBUTE(target, 'attr', value)`, or `SET_ATTRIBUTE_NULL` for
    /// a null.
    pub(crate) fn set_attr(&mut self, target: &A, attr: &str, value: A) {
        if value.is_null() {
            self.pcall(
                "DBMS_SCHEDULER.SET_ATTRIBUTE_NULL",
                vec![target.clone(), A::S(attr.into())],
            );
        } else {
            self.pcall(
                "DBMS_SCHEDULER.SET_ATTRIBUTE",
                vec![target.clone(), A::S(attr.into()), value],
            );
        }
    }

    /// `SET_ATTRIBUTE` with two values (`event_spec`).
    pub(crate) fn set_attr2(&mut self, target: &A, attr: &str, v1: A, v2: A) {
        if v1.is_null() && v2.is_null() {
            self.set_attr(target, attr, A::Null);
        } else {
            self.pcall(
                "DBMS_SCHEDULER.SET_ATTRIBUTE",
                vec![target.clone(), A::S(attr.into()), v1, v2],
            );
        }
    }

    /// A statement as is (built here).
    pub(crate) fn line(&mut self, s: &str) {
        let _ = writeln!(self.body, "  {s}");
    }

    /// From here on a failure drops what was created, with `undo`.
    pub(crate) fn created(&mut self, undo: &str) {
        self.declare("created BOOLEAN := FALSE;");
        self.line("created := TRUE;");
        self.undo = Some(undo.to_string());
    }

    /// `disable` (of the object `id`) runs now; from here on a failure
    /// leaves it disabled and raises [`LEFT_DISABLED`], which says so. It's
    /// never enabled again on a failure: `DBMS_SCHEDULER` has committed
    /// the calls before, and the object would run half changed (a job
    /// whose old schedule was nulled runs at once, and is dropped after
    /// if it's `auto_drop`).
    pub(crate) fn disabled_first(&mut self, id: &str, disable: impl FnOnce(&mut Block)) {
        disable(self);
        self.declare("disabled BOOLEAN := FALSE;");
        self.line("disabled := TRUE;");
        self.disabled = Some(id.to_string());
        self.note("Si una llamada falla después de deshabilitarlo, queda deshabilitado (no se vuelve a habilitar a medio cambiar): el error lo dice.");
    }

    /// `enable` runs now, and a later failure leaves it so.
    pub(crate) fn enabled_again(&mut self, enable: impl FnOnce(&mut Block)) {
        enable(self);
        if self.disabled.is_some() {
            self.line("disabled := FALSE;");
        }
    }

    fn text(&self, run: bool, secret: &dyn Fn(usize) -> String) -> String {
        let head = !run;
        let mut out = String::new();
        if head {
            for (i, h) in self.head.iter().enumerate() {
                let _ = writeln!(out, "-- {h}");
                if i == 0 && !self.atomic() {
                    let _ = writeln!(out, "-- {NOT_ATOMIC}");
                }
            }
        }
        let plain = self.declares.is_empty()
            && self.undo.is_none()
            && self.disabled.is_none()
            && !self.rollback;
        if !self.declares.is_empty() {
            out.push_str("DECLARE\n");
            for d in &self.declares {
                let _ = writeln!(out, "  {d}");
            }
        }
        out.push_str("BEGIN\n");
        // Secrets: `MARK n MARK`.
        let mut body = String::new();
        let mut rest = self.body.as_str();
        while let Some(i) = rest.find(MARK) {
            body.push_str(&rest[..i]);
            let after = &rest[i + MARK.len_utf8()..];
            let j = after.find(MARK).unwrap_or(after.len());
            let n: usize = after[..j].parse().unwrap_or(0);
            body.push_str(&secret(n));
            rest = after.get(j + MARK.len_utf8()..).unwrap_or("");
        }
        body.push_str(rest);
        out.push_str(&body);
        if self.returns_job {
            out.push_str(if run {
                "  :1 := j;\n"
            } else {
                "  DBMS_OUTPUT.PUT_LINE('Job ' || j);\n"
            });
        }
        if !plain {
            out.push_str("EXCEPTION\n  WHEN OTHERS THEN\n");
            if let Some(s) = &self.undo {
                let _ = writeln!(
                    out,
                    "    IF created THEN\n      BEGIN\n        {s}\n      EXCEPTION\n        WHEN OTHERS THEN NULL;\n      END;\n    END IF;"
                );
            }
            if let Some(id) = &self.disabled {
                // The server's error stays under this one (`TRUE`).
                let _ = writeln!(
                    out,
                    "    IF disabled THEN\n      RAISE_APPLICATION_ERROR(-{LEFT_DISABLED}, {}, TRUE);\n    END IF;",
                    lit(&format!(
                        "{} quedó deshabilitado: revisalo y habilitalo.",
                        comment_text(id)
                    ))
                );
            }
            if self.rollback {
                out.push_str("    ROLLBACK;\n");
            }
            out.push_str("    RAISE;\n");
        }
        out.push_str("END;");
        out
    }

    /// The script the designer shows: comments on top, secrets masked,
    /// ending in `/`.
    pub(crate) fn script(&self) -> String {
        let mask = lit(SECRET_MASK);
        let mut s = self.text(false, &|_| mask.clone());
        s.push_str("\n/\n");
        s
    }

    /// The block that runs and the secrets it binds, in order.
    pub(crate) fn runnable(&self) -> (String, Vec<String>) {
        (self.text(true, &|n| format!(":{n}")), self.secrets.clone())
    }
}

/// The server's message without the PL/SQL call stack.
pub(crate) fn message(e: &oracledb::Error) -> String {
    let text = e.to_string();
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty() && !l.starts_with("ORA-06512") && !l.starts_with("Help:"))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        e.to_string()
    } else {
        lines.join("\n")
    }
}

fn run_err(e: oracledb::Error, b: &Block) -> Error {
    match (db_code(&e), &b.disabled) {
        (Some(LEFT_DISABLED), Some(id)) => {
            // The block's own line on top; the server's error under it.
            let text = message(&e);
            let own = format!("ORA-{LEFT_DISABLED}");
            let cause: Vec<&str> = text.lines().filter(|l| !l.starts_with(&own)).collect();
            let cause = if cause.is_empty() {
                text.clone()
            } else {
                cause.join("\n")
            };
            Error::Query(format!(
                "«{}» quedó deshabilitado: una llamada falló después de deshabilitarlo y las anteriores ya quedaron hechas. Revisalo y habilitalo. {}",
                comment_text(id),
                cause
            ))
        }
        (Some(27486 | 1031), _) => Error::Query(format!(
            "Este usuario no tiene los privilegios para este cambio: {}",
            message(&e)
        )),
        _ => Error::Query(message(&e)),
    }
}

/// Runs the block, its secrets bound, outside a transaction of the
/// session's.
pub(crate) fn run(c: &Connection, b: &Block) -> Result<()> {
    no_open_transaction(c)?;
    let (sql, secrets) = b.runnable();
    let params: Vec<&dyn ToDbValue> = secrets.iter().map(|s| s as &dyn ToDbValue).collect();
    c.execute(&sql, &params)
        .map(|_| ())
        .map_err(|e| run_err(e, b))
}

/// Runs a `DBMS_JOB.SUBMIT` block, which ends `:1 := j;`, and returns the
/// new job's number.
pub(crate) fn run_returning(c: &Connection, b: &Block) -> Result<i64> {
    no_open_transaction(c)?;
    let (sql, _) = b.runnable();
    // An IN OUT bind: the value goes in and the job's number comes back.
    let mut r = c.execute(&sql, &[&0i64]).map_err(|e| run_err(e, b))?;
    let n: Option<i64> = r.out_bind_data().get(0).map_err(crate::err)?;
    n.ok_or_else(|| Error::Query("DBMS_JOB.SUBMIT no devolvió el número del job".into()))
}
