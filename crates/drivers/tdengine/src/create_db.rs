//! "Nueva base de datos" with options
//! ([`dbine_driver::Driver::create_database_fields`]): the parameters of
//! TDengine 3's `CREATE DATABASE`: time precision, retention (`KEEP`),
//! file span (`DURATION`), replicas, vgroups, memory (`BUFFER`, `PAGES`,
//! `PAGESIZE`), the last-row cache, the WAL, compression and block sizes.
//! Every value is checked before it reaches the statement.

use crate::ddl::q;
use dbine_driver::{Error, Field, FieldKind, Result};
use std::collections::BTreeMap;

pub(crate) fn fields() -> Vec<Field> {
    vec![
        Field::new(
            "precision",
            "Precisión del tiempo (PRECISION)",
            FieldKind::Select(vec![("ms", "Milisegundos (ms)"), ("us", "Microsegundos (us)"), ("ns", "Nanosegundos (ns)")]),
        )
        .help("Vacía: milisegundos. No se cambia después."),
        Field::new("keep", "Retención (KEEP)", FieldKind::Text)
            .placeholder("3650d")
            .help("Vacía: 3650d. Con unidad (m, h, d) o en minutos; hasta tres valores separados por comas (frío, tibio, caliente)."),
        Field::new("duration", "Días por archivo (DURATION)", FieldKind::Text).placeholder("10d").help("Vacío: 10d. El tramo de tiempo de cada archivo de datos."),
        Field::new("replica", "Réplicas (REPLICA)", FieldKind::Select(vec![("1", "1"), ("2", "2"), ("3", "3")]))
            .help("Vacío: 1. 2 necesita un árbitro; 3, tres dnodes."),
        Field::new("vgroups", "Vgroups (VGROUPS)", FieldKind::Number).help("Vacío: 2. En cuántos grupos de nodos virtuales se reparte la base."),
        Field::new("buffer", "Memoria de escritura por vnode (BUFFER, MB)", FieldKind::Number).help("Vacía: 256 MB."),
        Field::new("pages", "Páginas de metadatos por vnode (PAGES)", FieldKind::Number),
        Field::new("pagesize", "Tamaño de página (PAGESIZE, KB)", FieldKind::Number),
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
        .help("Vacía: none."),
        Field::new("cachesize", "Memoria de la caché por vnode (CACHESIZE, MB)", FieldKind::Number).help("Vacía: 1 MB."),
        Field::new("wal_level", "Nivel del WAL (WAL_LEVEL)", FieldKind::Select(vec![("1", "1: escribe sin fsync"), ("2", "2: escribe y hace fsync")]))
            .help("Vacío: 1."),
        Field::new("wal_fsync_period", "Período de fsync del WAL (WAL_FSYNC_PERIOD, ms)", FieldKind::Number)
            .help("Vacío: 3000. Con nivel 2; 0 hace fsync en cada escritura.")
            .when("wal_level", &["2"]),
        Field::new("comp", "Compresión (COMP)", FieldKind::Select(vec![("0", "0: sin compresión"), ("1", "1: una etapa"), ("2", "2: dos etapas")]))
            .help("Vacía: 2."),
        Field::new("minrows", "Filas mínimas por bloque (MINROWS)", FieldKind::Number).help("Vacío: 100."),
        Field::new("maxrows", "Filas máximas por bloque (MAXROWS)", FieldKind::Number).help("Vacío: 4096."),
        Field::new("stt_trigger", "Archivos STT antes de fusionar (STT_TRIGGER)", FieldKind::Number).help("Vacío: 2."),
        Field::new("single_stable", "Un solo supertable (SINGLE_STABLE)", FieldKind::Bool)
            .help("La base admite un único supertable; no se cambia después."),
    ]
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

fn bad(what: &str, v: &str) -> Error {
    Error::Query(format!("{what}: «{v}» no es un valor válido"))
}

/// `10d`, `12h`, `1440m` or a bare number of minutes, as written.
fn span(v: &str, what: &str) -> Result<String> {
    let s = v.trim().to_ascii_lowercase();
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    let ok = (1..=9).contains(&digits) && matches!(&s[digits..], "" | "m" | "h" | "d");
    if ok {
        Ok(s)
    } else {
        Err(bad(what, v))
    }
}

/// Numeric parameters, in statement order: (key, keyword, what, range).
const NUMBERS: [(&str, &str, &str, (u64, u64)); 10] = [
    ("vgroups", "VGROUPS", "vgroups", (1, 1024)),
    ("buffer", "BUFFER", "memoria de escritura", (3, 16384)),
    ("pages", "PAGES", "páginas", (64, 16384)),
    ("pagesize", "PAGESIZE", "tamaño de página", (1, 16384)),
    ("cachesize", "CACHESIZE", "memoria de la caché", (1, 65536)),
    ("wal_fsync_period", "WAL_FSYNC_PERIOD", "período de fsync", (0, 180_000)),
    ("minrows", "MINROWS", "filas mínimas", (10, 1_000_000)),
    ("maxrows", "MAXROWS", "filas máximas", (200, 10_000_000)),
    ("stt_trigger", "STT_TRIGGER", "archivos STT", (1, 16)),
    ("comp", "COMP", "compresión", (0, 2)),
];

/// The statement that creates `name`.
pub(crate) fn script(name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    if name.trim().is_empty() {
        return Err(Error::Query("falta el nombre de la base".into()));
    }
    let mut parts = Vec::new();
    if let Some(p) = opt(o, "precision") {
        if !matches!(p, "ms" | "us" | "ns") {
            return Err(bad("precisión", p));
        }
        parts.push(format!("PRECISION '{p}'"));
    }
    if let Some(k) = opt(o, "keep") {
        let list: Vec<&str> = k.split(',').collect();
        if list.len() > 3 {
            return Err(bad("retención", k));
        }
        let list = list.iter().map(|v| span(v, "retención")).collect::<Result<Vec<_>>>()?;
        parts.push(format!("KEEP {}", list.join(",")));
    }
    if let Some(d) = opt(o, "duration") {
        parts.push(format!("DURATION {}", span(d, "días por archivo")?));
    }
    if let Some(r) = opt(o, "replica") {
        if !matches!(r, "1" | "2" | "3") {
            return Err(bad("réplicas", r));
        }
        parts.push(format!("REPLICA {r}"));
    }
    if let Some(c) = opt(o, "cachemodel") {
        if !matches!(c, "none" | "last_row" | "last_value" | "both") {
            return Err(bad("caché del último dato", c));
        }
        parts.push(format!("CACHEMODEL '{c}'"));
    }
    if let Some(w) = opt(o, "wal_level") {
        if !matches!(w, "1" | "2") {
            return Err(bad("nivel del WAL", w));
        }
        parts.push(format!("WAL_LEVEL {w}"));
    }
    for (key, kw, what, (lo, hi)) in NUMBERS {
        if let Some(v) = opt(o, key) {
            let n = v.parse::<u64>().ok().filter(|n| (lo..=hi).contains(n)).ok_or_else(|| bad(what, v))?;
            parts.push(format!("{kw} {n}"));
        }
    }
    match opt(o, "single_stable") {
        None | Some("false") => {}
        Some("true") => parts.push("SINGLE_STABLE 1".into()),
        Some(v) => return Err(bad("un solo supertable", v)),
    }
    let tail = if parts.is_empty() { String::new() } else { format!(" {}", parts.join(" ")) };
    Ok(format!("CREATE DATABASE {}{tail}", q(name)))
}

/// The dialog's tab of each field: storage and replication, memory and
/// cache, WAL and compression.
pub(crate) fn grouped(f: Field) -> Field {
    let g = match f.key {
        "buffer" | "pages" | "pagesize" | "cachemodel" | "cachesize" => "Memoria y caché",
        "wal_level" | "wal_fsync_period" | "comp" | "minrows" | "maxrows" | "stt_trigger" => "WAL y compresión",
        _ => "Almacenamiento",
    };
    f.group(g)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn plain_name_is_the_old_create() {
        assert_eq!(script("planta", &o(&[("keep", " ")])).unwrap(), "CREATE DATABASE `planta`");
        assert_eq!(script("a`b", &o(&[])).unwrap(), "CREATE DATABASE `a``b`");
    }

    #[test]
    fn every_option() {
        assert_eq!(
            script(
                "p",
                &o(&[
                    ("precision", "us"),
                    ("keep", "30d,60d,3650D"),
                    ("duration", "1d"),
                    ("replica", "1"),
                    ("vgroups", "4"),
                    ("buffer", "64"),
                    ("pages", "128"),
                    ("pagesize", "8"),
                    ("cachemodel", "last_row"),
                    ("cachesize", "2"),
                    ("wal_level", "2"),
                    ("wal_fsync_period", "1000"),
                    ("comp", "1"),
                    ("minrows", "50"),
                    ("maxrows", "8192"),
                    ("stt_trigger", "4"),
                    ("single_stable", "true"),
                ])
            )
            .unwrap(),
            "CREATE DATABASE `p` PRECISION 'us' KEEP 30d,60d,3650d DURATION 1d REPLICA 1 CACHEMODEL 'last_row' WAL_LEVEL 2 \
             VGROUPS 4 BUFFER 64 PAGES 128 PAGESIZE 8 CACHESIZE 2 WAL_FSYNC_PERIOD 1000 MINROWS 50 MAXROWS 8192 STT_TRIGGER 4 COMP 1 SINGLE_STABLE 1"
        );
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("precision", "s"),
            ("keep", "1y"),
            ("keep", "1d,2d,3d,4d"),
            ("keep", "10d; DROP DATABASE x"),
            ("duration", "d"),
            ("replica", "4"),
            ("vgroups", "0"),
            ("buffer", "2"),
            ("cachemodel", "all"),
            ("wal_level", "0"),
            ("comp", "3"),
            ("single_stable", "1"),
        ] {
            assert!(script("p", &o(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script(" ", &o(&[])).is_err());
    }
}
