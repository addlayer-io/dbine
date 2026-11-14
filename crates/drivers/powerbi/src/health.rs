//! The engine's own health checks (spec §4.6 `health_checks`). Owned by
//! part C.
//!
//! Each its own read, skipped when not readable: last refresh failed / no
//! success in 7 days / schedule disabled; measures in an error state;
//! bidirectional and many-to-many relationships; inactive relationships
//! (info); auto date/time tables; calculated columns on tables over 1 M
//! rows; columns over 10 % of the model's memory (admin); RLS roles
//! without members where effective identity is required; model size near
//! the capacity's limit (SKU known). Fixes are advice, never scripts. The
//! text holds no table data, data-source hosts or M (the phone sees it,
//! S6): a failed refresh shows its error code only.

use crate::dax::quote::column_ref;
use crate::http::Priority;
use crate::meta::service::{named_model, Service};
use crate::meta::{is_one, relationship_name, ModelMeta, TableKind};
use crate::monitor::{history, sku_memory_gb, storage};
use crate::objects::both_directions;
use crate::refresh::{exception_text, Schedule};
use crate::Ctx;
use chrono::{DateTime, Duration, Utc};
use dbine_driver::health::{HealthCheck, Severity};
use dbine_driver::Result;

pub async fn health_checks(cx: &mut Ctx, database: &str) -> Result<Vec<HealthCheck>> {
    checks_in(cx, database).await
}

const REFRESH: &str = "Actualización";
const MODEL: &str = "Modelo";
const RELATIONS: &str = "Relaciones";
const SECURITY: &str = "Seguridad";
const MEMORY: &str = "Memoria";

pub(crate) async fn checks_in<S: Service>(s: &mut S, database: &str) -> Result<Vec<HealthCheck>> {
    checks_at(s, database, Utc::now()).await
}

