//! Reflections in the designer: what Dremio's "Reflections" page of a
//! dataset edits. A new one is `ALTER TABLE … CREATE RAW | AGGREGATE
//! REFLECTION` (or, created disabled or as an aggregation without
//! dimensions, which SQL can't say, `POST /api/v3/reflection`); a change is
//! `PUT /api/v3/reflection/{id}` (Dremio Software has no SQL for it): the
//! reflection keeps its id and its history, and Dremio materializes it
//! again when its columns change; deleting is `ALTER TABLE … DROP
//! REFLECTION "<id>"` (by id: two reflections of a dataset may share a
//! name). Its type can't change.

use super::{
    bad, column, columns_of, comment_text, one_line, path_label, path_of, rest_script, rows,
    segment, shown, sql_path, uuid_id, Plan, Step, Vals,
};
use crate::index_usage::dataset_path;
use crate::jobs::{is_uuid, policy};
use crate::{text, DremioSession};
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result,
    SchedulerItem, SchedulerList,
};
use reqwest::Method;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

const PREFIX: &str = "reflection";
const EXTERNAL: &str = "Es una reflexión externa: Dremio no la refresca ni la cambia, así que el diseñador solo la muestra. Se borra con ALTER TABLE … DROP REFLECTION.";
const STALE: &str = "La reflexión cambió en el servidor desde que se abrió el formulario: abrilo de nuevo para no pisar ese cambio.";
const EDIT_NOTE: &str = "Dremio no tiene SQL para cambiar una reflexión: DBine la cambia con su API REST, en el lugar. Conserva su id y su historial; si cambian sus columnas, particiones u orden, Dremio la vuelve a materializar.";
const TYPE_NOTE: &str =
    "El tipo de una reflexión no se cambia: para pasar de RAW a agregada, creá otra y borrá esta.";
const NEW_COLUMNS_NOTE: &str = "En una reflexión nueva, las columnas se escriben separadas por comas; al guardar, DBine las busca en la tabla elegida.";
const POLICY_NOTE: &str = "Cuándo se refresca la decide la política de refresco de su tabla o, si la tabla no tiene una, la de su origen (Políticas de refresco).";

const MEASURES: [(&str, &str); 5] = [
    ("COUNT", "COUNT"),
    ("SUM", "SUM"),
    ("MIN", "MIN"),
    ("MAX", "MAX"),
    ("APPROX_COUNT_DISTINCT", "APPROXIMATE COUNT DISTINCT"),
];
const TRANSFORMS: [&str; 7] = [
    "IDENTITY", "YEAR", "MONTH", "DAY", "HOUR", "BUCKET", "TRUNCATE",
];

/// Raw or aggregation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RKind {
    Raw,
    Aggregation,
}

impl RKind {
    fn key(self) -> &'static str {
        match self {
            RKind::Raw => "RAW",
            RKind::Aggregation => "AGGREGATION",
        }
    }
}

/// A partition column with its transform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Partition {
    pub(crate) name: String,
    /// IDENTITY, YEAR, MONTH, DAY, HOUR, BUCKET, TRUNCATE.
    pub(crate) transform: String,
    /// BUCKET's count, TRUNCATE's length.
    pub(crate) arg: Option<i64>,
}

/// A reflection's definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Def {
    pub(crate) name: String,
    pub(crate) kind: RKind,
    pub(crate) enabled: bool,
    pub(crate) display: Vec<String>,
    /// Column and granularity (NORMAL, DATE).
    pub(crate) dimensions: Vec<(String, String)>,
    /// Column and its functions (COUNT, SUM, MIN, MAX,
    /// APPROX_COUNT_DISTINCT).
    pub(crate) measures: Vec<(String, Vec<String>)>,
    pub(crate) partitions: Vec<Partition>,
    pub(crate) sort: Vec<String>,
    pub(crate) distribution: Vec<String>,
    /// CONSOLIDATED or STRIPED.
    pub(crate) strategy: String,
}

