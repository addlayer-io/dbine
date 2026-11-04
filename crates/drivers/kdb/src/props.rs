//! "Propiedades" of a kdb+ process ([`dbine_driver::Session::database_properties`]):
//! the runtime settings kdb+ changes over IPC with a system command, with
//! their current values, and what it runs on and its memory as facts.
//!
//! The settings: garbage collection (`\g`), the query timeout (`\T`), the
//! display precision (`\P`), the offset from UTC (`\o`), how dates written
//! as text are read (`\z`) and, when the process was started with `-w`, its
//! memory limit (`\w`, which kdb+ only lets go up). Each one is the whole
//! process's and lasts until it restarts. The port (`\p`), the secondary
//! threads (`\s`), `-u`, `-b` or `-E` aren't offered: they close the port
//! DBine is on, need a restart, or only change from the command line.

use crate::process::{bytes, Facts, Setting};
use dbine_driver::{DatabaseProperties, Error, Field, FieldKind, PropertyInfo, Result};
use std::collections::BTreeMap;

const MEMORY: &str = "Memoria";
const MB: f64 = 1024.0 * 1024.0;

/// A setting: its system command and the integers it takes.
struct Command {
    key: &'static str,
    min: i64,
    max: i64,
}

const COMMANDS: &[Command] = &[
    Command {
        key: "g",
        min: 0,
        max: 1,
    },
    Command {
        key: "T",
        min: 0,
        max: i32::MAX as i64,
    },
    Command {
        key: "P",
        min: 0,
        max: 17,
    },
    Command {
        key: "o",
        min: -23,
        max: 23,
    },
    Command {
        key: "z",
        min: 0,
        max: 1,
    },
    Command {
        key: "w",
        min: 1,
        max: i32::MAX as i64,
    },
];

/// The system commands (without their `\`) that apply `changes`, in
/// order; values are checked here, never passed as they came.
pub fn commands(changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    changes
        .iter()
        .map(|(key, value)| {
            let c = COMMANDS
                .iter()
                .find(|c| c.key == key)
                .ok_or_else(|| Error::Query(format!("propiedad desconocida: {key}")))?;
            let v = value.trim();
            // `\o 0N`: the OS's offset.
            if c.key == "o" && (v.is_empty() || v.eq_ignore_ascii_case("0N")) {
                return Ok("o 0N".to_string());
            }
            let n = v
                .parse::<i64>()
                .ok()
                .filter(|n| (c.min..=c.max).contains(n))
                .ok_or_else(|| {
                    Error::Query(format!(
                        "\\{}: «{v}» no es un número entero entre {} y {}",
                        c.key, c.min, c.max
                    ))
                })?;
            Ok(format!("{} {n}", c.key))
        })
        .collect()
}

