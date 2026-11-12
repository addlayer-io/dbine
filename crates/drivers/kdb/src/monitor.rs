//! The Monitor on kdb+: the process's memory (`.Q.w[]`), its open
//! connections (`.z.W`) and what it runs on. A kdb+ process runs one
//! request at a time, so there are connections, not running queries: a
//! connection can be closed (`hclose`), a query can't be stopped from a
//! client.

use crate::cells;
use crate::ipc::K;
use dbine_driver::{Metric, MetricUnit, MonitorSnapshot, MonitorTable, ServerProcess};
use serde_json::Value;

/// One query for the whole snapshot: memory, connections, facts.
pub const SNAPSHOT: &str =
    "(.Q.w[];count .z.W;.z.K;.z.k;.z.h;.z.o;.z.i;system\"s\";.z.c;system\"p\")";

/// The open connections: handle, bytes queued to it, and whether it's
/// this session's own.
pub const CONNECTIONS: &str = "([] h:key .z.W; queued:sum each value .z.W; own:(key .z.W)=.z.w)";

fn number(k: &K) -> Option<f64> {
    match k {
        K::Long(n) if *n != i64::MIN => Some(*n as f64),
        K::Int(n) if *n != i32::MIN => Some(*n as f64),
        K::Short(n) => Some(*n as f64),
        K::Float(f) if !f.is_nan() => Some(*f),
        K::Real(f) if !f.is_nan() => Some(*f as f64),
        _ => None,
    }
}

fn text(k: &K) -> String {
    match k {
        K::Date(d) => cells::date(*d),
        K::Float(f) => format!("{f}"),
        k => match cells::cell(k) {
            Value::String(s) => s,
            Value::Null => String::new(),
            v => v.to_string(),
        },
    }
}

/// `.Q.w[]`'s entry `name`.
fn mem(w: &K, name: &str) -> Option<f64> {
    let K::Dict(keys, values) = w else {
        return None;
    };
    let i = keys.symbols()?.iter().position(|k| k == name)?;
    number(&values.item(i)?)
}

pub fn snapshot(k: K) -> MonitorSnapshot {
    let mut s = MonitorSnapshot::default();
    let K::List(parts) = k else { return s };
    let get = |i: usize| parts.get(i).cloned().unwrap_or(K::Nil);
    let w = get(0);
    let limit = mem(&w, "wmax").filter(|&m| m > 0.0);
    s.metrics = vec![
        Metric {
            max: limit,
            ..Metric::new(
                "used",
                "Memoria en uso",
                "Memoria",
                MetricUnit::Bytes,
                mem(&w, "used"),
            )
            .help("Lo que ocupan los objetos de kdb+ (tablas, listas, variables) dentro del heap. Si el proceso tiene un límite de memoria (-w), ese es el tope: al alcanzarlo, kdb+ da el error wsfull.")
            .source(".Q.w[]: used (max: wmax, when -w or \\w sets one)")
        },
        Metric {
            max: limit,
            ..Metric::new(
                "heap",
                "Memoria reservada",
                "Memoria",
                MetricUnit::Bytes,
                mem(&w, "heap"),
            )
            .help("Memoria que el proceso reservó del sistema operativo para su heap. No la devuelve hasta que se ejecuta .Q.gc[], así que puede quedar muy por encima de la memoria en uso.")
            .source(".Q.w[]: heap (max: wmax, when -w or \\w sets one)")
        },
        Metric::new(
            "peak",
            "Pico de memoria",
            "Memoria",
            MetricUnit::Bytes,
            mem(&w, "peak"),
        )
        .help("Lo máximo que llegó a ocupar el heap desde que arrancó el proceso.")
        .source(".Q.w[]: peak"),
        Metric::new(
            "mmap",
            "Memoria mapeada",
            "Memoria",
            MetricUnit::Bytes,
            mem(&w, "mmap"),
        )
        .help("Memoria mapeada desde archivos, como las columnas de tablas splayed o particionadas en disco. Va aparte del heap: el sistema operativo la carga y la libera según el uso.")
        .source(".Q.w[]: mmap"),
        Metric::new(
            "syms",
            "Símbolos internados",
            "Memoria",
            MetricUnit::Count,
            mem(&w, "syms"),
        )
        .help("Cantidad de símbolos internados. kdb+ nunca los libera, así que un crecimiento constante indica una fuga: símbolos creados a partir de datos únicos, como identificadores o textos libres.")
        .source(".Q.w[]: syms"),
        Metric::new(
            "connections",
            "Conexiones abiertas",
            "Conexiones",
            MetricUnit::Count,
            number(&get(1)),
        )
        .help("Conexiones IPC abiertas en el proceso, entrantes y salientes, incluida la de DBine. La tabla de conexiones muestra los bytes en cola de cada una.")
        .source("count .z.W"),
    ];
    s.info = vec![
        (
            "Versión".into(),
            format!("kdb+ {} ({})", text(&get(2)), text(&get(3))),
        ),
        ("Servidor".into(), text(&get(4))),
        ("Sistema".into(), text(&get(5))),
        ("Proceso".into(), text(&get(6))),
        ("Hilos secundarios".into(), text(&get(7))),
        ("Núcleos".into(), text(&get(8))),
        ("Puerto".into(), text(&get(9))),
    ];
    s.notes = vec![
        "kdb+ atiende un pedido por vez: no hay consultas en curso que listar ni detener desde un cliente.".into(),
    ];
    s
}

