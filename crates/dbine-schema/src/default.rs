//! Column defaults. Each engine spells "now", "a new UUID" or "true"
//! differently, and catalogs wrap literals in casts and parentheses
//! (`('abc'::character varying)`, `((0))`, `(getdate())`). A default is
//! parsed into a [`DefaultValue`] and rendered in the target's spelling;
//! an expression nobody recognizes is kept verbatim and reported.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum DefaultValue {
    Null,
    /// A number, as written (`0`, `-1.5`).
    Number(String),
    /// A string literal, unescaped.
    Text(String),
    Bool(bool),
    CurrentTimestamp,
    CurrentDate,
    CurrentTime,
    /// A freshly generated UUID.
    NewUuid,
    /// The next value of a sequence (the auto-increment of engines without
    /// identity columns).
    NextVal(String),
    /// Anything else, verbatim in the source's syntax.
    Expr(String),
}

/// Strip the wrapping catalogs add: outer parentheses and PostgreSQL casts
/// (`'x'::text`, `0::numeric`).
pub fn strip_wrapping(s: &str) -> &str {
    let mut s = s.trim();
    loop {
        let before = s;
        if s.starts_with('(') && s.ends_with(')') && balanced(&s[1..s.len() - 1]) {
            s = s[1..s.len() - 1].trim();
        }
        // Trailing `::type` casts, outside quotes.
        if let Some(pos) = last_cast(s) {
            s = s[..pos].trim();
        }
        if s == before {
            return s;
        }
    }
}

fn balanced(s: &str) -> bool {
    let mut depth = 0i32;
    let mut q = false;
    for c in s.chars() {
        match c {
            '\'' => q = !q,
            '(' if !q => depth += 1,
            ')' if !q => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0 && !q
}

fn last_cast(s: &str) -> Option<usize> {
    let mut q = false;
    let mut depth = 0i32;
    let bytes = s.as_bytes();
    let mut found = None;
    for i in 0..bytes.len() {
        match bytes[i] {
            b'\'' => q = !q,
            b'(' if !q => depth += 1,
            b')' if !q => depth -= 1,
            b':' if !q && depth == 0 && i + 1 < bytes.len() && bytes[i + 1] == b':' => found = Some(i),
            _ => {}
        }
    }
    found
}

/// Parse a default written in any common SQL spelling.
pub fn parse_default(raw: &str) -> DefaultValue {
    // MySQL's `CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP`: the default is the first part.
    if let Some(i) = raw.to_ascii_lowercase().find(" on update ") {
        return parse_default(&raw[..i]);
    }
    let s = strip_wrapping(raw);
    // One spelling for the keywords some engines write with spaces
    // (Db2 / SQL Anywhere `CURRENT TIMESTAMP`).
    let lower = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
        .replace("current timestamp", "current_timestamp")
        .replace("current date", "current_date")
        .replace("current time", "current_time");
    let call = lower.trim_end_matches("()");
    if lower == "null" {
        return DefaultValue::Null;
    }
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') && balanced(s) {
        return DefaultValue::Text(s[1..s.len() - 1].replace("''", "'"));
    }
    // N'…' (SQL Server unicode literal).
    if (s.starts_with("N'") || s.starts_with("n'")) && s.ends_with('\'') && s.len() >= 3 {
        return DefaultValue::Text(s[2..s.len() - 1].replace("''", "'"));
    }
    if s.parse::<f64>().is_ok() {
        return DefaultValue::Number(s.to_string());
    }
    // Typed literals: `NUMERIC '0'` (Spanner, BigQuery), `DATE '2020-01-01'`.
    if let Some((kw, lit)) = s.split_once(char::is_whitespace) {
        let lit = lit.trim();
        if lit.len() >= 2 && lit.starts_with('\'') && lit.ends_with('\'') && kw.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            let inner = lit[1..lit.len() - 1].replace("''", "'");
            return match kw.to_ascii_lowercase().as_str() {
                "numeric" | "bignumeric" | "decimal" | "int64" | "float64" | "json" if inner.parse::<f64>().is_ok() => DefaultValue::Number(inner),
                _ => DefaultValue::Text(inner),
            };
        }
    }
    match lower.as_str() {
        "true" | "b'1'" => return DefaultValue::Bool(true),
        "false" | "b'0'" => return DefaultValue::Bool(false),
        // SQL Anywhere identity columns.
        "autoincrement" | "global autoincrement" => return DefaultValue::NextVal(String::new()),
        _ => {}
    }
    // Informix: `CURRENT YEAR TO FRACTION(3)`, `CURRENT HOUR TO SECOND`.
    if let Some(q) = lower.strip_prefix("current ") {
        if q.starts_with("year to") {
            return DefaultValue::CurrentTimestamp;
        }
        if q.starts_with("hour to") {
            return DefaultValue::CurrentTime;
        }
    }
    match call {
        "current_timestamp" | "now" | "getdate" | "sysdatetime" | "systimestamp" | "sysdate" | "localtimestamp"
        | "current_timestamp(3)" | "current_timestamp(6)" | "getutcdate" | "sysutcdatetime" | "clock_timestamp"
        | "transaction_timestamp" | "statement_timestamp" | "datetime('now')" | "sysdatetimeoffset"
        | "current_datetime" | "current_bigdatetime" | "current_utctimestamp" | "sys_datetime" | "sys_timestamp"
        | "local_timestamp" | "timestamp" => return DefaultValue::CurrentTimestamp,
        "current_date" | "curdate" | "date('now')" | "today" | "sys_date" | "date" => return DefaultValue::CurrentDate,
        "current_time" | "curtime" | "time('now')" | "localtime" | "local_time" | "sys_time" | "systime" | "current_bigtime"
        | "time" => return DefaultValue::CurrentTime,
        "gen_random_uuid" | "uuid_generate_v4" | "newid" | "newsequentialid" | "uuid" | "sys_guid" | "generateuuidv4"
        | "uuid_generate_v1" | "random_uuid" | "generate_uuid" | "uuid_string" | "gen_uuid" | "sysuuid" | "newuid"
        | "uuid_generate" => return DefaultValue::NewUuid,
        _ => {}
    }
    // Today and now as SQL Server and Oracle spell them (SQL Server's catalog
    // rewrites `CAST(GETDATE() AS date)` as `CONVERT([date],getdate())`).
    let compact: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    match compact.as_str() {
        "convert([date],getdate())" | "convert([date],sysdatetime())" | "convert(date,getdate())" | "cast(getdate()asdate)"
        | "cast(sysdatetime()asdate)" | "trunc(sysdate)" => return DefaultValue::CurrentDate,
        "convert([time],getdate())" | "convert([time],sysdatetime())" | "convert(time,getdate())" | "cast(getdate()astime)"
        | "cast(sysdatetime()astime)" => return DefaultValue::CurrentTime,
        _ => {}
    }
    // Any other `CAST(x AS t)` (DuckDB `CAST('t' AS BOOLEAN)`): the value is x.
    if lower.starts_with("cast(") && lower.ends_with(')') {
        if let Some(pos) = lower.rfind(" as ") {
            let inner = parse_default(s[5..pos].trim());
            if !matches!(inner, DefaultValue::Expr(_)) {
                return inner;
            }
        }
    }
    if ["current_timestamp(", "now(", "now64(", "localtimestamp(", "current_datetime("].iter().any(|p| lower.starts_with(p)) {
        return DefaultValue::CurrentTimestamp;
    }
    if let Some(rest) = lower.strip_prefix("nextval(") {
        let inner = rest.trim_end_matches(')');
        let name = inner.trim().trim_matches('\'');
        let name = name.split("::").next().unwrap_or(name).trim_matches('\'').trim_matches('"');
        return DefaultValue::NextVal(name.to_string());
    }
    if lower.starts_with("current_time(") {
        return DefaultValue::CurrentTime;
    }
    if let Some(seq) = lower.strip_suffix(".nextval") {
        return DefaultValue::NextVal(seq.trim_matches('"').to_string());
    }
    if lower.starts_with("next value for ") {
        return DefaultValue::NextVal(s[15..].trim().replace(['"', '[', ']'], ""));
    }
    DefaultValue::Expr(raw.trim().to_string())
}