/// What "Ver script" shows: one system command per line.
pub fn script(changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(commands(changes)?
        .iter()
        .map(|c| format!("\\{c}"))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// The `-w` limit `\w n` should have left, in bytes.
pub fn workspace_bytes(cmd: &str) -> Option<f64> {
    let n: f64 = cmd.strip_prefix("w ")?.parse().ok()?;
    Some(n * MB)
}

fn fact(group: &str, label: &str, value: impl Into<String>) -> PropertyInfo {
    PropertyInfo {
        group: group.into(),
        label: label.into(),
        value: value.into(),
    }
}

fn number(s: Setting) -> String {
    s.value().map(|n| n.to_string()).unwrap_or_default()
}

pub fn properties(f: &Facts) -> DatabaseProperties {
    let mut fields = Vec::new();
    let mut values = BTreeMap::new();
    let mut warnings = BTreeMap::new();
    let mut offer = |key: &str, field: Field, value: Option<String>, warning: &str| {
        if let Some(v) = value {
            values.insert(key.to_string(), v);
            fields.push(field);
            warnings.insert(key.to_string(), warning.to_string());
        }
    };
    offer(
        "g",
        Field::new(
            "g",
            "Recolección de memoria (\\g)",
            FieldKind::Select(vec![("0", "Diferida (0)"), ("1", "Inmediata (1)")]),
        ),
        f.gc.value().map(|n| n.to_string()),
        "Vale para todo el proceso hasta que se reinicie: con 1, la memoria liberada vuelve al sistema enseguida, a costa de algo de CPU.",
    );
    offer(
        "T",
        Field::new(
            "T",
            "Tiempo máximo de una consulta, en segundos (\\T)",
            FieldKind::Number,
        )
        .help("0: sin límite."),
        f.timeout.value().map(|n| n.to_string()),
        "Vale para todas las conexiones del proceso hasta que se reinicie: una consulta que dure más se corta con 'stop.",
    );
    offer(
        "P",
        Field::new("P", "Precisión al mostrar números (\\P)", FieldKind::Number)
            .help("Dígitos significativos, de 0 a 17 (0: todos)."),
        f.precision.value().map(|n| n.to_string()),
        "Cambia cómo convierte números a texto (string, la consola) todo el proceso, hasta que se reinicie.",
    );
    offer(
        "o",
        Field::new("o", "Diferencia con UTC, en horas (\\o)", FieldKind::Text)
            .help("De -23 a 23; vacío: la del sistema operativo."),
        f.offset.text(),
        "Cambia la hora local (.z.P, .z.T, .z.D) de todo el proceso, hasta que se reinicie.",
    );
    offer(
        "z",
        Field::new(
            "z",
            "Fechas escritas como texto (\\z)",
            FieldKind::Select(vec![("0", "mm/dd/aaaa (0)"), ("1", "dd/mm/aaaa (1)")]),
        ),
        f.date_format.value().map(|n| n.to_string()),
        "Cambia cómo leen fechas en texto (\"D\"$) todas las consultas del proceso, hasta que se reinicie.",
    );
    // `\w` changes only a limit set with -w, and only upwards.
    let limit = f.memory.wmax.filter(|w| *w > 0.0);
    offer(
        "w",
        Field::new("w", "Límite de memoria, en MB (\\w)", FieldKind::Number)
            .help("Solo se puede subir: el proceso lo fijó con -w al iniciar."),
        limit.map(|w| ((w / MB).round() as i64).to_string()),
        "Después no se puede bajar: kdb+ solo deja subir el límite. Dura hasta que el proceso se reinicie.",
    );

    let mut info = vec![
        fact("", "Versión", f.release.clone()),
        fact("", "Servidor", f.host.clone()),
        fact("", "Sistema", f.os.clone()),
        fact(
            "",
            "Proceso",
            f.pid.map(|p| p.to_string()).unwrap_or_default(),
        ),
        fact(
            "",
            "Núcleos",
            f.cores.map(|c| c.to_string()).unwrap_or_default(),
        ),
        fact("", "Hilos secundarios", number(f.threads)),
        fact("", "Puerto", number(f.port)),
    ];
    if let Some(line) = &f.command_line {
        info.push(fact("", "Línea de comandos", line.join(" ")));
    }
    let m = &f.memory;
    for (label, v) in [
        ("Memoria en uso", m.used),
        ("Memoria reservada", m.heap),
        ("Pico de memoria", m.peak),
        ("Memoria mapeada", m.mmap),
        ("Memoria física", m.mphy),
    ] {
        if let Some(v) = v {
            info.push(fact(MEMORY, label, bytes(v)));
        }
    }
    if m.wmax.is_some() {
        info.push(fact(
            MEMORY,
            "Límite de memoria (-w)",
            limit.map(bytes).unwrap_or_else(|| "sin límite".into()),
        ));
    }
    if let Some(s) = m.syms {
        info.push(fact(MEMORY, "Símbolos internados", (s as u64).to_string()));
    }
    if let Some(w) = m.symw {
        info.push(fact(MEMORY, "Memoria de los símbolos", bytes(w)));
    }
    info.retain(|i| !i.value.is_empty());
    DatabaseProperties {
        fields,
        values,
        info,
        choices: Vec::new(),
        warnings,
    }
}

impl crate::KdbSession {
    /// Run the system commands of `changes` one by one; a limit `\w`
    /// didn't raise (kdb+ only lets it go up) is an error.
    pub(crate) async fn alter_settings(
        &mut self,
        changes: &BTreeMap<String, String>,
    ) -> Result<()> {
        if self.read_only {
            return Err(Error::Query(
                "Conexión de solo lectura: no se pueden modificar las propiedades de una base."
                    .into(),
            ));
        }
        let cmds = commands(changes)?;
        self.cancel
            .flag
            .store(false, std::sync::atomic::Ordering::SeqCst);
        for (i, cmd) in cmds.iter().enumerate() {
            let r = match self.q(&format!("system {}", crate::edit::qstr(cmd))).await {
                Ok(_) => match workspace_bytes(cmd) {
                    Some(want) => {
                        let now = crate::process::parse(self.q(crate::process::FACTS).await?)
                            .memory
                            .wmax
                            .unwrap_or(0.0);
                        if (now - want).abs() < 1.0 {
                            Ok(())
                        } else {
                            Err(Error::Query(format!(
                                "kdb+ no cambió el límite de memoria, que sigue en {}: con -w solo se puede subir.",
                                bytes(now)
                            )))
                        }
                    }
                    None => Ok(()),
                },
                Err(e) => Err(e),
            };
            if let Err(e) = r {
                return Err(if i == 0 {
                    e
                } else {
                    Error::Query(format!(
                        "se aplicaron {i} de {} cambios; falló: \\{cmd}\n{e}",
                        cmds.len()
                    ))
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::K;
    use crate::process::{parse, tests::sample};

    fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn changes_are_system_commands() {
        assert_eq!(
            script(&c(&[
                ("g", "1"),
                ("T", " 60 "),
                ("P", "10"),
                ("o", "-3"),
                ("z", "1"),
                ("w", "2048")
            ]))
            .unwrap(),
            "\\P 10\n\\T 60\n\\g 1\n\\o -3\n\\w 2048\n\\z 1"
        );
        assert_eq!(script(&c(&[("o", "")])).unwrap(), "\\o 0N");
        assert_eq!(script(&c(&[])).unwrap(), "");
        assert_eq!(workspace_bytes("w 2"), Some(2.0 * MB));
        assert_eq!(workspace_bytes("g 1"), None);
    }

    #[test]
    fn values_are_checked_not_passed() {
        for (k, v) in [
            ("g", "2"),
            ("P", "18"),
            ("T", "-1"),
            ("o", "24"),
            ("z", "1;exit 0"),
            ("T", "5 6"),
            ("w", "0"),
            ("P", "1e3"),
        ] {
            let e = script(&c(&[(k, v)])).unwrap_err().to_string();
            assert!(e.contains("no es un número entero entre"), "{k} {v}: {e}");
        }
        let e = script(&c(&[("p", "0")])).unwrap_err().to_string();
        assert!(e.contains("propiedad desconocida"), "{e}");
    }

    #[test]
    fn settings_facts_and_the_limit_only_with_w() {
        let settings = || {
            [
                K::Int(0),
                K::Int(0),
                K::Int(7),
                K::Int(i32::MIN),
                K::Int(1),
                K::Int(4),
                K::Int(5000),
                K::Symbol(String::new()),
            ]
        };
        let mb = 1i64 << 20;
        let f = parse(sample(
            [100, 200, 300, 0, 0, 8 << 30, 10, 400],
            settings(),
            &["q", "-p", "5000"],
            false,
        ));
        let p = properties(&f);
        let keys: Vec<&str> = p.fields.iter().map(|f| f.key).collect();
        assert_eq!(keys, ["g", "T", "P", "o", "z"]);
        assert_eq!(p.values["o"], "");
        assert_eq!(p.values["P"], "7");
        assert_eq!(p.values["z"], "1");
        assert!(p.warnings.contains_key("T"));
        let get = |l: &str| {
            p.info
                .iter()
                .find(|i| i.label == l)
                .map(|i| i.value.clone())
        };
        assert_eq!(get("Hilos secundarios").as_deref(), Some("4"));
        assert_eq!(get("Límite de memoria (-w)").as_deref(), Some("sin límite"));
        assert_eq!(get("Línea de comandos").as_deref(), Some("q -p 5000"));
        assert_eq!(get("Memoria física").as_deref(), Some("8.0 GB"));
        // Started with -w: the limit can go up.
        let f = parse(sample(
            [100, 200, 300, 1000 * mb, 0, 0, 10, 400],
            settings(),
            &["q", "-w", "1000"],
            false,
        ));
        let p = properties(&f);
        assert_eq!(p.values["w"], "1000");
        assert!(p.fields.iter().any(|f| f.key == "w"));
        // What the server didn't answer isn't offered.
        let f = parse(sample(
            [0; 8],
            std::array::from_fn(|_| K::Symbol(String::new())),
            &[],
            false,
        ));
        assert!(properties(&f).fields.is_empty());
    }
}
