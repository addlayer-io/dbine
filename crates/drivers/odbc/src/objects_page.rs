//! The explorer's pages (`Session::list_objects_page`) where ODBC's
//! catalog functions can narrow them: a schema's tables and views, read
//! with SQLTables for that schema alone (an escaped search pattern), or, on
//! engines without schemas, the tables and views.
//!
//! ODBC's catalog functions have no limit, offset or case-insensitive
//! match, and answer in their own order (type, catalog, schema, name): that
//! folder's page is cut here out of its rows (`objects::page_of`, which also
//! drops what a pattern matched beyond the schema asked for). The rest
//! answers `Unsupported` and the app pages its whole list:
//!
//! - routines (SQLProcedures, read for every schema), and sequences,
//!   synonyms and types (each engine's catalog query, without filters);
//! - a search or a lookup over every schema, or the objects without one, on
//!   engines with schemas: SQLTables can't match a name ignoring case, so it
//!   would read every schema's tables.

use crate::presets::Preset;
use crate::{col, connstr, OdbcSession, Rows};
use dbine_driver::objects::page_of;
use dbine_driver::{kinds, DbObject, Error, ObjectPage, ObjectQuery, Result};

/// Every kind `list_objects` returns.
const ALL: &[&str] = &[
    kinds::TABLE,
    kinds::VIEW,
    kinds::PROCEDURE,
    kinds::FUNCTION,
    kinds::SEQUENCE,
    kinds::SYNONYM,
    kinds::TYPE,
];

/// What SQLTables reads for a page.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    /// No object can match.
    Nothing,
    /// The tables and views of this schema (`None`: of the engine, which
    /// has no schemas).
    Tables(Option<String>),
}

/// Whether `q` keeps objects of `kind`.
fn wants(q: &ObjectQuery, kind: &str) -> bool {
    q.kind.as_deref().is_none_or(|k| k == kind) && !q.not_kinds.iter().any(|k| k == kind)
}

/// How SQLTables answers `q` on `preset`'s engine, or `None` when it can't
/// narrow it.
pub(crate) fn plan(preset: &Preset, q: &ObjectQuery) -> Option<Plan> {
    let kinds: Vec<&str> = ALL.iter().copied().filter(|k| wants(q, k)).collect();
    if kinds.is_empty() {
        return Some(Plan::Nothing);
    }
    if kinds
        .iter()
        .any(|k| *k != kinds::TABLE && *k != kinds::VIEW)
    {
        return None;
    }
    if !preset.has_schemas {
        // Nothing has a schema.
        return Some(match &q.schema {
            Some(_) if !q.no_schema => Plan::Nothing,
            _ => Plan::Tables(None),
        });
    }
    match q.schema.as_deref() {
        _ if q.no_schema => None,
        None => None,
        // `list_objects` leaves out system schemas, and an empty one is none.
        Some(s) if s.is_empty() || preset.is_system_schema(s) => Some(Plan::Nothing),
        Some(s) => Some(Plan::Tables(Some(s.to_string()))),
    }
}

/// The tables and views of SQLTables' `rows`, as `list_objects` reads
/// them.
pub(crate) fn objects(preset: &Preset, rows: &Rows) -> Vec<DbObject> {
    rows.iter()
        .filter_map(|r| {
            let schema = col(r, 1);
            if schema
                .as_deref()
                .is_some_and(|s| preset.is_system_schema(s))
            {
                return None;
            }
            let kind = match col(r, 3).unwrap_or_default().to_ascii_uppercase().as_str() {
                "TABLE" | "BASE TABLE" => kinds::TABLE,
                "VIEW" => kinds::VIEW,
                _ => return None,
            };
            Some(DbObject {
                kind: kind.into(),
                schema: if preset.has_schemas {
                    schema.filter(|s| !s.is_empty())
                } else {
                    None
                },
                name: col(r, 2)?,
                parent: None,
            })
        })
        .collect()
}

impl OdbcSession {
    /// [`dbine_driver::Session::list_objects_page`]: see the module.
    pub(crate) async fn objects_page(&self, q: &ObjectQuery) -> Result<ObjectPage> {
        let schema = match plan(self.preset, q) {
            None => {
                return Err(Error::Unsupported(
                    "este motor todavía no lista sus objetos por páginas".into(),
                ))
            }
            Some(Plan::Nothing) => return Ok(ObjectPage::default()),
            Some(Plan::Tables(None)) => "%".to_string(),
            Some(Plan::Tables(Some(s))) => connstr::escape_pattern(&s, &self.escape),
        };
        let rows = self
            .run(move |c, slot| {
                let st = c.stmt(slot)?;
                st.tables(None, Some(&schema), Some("%"), None)?;
                st.text_rows()
            })
            .await?;
        Ok(page_of(&objects(self.preset, &rows), q))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(id: &str) -> &'static Preset {
        crate::presets::PRESETS.iter().find(|p| p.id == id).unwrap()
    }

