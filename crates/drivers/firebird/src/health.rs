//! "Chequeo de salud" findings of Firebird ([`dbine_driver::Session::health_checks`]):
//! the gap between the oldest interesting transaction and the next one
//! (garbage that needs a sweep, or a transaction left open), forced writes,
//! index statistics never computed on tables with data, inactive indexes
//! and tables without a primary key. It reads on a fresh attachment to the
//! database, apart from the session's transaction; each check is its own
//! query and one that fails is skipped.

use crate::{int, join_err, q, text, Conn, FirebirdSession};
use dbine_driver::health::{HealthCheck, Severity};
use dbine_driver::Result;
use rsfbclient_core::Column;

/// Objects listed per finding at most.
const MAX_OBJECTS: usize = 200;

/// The gap a sweep is measured against when automatic sweeping is off.
const DEFAULT_SWEEP: i64 = 20_000;

const MON: &str = "SELECT MON$OLDEST_TRANSACTION, MON$OLDEST_ACTIVE, MON$OLDEST_SNAPSHOT, MON$NEXT_TRANSACTION,
       MON$SWEEP_INTERVAL, MON$FORCED_WRITES, MON$READ_ONLY
  FROM MON$DATABASE";

/// Persistent user tables (no views, GTTs or external tables).
const TABLES: &str = "COALESCE(r.RDB$SYSTEM_FLAG, 0) = 0 AND r.RDB$VIEW_BLR IS NULL AND r.RDB$EXTERNAL_FILE IS NULL
   AND COALESCE(r.RDB$RELATION_TYPE, 0) = 0";

/// Active user indexes whose selectivity was never computed (0 or NULL).
fn zero_stats_sql() -> String {
    format!(
        "SELECT TRIM(i.RDB$INDEX_NAME), TRIM(i.RDB$RELATION_NAME)
           FROM RDB$INDICES i JOIN RDB$RELATIONS r ON r.RDB$RELATION_NAME = i.RDB$RELATION_NAME
          WHERE {TABLES} AND COALESCE(i.RDB$SYSTEM_FLAG, 0) = 0 AND COALESCE(i.RDB$INDEX_INACTIVE, 0) = 0
            AND COALESCE(i.RDB$STATISTICS, 0) = 0
          ORDER BY 2, 1"
    )
}

fn inactive_sql() -> String {
    format!(
        "SELECT TRIM(i.RDB$INDEX_NAME), TRIM(i.RDB$RELATION_NAME)
           FROM RDB$INDICES i JOIN RDB$RELATIONS r ON r.RDB$RELATION_NAME = i.RDB$RELATION_NAME
          WHERE {TABLES} AND COALESCE(i.RDB$SYSTEM_FLAG, 0) = 0 AND i.RDB$INDEX_INACTIVE = 1
          ORDER BY 2, 1 ROWS 200"
    )
}

fn no_pk_sql() -> String {
    format!(
        "SELECT TRIM(r.RDB$RELATION_NAME) FROM RDB$RELATIONS r
          WHERE {TABLES}
            AND NOT EXISTS (SELECT 1 FROM RDB$RELATION_CONSTRAINTS c
                             WHERE c.RDB$RELATION_NAME = r.RDB$RELATION_NAME AND c.RDB$CONSTRAINT_TYPE = 'PRIMARY KEY')
          ORDER BY 1 ROWS 200"
    )
}

