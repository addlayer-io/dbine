//! Schema conversion between database engines.
//!
//! Reads [`dbine_driver::TableSchema`]s the way one engine reports them and
//! produces the equivalent tables in another engine's terms, with a report
//! of everything that changed on the way: types that got wider or lost
//! precision, defaults without an equivalent, foreign keys the target
//! can't hold, identifiers that had to be shortened.
//!
//! The steps:
//!
//! 1. [`parse`] splits a native type (`numeric(10, 2)`, `int unsigned`,
//!    `timestamp(3) with time zone`, `Nullable(String)`) into its parts.
//! 2. The source [`dialect::Dialect`] classifies it as a [`LogicalType`].
//! 3. The target dialect renders the logical type in its own names and says
//!    what didn't carry over.
//! 4. Defaults, auto-increment, keys, indexes and names go through the
//!    target's [`dialect::Caps`].
//!
//! The result feeds the target driver's `table_ddl`, which writes the DDL.

pub mod compare;
pub mod convert;
pub mod default;
pub mod dialect;
pub mod ident;
pub mod issue;
pub mod logical;
pub mod parse;

pub use convert::{convert, ColumnMapping, Conversion, Error, Options};
pub use issue::{Issue, IssueCode, Severity};
pub use logical::LogicalType;