pub fn connections_table(rows: &[ServerProcess]) -> MonitorTable {
    let mut t = MonitorTable::new(
        "connections",
        "Conexiones",
        &["Handle", "Bytes en cola", "Esta sesión"],
    );
    t.rows = rows
        .iter()
        .map(|p| {
            vec![
                Value::String(p.id.clone()),
                p.writes.map_or(Value::Null, |n| n.into()),
                Value::Bool(p.own),
            ]
        })
        .collect();
    t
}

/// The rows of [`CONNECTIONS`].
pub fn processes(k: K) -> Vec<ServerProcess> {
    let K::Table(names, cols) = k else {
        return Vec::new();
    };
    let col = |n: &str| names.iter().position(|c| c == n).map(|i| &cols[i]);
    let (Some(h), Some(q), Some(own)) = (col("h"), col("queued"), col("own")) else {
        return Vec::new();
    };
    (0..h.count())
        .map(|i| ServerProcess {
            id: h.item(i).as_ref().map(text).unwrap_or_default(),
            own: matches!(own.item(i), Some(K::Bool(true))),
            writes: q.item(i).as_ref().and_then(number).map(|n| n as u64),
            status: Some("connected".into()),
            ..Default::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::Vector;

    #[test]
    fn snapshot_and_connections() {
        let w = K::Dict(
            Box::new(K::Vector(
                0,
                Vector::Symbol(
                    ["used", "heap", "peak", "wmax", "mmap", "syms"]
                        .map(String::from)
                        .to_vec(),
                ),
            )),
            Box::new(K::Vector(0, Vector::Long(vec![100, 200, 300, 0, 0, 42]))),
        );
        let k = K::List(vec![
            w,
            K::Long(3),
            K::Float(4.1),
            K::Date(9000),
            K::Symbol("host".into()),
            K::Symbol("l64".into()),
            K::Int(123),
            K::Int(0),
            K::Int(8),
            K::Int(5000),
        ]);
        let s = snapshot(k);
        assert_eq!(s.metrics[0].value, Some(100.0));
        assert_eq!(s.metrics[0].max, None);
        assert_eq!(s.metrics[4].value, Some(42.0));
        assert_eq!(s.metrics[5].value, Some(3.0));
        for m in &s.metrics {
            assert!(
                m.help.as_deref().is_some_and(|h| !h.is_empty()),
                "{}",
                m.key
            );
            assert!(
                m.source.as_deref().is_some_and(|h| !h.is_empty()),
                "{}",
                m.key
            );
        }
        assert_eq!(s.metrics.len(), 6);
        assert!(s.info[0].1.starts_with("kdb+ 4.1 ("));
        assert_eq!(s.info[6].1, "5000");

        let t = K::Table(
            vec!["h".into(), "queued".into(), "own".into()],
            vec![
                K::Vector(0, Vector::Int(vec![5, 7])),
                K::Vector(0, Vector::Long(vec![0, 10])),
                K::Vector(0, Vector::Bool(vec![true, false])),
            ],
        );
        let p = processes(t);
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].id.as_str(), p[0].own), ("5", true));
        assert_eq!(p[1].writes, Some(10));
        assert_eq!(connections_table(&p).rows.len(), 2);
    }
}
