//! Schema sync ("Comparar esquemas") for devices: a device's columns are
//! its time series, created with `CREATE [ALIGNED] TIMESERIES` and dropped
//! with `DELETE TIMESERIES` (with their data). IoTDB 1.x can't change a
//! series' type, encoding or compression, nor turn a device aligned: those
//! only warn.

use crate::{full_device, is_time, node, table_ddl};
use dbine_driver::{ColumnDef, DdlParts, Result, SyncScript, TableChange, TableSchema};

fn display(t: &TableSchema) -> String {
    match t.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

fn aligned(t: &TableSchema) -> bool {
    t.options.get("aligned").is_some_and(|v| v == "true" || v == "1")
}

fn opt(c: &ColumnDef, k: &str) -> Option<String> {
    c.options.get(k).map(|v| v.trim().to_ascii_uppercase()).filter(|v| !v.is_empty())
}

const CREATE: DdlParts = DdlParts { drop: false, if_exists: false, create: true, indexes: false, foreign_keys: false };

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let (mut drops, mut alters, mut creates, mut warnings) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ch in changes {
        match ch {
            TableChange::Create { table } => creates.push(table_ddl(table, CREATE)?),
            TableChange::Drop { table } => {
                warnings.push(format!("Se borran todas las series del dispositivo {} con sus datos.", display(table)));
                drops.push(table_ddl(table, DdlParts { drop: true, ..Default::default() })?);
            }
            TableChange::Alter { old, new } => alter(old, new, &mut alters, &mut warnings)?,
        }
    }
    Ok(SyncScript { statements: [drops, alters, creates].concat(), warnings })
}

fn alter(old: &TableSchema, new: &TableSchema, out: &mut Vec<String>, warnings: &mut Vec<String>) -> Result<()> {
    let device = full_device(new.schema.as_deref(), &new.name)?;
    let tname = display(new);
    if aligned(old) != aligned(new) {
        warnings.push(format!("{tname}: un dispositivo no pasa de alineado a no alineado (ni al revés) sin recrear sus series; se deja como está."));
    }
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    let series = |t: &TableSchema| t.columns.iter().filter(|c| !is_time(&c.name)).cloned().collect::<Vec<_>>();
    let (os, ns) = (series(old), series(new));

    for o in os.iter().filter(|o| !ns.iter().any(|n| eq(&n.name, &o.name))) {
        warnings.push(format!("Se borra la serie {tname}.{} con sus datos.", o.name));
        out.push(format!("DELETE TIMESERIES {device}.{};", node(&o.name)));
    }
    let added: Vec<ColumnDef> = ns.iter().filter(|n| !os.iter().any(|o| eq(&o.name, &n.name))).cloned().collect();
    if !added.is_empty() {
        // Added series keep the device as it is (aligned or not).
        let mut t = TableSchema { columns: added, ..new.clone() };
        t.options.insert("aligned".into(), aligned(old).to_string());
        out.push(table_ddl(&t, CREATE)?);
    }
    for n in &ns {
        let Some(o) = os.iter().find(|o| eq(&o.name, &n.name)) else { continue };
        if !eq(o.data_type.trim(), n.data_type.trim()) {
            warnings.push(format!(
                "{tname}.{}: {} → {}. IoTDB no cambia el tipo de una serie: hay que borrarla y volver a crearla (se pierden sus datos); se deja como está.",
                n.name, o.data_type, n.data_type
            ));
        }
        for (k, what) in [("encoding", "la codificación"), ("compression", "la compresión")] {
            if let (Some(a), Some(b)) = (opt(o, k), opt(n, k)) {
                if a != b {
                    warnings.push(format!("{tname}.{}: {what} de una serie no se cambia ({a} → {b}); se deja como está.", n.name));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
    }

    fn dev(aligned: bool) -> TableSchema {
        let mut t = TableSchema {
            kind: "device".into(),
            schema: Some("root.planta".into()),
            name: "d1".into(),
            columns: vec![col("Time", "TIMESTAMP"), col("temp", "FLOAT"), col("estado", "BOOLEAN")],
            ..Default::default()
        };
        if aligned {
            t.options.insert("aligned".into(), "true".into());
        }
        t
    }

    fn changed(aligned: bool) -> TableSchema {
        let mut n = dev(aligned);
        n.columns[1].data_type = "DOUBLE".into();
        n.columns.remove(2);
        let mut h = col("hum", "INT32");
        h.options.insert("encoding".into(), "RLE".into());
        n.columns.push(h);
        n
    }

    #[test]
    fn add_and_delete_series() {
        let s = sync_script(&[TableChange::Alter { old: dev(false), new: changed(false) }]).unwrap();
        assert_eq!(
            s.statements,
            vec!["DELETE TIMESERIES root.planta.d1.estado;", "CREATE TIMESERIES root.planta.d1.hum WITH DATATYPE=INT32, ENCODING=RLE;"]
        );
        assert_eq!(s.warnings.len(), 2, "{:?}", s.warnings);

        let s = sync_script(&[TableChange::Alter { old: dev(true), new: changed(true) }]).unwrap();
        assert_eq!(s.statements[1], "CREATE ALIGNED TIMESERIES root.planta.d1(\n    hum INT32 encoding=RLE\n);");

        let s = sync_script(&[TableChange::Alter { old: dev(true), new: dev(false) }]).unwrap();
        assert!(s.statements.is_empty() && s.warnings[0].contains("alineado"));
    }

    #[test]
    fn create_and_drop() {
        let mut old = dev(false);
        old.name = "viejo".into();
        let s = sync_script(&[TableChange::Create { table: dev(false) }, TableChange::Drop { table: old }]).unwrap();
        assert_eq!(s.statements[0], "DELETE TIMESERIES root.planta.viejo.**;");
        assert!(s.statements[1].starts_with("CREATE TIMESERIES root.planta.d1.temp WITH DATATYPE=FLOAT;"));
    }
}
