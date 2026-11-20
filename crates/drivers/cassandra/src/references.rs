//! What an object uses (`Session::references`), from `system_schema`.
//!
//! - A materialized view: its base table (`system_schema.views`
//!   `base_table_name`).
//! - A table: the user-defined types its columns use
//!   (`system_schema.columns` `type`, `frozen<address>`,
//!   `list<frozen<persona>>`…); CQL has no foreign keys.
//! - A type: the types its fields use (`system_schema.types`).
//! - A function: the types of its arguments and result
//!   (`system_schema.functions`); an aggregate (`system_schema.aggregates`):
//!   its state function (`SFUNC`), its final function (`FINALFUNC`) and
//!   the types it uses.
//!
//! Every row is `Confirmed`: the catalog records it. What the generic scan
//! finds besides is added (never less than it); when `system_schema` can't
//! be read the generic scan answers alone and the note says so. Amazon
//! Keyspaces has tables and types only.

use crate::{text, texts, CassandraSession, TYPE};
use dbine_driver::dependencies::references_scan;
use dbine_driver::{
    kinds, Confidence, DependencyReport, DependencyScan, DependencyTarget, Dependent, Relation,
    Result,
};

pub(crate) const VIEW_BASE: &str =
    "SELECT base_table_name FROM system_schema.views WHERE keyspace_name = ? AND view_name = ?";
pub(crate) const COLUMN_TYPES: &str =
    "SELECT column_name, type FROM system_schema.columns WHERE keyspace_name = ? AND table_name = ?";
pub(crate) const FIELD_TYPES: &str =
    "SELECT field_names, field_types FROM system_schema.types WHERE keyspace_name = ? AND type_name = ?";
pub(crate) const TYPE_NAMES: &str =
    "SELECT type_name FROM system_schema.types WHERE keyspace_name = ?";
pub(crate) const FUNCTION_TYPES: &str = "SELECT argument_types, return_type FROM system_schema.functions WHERE keyspace_name = ? AND function_name = ?";
pub(crate) const AGGREGATE: &str = "SELECT state_func, final_func, state_type, return_type, argument_types FROM system_schema.aggregates WHERE keyspace_name = ? AND aggregate_name = ?";

/// An aggregate's kind, should the explorer ever list them apart.
pub(crate) const AGGREGATE_KIND: &str = "aggregate";

/// What the report says when `system_schema` can't be read.
pub(crate) const NO_SCHEMA: &str = "No se pudo leer system_schema: se muestra solo lo que encuentra la búsqueda en el código del objeto.";

/// CQL's own types: never a user-defined one.
const NATIVE: &[&str] = &[
    "ascii",
    "bigint",
    "blob",
    "boolean",
    "counter",
    "date",
    "decimal",
    "double",
    "duration",
    "float",
    "inet",
    "int",
    "smallint",
    "text",
    "time",
    "timestamp",
    "timeuuid",
    "tinyint",
    "uuid",
    "varchar",
    "varint",
    "list",
    "set",
    "map",
    "tuple",
    "frozen",
    "vector",
    "empty",
];

/// The user-defined types a CQL type names (`map<text, frozen<"Dir">>` →
/// `Dir`), a keyspace qualifier dropped.
pub(crate) fn type_names(t: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut flush = |cur: &mut String, quoted_name: bool| {
        let name = std::mem::take(cur);
        let name = match name.rsplit_once('.') {
            Some((_, n)) if !quoted_name => n.to_string(),
            _ => name,
        };
        let native = !quoted_name && NATIVE.contains(&name.to_ascii_lowercase().as_str());
        let number = name.chars().all(|c| c.is_ascii_digit());
        if !name.is_empty() && !native && !number && !out.contains(&name) {
            out.push(name);
        }
    };
    for ch in t.chars() {
        if quoted {
            if ch == '"' {
                quoted = false;
                flush(&mut cur, true);
            } else {
                cur.push(ch);
            }
            continue;
        }
        match ch {
            '"' => {
                // `ks."Dir"`: the qualifier goes.
                cur.clear();
                quoted = true;
            }
            c if c.is_alphanumeric() || c == '_' || c == '.' => cur.push(c),
            _ => flush(&mut cur, false),
        }
    }
    flush(&mut cur, false);
    out
}

/// The keyspace's types among `names`, as the catalog spells them.
fn known(names: &[String], udts: &[String]) -> Vec<String> {
    names
        .iter()
        .filter_map(|n| {
            udts.iter()
                .find(|u| *u == n)
                .or_else(|| udts.iter().find(|u| u.eq_ignore_ascii_case(n)))
                .cloned()
        })
        .collect()
}

