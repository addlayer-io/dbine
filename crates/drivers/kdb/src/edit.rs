//! q text DBine writes: browse queries with the grid's filters and sort,
//! the scripts of edited, inserted and deleted rows, a table's definition
//! and the designer's.
//!
//! Values from the grid arrive as JSON without q's types, so every value
//! is written as a string and cast on the server to the column's type
//! (`"J"$"42"`, `"S"$"AAPL"`, `"D"$"2024.01.31"`): a column of strings
//! keeps the string. Names that aren't plain q identifiers are reached
//! through a symbol (`` `$"my table" ``).

use crate::cells;
use dbine_driver::{
    kinds, ColumnFilter, CreateTemplate, DdlParts, DesignerSpec, Error, FilterOp, ParamValue,
    Result, RowChange, SortKey, TableSchema,
};
use serde_json::Value;

/// `s` as a q string literal.
pub fn qstr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\{:03o}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A plain q name: a letter (or a dotted namespace path), then letters,
/// digits and `_`.
pub fn plain(name: &str) -> bool {
    !name.is_empty()
        && name.split('.').enumerate().all(|(i, part)| {
            if part.is_empty() {
                // Only a leading dot (`.ns.t`).
                return i == 0 && name.len() > 1;
            }
            let mut cs = part.chars();
            cs.next().is_some_and(|c| c.is_ascii_alphabetic())
                && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        && !name.ends_with('.')
}

/// `name` as a symbol: `` `trade `` or `` (`$"my table") ``.
pub fn qsym(name: &str) -> String {
    if plain(name) {
        format!("`{name}")
    } else {
        format!("(`$ {})", qstr(name))
    }
}

/// `name` as an expression that gives its value: the name itself, or
/// `` (get `$"…") ``.
pub fn qref(name: &str) -> String {
    if plain(name) {
        name.to_string()
    } else {
        format!("(get `$ {})", qstr(name))
    }
}

/// A grid value as the text q casts: ISO dates and times in q's form
/// (`2024.01.31`, `2024.01.31D13:45:00`), booleans as `1`/`0`, null as an
/// empty string (q's null of the column's type).
pub fn value_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => if *b { "1" } else { "0" }.into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => iso_to_q(s),
        other => other.to_string(),
    }
}

/// `2024-01-31` → `2024.01.31`; `2024-01-31 13:45:00.5` →
/// `2024.01.31D13:45:00.5`; `2024-01` → `2024.01`; the rest as is.
fn iso_to_q(s: &str) -> String {
    let b = s.as_bytes();
    let date = b.len() >= 10
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit);
    if date {
        let d = s[..10].replace('-', ".");
        let rest = &s[10..];
        return match rest.chars().next() {
            None => d,
            Some(' ' | 'T') => format!("{d}D{}", &rest[1..]),
            _ => s.to_string(),
        };
    }
    if b.len() == 7
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..].iter().all(u8::is_ascii_digit)
    {
        return s.replace('-', ".");
    }
    s.to_string()
}

// Helpers applied to a column's values (`x`, inside a q-sql template) and
// a string (`y`): a list of strings (type 0) compares and takes the string
// as is; an enumerated column (types 20..76, a database's sym columns)
// takes a symbol; any other is cast to the column's own type.

/// `y` cast to the column's type.
const CAST: &str = "{$[0h=type x;y;19<abs type x;`$y;(upper .Q.t abs type x)$y]}";
/// Which values of the column equal `y`.
const EQ: &str = "{$[0h=type x;x~\\:y;19<abs type x;x=`$y;x=(upper .Q.t abs type x)$y]}";
/// Which values of the column are in the list of strings `y`.
const IN: &str = "{$[0h=type x;x in y;19<abs type x;x in `$y;x in (upper .Q.t abs type x)$y]}";
/// The new value of an updated column: one string per row for a list of
/// strings, else the cast atom.
const SET: &str = "{$[0h=type x;count[x]#enlist y;19<abs type x;`$y;(upper .Q.t abs type x)$y]}";

