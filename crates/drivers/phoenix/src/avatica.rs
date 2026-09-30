//! "Apache Calcite Avatica": any server that speaks Avatica over HTTP
//! (JSON or protobuf), such as Calcite's own server, Apache Druid
//! (`/druid/v2/sql/avatica/`) or a Phoenix Query Server used generically.
//! The wire protocol is the Phoenix one; what changes is the catalog, which
//! comes from Avatica's metadata calls (`getTables`, `getColumns`) instead
//! of Phoenix's `SYSTEM.CATALOG`, and the plan, from `EXPLAIN PLAN FOR`
//! (Calcite's indented operator tree). DDL, the table designer and the
//! monitor depend on the database behind the server, so they're left out.

use crate::{text, PhoenixSession, Req, Resp};
use dbine_driver::sql::split_statements;
use dbine_driver::{
    kinds, ColumnDef, ColumnInfo, DbObject, DriverInfo, Family, Field, FieldKind, Language, ObjectKindInfo, ObjectRef, Plan,
    PlanNode, QueryOutcome, Result, TableSchema,
};
use serde_json::Value;
use std::collections::HashMap;

pub(crate) fn info() -> DriverInfo {
    DriverInfo {
        id: "avatica",
        name: "Apache Calcite Avatica",
        family: Family::Relational,
        language: Language::Sql,
        dialect: "standard",
        default_port: 8765,
        fields: vec![
            Field::host(),
            Field::port().placeholder("8765"),
            Field::new("path", "Ruta del servidor", FieldKind::Text)
                .placeholder("/")
                .help("La ruta HTTP del servidor Avatica: «/» en Calcite y Phoenix, «/druid/v2/sql/avatica/» en Druid.")
                .advanced(),
            Field::username(),
            Field::password(),
            Field::encrypt(),
            Field::trust_cert(),
            Field::new(
                "serialization",
                "Serialización",
                FieldKind::Select(vec![("json", "JSON (predeterminada)"), ("protobuf", "Protobuf")]),
            )
            .default_value("json")
            .help("La del servidor (en Druid, «/druid/v2/sql/avatica-protobuf/» usa protobuf).")
            .advanced(),
            Field::read_only(),
        ],
        databases_label: "",
        has_schemas: true,
        object_kinds: vec![ObjectKindInfo::tables(), ObjectKindInfo::views()],
    }
}

/// `/druid/v2/sql/avatica` → `/druid/v2/sql/avatica/`; empty → `/`.
pub(crate) fn url_path(path: Option<&str>) -> String {
    let p = path.unwrap_or("").trim().trim_matches('/');
    if p.is_empty() {
        "/".into()
    } else {
        format!("/{p}/")
    }
}

/// Rows of a result as maps by upper-case column name.
fn named(out: QueryOutcome) -> Vec<HashMap<String, Value>> {
    let Some(r) = out.results.into_iter().next() else { return Vec::new() };
    let names: Vec<String> = r.columns.iter().map(|c| c.name.to_ascii_uppercase()).collect();
    r.rows.into_iter().map(|row| names.iter().cloned().zip(row).collect()).collect()
}

fn s(row: &HashMap<String, Value>, k: &str) -> String {
    row.get(k).map(text).unwrap_or_default()
}

fn kind_of(table_type: &str) -> Option<&'static str> {
    match table_type.to_ascii_uppercase().as_str() {
        "TABLE" | "BASE TABLE" => Some(kinds::TABLE),
        "VIEW" => Some(kinds::VIEW),
        _ => None,
    }
}

/// A column's type as DDL would write it: `VARCHAR(20)`, `DECIMAL(10,2)`.
fn type_text(row: &HashMap<String, Value>) -> String {
    let base = s(row, "TYPE_NAME");
    if base.contains('(') {
        return base;
    }
    let size = s(row, "COLUMN_SIZE");
    let scale = s(row, "DECIMAL_DIGITS");
    match base.to_ascii_uppercase().as_str() {
        "VARCHAR" | "CHAR" | "CHARACTER" | "CHARACTER VARYING" | "BINARY" | "VARBINARY" if !size.is_empty() && size != "0" => {
            format!("{base}({size})")
        }
        "DECIMAL" | "NUMERIC" if !size.is_empty() && size != "0" => {
            if scale.is_empty() || scale == "0" {
                format!("{base}({size})")
            } else {
                format!("{base}({size},{scale})")
            }
        }
        _ => base,
    }
}

