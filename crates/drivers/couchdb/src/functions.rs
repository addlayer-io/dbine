//! The editor's completion (#97) for CouchDB: the Mango selector
//! operators, as `Operator` entries. CouchDB doesn't list them, so the
//! catalog is static, filtered by the server's version.

use dbine_driver::functions::{parse_catalog, FunctionInfo};

const MANGO: &str = include_str!("functions/mango.txt");

pub fn catalog(version: Option<&str>) -> Vec<FunctionInfo> {
    parse_catalog(MANGO, version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::FunctionKind;

    #[test]
    fn the_catalog_parses_line_by_line() {
        for line in MANGO.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
        let all = catalog(None);
        assert!(all
            .iter()
            .all(|f| f.kind == FunctionKind::Operator && f.name.starts_with('$')));
    }

    #[test]
    fn the_servers_version_filters_operators() {
        let has = |v, n: &str| catalog(Some(v)).iter().any(|f| f.name == n);
        assert!(has("3.5.2", "$beginsWith"));
        assert!(!has("3.3.3", "$beginsWith"));
        // $keyMapMatch is 3.2's (#3254).
        assert!(has("3.2.0", "$keyMapMatch"));
        assert!(!has("3.1.2", "$keyMapMatch"));
        assert!(!has("2.3.1", "$keyMapMatch"));
        assert!(has("2.3.1", "$allMatch"));
        assert!(!has("2.0.0", "$allMatch"));
        assert!(has("2.0.0", "$elemMatch"));
    }
}
