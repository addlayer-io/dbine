//! The function catalog (issue #97): Cosmos DB doesn't list its system
//! functions and has no server version to filter by, so they come from a
//! static catalog. User-defined functions are left out: each belongs to one
//! container (called as `udf.<name>()`), and listing them would cost a
//! request per container.

use dbine_driver::functions::parse_catalog;
use dbine_driver::FunctionInfo;

const CATALOG: &str = include_str!("functions/cosmosdb.txt");

/// The system functions.
pub fn system() -> Vec<FunctionInfo> {
    parse_catalog(CATALOG, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::FunctionKind;

    #[test]
    fn the_static_catalog_parses() {
        for line in CATALOG.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
        let c = system();
        let count = c.iter().find(|f| f.name == "COUNT").unwrap();
        assert_eq!(count.kind, FunctionKind::Aggregate);
        let contains = c.iter().find(|f| f.name == "ARRAY_CONTAINS").unwrap();
        assert_eq!(contains.kind, FunctionKind::Scalar);
        assert_eq!(contains.args, "(array_expr, expr [, bool_expr])");
        assert!(c.iter().all(|f| f.description.is_empty()));
    }
}
