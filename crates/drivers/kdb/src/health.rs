//! "Chequeo de salud" findings of a kdb+ process
//! ([`dbine_driver::Session::health_checks`]), from what it says about
//! itself ([`crate::process::FACTS`]): its memory against the `-w` limit
//! (or the machine's, without one), memory kept but unused, the symbols it
//! interned (kdb+ never frees them), open handles with a large queue (slow
//! subscribers), no query timeout, whether its port asks for a password and
//! takes TLS, and whether it has `reval`.

use crate::process::{bytes, Facts, Setting};
use dbine_driver::health::{HealthCheck, Severity};

const MEMORY: &str = "Memoria";
const CONFIG: &str = "Configuración";
const SECURITY: &str = "Seguridad";
const CONNECTIONS: &str = "Conexiones";

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Bytes queued to one handle from which it's a slow client.
const SLOW_QUEUE: u64 = 64 << 20;

/// Symbols from which the table's growth is worth a look, and a problem.
const MANY_SYMS: f64 = 10_000_000.0;
const TOO_MANY_SYMS: f64 = 50_000_000.0;

/// Memory kept but unused (deferred collection) worth giving back.
const IDLE_HEAP: f64 = GB;

fn share_severity(share: f64, warning: f64, critical: f64) -> Severity {
    if share >= critical {
        Severity::Critical
    } else if share >= warning {
        Severity::Warning
    } else {
        Severity::Ok
    }
}

fn percent(share: f64) -> String {
    format!("{:.0} %", share * 100.0)
}

