//! The editor's completion (#97) for MongoDB's language: the aggregation
//! stages and operators, query and update operators and system variables
//! (`Operator` entries, written where a `$` key or string goes), and the
//! shell methods the editor runs (`Scalar` entries, with `db`,
//! `db.collection` or `cursor` as their schema). MongoDB doesn't list its
//! operators, so the catalogs are static, filtered by the version the
//! server reports: Amazon DocumentDB has its own (what its documentation
//! says it supports, by DocumentDB version), FerretDB uses MongoDB's for
//! the MongoDB version it reports.

use crate::Flavor;
use dbine_driver::functions::{parse_catalog, FunctionInfo};

const MONGODB: &str = include_str!("functions/mongodb.txt");
const DOCUMENTDB: &str = include_str!("functions/documentdb.txt");
const SHELL: &str = include_str!("functions/shell.txt");

/// The catalog for `flavor` at `version` (buildInfo's, all entries when
/// unknown).
pub fn catalog(flavor: Flavor, version: Option<&str>) -> Vec<FunctionInfo> {
    let operators = match flavor {
        Flavor::DocumentDb => DOCUMENTDB,
        Flavor::Mongo | Flavor::Ferret => MONGODB,
    };
    let mut out = parse_catalog(operators, version);
    out.extend(parse_catalog(SHELL, version).into_iter().map(|mut f| {
        // `db.collection.find` → `find` in `db.collection`.
        if let Some((schema, name)) = f.name.rsplit_once('.') {
            f.schema = schema.to_string();
            f.name = name.to_string();
        }
        f
    }));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::FunctionKind;

    #[test]
    fn the_catalogs_parse_line_by_line() {
        for text in [MONGODB, DOCUMENTDB, SHELL] {
            let mut names = std::collections::HashSet::new();
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let f = parse_catalog(line, None);
                assert_eq!(f.len(), 1, "unreadable: {line}");
                assert!(names.insert(f[0].name.clone()), "twice: {line}");
            }
        }
    }

    #[test]
    fn operators_and_shell_methods() {
        let all = catalog(Flavor::Mongo, None);
        let find = |n: &str, schema: &str| all.iter().find(|f| f.name == n && f.schema == schema);
        let m = find("$match", "").unwrap();
        assert_eq!(m.kind, FunctionKind::Operator);
        assert_eq!(m.args, "");
        let f = find("find", "db.collection").unwrap();
        assert_eq!(f.kind, FunctionKind::Scalar);
        assert_eq!(f.args, "(filter, projection)");
        assert!(find("getCollectionNames", "db").is_some());
        assert!(find("limit", "cursor").is_some());
        assert!(all.iter().all(|f| f.description.is_empty()));
        assert!(all
            .iter()
            .filter(|f| f.kind == FunctionKind::Operator)
            .all(|f| f.name.starts_with('$') && f.schema.is_empty()));
    }

    #[test]
    fn the_servers_version_filters_operators() {
        let has = |flavor, v, n: &str| catalog(flavor, Some(v)).iter().any(|f| f.name == n);
        assert!(has(Flavor::Mongo, "7.0.14", "$dateTrunc"));
        assert!(!has(Flavor::Mongo, "4.4.29", "$dateTrunc"));
        assert!(has(Flavor::Mongo, "7.0.14", "$median"));
        assert!(!has(Flavor::Mongo, "6.0.19", "$median"));
        assert!(!has(Flavor::Mongo, "7.0.14", "$toUUID"));
        assert!(has(Flavor::Mongo, "8.0.4", "$toUUID"));
        // Score fusion and $sigmoid are 8.3's; DocumentDB has $sigmoid in
        // its 8.0.
        assert!(!has(Flavor::Mongo, "8.2.1", "$scoreFusion"));
        assert!(has(Flavor::Mongo, "8.3.0", "$scoreFusion"));
        assert!(!has(Flavor::Mongo, "8.0.4", "$sigmoid"));
        assert!(has(Flavor::Mongo, "8.3.0", "$sigmoid"));
        assert!(has(Flavor::DocumentDb, "8.0.0", "$sigmoid"));
        assert!(!has(Flavor::Mongo, "4.2.0", "hideIndex"));
        assert!(has(Flavor::Ferret, "7.0.77", "$setWindowFields"));
        // DocumentDB: what its own documentation says, by its version.
        assert!(!has(Flavor::DocumentDb, "5.0.0", "$dateTrunc"));
        assert!(has(Flavor::DocumentDb, "8.0.0", "$dateTrunc"));
        assert!(has(Flavor::DocumentDb, "4.0.0", "$lookup"));
        assert!(!has(Flavor::DocumentDb, "8.0.0", "$where"));
        assert!(!has(Flavor::DocumentDb, "8.0.0", "$function"));
        assert!(has(Flavor::Mongo, "8.0.0", "$where"));
    }
}
