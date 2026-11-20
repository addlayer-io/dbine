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
        .filter(|id| {
            dbine_schema::dialect::for_driver(id).is_none()
                && !NOT_APPLICABLE.iter().any(|(n, _)| n == id)
        })
        .collect();
    assert!(missing.is_empty(), "sin dialecto: {}", missing.join(" "));
}

/// A dialect claims vectors and vector indexes only where an engine it
/// serves has them: its caps are those of its drivers together (each
/// narrowed by its designer and capabilities), so a dialect of one engine
/// matches its driver exactly. Vector indexes need vectors, and a dialect
/// without vectors renders one as its fallback.
#[test]
fn dialect_vector_caps_follow_their_drivers() {
    use dbine_driver::VectorElement;
    use dbine_schema::dialect::{for_driver, target_caps, Caps};
    use dbine_schema::{IssueCode, LogicalType};
    // One entry per dialect: its caps and its drivers' narrowed ones.
    type Drivers = Vec<(&'static str, Caps)>;
    let mut dialects: Vec<(String, Caps, Drivers)> = Vec::new();
    for d in dbine_drivers::all() {
        let id = d.info().id;
        let Some(dialect) = for_driver(id) else {
            continue;
        };
        let caps = dialect.caps();
        let key = format!("{} {caps:?}", dialect.id());
        let narrowed = target_caps(&**d).unwrap();
        match dialects.iter_mut().find(|(k, _, _)| *k == key) {
            Some((_, _, drivers)) => drivers.push((id, narrowed)),
            None => dialects.push((key, caps, vec![(id, narrowed)])),
        }
    }
    let vector = LogicalType::Vector {
        dims: Some(3),
        element: VectorElement::F32,
        sparse: false,
    };
    let mut wrong = Vec::new();
    for (_, caps, drivers) in &dialects {
        let ids: Vec<&str> = drivers.iter().map(|(id, _)| *id).collect();
        let any = |f: fn(&Caps) -> bool| drivers.iter().any(|(_, c)| f(c));
        if caps.vector_indexes != any(|c| c.vector_indexes) {
            wrong.push(format!("{ids:?}: vector_indexes {}", caps.vector_indexes));
        }
        if caps.vectors != any(|c| c.vectors) {
            wrong.push(format!("{ids:?}: vectors {}", caps.vectors));
        }
        if caps.vector_indexes && !caps.vectors {
            wrong.push(format!("{ids:?}: vector indexes without vectors"));
        }
        let rendered = for_driver(ids[0]).unwrap().render_type(&vector);
        if !caps.vectors
            && !rendered
                .notes
                .iter()
                .any(|n| n.code == IssueCode::TypeApproximated)
        {
            wrong.push(format!(
                "{ids:?}: no vectors, yet renders «{}»",
                rendered.native
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