/// q that casts the string `text` to the type of column `col`.
fn cast(col: &str, text: &str) -> String {
    format!("{CAST}[{col};{}]", qstr(text))
}

fn eq(col: &str, text: &str) -> String {
    format!("{EQ}[{col};{}]", qstr(text))
}

/// The browse query: the first rows of a table or a view, or the start of
/// a variable's value.
pub fn browse(kind: &str, name: &str, limit: u32) -> String {
    let limit = limit.max(1);
    match kind {
        kinds::TABLE | kinds::VIEW => format!("select[{limit}] from {}", qref(name)),
        _ => format!("{limit} sublist {}", qref(name)),
    }
}

/// A browse query taken apart: `select[N] from SRC [where …]`, maybe
/// sorted (`N sublist `a xasc select from SRC …`).
#[derive(Debug, PartialEq)]
struct Browse {
    limit: u32,
    sort: Vec<(String, bool)>,
    /// What follows `from`, its where clause included.
    source: String,
}

impl Browse {
    fn parse(q: &str) -> Option<Self> {
        let q = q.trim();
        if let Some(rest) = q.strip_prefix("select[") {
            let (n, rest) = rest.split_once(']')?;
            let limit = n.trim().parse().ok()?;
            let source = rest.trim_start().strip_prefix("from ")?.trim().to_string();
            return Some(Self {
                limit,
                sort: Vec::new(),
                source,
            });
        }
        let (n, mut rest) = q.split_once(" sublist ")?;
        let limit = n.trim().parse().ok()?;
        let mut sort = Vec::new();
        loop {
            rest = rest.trim_start();
            let Some(r) = rest.strip_prefix('`') else {
                break;
            };
            let (col, r) = r.split_once(' ')?;
            let r = r.trim_start();
            let (desc, r) = if let Some(r) = r.strip_prefix("xdesc ") {
                (true, r)
            } else {
                (false, r.strip_prefix("xasc ")?)
            };
            sort.push((col.to_string(), desc));
            rest = r;
        }
        let source = rest.trim().strip_prefix("select from ")?.trim().to_string();
        Some(Self {
            limit,
            sort,
            source,
        })
    }

    fn render(&self) -> String {
        if self.sort.is_empty() {
            return format!("select[{}] from {}", self.limit, self.source);
        }
        let chain: String = self
            .sort
            .iter()
            .map(|(c, d)| format!("`{c} {} ", if *d { "xdesc" } else { "xasc" }))
            .collect();
        format!("{} sublist {chain}select from {}", self.limit, self.source)
    }
}

fn not_a_browse() -> Error {
    Error::Unsupported(
        "en kdb+ los filtros y el orden de la grilla se aplican a la consulta con la que se abrió una tabla o una vista".into(),
    )
}

fn column(c: &str) -> Result<&str> {
    if plain(c) && !c.contains('.') {
        Ok(c)
    } else {
        Err(Error::Unsupported(format!(
            "la columna {c} no tiene un nombre que q-sql pueda usar en un filtro"
        )))
    }
}