fn names_in(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|f| f.get("name").map(text))
                .filter(|n| !n.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// A reflection as `GET /api/v3/reflection/{id}` gives it; `None` for an
/// external one or a reply that isn't one.
pub(crate) fn def_of(v: &Value) -> Option<Def> {
    let kind = match v.get("type").and_then(Value::as_str)? {
        "RAW" => RKind::Raw,
        "AGGREGATION" => RKind::Aggregation,
        _ => return None,
    };
    let dimensions = v
        .get("dimensionFields")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|d| {
                    Some((
                        d.get("name").map(text)?,
                        d.get("granularity")
                            .map(text)
                            .filter(|g| g == "DATE")
                            .unwrap_or_else(|| "NORMAL".into()),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let measures = v
        .get("measureFields")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    Some((
                        m.get("name").map(text)?,
                        m.get("measureTypeList")
                            .and_then(Value::as_array)
                            .map(|t| t.iter().map(text).collect())
                            .unwrap_or_default(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let partitions = v
        .get("partitionFields")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let t = p.get("transform");
                    let transform = t
                        .and_then(|t| t.get("type"))
                        .map(text)
                        .filter(|x| !x.is_empty())
                        .unwrap_or_else(|| "IDENTITY".into());
                    let arg = t.and_then(|t| {
                        t.pointer("/bucketTransform/bucketCount")
                            .or_else(|| t.pointer("/truncateTransform/truncateLength"))
                            .and_then(Value::as_i64)
                    });
                    Some(Partition {
                        name: p.get("name").map(text)?,
                        transform,
                        arg,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Def {
        name: v.get("name").map(text).unwrap_or_default(),
        kind,
        enabled: v.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        display: names_in(v, "displayFields"),
        dimensions,
        measures,
        partitions,
        sort: names_in(v, "sortFields"),
        distribution: names_in(v, "distributionFields"),
        strategy: v
            .get("partitionDistributionStrategy")
            .map(text)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "CONSOLIDATED".into()),
    })
}

fn named(cols: &[String]) -> Value {
    Value::Array(cols.iter().map(|c| json!({"name": c})).collect())
}

/// The definition's fields as the REST API takes them, over `base` (the
/// reflection as it is, for an edit).
pub(crate) fn rest_body(d: &Def, mut base: Map<String, Value>) -> Value {
    for k in [
        "status",
        "currentSizeBytes",
        "totalSizeBytes",
        "createdAt",
        "updatedAt",
        "displayFields",
        "dimensionFields",
        "measureFields",
    ] {
        base.remove(k);
    }
    base.insert("type".into(), json!(d.kind.key()));
    base.insert("name".into(), json!(d.name));
    base.insert("enabled".into(), json!(d.enabled));
    match d.kind {
        RKind::Raw => {
            base.insert("displayFields".into(), named(&d.display));
        }
        RKind::Aggregation => {
            base.insert(
                "dimensionFields".into(),
                Value::Array(
                    d.dimensions
                        .iter()
                        .map(|(n, g)| json!({"name": n, "granularity": g}))
                        .collect(),
                ),
            );
            base.insert(
                "measureFields".into(),
                Value::Array(
                    d.measures
                        .iter()
                        .map(|(n, t)| json!({"name": n, "measureTypeList": t}))
                        .collect(),
                ),
            );
        }
    }
    base.insert(
        "partitionFields".into(),
        Value::Array(
            d.partitions
                .iter()
                .map(|p| {
                    let mut t = json!({"type": p.transform});
                    match (p.transform.as_str(), p.arg) {
                        ("BUCKET", Some(n)) => t["bucketTransform"] = json!({"bucketCount": n}),
                        ("TRUNCATE", Some(n)) => {
                            t["truncateTransform"] = json!({"truncateLength": n})
                        }
                        _ => {}
                    }
                    json!({"name": p.name, "transform": t})
                })
                .collect(),
        ),
    );
    base.insert("sortFields".into(), named(&d.sort));
    base.insert("distributionFields".into(), named(&d.distribution));
    base.insert("partitionDistributionStrategy".into(), json!(d.strategy));
    base.insert("entityType".into(), json!("reflection"));
    Value::Object(base)
}

fn q_list(cols: &[String]) -> String {
    cols.iter()
        .map(|c| crate::ddl::q(c))
        .collect::<Vec<_>>()
        .join(", ")
}

fn partition_sql(p: &Partition) -> String {
    let c = crate::ddl::q(&p.name);
    match (p.transform.as_str(), p.arg) {
        ("IDENTITY", _) => c,
        ("BUCKET" | "TRUNCATE", Some(n)) => format!("{}({n}, {c})", p.transform),
        (t, _) => format!("{t}({c})"),
    }
}

/// `ALTER TABLE … CREATE … REFLECTION` for `d` on the dataset at
/// `path_sql` (quoted); `None` where SQL can't say it (disabled, or an
/// aggregation without dimensions).
pub(crate) fn create_sql(path_sql: &str, d: &Def) -> Option<String> {
    if !d.enabled {
        return None;
    }
    statement_sql(path_sql, d)
}

/// The `CREATE … REFLECTION` that makes `d` (enabled), whatever it is now;
/// `None` for an aggregation without dimensions.
fn statement_sql(path_sql: &str, d: &Def) -> Option<String> {
    let mut s = match d.kind {
        RKind::Raw => format!(
            "ALTER TABLE {path_sql} CREATE RAW REFLECTION {} USING DISPLAY ({})",
            crate::ddl::q(&d.name),
            q_list(&d.display)
        ),
        RKind::Aggregation => {
            if d.dimensions.is_empty() {
                return None;
            }
            let dims: Vec<String> = d
                .dimensions
                .iter()
                .map(|(n, g)| {
                    if g == "DATE" {
                        format!("{} BY DAY", crate::ddl::q(n))
                    } else {
                        crate::ddl::q(n)
                    }
                })
                .collect();
            let mut s = format!(
                "ALTER TABLE {path_sql} CREATE AGGREGATE REFLECTION {} USING DIMENSIONS ({})",
                crate::ddl::q(&d.name),
                dims.join(", ")
            );
            if !d.measures.is_empty() {
                let ms: Vec<String> = d
                    .measures
                    .iter()
                    .map(|(n, types)| {
                        if types.is_empty() {
                            crate::ddl::q(n)
                        } else {
                            let ts: Vec<&str> = types
                                .iter()
                                .filter_map(|t| {
                                    MEASURES.iter().find(|(k, _)| k == t).map(|(_, sql)| *sql)
                                })
                                .collect();
                            format!("{} ({})", crate::ddl::q(n), ts.join(", "))
                        }
                    })
                    .collect();
                s.push_str(&format!(" MEASURES ({})", ms.join(", ")));
            }
            s
        }
    };
    if !d.distribution.is_empty() {
        s.push_str(&format!(" DISTRIBUTE BY ({})", q_list(&d.distribution)));
    }
    if !d.partitions.is_empty() {
        let ps: Vec<String> = d.partitions.iter().map(partition_sql).collect();
        s.push_str(&format!(" {} PARTITION BY ({})", d.strategy, ps.join(", ")));
    }
    if !d.sort.is_empty() {
        s.push_str(&format!(" LOCALSORT BY ({})", q_list(&d.sort)));
    }
    s.push(';');
    Some(s)
}

// ---------------------------------------------------------------------------
// The form's values
// ---------------------------------------------------------------------------

fn unique(cols: &[String], label: &str) -> Result<()> {
    for (i, c) in cols.iter().enumerate() {
        if cols[..i].iter().any(|x| x.eq_ignore_ascii_case(c)) {
            return Err(bad(format!(
                "«{label}»: la columna «{}» está dos veces.",
                shown(c)
            )));
        }
    }
    Ok(())
}

fn resolve_all(columns: &[String], names: &[String], label: &str) -> Result<Vec<String>> {
    let out = names
        .iter()
        .map(|n| column(columns, n, label))
        .collect::<Result<Vec<_>>>()?;
    unique(&out, label)?;
    Ok(out)
}

/// The form's values as a definition, its columns checked against the
/// dataset's (`kind`: the reflection's when it exists).
pub(crate) fn wanted(values: &FormValues, columns: &[String], kind: Option<RKind>) -> Result<Def> {
    let v = Vals(&values.values);
    let name = one_line(&v.text("name"), "Nombre")?;
    if name.is_empty() {
        return Err(bad("La reflexión necesita un nombre.".into()));
    }
    let kind = match kind {
        Some(k) => k,
        None => match v.text("reflection_type").to_ascii_uppercase().as_str() {
            "" | "RAW" => RKind::Raw,
            "AGGREGATION" => RKind::Aggregation,
            other => {
                return Err(bad(format!(
                    "«{}» no es un tipo de reflexión (RAW o AGGREGATION).",
                    shown(other)
                )))
            }
        },
    };
    let mut d = Def {
        name,
        kind,
        enabled: v.bool_or("enabled", true),
        display: Vec::new(),
        dimensions: Vec::new(),
        measures: Vec::new(),
        partitions: Vec::new(),
        sort: resolve_all(columns, &v.names("sort"), "Ordenar por")?,
        distribution: resolve_all(columns, &v.names("distribution"), "Distribuir por")?,
        strategy: match v.text("strategy").to_ascii_uppercase().as_str() {
            "" | "CONSOLIDATED" => "CONSOLIDATED".into(),
            "STRIPED" => "STRIPED".into(),
            other => {
                return Err(bad(format!(
                    "«{}» no es una distribución de particiones (CONSOLIDATED o STRIPED).",
                    shown(other)
                )))
            }
        },
    };
    let dims = rows(values, "dimensions");
    let measures = rows(values, "measures");
    match kind {
        RKind::Raw => {
            d.display = resolve_all(columns, &v.names("display"), "Columnas")?;
            if d.display.is_empty() {
                return Err(bad(
                    "Una reflexión RAW necesita al menos una columna.".into()
                ));
            }
            if dims
                .iter()
                .chain(&measures)
                .any(|r| !r.text("column").is_empty())
            {
                return Err(bad(
                    "Una reflexión RAW no tiene dimensiones ni medidas: son de las agregadas."
                        .into(),
                ));
            }
        }
        RKind::Aggregation => {
            for r in &dims {
                let raw = r.text("column");
                if raw.is_empty() {
                    continue;
                }
                let c = column(columns, &raw, "Dimensiones")?;
                let g = match r.text("granularity").to_ascii_uppercase().as_str() {
                    "" | "NORMAL" => "NORMAL",
                    "DATE" => "DATE",
                    other => {
                        return Err(bad(format!(
                            "«{}» no es una granularidad (NORMAL o DATE).",
                            shown(other)
                        )))
                    }
                };
                d.dimensions.push((c, g.into()));
            }
            for r in &measures {
                let raw = r.text("column");
                if raw.is_empty() {
                    continue;
                }
                let c = column(columns, &raw, "Medidas")?;
                let mut types = Vec::new();
                for t in r.names("functions") {
                    let up = t.to_ascii_uppercase();
                    if !MEASURES.iter().any(|(k, _)| *k == up) {
                        return Err(bad(format!(
                            "«{}» no es una medida de Dremio (COUNT, SUM, MIN, MAX o APPROX_COUNT_DISTINCT).",
                            shown(&t)
                        )));
                    }
                    if !types.contains(&up) {
                        types.push(up);
                    }
                }
                d.measures.push((c, types));
            }
            unique(
                &d.dimensions.iter().map(|x| x.0.clone()).collect::<Vec<_>>(),
                "Dimensiones",
            )?;
            unique(
                &d.measures.iter().map(|x| x.0.clone()).collect::<Vec<_>>(),
                "Medidas",
            )?;
            if d.dimensions.is_empty() && d.measures.is_empty() {
                return Err(bad(
                    "Una reflexión agregada necesita al menos una dimensión o una medida.".into(),
                ));
            }
            // «Columnas» is RAW's: hidden for an aggregation, it may still
            // come with what was typed before the type changed.
        }
    }
    for r in rows(values, "partitions") {
        let raw = r.text("column");
        if raw.is_empty() {
            continue;
        }
        let c = column(columns, &raw, "Particiones")?;
        let t = match r.text("transform").to_ascii_uppercase() {
            t if t.is_empty() => "IDENTITY".to_string(),
            t => t,
        };
        if !TRANSFORMS.contains(&t.as_str()) {
            return Err(bad(format!(
                "«{}» no es una transformación de partición (IDENTITY, YEAR, MONTH, DAY, HOUR, BUCKET o TRUNCATE).",
                shown(&t)
            )));
        }
        let arg = if t == "BUCKET" || t == "TRUNCATE" {
            let n = r
                .int("argument", "Argumento", 1, 1_000_000)?
                .ok_or_else(|| {
                    bad(format!(
                        "La partición {t} de «{}» necesita su número (cubetas o largo).",
                        shown(&c)
                    ))
                })?;
            Some(n)
        } else {
            None
        };
        if d.partitions.iter().any(|p| p.name.eq_ignore_ascii_case(&c)) {
            return Err(bad(format!(
                "«Particiones»: la columna «{}» está dos veces (Dremio usa cada columna en una sola transformación).",
                shown(&c)
            )));
        }
        d.partitions.push(Partition {
            name: c,
            transform: t,
            arg,
        });
    }
    Ok(d)
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

/// A field of columns: a pick from the dataset's (an existing reflection)
/// or typed, separated by commas (a new one).
fn columns_field(key: &str, label: &str, existing: bool) -> FormField {
    if existing {
        FormField::new(key, label, FormKind::reference("column", true))
    } else {
        FormField::new(key, label, FormKind::Text).placeholder("id, cliente, fecha")
    }
}

fn column_cell(existing: bool) -> FormField {
    if existing {
        FormField::new("column", "Columna", FormKind::reference("column", false)).required()
    } else {
        FormField::new("column", "Columna", FormKind::Text).required()
    }
}

fn granularity() -> FormKind {
    FormKind::select(&[("NORMAL", "Normal"), ("DATE", "Por día (BY DAY)")])
}

fn measure_functions() -> FormKind {
    FormKind::multi(&[
        ("COUNT", "COUNT"),
        ("SUM", "SUM"),
        ("MIN", "MIN"),
        ("MAX", "MAX"),
        ("APPROX_COUNT_DISTINCT", "Cantidad aproximada de distintos"),
    ])
}

fn transform() -> FormKind {
    FormKind::select(&[
        ("IDENTITY", "El valor (IDENTITY)"),
        ("YEAR", "Año (YEAR)"),
        ("MONTH", "Mes (MONTH)"),
        ("DAY", "Día (DAY)"),
        ("HOUR", "Hora (HOUR)"),
        ("BUCKET", "Cubetas (BUCKET)"),
        ("TRUNCATE", "Truncado (TRUNCATE)"),
    ])
}

/// A reflection as the form shows it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Existing {
    pub(crate) def: Option<Def>,
    pub(crate) tag: String,
    pub(crate) dataset_label: String,
    pub(crate) columns: Vec<String>,
    /// What the server says of it now: (field, label, value).
    pub(crate) state: Vec<(&'static str, &'static str, String)>,
    pub(crate) external: bool,
}

pub(crate) fn form_of(e: Option<&Existing>, datasets: &[(String, String)]) -> ObjectForm {
    let existing = e.is_some();
    let def = e.and_then(|e| e.def.clone());
    let kind = def.as_ref().map(|d| d.kind);
    let mut f = ObjectForm::default();
    let mut values: BTreeMap<String, Value> = BTreeMap::new();
    let mut general = FormSection::new("general", "General").field(
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .placeholder("pedidos_por_dia"),
    );
    if existing {
        general = general.field(
            FormField::new("dataset_label", "Tabla o vista", FormKind::Text).read_only(true),
        );
    } else {
        general = general.field(
            FormField::new(
                "dataset",
                "Tabla o vista",
                FormKind::reference("dataset", false),
            )
            .required(),
        );
    }
    general = general.fields(vec![
        FormField::new(
            "reflection_type",
            "Tipo",
            FormKind::select(&[
                ("RAW", "RAW: las filas, con las columnas elegidas"),
                ("AGGREGATION", "Agregada: dimensiones y medidas"),
            ]),
        )
        .required()
        .read_only(existing)
        .default(json!("RAW")),
        FormField::new("enabled", "Habilitada", FormKind::Bool)
            .default(json!(true))
            .help("Deshabilitarla borra sus datos y la saca de las consultas; al habilitarla, Dremio la materializa de nuevo."),
    ]);
    let mut cols = FormSection::new("columns", "Columnas");
    if kind != Some(RKind::Aggregation) {
        cols = cols.field(
            columns_field("display", "Columnas", existing)
                .when("reflection_type", &["RAW"])
                .help("Las columnas que guarda la reflexión RAW."),
        );
    }
    if kind != Some(RKind::Raw) {
        cols = cols
            .list(
                FormList::new(
                    "dimensions",
                    "Dimensiones (agregada)",
                    "column",
                    vec![
                        column_cell(existing),
                        FormField::new("granularity", "Granularidad", granularity())
                            .default(json!("NORMAL"))
                            .help("Por día agrupa una fecha u hora por su día."),
                    ],
                )
                .when("reflection_type", &["AGGREGATION"]),
            )
            .list(
                FormList::new(
                    "measures",
                    "Medidas (agregada)",
                    "column",
                    vec![
                        column_cell(existing),
                        FormField::new("functions", "Funciones", measure_functions())
                            .help("Sin ninguna, Dremio usa COUNT y SUM."),
                    ],
                )
                .when("reflection_type", &["AGGREGATION"]),
            );
    }
    let layout = FormSection::new("layout", "Organización")
        .list(
            FormList::new(
                "partitions",
                "Particiones",
                "column",
                vec![
                    column_cell(existing),
                    FormField::new("transform", "Transformación", transform())
                        .default(json!("IDENTITY")),
                    FormField::new(
                        "argument",
                        "Cubetas o largo",
                        FormKind::number(Some(1.0), None),
                    )
                    .when("transform", &["BUCKET", "TRUNCATE"]),
                ],
            )
            .orderable(),
        )
        .fields(vec![
            FormField::new(
                "strategy",
                "Archivos por partición",
                FormKind::select(&[
                    ("CONSOLIDATED", "Consolidados: pocos archivos grandes"),
                    (
                        "STRIPED",
                        "En franjas (STRIPED): varios, escritos en paralelo",
                    ),
                ]),
            )
            .default(json!("CONSOLIDATED")),
            columns_field("sort", "Ordenar por (LOCALSORT)", existing),
            columns_field("distribution", "Distribuir por (DISTRIBUTE BY)", existing),
        ]);
    let mut sections = vec![general, cols, layout];
    values.insert("enabled".into(), json!(true));
    values.insert("reflection_type".into(), json!("RAW"));
    values.insert("strategy".into(), json!("CONSOLIDATED"));
    if let Some(e) = e {
        values.insert("dataset_label".into(), json!(e.dataset_label));
        values.insert("tag".into(), json!(e.tag));
        if let Some(d) = &def {
            values.insert("name".into(), json!(d.name));
            values.insert("reflection_type".into(), json!(d.kind.key()));
            values.insert("enabled".into(), json!(d.enabled));
            values.insert("strategy".into(), json!(d.strategy));
            values.insert("display".into(), json!(d.display));
            values.insert("sort".into(), json!(d.sort));
            values.insert("distribution".into(), json!(d.distribution));
            let set_rows = |s: &mut FormSection, key: &str, rows: Vec<BTreeMap<String, Value>>| {
                for l in s.lists.iter_mut().filter(|l| l.key == key) {
                    l.rows = rows.clone();
                }
            };
            let dims = d
                .dimensions
                .iter()
                .map(|(n, g)| {
                    BTreeMap::from([
                        ("column".to_string(), json!(n)),
                        ("granularity".to_string(), json!(g)),
                    ])
                })
                .collect();
            let ms = d
                .measures
                .iter()
                .map(|(n, t)| {
                    BTreeMap::from([
                        ("column".to_string(), json!(n)),
                        ("functions".to_string(), json!(t)),
                    ])
                })
                .collect();
            let ps = d
                .partitions
                .iter()
                .map(|p| {
                    let mut r = BTreeMap::from([
                        ("column".to_string(), json!(p.name)),
                        ("transform".to_string(), json!(p.transform)),
                    ]);
                    if let Some(n) = p.arg {
                        r.insert("argument".into(), json!(n));
                    }
                    r
                })
                .collect();
            set_rows(&mut sections[1], "dimensions", dims);
            set_rows(&mut sections[1], "measures", ms);
            set_rows(&mut sections[2], "partitions", ps);
        }
        let mut state = FormSection::new("state", "Estado");
        for (key, label, value) in &e.state {
            let kind = if *key == "state_error" {
                FormKind::Textarea
            } else {
                FormKind::Text
            };
            state = state.field(FormField::new(key, label, kind).read_only(true));
            values.insert(key.to_string(), json!(value));
        }
        sections.push(state);
        f.choices.insert(
            "column".into(),
            e.columns.iter().map(|c| (c.clone(), c.clone())).collect(),
        );
        if e.external {
            f.read_only = true;
            f.notes.push(EXTERNAL.into());
        } else {
            f.notes.push(EDIT_NOTE.into());
            f.notes.push(TYPE_NOTE.into());
        }
    } else {
        f.choices.insert("dataset".into(), datasets.to_vec());
        f.notes.push(NEW_COLUMNS_NOTE.into());
    }
    f.notes.push(POLICY_NOTE.into());
    f.sections = sections;
    f.values = values;
    f
}

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

fn not_found(uuid: &str) -> Error {
    bad(format!(
        "no existe la reflexión «{uuid}» o este usuario no la puede ver"
    ))
}

/// What the server says, for the form's "Estado".
fn state_of(
    r: &Value,
    sys: Option<&Map<String, Value>>,
    schedule: Option<Option<String>>,
) -> Vec<(&'static str, &'static str, String)> {
    let st = |p: &str| r.pointer(p).map(text).unwrap_or_default();
    let sys_s = |k: &str| {
        sys.and_then(|m| m.get(k))
            .map(text)
            .filter(|v| v != "-1")
            .unwrap_or_default()
    };
    let date = |p: &str| {
        let v = st(p);
        if v.starts_with("1969") || v.starts_with("1970") {
            String::new()
        } else {
            v
        }
    };
    vec![
        ("state_status", "Estado", st("/status/combinedStatus")),
        ("state_refresh", "Refresco", st("/status/refresh")),
        (
            "state_availability",
            "Disponibilidad",
            st("/status/availability"),
        ),
        (
            "state_failures",
            "Fallas seguidas",
            st("/status/failureCount"),
        ),
        ("state_data", "Datos de", date("/status/lastDataFetch")),
        ("state_expires", "Vence", date("/status/expiresAt")),
        ("state_size", "Tamaño (bytes)", st("/currentSizeBytes")),
        ("state_rows", "Filas", sys_s("record_count")),
        (
            "state_method",
            "Método de refresco",
            sys_s("refresh_method"),
        ),
        (
            "state_policy",
            "Política de refresco",
            match schedule {
                Some(Some(p)) => p,
                Some(None) => "Nunca".into(),
                None => String::new(),
            },
        ),
        ("state_mode", "Modo", st("/reflectionMode")),
        ("state_created", "Creada", st("/createdAt")),
        ("state_updated", "Modificada", st("/updatedAt")),
        (
            "state_error",
            "Último error",
            sys_s("last_failure_message").chars().take(4000).collect(),
        ),
    ]
}

impl DremioSession {
    async fn reflection_json(&self, uuid: &str) -> Result<Value> {
        self.conn
            .send(Method::GET, &format!("/api/v3/reflection/{uuid}"), None)
            .await
            .map_err(|e| match e {
                Error::Query(m) if m.contains("not found") => not_found(uuid),
                e => e,
            })
    }

    /// The ids of a dataset's reflections now.
    async fn dataset_reflections(&self, dataset: &str) -> Result<Vec<String>> {
        let v = self
            .conn
            .send(
                Method::GET,
                &format!("/api/v3/dataset/{}/reflection", segment(dataset)),
                None,
            )
            .await?;
        Ok(v.get("data")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|r| r.get("id").map(text))
                    .map(|u| u.to_ascii_lowercase())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// The `sys.reflections` row of a reflection (read whole and picked
    /// here: nothing of the id goes to the server).
    async fn sys_row(&self, uuid: &str) -> Option<Map<String, Value>> {
        self.records("SELECT reflection_id, record_count, refresh_method, last_failure_message, external_reflection FROM sys.reflections")
            .await
            .ok()?
            .into_iter()
            .find(|r| {
                r.get("reflection_id")
                    .map(text)
                    .is_some_and(|x| x.eq_ignore_ascii_case(uuid))
            })
    }

    /// A dataset from the form's pick (its quoted path): the entity.
    async fn picked_dataset(&self, picked: &str) -> Result<Value> {
        let parts = dataset_path(picked.trim());
        if parts.len() < 2 || parts.iter().any(|p| p.is_empty() || p == "." || p == "..") {
            return Err(bad(format!(
                "«{}» no es una tabla o vista de Dremio.",
                shown(picked)
            )));
        }
        let e = self.catalog_at(&parts).await.map_err(|_| {
            bad(format!(
                "No existe la tabla o vista «{}» (o este usuario no la puede ver).",
                shown(&path_label(&parts))
            ))
        })?;
        if e.get("entityType").and_then(Value::as_str) != Some("dataset") {
            return Err(bad(format!(
                "«{}» no es una tabla ni una vista.",
                shown(&path_label(&parts))
            )));
        }
        Ok(e)
    }
}

pub(crate) async fn items(s: &mut DremioSession) -> Result<SchedulerList> {
    let rows = s
        .records("SELECT reflection_id, reflection_name, type, status, dataset_name, external_reflection FROM sys.reflections ORDER BY dataset_name, reflection_name")
        .await?;
    let g = |r: &Map<String, Value>, k: &str| r.get(k).map(text).unwrap_or_default();
    let items = rows
        .iter()
        .filter(|r| g(r, "external_reflection").trim().is_empty())
        .filter_map(|r| {
            let id = g(r, "reflection_id");
            is_uuid(&id).then(|| SchedulerItem {
                id: format!("{PREFIX}:{}", id.to_ascii_lowercase()),
                name: g(r, "reflection_name"),
                description: Some(format!("{} · {}", g(r, "type"), g(r, "dataset_name"))),
                enabled: Some(g(r, "status") != "DISABLED"),
                system: false,
                can_edit: None,
                can_delete: None,
                actions: None,
                run_from_step: None,
                clear_history: None,
            })
        })
        .collect();
    Ok(SchedulerList {
        items,
        notes: Vec::new(),
    })
}

pub(crate) async fn form(s: &mut DremioSession, id: Option<&str>) -> Result<ObjectForm> {
    let Some(id) = id else {
        let datasets = s.dataset_choices(true).await?;
        return Ok(form_of(None, &datasets));
    };
    let uuid = uuid_id(id, PREFIX, "reflexión")?;
    let r = s.reflection_json(&uuid).await?;
    let dataset = r.get("datasetId").map(text).filter(|d| is_uuid(d));
    let entity = match &dataset {
        Some(d) => s.catalog(d).await.ok(),
        None => None,
    };
    let sys = s.sys_row(&uuid).await;
    let external = def_of(&r).is_none()
        || sys
            .as_ref()
            .and_then(|m| m.get("external_reflection"))
            .is_some_and(|x| !text(x).trim().is_empty());
    let schedule = match &entity {
        Some(e) if e.get("type").and_then(Value::as_str) == Some("PHYSICAL_DATASET") => {
            match policy(e) {
                Some(p) => Some(p),
                None => {
                    let source = path_of(e).into_iter().next().unwrap_or_default();
                    s.catalog_at(&[source]).await.ok().and_then(|v| policy(&v))
                }
            }
        }
        Some(_) => Some(Some("De las tablas de la vista".into())),
        None => None,
    };
    let e = Existing {
        def: def_of(&r),
        tag: r.get("tag").map(text).unwrap_or_default(),
        dataset_label: entity
            .as_ref()
            .map(|e| path_label(&path_of(e)))
            .unwrap_or_default(),
        columns: entity.as_ref().map(columns_of).unwrap_or_default(),
        state: state_of(&r, sys.as_ref(), schedule),
        external,
    };
    Ok(form_of(Some(&e), &[]))
}

pub(crate) async fn plan(
    s: &mut DremioSession,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    match id {
        None => {
            let picked = Vals(&values.values)
                .opt("dataset")
                .ok_or_else(|| bad("Elegí la tabla o vista de la reflexión.".into()))?;
            let entity = s.picked_dataset(&picked).await?;
            let dataset = entity
                .get("id")
                .map(text)
                .filter(|d| is_uuid(d))
                .ok_or_else(|| bad("Dremio no dio el id de la tabla.".into()))?;
            let d = wanted(values, &columns_of(&entity), None)?;
            let path = path_of(&entity);
            Ok(create_plan(
                &d,
                &path,
                &dataset,
                s.dataset_reflections(&dataset).await?,
            ))
        }
        Some(id) => {
            let uuid = uuid_id(id, PREFIX, "reflexión")?;
            let r = s.reflection_json(&uuid).await?;
            let current = def_of(&r).ok_or_else(|| Error::Unsupported(EXTERNAL.into()))?;
            let tag = r.get("tag").map(text).unwrap_or_default();
            if Vals(&values.values).opt("tag").is_some_and(|t| t != tag) {
                return Err(bad(STALE.into()));
            }
            let dataset = r
                .get("datasetId")
                .map(text)
                .filter(|d| is_uuid(d))
                .ok_or_else(|| not_found(&uuid))?;
            let entity = s.catalog(&dataset).await?;
            let d = wanted(values, &columns_of(&entity), Some(current.kind))?;
            Ok(alter_plan(&uuid, &current, &d, &r))
        }
    }
}

/// A new reflection on the dataset at `path` (`before`: its reflections'
/// ids now).
pub(crate) fn create_plan(d: &Def, path: &[String], dataset: &str, before: Vec<String>) -> Plan {
    let label = path_label(path);
    match create_sql(&sql_path(path), d) {
        Some(sql) => Plan {
            script: format!(
                "-- Reflexión nueva «{}» de {}\n{sql}\n",
                comment_text(&d.name),
                comment_text(&label)
            ),
            steps: vec![
                Step::Sql(sql.trim_end_matches(';').to_string()),
                Step::FindReflection {
                    dataset: dataset.to_string(),
                    before,
                    name: d.name.clone(),
                },
            ],
            id: None,
            prefix: PREFIX,
        },
        None => {
            let mut body = rest_body(d, Map::new());
            body["datasetId"] = json!(dataset);
            let why = if d.enabled {
                format!("Reflexión nueva «{}» de {label}: una agregada sin dimensiones no se puede escribir en SQL, así que DBine la crea con la API REST de Dremio.", d.name)
            } else {
                format!("Reflexión nueva «{}» de {label}, deshabilitada: el SQL de Dremio no crea una reflexión deshabilitada, así que DBine la crea con su API REST.", d.name)
            };
            Plan {
                script: rest_script(&why, "POST", "/api/v3/reflection", Some(&body)),
                steps: vec![Step::Rest {
                    method: Method::POST,
                    path: "/api/v3/reflection".into(),
                    body: Some(body),
                }],
                id: None,
                prefix: PREFIX,
            }
        }
    }
}

/// A change to the reflection `uuid` (`r`: as the REST API has it).
pub(crate) fn alter_plan(uuid: &str, current: &Def, d: &Def, r: &Value) -> Plan {
    let id = format!("{PREFIX}:{uuid}");
    if d == current {
        return Plan::nothing(&format!("Reflexión «{}»", current.name), id, PREFIX);
    }
    let base = r.as_object().cloned().unwrap_or_default();
    let body = rest_body(d, base);
    let path = format!("/api/v3/reflection/{uuid}");
    let mut why = format!(
        "Reflexión «{}»: Dremio no tiene SQL para cambiar una reflexión, así que DBine la cambia con su API REST, en el lugar (conserva su id y su historial).",
        current.name
    );
    let shape = Def {
        name: current.name.clone(),
        enabled: current.enabled,
        ..d.clone()
    };
    if shape != *current {
        why.push_str("\nSus columnas u organización cambian: Dremio la vuelve a materializar.");
    }
    if current.enabled && !d.enabled {
        why.push_str("\nDeshabilitarla borra sus datos y la saca de las consultas.");
    }
    Plan {
        script: rest_script(&why, "PUT", &path, Some(&body)),
        steps: vec![Step::Rest {
            method: Method::PUT,
            path,
            body: Some(body),
        }],
        id: Some(id),
        prefix: PREFIX,
    }
}

/// `ALTER TABLE … DROP REFLECTION "<id>"` and the script that shows it.
pub(crate) fn drop_sql(path: &[String], uuid: &str, name: &str, label: &str) -> (String, String) {
    let sql = format!(
        "ALTER TABLE {} DROP REFLECTION {}",
        sql_path(path),
        crate::ddl::q(uuid)
    );
    let script = format!(
        "-- Borra la reflexión «{}» de {} (por su id: otra de la tabla puede llamarse igual)\n{sql};\n",
        comment_text(name),
        comment_text(label)
    );
    (script, sql)
}

pub(crate) async fn delete_plan(s: &mut DremioSession, id: &str) -> Result<Plan> {
    let uuid = uuid_id(id, PREFIX, "reflexión")?;
    let r = s.reflection_json(&uuid).await?;
    let dataset = r
        .get("datasetId")
        .map(text)
        .filter(|d| is_uuid(d))
        .ok_or_else(|| not_found(&uuid))?;
    let path = path_of(&s.catalog(&dataset).await?);
    if path.is_empty() {
        return Err(not_found(&uuid));
    }
    let name = r.get("name").map(text).unwrap_or_default();
    let (script, sql) = drop_sql(&path, &uuid, &name, &path_label(&path));
    Ok(Plan {
        script,
        steps: vec![Step::Sql(sql)],
        id: Some(format!("{PREFIX}:{uuid}")),
        prefix: PREFIX,
    })
}

/// The statement that recreates the reflection `uuid` exactly (its
/// dimensions' granularity, its measures' functions, its partition
/// transforms and distribution strategy, which `sys.reflections` doesn't
/// say), for the job's script; `None` when the REST API can't tell.
pub(crate) async fn exact_script(s: &DremioSession, uuid: &str) -> Option<String> {
    if !is_uuid(uuid) {
        return None;
    }
    let r = s.reflection_json(uuid).await.ok()?;
    let d = def_of(&r)?;
    let dataset = r.get("datasetId").map(text).filter(|d| is_uuid(d))?;
    let path = path_of(&s.catalog(&dataset).await.ok()?);
    if path.is_empty() {
        return None;
    }
    statement_sql(&sql_path(&path), &d).map(|sql| format!("{sql}\n"))
}

/// After a `CREATE … REFLECTION`: the id of the dataset's reflection that
/// wasn't there before.
pub(crate) async fn find_new(
    s: &mut DremioSession,
    dataset: &str,
    before: &[String],
    name: &str,
) -> Result<Option<String>> {
    let v = s
        .conn
        .send(
            Method::GET,
            &format!("/api/v3/dataset/{}/reflection", segment(dataset)),
            None,
        )
        .await?;
    Ok(v.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| r.get("name").map(text).as_deref() == Some(name))
        .filter_map(|r| r.get("id").map(text))
        .map(|u| u.to_ascii_lowercase())
        .find(|u| is_uuid(u) && !before.contains(u))
        .map(|u| format!("{PREFIX}:{u}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agg_json() -> Value {
        json!({"id": "d215b51c-01f2-49f0-8120-74e508386c42", "type": "AGGREGATION", "name": "dbine_test_sched_agg", "tag": "bjVsH5RwgVI=",
               "datasetId": "9534f3ff-ba65-4f91-a1f7-e523d93a6c92", "enabled": true, "currentSizeBytes": 9302,
               "status": {"combinedStatus": "CAN_ACCELERATE", "refresh": "SCHEDULED", "availability": "AVAILABLE", "failureCount": 0,
                          "lastDataFetch": "2026-11-12T21:45:22.854Z", "expiresAt": "2026-11-13T00:45:22.854Z"},
               "dimensionFields": [{"name": "ts", "granularity": "DATE"}, {"name": "c", "granularity": "NORMAL"}],
               "measureFields": [{"name": "b", "measureTypeList": ["SUM", "COUNT", "APPROX_COUNT_DISTINCT"]}, {"name": "a", "measureTypeList": ["COUNT", "SUM"]}],
               "partitionFields": [{"name": "c", "transform": {"type": "IDENTITY"}}], "sortFields": [{"name": "ts"}],
               "partitionDistributionStrategy": "STRIPED", "reflectionMode": "Manual", "entityType": "reflection"})
    }

    fn raw_json() -> Value {
        json!({"id": "448c11cd-39c1-4c53-8bb3-d07df3493b55", "type": "RAW", "name": "r2", "tag": "t", "datasetId": "9534f3ff-ba65-4f91-a1f7-e523d93a6c92",
               "enabled": true, "displayFields": [{"name": "a"}, {"name": "ts"}, {"name": "c"}],
               "partitionFields": [{"name": "ts", "transform": {"type": "DAY"}}, {"name": "a", "transform": {"type": "BUCKET", "bucketTransform": {"bucketCount": 4}}},
                                   {"name": "c", "transform": {"type": "TRUNCATE", "truncateTransform": {"truncateLength": 2}}}],
               "distributionFields": [{"name": "b"}], "sortFields": [{"name": "a"}], "partitionDistributionStrategy": "CONSOLIDATED"})
    }

    fn cols() -> Vec<String> {
        ["a", "b", "ts", "c"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// The form's values for a reflection, as the app sends them back.
    fn values_of(f: &ObjectForm) -> FormValues {
        let mut lists = BTreeMap::new();
        for s in &f.sections {
            for l in &s.lists {
                lists.insert(l.key.clone(), l.rows.clone());
            }
        }
        FormValues {
            values: f.values.clone(),
            lists,
        }
    }

    fn existing(r: &Value) -> Existing {
        Existing {
            def: def_of(r),
            tag: "t".into(),
            dataset_label: "$scratch.p".into(),
            columns: cols(),
            state: vec![("state_status", "Estado", "CAN_ACCELERATE".into())],
            external: false,
        }
    }

    #[test]
    fn rest_definitions_are_read() {
        let d = def_of(&agg_json()).unwrap();
        assert_eq!(d.kind, RKind::Aggregation);
        assert_eq!(
            d.dimensions,
            vec![("ts".into(), "DATE".into()), ("c".into(), "NORMAL".into())]
        );
        assert_eq!(
            d.measures[0].1,
            vec!["SUM", "COUNT", "APPROX_COUNT_DISTINCT"]
        );
        assert_eq!(d.strategy, "STRIPED");
        let r = def_of(&raw_json()).unwrap();
        assert_eq!(
            r.partitions[1],
            Partition {
                name: "a".into(),
                transform: "BUCKET".into(),
                arg: Some(4)
            }
        );
        assert_eq!(r.partitions[2].arg, Some(2));
        assert!(def_of(&json!({"type": "EXTERNAL"})).is_none());
    }

    #[test]
    fn create_statements() {
        let path = vec!["$scratch".to_string(), "p".to_string()];
        let r = def_of(&raw_json()).unwrap();
        assert_eq!(
            create_sql(&sql_path(&path), &r).unwrap(),
            "ALTER TABLE \"$scratch\".\"p\" CREATE RAW REFLECTION \"r2\" USING DISPLAY (\"a\", \"ts\", \"c\") DISTRIBUTE BY (\"b\") CONSOLIDATED PARTITION BY (DAY(\"ts\"), BUCKET(4, \"a\"), TRUNCATE(2, \"c\")) LOCALSORT BY (\"a\");"
        );
        let a = def_of(&agg_json()).unwrap();
        assert_eq!(
            create_sql(&sql_path(&path), &a).unwrap(),
            "ALTER TABLE \"$scratch\".\"p\" CREATE AGGREGATE REFLECTION \"dbine_test_sched_agg\" USING DIMENSIONS (\"ts\" BY DAY, \"c\") MEASURES (\"b\" (SUM, COUNT, APPROXIMATE COUNT DISTINCT), \"a\" (COUNT, SUM)) STRIPED PARTITION BY (\"c\") LOCALSORT BY (\"ts\");"
        );
        // Names are quoted whatever they hold.
        let mut odd = r.clone();
        odd.name = "x\" DROP".into();
        assert!(create_sql("\"t\"", &odd)
            .unwrap()
            .contains("REFLECTION \"x\"\" DROP\" USING"));
        // SQL can't say these: the REST API does.
        let mut off = r.clone();
        off.enabled = false;
        assert!(create_sql("\"t\"", &off).is_none());
        let p = create_plan(&off, &path, "9534f3ff-ba65-4f91-a1f7-e523d93a6c92", vec![]);
        assert!(p.script.contains("-- POST /api/v3/reflection"));
        assert!(p.script.lines().all(|l| l.starts_with("--")));
        match &p.steps[0] {
            Step::Rest { body: Some(b), .. } => {
                assert_eq!(b["enabled"], json!(false));
                assert_eq!(
                    b["datasetId"],
                    json!("9534f3ff-ba65-4f91-a1f7-e523d93a6c92")
                );
                assert_eq!(
                    b["partitionFields"][1]["transform"]["bucketTransform"]["bucketCount"],
                    json!(4)
                );
            }
            s => panic!("{s:?}"),
        }
        let p = create_plan(&r, &path, "ds", vec!["old".into()]);
        assert!(p
            .script
            .starts_with("-- Reflexión nueva «r2» de $scratch.p\nALTER TABLE"));
        assert!(
            matches!(&p.steps[1], Step::FindReflection { before, .. } if before == &["old".to_string()])
        );
    }

    #[test]
    fn an_untouched_form_changes_nothing() {
        for r in [agg_json(), raw_json()] {
            let f = form_of(Some(&existing(&r)), &[]);
            let v = values_of(&f);
            let current = def_of(&r).unwrap();
            let d = wanted(&v, &cols(), Some(current.kind)).unwrap();
            assert_eq!(d, current);
            let p = alter_plan("u", &current, &d, &r);
            assert!(
                p.steps.is_empty() && p.script.contains("no hay cambios"),
                "{}",
                p.script
            );
        }
    }

    #[test]
    fn a_change_is_a_put_in_place() {
        let r = agg_json();
        let f = form_of(Some(&existing(&r)), &[]);
        let mut v = values_of(&f);
        v.values.insert("enabled".into(), json!(false));
        v.values.insert("name".into(), json!("renamed"));
        v.lists.get_mut("measures").unwrap()[1].insert("functions".into(), json!(["MIN", "max"]));
        let current = def_of(&r).unwrap();
        let d = wanted(&v, &cols(), Some(current.kind)).unwrap();
        let p = alter_plan("d215b51c-01f2-49f0-8120-74e508386c42", &current, &d, &r);
        assert_eq!(
            p.id.as_deref(),
            Some("reflection:d215b51c-01f2-49f0-8120-74e508386c42")
        );
        assert!(p
            .script
            .contains("-- PUT /api/v3/reflection/d215b51c-01f2-49f0-8120-74e508386c42"));
        assert!(p.script.contains("vuelve a materializar"));
        assert!(p.script.contains("Deshabilitarla borra sus datos"));
        let Step::Rest {
            body: Some(b),
            method,
            ..
        } = &p.steps[0]
        else {
            panic!()
        };
        assert_eq!(*method, Method::PUT);
        assert_eq!(b["name"], json!("renamed"));
        assert_eq!(b["enabled"], json!(false));
        assert_eq!(b["tag"], json!("bjVsH5RwgVI="));
        assert_eq!(
            b["measureFields"][1]["measureTypeList"],
            json!(["MIN", "MAX"])
        );
        assert!(b.get("status").is_none() && b.get("displayFields").is_none());
    }

    #[test]
    fn values_are_checked() {
        let r = raw_json();
        let f = form_of(Some(&existing(&r)), &[]);
        let base = values_of(&f);
        let check = |edit: &dyn Fn(&mut FormValues)| {
            let mut v = base.clone();
            edit(&mut v);
            wanted(&v, &cols(), Some(RKind::Raw))
        };
        assert!(check(&|v| {
            v.values.insert("display".into(), json!(["nope"]));
        })
        .is_err());
        assert!(check(&|v| {
            v.values.insert("display".into(), json!([]));
        })
        .is_err());
        assert!(check(&|v| {
            v.values.insert("sort".into(), json!(["a", "A"]));
        })
        .is_err());
        assert!(check(&|v| {
            v.values.insert("strategy".into(), json!("RANDOM"));
        })
        .is_err());
        assert!(check(&|v| {
            v.values.insert("name".into(), json!(""));
        })
        .is_err());
        assert!(check(&|v| {
            v.values.insert("name".into(), json!("a\nb"));
        })
        .is_err());
        assert!(check(&|v| {
            v.lists.get_mut("partitions").unwrap()[1].insert("argument".into(), json!(null));
        })
        .is_err());
        assert!(check(&|v| {
            v.lists.get_mut("partitions").unwrap()[0].insert("transform".into(), json!("WEEK"));
        })
        .is_err());
        assert!(check(&|v| {
            v.lists.get_mut("partitions").unwrap()[0].insert("column".into(), json!("a"));
        })
        .is_err());
        assert!(check(&|v| {
            v.lists.insert(
                "dimensions".into(),
                vec![BTreeMap::from([("column".to_string(), json!("a"))])],
            );
        })
        .is_err());
        // A new one: typed columns, case as the table spells them.
        let new = FormValues {
            values: BTreeMap::from([
                ("name".to_string(), json!("n")),
                ("reflection_type".to_string(), json!("AGGREGATION")),
                ("sort".to_string(), json!("TS")),
            ]),
            lists: BTreeMap::from([
                (
                    "dimensions".to_string(),
                    vec![BTreeMap::from([
                        ("column".to_string(), json!("C")),
                        ("granularity".to_string(), json!("NORMAL")),
                    ])],
                ),
                (
                    "measures".to_string(),
                    vec![BTreeMap::from([
                        ("column".to_string(), json!("b")),
                        ("functions".to_string(), json!(["SUM", "BOGUS"])),
                    ])],
                ),
            ]),
        };
        assert!(wanted(&new, &cols(), None).is_err(), "an unknown measure");
        let mut ok = new.clone();
        ok.lists.get_mut("measures").unwrap()[0].insert("functions".into(), json!(["SUM"]));
        let d = wanted(&ok, &cols(), None).unwrap();
        assert_eq!(d.kind, RKind::Aggregation);
        assert_eq!(d.dimensions, vec![("c".into(), "NORMAL".into())]);
        assert_eq!(d.sort, vec!["ts"]);
        let mut bad_type = ok.clone();
        bad_type
            .values
            .insert("reflection_type".into(), json!("EXTERNAL"));
        assert!(wanted(&bad_type, &cols(), None).is_err());
        let mut empty = ok.clone();
        empty.lists.clear();
        assert!(
            wanted(&empty, &cols(), None).is_err(),
            "no dimensions nor measures"
        );
        // «Columnas» hides for an aggregation but keeps what was typed
        // while it was RAW: it doesn't apply, so it's left out.
        let mut typed_before = ok.clone();
        typed_before.values.insert("display".into(), json!(["a"]));
        let d = wanted(&typed_before, &cols(), None).unwrap();
        assert!(d.display.is_empty());
        assert_eq!(d.dimensions, vec![("c".into(), "NORMAL".into())]);
    }

    #[test]
    fn the_form_shows_what_the_type_has() {
        let f = form_of(Some(&existing(&raw_json())), &[]);
        assert!(
            f.sections[1].lists.is_empty(),
            "a RAW one has no dimensions"
        );
        assert!(f.sections[1].fields.iter().any(|x| x.key == "display"));
        assert_eq!(f.values["display"], json!(["a", "ts", "c"]));
        assert_eq!(f.choices["column"].len(), 4);
        assert!(f.notes.iter().any(|n| n.contains("API REST")));
        let f = form_of(Some(&existing(&agg_json())), &[]);
        assert!(f.sections[1].fields.is_empty());
        assert_eq!(f.sections[1].lists.len(), 2);
        assert_eq!(f.values["state_status"], json!("CAN_ACCELERATE"));
        let aggregation_only = Some((
            "reflection_type".to_string(),
            vec!["AGGREGATION".to_string()],
        ));
        assert!(f.sections[1]
            .lists
            .iter()
            .all(|l| l.when == aggregation_only));
        let f = form_of(None, &[("\"$scratch\".\"p\"".into(), "$scratch.p".into())]);
        assert_eq!(f.choices["dataset"].len(), 1);
        assert_eq!(f.sections[1].lists.len(), 2);
        // A new one shows its type's: the lists only for an aggregation.
        assert!(f.sections[1]
            .lists
            .iter()
            .all(|l| l.when == aggregation_only));
        assert!(matches!(f.sections[1].fields[0].kind, FormKind::Text));
        let mut e = existing(&agg_json());
        e.external = true;
        assert!(form_of(Some(&e), &[]).read_only);
    }

    #[test]
    fn the_drop_is_by_id() {
        let s = drop_sql(
            &["$scratch".into(), "p".into()],
            "d215b51c-01f2-49f0-8120-74e508386c42",
            "r",
            "$scratch.p",
        );
        assert!(s.0.ends_with("ALTER TABLE \"$scratch\".\"p\" DROP REFLECTION \"d215b51c-01f2-49f0-8120-74e508386c42\";\n"));
        assert!(!s.1.contains("--") && !s.1.ends_with(';'));
    }
}