async fn checks_at<S: Service>(
    s: &mut S,
    database: &str,
    now: DateTime<Utc>,
) -> Result<Vec<HealthCheck>> {
    let m = named_model(s, database).await?;
    let mut out = Vec::new();
    let mut skipped: Vec<&str> = Vec::new();

    // Refreshes and the schedule.
    match history(s, &m).await {
        Some(runs) => {
            if let Some(last) = runs.iter().find(|r| !r.running()) {
                if last.status == "Failed" {
                    let code = last
                        .exception
                        .as_deref()
                        .map(|e| exception_text(e, false))
                        .unwrap_or_else(|| "sin código".into());
                    out.push(
                        HealthCheck::new("refresh_failed", REFRESH, "La última actualización falló", Severity::Critical)
                            .detail(format!("Código de error de Power BI: {code}. Revisá las credenciales de los orígenes y el historial en Programadas › En los servidores.")),
                    );
                }
            }
            let week_ago = now - Duration::days(7);
            let recent_success = runs.iter().any(|r| {
                r.status == "Completed"
                    && r.end
                        .as_deref()
                        .and_then(crate::refresh::utc)
                        .and_then(|e| DateTime::parse_from_rfc3339(&e).ok())
                        .is_some_and(|e| e.with_timezone(&Utc) >= week_ago)
            });
            if !runs.is_empty() && !recent_success {
                out.push(
                    HealthCheck::new("refresh_stale", REFRESH, "Ninguna actualización correcta en 7 días", Severity::Warning)
                        .detail("Los datos del modelo pueden estar viejos: los informes muestran lo que tenía en la última actualización correcta."),
                );
            } else if recent_success && !out.iter().any(|c| c.id == "refresh_failed") {
                out.push(HealthCheck::new(
                    "refresh_ok",
                    REFRESH,
                    "Las actualizaciones están al día",
                    Severity::Ok,
                ));
            }
        }
        None => skipped.push("las actualizaciones (hace falta permiso de escritura en el modelo)"),
    }
    if m.is_refreshable != Some(false) {
        if let Ok(v) = s
            .get(
                &format!("{}/refreshSchedule", m.path()),
                Priority::Background,
            )
            .await
        {
            let sch = Schedule::from_json(&v);
            if sch.enabled == Some(false) {
                out.push(
                    HealthCheck::new("schedule_disabled", REFRESH, "La actualización programada está desactivada", Severity::Info)
                        .detail("El modelo se actualiza solo cuando alguien lo pide. Si es intencional, no hay nada que hacer."),
                );
            }
        }
    }

    // What the metadata tells.
    match crate::meta::model_for(s, &m).await {
        Ok(meta) => model_checks(&meta, m.effective_identity_required, &mut out, &mut skipped),
        Err(e) if crate::meta::is_fatal(&e) => return Err(e),
        Err(_) => skipped.push("la estructura del modelo (no se pudo leer)"),
    }

    // Memory (model admin, capacity).
    match storage(s, &m).await? {
        Some(st) if st.total > 0 => {
            let big: Vec<String> = st
                .columns
                .iter()
                .filter(|(_, _, b)| *b * 10 > st.total)
                .map(|(t, c, b)| format!("{} ({} %)", column_ref(t, c), b * 100 / st.total))
                .collect();
            if !big.is_empty() {
                out.push(
                    HealthCheck::new("large_columns", MEMORY, "Columnas que ocupan más del 10 % de la memoria del modelo", Severity::Warning)
                        .detail("Columnas de texto largo o de muchos valores distintos (identificadores, fechas con hora) ocupan mucho. Si no se usan en informes, quitarlas del modelo, o separar fecha y hora, lo achica.")
                        .objects(big),
                );
            }
            let (_, sku) = crate::monitor::capacity_text(s, &m).await;
            if let Some(limit) = sku.as_deref().and_then(sku_memory_gb) {
                let limit = limit * 1024 * 1024 * 1024;
                if st.total * 10 >= limit * 8 {
                    out.push(
                        HealthCheck::new("near_capacity_limit", MEMORY, "El modelo está cerca del límite de memoria de la capacidad", Severity::Warning)
                            .detail(format!(
                                "Ocupa {} MB de {} MB. Una actualización completa necesita memoria extra y puede fallar: conviene achicar el modelo o usar una capacidad más grande.",
                                st.total / 1024 / 1024,
                                limit / 1024 / 1024
                            )),
                    );
                }
            }
        }
        _ => skipped.push("la memoria (hacen falta permisos de administrador del modelo y capacidad Premium o Fabric)"),
    }

    if !skipped.is_empty() {
        out.push(
            HealthCheck::new(
                "not_checked",
                MODEL,
                "Algunas cosas no se revisaron",
                Severity::Info,
            )
            .detail(format!("No se revisaron {}.", skipped.join("; "))),
        );
    }
    Ok(out)
}

/// A measure state that means it can't be computed. Learn describes
/// `INFO.VIEW.MEASURES[State]` only as "such as valid or error" and the
/// exact strings are a live check, so only the known error states count
/// (TOM's `ObjectState` errors, any case); anything else, `Ready` or
/// `CalculationNeeded` included, is fine.
fn is_error_state(state: &str) -> bool {
    const ERRORS: &[&str] = &[
        "SemanticError",
        "EvaluationError",
        "DependencyError",
        "SyntaxError",
        "Error",
        "Invalid",
    ];
    let s = state.trim();
    ERRORS.iter().any(|e| s.eq_ignore_ascii_case(e))
}

