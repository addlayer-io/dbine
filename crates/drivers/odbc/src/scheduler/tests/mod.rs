//! Unit tests of the scheduler (no server): each engine's forms, scripts
//! and plans against canned result sets, the values' checks, and how a
//! plan runs (transaction, output parameters, the new id).

mod altibase;
mod ase;
mod common;
mod dameng;
mod db2;
mod db2i;
mod db2zos;
mod hive;
mod informix;
mod iris;
mod sqla;
mod vertica;
mod virtuoso;

use super::*;
use crate::presets::PRESETS;
use dbine_driver::ScriptError;
use serde_json::json;

pub(super) enum Reply {
    Rows(Set),
    Fail(Option<&'static str>),
    Out(Vec<Option<&'static str>>),
}

/// Answers the first reply whose key is in the SQL (empty otherwise) and
/// logs every statement, with its parameters, and the transaction calls.
#[derive(Default)]
pub(super) struct Fake {
    /// Key in the SQL, a parameter the statement must be bound with (or
    /// none), and the reply.
    replies: Vec<(&'static str, Option<&'static str>, Reply)>,
    pub(super) log: Vec<(String, Vec<String>)>,
    /// "begin", "commit", "rollback".
    pub(super) tx: Vec<&'static str>,
    /// The database has no transactions.
    pub(super) no_tx: bool,
}

impl Fake {
    pub(super) fn rows(
        mut self,
        key: &'static str,
        cols: &[&str],
        rows: Vec<Vec<Option<&str>>>,
    ) -> Self {
        self.replies
            .push((key, None, Reply::Rows(Set::new(cols, rows))));
        self
    }

    /// Rows for `key` only when one of the statement's parameters is
    /// `param` (the same query for another id answers something else).
    pub(super) fn rows_with(
        mut self,
        key: &'static str,
        param: &'static str,
        cols: &[&str],
        rows: Vec<Vec<Option<&str>>>,
    ) -> Self {
        self.replies
            .push((key, Some(param), Reply::Rows(Set::new(cols, rows))));
        self
    }

    pub(super) fn fail(mut self, key: &'static str, code: Option<&'static str>) -> Self {
        self.replies.push((key, None, Reply::Fail(code)));
        self
    }

    pub(super) fn out(mut self, key: &'static str, outs: Vec<Option<&'static str>>) -> Self {
        self.replies.push((key, None, Reply::Out(outs)));
        self
    }

    fn reply(&mut self, sql: &str, params: &[&str]) -> Result<Option<&Reply>> {
        self.log.push((
            sql.to_string(),
            params.iter().map(|p| p.to_string()).collect(),
        ));
        let r = self
            .replies
            .iter()
            .find(|(k, p, _)| sql.contains(k) && p.is_none_or(|p| params.contains(&p)))
            .map(|(_, _, r)| r);
        if let Some(Reply::Fail(code)) = r {
            let mut e = ScriptError::new("falló");
            if let Some(c) = code {
                e = e.with_code(*c);
            }
            return Err(e.into());
        }
        Ok(r)
    }

    pub(super) fn sent(&self, needle: &str) -> bool {
        self.log.iter().any(|(s, _)| s.contains(needle))
    }
}

impl JobSource for Fake {
    fn query(&mut self, sql: &str, params: &[&str]) -> Result<Set> {
        Ok(match self.reply(sql, params)? {
            Some(Reply::Rows(s)) => s.clone(),
            _ => Set::default(),
        })
    }

    fn exec(&mut self, sql: &str, params: &[&str]) -> Result<Option<u64>> {
        self.reply(sql, params)?;
        Ok(None)
    }
}

impl Src for Fake {
    /// Logs the values bound (NULL as "NULL"); answers the reply's values
    /// or NULL for each marker read back.
    fn call(&mut self, sql: &str, params: &[Param]) -> Result<Vec<Option<String>>> {
        let sent: Vec<String> = params
            .iter()
            .filter_map(|p| match p {
                Param::In(v) | Param::InOut(v) => Some(v.clone().unwrap_or_else(|| "NULL".into())),
                Param::Out => None,
            })
            .collect();
        let back = params.iter().filter(|p| !matches!(p, Param::In(_))).count();
        let sent: Vec<&str> = sent.iter().map(String::as_str).collect();
        Ok(match self.reply(sql, &sent)? {
            Some(Reply::Out(v)) => v.iter().map(|o| o.map(str::to_string)).collect(),
            _ => vec![None; back],
        })
    }

    fn begin(&mut self) -> bool {
        self.tx.push("begin");
        !self.no_tx
    }

    fn finish(&mut self, ok: bool) -> Result<()> {
        self.tx.push(if ok { "commit" } else { "rollback" });
        Ok(())
    }
}

pub(super) fn preset(id: &str) -> &'static Preset {
    PRESETS.iter().find(|p| p.id == id).expect("preset")
}

pub(super) fn ctx() -> Ctx {
    Ctx {
        database: "sysadmin".into(),
    }
}

pub(super) fn map(v: Value) -> BTreeMap<String, Value> {
    serde_json::from_value(v).unwrap()
}

pub(super) fn fv(values: Value, lists: Value) -> FormValues {
    FormValues {
        values: map(values),
        lists: serde_json::from_value(lists).unwrap(),
    }
}

/// The error's message, for a refused value.
pub(super) fn refused(r: Result<impl std::fmt::Debug>) -> String {
    match r {
        Err(Error::Query(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Every field, section, list and kind label of a form is set (the
/// extractor and the UI need them), and the keys of `values` are fields.
pub(super) fn well_formed(f: &ObjectForm) {
    let mut keys = Vec::new();
    for s in &f.sections {
        assert!(!s.label.is_empty() && !s.key.is_empty());
        for fl in &s.fields {
            assert!(!fl.label.is_empty(), "{}", fl.key);
            keys.push(fl.key.clone());
        }
        for l in &s.lists {
            assert!(!l.label.is_empty());
            assert!(l.fields.iter().any(|x| x.key == l.title_field), "{}", l.key);
        }
    }
    for k in f.values.keys() {
        assert!(keys.contains(k), "value without a field: {k}");
    }
    // A `when` names a field of the form.
    for s in &f.sections {
        for fl in &s.fields {
            if let Some((other, _)) = &fl.when {
                assert!(keys.contains(other), "{} when {other}", fl.key);
            }
        }
    }
}

/// The value of a form field.
pub(super) fn val(f: &ObjectForm, k: &str) -> Value {
    f.values.get(k).cloned().unwrap_or(Value::Null)
}

/// What the designer sends back for form `f`: its values and its lists'
/// rows, with `over` on top (a key `list:<key>` replaces that list).
pub(super) fn edit(f: &ObjectForm, over: Value) -> FormValues {
    let mut values = f.values.clone();
    let mut lists: BTreeMap<String, Vec<BTreeMap<String, Value>>> = f
        .sections
        .iter()
        .flat_map(|s| s.lists.iter())
        .map(|l| (l.key.clone(), l.rows.clone()))
        .collect();
    for (k, v) in over.as_object().expect("an object") {
        match k.strip_prefix("list:") {
            Some(l) => {
                lists.insert(l.to_string(), serde_json::from_value(v.clone()).unwrap());
            }
            None => {
                values.insert(k.clone(), v.clone());
            }
        }
    }
    FormValues { values, lists }
}
