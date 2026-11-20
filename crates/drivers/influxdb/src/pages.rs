//! The explorer's pages of measurements (`Session::list_objects_page`) on
//! the three APIs, put together with [`crate::object_pages`]. Every
//! measurement is of one kind and has no schema.
//!
//! How each keeps the tree's order (by name ignoring case, then as
//! written; Rust's `to_lowercase`):
//!
//! - **InfluxQL** (1.x): `SHOW MEASUREMENTS` sorts names as written, by
//!   bytes, and can't fold case. Names in ASCII without capitals
//!   ([`plain_v1`]) sort the same either way: the server pages those
//!   (`WITH MEASUREMENT =~ /…/ LIMIT … OFFSET …`). The rest (capitals,
//!   other alphabets) are read whole, narrowed by a case-insensitive regex
//!   when searching, and sorted here; with more than
//!   [`object_pages::WHOLE_MAX`] of them the pages are left to the app.
//! - **Flux** (2.x): `strings.toLower` folds ASCII as Rust does, and
//!   `sort` compares bytes: the server filters, sorts and pages ASCII names.
//!   Go's mappings differ from Rust's in places outside ASCII (`İ`, a
//!   final sigma), so those names are read whole and sorted here (the
//!   same limit).
//! - **SQL** (3.x): DataFusion's `lower` is Rust's `to_lowercase` and its
//!   strings compare by bytes: one query filters, sorts and pages, with
//!   the user's text bound (`$p`, `$n`).

use crate::object_pages::{self, WHOLE_MAX};
use crate::v2::flux_str;
use dbine_driver::{kinds, DbObject, ObjectQuery};

pub fn measurement(name: String) -> DbObject {
    DbObject {
        kind: kinds::MEASUREMENT.into(),
        schema: None,
        name,
        parent: None,
    }
}

/// Whether `q` can keep a measurement at all.
pub fn wanted(q: &ObjectQuery) -> bool {
    object_pages::wants(q, kinds::MEASUREMENT) && object_pages::schema_ok(q, None)
}

/// A name InfluxQL sorts as the tree does: ASCII without capitals.
pub fn plain_v1(name: &str) -> bool {
    name.bytes()
        .all(|b| b.is_ascii() && !b.is_ascii_uppercase())
}

/// A name Flux folds and sorts as the tree does: ASCII.
pub fn plain_v2(name: &str) -> bool {
    name.is_ascii()
}

/// `s` as plain text in an InfluxQL regex literal (`/…/`, Go's RE2):
/// metacharacters and the slash escaped, control and non-ASCII
/// characters by code point.
fn re_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
            | '/' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_ascii_graphic() || c == ' ' => out.push(c),
            c => out.push_str(&format!("\\x{{{:x}}}", c as u32)),
        }
    }
    out
}

/// The characters of [`plain_v1`] names, as a regex class.
const PLAIN_V1: &str = r"[\x00-\x40\x5B-\x7F]";

/// `SHOW MEASUREMENTS` of the [`plain_v1`] names `q` keeps, the page from
/// `offset` (one more than `size`); `None` when no such name can match.
pub fn v1_plain(q: &ObjectQuery, offset: usize, size: usize) -> Option<String> {
    let re = if let Some(n) = &q.name {
        let n = n.to_lowercase();
        let pattern_ok = q
            .pattern
            .as_deref()
            .is_none_or(|p| n.contains(&p.to_lowercase()));
        if !plain_v1(&n) || !pattern_ok {
            return None;
        }
        format!("^{}$", re_text(&n))
    } else if let Some(p) = &q.pattern {
        let p = p.to_lowercase();
        if !plain_v1(&p) {
            return None;
        }
        format!("^{PLAIN_V1}*{}{PLAIN_V1}*$", re_text(&p))
    } else {
        format!("^{PLAIN_V1}*$")
    };
    Some(format!(
        "SHOW MEASUREMENTS WITH MEASUREMENT =~ /{re}/ LIMIT {} OFFSET {offset}",
        size.saturating_add(1)
    ))
}

