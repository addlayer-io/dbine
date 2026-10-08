//! "Chequeo de salud" findings of the ODBC presets whose catalog answers
//! them ([`dbine_driver::Session::health_checks`]):
//!
//! - Db2 (LUW): invalid objects (SYSCAT.INVALIDOBJECTS), tables in
//!   reorg-pending or unavailable state (SYSIBMADM.ADMINTABINFO), tables
//!   in set-integrity-pending state, tables never analyzed by RUNSTATS and
//!   tables without a primary key, in the user schemas.
//! - Sybase ASE: the database's `sp_dboption` options that matter for
//!   recovery or access, the log sharing a device with the data, and
//!   tables without any index.
//! - Informix (and GBase 8s): a database without transaction logging,
//!   tables never analyzed by UPDATE STATISTICS and tables without a
//!   primary key.
//!
//! Every other preset: none (Db2 for i and z/OS, Teradata, Vertica and the
//! rest keep this in catalogs DBine doesn't read yet; the generic preset
//! doesn't know the engine). Each check is its own query: one that fails
//! (no access to a view, an older release) is skipped.

use crate::design::{self, Eng};
use crate::{col, OdbcSession, Rows};
use dbine_driver::health::{HealthCheck, Severity};
use dbine_driver::Result;

/// Objects listed per finding at most.
const MAX_OBJECTS: usize = 200;

/// Db2's schemas that belong to the system.
const DB2_USER: &str = "NOT LIKE 'SYS%' AND {} NOT IN ('NULLID', 'SQLJ', 'IBM_RTMON')";

fn db2_user(col: &str) -> String {
    format!("{col} {}", DB2_USER.replace("{}", col))
}

fn s(r: &[Option<String>], i: usize) -> String {
    col(r, i).map(|v| v.trim().to_string()).unwrap_or_default()
}

fn int(r: &[Option<String>], i: usize) -> i64 {
    s(r, i).parse().unwrap_or(0)
}