fn model_checks(
    meta: &ModelMeta,
    effective_identity_required: Option<bool>,
    out: &mut Vec<HealthCheck>,
    skipped: &mut Vec<&str>,
) {
    let broken: Vec<String> = meta
        .measures
        .iter()
        .filter(|m| m.state.as_deref().is_some_and(is_error_state))
        .map(|m| column_ref(&m.table, &m.name))
        .collect();
    if !broken.is_empty() {
        out.push(
            HealthCheck::new("measures_errors", MODEL, "Medidas con errores", Severity::Warning)
                .detail("Estas medidas no se pueden calcular (una columna o medida que usan cambió o se borró): los informes que las usan muestran error.")
                .objects(broken),
        );
    }
    if meta.tier == crate::meta::Tier::Stats {
        skipped.push(
            "las relaciones, medidas y roles (con estos permisos solo se leen tablas y columnas)",
        );
    }
    let both: Vec<String> = meta
        .model_relationships()
        .filter(|r| both_directions(r.cross_filtering.as_deref()))
        .map(relationship_name)
        .collect();
    if !both.is_empty() {
        out.push(
            HealthCheck::new("bidirectional", RELATIONS, "Relaciones que filtran en ambas direcciones", Severity::Warning)
                .detail("El filtro en ambas direcciones puede dar resultados ambiguos y hace más lentas las consultas. Preferí una dirección y CROSSFILTER en las medidas que lo necesiten.")
                .objects(both),
        );
    }
    let many: Vec<String> = meta
        .model_relationships()
        .filter(|r| {
            !is_one(r.to_cardinality.as_deref(), true)
                && !is_one(r.from_cardinality.as_deref(), false)
        })
        .map(relationship_name)
        .collect();
    if !many.is_empty() {
        out.push(
            HealthCheck::new("many_to_many", RELATIONS, "Relaciones de muchos a muchos", Severity::Warning)
                .detail("Una relación de muchos a muchos suma valores repetidos si no se piensa con cuidado; suele convenir una tabla puente.")
                .objects(many),
        );
    }
    let inactive: Vec<String> = meta
        .model_relationships()
        .filter(|r| !r.active)
        .map(relationship_name)
        .collect();
    if !inactive.is_empty() {
        out.push(
            HealthCheck::new(
                "inactive_relationships",
                RELATIONS,
                "Relaciones inactivas",
                Severity::Info,
            )
            .detail("Solo filtran en las medidas que las activan con USERELATIONSHIP.")
            .objects(inactive),
        );
    }
    let auto = meta
        .tables
        .iter()
        .filter(|t| t.kind == TableKind::AutoDate)
        .count();
    if auto > 0 {
        out.push(
            HealthCheck::new("auto_date_time", MODEL, "El modelo tiene tablas de fecha automáticas", Severity::Warning)
                .detail(format!("Hay {auto}: Power BI Desktop crea una tabla de fechas oculta por cada columna de fecha, y agrandan el modelo. Desactivá «Fecha y hora automáticas» en Archivo › Opciones › Carga de datos de Power BI Desktop y usá una tabla de fechas propia.")),
        );
    }
    let calc_big: Vec<String> = meta
        .tables
        .iter()
        .filter(|t| t.rows.is_some_and(|r| r > 1_000_000))
        .flat_map(|t| {
            t.columns
                .iter()
                .filter(|c| c.expression.is_some())
                .map(|c| column_ref(&t.name, &c.name))
        })
        .collect();
    if !calc_big.is_empty() {
        out.push(
            HealthCheck::new("calculated_columns_large", MODEL, "Columnas calculadas en tablas de más de un millón de filas", Severity::Info)
                .detail("Se calculan en cada actualización y no se comprimen tan bien como las importadas: calcularlas en el origen o en Power Query las hace más baratas.")
                .objects(calc_big),
        );
    }
    if effective_identity_required == Some(true) {
        let empty: Vec<String> = meta
            .roles
            .iter()
            .filter(|r| r.members.is_empty())
            .map(|r| r.name.clone())
            .collect();
        if !empty.is_empty() {
            out.push(
                HealthCheck::new("rls_no_members", SECURITY, "Roles de seguridad sin miembros", Severity::Warning)
                    .detail("El modelo exige una identidad efectiva (seguridad por filas): quien no esté en ningún rol no ve datos. Agregá los miembros en Power BI (Seguridad del modelo).")
                    .objects(empty),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Method;
    use crate::meta::fake::Fake;
    use serde_json::Value;

    fn sev(c: &[HealthCheck], id: &str) -> Option<Severity> {
        c.iter().find(|x| x.id == id).map(|x| x.severity)
    }

    #[tokio::test]
    async fn checks_from_refreshes_structure_and_memory() {
        let mut f = Fake::writer();
        f.definition(crate::definition::tmsl::tests::SAMPLE);
        let m = f.model.clone().unwrap();
        f.model.as_mut().unwrap().effective_identity_required = Some(true);
        f.route(
            Method::Get,
            &format!("{}/refreshes?$top=10", m.path()),
            200,
            serde_json::from_str(crate::refresh::tests::HISTORY_FAILED).unwrap(),
        );
        f.route(
            Method::Get,
            &format!("{}/refreshSchedule", m.path()),
            200,
            serde_json::json!({"days": [], "times": [], "enabled": false, "localTimeZoneId": "UTC", "notifyOption": "NoNotification"}),
        );
        let now = DateTime::parse_from_rfc3339("2026-11-14T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let c = checks_at(&mut f, "Ventas", now).await.unwrap();
        assert_eq!(sev(&c, "refresh_failed"), Some(Severity::Critical));
        let failed = c.iter().find(|x| x.id == "refresh_failed").unwrap();
        assert!(failed
            .detail
            .contains("ModelRefreshFailed_CredentialsNotSpecified"));
        assert_eq!(sev(&c, "refresh_stale"), Some(Severity::Warning));
        assert_eq!(sev(&c, "schedule_disabled"), Some(Severity::Info));
        assert_eq!(sev(&c, "bidirectional"), Some(Severity::Warning));
        assert_eq!(sev(&c, "inactive_relationships"), Some(Severity::Info));
        assert_eq!(sev(&c, "auto_date_time"), Some(Severity::Warning));
        assert!(sev(&c, "rls_no_members").is_none(), "West has a member");
        // No fix scripts: advice only.
        assert!(c.iter().all(|x| x.fix.is_none()));
        // Memory isn't readable here: said once.
        assert!(c
            .iter()
            .find(|x| x.id == "not_checked")
            .unwrap()
            .detail
            .contains("memoria"));
        // S6: nothing from the M sources or the data reaches the text.
        let all = format!("{c:?}");
        for leak in ["sql.contoso.com", "Seattle", "i45W", "abc123"] {
            assert!(!all.contains(leak), "{leak}");
        }
    }

    #[tokio::test]
    async fn memory_checks_on_capacity() {
        let mut f = Fake::writer();
        f.arrow = true;
        crate::monitor::tests::storage_answers(&mut f);
        let p = f.model.clone().unwrap();
        f.route(
            Method::Get,
            &format!("{}/refreshes?$top=10", p.path()),
            200,
            Value::Null,
        );
        let c = checks_in(&mut f, "Ventas").await.unwrap();
        let big = c.iter().find(|x| x.id == "large_columns").unwrap();
        assert_eq!(big.objects, ["'Sales'[Comment] (98 %)"]);
    }

    #[test]
    fn only_known_error_states_are_errors() {
        for bad in [
            "SemanticError",
            "semanticerror",
            "EvaluationError",
            "DependencyError",
            "SyntaxError",
            "Error",
            "INVALID",
        ] {
            assert!(is_error_state(bad), "{bad}");
        }
        for fine in [
            "Ready",
            "Valid",
            "valid",
            "CalculationNeeded",
            "Incomplete",
            "",
        ] {
            assert!(!is_error_state(fine), "{fine}");
        }
    }

    #[tokio::test]
    async fn a_viewer_gets_what_can_be_read() {
        let mut f = Fake::new();
        let c = checks_in(&mut f, "Ventas").await.unwrap();
        let n = c.iter().find(|x| x.id == "not_checked").unwrap();
        assert!(n.detail.contains("actualizaciones") && n.detail.contains("estructura"));
    }
}