/// `SHOW MEASUREMENTS` of the names that may be the rest of `q`'s
/// (capitals or other alphabets; searches narrowed case-insensitively,
/// `İ`, whose lowercase is two characters, always in), at most
/// [`WHOLE_MAX`] + 1. Each still goes through [`plain_v1`] and `q.matches`
/// here.
pub fn v1_rest(q: &ObjectQuery) -> String {
    let text = |s: &str| {
        let lower = s.to_lowercase();
        if lower == s {
            re_text(s)
        } else {
            format!("{}|{}", re_text(s), re_text(&lower))
        }
    };
    let re = if let Some(n) = &q.name {
        format!("(?i)^(?:{})$|\\x{{130}}", text(n))
    } else if let Some(p) = &q.pattern {
        format!("(?i){}|\\x{{130}}", text(p))
    } else {
        r"[A-Z\x{80}-\x{10FFFF}]".to_string()
    };
    format!(
        "SHOW MEASUREMENTS WITH MEASUREMENT =~ /{re}/ LIMIT {}",
        WHOLE_MAX + 1
    )
}

const FLUX_SCHEMA: &str = "import \"influxdata/influxdb/schema\"\nimport \"strings\"\n";

/// The Flux script of the ASCII names `q` keeps in `bucket`, filtered,
/// sorted and paged on the server (one more than `size`, from `offset`);
/// `None` when no ASCII name can match.
pub fn v2_plain(bucket: &str, q: &ObjectQuery, offset: usize, size: usize) -> Option<String> {
    let mut filters = vec![r"r._value =~ /^[\x00-\x7F]*$/".to_string()];
    if let Some(n) = &q.name {
        let n = n.to_lowercase();
        if !n.is_ascii() {
            return None;
        }
        filters.push(format!("strings.toLower(v: r._value) == {}", flux_str(&n)));
    }
    if let Some(p) = &q.pattern {
        let p = p.to_lowercase();
        if !p.is_ascii() {
            return None;
        }
        filters.push(format!(
            "strings.containsStr(v: strings.toLower(v: r._value), substr: {})",
            flux_str(&p)
        ));
    }
    Some(format!(
        "{FLUX_SCHEMA}schema.measurements(bucket: {}, {})\n  \
         |> filter(fn: (r) => {})\n  \
         |> map(fn: (r) => ({{_value: r._value, _k: strings.toLower(v: r._value)}}))\n  \
         |> group()\n  \
         |> sort(columns: [\"_k\", \"_value\"])\n  \
         |> limit(n: {}, offset: {offset})\n  \
         |> keep(columns: [\"_value\"])",
        flux_str(bucket),
        crate::transfer::FLUX_ALL_TIME,
        filters.join(" and "),
        size.saturating_add(1)
    ))
}

/// The Flux script of `bucket`'s names outside ASCII (filtered and sorted
/// here), at most [`WHOLE_MAX`] + 1.
pub fn v2_rest(bucket: &str) -> String {
    format!(
        "{FLUX_SCHEMA}schema.measurements(bucket: {}, {})\n  \
         |> filter(fn: (r) => r._value =~ /[^\\x00-\\x7F]/)\n  \
         |> limit(n: {})",
        flux_str(bucket),
        crate::transfer::FLUX_ALL_TIME,
        WHOLE_MAX + 1
    )
}

