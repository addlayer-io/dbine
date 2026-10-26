//! A session that answers catalog queries from a script, for the engines'
//! catalog readers: each rule matches the statements containing all of its
//! fragments and answers with rows of text (or fails); a rule with several
//! answers gives them in turn (the last one from then on), for a catalog
//! that changes as the clone is fixed. Every statement run is logged; one
//! no rule matches fails, so a reader that asks something unexpected shows
//! up.

use dbine_driver::{
    async_trait, ColumnInfo, DbObject, Error, ObjectRef, QueryOutcome, Result, ResultColumn,
    Session, StatementResult,
};

pub(super) type Rows = Vec<Vec<Option<String>>>;

enum Answer {
    Rows(Rows),
    Fail(String),
}

struct Rule {
    fragments: Vec<String>,
    answers: Vec<Answer>,
    used: usize,
}

#[derive(Default)]
pub(super) struct Scripted {
    rules: Vec<Rule>,
    log: Vec<String>,
}

/// A row of text cells (`None`: NULL).
pub(super) fn row(cells: &[Option<&str>]) -> Vec<Option<String>> {
    cells.iter().map(|c| c.map(str::to_string)).collect()
}

impl Scripted {
    pub fn new() -> Self {
        Self::default()
    }

    fn rule(mut self, fragments: &[&str], answers: Vec<Answer>) -> Self {
        self.rules.push(Rule {
            fragments: fragments.iter().map(|f| f.to_string()).collect(),
            answers,
            used: 0,
        });
        self
    }

    /// Statements containing every fragment answer `rows`.
    pub fn on(self, fragments: &[&str], rows: Rows) -> Self {
        self.rule(fragments, vec![Answer::Rows(rows)])
    }

    /// Statements containing every fragment answer each of `answers` in
    /// turn, then the last one.
    pub fn on_seq(self, fragments: &[&str], answers: Vec<Rows>) -> Self {
        self.rule(fragments, answers.into_iter().map(Answer::Rows).collect())
    }

    /// Statements containing every fragment fail with `error`.
    pub fn fail(self, fragments: &[&str], error: &str) -> Self {
        self.rule(fragments, vec![Answer::Fail(error.into())])
    }

    /// `other`'s rules after these (these win).
    pub fn chain(mut self, other: Scripted) -> Self {
        self.rules.extend(other.rules);
        self
    }

    /// The statements run, in order.
    pub fn ran(&self) -> Vec<String> {
        self.log.clone()
    }
}

#[async_trait]
impl Session for Scripted {
    async fn server_version(&mut self) -> Result<String> {
        Ok("scripted".into())
    }
    async fn list_databases(&mut self) -> Result<Vec<String>> {
        Ok(vec![])
    }
    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        Ok(vec![])
    }
    async fn columns(&mut self, _: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        Ok(vec![])
    }
    async fn definition(&mut self, _: &ObjectRef) -> Result<Option<String>> {
        Ok(None)
    }
    fn browse_query(&self, _: &ObjectRef, _: u32) -> String {
        String::new()
    }
    async fn execute(&mut self, text: &str, _: usize, out: &mut QueryOutcome) -> Result<()> {
        self.log.push(text.to_string());
        let Some(rule) = self
            .rules
            .iter_mut()
            .find(|r| r.fragments.iter().all(|f| text.contains(f.as_str())))
        else {
            return Err(Error::Query(format!("unexpected statement: {text}")));
        };
        let at = rule.used.min(rule.answers.len() - 1);
        rule.used += 1;
        match &rule.answers[at] {
            Answer::Rows(rows) => {
                let width = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
                out.results.push(StatementResult {
                    columns: (0..width)
                        .map(|i| ResultColumn {
                            name: format!("c{i}"),
                            type_name: String::new(),
                        })
                        .collect(),
                    rows: rows
                        .iter()
                        .map(|r| {
                            r.iter()
                                .map(|c| match c {
                                    Some(s) => serde_json::Value::String(s.clone()),
                                    None => serde_json::Value::Null,
                                })
                                .collect()
                        })
                        .collect(),
                    total_rows: rows.len() as u64,
                    ..Default::default()
                });
                Ok(())
            }
            Answer::Fail(e) => Err(Error::Query(e.clone())),
        }
    }
}
