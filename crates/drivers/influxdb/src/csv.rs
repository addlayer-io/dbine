//! Flux's annotated CSV (`#datatype`, `#group`, `#default` rows before
//! each header) as tables: one per distinct schema, cells typed by
//! `#datatype`.

use dbine_driver::{json_f64, json_i64, json_u64};
use serde_json::Value as J;

#[derive(Debug, Default, PartialEq)]
pub struct Table {
    pub columns: Vec<String>,
    pub types: Vec<String>,
    pub rows: Vec<Vec<J>>,
}

/// Records of a CSV text, `None` for a blank line (Flux's table separator).
fn records(text: &str) -> Vec<Option<Vec<String>>> {
    let mut out = Vec::new();
    let mut fields: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut line_has_content = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    quoted = false;
                }
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            '"' => {
                quoted = true;
                line_has_content = true;
            }
            ',' => {
                fields.push(std::mem::take(&mut cur));
                line_has_content = true;
            }
            '\r' => {}
            '\n' => {
                if line_has_content || !cur.is_empty() {
                    fields.push(std::mem::take(&mut cur));
                    out.push(Some(std::mem::take(&mut fields)));
                } else {
                    out.push(None);
                }
                line_has_content = false;
            }
            _ => {
                cur.push(c);
                line_has_content = true;
            }
        }
    }
    if line_has_content || !cur.is_empty() {
        fields.push(cur);
        out.push(Some(fields));
    }
    out
}

/// Tables of an annotated CSV response. Drops the annotation column and
/// `result` (always `_result` unless the script `yield`s several).
/// An `error,reference` table (an error mid-stream) becomes `Err`.
pub fn parse(text: &str) -> Result<Vec<Table>, String> {
    let mut tables: Vec<Table> = Vec::new();
    let mut types: Vec<String> = Vec::new();
    let mut defaults: Vec<String> = Vec::new();
    // Columns of the current table, as indexes into the CSV record.
    let mut keep: Vec<usize> = Vec::new();
    let mut in_table = false;
    let mut error_table = false;
    // The table rows go to (tables with the same schema share one).
    let mut current = 0;

    for rec in records(text) {
        let Some(rec) = rec else {
            in_table = false;
            continue;
        };
        let first = rec.first().map(String::as_str).unwrap_or("");
        if let Some(annotation) = first.strip_prefix('#') {
            in_table = false;
            match annotation {
                "datatype" => types = rec[1..].to_vec(),
                "default" => defaults = rec[1..].to_vec(),
                _ => {}
            }
            continue;
        }
        if !in_table {
            // The header.
            in_table = true;
            let names = &rec[1.min(rec.len())..];
            error_table = names.first().is_some_and(|n| n == "error");
            keep = (0..names.len()).filter(|&i| names[i] != "result" || names.len() == 1).collect();
            let columns: Vec<String> = keep.iter().map(|&i| names[i].clone()).collect();
            let col_types: Vec<String> = keep.iter().map(|&i| types.get(i).cloned().unwrap_or_default()).collect();
            if !error_table {
                current = match tables.iter().position(|t| t.columns == columns && t.types == col_types) {
                    Some(i) => i,
                    None => {
                        tables.push(Table { columns, types: col_types, rows: Vec::new() });
                        tables.len() - 1
                    }
                };
            }
            continue;
        }
        let cells = &rec[1.min(rec.len())..];
        if error_table {
            let msg = cells.first().cloned().unwrap_or_default();
            if !msg.is_empty() {
                return Err(msg);
            }
            continue;
        }
        let t = &mut tables[current];
        let row = keep
            .iter()
            .map(|&i| {
                let raw = cells.get(i).map(String::as_str).unwrap_or("");
                let raw = if raw.is_empty() { defaults.get(i).map(String::as_str).unwrap_or("") } else { raw };
                typed(raw, types.get(i).map(String::as_str).unwrap_or(""))
            })
            .collect();
        t.rows.push(row);
    }
    Ok(tables)
}

fn typed(raw: &str, datatype: &str) -> J {
    if raw.is_empty() && datatype != "string" {
        return J::Null;
    }
    match datatype {
        "long" => raw.parse::<i64>().map_or_else(|_| raw.into(), json_i64),
        "unsignedLong" => raw.parse::<u64>().map_or_else(|_| raw.into(), json_u64),
        "double" => raw.parse::<f64>().map_or_else(|_| raw.into(), json_f64),
        "boolean" => J::Bool(raw == "true"),
        t if t.starts_with("dateTime") => crate::http::iso_time(raw).unwrap_or_else(|| raw.to_string()).into(),
        _ => raw.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SAMPLE: &str = "#datatype,string,long,dateTime:RFC3339,long,string,string\r\n\
#group,false,false,false,false,true,true\r\n\
#default,_result,,,,,\r\n\
,result,table,_time,_value,_field,host\r\n\
,,0,2024-01-31T13:45:00Z,3,n,a\r\n\
,,1,2024-01-31T13:46:00Z,,n,\"b,c\"\r\n\
\r\n\
#datatype,string,long,dateTime:RFC3339,double,string,string\r\n\
#group,false,false,false,false,true,true\r\n\
#default,_result,,,,,\r\n\
,result,table,_time,_value,_field,host\r\n\
,,2,2024-01-31T13:45:00.5Z,1.5,value,a\r\n\
\r\n";

    #[test]
    fn tables_split_on_schema_changes() {
        let t = parse(SAMPLE).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].columns, ["table", "_time", "_value", "_field", "host"]);
        assert_eq!(t[0].rows[0], vec![json!(0), json!("2024-01-31 13:45:00"), json!(3), json!("n"), json!("a")]);
        // Empty long is null; a quoted comma stays in the cell.
        assert_eq!(t[0].rows[1][2], J::Null);
        assert_eq!(t[0].rows[1][4], json!("b,c"));
        assert_eq!(t[1].rows[0][1], json!("2024-01-31 13:45:00.500"));
        assert_eq!(t[1].rows[0][2], json!(1.5));
    }

    #[test]
    fn same_schema_tables_share_a_result() {
        let block = "#datatype,string,long,string\n#group,false,false,true\n#default,_result,,\n,result,table,_value\n,,0,cpu\n\n";
        let t = parse(&format!("{block}{block}")).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].rows.len(), 2);
    }

    #[test]
    fn mid_stream_errors() {
        let e = parse("#datatype,string,string\n#group,true,true\n#default,,\n,error,reference\n,\"boom, it failed\",897\n")
            .unwrap_err();
        assert_eq!(e, "boom, it failed");
    }

    #[test]
    fn quoted_newlines_and_quotes() {
        let r = records("a,\"x\ny \"\"z\"\"\",c\n\nd\n");
        assert_eq!(r[0], Some(vec!["a".to_string(), "x\ny \"z\"".into(), "c".into()]));
        assert_eq!(r[1], None);
        assert_eq!(r[2], Some(vec!["d".to_string()]));
    }

    #[test]
    fn empty_response_is_no_tables() {
        assert!(parse("\r\n").unwrap().is_empty());
    }
}
