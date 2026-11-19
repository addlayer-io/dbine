//! The driver's insert, update and delete scripts knowing the target
//! columns' types (`Driver::insert_script_typed` and its kin): a value is
//! written as its column takes it, a vector as its column's element and
//! storage form (pgvector's `halfvec`, libSQL's `F8_BLOB`, a MongoDB BinData
//! vector…).
//!
//! The types come from a result (its [`ResultColumn`]s, with `type_name`
//! and `vector`) or from a describe ([`described`]). A path with no types
//! (an older UI, a target that doesn't describe its columns) calls the
//! untyped methods, as before, so drivers keep their own lookups for it.

use crate::commands::vectors;
use dbine_driver::{Driver, ObjectRef, ResultColumn, RowChange};
use serde_json::Value;

/// A described column as a script's type: its native type, and its vector
/// when that type is one on `driver_id`'s engine.
pub(crate) fn described(driver_id: &str, name: &str, data_type: &str) -> ResultColumn {
    let c = ResultColumn::new(name, data_type);
    match vectors::of_type(driver_id, data_type) {
        Some(m) => c.with_vector(m),
        None => c,
    }
}

/// `columns`' types (name, native type) as a script's types on
/// `driver_id`'s engine.
pub(crate) fn of_describe<'a>(
    driver_id: &str,
    columns: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<ResultColumn> {
    columns
        .into_iter()
        .map(|(n, t)| described(driver_id, n, t))
        .collect()
}

/// The type of each of `names` in `types` (by name: as written first, then
/// without case), named as `names` says; a name `types` lacks is untyped.
pub(crate) fn for_names(names: &[String], types: &[ResultColumn]) -> Vec<ResultColumn> {
    names
        .iter()
        .map(|n| {
            types
                .iter()
                .find(|c| &c.name == n)
                .or_else(|| types.iter().find(|c| c.name.eq_ignore_ascii_case(n)))
                .map_or_else(
                    || ResultColumn::new(n.clone(), ""),
                    |c| ResultColumn {
                        name: n.clone(),
                        ..c.clone()
                    },
                )
        })
        .collect()
}

/// Whether a column's type says anything: a type name or a vector.
fn known(c: &ResultColumn) -> bool {
    !c.type_name.trim().is_empty() || c.vector.is_some()
}

/// Rows of `names` into `target`: typed by `types` (by name), or the
/// untyped script when none of them is known.
pub(crate) fn insert(
    driver: &dyn Driver,
    target: &ObjectRef,
    names: &[String],
    types: &[ResultColumn],
    rows: &[Vec<Value>],
) -> dbine_driver::Result<String> {
    insert_columns(driver, target, &for_names(names, types), rows)
}

/// Rows of `columns` (in the rows' order, each with its type) into
/// `target`; the untyped script when no type is known.
pub(crate) fn insert_columns(
    driver: &dyn Driver,
    target: &ObjectRef,
    columns: &[ResultColumn],
    rows: &[Vec<Value>],
) -> dbine_driver::Result<String> {
    if !columns.iter().any(known) {
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        return driver.insert_script(target, &names, rows);
    }
    driver.insert_script_typed(target, columns, rows)
}

/// Changes to `target`'s rows, typed by `types` when one is known.
pub(crate) fn update(
    driver: &dyn Driver,
    target: &ObjectRef,
    changes: &[RowChange],
    types: &[ResultColumn],
) -> dbine_driver::Result<String> {
    if !types.iter().any(known) {
        return driver.update_script(target, changes);
    }
    driver.update_script_typed(target, changes, types)
}

