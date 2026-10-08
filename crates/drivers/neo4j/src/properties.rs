//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]):
//!
//! - Neo4j: what `SHOW DATABASE` reports (status, access, role, address,
//!   default and home, store format, creation, topology, options…) and the
//!   node and relationship counts. Enterprise changes, with `ALTER
//!   DATABASE` on `system`: the access (READ ONLY / READ WRITE), the
//!   topology (primaries and secondaries), the transaction log enrichment
//!   (`txLogEnrichment`, for CDC) and, from 2025.06, the default Cypher
//!   version. Community has no `ALTER DATABASE`: facts only. The `system`
//!   database and composite databases don't change.
//! - Memgraph: what `SHOW STORAGE INFO` reports (counts, memory, disk,
//!   storage mode, isolation). It has no `ALTER DATABASE`: storage mode and
//!   isolation have their own statements, outside this dialog.
//! - Neptune: one database per cluster, whose settings live in the
//!   cluster's parameter group (AWS API), not behind openCypher: none.
//!
//! One statement per change, in a safe order: READ WRITE first, READ ONLY
//! last (nothing else could be written after it).

use crate::create_db::count;
use crate::{as_text, cypher, Flavor, GraphSession};
use dbine_driver::{DatabaseProperties, Error, Field, FieldKind, PropertyInfo, Result};
use serde_json::Value;
use std::collections::BTreeMap;

const TOPOLOGY: &str = "Topología";
const OPTIONS: &str = "Opciones";

fn yes(v: &str) -> bool {
    matches!(v.trim(), "true" | "1" | "on" | "ON")
}

fn bad(what: &str, v: &str) -> Error {
    Error::Query(format!("{what}: «{v}» no es un valor válido"))
}

/// The statements for `changes`, each run on `system`.
pub(crate) fn alter(f: Flavor, database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    if f != Flavor::Neo4j {
        return Err(Error::Unsupported("este motor no modifica las propiedades de una base".into()));
    }
    let name = database.trim();
    if name.is_empty() {
        return Err(Error::Query("falta el nombre de la base".into()));
    }
    if name.eq_ignore_ascii_case("system") {
        return Err(Error::Query("la base «system» no se modifica".into()));
    }
    let db = cypher::ident(name);
    let alter = |what: String| format!("ALTER DATABASE {db} {what} WAIT");
    let (mut first, mut out, mut last) = (Vec::new(), Vec::new(), Vec::new());
    let mut topology = Vec::new();
    for (key, value) in changes {
        let value = value.trim();
        match key.as_str() {
            "read_only" if yes(value) => last.push(alter("SET ACCESS READ ONLY".into())),
            "read_only" => first.push(alter("SET ACCESS READ WRITE".into())),
            "primaries" => {
                let n = count(value, "primarios", 1)?;
                topology.insert(0, format!("{n} {}", if n == 1 { "PRIMARY" } else { "PRIMARIES" }));
            }
            "secondaries" => {
                let n = count(value, "secundarios", 0)?;
                topology.push(format!("{n} {}", if n == 1 { "SECONDARY" } else { "SECONDARIES" }));
            }
            "tx_log_enrichment" => {
                if !matches!(value, "OFF" | "DIFF" | "FULL") {
                    return Err(bad("enriquecer el log", value));
                }
                out.push(alter(format!("SET OPTION txLogEnrichment '{value}'")));
            }
            "default_language" => {
                if !matches!(value, "5" | "25") {
                    return Err(bad("versión de Cypher", value));
                }
                out.push(alter(format!("SET DEFAULT LANGUAGE CYPHER {value}")));
            }
            k => return Err(Error::Query(format!("propiedad desconocida: {k}"))),
        }
    }
    if !topology.is_empty() {
        out.push(alter(format!("SET TOPOLOGY {}", topology.join(" "))));
    }
    Ok(first.into_iter().chain(out).chain(last).collect())
}

pub(crate) fn script(f: Flavor, database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(f, database, changes)?.into_iter().map(|s| s + ";").collect::<Vec<_>>().join("\n"))
}

fn fact(group: &str, label: &str, value: String) -> PropertyInfo {
    PropertyInfo { group: group.into(), label: label.into(), value }
}

fn yes_no(v: &Value) -> String {
    match v {
        Value::Bool(true) => "Sí".into(),
        Value::Bool(false) => "No".into(),
        other => as_text(other),
    }
}