fn dep(kind: &str, name: &str, detail: Option<String>) -> Dependent {
    Dependent {
        kind: kind.into(),
        schema: None,
        name: name.into(),
        parent: None,
        relation: Relation::Code,
        confidence: Confidence::Confirmed,
        detail,
        mentions: Vec::new(),
    }
}

/// The types `(holder, type)` pairs use (a table's columns, a type's
/// fields), one item per type with the holders that use it.
pub(crate) fn types_used(
    pairs: &[(String, String)],
    udts: &[String],
    own: Option<&str>,
) -> Vec<Dependent> {
    let mut by: Vec<(String, Vec<String>)> = Vec::new();
    for (holder, typ) in pairs {
        for u in known(&type_names(typ), udts) {
            if own.is_some_and(|o| o == u) {
                continue;
            }
            match by.iter_mut().find(|(n, _)| *n == u) {
                Some((_, hs)) => {
                    if !hs.contains(holder) {
                        hs.push(holder.clone());
                    }
                }
                None => by.push((u, vec![holder.clone()])),
            }
        }
    }
    by.into_iter()
        .map(|(u, hs)| {
            let detail = hs
                .into_iter()
                .filter(|h| !h.is_empty())
                .collect::<Vec<_>>()
                .join(", ");
            dep(TYPE, &u, (!detail.is_empty()).then_some(detail))
        })
        .collect()
}

/// An aggregate's row ([`AGGREGATE`]): its functions, then its types.
pub(crate) fn aggregate_uses(
    state_func: &str,
    final_func: &str,
    types: &[String],
    udts: &[String],
) -> Vec<Dependent> {
    let mut out = Vec::new();
    for (f, role) in [(state_func, "SFUNC"), (final_func, "FINALFUNC")] {
        if !f.is_empty() {
            out.push(dep(kinds::FUNCTION, f, Some(role.into())));
        }
    }
    let pairs: Vec<(String, String)> = types.iter().map(|t| (String::new(), t.clone())).collect();
    out.extend(types_used(&pairs, udts, None));
    out
}

/// `extra`'s items that `items` doesn't have yet.
pub(crate) fn add_missing(items: &mut Vec<Dependent>, extra: Vec<Dependent>) {
    for d in extra {
        if !items
            .iter()
            .any(|x| x.kind == d.kind && x.name.eq_ignore_ascii_case(&d.name))
        {
            items.push(d);
        }
    }
}

impl CassandraSession {
    pub(crate) async fn references_report(
        &mut self,
        target: &DependencyTarget,
        scan: &DependencyScan,
    ) -> Result<DependencyReport> {
        let kind = target.object.kind.as_str();
        if !matches!(
            kind,
            kinds::TABLE | kinds::MATERIALIZED_VIEW | TYPE | kinds::FUNCTION | AGGREGATE_KIND
        ) {
            return references_scan(self, target, scan).await;
        }
        let catalog = self.catalog_uses(target).await;
        let mut r = references_scan(self, target, scan).await?;
        match catalog {
            Ok(mut items) => {
                add_missing(&mut items, std::mem::take(&mut r.items));
                r.items = items;
            }
            Err(_) => r.note = Some(NO_SCHEMA.into()),
        }
        Ok(r)
    }

    /// The keyspace's user-defined types.
    async fn type_names(&self, ks: &str) -> Result<Vec<String>> {
        Ok(self
            .rows(TYPE_NAMES, (ks,))
            .await?
            .iter()
            .map(|r| text(r, 0))
            .collect())
    }

