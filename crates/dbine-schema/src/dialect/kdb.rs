//! kdb+ (q). A table is a set of typed columns, keyed or not; a keyed
//! table's key columns are its primary key (`([id:`long$()] px:`float$())`).
//! There are no NOT NULL (every type has its null), defaults,
//! auto-increment, foreign keys in the DDL or comments. Its indexes are
//! attributes, one per column at most (`s#` sorted, `u#` unique, `p#`
//! parted, `g#` grouped): a unique index of one column becomes `u#` and
//! any other of one column `g#` (a hash index, the closest to a secondary
//! index); one of several columns has no attribute to be. From kdb+, each
//! attribute is an index of its column, unique for `u#`. A schema is a
//! namespace (`sales` → `.sales`; none, or the source's default mapped to
//! `""`, is the root).
//!
//! Types are `meta`'s, by name as the driver reports them: `boolean`,
//! `guid`, `byte`, `short`, `int`, `long`, `real`, `float`, `char`,
//! `symbol`, `timestamp`, `month`, `date`, `datetime`, `timespan`,
//! `minute`, `second`, `time`; `string` for a list of chars (`C`), `long
//! list` for a column whose cells are lists of longs (`J`), and `list` for a
//! general list (`meta`'s blank), whose cells may hold anything.
//!
//! What doesn't carry over: an attribute's kind to another engine (its
//! index stays, of the default kind), enumerations (a database's `sym`
//! columns read as symbols) and general lists (no neutral type: reported
//! as unknown).
//! A symbol reads as text of any length and other engines' text comes in
//! as `string`: q can't tell a category from free text.

use super::{Caps, Dialect, IdentCase, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::{IndexDef, TableSchema};
use std::collections::HashSet;

pub struct Kdb;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: Kdb = Kdb;
    (driver_id == "kdb").then_some(&D as &dyn Dialect)
}

/// The engine as the generic messages name it.
const Q: &str = "q";

/// `.Q.res`: names q reserves, which can't name a table or a column.
const RESERVED: &[&str] = &[
    "abs", "acos", "asin", "atan", "avg", "bin", "binr", "by", "cor", "cos", "cov", "dev",
    "delete", "div", "do", "enlist", "exec", "exit", "exp", "from", "getenv", "hopen", "if", "in",
    "insert", "last", "like", "log", "max", "min", "prd", "select", "setenv", "sin", "sqrt", "ss",
    "sum", "tan", "update", "var", "wavg", "while", "within", "wsum", "xexp",
];

/// The scalar q types, by name.
const SCALARS: &[&str] = &[
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
    "timestamp",
    "month",
    "date",
    "datetime",
    "timespan",
    "minute",
    "second",
    "time",
];

/// A scalar type's logical form.
fn scalar(name: &str) -> Option<L> {
    Some(match name {
        "boolean" => L::Bool,
        "guid" => L::Uuid,
        // 0x00 to 0xff.
        "byte" => L::Int {
            bytes: 1,
            unsigned: true,
        },
        "short" => L::int(2),
        "int" => L::int(4),
        "long" => L::int(8),
        "real" => L::Float { bytes: 4 },
        "float" => L::Float { bytes: 8 },
        "char" => L::Char {
            len: Some(1),
            unicode: false,
        },
        // Interned strings, of any length.
        "symbol" => L::Varchar {
            len: None,
            unicode: true,
        },
        // Nanoseconds since 2000.01.01.
        "timestamp" => L::Timestamp {
            precision: Some(9),
            tz: false,
        },
        // A month is its first day.
        "month" | "date" => L::Date,
        // Days since 2000.01.01 as a float: milliseconds (deprecated in q).
        "datetime" => L::Timestamp {
            precision: Some(3),
            tz: false,
        },
        // A duration in nanoseconds.
        "timespan" => L::Interval,
        "minute" | "second" => L::Time {
            precision: Some(0),
            tz: false,
        },
        "time" => L::Time {
            precision: Some(3),
            tz: false,
        },
        _ => return None,
    })
}