/// Title, severity and detail of the transaction gap: `oit` the oldest
/// interesting, `oat` the oldest active, `next` the next transaction and
/// `interval` the sweep interval (0: automatic sweep off).
pub(crate) fn transaction_gap(oit: i64, oat: i64, next: i64, interval: i64) -> (String, Severity, String) {
    let limit = if interval > 0 { interval } else { DEFAULT_SWEEP };
    let gap = next - oit;
    let open = next - oat;
    let auto = if interval > 0 {
        format!("El barrido automático corre cada {interval} transacciones de brecha.")
    } else {
        "El barrido automático está apagado (intervalo 0): solo se barre a mano.".to_string()
    };
    if gap <= limit {
        return (
            format!("Brecha de transacciones: {gap} (sin barrido pendiente)"),
            Severity::Ok,
            format!("Distancia entre la transacción interesante más antigua y la siguiente. {auto}"),
        );
    }
    if open > limit && oat - oit <= limit {
        // The oldest active transaction holds everything back.
        return (
            format!("Una transacción abierta hace {open} transacciones frena la limpieza"),
            if open > limit * 10 { Severity::Critical } else { Severity::Warning },
            format!(
                "Mientras siga abierta, las versiones viejas de los registros no se pueden limpiar y la base crece y se vuelve más lenta. \
                 Buscá la conexión en MON$TRANSACTIONS (la de MON$TRANSACTION_ID = {oat}) y cerrala o terminá su transacción. {auto}"
            ),
        );
    }
    (
        format!("Brecha de {gap} transacciones: hace falta un barrido"),
        if gap > limit * 10 { Severity::Critical } else { Severity::Warning },
        format!(
            "Hay transacciones deshechas o en limbo que nadie limpió: la basura se acumula y cada lectura la recorre. \
             Corré un barrido fuera de horario pico (gfix -sweep <base> o gfix -sweep desde el servidor). {auto}"
        ),
    )
}

fn col(r: &[Column], i: usize) -> String {
    r.get(i).and_then(text).unwrap_or_default()
}

fn ival(r: &[Column], i: usize) -> Option<i64> {
    r.get(i).and_then(int)
}

/// What one attachment reads; every check its own `Result`.
struct Read {
    mon: Result<Vec<Vec<Column>>>,
    zero_stats: Result<Vec<(String, String)>>,
    inactive: Result<Vec<Vec<Column>>>,
    no_pk: Result<Vec<Vec<Column>>>,
}

fn read(c: &mut Conn) -> Read {
    let mon = c.rows(MON, vec![]);
    // Indexes with no statistics, on tables that have at least one row
    // (an index on an empty table has 0 and that's right).
    let zero_stats = c.rows(&zero_stats_sql(), vec![]).map(|rows| {
        let mut out = Vec::new();
        let mut checked: Vec<(String, bool)> = Vec::new();
        for r in rows {
            let (index, table) = (col(&r, 0), col(&r, 1));
            let has_rows = match checked.iter().find(|(t, _)| *t == table) {
                Some((_, v)) => *v,
                None => {
                    let v = c.rows(&format!("SELECT FIRST 1 1 FROM {}", q(&table)), vec![]).is_ok_and(|r| !r.is_empty());
                    checked.push((table.clone(), v));
                    v
                }
            };
            if has_rows {
                out.push((index, table));
                if out.len() >= MAX_OBJECTS {
                    break;
                }
            }
        }
        out
    });
    let inactive = c.rows(&inactive_sql(), vec![]);
    let no_pk = c.rows(&no_pk_sql(), vec![]);
    Read { mon, zero_stats, inactive, no_pk }
}

