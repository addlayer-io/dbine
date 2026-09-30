//! What the conversion changed, lost or couldn't do, per table and column,
//! for the review step: nothing is dropped or narrowed silently.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// A faithful change worth knowing (`serial` → `IDENTITY`).
    Info,
    /// Same values, different behavior (collation, time zone handling,
    /// an index kind the target doesn't have).
    Warning,
    /// Values may not fit or lose detail (precision, range, unicode).
    Loss,
    /// Left out: the target can't express it (foreign keys in ClickHouse,
    /// a check the target lacks). The user decides what to do.
    Dropped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueCode {
    /// The table becomes another kind of object (a node label, documents
    /// in a database without tables).
    TableChanged,
    TypeChanged,
    TypeApproximated,
    TypeUnknown,
    PrecisionLoss,
    RangeLoss,
    LengthLoss,
    UnicodeLoss,
    TimeZoneLoss,
    DefaultDropped,
    DefaultRewritten,
    AutoIncrementChanged,
    AutoIncrementDropped,
    NullabilityChanged,
    PrimaryKeyAdded,
    PrimaryKeyDropped,
    ForeignKeyDropped,
    ForeignKeyActionChanged,
    IndexDropped,
    IndexChanged,
    CheckDropped,
    IdentifierRenamed,
    CommentDropped,
    OptionDropped,
    OptionAdded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Issue {
    pub severity: Severity,
    pub code: IssueCode,
    /// Table as named in the source.
    pub table: String,
    /// Column, index or key the issue is about, if any.
    pub object: Option<String>,
    /// Spanish, for the user.
    pub message: String,
}

/// Collects issues while converting one table.
#[derive(Debug, Default)]
pub struct Report {
    pub issues: Vec<Issue>,
}

impl Report {
    pub fn push(&mut self, severity: Severity, code: IssueCode, table: &str, object: Option<&str>, message: impl Into<String>) {
        self.issues.push(Issue {
            severity,
            code,
            table: table.to_string(),
            object: object.map(str::to_string),
            message: message.into(),
        });
    }

    pub fn worst(&self) -> Option<Severity> {
        self.issues.iter().map(|i| i.severity).max()
    }
}