impl Dialect for Kdb {
    fn id(&self) -> &'static str {
        "kdb"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        let name = t.name.as_str();
        if name == "string" {
            return L::Text { unicode: true };
        }
        if let Some(t) = scalar(name) {
            return t;
        }
        match name.strip_suffix(" list") {
            // A column of byte lists: each cell is a binary value.
            Some("byte") => L::Blob,
            Some(base) => match scalar(base) {
                Some(of) => L::Array { of: Box::new(of) },
                None => L::Other {
                    native: t.raw.clone(),
                },
            },
            // `list` (a general list) and anything else.
            None => L::Other {
                native: t.raw.clone(),
            },
        }
    }

    fn render_type(&self, t: &L) -> Rendered {
        use IssueCode::*;
        use Severity::*;
        match t {
            L::Bool => Rendered::exact("boolean"),
            L::Int {
                bytes: 1,
                unsigned: true,
            } => Rendered::exact("byte"),
            L::Int { bytes, unsigned } => {
                let native = match L::signed_bytes_for(*bytes, *unsigned) {
                    1 | 2 => "short",
                    3 | 4 => "int",
                    8 => "long",
                    _ if *unsigned && *bytes == 8 => {
                        return Rendered::exact("float").with(
                            Loss,
                            PrecisionLoss,
                            "Entero sin signo grande como double: pierde precisión pasados los 15 dígitos.",
                        )
                    }
                    _ => {
                        return Rendered::exact("float").with(
                            Loss,
                            PrecisionLoss,
                            "Entero de 16 bytes como double: pierde precisión pasados los 15 dígitos.",
                        )
                    }
                };
                if *unsigned {
                    Rendered::exact(native).with(
                        Info,
                        TypeChanged,
                        format!(
                            "{Q} no tiene enteros sin signo: se usa un tipo más grande con signo."
                        ),
                    )
                } else {
                    Rendered::exact(native)
                }
            }
            // Whole numbers that fit a long stay exact.
            L::Decimal {
                precision: Some(p),
                scale,
            } if *p <= 18 && scale.unwrap_or(0) == 0 => Rendered::exact("long"),
            L::Decimal { .. } => Rendered::exact("float").with(
                Loss,
                PrecisionLoss,
                "No hay decimales exactos: FLOAT pierde precisión pasados los 15 dígitos.",
            ),
            L::Money => Rendered::exact("float").with(Loss, PrecisionLoss, format!("Moneda como {}.", "float")),
            L::Float { bytes: 4 } => Rendered::exact("real"),
            L::Float { .. } => Rendered::exact("float"),
            L::Char {
                len: Some(1) | None,
                ..
            } => Rendered::exact("char"),
            L::Char { .. } | L::Varchar { .. } | L::Text { .. } => Rendered::exact("string"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => Rendered::exact("byte list"),
            L::Bit { len: Some(1) | None } => Rendered::exact("boolean"),
            L::Bit { .. } => Rendered::exact("boolean list"),
            L::Date => Rendered::exact("date"),
            L::Time { precision, tz } => {
                let r = Rendered::exact(match precision {
                    Some(0) => "second",
                    Some(1..=3) | None => "time",
                    // Nanoseconds: a timespan.
                    _ => "timespan",
                });
                if *tz {
                    r.with(
                        Warning,
                        TimeZoneLoss,
                        format!("{Q} no guarda la zona horaria de una hora."),
                    )
                } else {
                    r
                }
            }
            L::Timestamp { tz, .. } => {
                let r = Rendered::exact("timestamp");
                if *tz {
                    r.with(
                        Warning,
                        TimeZoneLoss,
                        format!("{Q} no guarda la zona horaria: conviene guardar en UTC."),
                    )
                } else {
                    r
                }
            }
            L::Interval => Rendered::exact("timespan").with(
                Loss,
                RangeLoss,
                "Un timespan de q es una duración en nanosegundos: un intervalo con meses o años no entra.",
            ),
            L::Year => Rendered::exact("short").with(Info, TypeChanged, "Año como short."),
            L::Uuid => Rendered::exact("guid"),
            L::Json { .. } => Rendered::exact("string").with(Info, TypeChanged, "JSON como texto."),
            L::Xml => Rendered::exact("string").with(Info, TypeChanged, "XML como texto."),
            L::Enum { values } => Rendered::exact("symbol").with(
                Warning,
                TypeApproximated,
                format!(
                    "Enumerado como symbol: q no limita los valores. Valores: {}.",
                    values.join(", ")
                ),
            ),
            L::Set { values } => Rendered::exact("symbol list").with(
                Warning,
                TypeApproximated,
                format!("Conjunto como lista de texto. Valores: {}.", values.join(", ")),
            ),
            L::Array { of } => {
                let inner = self.render_type(of);
                let typed = SCALARS.contains(&inner.native.as_str());
                Rendered {
                    // Lists of strings or of lists: a general list.
                    native: if typed {
                        format!("{} list", inner.native)
                    } else {
                        "list".into()
                    },
                    notes: inner.notes,
                }
            }
            L::Map { .. } => {
                Rendered::exact("string").with(Warning, TypeApproximated, "Mapa como texto JSON.")
            }
            L::Geometry { .. } => Rendered::exact("string").with(
                Warning,
                TypeApproximated,
                "Dato espacial como texto (WKT).",
            ),
            L::Inet => {
                Rendered::exact("string").with(Info, TypeChanged, "Dirección IP como texto.")
            }
            L::MacAddr => {
                Rendered::exact("string").with(Info, TypeChanged, "Dirección MAC como texto.")
            }
            L::RowVersion => Rendered::exact("long").with(
                Warning,
                TypeApproximated,
                format!("{Q} no tiene versión de fila automática: no se actualiza sola."),
            ),
            L::Other { native } => Rendered::exact(native.clone()),
        }
    }

    fn render_default(&self, _d: &DefaultValue, _ty: &L) -> Option<String> {
        None
    }

    fn caps(&self) -> Caps {
        Caps {
            foreign_keys: false,
            on_delete: &[],
            on_update: &[],
            indexes: true,
            partial_indexes: false,
            supports_include: false,
            auto_increment: false,
            defaults: false,
            nullability: false,
            comments: false,
            max_identifier: 255,
            case: IdentCase::Preserve,
        }
    }

    /// Names q can't take become valid ones (as `.Q.id` does: other
    /// characters out, a letter first, a `1` after a reserved word), a key
    /// that holds every column is left out (a keyed table needs a column
    /// outside its key), and indexes become attributes ([`attributes`]).
    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        use IssueCode::*;
        use Severity::*;
        let source = t.name.clone();
        if let Some(s) = t.schema.take() {
            t.schema = namespace(&s);
            if let Some(ns) = t
                .schema
                .as_deref()
                .filter(|ns| ns[1..] != *s.trim_start_matches('.'))
            {
                report.push(
                    Info,
                    IdentifierRenamed,
                    &source,
                    None,
                    format!("«{s}» pasa a llamarse «{ns}»."),
                );
            }
        }
        let fixed = q_name(&t.name);
        if fixed != t.name {
            report.push(
                Info,
                IdentifierRenamed,
                &source,
                None,
                format!("«{}» pasa a llamarse «{}».", t.name, fixed),
            );
            t.name = fixed;
        }
        let mut taken: HashSet<String> = HashSet::new();
        for c in &mut t.columns {
            let mut name = q_name(&c.name);
            let base = name.clone();
            let mut n = 1;
            while taken.contains(&name) {
                n += 1;
                name = format!("{base}{n}");
            }
            taken.insert(name.clone());
            if name != c.name {
                report.push(
                    Info,
                    IdentifierRenamed,
                    &source,
                    Some(&c.name),
                    format!("«{}» pasa a llamarse «{}».", c.name, name),
                );
                if let Some(k) = t.primary_key.as_mut() {
                    for kc in k.columns.iter_mut().filter(|kc| **kc == c.name) {
                        *kc = name.clone();
                    }
                }
                for ic in t
                    .indexes
                    .iter_mut()
                    .flat_map(|i| i.columns.iter_mut())
                    .filter(|ic| **ic == c.name)
                {
                    *ic = name.clone();
                }
                c.name = name;
            }
        }
        let all_keys = t.primary_key.as_ref().is_some_and(|k| {
            !k.columns.is_empty() && t.columns.iter().all(|c| k.columns.contains(&c.name))
        });
        if all_keys {
            let cols = t
                .primary_key
                .take()
                .map(|k| k.columns.join(", "))
                .unwrap_or_default();
            report.push(
                Warning,
                PrimaryKeyDropped,
                &source,
                None,
                key_without_values(&cols),
            );
        }
        attributes(t, &source, report);
    }
}

