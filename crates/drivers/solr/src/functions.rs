//! The editor's completion (#97) for Solr: its function queries (used in
//! `fl`, `sort`, boosts and `{!func}`), from a static catalog filtered by
//! the server's version. Solr doesn't list them.

use dbine_driver::functions::{parse_catalog, FunctionInfo};

const SOLR: &str = include_str!("functions/solr.txt");

pub fn catalog(version: Option<&str>) -> Vec<FunctionInfo> {
    parse_catalog(SOLR, version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::FunctionKind;

    #[test]
    fn the_catalog_parses_line_by_line() {
        for line in SOLR.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
        let all = catalog(None);
        assert!(all.iter().all(|f| f.kind == FunctionKind::Scalar));
        let recip = all.iter().find(|f| f.name == "recip").unwrap();
        assert_eq!(recip.args, "(x, m, a, b)");
        assert_eq!(
            all.iter().filter(|f| f.name == "vectorSimilarity").count(),
            2
        );
    }

    #[test]
    fn the_servers_version_filters_functions() {
        let has = |v, n: &str| catalog(Some(v)).iter().any(|f| f.name == n);
        assert!(has("Apache Solr 9.8.1", "vectorSimilarity"));
        assert!(!has("Apache Solr 9.2.1", "vectorSimilarity"));
        assert!(has("Apache Solr 9.2.1", "isnan"));
        assert!(!has("Apache Solr 8.11.4 (SolrCloud)", "isnan"));
        assert!(has("Apache Solr 8.11.4 (SolrCloud)", "recip"));
        assert!(!has("Apache Solr 9.8.1", "lateVector"));
        assert!(has("Apache Solr 10.1.0", "lateVector"));
    }
}