/// `*`, `?` and `[` taken literally in a `like` pattern.
fn like_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '*' | '?' | '[' | ']' => {
                out.push('[');
                out.push(c);
                out.push(']');
            }
            c => out.push(c),
        }
    }
    out
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// One filter as a q condition.
fn condition(f: &ColumnFilter) -> Result<String> {
    if f.op == FilterOp::Sql {
        return Ok(format!("({})", f.sql.as_deref().unwrap_or("1b")));
    }
    let c = column(&f.column)?;
    let first = f.values.first().map(value_text).unwrap_or_default();
    let raw = like_escape(&f.values.first().map(text_of).unwrap_or_default());
    let strings = format!("{{$[0h=type x;x;string x]}}[{c}]");
    let pattern = |p: String| format!("(lower {strings}) like lower {}", qstr(&p));
    let empty = format!("{{$[0h=type x;0=count each x;null x]}}[{c}]");
    let cmp = |op: &str| format!("{c}{op}{}", cast(c, &first));
    Ok(match f.op {
        FilterOp::Eq => eq(c, &first),
        FilterOp::Ne => format!("not {}", eq(c, &first)),
        FilterOp::Gt => cmp(">"),
        FilterOp::Ge => cmp(">="),
        FilterOp::Lt => cmp("<"),
        FilterOp::Le => cmp("<="),
        FilterOp::Contains => pattern(format!("*{raw}*")),
        FilterOp::NotContains => format!("not {}", pattern(format!("*{raw}*"))),
        FilterOp::StartsWith => pattern(format!("{raw}*")),
        FilterOp::EndsWith => pattern(format!("*{raw}")),
        FilterOp::IsNull | FilterOp::IsEmpty => empty,
        FilterOp::NotNull | FilterOp::NotEmpty => format!("not {empty}"),
        FilterOp::In | FilterOp::NotIn => {
            let items: Vec<String> = f.values.iter().map(|v| qstr(&value_text(v))).collect();
            let list = qlist(&items);
            let test = format!("{IN}[{c};{list}]");
            if f.op == FilterOp::In {
                test
            } else {
                format!("not {test}")
            }
        }
        FilterOp::IsTrue | FilterOp::TrueOrNull => format!("{c}=1b"),
        FilterOp::IsFalse | FilterOp::FalseOrNull => format!("{c}=0b"),
        FilterOp::SqlRight => format!("{c} {}", f.sql.as_deref().unwrap_or_default()),
        FilterOp::Sql => unreachable!(),
    })
}

/// A q general list of `items` (one item needs `enlist`).
pub fn qlist(items: &[String]) -> String {
    match items {
        [] => "()".into(),
        [one] => format!("enlist {one}"),
        many => format!("({})", many.join(";")),
    }
}

pub fn filtered_browse(browse: &str, filters: &[ColumnFilter]) -> Result<String> {
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let mut b = Browse::parse(browse).ok_or_else(not_a_browse)?;
    let conds = filters
        .iter()
        .map(|f| condition(f).map(|c| format!("({c})")))
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    b.source = if b.source.contains(" where ") {
        format!("{}, {conds}", b.source)
    } else {
        format!("{} where {conds}", b.source)
    };
    Ok(b.render())
}

pub fn sorted_browse(query: &str, sort: &[SortKey]) -> Result<String> {
    if sort.is_empty() {
        return Ok(query.to_string());
    }
    let mut b = Browse::parse(query).ok_or_else(not_a_browse)?;
    // q sorts are stable: the last key first.
    b.sort = sort
        .iter()
        .rev()
        .map(|k| column(&k.column).map(|c| (c.to_string(), k.descending)))
        .collect::<Result<_>>()?;
    Ok(b.render())
}

