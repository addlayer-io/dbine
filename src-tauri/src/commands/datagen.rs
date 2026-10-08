//! "Generar datos de prueba…" on a table (docs/datos-de-prueba.md): rows of
//! made-up but plausible values, inserted in batches through the driver's
//! `insert_script` on a session of its own, like an import. Works on every
//! engine that inserts from DBine; the generators live here, not in the
//! drivers.
//!
//! Each column gets a generator: chosen by the user, or "auto" from its
//! name and type. Identity / auto-increment columns are skipped, primary
//! keys stay unique, foreign keys take values the parent table already has,
//! and texts respect the column's length.

use crate::commands::schema::driver_of;
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use chrono::{Duration as Days, NaiveDate};
use dbine_driver::{ColumnInfo, ObjectRef, QueryOutcome};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};

const PROGRESS_EVERY: Duration = Duration::from_millis(300);
/// Values read from a parent table for a foreign key.
const FK_SAMPLE: u32 = 1000;
/// Rows at most per run.
const MAX_ROWS: u64 = 10_000_000;

// -- random numbers --------------------------------------------------------------------

/// xorshift64*: fast, good enough for test data, no dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: Option<u64>) -> Self {
        let s = seed.unwrap_or_else(|| uuid::Uuid::new_v4().as_u64_pair().0);
        Rng(s | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// In `[lo, hi]`.
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        lo + (self.next() % ((hi - lo) as u64 + 1)) as i64
    }
    fn float(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next() % items.len() as u64) as usize]
    }
    fn percent(&mut self, p: u8) -> bool {
        p > 0 && (self.next() % 100) < p as u64
    }
}

const FIRST: &[&str] = &[
    "Ana", "Juan", "María", "Lucas", "Sofía", "Mateo", "Valentina", "Martín", "Camila", "Diego", "Lucía", "Tomás", "Julieta", "Nicolás",
    "Emma", "Liam", "Olivia", "Noah", "Mia", "James", "Laura", "Pedro", "Carla", "Pablo", "Elena", "Andrés", "Paula", "Bruno",
];
const LAST: &[&str] = &[
    "García", "Fernández", "González", "Rodríguez", "López", "Martínez", "Pérez", "Gómez", "Díaz", "Romero", "Sosa", "Torres", "Álvarez",
    "Ruiz", "Smith", "Johnson", "Brown", "Rossi", "Silva", "Costa", "Moreau", "Bianchi", "Müller", "Navarro",
];
const CITIES: &[&str] = &[
    "Buenos Aires", "Córdoba", "Rosario", "Montevideo", "Santiago", "Lima", "Bogotá", "Ciudad de México", "Madrid", "Barcelona", "São Paulo",
    "Lisboa", "Roma", "París", "Nueva York", "Londres",
];
const COUNTRIES: &[&str] = &["Argentina", "Uruguay", "Chile", "Perú", "Colombia", "México", "España", "Brasil", "Portugal", "Italia", "Francia", "Estados Unidos"];
const COMPANIES: &[&str] = &["Acme", "Globex", "Initech", "Umbrella", "Stark", "Wayne", "Hooli", "Vandelay", "Soylent", "Tyrell", "Cyberdyne", "Wonka"];
const STREETS: &[&str] = &["San Martín", "Belgrano", "Rivadavia", "Sarmiento", "Mitre", "Corrientes", "Main St", "Oak Ave", "Gran Vía", "Rua Augusta"];
const WORDS: &[&str] = &[
    "lorem", "ipsum", "dolor", "sit", "amet", "datos", "prueba", "cliente", "venta", "pedido", "rápido", "nuevo", "estado", "valor", "total",
    "nota", "general", "activo", "pendiente", "servicio", "producto", "detalle", "orden", "registro",
];
const DOMAINS: &[&str] = &["ejemplo.com", "example.org", "correo.test", "mail.test"];

// -- generators ------------------------------------------------------------------------

/// A column's generator as the user set it ("auto" when not).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ColumnGen {
    pub name: String,
    #[serde(default)]
    pub generator: String,
    /// Generator settings: `min`, `max`, `start`, `step`, `values`, `value`,
    /// `scale`, `words`…
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    /// Share of NULLs (nullable columns only).
    #[serde(default)]
    pub null_percent: u8,
}