/// Calcite's `EXPLAIN PLAN FOR` text: one operator per line, children
/// indented two spaces below their parent.
pub(crate) fn plan_from_indented(statement: &str, raw: &str) -> Plan {
    fn node(line: &str) -> PlanNode {
        let l = line.trim();
        let (op, detail) = match l.find('(') {
            Some(i) if l.ends_with(')') => (&l[..i], &l[i + 1..l.len() - 1]),
            _ => (l, ""),
        };
        let mut n = PlanNode { op: op.to_string(), ..Default::default() };
        for part in split_top(detail) {
            let (k, v) = part.split_once('=').unwrap_or(("", part));
            match k.trim() {
                "rowcount" => n.est_rows = v.trim().parse().ok(),
                "table" => n.object = Some(v.trim().trim_matches(|c| c == '[' || c == ']').replace("], [", ".").replace(", ", ".")),
                _ => {}
            }
            if !part.trim().is_empty() {
                n.props.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        n
    }
    // (indent, node) stack: pop into the parent when the indent goes back.
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    for line in &lines {
        let indent = line.len() - line.trim_start().len();
        while stack.len() > 1 && stack.last().is_some_and(|(i, _)| *i >= indent) {
            let (_, child) = stack.pop().unwrap_or_default();
            if let Some((_, parent)) = stack.last_mut() {
                parent.children.push(child);
            }
        }
        stack.push((indent, node(line)));
    }
    while stack.len() > 1 {
        let (_, child) = stack.pop().unwrap_or_default();
        if let Some((_, parent)) = stack.last_mut() {
            parent.children.push(child);
        }
    }
    let root = stack.pop().map(|(_, n)| n).unwrap_or_else(|| PlanNode { op: "PLAN".into(), ..Default::default() });
    Plan { statement: statement.to_string(), root, actual: false, raw_format: "text".into(), raw: raw.to_string() }
}

/// `a=[x, y], b=[z]` split at the commas outside brackets and parentheses.
fn split_top(s: &str) -> Vec<&str> {
    let (mut depth, mut start, mut out) = (0i32, 0, Vec::new());
    for (i, c) in s.char_indices() {
        match c {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        out.push(s[start..].trim());
    }
    out
}

impl PhoenixSession {
    async fn metadata(&mut self, req: Req) -> Result<Vec<HashMap<String, Value>>> {
        let mut out = QueryOutcome::default();
        self.run_req(req, 1_000_000, &mut out).await?;
        Ok(named(out))
    }

    pub(crate) async fn avatica_version(&mut self) -> Result<String> {
        let props = match self.client.call(Req::DatabaseProperties).await? {
            Resp::Properties(p) => p,
            _ => Vec::new(),
        };
        let get = |k: &str| props.iter().find(|(n, _)| n.contains(k)).map(|(_, v)| text(v)).unwrap_or_default();
        let (name, version) = (get("PRODUCT_NAME"), get("PRODUCT_VERSION"));
        Ok(format!("Avatica: {} {}", if name.is_empty() { "servidor" } else { &name }, version).trim().to_string())
    }

    pub(crate) async fn avatica_objects(&mut self) -> Result<Vec<DbObject>> {
        let mut out: Vec<DbObject> = self
            .metadata(Req::Tables)
            .await?
            .iter()
            .filter_map(|r| {
                Some(DbObject {
                    kind: kind_of(&s(r, "TABLE_TYPE"))?.into(),
                    schema: Some(s(r, "TABLE_SCHEM")).filter(|x| !x.is_empty()),
                    name: s(r, "TABLE_NAME"),
                    parent: None,
                })
            })
            .collect();
        out.sort_by(|a, b| (&a.schema, &a.name).cmp(&(&b.schema, &b.name)));
        Ok(out)
    }

    pub(crate) async fn avatica_columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let mut rows = self
            .metadata(Req::Columns { schema: obj.schema().map(str::to_string), table: Some(obj.name.clone()) })
            .await?;
        // The patterns are LIKE patterns: keep the exact names only.
        rows.retain(|r| s(r, "TABLE_NAME") == obj.name && obj.schema().is_none_or(|sc| s(r, "TABLE_SCHEM") == sc));
        rows.sort_by_key(|r| s(r, "ORDINAL_POSITION").parse::<i64>().unwrap_or(0));
        Ok(rows
            .iter()
            .map(|r| ColumnInfo {
                name: s(r, "COLUMN_NAME"),
                data_type: type_text(r),
                // java.sql.DatabaseMetaData.columnNoNulls = 0
                nullable: s(r, "NULLABLE") != "0",
                primary_key: false,
                auto_increment: s(r, "IS_AUTOINCREMENT") == "YES",
                default_value: Some(s(r, "COLUMN_DEF")).filter(|d| !d.is_empty()),
            })
            .collect())
    }

    pub(crate) async fn avatica_schema(&mut self) -> Result<Vec<TableSchema>> {
        let mut tables: Vec<TableSchema> = self
            .avatica_objects()
            .await?
            .into_iter()
            .filter(|o| o.kind == kinds::TABLE)
            .map(|o| TableSchema { kind: o.kind, schema: o.schema, name: o.name, ..Default::default() })
            .collect();
        let index: HashMap<(String, String), usize> =
            tables.iter().enumerate().map(|(i, t)| ((t.schema.clone().unwrap_or_default(), t.name.clone()), i)).collect();
        let mut rows = self.metadata(Req::Columns { schema: None, table: None }).await?;
        rows.sort_by_key(|r| s(r, "ORDINAL_POSITION").parse::<i64>().unwrap_or(0));
        for r in &rows {
            if let Some(&i) = index.get(&(s(r, "TABLE_SCHEM"), s(r, "TABLE_NAME"))) {
                tables[i].columns.push(ColumnDef {
                    name: s(r, "COLUMN_NAME"),
                    data_type: type_text(r),
                    nullable: s(r, "NULLABLE") != "0",
                    default_value: Some(s(r, "COLUMN_DEF")).filter(|d| !d.is_empty()),
                    comment: Some(s(r, "REMARKS")).filter(|d| !d.is_empty()),
                    ..Default::default()
                });
            }
        }
        Ok(tables)
    }

    /// `EXPLAIN PLAN FOR` each query; with `analyze`, the statements also
    /// run (Avatica has no measured plans).
    pub(crate) async fn avatica_explain(&mut self, script: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let mut noted = false;
        for stmt in split_statements(script) {
            let first = stmt.split_whitespace().next().unwrap_or_default().to_ascii_uppercase();
            if matches!(first.as_str(), "SELECT" | "WITH" | "INSERT" | "UPDATE" | "DELETE" | "UPSERT" | "VALUES") {
                let mut local = QueryOutcome::default();
                self.run(&format!("EXPLAIN PLAN FOR {stmt}"), 10_000, &mut local).await?;
                let raw: Vec<String> =
                    local.results.pop().map(|r| r.rows.iter().filter_map(|row| row.first().map(text)).collect()).unwrap_or_default();
                out.plans.push(plan_from_indented(&stmt, &raw.join("\n")));
                if analyze && !noted {
                    out.messages.push("Avatica no da cifras reales por paso: se muestra el plan estimado junto al resultado.".into());
                    noted = true;
                }
            } else if !analyze {
                out.messages.push(format!("Sin plan para «{}».", stmt.trim()));
            }
            if analyze {
                self.run(&stmt, max_rows, out).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_types() {
        assert_eq!(url_path(None), "/");
        assert_eq!(url_path(Some(" / ")), "/");
        assert_eq!(url_path(Some("druid/v2/sql/avatica")), "/druid/v2/sql/avatica/");
        let row = |t: &str, size: &str, scale: &str| -> HashMap<String, Value> {
            [("TYPE_NAME", t), ("COLUMN_SIZE", size), ("DECIMAL_DIGITS", scale)].iter().map(|(k, v)| (k.to_string(), Value::from(*v))).collect()
        };
        assert_eq!(type_text(&row("VARCHAR", "20", "")), "VARCHAR(20)");
        assert_eq!(type_text(&row("DECIMAL", "10", "2")), "DECIMAL(10,2)");
        assert_eq!(type_text(&row("INTEGER", "10", "0")), "INTEGER");
        assert_eq!(kind_of("SYSTEM TABLE"), None);
        assert_eq!(kind_of("VIEW"), Some(kinds::VIEW));
    }

    #[test]
    fn calcite_plans_by_indent() {
        let raw = "EnumerableCalc(expr#0..2=[{inputs}], proj#0..1=[{exprs}])\n  EnumerableFilter(condition=[>($0, 1)])\n    EnumerableTableScan(table=[[hr, emps]])\n  EnumerableValues(tuples=[[{ 1 }]])";
        let p = plan_from_indented("SELECT 1", raw);
        assert_eq!(p.root.op, "EnumerableCalc");
        assert_eq!(p.root.children.len(), 2);
        assert_eq!(p.root.children[0].op, "EnumerableFilter");
        let scan = &p.root.children[0].children[0];
        assert_eq!(scan.op, "EnumerableTableScan");
        assert_eq!(scan.object.as_deref(), Some("hr.emps"));
        assert_eq!(p.root.children[1].op, "EnumerableValues");
        assert_eq!(split_top("a=[x, y], b=(1, 2), c=3"), vec!["a=[x, y]", "b=(1, 2)", "c=3"]);
    }
}
