//! The editor's command completion (#97): the etcdctl commands DBine's
//! editor runs (see [`crate::command`]), from a static catalog filtered by
//! the server's version. etcd has no catalog of its own to read.

use dbine_driver::functions::{parse_catalog, FunctionInfo};

const CATALOG: &str = include_str!("functions/etcd.txt");

/// The commands for an etcd `version` (all of them when unknown):
/// `lease|grant` is shown as `lease grant`.
pub fn catalog(version: Option<&str>) -> Vec<FunctionInfo> {
    parse_catalog(CATALOG, version)
        .into_iter()
        .map(|mut f| {
            f.name = f.name.replace('|', " ");
            f
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command;
    use dbine_driver::FunctionKind;

    #[test]
    fn the_catalog_parses_line_by_line() {
        for line in CATALOG.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
    }

    #[test]
    fn every_command_is_one_the_editor_runs() {
        let all = catalog(None);
        assert!(all.iter().all(|f| f.kind == FunctionKind::Command));
        assert!(all.iter().all(|f| f.description.is_empty()));
        for f in &all {
            // The editor's own reading of the line names the same command.
            let c = command::parse_script(&format!("{} x", f.name)).unwrap();
            assert_eq!(c[0].name(), f.name, "{}", f.name);
        }
        let get = all.iter().find(|f| f.name == "get").unwrap();
        assert!(get.args.starts_with("<key> [<range_end>] [--prefix]"));
        assert!(all.iter().any(|f| f.name == "lease grant"));
        assert!(!all
            .iter()
            .any(|f| f.name == "watch" || f.name == "lease keep-alive"));
    }

    #[test]
    fn auth_status_needs_etcd_3_5() {
        let has = |v| catalog(Some(v)).iter().any(|f| f.name == "auth status");
        assert!(has("etcd 3.5.17"));
        assert!(!has("etcd 3.4.30"));
        assert!(catalog(Some("etcd 3.4.30"))
            .iter()
            .any(|f| f.name == "auth enable"));
    }
}