/// What a column becomes once its generator is resolved.
#[derive(Debug, Clone)]
enum Gen {
    /// Left out of the INSERT (identity, defaults).
    Skip,
    Null,
    Fixed(String),
    Seq { next: i64, step: i64 },
    Int { min: i64, max: i64 },
    Dec { min: f64, max: f64, scale: u32 },
    Bool,
    Date { from: NaiveDate, days: i64, time: bool },
    Uuid,
    Text { min: usize, max: usize },
    First,
    Last,
    Full,
    Email,
    Phone,
    City,
    Country,
    Company,
    Address,
    List(Vec<String>),
    /// Values taken from the parent of a foreign key.
    Pool(Vec<Value>),
}

/// The generator ids the dialog offers (besides "auto").
pub const GENERATORS: &[&str] = &[
    "skip", "null", "fixed", "sequence", "integer", "decimal", "boolean", "date", "datetime", "uuid", "text", "first_name", "last_name",
    "full_name", "email", "phone", "city", "country", "company", "address", "list", "foreign_key",
];

/// A column, with what the catalog says about it.
#[derive(Debug, Clone)]
struct Meta {
    info: ColumnInfo,
    /// The parent's values when it's a foreign key.
    pool: Option<Vec<Value>>,
    /// Single-column primary key or unique: values may not repeat.
    unique: bool,
}

/// The length in `varchar(40)` / `nvarchar(max)` (None for max or none).
fn length(ty: &str) -> Option<usize> {
    let open = ty.find('(')?;
    let inner = &ty[open + 1..ty[open..].find(')').map(|c| open + c)?];
    inner.split(',').next()?.trim().parse().ok()
}

/// `numeric(p, s)`'s scale.
fn scale(ty: &str) -> Option<u32> {
    let open = ty.find('(')?;
    let inner = &ty[open + 1..ty[open..].find(')').map(|c| open + c)?];
    inner.split(',').nth(1)?.trim().parse().ok()
}

/// The largest value an integer type holds (roughly, by name).
fn int_max(ty: &str) -> i64 {
    let t = ty.to_ascii_lowercase();
    if t.contains("tinyint") {
        127
    } else if t.contains("smallint") || t == "int2" {
        32_767
    } else if t.contains("bigint") || t == "int8" || t.contains("long") {
        9_000_000_000_000
    } else {
        2_000_000_000
    }
}

fn has(name: &str, words: &[&str]) -> bool {
    words.iter().any(|w| name.contains(w))
}

/// The generator "auto" picks: by name first (email, nombre…), then type.
fn auto(m: &Meta, rng: &mut Rng) -> Gen {
    if m.info.auto_increment {
        return Gen::Skip;
    }
    if let Some(pool) = &m.pool {
        return Gen::Pool(pool.clone());
    }
    let n = m.info.name.to_lowercase();
    let t = m.info.data_type.to_lowercase();
    let texty = t.contains("char") || t.contains("text") || t.contains("string") || t.is_empty() || t.contains("clob") || t == "str";
    if texty {
        if has(&n, &["email", "mail", "correo"]) {
            return Gen::Email;
        }
        if has(&n, &["first", "nombre_pila", "firstname", "given"]) {
            return Gen::First;
        }
        if has(&n, &["last", "apellido", "surname"]) {
            return Gen::Last;
        }
        if has(&n, &["phone", "tel", "celular", "movil", "móvil"]) {
            return Gen::Phone;
        }
        if has(&n, &["city", "ciudad", "localidad"]) {
            return Gen::City;
        }
        if has(&n, &["country", "pais", "país", "nacion"]) {
            return Gen::Country;
        }
        if has(&n, &["company", "empresa", "compania", "compañia", "razon"]) {
            return Gen::Company;
        }
        if has(&n, &["address", "direccion", "dirección", "calle", "domicilio"]) {
            return Gen::Address;
        }
        if has(&n, &["uuid", "guid"]) {
            return Gen::Uuid;
        }
        if has(&n, &["name", "nombre"]) {
            return Gen::Full;
        }
        if m.unique {
            return Gen::Uuid;
        }
        return Gen::Text { min: 1, max: 4 };
    }
    if t.contains("uuid") || t.contains("uniqueidentifier") {
        return Gen::Uuid;
    }
    if t.contains("bool") || t == "bit" {
        return Gen::Bool;
    }
    if t.contains("date") || t.contains("time") {
        let today = chrono::Local::now().date_naive();
        return Gen::Date { from: today - Days::days(3 * 365), days: 3 * 365, time: t.contains("time") };
    }
    if t.contains("int") || t.contains("serial") || t == "number" && scale(&t).unwrap_or(0) == 0 && !t.contains(',') {
        if m.unique {
            let max = int_max(&t);
            // A random high start keeps clear of the ids a table usually has.
            let start = rng.range(max / 4, max / 2);
            return Gen::Seq { next: start, step: 1 };
        }
        let max = int_max(&t).min(100_000);
        return Gen::Int { min: 1, max };
    }
    if t.contains("dec") || t.contains("num") || t.contains("money") || t.contains("float") || t.contains("real") || t.contains("double") {
        return Gen::Dec { min: 0.0, max: 10_000.0, scale: scale(&t).unwrap_or(2).min(6) };
    }
    if t.contains("json") || t.contains("object") || t.contains("map") || t.contains("document") {
        return Gen::Fixed("{}".into());
    }
    Gen::Text { min: 1, max: 3 }
}