    async fn catalog_uses(&self, target: &DependencyTarget) -> Result<Vec<Dependent>> {
        let t = &target.object;
        let ks = self.ks(t)?;
        let name = t.name.as_str();
        Ok(match t.kind.as_str() {
            kinds::MATERIALIZED_VIEW => self
                .rows(VIEW_BASE, (&ks, name))
                .await?
                .first()
                .map(|r| text(r, 0))
                .filter(|b| !b.is_empty())
                .map(|b| vec![dep(kinds::TABLE, &b, None)])
                .unwrap_or_default(),
            kinds::TABLE => {
                let pairs: Vec<(String, String)> = self
                    .rows(COLUMN_TYPES, (&ks, name))
                    .await?
                    .iter()
                    .map(|r| (text(r, 0), text(r, 1)))
                    .filter(|(c, _)| target.column.as_deref().is_none_or(|x| x == c))
                    .collect();
                if pairs.iter().all(|(_, typ)| type_names(typ).is_empty()) {
                    return Ok(Vec::new());
                }
                types_used(&pairs, &self.type_names(&ks).await?, None)
            }
            TYPE => {
                let rows = self.rows(FIELD_TYPES, (&ks, name)).await?;
                let pairs: Vec<(String, String)> = rows
                    .first()
                    .map(|r| texts(r, 0).into_iter().zip(texts(r, 1)).collect())
                    .unwrap_or_default();
                types_used(&pairs, &self.type_names(&ks).await?, Some(name))
            }
            _ => {
                let mut out = Vec::new();
                let udts = self.type_names(&ks).await?;
                for r in self.rows(FUNCTION_TYPES, (&ks, name)).await? {
                    let mut types = texts(&r, 0);
                    types.push(text(&r, 1));
                    let pairs: Vec<(String, String)> =
                        types.into_iter().map(|t| (String::new(), t)).collect();
                    add_missing(&mut out, types_used(&pairs, &udts, None));
                }
                for r in self.rows(AGGREGATE, (&ks, name)).await? {
                    let mut types = texts(&r, 4);
                    types.push(text(&r, 2));
                    types.push(text(&r, 3));
                    add_missing(
                        &mut out,
                        aggregate_uses(&text(&r, 0), &text(&r, 1), &types, &udts),
                    );
                }
                out
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn the_catalog_queries() {
        assert_eq!(
            VIEW_BASE,
            "SELECT base_table_name FROM system_schema.views WHERE keyspace_name = ? AND view_name = ?"
        );
        assert!(AGGREGATE.starts_with("SELECT state_func, final_func, state_type, return_type, argument_types FROM system_schema.aggregates"));
        assert!(COLUMN_TYPES.contains("system_schema.columns"));
    }

    #[test]
    fn user_types_inside_cql_types() {
        assert_eq!(type_names("frozen<address>"), ["address"]);
        assert_eq!(
            type_names("map<text, frozen<list<frozen<\"Persona\">>>>"),
            ["Persona"]
        );
        assert_eq!(type_names("ks.dir"), ["dir"]);
        assert_eq!(type_names("ks.\"Dir.x\""), ["Dir.x"]);
        assert_eq!(type_names("vector<float, 3>"), Vec::<String>::new());
        assert_eq!(type_names("tuple<int, text>"), Vec::<String>::new());
        assert_eq!(type_names("frozen<\"text\">"), ["text"]);
        assert!(type_names("").is_empty() && type_names("<<,>").is_empty());
    }

    #[test]
    fn a_tables_types_and_an_aggregates_functions() {
        let udts = s(&["address", "persona", "Telefono"]);
        let pairs = vec![
            ("id".to_string(), "int".to_string()),
            ("casa".to_string(), "frozen<address>".to_string()),
            ("trabajo".to_string(), "frozen<address>".to_string()),
            ("familia".to_string(), "list<frozen<persona>>".to_string()),
            ("tel".to_string(), "frozen<telefono>".to_string()),
            ("otro".to_string(), "frozen<borrado>".to_string()),
        ];
        let got: Vec<(String, String, Option<String>)> = types_used(&pairs, &udts, None)
            .into_iter()
            .map(|d| (d.kind, d.name, d.detail))
            .collect();
        assert_eq!(
            got,
            [
                (TYPE.into(), "address".into(), Some("casa, trabajo".into())),
                (TYPE.into(), "persona".into(), Some("familia".into())),
                (TYPE.into(), "Telefono".into(), Some("tel".into())),
            ]
        );
        // A type doesn't use itself.
        let own = vec![("next".to_string(), "frozen<persona>".to_string())];
        assert!(types_used(&own, &udts, Some("persona")).is_empty());
        let got: Vec<(String, String, Option<String>, Confidence)> = aggregate_uses(
            "acumular",
            "promedio",
            &s(&["int", "frozen<persona>", "tuple<int, bigint>", "double"]),
            &udts,
        )
        .into_iter()
        .map(|d| (d.kind, d.name, d.detail, d.confidence))
        .collect();
        assert_eq!(
            got,
            [
                (
                    kinds::FUNCTION.into(),
                    "acumular".into(),
                    Some("SFUNC".into()),
                    Confidence::Confirmed
                ),
                (
                    kinds::FUNCTION.into(),
                    "promedio".into(),
                    Some("FINALFUNC".into()),
                    Confidence::Confirmed
                ),
                (TYPE.into(), "persona".into(), None, Confidence::Confirmed),
            ]
        );
        // No final function.
        assert_eq!(aggregate_uses("f", "", &[], &udts).len(), 1);
        // The generic scan's extras, never a duplicate.
        let mut items = vec![dep(kinds::TABLE, "base", None)];
        add_missing(
            &mut items,
            vec![
                dep(kinds::TABLE, "Base", None),
                dep(kinds::TABLE, "otra", None),
            ],
        );
        assert_eq!(items.len(), 2);
    }
}
