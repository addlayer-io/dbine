//! DAX text: tokens, units, quoting, the queries the driver writes. Owned
//! by part B; part C uses [`lexer`] (dependencies, search) and [`quote`]
//! (definitions, names).

pub mod aggregate;
pub mod browse;
pub mod lexer;
pub mod quote;
pub mod split;