fn param<T: std::str::FromStr>(g: &ColumnGen, k: &str) -> Option<T> {
    g.params.get(k).and_then(|v| v.trim().parse().ok())
}

/// The user's choice, with its settings.
fn chosen(g: &ColumnGen, m: &Meta, rng: &mut Rng) -> CommandResult<Gen> {
    let today = chrono::Local::now().date_naive();
    let date = |k: &str| g.params.get(k).and_then(|v| NaiveDate::parse_from_str(v.trim(), "%Y-%m-%d").ok());
    Ok(match g.generator.as_str() {
        "" | "auto" => auto(m, rng),
        "skip" => Gen::Skip,
        "null" => Gen::Null,
        "fixed" => Gen::Fixed(g.params.get("value").cloned().unwrap_or_default()),
        "sequence" => Gen::Seq { next: param(g, "start").unwrap_or(1), step: param(g, "step").unwrap_or(1) },
        "integer" => Gen::Int { min: param(g, "min").unwrap_or(1), max: param(g, "max").unwrap_or(1000) },
        "decimal" => Gen::Dec { min: param(g, "min").unwrap_or(0.0), max: param(g, "max").unwrap_or(1000.0), scale: param(g, "scale").unwrap_or(2) },
        "boolean" => Gen::Bool,
        "date" | "datetime" => {
            let from = date("from").unwrap_or(today - Days::days(365));
            let to = date("to").unwrap_or(today);
            Gen::Date { from, days: (to - from).num_days().max(0), time: g.generator == "datetime" }
        }
        "uuid" => Gen::Uuid,
        "text" => Gen::Text { min: param(g, "min").unwrap_or(1), max: param(g, "max").unwrap_or(5) },
        "first_name" => Gen::First,
        "last_name" => Gen::Last,
        "full_name" => Gen::Full,
        "email" => Gen::Email,
        "phone" => Gen::Phone,
        "city" => Gen::City,
        "country" => Gen::Country,
        "company" => Gen::Company,
        "address" => Gen::Address,
        "list" => {
            let values: Vec<String> = g.params.get("values").map(|v| v.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()).unwrap_or_default();
            if values.is_empty() {
                return Err(CommandError::BadRequest(format!("«{}»: la lista de valores está vacía", g.name)));
            }
            Gen::List(values)
        }
        "foreign_key" => match &m.pool {
            Some(p) if !p.is_empty() => Gen::Pool(p.clone()),
            _ => return Err(CommandError::BadRequest(format!("«{}»: la tabla referenciada no tiene filas para tomar valores", g.name))),
        },
        other => return Err(CommandError::BadRequest(format!("«{}»: generador desconocido «{other}»", g.name))),
    })
}

