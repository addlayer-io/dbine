//! Tier A: the `INFO` functions over `executeDaxQueries` (spec §4.3). Owned
//! by part C.
//!
//! - One request with `INFO.VIEW.TABLES()`, `INFO.VIEW.COLUMNS()`,
//!   `INFO.VIEW.MEASURES()`, `INFO.VIEW.RELATIONSHIPS()`.
//! - A second, tolerated if refused (model admin / write): `INFO.HIERARCHIES`,
//!   `INFO.LEVELS`, `INFO.CALCULATIONGROUPS`, `INFO.CALCULATIONITEMS`,
//!   `INFO.ROLES`, `INFO.TABLEPERMISSIONS`, `INFO.COLUMNPERMISSIONS`,
//!   `INFO.ROLEMEMBERSHIPS`, `INFO.PERSPECTIVE*`, `INFO.PARTITIONS`,
//!   `INFO.EXPRESSIONS`, `INFO.USERDEFINEDFUNCTIONS`, `INFO.STORAGETABLES`,
//!   `INFO.MODEL`; on failure one at a time, once per model, remembering
//!   which work.
//! - Calculation items without admin: `EVALUATE 'Group'`.
//!
//! The `INFO.*` functions join by ids: `INFO.TABLES`, `INFO.COLUMNS` and
//! `INFO.MEASURES` (same request) map them to names, else the `INFO.VIEW`
//! ids are used.

use super::rows::Rows;
use super::service::{session_model, Service};
use super::{
    camel, is_auto_date, normalize_relationship, redact_m, CalculationGroup, CalculationItem,
    Column, DataType, Function, Hierarchy, Level, Measure, ModelMeta, NamedExpression, Partition,
    Perspective, Relationship, Role, Table, TableKind, Tier,
};
use crate::catalog::Model;
use crate::dax::quote::quote_table;
use crate::query::{ApiKind, QueryOpts, Rowset, Target};
use crate::Ctx;
use dbine_driver::{Error, Result};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// One row of `INFO.CALCDEPENDENCY()` (write permission).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CalcDependency {
    pub object_type: String,
    pub table: Option<String>,
    pub object: String,
    pub referenced_object_type: String,
    pub referenced_table: Option<String>,
    pub referenced_object: String,
    /// The dependent's DAX (where the mentions are pointed at).
    pub expression: Option<String>,
}

/// What any reader of the model may run (a live check: Learn says INFO
/// functions need model admin, and only hides some `INFO.VIEW` columns
/// without write permission).
const VIEW: &str = "EVALUATE INFO.VIEW.TABLES()\nEVALUATE INFO.VIEW.COLUMNS()\nEVALUATE INFO.VIEW.MEASURES()\nEVALUATE INFO.VIEW.RELATIONSHIPS()";

/// The model admin's functions, in the order they're asked.
pub const ADMIN: &[&str] = &[
    "INFO.TABLES",
    "INFO.COLUMNS",
    "INFO.MEASURES",
    "INFO.HIERARCHIES",
    "INFO.LEVELS",
    "INFO.CALCULATIONGROUPS",
    "INFO.CALCULATIONITEMS",
    "INFO.ROLES",
    "INFO.TABLEPERMISSIONS",
    "INFO.COLUMNPERMISSIONS",
    "INFO.ROLEMEMBERSHIPS",
    "INFO.PERSPECTIVES",
    "INFO.PERSPECTIVETABLES",
    "INFO.PERSPECTIVECOLUMNS",
    "INFO.PERSPECTIVEMEASURES",
    "INFO.PARTITIONS",
    "INFO.EXPRESSIONS",
    "INFO.USERDEFINEDFUNCTIONS",
    "INFO.STORAGETABLES",
    "INFO.MODEL",
];

/// Which admin functions work, per account and model, for the host's life.
static WORKS: Mutex<BTreeMap<String, Vec<&'static str>>> = Mutex::new(BTreeMap::new());

fn arrow() -> QueryOpts {
    QueryOpts {
        api: Some(ApiKind::Arrow),
        ..QueryOpts::metadata()
    }
}

fn evaluate(functions: &[&str]) -> String {
    functions
        .iter()
        .map(|f| format!("EVALUATE {f}()"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn remembered<S: Service>(s: &mut S, m: &Model) -> Option<Vec<&'static str>> {
    match super::host_key(s, m) {
        Some(k) => WORKS.lock().ok()?.get(&k).cloned(),
        None => s.meta_state().admin_works.get(&m.id).cloned(),
    }
}

fn remember<S: Service>(s: &mut S, m: &Model, works: Option<Vec<&'static str>>) {
    match super::host_key(s, m) {
        Some(k) => {
            if let Ok(mut w) = WORKS.lock() {
                match works {
                    Some(v) => w.insert(k, v),
                    None => w.remove(&k),
                };
            }
        }
        None => {
            let st = s.meta_state();
            match works {
                Some(v) => st.admin_works.insert(m.id.clone(), v),
                None => st.admin_works.remove(&m.id),
            };
        }
    }
}

/// The admin functions' result sets the user may read, by function.
async fn admin_sets<S: Service>(s: &mut S, m: &Model) -> Result<BTreeMap<&'static str, Rowset>> {
    let target = Target::of(m);
    let known = remembered(s, m);
    let list: Vec<&'static str> = known.clone().unwrap_or_else(|| ADMIN.to_vec());
    if list.is_empty() {
        return Ok(BTreeMap::new());
    }
    match s.dax(&target, &evaluate(&list), &arrow()).await {
        Ok(sets) if sets.len() == list.len() => {
            if known.is_none() {
                remember(s, m, Some(list.clone()));
            }
            return Ok(list.into_iter().zip(sets).collect());
        }
        Err(e) if super::is_fatal(&e) => return Err(e),
        _ => {}
    }
    if known.is_some() {
        // What worked before doesn't now (permissions changed): ask again
        // next time.
        remember(s, m, None);
        return Ok(BTreeMap::new());
    }
    // One cheap probe before asking each: a viewer is refused them all.
    match s.dax(&target, &evaluate(&["INFO.MODEL"]), &arrow()).await {
        Err(e) if super::is_fatal(&e) => return Err(e),
        Err(_) => {
            remember(s, m, Some(Vec::new()));
            return Ok(BTreeMap::new());
        }
        Ok(_) => {}
    }
    let mut out = BTreeMap::new();
    for f in ADMIN {
        match s.dax(&target, &evaluate(&[f]), &arrow()).await {
            Ok(mut sets) if !sets.is_empty() => {
                out.insert(*f, sets.remove(0));
            }
            Err(e) if super::is_fatal(&e) => return Err(e),
            _ => {}
        }
    }
    remember(s, m, Some(out.keys().copied().collect()));
    Ok(out)
}

pub async fn read(cx: &mut Ctx) -> Result<ModelMeta> {
    let m = crate::catalog::model(cx)?.clone();
    read_model(cx, &m).await
}

/// `INFO.CALCDEPENDENCY()`, on demand (dependencies).
pub async fn calc_dependencies(cx: &mut Ctx) -> Result<Vec<CalcDependency>> {
    let m = session_model(cx)?;
    calc_dependencies_of(cx, &m).await
}

pub(crate) async fn calc_dependencies_of<S: Service>(
    s: &mut S,
    m: &Model,
) -> Result<Vec<CalcDependency>> {
    let sets = s
        .dax(&Target::of(m), "EVALUATE INFO.CALCDEPENDENCY()", &arrow())
        .await?;
    let Some(set) = sets.first() else {
        return Ok(Vec::new());
    };
    let rows = Rows::new(set);
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some(CalcDependency {
                object_type: r.str("OBJECT_TYPE")?,
                table: r.str("TABLE"),
                object: r.str("OBJECT")?,
                referenced_object_type: r.str("REFERENCED_OBJECT_TYPE")?,
                referenced_table: r.str("REFERENCED_TABLE"),
                referenced_object: r.str("REFERENCED_OBJECT")?,
                expression: r.str("EXPRESSION"),
            })
        })
        .collect())
}

