//! Parity: every driver DBine ships has a dialect, or is listed here with
//! the reason schema conversion doesn't apply to it.

/// Engines without tables to convert.
const NOT_APPLICABLE: &[(&str, &str)] = &[
    ("redis", "clave-valor: no hay tablas ni columnas"),
    ("valkey", "clave-valor: no hay tablas ni columnas"),
    ("dragonfly", "clave-valor: no hay tablas ni columnas"),
    ("etcd", "árbol de claves sin esquema"),
    ("flightsql", "protocolo genérico: el motor de atrás (DuckDB, Dremio, InfluxDB 3, Doris) no se conoce, y cada uno tiene su propia conexión"),
];

#[test]
fn every_driver_has_a_dialect() {
    let missing: Vec<&str> = dbine_drivers::all()
        .iter()
        .map(|d| d.info().id)
        .filter(|id| dbine_schema::dialect::for_driver(id).is_none() && !NOT_APPLICABLE.iter().any(|(n, _)| n == id))
        .collect();
    assert!(missing.is_empty(), "sin dialecto: {}", missing.join(" "));
}
