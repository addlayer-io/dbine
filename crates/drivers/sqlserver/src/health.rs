//! "Chequeo de salud" findings of SQL Server ([`dbine_driver::Session::health_checks`]):
//! configuration (AUTO_SHRINK, AUTO_CLOSE, PAGE_VERIFY, FULL recovery
//! without log backups, compatibility, VLFs), statistics, unused indexes
//! (with the window they were observed in), untrusted constraints, foreign
//! keys without an index, heaps and disabled indexes. Each check is its own
//! query: one that fails (no VIEW SERVER STATE, Azure limits) is skipped.

use crate::variant::Variant;
use crate::{comment_text, text, SqlServerSession};
use dbine_driver::health::{HealthCheck, Severity};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::Result;

/// Objects listed per finding at most.
const MAX_OBJECTS: usize = 200;
/// Days the index-usage counters must cover before "unused" means anything.
const MIN_WINDOW_DAYS: i64 = 14;

fn q(name: &str) -> String {
    qualified_name(Quote::Bracket, None, name)
}

fn num(r: &tiberius::Row, i: usize) -> i64 {
    text(r, i).and_then(|v| v.trim().parse::<f64>().ok()).map(|v| v as i64).unwrap_or(0)
}

impl SqlServerSession {
    pub(crate) async fn health_checks_impl(&mut self, database: &str) -> Result<Vec<HealthCheck>> {
        if !matches!(self.variant, Variant::SqlServer | Variant::AzureSql) {
            return Ok(Vec::new());
        }
        let azure = self.variant == Variant::AzureSql;
        let db = q(database);
        let mut out = Vec::new();

        // Database settings.
        if let Ok(rows) = self
            .rows(
                "SELECT CAST(is_auto_shrink_on AS nvarchar(1)), CAST(is_auto_close_on AS nvarchar(1)), page_verify_option_desc, recovery_model_desc,
                        CAST(compatibility_level AS nvarchar(8)),
                        CAST(CAST(SERVERPROPERTY('ProductMajorVersion') AS int) * 10 AS nvarchar(8))
                 FROM sys.databases WHERE name = @P1",
                &[database],
            )
            .await
        {
            if let Some(r) = rows.first() {
                let shrink = num(r, 0) == 1;
                out.push(
                    HealthCheck::new("auto_shrink", "Configuración", if shrink { "AUTO_SHRINK está activo" } else { "AUTO_SHRINK está apagado" }, if shrink { Severity::Warning } else { Severity::Ok })
                        .detail("Achicar los archivos solo fragmenta los índices y vuelve a crecer después: conviene dejarlo apagado.")
                        .fix_if(shrink, format!("ALTER DATABASE {db} SET AUTO_SHRINK OFF")),
                );
                let close = num(r, 1) == 1;
                out.push(
                    HealthCheck::new("auto_close", "Configuración", if close { "AUTO_CLOSE está activo" } else { "AUTO_CLOSE está apagado" }, if close { Severity::Warning } else { Severity::Ok })
                        .detail("Con AUTO_CLOSE la base se cierra cuando no hay conexiones y cada apertura vacía la caché.")
                        .fix_if(close, format!("ALTER DATABASE {db} SET AUTO_CLOSE OFF")),
                );
                let verify = text(r, 2).unwrap_or_default();
                if !azure {
                    let ok = verify == "CHECKSUM";
                    out.push(
                        HealthCheck::new("page_verify", "Configuración", format!("Verificación de página: {verify}"), if ok { Severity::Ok } else { Severity::Warning })
                            .detail("CHECKSUM detecta páginas dañadas al leerlas; TORN_PAGE_DETECTION o NONE pueden dejar pasar corrupción.")
                            .fix_if(!ok, format!("ALTER DATABASE {db} SET PAGE_VERIFY CHECKSUM")),
                    );
                }
                let compat = num(r, 4);
                let server = num(r, 5);
                if server > 0 && compat > 0 && compat < server {
                    out.push(
                        HealthCheck::new("compatibility", "Configuración", format!("Nivel de compatibilidad {compat}; el servidor admite {server}"), Severity::Info)
                            .detail("Con un nivel más viejo la base no usa las mejoras del optimizador. Subirlo puede cambiar planes: probalo antes en otro ambiente."),
                    );
                }
                // FULL recovery without log backups: the log only grows.
                if !azure && text(r, 3).as_deref() == Some("FULL") {
                    if let Ok(rows) = self
                        .rows(
                            "SELECT CAST(DATEDIFF(hour, MAX(CASE WHEN type = 'L' THEN backup_finish_date END), GETDATE()) AS nvarchar(20)),
                                    CAST(COUNT(CASE WHEN type = 'D' THEN 1 END) AS nvarchar(20))
                             FROM msdb.dbo.backupset WHERE database_name = @P1",
                            &[database],
                        )
                        .await
                    {
                        let hours = rows.first().and_then(|r| text(r, 0)).and_then(|h| h.parse::<i64>().ok());
                        let fulls = rows.first().map(|r| num(r, 1)).unwrap_or(0);
                        let (title, sev) = match hours {
                            // Until the first full backup the log truncates by
                            // itself: the issue is having no backup at all.
                            None if fulls == 0 => ("La base nunca tuvo un backup completo".to_string(), Severity::Warning),
                            None => ("Modelo FULL sin ningún backup de log".to_string(), Severity::Critical),
                            Some(h) if h > 24 => (format!("Modelo FULL: el último backup de log fue hace {h} horas"), Severity::Warning),
                            Some(h) => (format!("Último backup de log hace {h} horas"), Severity::Ok),
                        };
                        out.push(HealthCheck::new("log_backups", "Backups", title, sev).detail(
                            "En FULL el log se vacía solo con backups de log: sin ellos crece hasta llenar el disco. Programá backups de log, o pasá a SIMPLE si no necesitás restaurar a un momento puntual.",
                        ));
                    }
                }
            }
        }

        // Virtual log files: too many slow down recovery and log backups.
        if let Ok(rows) = self.rows("SELECT CAST(COUNT(*) AS nvarchar(20)) FROM sys.dm_db_log_info(DB_ID(@P1))", &[database]).await {
            let vlf = rows.first().map(|r| num(r, 0)).unwrap_or(0);
            if vlf > 0 {
                out.push(
                    HealthCheck::new(
                        "vlf",
                        "Espacio",
                        format!("{vlf} archivos de log virtuales (VLF)"),
                        if vlf > 1000 { Severity::Warning } else { Severity::Ok },
                    )
                    .detail("Muchos VLF hacen lentos el arranque, la restauración y los backups de log. Se reducen achicando el log y haciéndolo crecer en pasos grandes."),
                );
            }
        }

        // Statistics with many changes since their last update.
        if let Ok(rows) = self
            .rows(
                "SELECT TOP (200) s.name + '.' + o.name + ' (' + st.name + ')', QUOTENAME(s.name) + '.' + QUOTENAME(o.name),
                        CAST(sp.modification_counter AS nvarchar(20)), CAST(sp.rows AS nvarchar(20))
                 FROM sys.stats st
                 JOIN sys.objects o ON o.object_id = st.object_id AND o.is_ms_shipped = 0 AND o.type = 'U'
                 JOIN sys.schemas s ON s.schema_id = o.schema_id
                 CROSS APPLY sys.dm_db_stats_properties(st.object_id, st.stats_id) sp
                 WHERE sp.rows > 1000 AND sp.modification_counter > sp.rows * 0.2
                 ORDER BY sp.modification_counter DESC",
                &[],
            )
            .await
        {
            let objects: Vec<String> = rows.iter().filter_map(|r| text(r, 0)).collect();
            let mut tables: Vec<String> = rows.iter().filter_map(|r| text(r, 1)).collect();
            tables.sort();
            tables.dedup();
            out.push(
                HealthCheck::new(
                    "stale_stats",
                    "Rendimiento",
                    if objects.is_empty() { "Estadísticas al día".to_string() } else { format!("{} estadísticas con más de 20 % de filas cambiadas", objects.len()) },
                    if objects.is_empty() { Severity::Ok } else { Severity::Warning },
                )
                .detail("Con estadísticas viejas el optimizador estima mal la cantidad de filas y elige planes lentos.")
                .objects(objects)
                .fix_if(!tables.is_empty(), tables.iter().map(|t| format!("UPDATE STATISTICS {t};")).collect::<Vec<_>>().join("\n")),
            );
        }

        // Unused indexes, with the window the counters cover.
        let window = if azure {
            None
        } else {
            self.rows("SELECT CAST(DATEDIFF(day, sqlserver_start_time, GETDATE()) AS nvarchar(20)) FROM sys.dm_os_sys_info", &[])
                .await
                .ok()
                .and_then(|r| r.first().map(|r| num(r, 0)))
        };
        if let Ok(rows) = self
            .rows(
                "SELECT TOP (200) s.name + '.' + o.name + ' · ' + i.name, QUOTENAME(i.name) + ' ON ' + QUOTENAME(s.name) + '.' + QUOTENAME(o.name),
                        CAST(ISNULL(u.user_updates, 0) AS nvarchar(20))
                 FROM sys.indexes i
                 JOIN sys.objects o ON o.object_id = i.object_id AND o.is_ms_shipped = 0 AND o.type = 'U'
                 JOIN sys.schemas s ON s.schema_id = o.schema_id
                 LEFT JOIN sys.dm_db_index_usage_stats u ON u.object_id = i.object_id AND u.index_id = i.index_id AND u.database_id = DB_ID()
                 WHERE i.type_desc = 'NONCLUSTERED' AND i.is_primary_key = 0 AND i.is_unique_constraint = 0 AND i.is_unique = 0
                   AND i.is_disabled = 0 AND i.is_hypothetical = 0
                   AND ISNULL(u.user_seeks, 0) + ISNULL(u.user_scans, 0) + ISNULL(u.user_lookups, 0) = 0
                 ORDER BY ISNULL(u.user_updates, 0) DESC",
                &[],
            )
            .await
        {
            let objects: Vec<String> = rows.iter().filter_map(|r| text(r, 0)).collect();
            let conclusive = window.is_some_and(|d| d >= MIN_WINDOW_DAYS);
            let since = match window {
                Some(d) => format!("los contadores cubren {d} días, desde el último reinicio"),
                None => "el servidor no dice desde cuándo cuenta".to_string(),
            };
            let (title, sev) = if objects.is_empty() {
                ("Todos los índices se usaron".to_string(), Severity::Ok)
            } else if conclusive {
                (format!("{} índices sin lecturas ({since})", objects.len()), Severity::Warning)
            } else {
                (format!("No concluyente: {} índices sin lecturas, pero {since}", objects.len()), Severity::Info)
            };
            let mut check = HealthCheck::new("unused_indexes", "Rendimiento", title, sev)
                .detail("Un índice que nunca se lee solo cuesta en cada escritura. Antes de borrarlo, tené en cuenta procesos de fin de mes o reportes ocasionales, y que en un grupo de disponibilidad cada réplica cuenta por separado.")
                .objects(objects);
            if conclusive && !rows.is_empty() {
                let drops: Vec<String> = rows.iter().filter_map(|r| text(r, 1)).map(|i| format!("-- DROP INDEX {};", comment_text(&i))).collect();
                check = check.fix(format!("-- Revisá cada uno antes de borrarlo:\n{}", drops.join("\n")));
            }
            out.push(check);
        }

        // Constraints the engine doesn't trust (created or re-enabled WITH NOCHECK).
        if let Ok(rows) = self
            .rows(
                "SELECT TOP (200) s.name + '.' + o.name + ' · ' + c.name, QUOTENAME(s.name) + '.' + QUOTENAME(o.name), QUOTENAME(c.name)
                 FROM (SELECT name, parent_object_id FROM sys.foreign_keys WHERE is_not_trusted = 1 AND is_disabled = 0
                       UNION ALL SELECT name, parent_object_id FROM sys.check_constraints WHERE is_not_trusted = 1 AND is_disabled = 0) c
                 JOIN sys.objects o ON o.object_id = c.parent_object_id
                 JOIN sys.schemas s ON s.schema_id = o.schema_id",
                &[],
            )
            .await
        {
            let objects: Vec<String> = rows.iter().filter_map(|r| text(r, 0)).collect();
            let fixes: Vec<String> = rows.iter().filter_map(|r| Some(format!("ALTER TABLE {} WITH CHECK CHECK CONSTRAINT {};", text(r, 1)?, text(r, 2)?))).collect();
            out.push(
                HealthCheck::new(
                    "untrusted_constraints",
                    "Integridad",
                    if objects.is_empty() { "Todas las restricciones son confiables".to_string() } else { format!("{} restricciones no confiables", objects.len()) },
                    if objects.is_empty() { Severity::Ok } else { Severity::Warning },
                )
                .detail("Una FK o CHECK creada o reactivada WITH NOCHECK no se usa para optimizar consultas y puede haber filas que no la cumplen. Revalidarla recorre la tabla.")
                .objects(objects)
                .fix_if(!fixes.is_empty(), fixes.join("\n")),
            );
        }

        // Foreign keys whose columns no index starts with.
        if let Ok(rows) = self
            .rows(
                "SELECT TOP (200) s.name + '.' + o.name + ' (' + c.name + ')', QUOTENAME(s.name) + '.' + QUOTENAME(o.name), c.name, o.name
                 FROM sys.foreign_key_columns fc
                 JOIN sys.objects o ON o.object_id = fc.parent_object_id
                 JOIN sys.schemas s ON s.schema_id = o.schema_id
                 JOIN sys.columns c ON c.object_id = fc.parent_object_id AND c.column_id = fc.parent_column_id
                 WHERE fc.constraint_column_id = 1
                   AND NOT EXISTS (SELECT 1 FROM sys.index_columns ic
                                   WHERE ic.object_id = fc.parent_object_id AND ic.column_id = fc.parent_column_id AND ic.key_ordinal = 1)",
                &[],
            )
            .await
        {
            let objects: Vec<String> = rows.iter().filter_map(|r| text(r, 0)).collect();
            let fixes: Vec<String> = rows
                .iter()
                .filter_map(|r| {
                    let (table, col, name) = (text(r, 1)?, text(r, 2)?, text(r, 3)?);
                    Some(format!("CREATE INDEX {} ON {table} ({});", q(&format!("IX_{name}_{col}")), q(&col)))
                })
                .collect();
            out.push(
                HealthCheck::new(
                    "fk_without_index",
                    "Rendimiento",
                    if objects.is_empty() { "Todas las claves foráneas tienen índice".to_string() } else { format!("{} claves foráneas sin índice", objects.len()) },
                    if objects.is_empty() { Severity::Ok } else { Severity::Info },
                )
                .detail("Sin índice, borrar o actualizar en la tabla padre recorre la tabla hija entera, y los joins por esa columna son más lentos.")
                .objects(objects)
                .fix_if(!fixes.is_empty(), fixes.join("\n")),
            );
        }

        // Heaps: tables without a clustered index.
        if let Ok(rows) = self
            .rows(
                "SELECT TOP (200) s.name + '.' + o.name
                 FROM sys.indexes i JOIN sys.objects o ON o.object_id = i.object_id AND o.is_ms_shipped = 0 AND o.type = 'U'
                 JOIN sys.schemas s ON s.schema_id = o.schema_id
                 WHERE i.index_id = 0",
                &[],
            )
            .await
        {
            let objects: Vec<String> = rows.iter().filter_map(|r| text(r, 0)).collect();
            if !objects.is_empty() {
                out.push(
                    HealthCheck::new("heaps", "Diseño", format!("{} tablas sin índice clustered (heaps)", objects.len()), Severity::Info)
                        .detail("Un heap no tiene orden: las búsquedas por rango y las actualizaciones que agrandan filas (forwarded records) son más lentas. Suele convenir un índice clustered, normalmente la clave primaria.")
                        .objects(objects),
                );
            }
        }

        // Disabled indexes.
        if let Ok(rows) = self
            .rows(
                "SELECT s.name + '.' + o.name + ' · ' + i.name, QUOTENAME(i.name) + ' ON ' + QUOTENAME(s.name) + '.' + QUOTENAME(o.name)
                 FROM sys.indexes i JOIN sys.objects o ON o.object_id = i.object_id AND o.is_ms_shipped = 0
                 JOIN sys.schemas s ON s.schema_id = o.schema_id
                 WHERE i.is_disabled = 1",
                &[],
            )
            .await
        {
            let objects: Vec<String> = rows.iter().filter_map(|r| text(r, 0)).collect();
            if !objects.is_empty() {
                let fixes: Vec<String> = rows.iter().filter_map(|r| text(r, 1)).map(|i| format!("ALTER INDEX {i} REBUILD;")).collect();
                out.push(
                    HealthCheck::new("disabled_indexes", "Rendimiento", format!("{} índices deshabilitados", objects.len()), Severity::Info)
                        .detail("Un índice deshabilitado no se usa ni se mantiene. Si ya no hace falta, borralo; si sí, reconstruirlo lo vuelve a habilitar.")
                        .objects(objects)
                        .fix(fixes.join("\n")),
                );
            }
        }

        for c in &mut out {
            c.objects.truncate(MAX_OBJECTS);
        }
        Ok(out)
    }
}

/// A fix only when there's something to fix.
trait FixIf {
    fn fix_if(self, when: bool, script: String) -> Self;
}

impl FixIf for HealthCheck {
    fn fix_if(self, when: bool, script: String) -> Self {
        if when {
            self.fix(script)
        } else {
            self
        }
    }
}