/// Memgraph's `SHOW STORAGE INFO` keys, in Spanish.
fn memgraph_label(k: &str) -> String {
    match k {
        "name" => "Nombre".into(),
        "vertex_count" => "Nodos (vertex_count)".into(),
        "edge_count" => "Relaciones (edge_count)".into(),
        "average_degree" => "Grado medio (average_degree)".into(),
        "memory_res" => "Memoria residente (memory_res)".into(),
        "peak_memory_res" => "Pico de memoria (peak_memory_res)".into(),
        "disk_usage" => "Uso de disco (disk_usage)".into(),
        "memory_tracked" => "Memoria registrada (memory_tracked)".into(),
        "allocation_limit" => "Límite de memoria (allocation_limit)".into(),
        "storage_mode" => "Modo de almacenamiento (storage_mode)".into(),
        "global_isolation_level" => "Aislamiento global (global_isolation_level)".into(),
        "session_isolation_level" => "Aislamiento de la sesión (session_isolation_level)".into(),
        "next_session_isolation_level" => "Aislamiento de la próxima sesión (next_session_isolation_level)".into(),
        "unreleased_delta_objects" => "Deltas sin liberar (unreleased_delta_objects)".into(),
        "database_uuid" => "Identificador (database_uuid)".into(),
        "state" => "Estado (state)".into(),
        "health" => "Salud (health)".into(),
        "graph_memory_tracked" => "Memoria del grafo (graph_memory_tracked)".into(),
        "query_memory_tracked" => "Memoria de las consultas (query_memory_tracked)".into(),
        "vector_index_memory_tracked" => "Memoria de los índices vectoriales (vector_index_memory_tracked)".into(),
        "tenant_memory_tracked" => "Memoria de la base (tenant_memory_tracked)".into(),
        "tenant_peak_memory_tracked" => "Pico de memoria de la base (tenant_peak_memory_tracked)".into(),
        "tenant_memory_limit" => "Límite de memoria de la base (tenant_memory_limit)".into(),
        "storage_isolation_level" => "Aislamiento (storage_isolation_level)".into(),
        "global_storage_mode" => "Modo de almacenamiento (global_storage_mode)".into(),
        "memory_limit" => "Límite de memoria (memory_limit)".into(),
        other => other.into(),
    }
}

