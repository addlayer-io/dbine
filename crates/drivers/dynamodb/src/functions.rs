//! The editor's completion (#97) for DynamoDB's PartiQL: its few
//! functions, from a static catalog (the service doesn't list them, and
//! has no versions).

use dbine_driver::functions::{parse_catalog, FunctionInfo};

const PARTIQL: &str = include_str!("functions/partiql.txt");

pub fn catalog() -> Vec<FunctionInfo> {
    parse_catalog(PARTIQL, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::FunctionKind;

    #[test]
    fn the_catalog_parses_line_by_line() {
        for line in PARTIQL.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
        let all = catalog();
        assert_eq!(all.len(), 5);
        let bw = all.iter().find(|f| f.name == "BEGINS_WITH").unwrap();
        assert_eq!(bw.args, "(path, value)");
        assert_eq!(bw.returns, "boolean");
        assert!(all.iter().all(|f| f.kind == FunctionKind::Scalar));
    }
}