/// Tier A for `m`.
pub(crate) async fn read_model<S: Service>(s: &mut S, m: &Model) -> Result<ModelMeta> {
    let target = Target::of(m);
    let view = s.dax(&target, VIEW, &arrow()).await?;
    if view.len() < 4 {
        return Err(Error::Query(
            "INFO.VIEW devolvió menos resultados de los pedidos".into(),
        ));
    }
    let admin = admin_sets(s, m).await?;
    let mut meta = build(&view, &admin);
    if admin.is_empty() {
        meta.notes.push(
            "Sin permiso de administrador del modelo, las funciones INFO no muestran jerarquías, roles, perspectivas, particiones, expresiones ni funciones del modelo; de los grupos de cálculo se ven solo sus elementos.".into(),
        );
        // Calculation items: any reader may list a group's table.
        let groups: Vec<String> = meta
            .calculation_groups
            .iter()
            .map(|g| g.table.clone())
            .collect();
        if !groups.is_empty() {
            let dax = groups
                .iter()
                .map(|g| format!("EVALUATE {}", quote_table(g)))
                .collect::<Vec<_>>()
                .join("\n");
            match s.dax(&target, &dax, &QueryOpts::metadata()).await {
                Ok(sets) => {
                    for (g, set) in meta.calculation_groups.iter_mut().zip(&sets) {
                        g.items = items_of_table(set);
                    }
                }
                Err(e) if super::is_fatal(&e) => return Err(e),
                Err(_) => {}
            }
        }
    }
    if !meta.measures.is_empty() && meta.measures.iter().all(|x| x.expression.is_none()) {
        meta.notes.push(
            "Sin permiso de escritura en el modelo, INFO.VIEW no devuelve las expresiones: las medidas y las tablas calculadas se ven sin su DAX.".into(),
        );
    }
    Ok(meta)
}

/// The items a calculation group's table lists (`EVALUATE 'Group'`): its
/// name column and `Ordinal`.
fn items_of_table(set: &Rowset) -> Vec<CalculationItem> {
    let rows = Rows::new(set);
    let name_col = set
        .columns
        .iter()
        .map(|c| super::rows::bare(c).to_string())
        .find(|c| !c.eq_ignore_ascii_case("Ordinal"));
    let Some(name_col) = name_col else {
        return Vec::new();
    };
    let mut items: Vec<CalculationItem> = rows
        .iter()
        .filter_map(|r| {
            Some(CalculationItem {
                name: r.str(&name_col)?,
                ordinal: r.i64("Ordinal"),
                ..Default::default()
            })
        })
        .collect();
    items.sort_by_key(|i| i.ordinal.unwrap_or(i64::MAX));
    items
}

fn opt(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.trim().is_empty())
}

/// `ModelPermission` codes of `INFO.ROLES`.
fn model_permission(code: Option<String>) -> Option<String> {
    let c = code?;
    Some(
        match c.as_str() {
            "1" => "none",
            "2" => "read",
            "3" => "readRefresh",
            "4" => "refresh",
            "5" => "administrator",
            other => return Some(camel(other)),
        }
        .to_string(),
    )
}

/// `MetadataPermission` codes of `INFO.COLUMNPERMISSIONS`: TOM's
/// `MetadataPermission` enum (`Default = 0`, `None = 1`, `Read = 2`).
fn metadata_permission(code: Option<String>) -> String {
    match code.as_deref() {
        Some("0") => "default".into(),
        Some("1") => "none".into(),
        Some("2") => "read".into(),
        Some(other) => camel(other),
        None => "default".into(),
    }
}

/// `Type` codes of `INFO.PARTITIONS`.
fn partition_kind(code: Option<String>) -> Option<String> {
    let c = code?;
    Some(
        match c.as_str() {
            "1" => "query",
            "2" => "calculated",
            "3" => "none",
            "4" => "m",
            "5" => "entity",
            "6" => "policyRange",
            "7" => "calculationGroup",
            "8" => "inferred",
            other => return Some(camel(other)),
        }
        .to_string(),
    )
}

