//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]):
//! what `information_schema.ins_databases` reports and `ALTER DATABASE`
//! changes on TDengine 3.
//!
//! Only the parameters the server reports are offered, so each version
//! shows what it has: shared storage is `S3_*` up to 3.3.6 and `SS_*`
//! afterwards; `CACHESHARDBITS`, `ALLOW_DROP` and `SECURE_DELETE` come
//! with later versions. `MAXROWS` and `KEEP_TIME_OFFSET` are shown as
//! facts: 3.3 refuses to alter the first and takes the second without
//! applying it.
//!
//! One statement per change (TDengine takes several options in one, but
//! then a refusal doesn't say which); the WAL goes first (a replica change
//! needs `WAL_LEVEL` above 0) and the replica last (it moves vnodes).

use crate::create_db::span;
use crate::ddl::{lit, q};
use crate::TdSession;
use dbine_driver::{DatabaseProperties, Error, Field, FieldKind, PropertyInfo, Result};
use serde_json::Value;
use std::collections::BTreeMap;

const STORAGE: &str = "Almacenamiento";
const MEMORY: &str = "Memoria y caché";
const WAL: &str = "WAL";
const FILES: &str = "Archivos";
const COMPACT: &str = "Compactación";
const SHARED: &str = "Almacenamiento compartido";
const SAFETY: &str = "Seguridad";
const ENTERPRISE: &str = "Solo tiene efecto en TDengine Enterprise.";

/// A numeric parameter: field key (= `ins_databases` column), keyword,
/// label, range, tab.
type Number = (&'static str, &'static str, &'static str, (i64, i64), &'static str);

const NUMBERS: &[Number] = &[
    ("buffer", "BUFFER", "Memoria de escritura por vnode (BUFFER, MB)", (3, 16384), MEMORY),
    ("pages", "PAGES", "Páginas de metadatos por vnode (PAGES)", (64, i32::MAX as i64), MEMORY),
    ("cachesize", "CACHESIZE", "Memoria de la caché por vnode (CACHESIZE, MB)", (1, 65536), MEMORY),
    ("cacheshardbits", "CACHESHARDBITS", "Fragmentos de la caché (CACHESHARDBITS, 2^n)", (-1, 19), MEMORY),
    ("wal_fsync_period", "WAL_FSYNC_PERIOD", "Período de fsync del WAL (WAL_FSYNC_PERIOD, ms)", (0, 180_000), WAL),
    ("wal_retention_period", "WAL_RETENTION_PERIOD", "Retención extra del WAL para suscripciones (WAL_RETENTION_PERIOD, s)", (-1, i32::MAX as i64), WAL),
    ("wal_retention_size", "WAL_RETENTION_SIZE", "Tamaño extra del WAL para suscripciones (WAL_RETENTION_SIZE, KB)", (-1, i64::MAX), WAL),
    ("minrows", "MINROWS", "Filas mínimas por bloque (MINROWS)", (10, 1_000_000), FILES),
    ("stt_trigger", "STT_TRIGGER", "Archivos STT antes de fusionar (STT_TRIGGER)", (1, 16), FILES),
];

/// The 0/1 switches: key (= column), keyword, label, tab.
const SWITCHES: &[(&str, &str, &str, &str)] = &[
    ("s3_compact", "S3_COMPACT", "Compactar antes de migrar (S3_COMPACT)", SHARED),
    ("ss_compact", "SS_COMPACT", "Compactar antes de migrar (SS_COMPACT)", SHARED),
    ("allow_drop", "ALLOW_DROP", "Se puede borrar (ALLOW_DROP)", SAFETY),
    ("secure_delete", "SECURE_DELETE", "Borrado seguro (SECURE_DELETE)", SAFETY),
];

fn bad(what: &str, v: &str) -> Error {
    Error::Query(format!("{what}: «{v}» no es un valor válido"))
}

fn yes(v: &str) -> bool {
    matches!(v, "true" | "1")
}

/// `KEEP`: up to three spans, `keep0 <= keep1 <= keep2` (the server checks
/// the order and the minimum of 3 × DURATION).
fn keep(v: &str) -> Result<String> {
    let list: Vec<&str> = v.split(',').map(str::trim).collect();
    if list.len() > 3 {
        return Err(bad("retención", v));
    }
    Ok(list.iter().map(|p| span(p, "retención")).collect::<Result<Vec<_>>>()?.join(","))
}