/// `"NAME"`, doubling embedded quotes.
fn dq(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `'text'`, doubling embedded quotes.
fn lit(v: &str) -> String {
    format!("'{}'", v.replace('\'', "''"))
}

/// An identifier usable unquoted (ASE and Informix catalog prefixes).
fn plain(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.starts_with(|c: char| c.is_ascii_digit())
}

/// A Db2 command run through SYSPROC.ADMIN_CMD.
pub(crate) fn db2_cmd(cmd: &str) -> String {
    format!("CALL SYSPROC.ADMIN_CMD({});", lit(cmd))
}

fn list(rows: &Rows, f: impl Fn(&[Option<String>]) -> String) -> Vec<String> {
    rows.iter().map(|r| f(r)).collect()
}

trait FixIfSome {
    fn fix_if_some(self, script: Option<String>) -> Self;
}

impl FixIfSome for HealthCheck {
    fn fix_if_some(self, script: Option<String>) -> Self {
        match script {
            Some(f) => self.fix(f),
            None => self,
        }
    }
}

fn found(id: &str, category: &str, none: &str, some: String, sev: Severity, n: usize) -> HealthCheck {
    if n == 0 {
        HealthCheck::new(id, category, none, Severity::Ok)
    } else {
        HealthCheck::new(id, category, some, sev)
    }
}

impl OdbcSession {
    pub(crate) async fn health_checks_impl(&mut self, database: &str) -> Result<Vec<HealthCheck>> {
        let db = if database.is_empty() { self.database.clone() } else { database.to_string() };
        let mut out = match design::eng(self.preset) {
            Eng::Db2 => self.db2_health().await,
            Eng::Ase => self.ase_health(&db).await,
            Eng::Informix => self.informix_health(&db).await,
            _ => Vec::new(),
        };
        for c in &mut out {
            c.objects.truncate(MAX_OBJECTS);
        }
        Ok(out)
    }

    async fn rows_of(&self, sql: &str, params: &[&str]) -> Result<Rows> {
        self.query(sql.to_string(), params.iter().map(|p| p.to_string()).collect()).await
    }

    async fn db2_health(&self) -> Vec<HealthCheck> {
        let mut out = Vec::new();

        let sql = format!(
            "SELECT OBJECTSCHEMA, OBJECTNAME, OBJECTTYPE FROM SYSCAT.INVALIDOBJECTS WHERE {}
             ORDER BY OBJECTSCHEMA, OBJECTNAME FETCH FIRST 200 ROWS ONLY",
            db2_user("OBJECTSCHEMA")
        );
        if let Ok(rows) = self.rows_of(&sql, &[]).await {
            let mut schemas: Vec<String> = rows.iter().map(|r| s(r, 0)).collect();
            schemas.dedup();
            let mut c = found("invalid_objects", "Integridad", "No hay objetos inválidos", format!("{} objetos inválidos", rows.len()), Severity::Warning, rows.len())
                .detail("Un objeto inválido (vista, rutina, trigger…) falla o se revalida al usarse; si lo que cambió lo rompió, la revalidación falla y SYSCAT.INVALIDOBJECTS dice por qué.")
                .objects(list(&rows, |r| format!("{}.{} ({})", s(r, 0), s(r, 1), s(r, 2))));
            if !schemas.is_empty() {
                c = c.fix(schemas.iter().map(|sch| format!("CALL SYSPROC.ADMIN_REVALIDATE_DB_OBJECTS(NULL, {}, NULL);", lit(sch))).collect::<Vec<_>>().join("\n"));
            }
            out.push(c);
        }

        let sql = format!(
            "SELECT TABSCHEMA, TABNAME, REORG_PENDING, AVAILABLE FROM SYSIBMADM.ADMINTABINFO
             WHERE (REORG_PENDING = 'Y' OR AVAILABLE = 'N') AND {} FETCH FIRST 200 ROWS ONLY",
            db2_user("TABSCHEMA")
        );
        if let Ok(rows) = self.rows_of(&sql, &[]).await {
            let unavailable = rows.iter().any(|r| s(r, 3) == "N");
            let fixes: Vec<String> = rows
                .iter()
                .filter(|r| s(r, 2) == "Y")
                .map(|r| db2_cmd(&format!("REORG TABLE {}.{}", dq(&s(r, 0)), dq(&s(r, 1)))))
                .collect();
            let mut c = found(
                "reorg_pending",
                "Mantenimiento",
                "Ninguna tabla espera un REORG",
                format!("{} tablas en espera de REORG o no disponibles", rows.len()),
                if unavailable { Severity::Critical } else { Severity::Warning },
                rows.len(),
            )
            .detail("Después de ciertos ALTER TABLE la tabla queda en espera de REORG: se puede leer, pero no cambiar. Una tabla no disponible (por ejemplo, tras una carga NOT RECOVERABLE restaurada) hay que recargarla o volver a crearla.")
            .objects(list(&rows, |r| format!("{}.{}{}", s(r, 0), s(r, 1), if s(r, 3) == "N" { " (no disponible)" } else { "" })));
            if !fixes.is_empty() {
                c = c.fix(fixes.join("\n"));
            }
            out.push(c);
        }

        let sql = format!(
            "SELECT TABSCHEMA, TABNAME FROM SYSCAT.TABLES WHERE STATUS = 'C' AND {} ORDER BY 1, 2 FETCH FIRST 200 ROWS ONLY",
            db2_user("TABSCHEMA")
        );
        if let Ok(rows) = self.rows_of(&sql, &[]).await {
            if !rows.is_empty() {
                let fixes: Vec<String> = rows.iter().map(|r| format!("SET INTEGRITY FOR {}.{} IMMEDIATE CHECKED;", dq(&s(r, 0)), dq(&s(r, 1)))).collect();
                out.push(
                    HealthCheck::new("set_integrity_pending", "Integridad", format!("{} tablas en estado de integridad pendiente", rows.len()), Severity::Critical)
                        .detail("Una tabla en SET INTEGRITY pending (tras un LOAD o al agregar una restricción) no se puede leer ni cambiar hasta que se verifiquen sus restricciones.")
                        .objects(list(&rows, |r| format!("{}.{}", s(r, 0), s(r, 1))))
                        .fix(fixes.join("\n")),
                );
            }
        }

        let sql = format!(
            "SELECT TABSCHEMA, TABNAME FROM SYSCAT.TABLES WHERE TYPE = 'T' AND STATS_TIME IS NULL AND {}
             ORDER BY 1, 2 FETCH FIRST 200 ROWS ONLY",
            db2_user("TABSCHEMA")
        );
        if let Ok(rows) = self.rows_of(&sql, &[]).await {
            let fixes: Vec<String> = rows
                .iter()
                .map(|r| db2_cmd(&format!("RUNSTATS ON TABLE {}.{} WITH DISTRIBUTION AND INDEXES ALL", dq(&s(r, 0)), dq(&s(r, 1)))))
                .collect();
            let mut c = found("missing_stats", "Rendimiento", "Todas las tablas tienen estadísticas", format!("{} tablas sin estadísticas (nunca se corrió RUNSTATS)", rows.len()), Severity::Warning, rows.len())
                .detail("Sin estadísticas el optimizador supone tamaños por defecto y puede elegir planes muy malos. La recolección automática las junta con el tiempo; si está apagada, corré RUNSTATS.")
                .objects(list(&rows, |r| format!("{}.{}", s(r, 0), s(r, 1))));
            if !fixes.is_empty() {
                c = c.fix(fixes.join("\n"));
            }
            out.push(c);
        }

        let sql = format!(
            "SELECT t.TABSCHEMA, t.TABNAME FROM SYSCAT.TABLES t
             WHERE t.TYPE = 'T' AND {}
               AND NOT EXISTS (SELECT 1 FROM SYSCAT.TABCONST k WHERE k.TABSCHEMA = t.TABSCHEMA AND k.TABNAME = t.TABNAME AND k.TYPE = 'P')
             ORDER BY 1, 2 FETCH FIRST 200 ROWS ONLY",
            db2_user("t.TABSCHEMA")
        );
        if let Ok(rows) = self.rows_of(&sql, &[]).await {
            out.push(
                found("no_primary_key", "Diseño", "Todas las tablas tienen clave primaria", format!("{} tablas sin clave primaria", rows.len()), Severity::Info, rows.len())
                    .detail("Sin clave primaria no hay forma segura de identificar una fila: se complican la edición de datos, la replicación y las comparaciones.")
                    .objects(list(&rows, |r| format!("{}.{}", s(r, 0), s(r, 1)))),
            );
        }
        out
    }

    async fn ase_health(&self, db: &str) -> Vec<HealthCheck> {
        let mut out = Vec::new();
        if let Ok(rows) = self.rows_of("SELECT status, status2 FROM master..sysdatabases WHERE name = ?", &[db]).await {
            if let Some(r) = rows.first() {
                let (st, _st2) = (int(r, 0), int(r, 1));
                let on = |bit: i64| st & bit != 0;
                let option = |key: &str| format!("USE master\ngo\nEXEC sp_dboption {}, {}, false\ngo\nUSE {db}\ngo\nCHECKPOINT\ngo", lit(db), lit(key));
                if on(4096) || on(2048) {
                    let key = if on(4096) { "single user" } else { "dbo use only" };
                    out.push(
                        HealthCheck::new("access_restricted", "Configuración", format!("La base está en «{key}»"), Severity::Warning)
                            .detail("Solo un usuario (o solo el dueño) puede entrar. Suele quedar así después de un mantenimiento: si ya terminó, sacá la opción.")
                            .fix_if_some(plain(db).then(|| option(key))),
                    );
                }
                let trunc = on(8);
                out.push(
                    HealthCheck::new(
                        "trunc_log_on_chkpt",
                        "Backups",
                        if trunc { "«trunc log on chkpt» está activo" } else { "«trunc log on chkpt» está apagado" },
                        if trunc { Severity::Info } else { Severity::Ok },
                    )
                    .detail("Con la opción activa el log se vacía en cada checkpoint: no se puede hacer DUMP TRANSACTION ni restaurar a un momento puntual. Está bien en bases de desarrollo; en producción, apagala y programá dumps de log."),
                );
                if on(4) {
                    out.push(
                        HealthCheck::new("select_into", "Backups", "«select into/bulkcopy/pllsort» está activo", Severity::Info)
                            .detail("Las operaciones mínimamente registradas (SELECT INTO, bcp rápido) cortan la cadena de dumps de log: después de una hace falta un DUMP DATABASE para volver a poder hacer DUMP TRANSACTION."),
                    );
                }
            }
        }

        // Log and data on the same device fragments (segmap 3 = data, 4 = log).
        if let Ok(rows) = self
            .rows_of("SELECT COUNT(*) FROM master..sysusages WHERE dbid = db_id(?) AND (segmap & 4) = 4 AND (segmap & 3) <> 0", &[db])
            .await
        {
            let mixed = rows.first().map(|r| int(r, 0)).unwrap_or(0) > 0;
            out.push(
                HealthCheck::new(
                    "mixed_log",
                    "Espacio",
                    if mixed { "El log comparte espacio con los datos" } else { "El log tiene su propio espacio" },
                    if mixed { Severity::Warning } else { Severity::Ok },
                )
                .detail("Con datos y log mezclados no se puede hacer DUMP TRANSACTION (solo dumps completos) y un log que crece le quita espacio a los datos. Conviene mover el log a un dispositivo propio (ALTER DATABASE … LOG ON y sp_logdevice)."),
            );
        }

        if plain(db) {
            let sql = format!(
                "SELECT TOP 200 user_name(o.uid) + '.' + o.name FROM {db}..sysobjects o
                 WHERE o.type = 'U' AND NOT EXISTS (SELECT 1 FROM {db}..sysindexes i WHERE i.id = o.id AND i.indid > 0)
                 ORDER BY 1"
            );
            if let Ok(rows) = self.rows_of(&sql, &[]).await {
                out.push(
                    found("no_index", "Diseño", "Todas las tablas tienen algún índice", format!("{} tablas sin ningún índice", rows.len()), Severity::Info, rows.len())
                        .detail("Sin índices (ni clave primaria) cada búsqueda recorre la tabla entera, y con el bloqueo por página o tabla las escrituras se pisan entre sí.")
                        .objects(list(&rows, |r| s(r, 0))),
                );
            }
        }
        out
    }

    async fn informix_health(&self, db: &str) -> Vec<HealthCheck> {
        let mut out = Vec::new();
        if let Ok(rows) = self.rows_of("SELECT is_logging, is_buff_log FROM sysmaster:sysdatabases WHERE name = ?", &[db]).await {
            if let Some(r) = rows.first() {
                let logging = int(r, 0) == 1;
                let buffered = int(r, 1) == 1;
                let (title, sev) = match (logging, buffered) {
                    (false, _) => ("La base no tiene log de transacciones", Severity::Warning),
                    (true, true) => ("Log de transacciones con buffer", Severity::Info),
                    (true, false) => ("Log de transacciones sin buffer", Severity::Ok),
                };
                out.push(HealthCheck::new("logging", "Backups", title, sev).detail(
                    "Sin log no hay ROLLBACK ni recuperación de lo confirmado después del último backup. Con buffer, una caída puede perder las últimas transacciones confirmadas. \
                     El modo se cambia con ondblog u ontape y un backup de nivel 0, no con SQL.",
                ));
            }
        }
        if !plain(db) {
            return out;
        }
        let sql = format!(
            "SELECT FIRST 200 TRIM(owner) || '.' || TRIM(tabname), TRIM(tabname) FROM {db}:systables
             WHERE tabid >= 100 AND tabtype = 'T' AND ustlowts IS NULL ORDER BY 1"
        );
        if let Ok(rows) = self.rows_of(&sql, &[]).await {
            let fixes: Vec<String> = rows
                .iter()
                .filter(|r| plain(&s(r, 1)))
                .map(|r| format!("UPDATE STATISTICS LOW FOR TABLE {};", s(r, 1)))
                .collect();
            let mut c = found("missing_stats", "Rendimiento", "Todas las tablas tienen estadísticas", format!("{} tablas sin UPDATE STATISTICS", rows.len()), Severity::Warning, rows.len())
                .detail("Sin estadísticas el optimizador no sabe cuántas filas tiene la tabla y puede elegir planes malos. Corré UPDATE STATISTICS (LOW para todas; MEDIUM o HIGH en las columnas de filtro) o programalo con la tarea automática.")
                .objects(list(&rows, |r| s(r, 0)));
            if !fixes.is_empty() {
                c = c.fix(fixes.join("\n"));
            }
            out.push(c);
        }
        let sql = format!(
            "SELECT FIRST 200 TRIM(t.owner) || '.' || TRIM(t.tabname) FROM {db}:systables t
             WHERE t.tabid >= 100 AND t.tabtype = 'T'
               AND NOT EXISTS (SELECT 1 FROM {db}:sysconstraints c WHERE c.tabid = t.tabid AND c.constrtype = 'P')
             ORDER BY 1"
        );
        if let Ok(rows) = self.rows_of(&sql, &[]).await {
            out.push(
                found("no_primary_key", "Diseño", "Todas las tablas tienen clave primaria", format!("{} tablas sin clave primaria", rows.len()), Severity::Info, rows.len())
                    .detail("Sin clave primaria no hay forma segura de identificar una fila: se complican la edición de datos, la replicación (Enterprise Replication la exige) y las comparaciones.")
                    .objects(list(&rows, |r| s(r, 0))),
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_for_db2_commands() {
        assert_eq!(db2_cmd(&format!("REORG TABLE {}.{}", dq("S"), dq("it's"))), "CALL SYSPROC.ADMIN_CMD('REORG TABLE \"S\".\"it''s\"');");
        assert_eq!(db2_user("TABSCHEMA"), "TABSCHEMA NOT LIKE 'SYS%' AND TABSCHEMA NOT IN ('NULLID', 'SQLJ', 'IBM_RTMON')");
        assert!(plain("ventas_2024") && !plain("a b") && !plain("x;drop") && !plain("1abc"));
    }
}