/// The auto generator's id, for the dialog to show what "auto" means.
fn id_of(g: &Gen) -> &'static str {
    match g {
        Gen::Skip => "skip",
        Gen::Null => "null",
        Gen::Fixed(_) => "fixed",
        Gen::Seq { .. } => "sequence",
        Gen::Int { .. } => "integer",
        Gen::Dec { .. } => "decimal",
        Gen::Bool => "boolean",
        Gen::Date { time: false, .. } => "date",
        Gen::Date { time: true, .. } => "datetime",
        Gen::Uuid => "uuid",
        Gen::Text { .. } => "text",
        Gen::First => "first_name",
        Gen::Last => "last_name",
        Gen::Full => "full_name",
        Gen::Email => "email",
        Gen::Phone => "phone",
        Gen::City => "city",
        Gen::Country => "country",
        Gen::Company => "company",
        Gen::Address => "address",
        Gen::List(_) => "list",
        Gen::Pool(_) => "foreign_key",
    }
}

fn clip(s: String, max: Option<usize>) -> String {
    match max {
        Some(n) if s.chars().count() > n => s.chars().take(n).collect(),
        _ => s,
    }
}

/// One value; `row` makes unique texts (emails) distinct.
fn value(g: &mut Gen, rng: &mut Rng, row: u64, len: Option<usize>) -> Value {
    let s = |v: String| Value::String(clip(v, len));
    match g {
        Gen::Skip | Gen::Null => Value::Null,
        Gen::Fixed(v) => s(v.clone()),
        Gen::Seq { next, step } => {
            let v = *next;
            *next += *step;
            json!(v)
        }
        Gen::Int { min, max } => json!(rng.range(*min, *max)),
        Gen::Dec { min, max, scale } => {
            let v = *min + rng.float() * (*max - *min);
            let f = 10f64.powi(*scale as i32);
            json!((v * f).round() / f)
        }
        Gen::Bool => json!(rng.next() % 2 == 0),
        Gen::Date { from, days, time } => {
            let d = *from + Days::days(rng.range(0, *days));
            if *time {
                Value::String(format!("{} {:02}:{:02}:{:02}", d.format("%Y-%m-%d"), rng.range(0, 23), rng.range(0, 59), rng.range(0, 59)))
            } else {
                Value::String(d.format("%Y-%m-%d").to_string())
            }
        }
        Gen::Uuid => Value::String(uuid::Uuid::from_u64_pair(rng.next(), rng.next()).to_string()),
        Gen::Text { min, max } => {
            let n = rng.range(*min as i64, (*max).max(*min) as i64) as usize;
            let words: Vec<&str> = (0..n.max(1)).map(|_| *rng.pick(WORDS)).collect();
            let mut t = words.join(" ");
            if let Some(c) = t.get_mut(0..1) {
                c.make_ascii_uppercase();
            }
            s(t)
        }
        Gen::First => s(rng.pick(FIRST).to_string()),
        Gen::Last => s(rng.pick(LAST).to_string()),
        Gen::Full => s(format!("{} {}", rng.pick(FIRST), rng.pick(LAST))),
        Gen::Email => {
            let user = format!("{}.{}{}", rng.pick(FIRST), rng.pick(LAST), row);
            let user: String = user.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '.').collect();
            s(format!("{user}@{}", rng.pick(DOMAINS)))
        }
        Gen::Phone => s(format!("+54 11 {:04}-{:04}", rng.range(1000, 9999), rng.range(0, 9999))),
        Gen::City => s(rng.pick(CITIES).to_string()),
        Gen::Country => s(rng.pick(COUNTRIES).to_string()),
        Gen::Company => s(format!("{} {}", rng.pick(COMPANIES), rng.pick(&["S.A.", "SRL", "Inc.", "Ltd."]))),
        Gen::Address => s(format!("{} {}", rng.pick(STREETS), rng.range(1, 9999))),
        Gen::List(v) => s(rng.pick(v).clone()),
        Gen::Pool(v) => rng.pick(v).clone(),
    }
}

/// Resolved generators, in column order, with the columns that go in the INSERT.
struct Plan {
    columns: Vec<String>,
    gens: Vec<(Gen, Option<usize>, u8, bool)>,
    /// What "auto" picked per column (all columns), for the dialog.
    auto: BTreeMap<String, String>,
}