/// The attribute (`s`, `u`, `p`, `g`) an index of one column is: the one
/// its kind names (a kdb+ index), else `u` for a unique one and `g` for
/// any other.
fn attribute(ix: &IndexDef) -> char {
    let kind = ix.kind.as_deref().unwrap_or("").trim();
    match kind.strip_suffix('#').unwrap_or(kind) {
        _ if ix.unique => 'u',
        "s" => 's',
        "p" => 'p',
        _ => 'g',
    }
}

/// `t`'s indexes as attributes, one per column, each an index named
/// after it (`g#sym`). An index of several columns, or a second one on a
/// column, is left out and said; of two on a column a unique one stays.
fn attributes(t: &mut TableSchema, source: &str, report: &mut Report) {
    use IssueCode::*;
    use Severity::*;
    let mut out: Vec<IndexDef> = Vec::new();
    for ix in std::mem::take(&mut t.indexes) {
        let [c] = ix.columns.as_slice() else {
            report.push(
                Dropped,
                IndexDropped,
                source,
                Some(&ix.name),
                format!(
                    "{}: q no tiene índices de varias columnas; un atributo (s#, u#, p#, g#) es de una sola.",
                    ix.name
                ),
            );
            continue;
        };
        let c = c.clone();
        let a = attribute(&ix);
        let attr = IndexDef {
            name: format!("{a}#{c}"),
            columns: vec![c.clone()],
            unique: a == 'u',
            kind: Some(format!("{a}#")),
            ..Default::default()
        };
        if let Some(i) = out.iter().position(|o| o.columns[0] == *c) {
            let (gone, kept) = if a == 'u' && !out[i].unique {
                (std::mem::replace(&mut out[i], attr), &out[i])
            } else {
                (ix, &out[i])
            };
            report.push(
                Warning,
                IndexDropped,
                source,
                Some(&gone.name),
                format!(
                    "{c}: q admite un atributo por columna; queda {}.",
                    kept.name
                ),
            );
            continue;
        }
        if ix.kind.as_deref() != attr.kind.as_deref() {
            report.push(
                Info,
                IndexChanged,
                source,
                Some(&ix.name),
                format!(
                    "«{}» pasa a ser el atributo {a}# de la columna {c}.",
                    ix.name
                ),
            );
        }
        out.push(attr);
    }
    t.indexes = out;
}