/// SQL string literal with `'` doubled.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use DefaultValue::*;

    #[test]
    fn catalog_wrapping() {
        assert_eq!(parse_default("('abc'::character varying)"), Text("abc".into()));
        assert_eq!(parse_default("((0))"), Number("0".into()));
        assert_eq!(parse_default("(getdate())"), CurrentTimestamp);
        assert_eq!(parse_default("'it''s'::text"), Text("it's".into()));
        assert_eq!(parse_default("N'hola'"), Text("hola".into()));
        assert_eq!(parse_default("0::numeric"), Number("0".into()));
    }

    #[test]
    fn functions() {
        assert_eq!(parse_default("now()"), CurrentTimestamp);
        assert_eq!(parse_default("CURRENT_TIMESTAMP(6)"), CurrentTimestamp);
        assert_eq!(parse_default("SYSDATE"), CurrentTimestamp);
        assert_eq!(parse_default("gen_random_uuid()"), NewUuid);
        assert_eq!(parse_default("(newid())"), NewUuid);
        assert_eq!(parse_default("CURRENT_DATE"), CurrentDate);
        assert_eq!(parse_default("true"), Bool(true));
        assert_eq!(parse_default("NULL"), Null);
    }

    #[test]
    fn engine_spellings() {
        assert_eq!(parse_default("CURRENT TIMESTAMP"), CurrentTimestamp);
        assert_eq!(parse_default("current date"), CurrentDate);
        assert_eq!(parse_default("CURRENT YEAR TO FRACTION(3)"), CurrentTimestamp);
        assert_eq!(parse_default("CURRENT HOUR TO SECOND"), CurrentTime);
        assert_eq!(parse_default("now64(3)"), CurrentTimestamp);
        assert_eq!(parse_default("CURRENT_DATETIME()"), CurrentTimestamp);
        assert_eq!(parse_default("GENERATE_UUID()"), NewUuid);
        assert_eq!(parse_default("UUID_STRING()"), NewUuid);
        assert_eq!(parse_default("SYSTIME"), CurrentTime);
        assert_eq!(parse_default("autoincrement"), NextVal(String::new()));
        assert_eq!(parse_default("CAST('t' AS BOOLEAN)"), Text("t".into()));
        assert_eq!(parse_default("NUMERIC '0'"), Number("0".into()));
        assert_eq!(parse_default("DATE '2020-01-01'"), Text("2020-01-01".into()));
        assert_eq!(parse_default("CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP"), CurrentTimestamp);
    }

    #[test]
    fn sequences_and_expressions() {
        assert_eq!(parse_default("nextval('pedidos_id_seq'::regclass)"), NextVal("pedidos_id_seq".into()));
        assert_eq!(parse_default("PEDIDOS_SEQ.NEXTVAL"), NextVal("pedidos_seq".into()));
        assert_eq!(parse_default("NEXT VALUE FOR [dbo].[seq]"), NextVal("dbo.seq".into()));
        assert_eq!(parse_default("lower('X')"), Expr("lower('X')".into()));
        assert_eq!(parse_default("CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP"), CurrentTimestamp);
        assert_eq!(parse_default("(CONVERT([date],getdate()))"), CurrentDate);
        assert_eq!(parse_default("TRUNC(SYSDATE)"), CurrentDate);
    }
}