    fn folder(schema: Option<&str>, kind: &str) -> ObjectQuery {
        ObjectQuery {
            schema: schema.map(Into::into),
            kind: Some(kind.into()),
            ..Default::default()
        }
    }

    #[test]
    fn a_schemas_tables_and_views_are_read_for_it_alone() {
        let db2 = preset("db2");
        assert_eq!(
            plan(db2, &folder(Some("APP"), "table")),
            Some(Plan::Tables(Some("APP".into())))
        );
        let q = ObjectQuery {
            schema: Some("APP".into()),
            not_kinds: ["procedure", "function", "sequence", "synonym", "type"]
                .map(String::from)
                .to_vec(),
            pattern: Some("ord".into()),
            ..Default::default()
        };
        assert_eq!(plan(db2, &q), Some(Plan::Tables(Some("APP".into()))));
        // System schemas are left out, as `list_objects` does.
        assert_eq!(
            plan(db2, &folder(Some("SYSIBM"), "view")),
            Some(Plan::Nothing)
        );
        assert_eq!(plan(db2, &folder(Some(""), "view")), Some(Plan::Nothing));
    }

    #[test]
    fn what_sqltables_cant_narrow_is_left_to_the_app() {
        let db2 = preset("db2");
        // Routines, sequences, synonyms, types.
        for k in ["procedure", "function", "sequence", "synonym", "type"] {
            assert_eq!(plan(db2, &folder(Some("APP"), k)), None, "{k}");
        }
        // Every kind of a schema, "Otros", every schema, no schema.
        assert_eq!(
            plan(
                db2,
                &ObjectQuery {
                    schema: Some("APP".into()),
                    ..Default::default()
                }
            ),
            None
        );
        assert_eq!(
            plan(
                db2,
                &ObjectQuery {
                    not_kinds: vec!["table".into(), "view".into()],
                    ..Default::default()
                }
            ),
            None
        );
        assert_eq!(plan(db2, &folder(None, "table")), None);
        let q = ObjectQuery {
            no_schema: true,
            ..folder(Some("APP"), "table")
        };
        assert_eq!(plan(db2, &q), None);
        // No kind left.
        let q = ObjectQuery {
            not_kinds: ALL.iter().map(|k| k.to_string()).collect(),
            ..Default::default()
        };
        assert_eq!(plan(db2, &q), Some(Plan::Nothing));
    }

    #[test]
    fn engines_without_schemas_read_their_tables() {
        let p = crate::presets::PRESETS
            .iter()
            .find(|p| !p.has_schemas)
            .expect("a preset without schemas");
        assert_eq!(plan(p, &folder(None, "table")), Some(Plan::Tables(None)));
        let q = ObjectQuery {
            no_schema: true,
            ..folder(None, "view")
        };
        assert_eq!(plan(p, &q), Some(Plan::Tables(None)));
        assert_eq!(plan(p, &folder(Some("x"), "table")), Some(Plan::Nothing));
    }

    #[test]
    fn rows_read_as_list_objects_reads_them() {
        let db2 = preset("db2");
        let row = |s: Option<&str>, n: &str, t: &str| -> Vec<Option<String>> {
            vec![
                Some("TESTDB".into()),
                s.map(Into::into),
                Some(n.into()),
                Some(t.into()),
            ]
        };
        let rows = vec![
            row(Some("APP"), "T", "TABLE"),
            row(Some("APP"), "V", "VIEW"),
            row(Some("APP"), "A", "ALIAS"),
            row(Some("SYSIBM"), "S", "SYSTEM TABLE"),
            row(Some("SYSCAT"), "X", "VIEW"),
            row(Some(""), "N", "base table"),
        ];
        let got: Vec<(String, Option<String>, String)> = objects(db2, &rows)
            .into_iter()
            .map(|o| (o.kind, o.schema, o.name))
            .collect();
        assert_eq!(
            got,
            [
                ("table".into(), Some("APP".into()), "T".into()),
                ("view".into(), Some("APP".into()), "V".into()),
                ("table".into(), None, "N".into()),
            ]
        );
    }
}