/// `Mode` codes of `INFO.PARTITIONS`.
fn partition_mode(code: Option<String>) -> Option<String> {
    let c = code?;
    Some(
        match c.as_str() {
            "0" => "import",
            "1" => "directQuery",
            "2" => "default",
            "3" => "push",
            "4" => "dual",
            "5" => "directLake",
            other => return Some(camel(other)),
        }
        .to_string(),
    )
}

/// The model from the `INFO.VIEW` result sets and whatever admin sets came
/// back.
fn build(view: &[Rowset], admin: &BTreeMap<&'static str, Rowset>) -> ModelMeta {
    let mut meta = ModelMeta {
        tier: Tier::Info,
        ..Default::default()
    };
    let set = |k: &str| admin.get(k).map(Rows::new);

    // Ids → names.
    let mut table_ids: BTreeMap<i64, String> = BTreeMap::new();
    let mut column_ids: BTreeMap<i64, (String, String)> = BTreeMap::new();
    let mut measure_ids: BTreeMap<i64, (String, String)> = BTreeMap::new();
    let vt = Rows::new(&view[0]);
    let vc = Rows::new(&view[1]);
    let vm = Rows::new(&view[2]);
    let vr = Rows::new(&view[3]);
    match set("INFO.TABLES") {
        Some(t) => {
            for r in t.iter() {
                if let (Some(id), Some(n)) = (r.i64("ID"), r.str("Name")) {
                    table_ids.insert(id, n);
                }
            }
        }
        None => {
            for r in vt.iter() {
                if let (Some(id), Some(n)) = (r.i64("ID"), r.str("Name")) {
                    table_ids.insert(id, n);
                }
            }
        }
    }
    match set("INFO.COLUMNS") {
        Some(c) => {
            for r in c.iter() {
                let name = r.str("ExplicitName").or_else(|| r.str("InferredName"));
                if let (Some(id), Some(t), Some(n)) = (r.i64("ID"), r.i64("TableID"), name) {
                    if let Some(t) = table_ids.get(&t) {
                        column_ids.insert(id, (t.clone(), n));
                    }
                }
            }
        }
        None => {
            for r in vc.iter() {
                if let (Some(id), Some(t), Some(n)) = (r.i64("ID"), r.str("Table"), r.str("Name")) {
                    column_ids.insert(id, (t, n));
                }
            }
        }
    }
    match set("INFO.MEASURES") {
        Some(ms) => {
            for r in ms.iter() {
                if let (Some(id), Some(t), Some(n)) = (r.i64("ID"), r.i64("TableID"), r.str("Name"))
                {
                    if let Some(t) = table_ids.get(&t) {
                        measure_ids.insert(id, (t.clone(), n));
                    }
                }
            }
        }
        None => {
            for r in vm.iter() {
                if let (Some(id), Some(t), Some(n)) = (r.i64("ID"), r.str("Table"), r.str("Name")) {
                    measure_ids.insert(id, (t, n));
                }
            }
        }
    }

    // Calculation groups (by table).
    let mut group_tables: BTreeMap<String, CalculationGroup> = BTreeMap::new();
    if let Some(cg) = set("INFO.CALCULATIONGROUPS") {
        let items = set("INFO.CALCULATIONITEMS");
        for r in cg.iter() {
            let Some(table) = r.i64("TableID").and_then(|t| table_ids.get(&t)) else {
                continue;
            };
            let id = r.i64("ID");
            let mut its: Vec<CalculationItem> = items
                .as_ref()
                .map(|it| {
                    it.iter()
                        .filter(|i| i.i64("CalculationGroupID") == id)
                        .filter_map(|i| {
                            Some(CalculationItem {
                                name: i.str("Name")?,
                                expression: opt(i.str("Expression")),
                                ordinal: i.i64("Ordinal"),
                                format_string_expression: None,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            its.sort_by_key(|i| i.ordinal.unwrap_or(i64::MAX));
            group_tables.insert(
                table.to_lowercase(),
                CalculationGroup {
                    table: table.clone(),
                    precedence: r.i64("Precedence"),
                    items: its,
                    description: opt(r.str("Description")),
                },
            );
        }
    }

    // Partitions and row counts (admin).
    let mut partitions: BTreeMap<String, Vec<Partition>> = BTreeMap::new();
    if let Some(p) = set("INFO.PARTITIONS") {
        for r in p.iter() {
            let Some(table) = r.i64("TableID").and_then(|t| table_ids.get(&t)) else {
                continue;
            };
            let kind = partition_kind(r.str("Type"));
            let expression = opt(r.str("QueryDefinition")).map(|q| match kind.as_deref() {
                Some("calculated") => q,
                _ => redact_m(&q),
            });
            partitions
                .entry(table.to_lowercase())
                .or_default()
                .push(Partition {
                    name: r.str("Name").unwrap_or_default(),
                    source_kind: kind,
                    expression,
                    mode: partition_mode(r.str("Mode")),
                });
        }
    }
    let mut row_counts: BTreeMap<String, u64> = BTreeMap::new();
    if let Some(st) = set("INFO.STORAGETABLES") {
        for r in st.iter() {
            let id = r.str("TABLE_ID").unwrap_or_default();
            if id.starts_with("H$") || id.starts_with("R$") || id.starts_with("U$") {
                continue;
            }
            if let (Some(t), Some(n)) = (r.str("DIMENSION_NAME"), r.u64("ROWS_COUNT")) {
                let e = row_counts.entry(t.to_lowercase()).or_insert(0);
                *e = (*e).max(n);
            }
        }
    }

    // Columns by table.
    let mut columns: BTreeMap<String, Vec<Column>> = BTreeMap::new();
    for r in vc.iter() {
        let (Some(table), Some(name)) = (r.str("Table"), r.str("Name")) else {
            continue;
        };
        let kind = r.str("Type").unwrap_or_default();
        if kind.eq_ignore_ascii_case("RowNumber") || name.starts_with("RowNumber-") {
            continue;
        }
        let calculated = kind.to_ascii_lowercase().starts_with("calculated")
            && !kind.eq_ignore_ascii_case("CalculatedTableColumn");
        columns
            .entry(table.to_lowercase())
            .or_default()
            .push(Column {
                name,
                data_type: r
                    .str("DataType")
                    .map(|d| DataType::parse(&d))
                    .unwrap_or_default(),
                expression: if calculated {
                    opt(r.str("Expression"))
                } else {
                    None
                },
                description: opt(r.str("Description")),
                hidden: r.bool("IsHidden"),
                key: r.bool("IsKey"),
                unique: r.bool("IsUnique"),
                nullable: r.value("IsNullable").is_none() || r.bool("IsNullable"),
                cardinality: None,
                format_string: opt(r.str("FormatString")),
            });
    }

    // Tables.
    for r in vt.iter() {
        let Some(name) = r.str("Name") else {
            continue;
        };
        let key = name.to_lowercase();
        let expression = opt(r.str("Expression"));
        let parts = partitions.remove(&key).unwrap_or_default();
        let precedence = r.i64("CalculationGroupPrecedence");
        let kind = if is_auto_date(&name) {
            TableKind::AutoDate
        } else if group_tables.contains_key(&key) || precedence.is_some() {
            TableKind::CalculationGroup
        } else if expression.is_some()
            || parts
                .iter()
                .any(|p| p.source_kind.as_deref() == Some("calculated"))
        {
            TableKind::Calculated
        } else {
            TableKind::Data
        };
        let expression = expression.or_else(|| {
            parts
                .iter()
                .find(|p| p.source_kind.as_deref() == Some("calculated"))
                .and_then(|p| p.expression.clone())
        });
        if kind == TableKind::CalculationGroup && !group_tables.contains_key(&key) {
            group_tables.insert(
                key.clone(),
                CalculationGroup {
                    table: name.clone(),
                    precedence,
                    description: opt(r.str("Description")),
                    items: Vec::new(),
                },
            );
        }
        meta.tables.push(Table {
            columns: columns.remove(&key).unwrap_or_default(),
            rows: row_counts.get(&key).copied(),
            storage_mode: opt(r.str("StorageMode")).map(|m| camel(&m)),
            name,
            kind,
            description: opt(r.str("Description")),
            hidden: r.bool("IsHidden"),
            expression: if kind == TableKind::Calculated {
                expression
            } else {
                None
            },
            partitions: parts,
        });
    }
    meta.calculation_groups = group_tables.into_values().collect();

    for r in vm.iter() {
        let (Some(table), Some(name)) = (r.str("Table"), r.str("Name")) else {
            continue;
        };
        meta.measures.push(Measure {
            table,
            name,
            expression: opt(r.str("Expression")),
            description: opt(r.str("Description")),
            format_string: opt(r.str("FormatString")),
            display_folder: opt(r.str("DisplayFolder")),
            hidden: r.bool("IsHidden"),
            state: opt(r.str("State")),
        });
    }

    for r in vr.iter() {
        let (Some(ft), Some(fc), Some(tt), Some(tc)) = (
            r.str("FromTable"),
            r.str("FromColumn"),
            r.str("ToTable"),
            r.str("ToColumn"),
        ) else {
            continue;
        };
        meta.relationships
            .push(normalize_relationship(Relationship {
                name: opt(r.str("Name")),
                from_table: ft,
                from_column: fc,
                to_table: tt,
                to_column: tc,
                from_cardinality: opt(r.str("FromCardinality")),
                to_cardinality: opt(r.str("ToCardinality")),
                cross_filtering: opt(r.str("CrossFilteringBehavior")),
                active: r.value("IsActive").is_none() || r.bool("IsActive"),
                security_filtering: opt(r.str("SecurityFilteringBehavior")),
            }));
    }

    if let Some(h) = set("INFO.HIERARCHIES") {
        let levels = set("INFO.LEVELS");
        for r in h.iter() {
            let (Some(table), Some(name)) = (
                r.i64("TableID").and_then(|t| table_ids.get(&t)),
                r.str("Name"),
            ) else {
                continue;
            };
            let id = r.i64("ID");
            let mut lv: Vec<(i64, Level)> = levels
                .as_ref()
                .map(|l| {
                    l.iter()
                        .filter(|x| x.i64("HierarchyID") == id)
                        .filter_map(|x| {
                            let column = x
                                .i64("ColumnID")
                                .and_then(|c| column_ids.get(&c))
                                .map(|(_, c)| c.clone())?;
                            Some((
                                x.i64("Ordinal").unwrap_or(0),
                                Level {
                                    name: x.str("Name").unwrap_or_else(|| column.clone()),
                                    column,
                                },
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            lv.sort_by_key(|(o, _)| *o);
            meta.hierarchies.push(Hierarchy {
                table: table.clone(),
                name,
                levels: lv.into_iter().map(|(_, l)| l).collect(),
                description: opt(r.str("Description")),
                hidden: r.bool("IsHidden"),
            });
        }
    }

    if let Some(roles) = set("INFO.ROLES") {
        let tp = set("INFO.TABLEPERMISSIONS");
        let cp = set("INFO.COLUMNPERMISSIONS");
        let rm = set("INFO.ROLEMEMBERSHIPS");
        for r in roles.iter() {
            let Some(name) = r.str("Name") else {
                continue;
            };
            let id = r.i64("ID");
            let mut role = Role {
                name,
                description: opt(r.str("Description")),
                model_permission: model_permission(r.str("ModelPermission")),
                ..Default::default()
            };
            if let Some(tp) = &tp {
                for p in tp.iter().filter(|p| p.i64("RoleID") == id) {
                    let Some(table) = p.i64("TableID").and_then(|t| table_ids.get(&t)) else {
                        continue;
                    };
                    if let Some(f) = opt(p.str("FilterExpression")) {
                        role.table_filters.push((table.clone(), f));
                    }
                    let pid = p.i64("ID");
                    if let Some(cp) = &cp {
                        for c in cp.iter().filter(|c| c.i64("TablePermissionID") == pid) {
                            if let Some((t, col)) =
                                c.i64("ColumnID").and_then(|c| column_ids.get(&c))
                            {
                                role.column_permissions.push((
                                    t.clone(),
                                    col.clone(),
                                    metadata_permission(c.str("MetadataPermission")),
                                ));
                            }
                        }
                    }
                }
            }
            if let Some(rm) = &rm {
                role.members = rm
                    .iter()
                    .filter(|x| x.i64("RoleID") == id)
                    .filter_map(|x| x.str("MemberName"))
                    .collect();
            }
            meta.roles.push(role);
        }
    }

    if let Some(ps) = set("INFO.PERSPECTIVES") {
        let pt = set("INFO.PERSPECTIVETABLES");
        let pc = set("INFO.PERSPECTIVECOLUMNS");
        let pm = set("INFO.PERSPECTIVEMEASURES");
        for r in ps.iter() {
            let Some(name) = r.str("Name") else {
                continue;
            };
            let id = r.i64("ID");
            let mut p = Perspective {
                name,
                description: opt(r.str("Description")),
                ..Default::default()
            };
            let tables: Vec<(Option<i64>, String)> = pt
                .as_ref()
                .map(|pt| {
                    pt.iter()
                        .filter(|t| t.i64("PerspectiveID") == id)
                        .filter_map(|t| {
                            Some((t.i64("ID"), table_ids.get(&t.i64("TableID")?)?.clone()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            for (ptid, t) in &tables {
                p.tables.push(t.clone());
                if let Some(pc) = &pc {
                    for c in pc.iter().filter(|c| c.i64("PerspectiveTableID") == *ptid) {
                        if let Some(col) = c.i64("ColumnID").and_then(|c| column_ids.get(&c)) {
                            p.columns.push(col.clone());
                        }
                    }
                }
                if let Some(pm) = &pm {
                    for x in pm.iter().filter(|x| x.i64("PerspectiveTableID") == *ptid) {
                        if let Some(ms) = x.i64("MeasureID").and_then(|c| measure_ids.get(&c)) {
                            p.measures.push(ms.clone());
                        }
                    }
                }
            }
            meta.perspectives.push(p);
        }
    }

    if let Some(ex) = set("INFO.EXPRESSIONS") {
        for r in ex.iter() {
            let Some(name) = r.str("Name") else {
                continue;
            };
            let expression = opt(r.str("Expression"));
            let parameter = expression
                .as_deref()
                .is_some_and(|e| e.replace(' ', "").contains("IsParameterQuery=true"));
            meta.expressions.push(NamedExpression {
                name,
                kind: Some(if parameter { "parameter" } else { "m" }.into()),
                expression: expression.map(|e| redact_m(&e)),
                description: opt(r.str("Description")),
            });
        }
    }

    if let Some(f) = set("INFO.USERDEFINEDFUNCTIONS") {
        for r in f.iter() {
            if let Some(name) = r.str("Name") {
                meta.functions.push(Function {
                    name,
                    expression: opt(r.str("Expression")),
                    description: opt(r.str("Description")),
                });
            }
        }
    }

    if let Some(model) = set("INFO.MODEL") {
        meta.culture = model.iter().next().and_then(|r| opt(r.str("Culture")));
    }
    meta.relationships
        .retain(|r| !r.from_table.is_empty() && !r.to_table.is_empty());
    meta
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::fake::{rowset, Fake};
    use super::*;
    use serde_json::json;

    #[test]
    fn metadata_permission_codes_follow_tom() {
        let p = |c: &str| metadata_permission(Some(c.into()));
        assert_eq!(p("0"), "default");
        assert_eq!(p("1"), "none");
        assert_eq!(p("2"), "read");
        assert_eq!(metadata_permission(None), "default");
    }

    /// `INFO.VIEW.*` as a Read + Build viewer gets it: no expressions.
    pub(crate) fn viewer_answers(f: &mut Fake) {
        f.dax_answer(
            "INFO.VIEW.TABLES()",
            vec![rowset(
                &[
                    "[ID]",
                    "[Name]",
                    "[Model]",
                    "[DataCategory]",
                    "[Description]",
                    "[IsHidden]",
                    "[StorageMode]",
                    "[TableStorage]",
                    "[Expression]",
                    "[ShowAsVariationOnly]",
                    "[IsPrivate]",
                    "[CalculationGroupPrecedence]",
                    "[LineageTag]",
                ],
                vec![
                    json!([
                        12,
                        "Sales",
                        "m1",
                        null,
                        "Ventas por línea",
                        false,
                        "Import",
                        null,
                        null,
                        false,
                        false,
                        null,
                        "a"
                    ]),
                    json!([
                        13, "Customer", "m1", null, null, false, "Import", null, null, false,
                        false, null, "b"
                    ]),
                    json!([
                        14,
                        "Time Intelligence",
                        "m1",
                        null,
                        null,
                        false,
                        "Import",
                        null,
                        null,
                        false,
                        false,
                        1,
                        "c"
                    ]),
                    json!([
                        15,
                        "LocalDateTable_1b2c",
                        "m1",
                        null,
                        null,
                        true,
                        "Import",
                        null,
                        null,
                        true,
                        true,
                        null,
                        "d"
                    ]),
                ],
            )],
        );
        f.dax_answer(
            "INFO.VIEW.COLUMNS()",
            vec![rowset(
                &[
                    "[ID]",
                    "[Name]",
                    "[Table]",
                    "[DataType]",
                    "[DataCategory]",
                    "[Description]",
                    "[IsHidden]",
                    "[IsUnique]",
                    "[IsKey]",
                    "[IsNullable]",
                    "[Type]",
                    "[Expression]",
                    "[FormatString]",
                ],
                vec![
                    json!([
                        100,
                        "RowNumber-2662979B-1795-4F74-8F37-6A1BA8059B61",
                        "Sales",
                        "Whole Number",
                        null,
                        null,
                        true,
                        true,
                        false,
                        false,
                        "RowNumber",
                        null,
                        null
                    ]),
                    json!([
                        101,
                        "CustomerKey",
                        "Sales",
                        "Whole Number",
                        null,
                        null,
                        true,
                        false,
                        false,
                        true,
                        "Data",
                        null,
                        null
                    ]),
                    json!([
                        102,
                        "Amount",
                        "Sales",
                        "Fixed Decimal Number",
                        null,
                        "Importe con IVA",
                        false,
                        false,
                        false,
                        true,
                        "Data",
                        null,
                        "$ #,0.00"
                    ]),
                    json!([
                        103,
                        "Margin",
                        "Sales",
                        "Decimal Number",
                        null,
                        null,
                        false,
                        false,
                        false,
                        true,
                        "Calculated",
                        null,
                        null
                    ]),
                    json!([
                        110,
                        "CustomerKey",
                        "Customer",
                        "Whole Number",
                        null,
                        null,
                        false,
                        true,
                        false,
                        false,
                        "Data",
                        null,
                        null
                    ]),
                    json!([
                        111, "City", "Customer", "Text", null, null, false, false, false, true,
                        "Data", null, null
                    ]),
                    json!([
                        120,
                        "Name",
                        "Time Intelligence",
                        "Text",
                        null,
                        null,
                        false,
                        false,
                        false,
                        true,
                        "Data",
                        null,
                        null
                    ]),
                    json!([
                        121,
                        "Ordinal",
                        "Time Intelligence",
                        "Whole Number",
                        null,
                        null,
                        true,
                        false,
                        false,
                        true,
                        "Data",
                        null,
                        null
                    ]),
                ],
            )],
        );
        f.dax_answer(
            "INFO.VIEW.MEASURES()",
            vec![rowset(
                &[
                    "[ID]",
                    "[Name]",
                    "[Table]",
                    "[Description]",
                    "[DataType]",
                    "[Expression]",
                    "[FormatString]",
                    "[IsHidden]",
                    "[State]",
                    "[DisplayFolder]",
                ],
                vec![
                    json!([
                        200,
                        "Total Sales",
                        "Sales",
                        "Suma de importes",
                        "Fixed Decimal Number",
                        null,
                        "$ #,0",
                        false,
                        "Ready",
                        "KPIs"
                    ]),
                    json!([
                        201,
                        "Broken",
                        "Sales",
                        null,
                        "Variant",
                        null,
                        null,
                        false,
                        "SemanticError",
                        null
                    ]),
                ],
            )],
        );
        f.dax_answer(
            "INFO.VIEW.RELATIONSHIPS()",
            vec![rowset(
                &[
                    "[ID]",
                    "[Name]",
                    "[Relationship]",
                    "[Model]",
                    "[IsActive]",
                    "[CrossFilteringBehavior]",
                    "[RelyOnReferentialIntegrity]",
                    "[FromTable]",
                    "[FromColumn]",
                    "[FromCardinality]",
                    "[ToTable]",
                    "[ToColumn]",
                    "[ToCardinality]",
                    "[State]",
                    "[SecurityFilteringBehavior]",
                ],
                vec![json!([
                    300,
                    "2f0d3c3e-9a4b-4b0e-8b1a-3c1f2e9d7a11",
                    "'Sales'[CustomerKey] *-1 'Customer'[CustomerKey]",
                    "m1",
                    true,
                    "OneDirection",
                    false,
                    "Sales",
                    "CustomerKey",
                    "Many",
                    "Customer",
                    "CustomerKey",
                    "One",
                    "Ready",
                    "OneDirection"
                ])],
            )],
        );
        // Any reader may list a calculation group's items.
        f.dax_answer(
            "'Time Intelligence'",
            vec![rowset(
                &["Time Intelligence[Name]", "Time Intelligence[Ordinal]"],
                vec![json!(["YTD", 1]), json!(["Actual", 0])],
            )],
        );
        // Every admin function is refused.
    }

    /// The admin functions a model admin reads (Learn's columns).
    pub(crate) fn admin_answers(f: &mut Fake) {
        let a = |f: &mut Fake, name: &str, cols: &[&str], rows: Vec<serde_json::Value>| {
            f.dax_answer(&format!("{name}()"), vec![rowset(cols, rows)]);
        };
        a(
            f,
            "INFO.TABLES",
            &["[ID]", "[ModelID]", "[Name]"],
            vec![
                json!([12, 1, "Sales"]),
                json!([13, 1, "Customer"]),
                json!([14, 1, "Time Intelligence"]),
                json!([15, 1, "LocalDateTable_1b2c"]),
            ],
        );
        a(
            f,
            "INFO.COLUMNS",
            &["[ID]", "[TableID]", "[ExplicitName]", "[InferredName]"],
            vec![
                json!([101, 12, "CustomerKey", null]),
                json!([102, 12, "Amount", null]),
                json!([110, 13, "CustomerKey", null]),
                json!([111, 13, "City", null]),
            ],
        );
        a(
            f,
            "INFO.MEASURES",
            &["[ID]", "[TableID]", "[Name]"],
            vec![json!([200, 12, "Total Sales"])],
        );
        a(
            f,
            "INFO.HIERARCHIES",
            &["[ID]", "[TableID]", "[Name]", "[Description]", "[IsHidden]"],
            vec![json!([400, 13, "Geography", "País y ciudad", false])],
        );
        a(
            f,
            "INFO.LEVELS",
            &["[ID]", "[HierarchyID]", "[Ordinal]", "[Name]", "[ColumnID]"],
            vec![
                json!([401, 400, 1, "Ciudad", 111]),
                json!([402, 400, 0, "Cliente", 110]),
            ],
        );
        a(
            f,
            "INFO.CALCULATIONGROUPS",
            &["[ID]", "[TableID]", "[Description]", "[Precedence]"],
            vec![json!([500, 14, "Comparaciones de tiempo", 1])],
        );
        a(
            f,
            "INFO.CALCULATIONITEMS",
            &[
                "[ID]",
                "[CalculationGroupID]",
                "[FormatStringDefinition]",
                "[Name]",
                "[Description]",
                "[State]",
                "[ErrorMessage]",
                "[Expression]",
                "[Ordinal]",
            ],
            vec![
                json!([
                    501,
                    500,
                    null,
                    "YTD",
                    null,
                    "Ready",
                    null,
                    "CALCULATE(SELECTEDMEASURE(), DATESYTD('Date'[Date]))",
                    1
                ]),
                json!([
                    502,
                    500,
                    null,
                    "Actual",
                    null,
                    "Ready",
                    null,
                    "SELECTEDMEASURE()",
                    0
                ]),
            ],
        );
        a(
            f,
            "INFO.ROLES",
            &[
                "[ID]",
                "[ModelID]",
                "[Name]",
                "[Description]",
                "[ModelPermission]",
            ],
            vec![json!([600, 1, "West", "Solo el oeste", 2])],
        );
        a(
            f,
            "INFO.TABLEPERMISSIONS",
            &[
                "[ID]",
                "[RoleID]",
                "[TableID]",
                "[FilterExpression]",
                "[MetadataPermission]",
            ],
            vec![json!([601, 600, 13, "[City] = \"Seattle\"", 1])],
        );
        a(
            f,
            "INFO.COLUMNPERMISSIONS",
            &[
                "[ID]",
                "[TablePermissionID]",
                "[ColumnID]",
                "[MetadataPermission]",
            ],
            // TOM's MetadataPermission: 1 = None (hidden by object-level
            // security), 2 = Read.
            vec![json!([602, 601, 111, 1]), json!([604, 601, 110, 2])],
        );
        a(
            f,
            "INFO.ROLEMEMBERSHIPS",
            &[
                "[ID]",
                "[RoleID]",
                "[MemberName]",
                "[MemberID]",
                "[IdentityProvider]",
                "[MemberType]",
            ],
            vec![json!([603, 600, "ana@contoso.com", "x", "AzureAD", 1])],
        );
        a(
            f,
            "INFO.PERSPECTIVES",
            &["[ID]", "[ModelID]", "[Name]", "[Description]"],
            vec![json!([700, 1, "Comercial", null])],
        );
        a(
            f,
            "INFO.PERSPECTIVETABLES",
            &["[ID]", "[PerspectiveID]", "[TableID]", "[IncludeAll]"],
            vec![json!([701, 700, 12, false])],
        );
        a(
            f,
            "INFO.PERSPECTIVECOLUMNS",
            &["[ID]", "[PerspectiveTableID]", "[ColumnID]"],
            vec![json!([702, 701, 102])],
        );
        a(
            f,
            "INFO.PERSPECTIVEMEASURES",
            &["[ID]", "[PerspectiveTableID]", "[MeasureID]"],
            vec![json!([703, 701, 200])],
        );
        a(f, "INFO.PARTITIONS", &["[ID]", "[TableID]", "[Name]", "[Description]", "[DataSourceID]", "[QueryDefinition]", "[State]", "[Type]", "[PartitionStorageID]", "[Mode]"], vec![json!([800, 12, "Sales-1", null, null, "let\n    Source = Sql.Database(\"sql.contoso.com\", \"dw\")\nin\n    Source", 1, 4, 1, 0]), json!([801, 13, "Customer", null, null, "let Source = Table.FromRows(Json.Document(Binary.Decompress(Binary.FromText(\"i45WSk1RKMnIzEtXitWJVkpUitWJBQA=\", BinaryEncoding.Base64), Compression.Deflate))) in Source", 1, 4, 1, 0])]);
        a(f, "INFO.EXPRESSIONS", &["[ID]", "[ModelID]", "[Name]", "[Description]", "[Kind]", "[Expression]"], vec![json!([900, 1, "Servidor", null, 0, "\"sql.contoso.com\" meta [IsParameterQuery=true, Type=\"Text\", IsParameterQueryRequired=true]"])]);
        a(
            f,
            "INFO.USERDEFINEDFUNCTIONS",
            &[
                "[ID]",
                "[ModelID]",
                "[Name]",
                "[Description]",
                "[Expression]",
                "[IsHidden]",
            ],
            vec![json!([
                950,
                1,
                "AddTax",
                "Suma el IVA",
                "(amount : NUMERIC) => amount * 1.21",
                false
            ])],
        );
        a(
            f,
            "INFO.STORAGETABLES",
            &[
                "[Database_Name]",
                "[Cube_Name]",
                "[Measure_Group_Name]",
                "[Dimension_Name]",
                "[Table_ID]",
                "[Table_Partitions_Count]",
                "[Hint_Table_Type]",
                "[Rows_Count]",
            ],
            vec![
                json!([
                    "db",
                    "Model",
                    "Sales",
                    "Sales",
                    "Sales (12)",
                    1,
                    null,
                    60398
                ]),
                json!([
                    "db",
                    "Model",
                    "Sales",
                    "Sales",
                    "H$Sales (12)$CustomerKey (101)",
                    1,
                    null,
                    18484
                ]),
                json!([
                    "db",
                    "Model",
                    "Customer",
                    "Customer",
                    "Customer (13)",
                    1,
                    null,
                    18484
                ]),
            ],
        );
        a(
            f,
            "INFO.MODEL",
            &[
                "[ID]",
                "[Name]",
                "[Description]",
                "[StorageLocation]",
                "[DefaultMode]",
                "[DefaultDataView]",
                "[Culture]",
            ],
            vec![json!([1, "Model", null, null, 0, 3, "es-AR"])],
        );
    }

    #[tokio::test]
    async fn a_viewer_gets_the_view_functions_and_the_items() {
        let mut f = Fake::new();
        f.arrow = true;
        viewer_answers(&mut f);
        let m = f.model.clone().unwrap();
        let meta = read_model(&mut f, &m).await.unwrap();
        assert_eq!(meta.tier, Tier::Info);
        let sales = meta.table("Sales").unwrap();
        // The row-number column is left out; types and keys read.
        assert_eq!(
            sales
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["CustomerKey", "Amount", "Margin"]
        );
        assert_eq!(sales.columns[1].data_type, DataType::Decimal);
        assert_eq!(sales.columns[2].data_type, DataType::Double);
        assert_eq!(sales.description.as_deref(), Some("Ventas por línea"));
        assert_eq!(
            meta.table("LocalDateTable_1b2c").unwrap().kind,
            TableKind::AutoDate
        );
        assert_eq!(
            meta.table("Time Intelligence").unwrap().kind,
            TableKind::CalculationGroup
        );
        let g = &meta.calculation_groups[0];
        assert_eq!(g.precedence, Some(1));
        assert_eq!(
            g.items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            ["Actual", "YTD"]
        );
        assert_eq!(meta.measures.len(), 2);
        assert_eq!(meta.measures[1].state.as_deref(), Some("SemanticError"));
        let r = &meta.relationships[0];
        assert_eq!(
            (
                r.from_table.as_str(),
                r.to_table.as_str(),
                r.cross_filtering.as_deref()
            ),
            ("Sales", "Customer", Some("oneDirection"))
        );
        assert!(meta.hierarchies.is_empty() && meta.roles.is_empty());
        assert!(meta.notes.iter().any(|n| n.contains("administrador")));
        assert!(meta.notes.iter().any(|n| n.contains("expresiones")));
        // The admin batch, the probe, then never again for this model.
        let calls = f.dax_calls();
        read_model(&mut f, &m).await.unwrap();
        assert_eq!(f.dax_calls(), calls + 2, "{:?}", f.dax_log);
    }

    #[tokio::test]
    async fn a_model_admin_gets_everything_in_two_requests() {
        let mut f = Fake::new();
        f.arrow = true;
        viewer_answers(&mut f);
        admin_answers(&mut f);
        let m = f.model.clone().unwrap();
        let meta = read_model(&mut f, &m).await.unwrap();
        assert_eq!(f.dax_calls(), 2, "{:?}", f.dax_log);
        let h = &meta.hierarchies[0];
        assert_eq!(
            (h.table.as_str(), h.name.as_str()),
            ("Customer", "Geography")
        );
        assert_eq!(
            h.levels
                .iter()
                .map(|l| (l.name.as_str(), l.column.as_str()))
                .collect::<Vec<_>>(),
            [("Cliente", "CustomerKey"), ("Ciudad", "City")]
        );
        let g = &meta.calculation_groups[0];
        assert_eq!(
            g.items[1].expression.as_deref(),
            Some("CALCULATE(SELECTEDMEASURE(), DATESYTD('Date'[Date]))")
        );
        let role = &meta.roles[0];
        assert_eq!(role.model_permission.as_deref(), Some("read"));
        assert_eq!(
            role.table_filters,
            [("Customer".into(), "[City] = \"Seattle\"".into())]
        );
        assert_eq!(
            role.column_permissions,
            [
                ("Customer".into(), "City".into(), "none".into()),
                ("Customer".into(), "CustomerKey".into(), "read".into())
            ]
        );
        assert_eq!(role.members, ["ana@contoso.com"]);
        let p = &meta.perspectives[0];
        assert_eq!(p.tables, ["Sales"]);
        assert_eq!(p.columns, [("Sales".into(), "Amount".into())]);
        assert_eq!(p.measures, [("Sales".into(), "Total Sales".into())]);
        let sales = meta.table("Sales").unwrap();
        assert_eq!(sales.rows, Some(60398));
        assert_eq!(sales.partitions[0].source_kind.as_deref(), Some("m"));
        assert_eq!(sales.partitions[0].mode.as_deref(), Some("import"));
        let customer = meta.table("Customer").unwrap();
        let src = customer.partitions[0].expression.as_deref().unwrap();
        assert!(!src.contains("i45W"), "{src}");
        assert_eq!(meta.expressions[0].kind.as_deref(), Some("parameter"));
        assert_eq!(meta.functions[0].name, "AddTax");
        assert_eq!(meta.culture.as_deref(), Some("es-AR"));
        assert!(!meta.notes.iter().any(|n| n.contains("administrador")));
    }

    #[tokio::test]
    async fn the_admin_functions_that_work_are_asked_alone_once() {
        let mut f = Fake::new();
        f.arrow = true;
        viewer_answers(&mut f);
        admin_answers(&mut f);
        // This engine has no user-defined functions: the batch fails.
        f.dax.retain(|(n, _)| n != "INFO.USERDEFINEDFUNCTIONS()");
        f.dax_refuse("INFO.USERDEFINEDFUNCTIONS()");
        let m = f.model.clone().unwrap();
        let meta = read_model(&mut f, &m).await.unwrap();
        assert!(!meta.roles.is_empty() && meta.functions.is_empty());
        // view + batch + probe + one per function.
        assert_eq!(f.dax_calls(), 3 + ADMIN.len());
        let calls = f.dax_calls();
        read_model(&mut f, &m).await.unwrap();
        // Next time: view + the working ones in one request.
        assert_eq!(f.dax_calls(), calls + 2);
        assert!(!f.dax_log.last().unwrap().contains("USERDEFINEDFUNCTIONS"));
    }

    #[tokio::test]
    async fn calc_dependencies_read_learns_columns() {
        let mut f = Fake::new();
        f.arrow = true;
        f.dax_answer(
            "INFO.CALCDEPENDENCY()",
            vec![rowset(
                &[
                    "[DATABASE_NAME]",
                    "[OBJECT_TYPE]",
                    "[TABLE]",
                    "[OBJECT]",
                    "[EXPRESSION]",
                    "[REFERENCED_OBJECT_TYPE]",
                    "[REFERENCED_TABLE]",
                    "[REFERENCED_OBJECT]",
                    "[REFERENCED_EXPRESSION]",
                    "[QUERY]",
                ],
                vec![json!([
                    "db",
                    "MEASURE",
                    "Sales",
                    "Total Sales",
                    "SUM(Sales[Amount])",
                    "COLUMN",
                    "Sales",
                    "Amount",
                    null,
                    null
                ])],
            )],
        );
        let m = f.model.clone().unwrap();
        let deps = calc_dependencies_of(&mut f, &m).await.unwrap();
        assert_eq!(deps[0].object, "Total Sales");
        assert_eq!(deps[0].referenced_object, "Amount");
        assert_eq!(deps[0].expression.as_deref(), Some("SUM(Sales[Amount])"));
    }
}
