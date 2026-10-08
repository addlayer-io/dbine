//! "Propiedades" of a bucket ([`dbine_driver::Session::database_properties`]):
//! what `GET /pools/default/buckets/{bucket}` reports (items, memory, disk,
//! type, storage) and the settings `POST /pools/default/buckets/{bucket}`
//! lets edit after creation: RAM quota, replicas, flush, rank, ejection,
//! watermarks, warmup, access scanner, expiry pager, durability, maximum
//! TTL, compression and the change history of Magma buckets.
//!
//! A setting is shown only when the server reports it for the bucket, so
//! the ones of Enterprise Edition (maxTTL, compression…) or of Magma, or of
//! newer versions, appear only where they exist. Not editable after
//! creation (facts): type, storage backend (it changes through a migration
//! with rebalance), conflict resolution. The auto-compaction and
//! encryption-at-rest settings are left out.
//!
//! All the changes go in one request, exactly as "Ver script" shows it.

use crate::create_db::{body, bucket_name};
use crate::{encode, CbSession};
use dbine_driver::{DatabaseProperties, Error, Field, FieldKind, PropertyInfo, Result};
use serde_json::Value;
use std::collections::BTreeMap;

const MIB: u64 = 1024 * 1024;

#[derive(Clone, Copy)]
enum Kind {
    /// An integer in this range.
    Num(u64, u64),
    /// 0, or at least 2 GiB (historyRetentionBytes).
    HistoryBytes,
    /// One of these `(value, label)`.
    OneOf(&'static [(&'static str, &'static str)]),
    /// A checkbox: the form's values for on and off.
    Bool(&'static str, &'static str),
}

/// Where the current value is in the bucket's JSON.
#[derive(Clone, Copy)]
enum Read {
    Ptr(&'static str),
    /// `quota.rawRAM`, in MiB (the per-node quota, as `ramQuota` takes it).
    RamMib,
    /// Flush is on when the bucket has a flush controller.
    Flush,
}

struct Setting {
    key: &'static str,
    param: &'static str,
    label: &'static str,
    help: &'static str,
    group: &'static str,
    kind: Kind,
    read: Read,
}

const EVICTION: &[(&str, &str)] = &[
    ("valueOnly", "Solo valores (valueOnly)"),
    ("fullEviction", "Completa (fullEviction)"),
    ("noEviction", "Sin desalojo (noEviction)"),
    ("nruEviction", "Menos usados (nruEviction)"),
];
const DURABILITY: &[(&str, &str)] = &[
    ("none", "Ninguna (none)"),
    ("majority", "Mayoría (majority)"),
    ("majorityAndPersistActive", "Mayoría y disco en el activo (majorityAndPersistActive)"),
    ("persistToMajority", "Disco en la mayoría (persistToMajority)"),
];

const SETTINGS: &[Setting] = &[
    Setting {
        key: "ram_quota",
        param: "ramQuota",
        label: "Memoria por nodo (ramQuota, MB)",
        help: "Sale de la cuota libre del cluster. Mínimo 100 MB (Magma: 1024 MB antes de 7.6).",
        group: "",
        kind: Kind::Num(100, 1_048_576),
        read: Read::RamMib,
    },
    Setting {
        key: "replicas",
        param: "replicaNumber",
        label: "Réplicas (replicaNumber)",
        help: "Hacen falta tantos nodos de datos como réplicas más uno.",
        group: "",
        kind: Kind::OneOf(&[("0", "0"), ("1", "1"), ("2", "2"), ("3", "3")]),
        read: Read::Ptr("/replicaNumber"),
    },
    Setting {
        key: "flush",
        param: "flushEnabled",
        label: "Permitir vaciar el bucket (flushEnabled)",
        help: "Con flush activo, cualquiera con permiso puede borrar todos los documentos de una vez.",
        group: "",
        kind: Kind::Bool("1", "0"),
        read: Read::Flush,
    },
    Setting {
        key: "rank",
        param: "rank",
        label: "Prioridad en el rebalanceo (rank)",
        help: "De 0 a 1000: los buckets de rank más alto se rebalancean primero.",
        group: "",
        kind: Kind::Num(0, 1000),
        read: Read::Ptr("/rank"),
    },
    Setting {
        key: "eviction",
        param: "evictionPolicy",
        label: "Política de desalojo (evictionPolicy)",
        help: "Couchbase: valueOnly o fullEviction. Efímero: noEviction o nruEviction.",
        group: "Memoria",
        kind: Kind::OneOf(EVICTION),
        read: Read::Ptr("/evictionPolicy"),
    },
    Setting {
        key: "memory_low_watermark",
        param: "memoryLowWatermark",
        label: "Marca baja de memoria (memoryLowWatermark, %)",
        help: "De 50 a 89: el desalojo libera memoria hasta bajar a este porcentaje de la cuota.",
        group: "Memoria",
        kind: Kind::Num(50, 89),
        read: Read::Ptr("/memoryLowWatermark"),
    },
    Setting {
        key: "memory_high_watermark",
        param: "memoryHighWatermark",
        label: "Marca alta de memoria (memoryHighWatermark, %)",
        help: "De 51 a 90: al llegar a este porcentaje de la cuota empieza el desalojo.",
        group: "Memoria",
        kind: Kind::Num(51, 90),
        read: Read::Ptr("/memoryHighWatermark"),
    },
    Setting {
        key: "warmup",
        param: "warmupBehavior",
        label: "Carga al arrancar (warmupBehavior)",
        help: "Cómo se cargan los datos del disco a memoria cuando arranca el bucket o el nodo.",
        group: "Memoria",
        kind: Kind::OneOf(&[
            ("background", "En segundo plano (background)"),
            ("blocking", "Bloqueante (blocking)"),
            ("none", "Ninguna (none)"),
        ]),
        read: Read::Ptr("/warmupBehavior"),
    },
    Setting {
        key: "access_scanner",
        param: "accessScannerEnabled",
        label: "Registrar las claves más usadas (accessScannerEnabled)",
        help: "Las carga primero al arrancar.",
        group: "Memoria",
        kind: Kind::Bool("true", "false"),
        read: Read::Ptr("/accessScannerEnabled"),
    },
    Setting {
        key: "expiry_pager",
        param: "expiryPagerSleepTime",
        label: "Intervalo del borrado de vencidos (expiryPagerSleepTime, s)",
        help: "Cada cuánto se buscan y borran los documentos vencidos. Por defecto, 600.",
        group: "Documentos",
        kind: Kind::Num(0, 2_147_483_647),
        read: Read::Ptr("/expiryPagerSleepTime"),
    },
    Setting {
        key: "max_ttl",
        param: "maxTTL",
        label: "Vida máxima de los documentos (maxTTL, segundos)",
        help: "0: no vencen. Solo afecta a los documentos que se crean o modifican después.",
        group: "Documentos",
        kind: Kind::Num(0, 2_147_483_647),
        read: Read::Ptr("/maxTTL"),
    },
    Setting {
        key: "compression",
        param: "compressionMode",
        label: "Compresión (compressionMode)",
        help: "",
        group: "Documentos",
        kind: Kind::OneOf(&[("off", "Desactivada (off)"), ("passive", "Pasiva (passive)"), ("active", "Activa (active)")]),
        read: Read::Ptr("/compressionMode"),
    },
    Setting {
        key: "durability",
        param: "durabilityMinLevel",
        label: "Durabilidad mínima (durabilityMinLevel)",
        help: "Las que escriben a disco no van con un bucket efímero.",
        group: "Durabilidad",
        kind: Kind::OneOf(DURABILITY),
        read: Read::Ptr("/durabilityMinLevel"),
    },
    Setting {
        key: "durability_fallback",
        param: "durabilityImpossibleFallback",
        label: "Si no se alcanza la mayoría (durabilityImpossibleFallback)",
        help: "fallbackToActiveAck da por buena una escritura durable que solo llegó al nodo activo.",
        group: "Durabilidad",
        kind: Kind::OneOf(&[("disabled", "Falla la escritura (disabled)"), ("fallbackToActiveAck", "Alcanza con el activo (fallbackToActiveAck)")]),
        read: Read::Ptr("/durabilityImpossibleFallback"),
    },
    Setting {
        key: "history_default",
        param: "historyRetentionCollectionDefault",
        label: "Historial de cambios en las colecciones (historyRetentionCollectionDefault)",
        help: "Solo Magma. Hace falta además un límite de tiempo o de tamaño.",
        group: "Historial",
        kind: Kind::Bool("true", "false"),
        read: Read::Ptr("/historyRetentionCollectionDefault"),
    },
    Setting {
        key: "history_seconds",
        param: "historyRetentionSeconds",
        label: "Historial: segundos que cubre (historyRetentionSeconds)",
        help: "0: sin límite de tiempo.",
        group: "Historial",
        kind: Kind::Num(0, u64::MAX),
        read: Read::Ptr("/historyRetentionSeconds"),
    },
    Setting {
        key: "history_bytes",
        param: "historyRetentionBytes",
        label: "Historial: tamaño máximo (historyRetentionBytes, bytes)",
        help: "0: sin límite de tamaño. Si no, al menos 2 GiB (2147483648), por réplica.",
        group: "Historial",
        kind: Kind::HistoryBytes,
        read: Read::Ptr("/historyRetentionBytes"),
    },
    Setting {
        key: "magma_block",
        param: "magmaSeqTreeDataBlockSize",
        label: "Bloque del índice por secuencia (magmaSeqTreeDataBlockSize, bytes)",
        help: "De 4096 a 131072.",
        group: "Historial",
        kind: Kind::Num(4096, 131_072),
        read: Read::Ptr("/magmaSeqTreeDataBlockSize"),
    },
];

fn bad(what: &str, v: &str) -> Error {
    Error::Query(format!("{what}: «{v}» no es un valor válido"))
}

/// The form value of `s` for the dialog's value `v`.
fn form_value(s: &Setting, v: &str) -> Result<String> {
    let v = v.trim();
    let num = |lo: u64, hi: u64| v.parse::<u64>().ok().filter(|n| (lo..=hi).contains(n)).map(|n| n.to_string()).ok_or_else(|| bad(s.label, v));
    match s.kind {
        Kind::Num(lo, hi) => num(lo, hi),
        Kind::HistoryBytes => v
            .parse::<u64>()
            .ok()
            .filter(|n| *n == 0 || *n >= 2_147_483_648)
            .map(|n| n.to_string())
            .ok_or_else(|| bad(s.label, v)),
        Kind::OneOf(options) => options.iter().find(|o| o.0 == v).map(|o| o.0.to_string()).ok_or_else(|| bad(s.label, v)),
        Kind::Bool(on, off) => match v {
            "true" => Ok(on.into()),
            "" | "false" => Ok(off.into()),
            _ => Err(bad(s.label, v)),
        },
    }
}

/// One edit request: its path and form.
pub(crate) type Request = (String, Vec<(&'static str, String)>);

/// The request for `changes` (none when nothing changed).
pub(crate) fn alter(bucket: &str, changes: &BTreeMap<String, String>) -> Result<Option<Request>> {
    let name = bucket_name(bucket)?;
    let mut form = Vec::new();
    // In the table's order.
    for s in SETTINGS {
        if let Some(v) = changes.get(s.key) {
            form.push((s.param, form_value(s, v)?));
        }
    }
    if let Some(k) = changes.keys().find(|k| !SETTINGS.iter().any(|s| s.key == k.as_str())) {
        return Err(Error::Query(format!("propiedad desconocida: {k}")));
    }
    if form.is_empty() {
        return Ok(None);
    }
    Ok(Some((format!("/pools/default/buckets/{}", encode(name)), form)))
}

pub(crate) fn script(bucket: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(bucket, changes)?.map(|(path, form)| format!("POST {path}\n{}", body(&form))).unwrap_or_default())
}

fn current(s: &Setting, b: &Value) -> Option<String> {
    match s.read {
        Read::RamMib => b.pointer("/quota/rawRAM").and_then(Value::as_u64).map(|n| (n / MIB).to_string()),
        Read::Flush => Some(if b.pointer("/controllers/flush").is_some() { "true".into() } else { String::new() }),
        Read::Ptr(p) => match (b.pointer(p)?, s.kind) {
            (Value::Bool(on), _) => Some(if *on { "true".into() } else { String::new() }),
            (Value::String(v), Kind::Bool(on, _)) => Some(if v == on || v == "true" { "true".into() } else { String::new() }),
            (Value::String(v), _) => Some(v.clone()),
            (Value::Number(n), _) => Some(n.to_string()),
            _ => None,
        },
    }
}

fn mb(v: Option<&Value>) -> String {
    v.and_then(Value::as_u64).map(|n| format!("{:.1} MB", n as f64 / MIB as f64)).unwrap_or_default()
}

fn shown(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

impl CbSession {
    pub(crate) async fn properties(&mut self, bucket: &str) -> Result<DatabaseProperties> {
        let name = bucket_name(bucket)?;
        let b = self.conn.mgmt_get(&format!("/pools/default/buckets/{}", encode(name))).await?;
        let kind = b.get("bucketType").and_then(Value::as_str).unwrap_or_default();
        let fact = |group: &str, label: &str, value: String| PropertyInfo { group: group.into(), label: label.into(), value };
        let mut info = vec![
            fact(
                "",
                "Tipo (bucketType)",
                match kind {
                    "membase" | "couchbase" => "Couchbase".into(),
                    "ephemeral" => "Efímero (solo en memoria)".into(),
                    other => other.to_string(),
                },
            ),
            fact("", "Documentos", shown(b.pointer("/basicStats/itemCount"))),
            fact("", "Memoria usada", mb(b.pointer("/basicStats/memUsed"))),
            fact("", "Memoria total (todos los nodos)", mb(b.pointer("/quota/ram"))),
            fact("", "Cuota de memoria usada", b.pointer("/basicStats/quotaPercentUsed").and_then(Value::as_f64).map(|p| format!("{p:.1} %")).unwrap_or_default()),
        ];
        if kind != "ephemeral" {
            info.push(fact("", "Disco usado", mb(b.pointer("/basicStats/diskUsed"))));
            info.push(fact("", "Datos en disco", mb(b.pointer("/basicStats/dataUsed"))));
        }
        info.push(fact("", "Operaciones por segundo", shown(b.pointer("/basicStats/opsPerSec"))));
        info.push(fact("", "Nodos", b.get("nodes").and_then(Value::as_array).map(|n| n.len().to_string()).unwrap_or_default()));
        if let Some(s) = b.get("storageBackend") {
            info.push(fact("", "Almacenamiento (storageBackend)", shown(Some(s))));
        }
        if let Some(c) = b.get("conflictResolutionType") {
            info.push(fact("", "Resolución de conflictos (conflictResolutionType)", shown(Some(c))));
        }
        info.push(fact("", "vBuckets", shown(b.get("numVBuckets"))));
        info.push(fact("", "UUID", shown(b.get("uuid"))));

        let mut fields = Vec::new();
        let mut values = BTreeMap::new();
        for s in SETTINGS {
            // Memcached buckets (gone in Couchbase 8) only take the quota and flush.
            if kind == "memcached" && !matches!(s.key, "ram_quota" | "flush") {
                continue;
            }
            let Some(v) = current(s, &b) else { continue };
            let field_kind = match s.kind {
                Kind::Num(..) | Kind::HistoryBytes => FieldKind::Number,
                Kind::Bool(..) => FieldKind::Bool,
                Kind::OneOf(o) if s.key == "eviction" => {
                    FieldKind::Select(o.iter().copied().filter(|(v, _)| (kind == "ephemeral") == matches!(*v, "noEviction" | "nruEviction")).collect())
                }
                Kind::OneOf(o) if s.key == "durability" && kind == "ephemeral" => {
                    FieldKind::Select(o.iter().copied().filter(|(v, _)| matches!(*v, "none" | "majority")).collect())
                }
                Kind::OneOf(o) => FieldKind::Select(o.to_vec()),
            };
            fields.push(Field::new(s.key, s.label, field_kind).help(s.help).group(s.group));
            values.insert(s.key.to_string(), v);
        }

        let mut warnings = BTreeMap::new();
        warnings.insert(
            "replicas".into(),
            "Las réplicas nuevas (o las que se quitan) no se crean hasta rebalancear el cluster (Rebalance); mientras tanto, el bucket queda con la cantidad anterior.".into(),
        );
        if kind != "ephemeral" {
            warnings.insert(
                "eviction".into(),
                "Cambiar la política de desalojo reinicia el bucket: deja de atender unos instantes y vuelve a cargar los datos del disco.".into(),
            );
        }
        warnings.insert(
            "flush".into(),
            "Con flush activo, una sola orden borra todos los documentos del bucket, sin vuelta atrás.".into(),
        );
        warnings.insert(
            "durability_fallback".into(),
            "fallbackToActiveAck da por durables escrituras que solo están en un nodo: si ese nodo falla, se pierden.".into(),
        );
        warnings.insert(
            "ram_quota".into(),
            "Bajar la memoria por debajo de lo que ocupan los datos desaloja documentos de la memoria (o, en un bucket efímero, rechaza escrituras).".into(),
        );
        Ok(DatabaseProperties { fields, values, info, choices: Vec::new(), warnings })
    }

    pub(crate) async fn alter_database_impl(&mut self, bucket: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        if self.conn.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden modificar las propiedades de una base.".into()));
        }
        // One request: it applies all of it or nothing.
        let Some((path, form)) = alter(bucket, changes)? else { return Ok(()) };
        let rb = self
            .conn
            .http
            .post(format!("{}{path}", self.conn.mgmt))
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body(&form));
        self.conn.mgmt_send(rb).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn all_changes_in_one_request() {
        assert_eq!(
            script(
                "ventas",
                &c(&[
                    ("flush", ""),
                    ("eviction", "fullEviction"),
                    ("ram_quota", "256"),
                    ("replicas", "0"),
                    ("durability", "majority"),
                    ("access_scanner", "true"),
                    ("history_bytes", "0"),
                ])
            )
            .unwrap(),
            "POST /pools/default/buckets/ventas\nramQuota=256&replicaNumber=0&flushEnabled=0&evictionPolicy=fullEviction&accessScannerEnabled=true&durabilityMinLevel=majority&historyRetentionBytes=0"
        );
        assert_eq!(script("a%b", &c(&[("flush", "true")])).unwrap(), "POST /pools/default/buckets/a%25b\nflushEnabled=1");
        assert_eq!(script("v", &c(&[])).unwrap(), "");
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("ram_quota", "99"),
            ("ram_quota", "1e3"),
            ("replicas", "4"),
            ("eviction", "lru"),
            ("durability", "all"),
            ("rank", "1001"),
            ("memory_low_watermark", "49"),
            ("memory_high_watermark", "91"),
            ("warmup", "lazy"),
            ("max_ttl", "-1"),
            ("compression", "zstd"),
            ("history_bytes", "1024"),
            ("magma_block", "100"),
            ("flush", "1"),
            ("expiry_pager", "10&x=1"),
            ("conflict_resolution", "lww"),
        ] {
            assert!(script("v", &c(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script("a&b=c", &c(&[("flush", "true")])).is_err());
    }

    #[test]
    fn current_values() {
        let b = serde_json::json!({
            "quota": {"rawRAM": 268435456u64}, "controllers": {"flush": "/x"}, "replicaNumber": 1,
            "accessScannerEnabled": false, "historyRetentionCollectionDefault": true, "evictionPolicy": "valueOnly"
        });
        let get = |k: &str| current(SETTINGS.iter().find(|s| s.key == k).unwrap(), &b);
        assert_eq!(get("ram_quota").as_deref(), Some("256"));
        assert_eq!(get("flush").as_deref(), Some("true"));
        assert_eq!(get("replicas").as_deref(), Some("1"));
        assert_eq!(get("access_scanner").as_deref(), Some(""));
        assert_eq!(get("history_default").as_deref(), Some("true"));
        assert_eq!(get("eviction").as_deref(), Some("valueOnly"));
        assert_eq!(get("max_ttl"), None, "not reported: not shown");
    }
}
