//! `SDO_GEOMETRY` for the results' map. The thin client can't decode object
//! types (a query with one fails with "unsupported database type
//! DB_TYPE_OBJECT"), so the server turns each geometry into EWKT text:
//!
//! - a table's data (`browse_query`) selects its `SDO_GEOMETRY` columns
//!   through [`ewkt`];
//! - any other query with an object column is run again as
//!   `WITH "DBINE_Q" (C1…Cn) AS (<query>) SELECT …` with those columns
//!   through [`ewkt`] ([`wrap`]); the editor text stays as written. The
//!   column alias list renames by position, so duplicate names and `*` in
//!   the query don't matter, and its `ORDER BY` stays inside.
//!
//! EWKT keeps the SRID (`SRID=4326;POINT (…)`), with Oracle's own WGS 84
//! (8307) as 4326; a geometry without one comes as plain WKT. Its numbers
//! are the stored `NUMBER`s as Oracle prints them, so nothing is rounded on
//! the way. A column converted that way ([`converted`]) is typed
//! `SDO_GEOMETRY` in the result and comes whole (not cut at 64 KB like
//! other text).

use dbine_driver::sql::{quote_ident, Quote};

/// The type name a converted column gets in the result.
pub const TYPE_NAME: &str = "SDO_GEOMETRY";
/// Longest EWKT kept whole (as `json_spatial_bytes` does for bytes).
pub const TEXT_CAP: usize = 16 * 1024 * 1024;

/// `SDO_GEOMETRY` column names of a table or view, in order, with whether
/// each is a geometry: what its browse query selects. Invisible columns
/// (no `column_id`) are left out, as `SELECT *` does.
pub const BROWSE_COLUMNS: &str = "SELECT column_name,
        CASE WHEN data_type = 'SDO_GEOMETRY' AND data_type_owner IN ('MDSYS', 'PUBLIC') THEN 1 ELSE 0 END
   FROM all_tab_columns
  WHERE owner = :1 AND table_name = :2 AND column_id IS NOT NULL
  ORDER BY column_id";

const FUNCTION: &str = "SDO_UTIL.TO_WKTGEOMETRY(";

/// The EWKT of geometry `g` (a qualified column reference): NULL stays NULL.
pub fn ewkt(g: &str) -> String {
    format!(
        "CASE WHEN {g}.SDO_SRID IS NOT NULL THEN 'SRID=' || DECODE({g}.SDO_SRID, 8307, 4326, {g}.SDO_SRID) || ';' END || {FUNCTION}{g})"
    )
}

/// A table's browse query with its geometries as EWKT (`columns`: name and
/// whether it is `SDO_GEOMETRY`); `None` when it has none (plain
/// `SELECT *`).
pub fn browse(table: &str, columns: &[(String, bool)], limit: u32) -> Option<String> {
    if !columns.iter().any(|(_, g)| *g) {
        return None;
    }
    let list: Vec<String> = columns
        .iter()
        .map(|(name, geo)| {
            let q = quote_ident(Quote::Double, name);
            if *geo {
                format!("{} AS {q}", ewkt(&format!("t.{q}")))
            } else {
                format!("t.{q}")
            }
        })
        .collect();
    Some(format!("SELECT {}\nFROM {table} t\nFETCH FIRST {limit} ROWS ONLY", list.join(", ")))
}

/// `sql` (one query) run again with its object columns (`columns`: name and
/// whether it is an object) as EWKT, the other columns as they were.
pub fn wrap(sql: &str, columns: &[(String, bool)]) -> String {
    let body = sql.trim().trim_end_matches(';').trim_end();
    let names: Vec<String> = (1..=columns.len()).map(|i| format!("\"C{i}\"")).collect();
    let list: Vec<String> = columns
        .iter()
        .zip(&names)
        .map(|((name, object), c)| {
            let alias = quote_ident(Quote::Double, name);
            if *object {
                format!("{} AS {alias}", ewkt(&format!("q.{c}")))
            } else {
                format!("q.{c} AS {alias}")
            }
        })
        .collect();
    format!("WITH \"DBINE_Q\" ({}) AS (\n{body}\n)\nSELECT {}\nFROM \"DBINE_Q\" q", names.join(", "), list.join(", "))
}