/// Hours 0–23, with or without `h`.
fn hours(v: &str, what: &str) -> Result<u32> {
    let n = v.strip_suffix(['h', 'H']).unwrap_or(v);
    n.parse::<u32>().ok().filter(|n| *n <= 23).ok_or_else(|| bad(what, v))
}

/// `COMPACT_TIME_RANGE`: two offsets into the past (`-300d,-20d`, `0,0`).
fn time_range(v: &str) -> Result<String> {
    let parts: Vec<&str> = v.split(',').map(str::trim).collect();
    if parts.len() != 2 {
        return Err(bad("rango de compactación", v));
    }
    let one = |p: &str| -> Result<String> {
        if p == "0" {
            return Ok("0".into());
        }
        let s = p.strip_prefix('-').ok_or_else(|| bad("rango de compactación", v))?;
        Ok(format!("-{}", span(s, "rango de compactación")?))
    };
    Ok(format!("{},{}", one(parts[0])?, one(parts[1])?))
}

/// The statements for `changes`, in a safe order: the WAL level first, the
/// other settings, the replica last.
pub(crate) fn alter(database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    if database.trim().is_empty() {
        return Err(Error::Query("falta el nombre de la base".into()));
    }
    let db = q(database);
    let set = |what: String| format!("ALTER DATABASE {db} {what}");
    let (mut first, mut out, mut last) = (Vec::new(), Vec::new(), Vec::new());
    for (key, value) in changes {
        let value = value.trim();
        match key.as_str() {
            "keep" => out.push(set(format!("KEEP {}", keep(value)?))),
            "cachemodel" => {
                if !matches!(value, "none" | "last_row" | "last_value" | "both") {
                    return Err(bad("caché del último dato", value));
                }
                out.push(set(format!("CACHEMODEL {}", lit(value))));
            }
            "wal_level" => {
                if !matches!(value, "0" | "1" | "2") {
                    return Err(bad("nivel del WAL", value));
                }
                first.push(set(format!("WAL_LEVEL {value}")));
            }
            "replica" => {
                if !matches!(value, "1" | "2" | "3") {
                    return Err(bad("réplicas", value));
                }
                last.push(set(format!("REPLICA {value}")));
            }
            "compact_interval" => {
                let v = if value == "0" { "0".to_string() } else { span(value, "intervalo de compactación")? };
                out.push(set(format!("COMPACT_INTERVAL {v}")));
            }
            "compact_time_range" => out.push(set(format!("COMPACT_TIME_RANGE {}", time_range(value)?))),
            "compact_time_offset" => out.push(set(format!("COMPACT_TIME_OFFSET {}", hours(value, "hora de compactación")?))),
            "s3_keeplocal" | "ss_keeplocal" => out.push(set(format!("{} {}", key.to_ascii_uppercase(), span(value, "tiempo local")?))),
            k => {
                if let Some((_, kw, _, (lo, hi), _)) = NUMBERS.iter().find(|n| n.0 == k) {
                    let n = value.parse::<i64>().ok().filter(|n| (*lo..=*hi).contains(n)).ok_or_else(|| bad(kw, value))?;
                    out.push(set(format!("{kw} {n}")));
                } else if let Some((_, kw, _, _)) = SWITCHES.iter().find(|s| s.0 == k) {
                    out.push(set(format!("{kw} {}", if yes(value) { 1 } else { 0 })));
                } else {
                    return Err(Error::Query(format!("propiedad desconocida: {k}")));
                }
            }
        }
    }
    Ok(first.into_iter().chain(out).chain(last).collect())
}

pub(crate) fn script(database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(database, changes)?.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n"))
}

fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "1" } else { "0" }.to_string(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

/// `144000m` as `100d`, `1440m` as `1d`; anything else unchanged.
fn tidy_span(v: &str) -> String {
    match v.strip_suffix('m').and_then(|n| n.parse::<u64>().ok()) {
        Some(n) if n > 0 && n.is_multiple_of(1440) => format!("{}d", n / 1440),
        Some(n) if n > 0 && n.is_multiple_of(60) => format!("{}h", n / 60),
        _ => v.to_string(),
    }
}

/// `3650d,3650d,3650d` as `3650d` (the community edition repeats one).
fn tidy_keep(v: &str) -> String {
    let parts: Vec<String> = v.split(',').map(|p| tidy_span(p.trim())).collect();
    if parts.windows(2).all(|w| w[0] == w[1]) {
        parts.first().cloned().unwrap_or_default()
    } else {
        parts.join(",")
    }
}

impl TdSession {
    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let rows = self.records(&format!("SELECT * FROM information_schema.ins_databases WHERE name = {}", lit(database))).await?;
        let r = rows.into_iter().next().ok_or_else(|| Error::Query(format!("no existe la base «{database}»")))?;
        let has = |k: &str| r.contains_key(k);
        let col = |k: &str| text(r.get(k));
        let mut values = BTreeMap::new();
        let mut fields = Vec::new();
        let mut push = |f: Field, v: String| {
            values.insert(f.key.to_string(), v);
            fields.push(f);
        };

        if has("keep") {
            push(
                Field::new("keep", "Retención (KEEP)", FieldKind::Text)
                    .placeholder("3650d")
                    .help("Con unidad (m, h, d) o en días; al menos 3 veces DURATION. Hasta tres valores separados por comas (niveles de almacenamiento de Enterprise)."),
                tidy_keep(&col("keep")),
            );
        }
        if has("replica") {
            push(
                Field::new("replica", "Réplicas (REPLICA)", FieldKind::Select(vec![("1", "1"), ("2", "2"), ("3", "3")]))
                    .help("No puede superar la cantidad de dnodes. De 1 se puede pasar a 2 (Enterprise) o 3; de 2 no se cambia."),
                col("replica"),
            );
        }
        if has("cachemodel") {
            push(
                Field::new(
                    "cachemodel",
                    "Caché del último dato (CACHEMODEL)",
                    FieldKind::Select(vec![
                        ("none", "Ninguna (none)"),
                        ("last_row", "Última fila (last_row)"),
                        ("last_value", "Último valor de cada columna (last_value)"),
                        ("both", "Ambas (both)"),
                    ]),
                )
                .group(MEMORY),
                col("cachemodel"),
            );
        }
        if has("wal_level") {
            push(
                Field::new(
                    "wal_level",
                    "Nivel del WAL (WAL_LEVEL)",
                    FieldKind::Select(vec![("0", "0: sin WAL"), ("1", "1: escribe sin fsync"), ("2", "2: escribe y hace fsync")]),
                )
                .group(WAL),
                col("wal_level"),
            );
        }
        for &(key, _, label, _, group) in NUMBERS {
            if has(key) {
                let help = match key {
                    "wal_fsync_period" => "Con nivel 2; 0 hace fsync en cada escritura.",
                    "wal_retention_period" => "Para las suscripciones (TMQ); -1: sin límite.",
                    "wal_retention_size" => "Para las suscripciones (TMQ); 0 o -1: sin límite.",
                    "cacheshardbits" => "-1: lo calcula el servidor según CACHESIZE.",
                    _ => "",
                };
                push(Field::new(key, label, FieldKind::Number).help(help).group(group), col(key));
            }
        }
        for (key, label, help) in [
            ("compact_interval", "Intervalo de la compactación automática (COMPACT_INTERVAL)", "0: no compacta sola. Con unidad (m, h, d), entre 10m y el KEEP más largo."),
            ("compact_time_range", "Rango que compacta (COMPACT_TIME_RANGE)", "Dos desplazamientos al pasado, como -300d,-20d; 0,0: de -KEEP2 a -DURATION."),
            ("compact_time_offset", "Hora de la compactación (COMPACT_TIME_OFFSET)", "Horas después de la medianoche local (0 a 23)."),
        ] {
            if has(key) {
                push(Field::new(key, label, FieldKind::Text).help(help).group(COMPACT), col(key));
            }
        }
        for key in ["s3_keeplocal", "ss_keeplocal"] {
            if has(key) {
                let label = if key == "s3_keeplocal" { "Tiempo local antes de migrar (S3_KEEPLOCAL)" } else { "Tiempo local antes de migrar (SS_KEEPLOCAL)" };
                push(
                    Field::new(key, label, FieldKind::Text).placeholder("365d").help("Con unidad (m, h, d); al menos 3 veces DURATION.").group(SHARED),
                    tidy_span(&col(key)),
                );
            }
        }
        for &(key, _, label, group) in SWITCHES {
            if has(key) {
                let help = if group == SHARED { ENTERPRISE } else { "" };
                push(Field::new(key, label, FieldKind::Bool).help(help).group(group), if yes(&col(key)) { "true".into() } else { String::new() });
            }
        }

        // Facts: what can't be changed after the create, and the state.
        let mut info = Vec::new();
        let mut fact = |group: &str, label: &str, v: String| info.push(PropertyInfo { group: group.into(), label: label.into(), value: v });
        fact("", "Estado", col("status"));
        fact("", "Creada", crate::timestamp(&col("create_time")));
        fact("", "Precisión del tiempo (PRECISION)", col("precision"));
        fact("", "Vgroups (VGROUPS)", col("vgroups"));
        fact("", "Tablas", col("ntables"));
        if let Ok(rows) = self.strings(&format!("SELECT count(*) FROM information_schema.ins_stables WHERE db_name = {}", lit(database))).await {
            fact("", "Supertablas", rows.first().and_then(|r| r.first()).cloned().unwrap_or_default());
        }
        for (k, label) in [
            ("keep_time_offset", "Demora del borrado de lo vencido (KEEP_TIME_OFFSET, horas)"),
            ("duration", "Días por archivo (DURATION)"), ("single_stable", "Un solo supertable (SINGLE_STABLE)"), ("strict", "Modo estricto (STRICT)")] {
            if has(k) {
                fact(STORAGE, label, col(k));
            }
        }
        if let Ok(rows) = self.strings(&format!("SHOW {}.DISK_INFO", q(database))).await {
            for r in rows {
                if let Some((k, v)) = r.first().and_then(|s| s.split_once('=')) {
                    let label = match k {
                        "Disk_occupied" => "Espacio en disco",
                        "Compress_ratio" => "Tasa de compresión",
                        _ => continue,
                    };
                    fact(STORAGE, label, v.trim_matches(['[', ']']).to_string());
                }
            }
        }
        for (k, label) in [("pagesize", "Tamaño de página (PAGESIZE, KB)"), ("tsdb_pagesize", "Tamaño de página TSDB (TSDB_PAGESIZE, KB)")] {
            if has(k) {
                fact(MEMORY, label, col(k));
            }
        }
        for (k, label) in [("wal_roll_period", "Período de rotación (WAL_ROLL_PERIOD)"), ("wal_segment_size", "Tamaño de segmento (WAL_SEGMENT_SIZE)")] {
            if has(k) {
                fact(WAL, label, col(k));
            }
        }
        for (k, label) in [
            ("maxrows", "Filas máximas por bloque (MAXROWS)"),
            ("comp", "Compresión (COMP)"),
            ("table_prefix", "Prefijo ignorado al repartir tablas (TABLE_PREFIX)"),
            ("table_suffix", "Sufijo ignorado al repartir tablas (TABLE_SUFFIX)"),
        ] {
            if has(k) {
                fact(FILES, label, col(k));
            }
        }
        for (k, label) in [("s3_chunkpages", "Páginas por fragmento (S3_CHUNKPAGES)"), ("ss_chunkpages", "Páginas por fragmento (SS_CHUNKPAGES)")] {
            if has(k) {
                fact(SHARED, label, col(k));
            }
        }
        for (k, label) in [("encrypt_algorithm", "Cifrado (ENCRYPT_ALGORITHM)"), ("with_arbitrator", "Con árbitro (WITH_ARBITRATOR)")] {
            if has(k) {
                fact(SAFETY, label, col(k));
            }
        }

        let mut warnings = BTreeMap::new();
        let mut warn = |k: &str, w: &str| {
            warnings.insert(k.to_string(), w.to_string());
        };
        warn("keep", "Si la retención nueva es más corta, se borran los datos más viejos que ella.");
        warn("replica", "Cambiar las réplicas crea o mueve vnodes entre dnodes: copia los datos de cada vgroup y carga el cluster mientras dura.");
        warn("wal_level", "Con 0 no hay WAL: lo que esté en memoria se pierde si el servidor se cae. Con 1 no hay fsync: se pueden perder las últimas escrituras ante un corte de energía.");
        warn("wal_fsync_period", "Un período más largo deja más escrituras en riesgo ante un corte de energía.");
        warn("cachemodel", "Cambiar el modelo de caché de un lado a otro puede dar resultados inexactos en LAST y LAST_ROW.");
        warn("cacheshardbits", "Invalida la caché de últimos valores de todos los vnodes: las consultas la recargan desde disco.");
        warn("wal_retention_period", "Si se acorta, los consumidores de suscripciones atrasados pueden perder datos.");
        warn("wal_retention_size", "Si se achica, los consumidores de suscripciones atrasados pueden perder datos.");
        warn("allow_drop", "Con 0, nadie puede borrar la base hasta volver a permitirlo.");
        warnings.retain(|k, _| values.contains_key(k));
        Ok(DatabaseProperties { fields, values, info, choices: Vec::new(), warnings })
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        let statements = alter(database, changes)?;
        for (i, sql) in statements.iter().enumerate() {
            if let Err(e) = self.query(sql).await {
                return Err(if i == 0 { e } else { Error::Query(format!("se aplicaron {i} de {} cambios; falló: {sql}\n{e}", statements.len())) });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn one_statement_per_change_in_a_safe_order() {
        assert_eq!(
            script(
                "planta",
                &c(&[
                    ("replica", "3"),
                    ("keep", "365D"),
                    ("cachemodel", "last_row"),
                    ("wal_level", "2"),
                    ("buffer", "128"),
                    ("s3_compact", ""),
                    ("compact_time_range", "-300d, -20d"),
                ])
            )
            .unwrap(),
            "ALTER DATABASE `planta` WAL_LEVEL 2;
ALTER DATABASE `planta` BUFFER 128;
ALTER DATABASE `planta` CACHEMODEL 'last_row';
ALTER DATABASE `planta` COMPACT_TIME_RANGE -300d,-20d;
ALTER DATABASE `planta` KEEP 365d;
ALTER DATABASE `planta` S3_COMPACT 0;
ALTER DATABASE `planta` REPLICA 3;"
        );
        assert_eq!(script("a`b", &c(&[("keep", "30d,60d,90d"), ("ss_keeplocal", "100d")])).unwrap(), "ALTER DATABASE `a``b` KEEP 30d,60d,90d;\nALTER DATABASE `a``b` SS_KEEPLOCAL 100d;");
        assert_eq!(script("p", &c(&[("compact_interval", "0"), ("wal_retention_period", "-1"), ("allow_drop", "true")])).unwrap(),
            "ALTER DATABASE `p` ALLOW_DROP 1;\nALTER DATABASE `p` COMPACT_INTERVAL 0;\nALTER DATABASE `p` WAL_RETENTION_PERIOD -1;");
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("keep", "1y"),
            ("keep", "1d,2d,3d,4d"),
            ("keep", "10d; DROP DATABASE x"),
            ("replica", "4"),
            ("buffer", "2"),
            ("cachemodel", "all"),
            ("cachemodel", "none'; DROP DATABASE x; --"),
            ("wal_level", "3"),
            ("wal_fsync_period", "200000"),
            ("stt_trigger", "0"),
            ("keep_time_offset", "2"),
            ("compact_time_range", "-1d"),
            ("compact_time_range", "1d,2d"),
            ("compact_interval", "1w"),
            ("maxrows", "8192"),
            ("precision", "us"),
        ] {
            assert!(script("p", &c(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script(" ", &c(&[("keep", "1d")])).is_err());
    }

    #[test]
    fn values_as_shown() {
        assert_eq!(tidy_keep("3650d,3650d,3650d"), "3650d");
        assert_eq!(tidy_keep("30d,60d,90d"), "30d,60d,90d");
        assert_eq!(tidy_span("144000m"), "100d");
        assert_eq!(tidy_span("90m"), "90m");
        assert_eq!(text(Some(&Value::Bool(true))), "1");
    }
}