/// A `where` over the key's values.
fn key_where(key: &[(String, Value)]) -> Result<String> {
    if key.is_empty() {
        return Err(Error::Unsupported(
            "la fila no tiene valores con los que encontrarla".into(),
        ));
    }
    let conds = key
        .iter()
        .map(|(c, v)| {
            let c = column(c)?;
            Ok(format!("({})", eq(c, &value_text(v))))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(conds.join(", "))
}

/// Edited rows: one `update … from `t where <key>` each.
pub fn update_script(target: &str, changes: &[RowChange]) -> Result<String> {
    let mut out = Vec::new();
    for ch in changes {
        if ch.set.is_empty() {
            continue;
        }
        let sets = ch
            .set
            .iter()
            .map(|(c, v)| {
                let c = column(c)?;
                Ok(format!("{c}:{SET}[{c};{}]", qstr(&value_text(v))))
            })
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        out.push(format!(
            "update {sets} from {} where {}",
            qsym(target),
            key_where(&ch.key)?
        ));
    }
    Ok(out.join("\n"))
}

pub fn delete_script(target: &str, keys: &[Vec<(String, Value)>]) -> Result<String> {
    keys.iter()
        .map(|k| {
            Ok(format!(
                "delete from {} where {}",
                qsym(target),
                key_where(k)?
            ))
        })
        .collect::<Result<Vec<_>>>()
        .map(|v| v.join("\n"))
}

/// Rows inserted, 500 per statement: each column's strings are cast to
/// the table's type for it (`meta`), strings stay strings.
pub fn insert_script(target: &str, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
    if columns.is_empty() {
        return Err(Error::Unsupported("no hay columnas que insertar".into()));
    }
    for c in columns {
        column(c)?;
    }
    let names: String = columns.iter().map(|c| format!("`{c}")).collect();
    let names = if columns.len() == 1 {
        format!("enlist {names}")
    } else {
        names
    };
    let mut out = Vec::new();
    for chunk in rows.chunks(500) {
        let cols: Vec<String> = (0..columns.len())
            .map(|i| {
                let vals: Vec<String> = chunk
                    .iter()
                    .map(|r| qstr(&value_text(r.get(i).unwrap_or(&Value::Null))))
                    .collect();
                qlist(&vals)
            })
            .collect();
        out.push(format!(
            "{{[n;c;v] m:0!meta n; y:(exec t from m)(exec c from m)?c; n insert flip c!{{$[x in \"C \";y;(upper x)$y]}}'[y;v]}}[{};{names};{}]",
            qsym(target),
            qlist(&cols)
        ));
    }
    Ok(out.join("\n"))
}

/// A column type as the empty list q starts a table with: `` `long$() ``,
/// `()` for strings and general lists.
fn empty_column(data_type: &str) -> String {
    match cells::meta_letter(data_type) {
        Some(c) if c.is_ascii_lowercase() => {
            format!("`{}$()", data_type.trim().to_ascii_lowercase())
        }
        _ => "()".into(),
    }
}

/// `t:([k:`long$()] c:`symbol$(); …)`.
pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> Result<String> {
    let name = &t.name;
    let keys: Vec<&str> = t
        .primary_key
        .as_ref()
        .map(|k| k.columns.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let col = |c: &dbine_driver::ColumnDef| -> Result<String> {
        Ok(format!(
            "{}:{}",
            column(&c.name)?,
            empty_column(&c.data_type)
        ))
    };
    let key_cols = t
        .columns
        .iter()
        .filter(|c| keys.contains(&c.name.as_str()))
        .map(col)
        .collect::<Result<Vec<_>>>()?;
    let value_cols = t
        .columns
        .iter()
        .filter(|c| !keys.contains(&c.name.as_str()))
        .map(col)
        .collect::<Result<Vec<_>>>()?;
    let mut out = Vec::new();
    if parts.drop {
        // `delete t from `.` (or from its namespace), functionally.
        let (space, base) = match name.rfind('.') {
            Some(i) if i > 0 && name.starts_with('.') && plain(name) => {
                (&name[..i], &name[i + 1..])
            }
            _ => (".", name.as_str()),
        };
        out.push(format!("![`{space};();0b;enlist {}]", qsym(base)));
    }
    if parts.create {
        let body = format!("([{}] {})", key_cols.join("; "), value_cols.join("; "));
        out.push(if plain(name) {
            format!("{name}:{body}")
        } else {
            format!("{} set {body}", qsym(name))
        });
    }
    Ok(out.join("\n"))
}

pub const DATA_TYPES: &[&str] = &[
    "boolean",
    "guid",
    "byte",
    "short",
    "int",
    "long",
    "real",
    "float",
    "char",
    "symbol",
    "string",
    "timestamp",
    "month",
    "date",
    "datetime",
    "timespan",
    "minute",
    "second",
    "time",
];

pub fn designer() -> DesignerSpec {
    DesignerSpec {
        auto_increment: false,
        defaults: false,
        nullability: false,
        indexes: false,
        foreign_keys: false,
        ..DesignerSpec::sql_table(DATA_TYPES.to_vec())
    }
}

pub fn templates() -> Vec<CreateTemplate> {
    vec![
        CreateTemplate {
            kind: kinds::VIEW,
            label: "Nueva vista",
            template: "{name}::select from tabla where columna>0".into(),
        },
        CreateTemplate {
            kind: kinds::FUNCTION,
            label: "Nueva función",
            template: "{name}:{[x;y]\n  x+y\n  }".into(),
        },
    ]
}

/// A notebook parameter as a q literal.
pub fn param_literal(v: &ParamValue) -> String {
    match v {
        ParamValue::Null => "(::)".into(),
        ParamValue::Text(s) => qstr(s),
        // `42` is a long and `1.5` or `1e3` a float in q, as written.
        ParamValue::Number(n) => n.trim().to_string(),
        ParamValue::Bool(b) => if *b { "1b" } else { "0b" }.into(),
        ParamValue::Date(d) => iso_to_q(d),
        ParamValue::DateTime(d) => {
            let q = iso_to_q(d);
            if q.contains('D') {
                q
            } else {
                format!("{q}D00:00:00")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ColumnDef, KeyDef};
    use serde_json::json;

    #[test]
    fn names_strings_and_symbols() {
        assert_eq!(qstr("a\"b\\c\nd"), "\"a\\\"b\\\\c\\nd\"");
        assert!(plain("trade") && plain(".ns.t") && plain("t_1"));
        assert!(!plain("my table") && !plain("1t") && !plain("t.") && !plain("") && !plain("."));
        assert_eq!(qsym("trade"), "`trade");
        assert_eq!(qsym("my t"), "(`$ \"my t\")");
        assert_eq!(qref("my t"), "(get `$ \"my t\")");
        assert_eq!(browse(kinds::TABLE, "trade", 100), "select[100] from trade");
        assert_eq!(browse("variable", ".ns.x", 10), "10 sublist .ns.x");
    }

    #[test]
    fn grid_values_become_q_text() {
        assert_eq!(value_text(&json!("2024-01-31")), "2024.01.31");
        assert_eq!(
            value_text(&json!("2024-01-31 13:45:00.5")),
            "2024.01.31D13:45:00.5"
        );
        assert_eq!(value_text(&json!("2024-01")), "2024.01");
        assert_eq!(value_text(&json!("AAPL")), "AAPL");
        assert_eq!(value_text(&json!(1.5)), "1.5");
        assert_eq!(value_text(&json!(true)), "1");
        assert_eq!(value_text(&Value::Null), "");
    }

    #[test]
    fn filters_and_sort_build_on_the_browse() {
        let f = |c: &str, op, v: Value| ColumnFilter {
            column: c.into(),
            op,
            values: vec![v],
            sql: None,
        };
        let q = filtered_browse(
            "select[100] from trade",
            &[
                f("sym", FilterOp::Eq, json!("AAPL")),
                f("px", FilterOp::Gt, json!(10)),
            ],
        )
        .unwrap();
        assert!(q.starts_with("select[100] from trade where ("), "{q}");
        assert!(q.contains("[sym;\"AAPL\"]") && q.contains("px>"), "{q}");
        let s = sorted_browse(
            &q,
            &[
                SortKey {
                    column: "px".into(),
                    descending: true,
                },
                SortKey {
                    column: "sym".into(),
                    descending: false,
                },
            ],
        )
        .unwrap();
        assert!(
            s.starts_with("100 sublist `sym xasc `px xdesc select from trade where ("),
            "{s}"
        );
        // Filtering a sorted browse keeps the sort and adds to the where.
        let again = filtered_browse(&s, &[f("size", FilterOp::IsNull, Value::Null)]).unwrap();
        assert!(
            again.starts_with("100 sublist `sym xasc `px xdesc select from trade where ("),
            "{again}"
        );
        assert_eq!(again.matches(" where ").count(), 1);
        let c = filtered_browse(
            "select[5] from t",
            &[f("note", FilterOp::Contains, json!("a*b"))],
        )
        .unwrap();
        assert!(c.contains("like lower \"*a[*]b*\""), "{c}");
        assert!(filtered_browse("10 sublist x", &[f("a", FilterOp::Eq, json!(1))]).is_err());
        assert!(
            filtered_browse("select[5] from t", &[f("bad col", FilterOp::Eq, json!(1))]).is_err()
        );
        let raw = ColumnFilter {
            column: "x".into(),
            op: FilterOp::Sql,
            values: vec![],
            sql: Some("x within 1 5".into()),
        };
        assert!(filtered_browse("select[5] from t", &[raw])
            .unwrap()
            .ends_with("where ((x within 1 5))"));
        let multi = ColumnFilter {
            column: "sym".into(),
            op: FilterOp::In,
            values: vec![json!("A"), json!("B")],
            sql: None,
        };
        assert!(filtered_browse("select[5] from t", &[multi])
            .unwrap()
            .contains("[sym;(\"A\";\"B\")]"));
    }

    #[test]
    fn row_scripts() {
        let ch = RowChange {
            key: vec![("id".into(), json!(7))],
            set: vec![("px".into(), json!(1.5)), ("note".into(), json!("hi"))],
            row: vec![],
        };
        let u = update_script("trade", &[ch]).unwrap();
        assert!(u.starts_with("update px:"), "{u}");
        assert!(
            u.contains(&format!("from `trade where ({EQ}[id;\"7\"])")),
            "{u}"
        );
        let d = delete_script("my t", &[vec![("id".into(), json!(7))]]).unwrap();
        assert!(d.starts_with("delete from (`$ \"my t\") where "), "{d}");
        assert!(update_script(
            "t",
            &[RowChange {
                key: vec![],
                set: vec![("a".into(), json!(1))],
                row: vec![]
            }]
        )
        .is_err());
        let i = insert_script(
            "trade",
            &["sym".into(), "px".into()],
            &[vec![json!("A"), json!(1)], vec![json!("B"), Value::Null]],
        )
        .unwrap();
        assert!(
            i.ends_with("[`trade;`sym`px;((\"A\";\"B\");(\"1\";\"\"))]"),
            "{i}"
        );
        let one = insert_script("t", &["a".into()], &[vec![json!(1)]]).unwrap();
        assert!(one.ends_with("[`t;enlist `a;enlist enlist \"1\"]"), "{one}");
        let many: Vec<Vec<Value>> = (0..1200).map(|i| vec![json!(i)]).collect();
        assert_eq!(
            insert_script("t", &["a".into()], &many)
                .unwrap()
                .lines()
                .count(),
            3
        );
    }

    #[test]
    fn table_definition() {
        let t = TableSchema {
            name: "trade".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: "long".into(),
                    ..Default::default()
                },
                ColumnDef {
                    name: "sym".into(),
                    data_type: "symbol".into(),
                    ..Default::default()
                },
                ColumnDef {
                    name: "note".into(),
                    data_type: "string".into(),
                    ..Default::default()
                },
            ],
            primary_key: Some(KeyDef {
                name: None,
                columns: vec!["id".into()],
            }),
            ..Default::default()
        };
        let parts = DdlParts {
            drop: false,
            if_exists: false,
            create: true,
            indexes: false,
            foreign_keys: false,
        };
        assert_eq!(
            table_ddl(&t, parts).unwrap(),
            "trade:([id:`long$()] sym:`symbol$(); note:())"
        );
        let unkeyed = TableSchema {
            primary_key: None,
            ..t
        };
        assert_eq!(
            table_ddl(&unkeyed, parts).unwrap(),
            "trade:([] id:`long$(); sym:`symbol$(); note:())"
        );
    }

    #[test]
    fn parameters() {
        assert_eq!(
            param_literal(&ParamValue::Text("a\"b".into())),
            "\"a\\\"b\""
        );
        assert_eq!(param_literal(&ParamValue::Number("42".into())), "42");
        assert_eq!(param_literal(&ParamValue::Number("1.5".into())), "1.5");
        assert_eq!(param_literal(&ParamValue::Bool(true)), "1b");
        assert_eq!(
            param_literal(&ParamValue::Date("2026-10-18".into())),
            "2026.10.18"
        );
        assert_eq!(
            param_literal(&ParamValue::DateTime("2026-10-18T14:30:05".into())),
            "2026.10.18D14:30:05"
        );
        assert_eq!(param_literal(&ParamValue::Null), "(::)");
    }
}