impl GraphSession {
    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        match self.flavor {
            Flavor::Neo4j => self.neo4j_properties(database).await,
            Flavor::Memgraph => self.memgraph_properties(database).await,
            Flavor::Neptune => Err(Error::Unsupported("Neptune tiene una sola base por cluster: su configuración está en el grupo de parámetros del cluster".into())),
        }
    }

    async fn memgraph_properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        // Memgraph 3 reports a database's own storage with `ON DATABASE`;
        // before, `SHOW STORAGE INFO` was the session's database.
        let on = format!("SHOW STORAGE INFO ON DATABASE {}", cypher::ident(database.trim()));
        let rows = match self.query_on(&on, None).await {
            Ok((_, rows)) => rows,
            Err(Error::Query(_)) => self.query_on("SHOW STORAGE INFO", Some(database)).await?.1,
            Err(e) => return Err(e),
        };
        let mut info: Vec<PropertyInfo> = rows
            .iter()
            .filter_map(|r| Some((as_text(r.first()?), as_text(r.get(1)?))))
            .filter(|(k, _)| k != "name")
            .map(|(k, v)| {
                let group = if k.contains("isolation") || k.contains("storage_mode") {
                    "Configuración"
                } else if k.contains("memory") || k.contains("disk") || k == "vm_max_map_count" {
                    "Memoria y disco"
                } else {
                    ""
                };
                fact(group, &memgraph_label(&k), v)
            })
            .collect();
        info.push(fact(
            "Configuración",
            "Nota",
            "Memgraph no tiene ALTER DATABASE: el modo de almacenamiento y el aislamiento se cambian con STORAGE MODE y SET … TRANSACTION ISOLATION LEVEL, fuera de este diálogo.".into(),
        ));
        Ok(DatabaseProperties { info, ..Default::default() })
    }

    async fn neo4j_properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let q = format!("SHOW DATABASE {} YIELD *", cypher::ident(database.trim()));
        let (cols, rows) = self.query_on(&q, Some("system")).await?;
        let recs: Vec<serde_json::Map<String, Value>> = rows.into_iter().map(|r| cols.iter().cloned().zip(r).collect()).collect();
        // One row per server that hosts it: the writer's (or the first) for the database's own facts.
        let r = recs
            .iter()
            .find(|r| r.get("writer") == Some(&Value::Bool(true)))
            .or_else(|| recs.first())
            .ok_or_else(|| Error::Query(format!("no existe la base «{database}»")))?
            .clone();
        let get = |k: &str| r.get(k).filter(|v| !v.is_null());
        let text = |k: &str| get(k).map(as_text).unwrap_or_default();

        let mut info = Vec::new();
        let mut push = |group: &str, label: &str, v: Option<String>| {
            if let Some(v) = v.filter(|v| !v.is_empty()) {
                info.push(fact(group, label, v));
            }
        };
        let status = text("currentStatus");
        let message = text("statusMessage");
        push("", "Estado", Some(if message.is_empty() { status.clone() } else { format!("{status} ({message})") }));
        push("", "Estado pedido (requestedStatus)", get("requestedStatus").map(as_text).filter(|s| *s != status));
        push("", "Tipo", get("type").map(as_text));
        push("", "Acceso", get("access").map(as_text));
        push("", "Predeterminada del servidor (default)", get("default").map(yes_no));
        push("", "De inicio del usuario (home)", get("home").map(yes_no));
        push("", "Creada", get("creationTime").map(as_text));
        push("", "Último inicio", get("lastStartTime").map(as_text));
        push("", "Última detención", get("lastStopTime").map(as_text));
        push("", "Formato de almacenamiento (store)", get("store").map(as_text));
        push("", "Última transacción confirmada", get("lastCommittedTxn").map(as_text));
        push("", "Versión de Cypher predeterminada", get("defaultLanguage").map(as_text));
        push(
            "",
            "Alias",
            get("aliases").and_then(Value::as_array).map(|a| a.iter().map(as_text).collect::<Vec<_>>().join(", ")),
        );
        push(
            "",
            "Bases que la componen (constituents)",
            get("constituents").and_then(Value::as_array).map(|a| a.iter().map(as_text).collect::<Vec<_>>().join(", ")),
        );
        push(TOPOLOGY, "Primarios en línea (currentPrimariesCount)", get("currentPrimariesCount").map(as_text));
        push(TOPOLOGY, "Secundarios en línea (currentSecondariesCount)", get("currentSecondariesCount").map(as_text));
        for s in &recs {
            let at = |k: &str| s.get(k).map(as_text).unwrap_or_default();
            let lag = at("replicationLag");
            let lag = if lag.is_empty() || lag == "0" { String::new() } else { format!(", atraso {lag}") };
            push(TOPOLOGY, &format!("Servidor {}", at("address")), Some(format!("{} ({}{lag})", at("role"), at("currentStatus"))));
        }
        let opts = get("options").and_then(Value::as_object).cloned().unwrap_or_default();
        for (k, v) in &opts {
            if k != "txLogEnrichment" {
                push(OPTIONS, k, Some(as_text(v)));
            }
        }
        // Counts from the count store (cheap), when the database is online.
        if status == "online" && text("type") != "system" {
            for (q, label) in [("MATCH (n) RETURN count(n)", "Nodos"), ("MATCH ()-[r]->() RETURN count(r)", "Relaciones")] {
                if let Ok((_, rows)) = self.query_on(q, Some(database)).await {
                    push("", label, rows.first().and_then(|r| r.first()).map(as_text));
                }
            }
        }

        let edition = self
            .query_on("CALL dbms.components() YIELD edition RETURN edition", Some("system"))
            .await
            .ok()
            .and_then(|(_, rows)| rows.into_iter().next())
            .and_then(|r| r.first().map(as_text))
            .unwrap_or_default();
        let kind = text("type");
        let mut p = DatabaseProperties { info, ..Default::default() };
        if edition.eq_ignore_ascii_case("community") {
            p.info.push(fact("", "Edición", "Community: no tiene ALTER DATABASE; para cambiar la base hace falta Neo4j Enterprise.".into()));
            return Ok(p);
        }
        if kind == "system" || database.trim().eq_ignore_ascii_case("system") {
            p.info.push(fact("", "Cambios", "La base «system» no se modifica.".into()));
            return Ok(p);
        }
        if kind != "standard" {
            p.info.push(fact("", "Cambios", format!("Una base de tipo «{kind}» no se modifica con ALTER DATABASE.")));
            return Ok(p);
        }

        let mut fields = vec![
            Field::new("read_only", "Solo lectura (ACCESS READ ONLY)", FieldKind::Bool),
            Field::new("primaries", "Primarios (TOPOLOGY … PRIMARIES)", FieldKind::Number)
                .help("Copias que aceptan escrituras. Más de 1 necesita un cluster con servidores libres.")
                .group(TOPOLOGY),
            Field::new("secondaries", "Secundarios (SECONDARIES)", FieldKind::Number).help("Copias de solo lectura.").group(TOPOLOGY),
        ];
        p.values.insert("read_only".into(), if text("access") == "read-only" { "true".into() } else { String::new() });
        for (k, col) in [("primaries", "requestedPrimariesCount"), ("secondaries", "requestedSecondariesCount")] {
            if let Some(v) = get(col) {
                p.values.insert(k.into(), as_text(v));
            }
        }
        if r.contains_key("options") {
            fields.push(
                Field::new(
                    "tx_log_enrichment",
                    "Enriquecer el log de transacciones (txLogEnrichment)",
                    FieldKind::Select(vec![("OFF", "No (OFF)"), ("DIFF", "Solo los cambios (DIFF)"), ("FULL", "Completo (FULL)")]),
                )
                .help("DIFF o FULL activan la captura de cambios (CDC).")
                .group(OPTIONS),
            );
            let tle = opts.get("txLogEnrichment").map(as_text).filter(|s| !s.is_empty()).unwrap_or_else(|| "OFF".into());
            p.values.insert("tx_log_enrichment".into(), tle);
        }
        if let Some(lang) = get("defaultLanguage") {
            fields.push(
                Field::new("default_language", "Versión de Cypher predeterminada", FieldKind::Select(vec![("5", "Cypher 5"), ("25", "Cypher 25")]))
                    .help("La que usan las consultas que no empiezan con CYPHER 5 o CYPHER 25.")
                    .group(OPTIONS),
            );
            let digits: String = as_text(lang).chars().filter(char::is_ascii_digit).collect();
            p.values.insert("default_language".into(), digits);
        }
        p.fields = fields;
        p.warnings.insert("read_only".into(), "En solo lectura la base rechaza toda escritura, también la de las sesiones que ya están abiertas.".into());
        for k in ["primaries", "secondaries"] {
            p.warnings.insert(
                k.into(),
                "Cambia cuántas copias tiene la base en el cluster: los servidores crean o quitan copias y hace falta que haya servidores libres suficientes.".into(),
            );
        }
        p.warnings.insert(
            "tx_log_enrichment".into(),
            "DIFF o FULL agregan datos a cada transacción del log (para CDC): crece el log y escribir cuesta más.".into(),
        );
        p.warnings.insert(
            "default_language".into(),
            "Cambia cómo se interpretan las consultas sin versión explícita: las que dependen de la otra versión pueden fallar.".into(),
        );
        Ok(p)
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        self.refuse_if_read_only("modificar las propiedades de una base")?;
        let statements = alter(self.flavor, database, changes)?;
        for (i, q) in statements.iter().enumerate() {
            if let Err(e) = self.query_on(q, Some("system")).await {
                let e = alter_hint(e);
                return Err(if i == 0 { e } else { Error::Query(format!("se aplicaron {i} de {} cambios; falló: {q}\n{e}", statements.len())) });
            }
        }
        Ok(())
    }
}