fn plan(metas: &[Meta], specs: &[ColumnGen], rng: &mut Rng) -> CommandResult<Plan> {
    let mut out = Plan { columns: Vec::new(), gens: Vec::new(), auto: BTreeMap::new() };
    for m in metas {
        let auto_gen = auto(m, rng);
        out.auto.insert(m.info.name.clone(), id_of(&auto_gen).to_string());
        let spec = specs.iter().find(|s| s.name == m.info.name);
        let g = match spec {
            Some(s) => chosen(s, m, rng)?,
            None => auto_gen,
        };
        if matches!(g, Gen::Skip) {
            continue;
        }
        if matches!(g, Gen::Null) && !m.info.nullable {
            return Err(CommandError::BadRequest(format!("«{}» no acepta nulos", m.info.name)));
        }
        let nulls = if m.info.nullable && !m.unique { spec.map_or(0, |s| s.null_percent.min(100)) } else { 0 };
        let texty = !matches!(g, Gen::Seq { .. } | Gen::Int { .. } | Gen::Dec { .. } | Gen::Bool | Gen::Pool(_));
        let len = if texty { length(&m.info.data_type) } else { None };
        out.columns.push(m.info.name.clone());
        out.gens.push((g, len, nulls, m.unique));
    }
    if out.columns.is_empty() {
        return Err(CommandError::BadRequest("no queda ninguna columna para llenar".into()));
    }
    Ok(out)
}

/// `n` rows; unique columns get distinct values (retried a few times).
fn rows(plan: &mut Plan, rng: &mut Rng, from_row: u64, n: u64, seen: &mut [HashSet<String>]) -> Vec<Vec<Value>> {
    (0..n)
        .map(|i| {
            let row = from_row + i;
            plan.gens
                .iter_mut()
                .enumerate()
                .map(|(c, (g, len, nulls, unique))| {
                    if rng.percent(*nulls) {
                        return Value::Null;
                    }
                    let mut v = value(g, rng, row, *len);
                    if *unique {
                        for _ in 0..20 {
                            if seen[c].insert(v.to_string()) {
                                break;
                            }
                            v = value(g, rng, row, *len);
                        }
                    }
                    v
                })
                .collect()
        })
        .collect()
}

// -- commands --------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct DataGenArgs {
    pub connection_id: String,
    pub database: String,
    pub table: ObjectRef,
    #[serde(default)]
    pub columns: Vec<ColumnGen>,
    pub rows: u64,
    #[serde(default)]
    pub seed: Option<u64>,
    /// The run's id (events and `cancel_query` on `datagen:<id>`).
    #[serde(default)]
    pub gen_id: String,
    #[serde(default = "default_batch")]
    pub batch: usize,
}

fn default_batch() -> usize {
    500
}

#[derive(Serialize)]
pub struct Preview {
    /// Every column of the table, with what the catalog says and what "auto" picks.
    pub table_columns: Vec<PreviewColumn>,
    pub generators: Vec<&'static str>,
    /// The columns that go in the INSERT, and sample rows.
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Serialize)]
pub struct PreviewColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
    pub auto_increment: bool,
    pub foreign_key: Option<String>,
    pub auto: String,
}

