//! What the engine's catalog already knows about objects, for documenting
//! a database ([`crate::Session::row_estimates`],
//! [`crate::Session::object_comments`]).
//!
//! Row counts come only from statistics the engine keeps (planner
//! statistics, partition stats, collection metadata): never a `COUNT(*)`,
//! which scans the table, takes locks on some engines and loads the server.

use crate::ObjectRef;
use serde::{Deserialize, Serialize};

/// A table's (or collection's) approximate row count.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowEstimate {
    pub object: ObjectRef,
    pub rows: u64,
}

/// A comment the engine keeps on an object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectComment {
    pub object: ObjectRef,
    pub comment: String,
}