/// Rows of `target` to delete by their keys, typed by `types` when one is
/// known.
pub(crate) fn delete(
    driver: &dyn Driver,
    target: &ObjectRef,
    keys: &[Vec<(String, Value)>],
    types: &[ResultColumn],
) -> dbine_driver::Result<String> {
    if !types.iter().any(known) {
        return driver.delete_script(target, keys);
    }
    driver.delete_script_typed(target, keys, types)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use dbine_driver::{VectorElement, VectorMeta};
    use std::sync::Mutex;

    /// A driver that says which script method ran, and with what types.
    #[derive(Default)]
    pub(crate) struct Recorder {
        pub(crate) calls: Mutex<Vec<(String, Vec<ResultColumn>)>>,
    }

    impl Recorder {
        pub(crate) fn take(&self) -> Vec<(String, Vec<ResultColumn>)> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }
        fn note(&self, what: &str, types: &[ResultColumn]) {
            self.calls
                .lock()
                .unwrap()
                .push((what.to_string(), types.to_vec()));
        }
    }

    #[async_trait::async_trait]
    impl Driver for Recorder {
        fn info(&self) -> &dbine_driver::DriverInfo {
            dbine_drivers::find("postgres").unwrap().info()
        }
        async fn connect(
            &self,
            _: &dbine_driver::ConnectionConfig,
            _: Option<&str>,
        ) -> dbine_driver::Result<Box<dyn dbine_driver::Session>> {
            Err(dbine_driver::Error::Unsupported("test".into()))
        }
        fn insert_script(
            &self,
            _: &ObjectRef,
            columns: &[String],
            _: &[Vec<Value>],
        ) -> dbine_driver::Result<String> {
            let named: Vec<ResultColumn> = columns
                .iter()
                .map(|c| ResultColumn::new(c.clone(), ""))
                .collect();
            self.note("insert", &named);
            Ok("insert".into())
        }
        fn insert_script_typed(
            &self,
            _: &ObjectRef,
            columns: &[ResultColumn],
            _: &[Vec<Value>],
        ) -> dbine_driver::Result<String> {
            self.note("insert_typed", columns);
            Ok("insert".into())
        }
        fn update_script(&self, _: &ObjectRef, _: &[RowChange]) -> dbine_driver::Result<String> {
            self.note("update", &[]);
            Ok("update".into())
        }
        fn update_script_typed(
            &self,
            _: &ObjectRef,
            _: &[RowChange],
            types: &[ResultColumn],
        ) -> dbine_driver::Result<String> {
            self.note("update_typed", types);
            Ok("update".into())
        }
        fn delete_script(
            &self,
            _: &ObjectRef,
            _: &[Vec<(String, Value)>],
        ) -> dbine_driver::Result<String> {
            self.note("delete", &[]);
            Ok("delete".into())
        }
        fn delete_script_typed(
            &self,
            _: &ObjectRef,
            _: &[Vec<(String, Value)>],
            types: &[ResultColumn],
        ) -> dbine_driver::Result<String> {
            self.note("delete_typed", types);
            Ok("delete".into())
        }
    }

    pub(crate) fn table(name: &str) -> ObjectRef {
        ObjectRef {
            kind: "table".into(),
            schema: None,
            name: name.into(),
        }
    }

    /// With types, the typed methods run, each insert column with its own
    /// type (found by name, in the names' spelling); without, the untyped
    /// ones, as before.
    #[test]
    fn scripts_are_typed_when_the_types_are_known() {
        let r = Recorder::default();
        let t = table("t");
        let v = ResultColumn::new("Emb", "halfvec(3)")
            .with_vector(VectorMeta::dense(Some(3), VectorElement::F16));
        let types = vec![ResultColumn::new("id", "int4"), v.clone()];
        let names = vec!["emb".to_string(), "id".to_string(), "otra".to_string()];
        insert(&r, &t, &names, &types, &[]).unwrap();
        update(&r, &t, &[], &types).unwrap();
        delete(&r, &t, &[], &types).unwrap();
        let calls = r.take();
        assert_eq!(calls[0].0, "insert_typed");
        let cols = &calls[0].1;
        assert_eq!(cols.len(), 3);
        assert_eq!(cols[0].name, "emb");
        assert_eq!(cols[0].type_name, "halfvec(3)");
        assert_eq!(cols[0].vector, v.vector);
        assert_eq!(cols[1].type_name, "int4");
        assert_eq!(
            (cols[2].type_name.as_str(), cols[2].vector.is_none()),
            ("", true)
        );
        let names_of = |c: &[ResultColumn]| -> Vec<String> {
            c.iter()
                .map(|c| format!("{} {}", c.name, c.type_name))
                .collect()
        };
        assert_eq!(calls[1].0, "update_typed");
        assert_eq!(names_of(&calls[1].1), ["id int4", "Emb halfvec(3)"]);
        assert_eq!(calls[2].0, "delete_typed");
        assert_eq!(names_of(&calls[2].1), ["id int4", "Emb halfvec(3)"]);

        insert(&r, &t, &names, &[], &[]).unwrap();
        update(&r, &t, &[], &[]).unwrap();
        delete(&r, &t, &[], &[]).unwrap();
        // Types that say nothing (no type name, no vector) are no types.
        let blank = vec![ResultColumn::new("id", " "), ResultColumn::new("emb", "")];
        insert(&r, &t, &names, &blank, &[]).unwrap();
        update(&r, &t, &[], &blank).unwrap();
        delete(&r, &t, &[], &blank).unwrap();
        let what: Vec<String> = r.take().into_iter().map(|c| c.0).collect();
        assert_eq!(
            what,
            ["insert", "update", "delete", "insert", "update", "delete"]
        );
    }

    /// A described column carries its vector when its engine's dialect
    /// says the type is one.
    #[test]
    fn a_described_vector_column_says_it() {
        let c = described("postgres", "v", "vector(3)");
        assert_eq!(c.type_name, "vector(3)");
        assert_eq!(c.vector.as_ref().and_then(|m| m.dims), Some(3));
        let c = described("postgres", "n", "integer");
        assert!(c.vector.is_none());
        // An engine the build doesn't know: the type, untyped vector.
        assert!(described("no-such-engine", "v", "vector(3)")
            .vector
            .is_none());
    }
}