/// Which result columns of `sql` are geometries turned into text by
/// `SDO_UTIL.TO_WKTGEOMETRY(…)`: those whose alias follows the call (as
/// [`ewkt`] writes it, and as `SELECT SDO_UTIL.TO_WKTGEOMETRY(g) AS g` reads)
/// and those named after the call itself (no alias).
pub fn converted(sql: &str, names: &[&str]) -> Vec<bool> {
    let upper = sql.to_ascii_uppercase();
    let mut aliases: Vec<String> = Vec::new();
    let mut from = 0;
    while let Some(i) = upper[from..].find(FUNCTION) {
        let start = from + i + FUNCTION.len();
        from = start;
        // The call's closing parenthesis, then `[AS] alias`.
        let mut depth = 1;
        let mut end = None;
        for (k, ch) in sql[start..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(start + k + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        if let Some(alias) = alias_after(&sql[end..]) {
            aliases.push(alias);
        }
    }
    names
        .iter()
        .map(|n| n.to_ascii_uppercase().starts_with(FUNCTION) || aliases.iter().any(|a| a == n))
        .collect()
}

/// The alias at the start of `rest` (`AS "x"`, `AS x`, `"x"` or `x`), as
/// the result names it (unquoted ones upper-cased).
fn alias_after(rest: &str) -> Option<String> {
    let mut s = rest.trim_start();
    if s.len() >= 3 && s[..2].eq_ignore_ascii_case("AS") && s[2..].starts_with(|c: char| c.is_whitespace() || c == '"') {
        s = s[2..].trim_start();
    }
    if let Some(q) = s.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = q.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    out.push('"');
                } else {
                    return Some(out);
                }
            } else {
                out.push(c);
            }
        }
        return None;
    }
    let word: String = s.chars().take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '$' | '#')).collect();
    let up = word.to_ascii_uppercase();
    (!word.is_empty() && !matches!(up.as_str(), "FROM" | "AS" | "END" | "WHERE" | "UNION" | "ORDER" | "GROUP" | "FETCH"))
        .then_some(up)
}

/// The query's columns could not be converted: what to do instead.
pub fn hint(objects: &[String]) -> String {
    format!(
        "El cliente de Oracle que usa DBine no lee tipos de objeto ({}). Las columnas SDO_GEOMETRY se convierten solas; \
         para otros tipos, seleccioná sus atributos, o convertí las geometrías con SDO_UTIL.TO_WKTGEOMETRY(columna).",
        objects.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_converts_only_geometries() {
        let cols = [("ID".to_string(), false), ("Geom \"x\"".to_string(), true)];
        assert_eq!(
            browse("\"APP\".\"T\"", &cols, 100).unwrap(),
            "SELECT t.\"ID\", CASE WHEN t.\"Geom \"\"x\"\"\".SDO_SRID IS NOT NULL THEN 'SRID=' || DECODE(t.\"Geom \"\"x\"\"\".SDO_SRID, 8307, 4326, \
             t.\"Geom \"\"x\"\"\".SDO_SRID) || ';' END || SDO_UTIL.TO_WKTGEOMETRY(t.\"Geom \"\"x\"\"\") AS \"Geom \"\"x\"\"\"\n\
             FROM \"APP\".\"T\" t\nFETCH FIRST 100 ROWS ONLY"
        );
        assert!(browse("\"APP\".\"T\"", &[("ID".into(), false)], 5).is_none());
    }

    #[test]
    fn wrap_renames_by_position() {
        let w = wrap("SELECT a.id, b.id, a.g FROM a JOIN b ON a.id = b.id ORDER BY 1 DESC;", &[("ID".into(), false), ("ID".into(), false), ("G".into(), true)]);
        assert!(w.starts_with("WITH \"DBINE_Q\" (\"C1\", \"C2\", \"C3\") AS (\nSELECT a.id, b.id, a.g FROM a JOIN b ON a.id = b.id ORDER BY 1 DESC\n)\n"), "{w}");
        assert!(w.contains("SELECT q.\"C1\" AS \"ID\", q.\"C2\" AS \"ID\", CASE WHEN q.\"C3\".SDO_SRID"), "{w}");
        assert!(w.ends_with("SDO_UTIL.TO_WKTGEOMETRY(q.\"C3\") AS \"G\"\nFROM \"DBINE_Q\" q"), "{w}");
    }

    #[test]
    fn finds_converted_columns() {
        let cols = [("ID".to_string(), false), ("GEOM".to_string(), true)];
        let sql = browse("\"T\"", &cols, 5).unwrap();
        assert_eq!(converted(&sql, &["ID", "GEOM"]), [false, true]);
        let sql = "select id, sdo_util.to_wktgeometry(nvl(g, x)) shape, sdo_util.to_wktgeometry(g) as \"Mixed\", sdo_util.to_wktgeometry(h) from t";
        assert_eq!(
            converted(sql, &["ID", "SHAPE", "Mixed", "SDO_UTIL.TO_WKTGEOMETRY(H)"]),
            [false, true, true, true]
        );
        assert_eq!(converted("SELECT SDO_UTIL.TO_WKTGEOMETRY(g) FROM t", &["X"]), [false]);
    }
}