/// The findings; `reval`: the server has it.
pub fn checks(f: &Facts, reval: bool) -> Vec<HealthCheck> {
    let mut out = Vec::new();
    let m = &f.memory;

    // Memory against the workspace limit, or the machine's without one.
    if let Some(heap) = m.heap {
        match m.wmax.filter(|w| *w > 0.0) {
            Some(limit) => {
                let share = heap / limit;
                out.push(
                    HealthCheck::new(
                        "workspace_limit",
                        MEMORY,
                        format!(
                            "Memoria reservada: {} de {} del límite -w ({})",
                            bytes(heap),
                            bytes(limit),
                            percent(share)
                        ),
                        share_severity(share, 0.75, 0.9),
                    )
                    .detail("Un pedido que necesite más memoria que el límite falla con 'wsfull. Liberá memoria (.Q.gc[] devuelve la que no se usa; borrá tablas o variables que ya no hacen falta) o subí el límite con \\w, que solo sube."),
                );
            }
            None => {
                let share = m.mphy.filter(|p| *p > 0.0).map(|p| heap / p);
                let sev = match share {
                    Some(s) if s >= 0.5 => share_severity(s, 0.5, 0.8),
                    _ => Severity::Info,
                };
                let title = match (share, m.mphy) {
                    (Some(s), Some(p)) => format!(
                        "Sin límite de memoria (-w): {} reservados de {} del equipo ({})",
                        bytes(heap),
                        bytes(p),
                        percent(s)
                    ),
                    _ => "Sin límite de memoria (-w)".to_string(),
                };
                out.push(
                    HealthCheck::new("workspace_limit", MEMORY, title, sev)
                        .detail("Sin -w el proceso crece hasta agotar la memoria del equipo y el sistema puede terminarlo. Con -w al iniciarlo, un pedido que se pase del límite falla con 'wsfull y el proceso sigue."),
                );
            }
        }
    }

    // Memory kept but unused: deferred collection gives it back only on
    // `.Q.gc[]` (or when the limit is reached).
    if let (Some(heap), Some(used), Setting::Value(g)) = (m.heap, m.used, f.gc) {
        let idle = (heap - used).max(0.0);
        let mut check = if g == 0 && idle >= IDLE_HEAP && idle >= heap / 2.0 {
            HealthCheck::new(
                "idle_heap",
                MEMORY,
                format!(
                    "{} reservados sin usar (recolección diferida, \\g 0)",
                    bytes(idle)
                ),
                Severity::Info,
            )
            .fix(".Q.gc[]")
        } else {
            HealthCheck::new(
                "idle_heap",
                MEMORY,
                if g == 0 {
                    "Recolección de memoria diferida (\\g 0)"
                } else {
                    "Recolección de memoria inmediata (\\g 1)"
                },
                Severity::Ok,
            )
        };
        check = check.detail("Con \\g 0 (lo predeterminado) kdb+ guarda la memoria que libera para reusarla y la devuelve al sistema solo con .Q.gc[]; con \\g 1 la devuelve en cuanto la libera, a cambio de algo de CPU.");
        out.push(check);
    }

    // Interned symbols are never freed.
    if let Some(syms) = m.syms {
        let sev = if syms >= TOO_MANY_SYMS {
            Severity::Critical
        } else if syms >= MANY_SYMS {
            Severity::Warning
        } else {
            Severity::Ok
        };
        let size = m
            .symw
            .map(|w| format!(" ({})", bytes(w)))
            .unwrap_or_default();
        out.push(
            HealthCheck::new(
                "symbols",
                MEMORY,
                format!("{} símbolos internados{size}", syms as u64),
                sev,
            )
            .detail("kdb+ nunca libera un símbolo: cada valor distinto de una columna symbol queda en memoria hasta que el proceso termina (y en el archivo sym de una base en disco). Las columnas de valores casi únicos (ids, textos libres) van mejor como string o guid."),
        );
    }

    // A slow client's queue grows in the process's memory.
    if !f.handles.is_empty() {
        let slow: Vec<String> = f
            .handles
            .iter()
            .filter(|(_, q)| *q >= SLOW_QUEUE)
            .map(|(h, q)| format!("handle {h} · {} en cola", bytes(*q as f64)))
            .collect();
        let check = if slow.is_empty() {
            HealthCheck::new(
                "slow_handles",
                CONNECTIONS,
                format!(
                    "{} conexiones abiertas, ninguna con mensajes acumulados",
                    f.handles.len()
                ),
                Severity::Ok,
            )
        } else {
            HealthCheck::new(
                "slow_handles",
                CONNECTIONS,
                format!(
                    "{} conexiones con más de {} en cola",
                    slow.len(),
                    bytes(SLOW_QUEUE as f64)
                ),
                Severity::Warning,
            )
            .objects(slow)
        };
        out.push(check.detail("Lo que el proceso manda a un cliente que no lee (un suscriptor lento) se acumula en su memoria hasta que lo lee: puede crecer hasta 'wsfull. Revisá ese cliente; cerrar su conexión (desde el Monitor) libera la cola."));
    }

    // One request at a time: a query that never ends holds everyone.
    if let Setting::Value(t) = f.timeout {
        let check = if t == 0 {
            HealthCheck::new(
                "query_timeout",
                CONFIG,
                "Sin tiempo máximo de consulta (\\T 0)",
                Severity::Warning,
            )
            .fix("\\T 60")
        } else {
            HealthCheck::new(
                "query_timeout",
                CONFIG,
                format!("Tiempo máximo de consulta: {t} s (\\T)"),
                Severity::Ok,
            )
        };
        out.push(check.detail("kdb+ atiende un pedido por vez: una consulta que no termina deja esperando a todos los clientes. Con \\T n (o -T al iniciar) se corta a los n segundos con 'stop."));
    }

    // Who can reach the port runs any q, `system` included.
    let auth = if f.password_handler {
        Some(HealthCheck::new(
            "authentication",
            SECURITY,
            "El proceso verifica las contraseñas (.z.pw)",
            Severity::Ok,
        ))
    } else {
        match (f.has_flag("-u"), f.has_flag("-U")) {
            (Some(true), _) => Some(HealthCheck::new(
                "authentication",
                SECURITY,
                "Usuarios y contraseñas de un archivo (-u)",
                Severity::Ok,
            )),
            (_, Some(true)) => Some(HealthCheck::new(
                "authentication",
                SECURITY,
                "Usuarios y contraseñas de un archivo (-U), sin límite de acceso a archivos",
                Severity::Info,
            )),
            (Some(false), Some(false)) => Some(HealthCheck::new(
                "authentication",
                SECURITY,
                "El puerto acepta conexiones sin contraseña",
                Severity::Critical,
            )),
            _ => None,
        }
    };
    if let Some(a) = auth {
        out.push(a.detail("Sin -u, -U ni .z.pw, cualquiera que llegue al puerto ejecuta cualquier q, comandos del sistema operativo incluidos (system). -u además limita los archivos a los que llega un cliente; -U no. Limitá también quién llega al puerto."));
    }

    if let Setting::Value(e) = f.tls {
        let (title, sev) = match e {
            2 => ("Solo acepta conexiones con TLS (-E 2)", Severity::Ok),
            1 => ("Acepta conexiones con y sin TLS (-E 1)", Severity::Info),
            _ => ("No acepta TLS (-E 0)", Severity::Info),
        };
        out.push(
            HealthCheck::new("tls", SECURITY, title, sev)
                .detail("Sin TLS, usuarios, contraseñas y datos viajan sin cifrar entre el cliente y el proceso."),
        );
    }

    if !reval {
        out.push(
            HealthCheck::new(
                "reval",
                SECURITY,
                "Sin reval (kdb+ anterior a 3.6)",
                Severity::Warning,
            )
            .detail("Sin reval, el servidor no puede asegurar que una consulta solo lea: las conexiones de solo lectura de DBine, MCP y el asistente solo abren tablas y variables. kdb+ 3.6 o posterior tiene reval."),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::K;
    use crate::process::{parse, tests::sample};

    fn find<'a>(c: &'a [HealthCheck], id: &str) -> &'a HealthCheck {
        c.iter()
            .find(|x| x.id == id)
            .unwrap_or_else(|| panic!("no {id} in {c:?}"))
    }

    fn settings(g: i32, t: i32, e: i32) -> [K; 8] {
        [
            K::Int(g),
            K::Int(t),
            K::Int(7),
            K::Int(i32::MIN),
            K::Int(0),
            K::Int(0),
            K::Int(5000),
            K::Int(e),
        ]
    }

    #[test]
    fn an_open_process_without_limits() {
        let g = 1i64 << 30;
        // 6 GB reserved of 8 GB physical, 1 GB used, no -w, no password.
        let f = parse(sample(
            [g, 6 * g, 6 * g, 0, 0, 8 * g, 60_000_000, 3 * g],
            settings(0, 0, 0),
            &["q", "-p", "5000"],
            false,
        ));
        let c = checks(&f, true);
        let w = find(&c, "workspace_limit");
        assert_eq!(w.severity, Severity::Warning);
        assert!(
            w.title.contains("6.0 GB") && w.title.contains("75 %"),
            "{}",
            w.title
        );
        let idle = find(&c, "idle_heap");
        assert_eq!(idle.fix.as_deref(), Some(".Q.gc[]"));
        assert!(idle.title.starts_with("5.0 GB reservados sin usar"));
        assert_eq!(find(&c, "symbols").severity, Severity::Critical);
        let slow = find(&c, "slow_handles");
        assert_eq!(slow.severity, Severity::Warning);
        assert_eq!(slow.objects, ["handle 9 · 200.0 MB en cola"]);
        let t = find(&c, "query_timeout");
        assert_eq!(
            (t.severity, t.fix.as_deref()),
            (Severity::Warning, Some("\\T 60"))
        );
        assert_eq!(find(&c, "authentication").severity, Severity::Critical);
        assert_eq!(find(&c, "tls").severity, Severity::Info);
        assert!(c.iter().all(|x| x.id != "reval"));
    }

    #[test]
    fn a_guarded_process() {
        let mb = 1i64 << 20;
        // 900 MB reserved of a 1000 MB limit.
        let f = parse(sample(
            [800 * mb, 900 * mb, 900 * mb, 1000 * mb, 0, 0, 5000, 1000],
            settings(1, 30, 2),
            &["q", "-u", "/u.txt", "-w", "1000"],
            false,
        ));
        let c = checks(&f, false);
        assert_eq!(find(&c, "workspace_limit").severity, Severity::Critical);
        let idle = find(&c, "idle_heap");
        assert_eq!((idle.severity, idle.fix.as_ref()), (Severity::Ok, None));
        assert_eq!(find(&c, "symbols").severity, Severity::Ok);
        assert_eq!(find(&c, "query_timeout").severity, Severity::Ok);
        assert_eq!(find(&c, "authentication").severity, Severity::Ok);
        assert_eq!(find(&c, "tls").severity, Severity::Ok);
        assert_eq!(find(&c, "reval").severity, Severity::Warning);
        // .z.pw counts as a password check, and -U is said.
        let pw = parse(sample([0; 8], settings(0, 0, 0), &["q"], true));
        assert_eq!(
            find(&checks(&pw, true), "authentication").severity,
            Severity::Ok
        );
        let big_u = parse(sample([0; 8], settings(0, 0, 0), &["q", "-U", "f"], false));
        assert_eq!(
            find(&checks(&big_u, true), "authentication").severity,
            Severity::Info
        );
        // Without .z.X it can't be told.
        let old = parse(sample([0; 8], settings(0, 0, 0), &[], false));
        assert!(checks(&old, true).iter().all(|x| x.id != "authentication"));
    }
}