/// The table's columns, its single-column unique keys, and each foreign
/// key's parent values (a sample).
async fn metas(s: &mut Box<dyn dbine_driver::Session>, driver: &dyn dbine_driver::Driver, table: &ObjectRef) -> CommandResult<(Vec<Meta>, BTreeMap<String, String>)> {
    let cols = s.columns(table).await?;
    if cols.is_empty() {
        return Err(CommandError::BadRequest(format!("«{}» no tiene columnas conocidas", table.name)));
    }
    let mut fks: BTreeMap<String, (ObjectRef, String)> = BTreeMap::new();
    let mut unique: HashSet<String> = cols.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
    if cols.iter().filter(|c| c.primary_key).count() > 1 {
        unique.clear();
    }
    if driver.capabilities().foreign_keys {
        if let Ok(schema) = s.database_schema().await {
            if let Some(t) = schema.iter().find(|t| t.name == table.name && (table.schema.is_none() || t.schema == table.schema)) {
                for fk in t.foreign_keys.iter().filter(|f| f.columns.len() == 1) {
                    let parent = ObjectRef { kind: "table".into(), schema: fk.ref_schema.clone().or(t.schema.clone()), name: fk.ref_table.clone() };
                    fks.insert(fk.columns[0].clone(), (parent, fk.ref_columns[0].clone()));
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut labels = BTreeMap::new();
    for c in cols {
        let pool = match fks.get(&c.name) {
            Some((parent, col)) => {
                labels.insert(c.name.clone(), format!("{}.{col}", parent.name));
                let mut o = QueryOutcome::default();
                let sql = s.browse_query(parent, FK_SAMPLE);
                s.execute(&sql, FK_SAMPLE as usize, &mut o).await?;
                let values: Vec<Value> = o
                    .results
                    .first()
                    .and_then(|r| r.columns.iter().position(|x| x.name.eq_ignore_ascii_case(col)).map(|i| r.rows.iter().map(|row| row[i].clone()).filter(|v| !v.is_null()).collect()))
                    .unwrap_or_default();
                Some(values)
            }
            None => None,
        };
        let is_unique = unique.contains(&c.name);
        out.push(Meta { info: c, pool, unique: is_unique });
    }
    Ok((out, labels))
}

#[tauri::command(rename_all = "camelCase")]
pub async fn datagen_preview(state: State<'_, AppState>, args: DataGenArgs) -> CommandResult<Preview> {
    let driver = driver_of(&state, &args.connection_id)?;
    let key = format!("datagen-preview:{}", uuid::Uuid::new_v4());
    let entry = state.dedicated_session(&key, &args.connection_id, &args.database, true).await?;
    let res = async {
        let mut s = entry.session.lock().await;
        let (metas, fk_labels) = metas(&mut s, driver.as_ref(), &args.table).await?;
        let mut rng = Rng::new(args.seed);
        let mut p = plan(&metas, &args.columns, &mut rng)?;
        let mut seen = vec![HashSet::new(); p.columns.len()];
        let sample = rows(&mut p, &mut rng, 1, args.rows.clamp(1, 20), &mut seen);
        let table_columns = metas
            .iter()
            .map(|m| PreviewColumn {
                name: m.info.name.clone(),
                data_type: m.info.data_type.clone(),
                nullable: m.info.nullable,
                primary_key: m.info.primary_key,
                auto_increment: m.info.auto_increment,
                foreign_key: fk_labels.get(&m.info.name).cloned(),
                auto: p.auto.get(&m.info.name).cloned().unwrap_or_default(),
            })
            .collect();
        Ok(Preview { table_columns, generators: GENERATORS.to_vec(), columns: p.columns.clone(), rows: sample })
    }
    .await;
    state.sessions.remove(&key);
    res
}

#[derive(Serialize, Clone)]
struct GenProgress<'a> {
    id: &'a str,
    rows: u64,
    total: u64,
}

#[derive(Serialize)]
pub struct GenResult {
    pub rows: u64,
    pub elapsed_ms: u64,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn datagen_run(app: AppHandle, state: State<'_, AppState>, args: DataGenArgs) -> CommandResult<GenResult> {
    let started = Instant::now();
    if args.rows == 0 || args.rows > MAX_ROWS {
        return Err(CommandError::BadRequest(format!("la cantidad de filas tiene que estar entre 1 y {MAX_ROWS}")));
    }
    let conn = state.store.get_connection(&args.connection_id)?.ok_or_else(|| CommandError::NotFound("conexión inexistente".into()))?;
    if conn.config.read_only {
        return Err(CommandError::BadRequest(format!("«{}» es de solo lectura: no se pueden insertar datos", conn.name)));
    }
    let driver = driver_of(&state, &args.connection_id)?;
    let key = format!("datagen:{}", args.gen_id);
    let entry = state.dedicated_session(&key, &args.connection_id, &args.database, false).await?;
    let emit = |rows: u64| {
        let _ = app.emit("datagen-progress", GenProgress { id: &args.gen_id, rows, total: args.rows });
    };
    let run = async {
        let mut s = entry.session.lock().await;
        let (metas, _) = metas(&mut s, driver.as_ref(), &args.table).await?;
        let mut rng = Rng::new(args.seed);
        let mut p = plan(&metas, &args.columns, &mut rng)?;
        let mut seen = vec![HashSet::new(); p.columns.len()];
        let batch = args.batch.clamp(1, 5000) as u64;
        let mut done = 0u64;
        let mut last = Instant::now();
        emit(0);
        while done < args.rows {
            if entry.cancelled.load(Ordering::SeqCst) {
                return Err(CommandError::Cancelled);
            }
            let n = batch.min(args.rows - done);
            let chunk = rows(&mut p, &mut rng, done + 1, n, &mut seen);
            let script = driver.insert_script(&args.table, &p.columns, &chunk)?;
            let mut out = QueryOutcome::default();
            let r = tokio::select! {
                r = s.execute(&script, 1, &mut out) => r.map_err(CommandError::from),
                _ = entry.cancel.notified() => Err(CommandError::Cancelled),
            };
            r.map_err(|e| CommandError::Sql(format!("filas {}–{}: {e}", done + 1, done + n)))?;
            if let Some(e) = out.error.take() {
                return Err(CommandError::Sql(format!("filas {}–{}: {e}", done + 1, done + n)));
            }
            done += n;
            if last.elapsed() >= PROGRESS_EVERY {
                last = Instant::now();
                emit(done);
            }
        }
        emit(done);
        Ok(done)
    };
    let res = run.await;
    state.sessions.remove(&key);
    Ok(GenResult { rows: res?, elapsed_ms: started.elapsed().as_millis() as u64 })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against a real SQL Server (`DBINE_TEST_SQLSERVER_URL`,
    /// `mssql://user:pass@host:port`): a child table with a primary key, a
    /// foreign key, a short text and dates gets 1000 rows through the same
    /// plan → rows → insert_script path the command runs.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn sqlserver_end_to_end() {
        let Ok(url) = std::env::var("DBINE_TEST_SQLSERVER_URL") else {
            eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
            return;
        };
        let rest = url.split_once("://").unwrap().1;
        let (auth, hostport) = rest.rsplit_once('@').unwrap();
        let (user, pass) = auth.split_once(':').unwrap();
        let (host, port) = hostport.split_once(':').unwrap();
        let cfg = dbine_driver::ConnectionConfig {
            driver: "sqlserver".into(),
            host: host.into(),
            port: port.trim_end_matches('/').parse().unwrap(),
            database: "master".into(),
            username: Some(user.into()),
            password: Some(pass.into()),
            trust_server_certificate: true,
            ..Default::default()
        };
        let driver = dbine_drivers::find("sqlserver").unwrap();
        let mut s = dbine_drivers::open_session(&cfg, Some("master")).await.unwrap();
        let _ = s.drop_database("dbine_datagen").await;
        s.create_database("dbine_datagen").await.unwrap();
        let mut s = dbine_drivers::open_session(&cfg, Some("dbine_datagen")).await.unwrap();
        let mut out = QueryOutcome::default();
        s.execute(
            "CREATE TABLE dbo.clientes (id int PRIMARY KEY, nombre nvarchar(60));
             INSERT INTO dbo.clientes VALUES (1, 'a'), (2, 'b'), (3, 'c');
             CREATE TABLE dbo.pedidos (id int PRIMARY KEY, cliente_id int NOT NULL REFERENCES dbo.clientes(id),
                                       email varchar(30), total decimal(10,2), creado datetime2, nota nvarchar(8) NULL);",
            10,
            &mut out,
        )
        .await
        .unwrap();
        assert!(out.error.is_none(), "{:?}", out.error);

        let table = ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "pedidos".into() };
        let (metas, labels) = metas(&mut s, driver.as_ref(), &table).await.unwrap();
        assert_eq!(labels.get("cliente_id").map(String::as_str), Some("clientes.id"));
        let mut rng = Rng::new(Some(42));
        let specs = [ColumnGen { name: "nota".into(), null_percent: 30, ..Default::default() }];
        let mut p = plan(&metas, &specs, &mut rng).unwrap();
        let mut seen = vec![HashSet::new(); p.columns.len()];
        let mut done = 0;
        while done < 1000 {
            let chunk = rows(&mut p, &mut rng, done + 1, 250, &mut seen);
            let mut o = QueryOutcome::default();
            s.execute(&driver.insert_script(&table, &p.columns, &chunk).unwrap(), 1, &mut o).await.unwrap();
            assert!(o.error.is_none(), "{:?}", o.error);
            done += 250;
        }
        let mut o = QueryOutcome::default();
        s.execute(
            "SELECT COUNT(*), COUNT(DISTINCT id), SUM(CASE WHEN cliente_id IN (1, 2, 3) THEN 1 ELSE 0 END),
                    SUM(CASE WHEN nota IS NULL THEN 1 ELSE 0 END), MAX(LEN(email))
             FROM dbo.pedidos",
            1,
            &mut o,
        )
        .await
        .unwrap();
        let r = &o.results[0].rows[0];
        let n = |i: usize| r[i].as_i64().or_else(|| r[i].as_str().and_then(|v| v.parse().ok())).unwrap();
        assert_eq!((n(0), n(1), n(2)), (1000, 1000, 1000), "count, distinct ids, valid foreign keys");
        assert!((200..400).contains(&n(3)), "about 30% NULL notes: {}", n(3));
        assert!(n(4) <= 30, "emails fit varchar(30)");
        drop(s);
        let mut m = dbine_drivers::open_session(&cfg, Some("master")).await.unwrap();
        m.drop_database("dbine_datagen").await.unwrap();
    }

    fn meta(name: &str, ty: &str, pk: bool) -> Meta {
        Meta {
            info: ColumnInfo { name: name.into(), data_type: ty.into(), nullable: !pk, primary_key: pk, auto_increment: false, default_value: None },
            pool: None,
            unique: pk,
        }
    }

    #[test]
    fn auto_by_name_and_type() {
        let mut rng = Rng::new(Some(7));
        let ids: Vec<&str> = [
            meta("email", "varchar(100)", false),
            meta("nombre", "nvarchar(50)", false),
            meta("apellido", "text", false),
            meta("total", "decimal(10,2)", false),
            meta("activo", "bit", false),
            meta("creado", "datetime2", false),
            meta("id", "int", true),
            meta("codigo", "varchar(20)", true),
        ]
        .iter()
        .map(|m| id_of(&auto(m, &mut rng)))
        .collect();
        assert_eq!(ids, ["email", "full_name", "last_name", "decimal", "boolean", "datetime", "sequence", "uuid"]);
        let mut ai = meta("id", "int", true);
        ai.info.auto_increment = true;
        assert!(matches!(auto(&ai, &mut rng), Gen::Skip));
    }

    #[test]
    fn rows_respect_length_uniqueness_and_nulls() {
        let mut rng = Rng::new(Some(1));
        let metas = [meta("id", "int", true), meta("nombre", "varchar(5)", false), meta("nota", "varchar(50)", false)];
        let specs = [ColumnGen { name: "nota".into(), generator: "null".into(), ..Default::default() }];
        let mut p = plan(&metas, &specs, &mut rng).unwrap();
        assert_eq!(p.columns, ["id", "nombre", "nota"]);
        let mut seen = vec![HashSet::new(); 3];
        let rs = rows(&mut p, &mut rng, 1, 200, &mut seen);
        let ids: HashSet<String> = rs.iter().map(|r| r[0].to_string()).collect();
        assert_eq!(ids.len(), 200, "the primary key doesn't repeat");
        assert!(rs.iter().all(|r| r[1].as_str().unwrap().chars().count() <= 5));
        assert!(rs.iter().all(|r| r[2].is_null()));
    }

    #[test]
    fn bad_settings_are_refused() {
        let mut rng = Rng::new(Some(1));
        let metas = [meta("id", "int", true)];
        let null_pk = [ColumnGen { name: "id".into(), generator: "null".into(), ..Default::default() }];
        assert!(plan(&metas, &null_pk, &mut rng).is_err());
        let empty_list = [ColumnGen { name: "id".into(), generator: "list".into(), ..Default::default() }];
        assert!(plan(&metas, &empty_list, &mut rng).is_err());
        assert_eq!(length("nvarchar(40)"), Some(40));
        assert_eq!(length("nvarchar(max)"), None);
        assert_eq!(scale("numeric(12, 4)"), Some(4));
    }
}