impl FirebirdSession {
    pub(crate) async fn health_checks_impl(&mut self, database: &str) -> Result<Vec<HealthCheck>> {
        let mut t = self.target.clone();
        let database = database.trim();
        t.attach.db_name = if database.is_empty() { self.database.clone() } else { database.to_string() };
        let r = tokio::task::spawn_blocking(move || Conn::open(&t).map(|mut c| read(&mut c))).await.map_err(join_err)??;
        let mut out = Vec::new();

        if let Some(m) = r.mon.ok().and_then(|rows| rows.into_iter().next()) {
            if let (Some(oit), Some(oat), Some(next)) = (ival(&m, 0), ival(&m, 1), ival(&m, 3)) {
                let (title, sev, detail) = transaction_gap(oit, oat, next, ival(&m, 4).unwrap_or(0));
                out.push(
                    HealthCheck::new("transaction_gap", "Mantenimiento", title, sev).detail(format!(
                        "{detail} (más antigua interesante: {oit}, más antigua activa: {oat}, siguiente: {next})"
                    )),
                );
            }
            let read_only = ival(&m, 6) == Some(1);
            if !read_only {
                if let Some(fw) = ival(&m, 5) {
                    let on = fw == 1;
                    out.push(
                        HealthCheck::new(
                            "forced_writes",
                            "Configuración",
                            if on { "Escrituras forzadas activas" } else { "Escrituras forzadas apagadas" },
                            if on { Severity::Ok } else { Severity::Warning },
                        )
                        .detail(if on {
                            "Cada página se escribe a disco al confirmar: una caída del sistema no deja la base inconsistente."
                        } else {
                            "Sin escrituras forzadas el sistema operativo demora las escrituras: un corte de luz o una caída del sistema puede corromper la base. \
                             Activalas desde el servidor con gfix -write sync <base>."
                        }),
                    );
                }
            }
        }

        if let Ok(rows) = r.zero_stats {
            let objects: Vec<String> = rows.iter().map(|(i, t)| format!("{t} · {i}")).collect();
            let fixes: Vec<String> = rows.iter().map(|(i, _)| format!("SET STATISTICS INDEX {};", q(i))).collect();
            let mut check = HealthCheck::new(
                "index_statistics",
                "Rendimiento",
                if objects.is_empty() {
                    "Los índices tienen estadísticas calculadas".to_string()
                } else {
                    format!("{} índices sin estadísticas en tablas con datos", objects.len())
                },
                if objects.is_empty() { Severity::Ok } else { Severity::Warning },
            )
            .detail(
                "Firebird calcula la selectividad de un índice al crearlo y no la actualiza sola: si se creó con la tabla vacía, el optimizador \
                 cree que no sirve y arma planes malos. Recalculala con SET STATISTICS, y repetilo de vez en cuando en las tablas que cambian mucho.",
            )
            .objects(objects);
            if !fixes.is_empty() {
                check = check.fix(fixes.join("\n"));
            }
            out.push(check);
        }

        if let Ok(rows) = r.inactive {
            if !rows.is_empty() {
                let objects: Vec<String> = rows.iter().map(|r| format!("{} · {}", col(r, 1), col(r, 0))).collect();
                let fixes: Vec<String> = rows.iter().map(|r| format!("ALTER INDEX {} ACTIVE;", q(&col(r, 0)))).collect();
                out.push(
                    HealthCheck::new("inactive_indexes", "Rendimiento", format!("{} índices inactivos", objects.len()), Severity::Info)
                        .detail("Un índice inactivo no se usa ni se mantiene. Si ya no hace falta, borralo; si sí, activarlo lo reconstruye.")
                        .objects(objects)
                        .fix(fixes.join("\n")),
                );
            }
        }

        if let Ok(rows) = r.no_pk {
            let objects: Vec<String> = rows.iter().map(|r| col(r, 0)).collect();
            out.push(
                HealthCheck::new(
                    "no_primary_key",
                    "Diseño",
                    if objects.is_empty() { "Todas las tablas tienen clave primaria".to_string() } else { format!("{} tablas sin clave primaria", objects.len()) },
                    if objects.is_empty() { Severity::Ok } else { Severity::Info },
                )
                .detail("Sin clave primaria no hay forma segura de identificar una fila: se complican la edición de datos, la replicación y las comparaciones.")
                .objects(objects),
            );
        }

        for c in &mut out {
            c.objects.truncate(MAX_OBJECTS);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gap_tells_a_sweep_from_an_open_transaction() {
        assert_eq!(transaction_gap(100, 105, 200, 20_000).1, Severity::Ok);
        // The oldest active transaction is the one holding things back.
        let (title, sev, _) = transaction_gap(100, 101, 50_100, 20_000);
        assert!(title.contains("transacción abierta"), "{title}");
        assert_eq!(sev, Severity::Warning);
        // Rolled-back transactions keep the OIT behind: a sweep.
        let (title, sev, detail) = transaction_gap(100, 90_000, 90_100, 0);
        assert!(title.contains("barrido"), "{title}");
        assert_eq!(sev, Severity::Warning);
        assert!(detail.contains("apagado"));
        assert_eq!(transaction_gap(0, 300_000, 300_000, 20_000).1, Severity::Critical);
    }
}