/// Community's refusal, said plainly.
fn alter_hint(e: Error) -> Error {
    match e {
        Error::Query(m) if m.contains("Unsupported administration command") || m.contains("not available in community") => {
            Error::Unsupported(format!("Cambiar una base requiere Neo4j Enterprise: {m}"))
        }
        e => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn read_only_goes_last() {
        assert_eq!(
            script(Flavor::Neo4j, "ventas", &c(&[("read_only", "true"), ("tx_log_enrichment", "DIFF"), ("secondaries", "1"), ("primaries", "3")])).unwrap(),
            "ALTER DATABASE ventas SET OPTION txLogEnrichment 'DIFF' WAIT;
ALTER DATABASE ventas SET TOPOLOGY 3 PRIMARIES 1 SECONDARY WAIT;
ALTER DATABASE ventas SET ACCESS READ ONLY WAIT;"
        );
    }

    #[test]
    fn read_write_goes_first() {
        assert_eq!(
            alter(Flavor::Neo4j, "my-db", &c(&[("read_only", ""), ("secondaries", "0"), ("default_language", "25")])).unwrap(),
            [
                "ALTER DATABASE `my-db` SET ACCESS READ WRITE WAIT",
                "ALTER DATABASE `my-db` SET DEFAULT LANGUAGE CYPHER 25 WAIT",
                "ALTER DATABASE `my-db` SET TOPOLOGY 0 SECONDARIES WAIT",
            ]
        );
        assert_eq!(alter(Flavor::Neo4j, "v", &c(&[("primaries", "1")])).unwrap(), ["ALTER DATABASE v SET TOPOLOGY 1 PRIMARY WAIT"]);
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("primaries", "0"),
            ("primaries", "1 PRIMARY"),
            ("secondaries", "-1"),
            ("tx_log_enrichment", "full"),
            ("tx_log_enrichment", "OFF' WAIT"),
            ("default_language", "4"),
            ("nope", "1"),
        ] {
            assert!(alter(Flavor::Neo4j, "v", &c(&[bad])).is_err(), "{bad:?}");
        }
        assert!(alter(Flavor::Neo4j, "system", &c(&[("read_only", "true")])).is_err());
        assert!(alter(Flavor::Neo4j, "SYSTEM", &c(&[])).is_err());
        assert!(alter(Flavor::Memgraph, "v", &c(&[("read_only", "true")])).is_err());
        assert!(alter(Flavor::Neptune, "v", &c(&[])).is_err());
    }
}