/// Why a keyed table whose key holds every column stays unkeyed (the
/// driver's sync says the same).
fn key_without_values(cols: &str) -> String {
    format!("Una tabla con clave de q necesita al menos una columna fuera de la clave: la tabla queda sin clave ({cols}).")
}

/// kdb+'s own namespaces and DBine's staging one: a schema never becomes
/// one of them.
const OWN_NAMESPACES: &[&str] = &["q", "Q", "h", "j", "o", "z", "dbine_stage"];

/// A schema as the namespace it is in q: `sales` → `.sales` (a name q
/// takes, never one of kdb+'s own); `None` for an empty one (the source's
/// default schema, which the app maps to `""`) or `.`: the root.
fn namespace(schema: &str) -> Option<String> {
    let s = schema.trim().trim_start_matches('.');
    if s.is_empty() {
        return None;
    }
    let mut n = q_name(s);
    if OWN_NAMESPACES.contains(&n.as_str()) {
        n.push('1');
    }
    Some(format!(".{n}"))
}

/// `name` as q takes it: letters, digits and `_`, starting with a letter,
/// not a reserved word.
fn q_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if !out.starts_with(|c: char| c.is_ascii_alphabetic()) {
        out.insert(0, 'a');
    }
    if RESERVED.contains(&out.as_str()) {
        out.push('1');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;
    use crate::Options;
    use dbine_driver::{ColumnDef, KeyDef};

    fn p(s: &str) -> L {
        crate::convert::logical_of(&Kdb, &parse(s))
    }

    fn r(t: L) -> String {
        Kdb.render_type(&t).native
    }

    #[test]
    fn every_q_type_parses() {
        assert_eq!(p("boolean"), L::Bool);
        assert_eq!(p("guid"), L::Uuid);
        assert_eq!(
            p("byte"),
            L::Int {
                bytes: 1,
                unsigned: true
            }
        );
        assert_eq!(p("short"), L::int(2));
        assert_eq!(p("int"), L::int(4));
        assert_eq!(p("long"), L::int(8));
        assert_eq!(p("real"), L::Float { bytes: 4 });
        assert_eq!(p("float"), L::Float { bytes: 8 });
        assert_eq!(
            p("char"),
            L::Char {
                len: Some(1),
                unicode: false
            }
        );
        assert_eq!(
            p("symbol"),
            L::Varchar {
                len: None,
                unicode: true
            }
        );
        assert_eq!(p("string"), L::Text { unicode: true });
        assert_eq!(
            p("timestamp"),
            L::Timestamp {
                precision: Some(9),
                tz: false
            }
        );
        assert_eq!(p("month"), L::Date);
        assert_eq!(p("date"), L::Date);
        assert_eq!(
            p("datetime"),
            L::Timestamp {
                precision: Some(3),
                tz: false
            }
        );
        assert_eq!(p("timespan"), L::Interval);
        for t in ["minute", "second"] {
            assert_eq!(
                p(t),
                L::Time {
                    precision: Some(0),
                    tz: false
                }
            );
        }
        assert_eq!(
            p("time"),
            L::Time {
                precision: Some(3),
                tz: false
            }
        );
        assert_eq!(
            p("long list"),
            L::Array {
                of: Box::new(L::int(8))
            }
        );
        assert_eq!(p("byte list"), L::Blob);
        // A general list has no neutral type: the conversion reports it.
        assert!(matches!(p("list"), L::Other { .. }));
        assert!(matches!(p("table"), L::Other { .. }));
    }

    #[test]
    fn neutral_types_render_as_q() {
        assert_eq!(r(L::int(1)), "short");
        assert_eq!(r(L::int(4)), "int");
        assert_eq!(
            r(L::Int {
                bytes: 4,
                unsigned: true
            }),
            "long"
        );
        assert_eq!(r(L::int(16)), "float");
        assert_eq!(
            r(L::Decimal {
                precision: Some(10),
                scale: Some(0)
            }),
            "long"
        );
        let d = Kdb.render_type(&L::Decimal {
            precision: Some(10),
            scale: Some(2),
        });
        assert_eq!(d.native, "float");
        assert_eq!(d.notes[0].code, IssueCode::PrecisionLoss);
        assert_eq!(
            r(L::Varchar {
                len: Some(40),
                unicode: true
            }),
            "string"
        );
        assert_eq!(
            r(L::Char {
                len: Some(1),
                unicode: false
            }),
            "char"
        );
        assert_eq!(r(L::Blob), "byte list");
        assert_eq!(r(L::Uuid), "guid");
        assert_eq!(
            r(L::Time {
                precision: Some(6),
                tz: false
            }),
            "timespan"
        );
        let ts = Kdb.render_type(&L::Timestamp {
            precision: Some(6),
            tz: true,
        });
        assert_eq!(ts.native, "timestamp");
        assert_eq!(ts.notes[0].code, IssueCode::TimeZoneLoss);
        assert_eq!(
            r(L::Array {
                of: Box::new(L::Float { bytes: 8 })
            }),
            "float list"
        );
        assert_eq!(
            r(L::Array {
                of: Box::new(L::Text { unicode: true })
            }),
            "list"
        );
        assert_eq!(r(L::Enum { values: vec![] }), "symbol");
        assert_eq!(r(L::Json { binary: true }), "string");
        assert_eq!(
            r(L::Other {
                native: "hstore".into()
            }),
            "hstore"
        );
    }

    #[test]
    fn q_types_survive_a_round_trip() {
        for t in SCALARS.iter().copied().chain(["string", "long list"]) {
            let back = r(p(t));
            let expect = match t {
                // A month is a date, minutes are seconds and a datetime a
                // timestamp outside q; a symbol is text of any length.
                "month" => "date",
                "symbol" => "string",
                "minute" => "second",
                "datetime" => "timestamp",
                _ => t,
            };
            assert_eq!(back, expect, "{t}");
        }
    }

    #[test]
    fn schemas_become_namespaces() {
        let ns = |s: Option<&str>| {
            let mut t = TableSchema {
                schema: s.map(String::from),
                name: "t".into(),
                ..Default::default()
            };
            let mut rep = Report::default();
            Kdb.finalize(&mut t, &mut rep);
            (t.schema, rep.issues.len())
        };
        assert_eq!(ns(Some("sales")), (Some(".sales".into()), 0));
        // Already q's, the root, and none.
        assert_eq!(ns(Some(".sales")), (Some(".sales".into()), 0));
        assert_eq!(ns(Some("")), (None, 0));
        assert_eq!(ns(Some(".")), (None, 0));
        assert_eq!(ns(None), (None, 0));
        // Fixed as a name is, and said; never kdb+'s own.
        assert_eq!(ns(Some("my schema")), (Some(".my_schema".into()), 1));
        assert_eq!(ns(Some("dfs.tmp")), (Some(".dfs_tmp".into()), 1));
        assert_eq!(ns(Some("z")), (Some(".z1".into()), 1));
        assert_eq!(ns(Some("Q")), (Some(".Q1".into()), 1));
        assert_eq!(ns(Some("dbine_stage")), (Some(".dbine_stage1".into()), 1));
        // A default schema the app maps to the root, end to end.
        let src = |schema: &str| TableSchema {
            kind: "table".into(),
            schema: Some(schema.into()),
            name: "orders".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                data_type: "bigint".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = Options {
            keep_schemas: true,
            rename_schemas: vec![("public".into(), String::new())],
            ..Options::default()
        };
        let r = crate::convert(&[src("public"), src("sales")], "postgres", "kdb", &opts).unwrap();
        let got: Vec<(Option<&str>, &str)> = r
            .tables
            .iter()
            .map(|t| (t.schema.as_deref(), t.name.as_str()))
            .collect();
        assert_eq!(got, [(None, "orders"), (Some(".sales"), "orders")]);
        // Into one schema the user names, with or without its dot.
        let one = Options {
            target_schema: Some("ns".into()),
            ..Options::default()
        };
        let r = crate::convert(&[src("public")], "postgres", "kdb", &one).unwrap();
        assert_eq!(r.tables[0].schema.as_deref(), Some(".ns"));
    }

    #[test]
    fn finalize_fixes_names_and_a_key_of_every_column() {
        let col = |n: &str| ColumnDef {
            name: n.into(),
            data_type: "long".into(),
            ..Default::default()
        };
        let mut t = TableSchema {
            name: "order items".into(),
            columns: vec![col("select"), col("2nd"), col("a b"), col("a_b")],
            primary_key: Some(KeyDef {
                name: None,
                columns: vec!["select".into()],
            }),
            ..Default::default()
        };
        let mut rep = Report::default();
        Kdb.finalize(&mut t, &mut rep);
        assert_eq!(t.name, "order_items");
        let names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["select1", "a2nd", "a_b", "a_b2"]);
        assert_eq!(t.primary_key.as_ref().unwrap().columns, ["select1"]);
        // The table, `select`, `2nd`, `a b` and the second `a_b`.
        assert_eq!(rep.issues.len(), 5);

        let mut link = TableSchema {
            name: "link".into(),
            columns: vec![col("a"), col("b")],
            primary_key: Some(KeyDef {
                name: None,
                columns: vec!["a".into(), "b".into()],
            }),
            ..Default::default()
        };
        let mut rep = Report::default();
        Kdb.finalize(&mut link, &mut rep);
        assert!(link.primary_key.is_none());
        assert_eq!(rep.issues[0].code, IssueCode::PrimaryKeyDropped);
    }

    fn index(name: &str, cols: &[&str], unique: bool, kind: Option<&str>) -> IndexDef {
        IndexDef {
            name: name.into(),
            columns: cols.iter().map(|c| c.to_string()).collect(),
            unique,
            kind: kind.map(str::to_string),
            ..Default::default()
        }
    }

    fn with_indexes(cols: &[(&str, &str)], indexes: Vec<IndexDef>) -> TableSchema {
        TableSchema {
            name: "orders".into(),
            columns: cols
                .iter()
                .map(|(n, t)| ColumnDef {
                    name: n.to_string(),
                    data_type: t.to_string(),
                    ..Default::default()
                })
                .collect(),
            indexes,
            ..Default::default()
        }
    }

    /// `(name, column, unique, kind)` of each index.
    fn shape(t: &TableSchema) -> Vec<(String, String, bool, String)> {
        t.indexes
            .iter()
            .map(|i| {
                (
                    i.name.clone(),
                    i.columns.join(","),
                    i.unique,
                    i.kind.clone().unwrap_or_default(),
                )
            })
            .collect()
    }

    #[test]
    fn indexes_become_attributes_and_back() {
        let pg = with_indexes(
            &[
                ("id", "bigint"),
                ("code", "text"),
                ("customer", "integer"),
                ("day", "date"),
                ("select", "text"),
            ],
            vec![
                index("ix_code", &["code"], true, Some("btree")),
                index("ix_customer", &["customer"], false, None),
                index("ix_day_customer", &["day", "customer"], false, None),
                index("ix_code2", &["code"], false, Some("hash")),
                index("ix_select", &["select"], false, None),
            ],
        );
        let r = crate::convert(
            std::slice::from_ref(&pg),
            "postgres",
            "kdb",
            &Options::default(),
        )
        .unwrap();
        // One attribute per column: u# for a unique index, g# for another;
        // `select`, renamed by q's rules, keeps its index.
        assert_eq!(
            shape(&r.tables[0]),
            [
                (
                    "u#code".to_string(),
                    "code".to_string(),
                    true,
                    "u#".to_string()
                ),
                ("g#customer".into(), "customer".into(), false, "g#".into()),
                ("g#select1".into(), "select1".into(), false, "g#".into()),
            ]
        );
        let dropped: Vec<&str> = r
            .issues
            .iter()
            .filter(|i| i.code == IssueCode::IndexDropped)
            .filter_map(|i| i.object.as_deref())
            .collect();
        assert_eq!(dropped, ["ix_day_customer", "ix_code2"], "{:?}", r.issues);

        // From kdb+: each attribute is an index of its column; u# unique.
        let q = with_indexes(
            &[("sym", "symbol"), ("id", "long"), ("time", "timestamp")],
            vec![
                index("g#sym", &["sym"], false, Some("g#")),
                index("u#id", &["id"], true, Some("u#")),
                index("s#time", &["time"], false, Some("s#")),
            ],
        );
        let to_pg = crate::convert(
            std::slice::from_ref(&q),
            "kdb",
            "postgres",
            &Options::default(),
        )
        .unwrap();
        let got: Vec<(String, bool, Option<String>)> = to_pg.tables[0]
            .indexes
            .iter()
            .map(|i| (i.columns.join(","), i.unique, i.kind.clone()))
            .collect();
        assert_eq!(
            got,
            [
                ("sym".to_string(), false, None),
                ("id".into(), true, None),
                ("time".into(), false, None),
            ]
        );
        // q to q keeps each attribute.
        let same =
            crate::convert(std::slice::from_ref(&q), "kdb", "kdb", &Options::default()).unwrap();
        assert_eq!(shape(&same.tables[0]), shape(&q));
    }

    #[test]
    fn a_unique_index_wins_its_column() {
        let mut t = with_indexes(
            &[("a", "long")],
            vec![
                index("x", &["a"], false, None),
                index("y", &["a"], true, None),
            ],
        );
        let mut rep = Report::default();
        Kdb.finalize(&mut t, &mut rep);
        assert_eq!(
            shape(&t),
            [("u#a".to_string(), "a".to_string(), true, "u#".to_string())]
        );
        let gone: Vec<&str> = rep
            .issues
            .iter()
            .filter(|i| i.code == IssueCode::IndexDropped)
            .filter_map(|i| i.object.as_deref())
            .collect();
        assert_eq!(gone, ["g#a"]);
    }
}