/// InfluxDB 3's page: the SQL (the search bound as `$p`, the lookup as
/// `$n`) and its parameters.
pub fn v3_page(
    q: &ObjectQuery,
    offset: usize,
    size: usize,
) -> (String, serde_json::Map<String, serde_json::Value>) {
    let mut params = serde_json::Map::new();
    let mut sql =
        "SELECT table_name FROM information_schema.tables WHERE table_schema = 'iox'".to_string();
    if let Some(n) = &q.name {
        sql.push_str(" AND lower(table_name) = lower($n)");
        params.insert("n".into(), n.clone().into());
    }
    if let Some(p) = &q.pattern {
        sql.push_str(" AND strpos(lower(table_name), lower($p)) > 0");
        params.insert("p".into(), p.clone().into());
    }
    sql.push_str(&format!(
        " ORDER BY lower(table_name), table_name LIMIT {} OFFSET {offset}",
        size.saturating_add(1)
    ));
    (sql, params)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(pattern: Option<&str>, name: Option<&str>) -> ObjectQuery {
        ObjectQuery {
            pattern: pattern.map(Into::into),
            name: name.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn influxql_regexes_escape_the_text() {
        assert_eq!(
            v1_plain(&q(None, None), 20, 10).unwrap(),
            r"SHOW MEASUREMENTS WITH MEASUREMENT =~ /^[\x00-\x40\x5B-\x7F]*$/ LIMIT 11 OFFSET 20"
        );
        assert_eq!(
            v1_plain(&q(Some("A.b/c"), None), 0, 1000).unwrap(),
            r"SHOW MEASUREMENTS WITH MEASUREMENT =~ /^[\x00-\x40\x5B-\x7F]*a\.b\/c[\x00-\x40\x5B-\x7F]*$/ LIMIT 1001 OFFSET 0"
        );
        assert_eq!(
            v1_plain(&q(None, Some("Cpu\\")), 0, 5).unwrap(),
            r"SHOW MEASUREMENTS WITH MEASUREMENT =~ /^cpu\\$/ LIMIT 6 OFFSET 0"
        );
        // Text no plain name can hold: only the rest can match.
        assert!(v1_plain(&q(Some("ñ"), None), 0, 5).is_none());
        assert!(v1_plain(&q(Some("x"), Some("abc")), 0, 5).is_none());
        assert_eq!(
            v1_rest(&q(None, None)),
            r"SHOW MEASUREMENTS WITH MEASUREMENT =~ /[A-Z\x{80}-\x{10FFFF}]/ LIMIT 10001"
        );
        assert_eq!(
            v1_rest(&q(Some("Ñ(\n"), None)),
            r"SHOW MEASUREMENTS WITH MEASUREMENT =~ /(?i)\x{d1}\(\x{a}|\x{f1}\(\x{a}|\x{130}/ LIMIT 10001"
        );
        assert_eq!(
            v1_rest(&q(None, Some("ab"))),
            r"SHOW MEASUREMENTS WITH MEASUREMENT =~ /(?i)^(?:ab)$|\x{130}/ LIMIT 10001"
        );
    }

    #[test]
    fn plain_names() {
        assert!(plain_v1("cpu_load.1 x,y"));
        assert!(!plain_v1("Cpu") && !plain_v1("ñu"));
        assert!(plain_v2("Cpu") && !plain_v2("ñu"));
    }

    #[test]
    fn flux_scripts_quote_the_text() {
        let s = v2_plain("b\"1", &q(Some("A\"${x}"), Some("N")), 7, 3).unwrap();
        assert!(s.contains(r#"schema.measurements(bucket: "b\"1", "#), "{s}");
        assert!(s.contains(r#"strings.toLower(v: r._value) == "n""#), "{s}");
        assert!(s.contains(r#"substr: "a\"\${x}")"#), "{s}");
        assert!(s.contains("|> limit(n: 4, offset: 7)"), "{s}");
        assert!(v2_plain("b", &q(Some("ñ"), None), 0, 3).is_none());
        assert!(v2_rest("b").ends_with("|> limit(n: 10001)"));
    }

    #[test]
    fn sql_binds_the_text() {
        let (sql, params) = v3_page(&q(Some("q'z"), Some("y")), 40, 20);
        assert!(!sql.contains("q'z"), "{sql}");
        assert_eq!(params["p"], "q'z");
        assert_eq!(params["n"], "y");
        assert!(sql.ends_with("ORDER BY lower(table_name), table_name LIMIT 21 OFFSET 40"));
    }
}
