//! What a kdb+ process says about itself, in one round trip: its memory
//! (`.Q.w[]`), the runtime settings the system commands show (`\g`, `\T`,
//! `\P`, `\o`, `\z`, `\s`, `\p`, `\E`), its command line (`.z.X`), whether
//! it checks passwords itself (`.z.pw`), what it runs on and its open
//! handles (`.z.W`). The "Chequeo de salud" ([`crate::health`]) and the
//! "Propiedades" ([`crate::props`]) read it. It only reads: on a
//! read-only connection it runs inside `reval`.

use crate::cells;
use crate::ipc::K;
use serde_json::Value;

/// Each item under protection: what an older kdb+ lacks (`.z.X`, `\E`)
/// comes as the empty symbol.
pub const FACTS: &str = concat!(
    "{(@[.Q.w;::;()!()];",
    "@[system;\"g\";`];@[system;\"T\";`];@[system;\"P\";`];@[system;\"o\";`];@[system;\"z\";`];",
    "@[system;\"s\";`];@[system;\"p\";`];@[system;\"E\";`];",
    "@[{.z.X};::;`];`pw in key `.z;.z.K;.z.k;.z.h;.z.o;.z.i;.z.c;",
    "@[{(key x;sum each value x)};.z.W;`])}[]"
);

/// `.Q.w[]`, in bytes (`syms` is a count).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Memory {
    pub used: Option<f64>,
    pub heap: Option<f64>,
    pub peak: Option<f64>,
    /// The `-w` limit; `0`: none.
    pub wmax: Option<f64>,
    pub mmap: Option<f64>,
    /// The machine's physical memory.
    pub mphy: Option<f64>,
    pub syms: Option<f64>,
    pub symw: Option<f64>,
}

/// A setting a system command shows: `Unknown` when the server didn't
/// answer it (an older kdb+), `Null` for `0N` (`\o`: the OS's offset).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub enum Setting {
    #[default]
    Unknown,
    Null,
    Value(i64),
}

impl Setting {
    pub fn value(self) -> Option<i64> {
        match self {
            Setting::Value(n) => Some(n),
            _ => None,
        }
    }

    /// The value as a property form shows it (`""` for `0N`).
    pub fn text(self) -> Option<String> {
        match self {
            Setting::Value(n) => Some(n.to_string()),
            Setting::Null => Some(String::new()),
            Setting::Unknown => None,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Facts {
    pub memory: Memory,
    /// `\g`: 0 deferred, 1 immediate.
    pub gc: Setting,
    /// `\T`: seconds, 0 none.
    pub timeout: Setting,
    /// `\P`: display precision.
    pub precision: Setting,
    /// `\o`: hours from UTC; null: the OS's.
    pub offset: Setting,
    /// `\z`: 0 mm/dd/yyyy, 1 dd/mm/yyyy.
    pub date_format: Setting,
    /// `\s`: secondary threads.
    pub threads: Setting,
    /// `\p`: the port it listens on.
    pub port: Setting,
    /// `\E`: 0 plain only, 1 plain and TLS, 2 TLS only.
    pub tls: Setting,
    /// `.z.X`, when the server has it.
    pub command_line: Option<Vec<String>>,
    /// `.z.pw` is defined.
    pub password_handler: bool,
    /// `.z.K`.
    pub version: Option<f64>,
    /// `kdb+ 4.1 (2024.10.16)`.
    pub release: String,
    pub host: String,
    pub os: String,
    pub pid: Option<i64>,
    pub cores: Option<i64>,
    /// Each open handle and the bytes queued to it.
    pub handles: Vec<(i64, u64)>,
}

impl Facts {
    /// The command line has `flag` (`-u`, `-U`, `-b`): `None` when the
    /// server doesn't say (no `.z.X`).
    pub fn has_flag(&self, flag: &str) -> Option<bool> {
        self.command_line
            .as_ref()
            .map(|args| args.iter().any(|a| a == flag))
    }
}

fn number(k: &K) -> Option<f64> {
    match k {
        K::Long(n) if *n != i64::MIN => Some(*n as f64),
        K::Int(n) if *n != i32::MIN => Some(*n as f64),
        K::Short(n) if *n != i16::MIN => Some(*n as f64),
        K::Float(f) if f.is_finite() => Some(*f),
        K::Real(f) if f.is_finite() => Some(*f as f64),
        _ => None,
    }
}

fn integer(k: &K) -> Option<i64> {
    match k {
        K::Long(n) if *n != i64::MIN => Some(*n),
        K::Int(n) if *n != i32::MIN => Some(*n as i64),
        K::Short(n) if *n != i16::MIN => Some(*n as i64),
        _ => None,
    }
}

fn setting(k: &K) -> Setting {
    match k {
        K::Int(i32::MIN) | K::Long(i64::MIN) | K::Short(i16::MIN) => Setting::Null,
        k => integer(k).map_or(Setting::Unknown, Setting::Value),
    }
}

fn text(k: &K) -> String {
    match cells::cell(k) {
        Value::String(s) => s,
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

fn memory(w: &K) -> Memory {
    let get = |name: &str| -> Option<f64> {
        let K::Dict(keys, values) = w else {
            return None;
        };
        let i = keys.symbols()?.iter().position(|k| k == name)?;
        number(&values.item(i)?)
    };
    Memory {
        used: get("used"),
        heap: get("heap"),
        peak: get("peak"),
        wmax: get("wmax"),
        mmap: get("mmap"),
        mphy: get("mphy"),
        syms: get("syms"),
        symw: get("symw"),
    }
}

/// [`FACTS`]'s answer.
pub fn parse(k: K) -> Facts {
    let K::List(parts) = k else {
        return Facts::default();
    };
    let get = |i: usize| parts.get(i).cloned().unwrap_or(K::Nil);
    let command_line = match get(9) {
        K::List(items) if !items.is_empty() => {
            Some(items.iter().filter_map(K::text).collect::<Vec<_>>())
        }
        _ => None,
    };
    let handles = match get(17) {
        K::List(hq) if hq.len() == 2 => (0..hq[0].count())
            .filter_map(|i| {
                let h = integer(&hq[0].item(i)?)?;
                let q = number(&hq[1].item(i)?).unwrap_or(0.0).max(0.0) as u64;
                Some((h, q))
            })
            .collect(),
        _ => Vec::new(),
    };
    let version = number(&get(11));
    Facts {
        memory: memory(&get(0)),
        gc: setting(&get(1)),
        timeout: setting(&get(2)),
        precision: setting(&get(3)),
        offset: setting(&get(4)),
        date_format: setting(&get(5)),
        threads: setting(&get(6)),
        port: setting(&get(7)),
        tls: setting(&get(8)),
        command_line,
        password_handler: get(10) == K::Bool(true),
        version,
        release: format!(
            "kdb+ {} ({})",
            version.map(|v| v.to_string()).unwrap_or_default(),
            match get(12) {
                K::Date(d) => cells::date(d),
                _ => String::new(),
            }
        ),
        host: text(&get(13)),
        os: text(&get(14)),
        pid: integer(&get(15)),
        cores: integer(&get(16)),
        handles,
    }
}

/// Bytes in the largest unit that keeps a whole part (`1.5 GB`).
pub fn bytes(n: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut u) = (n, 0);
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} B", n as i64)
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ipc::Vector;

    pub(crate) fn sample(w: [i64; 8], settings: [K; 8], line: &[&str], pw: bool) -> K {
        let mem = K::Dict(
            Box::new(K::Vector(
                0,
                Vector::Symbol(
                    [
                        "used", "heap", "peak", "wmax", "mmap", "mphy", "syms", "symw",
                    ]
                    .map(String::from)
                    .to_vec(),
                ),
            )),
            Box::new(K::Vector(0, Vector::Long(w.to_vec()))),
        );
        let mut parts = vec![mem];
        parts.extend(settings);
        parts.push(if line.is_empty() {
            K::Symbol(String::new())
        } else {
            K::List(
                line.iter()
                    .map(|a| K::Vector(0, Vector::Char(a.as_bytes().to_vec())))
                    .collect(),
            )
        });
        parts.extend([
            K::Bool(pw),
            K::Float(4.1),
            K::Date(9000),
            K::Symbol("host1".into()),
            K::Symbol("l64".into()),
            K::Int(77),
            K::Int(8),
            K::List(vec![
                K::Vector(0, Vector::Int(vec![6, 9])),
                K::Vector(0, Vector::Long(vec![0, 200 << 20])),
            ]),
        ]);
        K::List(parts)
    }

    #[test]
    fn facts_are_read_and_unknowns_said() {
        let k = sample(
            [100, 200, 300, 0, 0, 1000, 42, 4200],
            [
                K::Int(0),
                K::Int(30),
                K::Int(7),
                K::Int(i32::MIN),
                K::Symbol(String::new()),
                K::Int(2),
                K::Int(5000),
                K::Int(1),
            ],
            &["q", "db.q", "-u", "/users.txt"],
            false,
        );
        let f = parse(k);
        assert_eq!(f.memory.used, Some(100.0));
        assert_eq!(f.memory.wmax, Some(0.0));
        assert_eq!(f.memory.syms, Some(42.0));
        assert_eq!(f.gc, Setting::Value(0));
        assert_eq!(f.timeout.value(), Some(30));
        assert_eq!(f.offset, Setting::Null);
        assert_eq!(f.offset.text().as_deref(), Some(""));
        assert_eq!(f.date_format, Setting::Unknown);
        assert_eq!(f.date_format.text(), None);
        assert_eq!(f.has_flag("-u"), Some(true));
        assert_eq!(f.has_flag("-U"), Some(false));
        assert!(!f.password_handler);
        assert!(f.release.starts_with("kdb+ 4.1 ("), "{}", f.release);
        assert_eq!(
            (f.host.as_str(), f.pid, f.cores),
            ("host1", Some(77), Some(8))
        );
        assert_eq!(f.handles, [(6, 0), (9, 200 << 20)]);
        // Without `.z.X` nothing is known about the command line.
        let f = parse(sample(
            [0; 8],
            std::array::from_fn(|_| K::Int(0)),
            &[],
            true,
        ));
        assert_eq!(f.has_flag("-u"), None);
        assert!(f.password_handler);
        assert_eq!(parse(K::Nil), Facts::default());
        assert!(FACTS.starts_with("{(@[.Q.w;::;()!()];") && FACTS.ends_with(")}[]"));
    }

    #[test]
    fn sizes() {
        assert_eq!(bytes(512.0), "512 B");
        assert_eq!(bytes(1536.0), "1.5 KB");
        assert_eq!(bytes(3.0 * 1024.0 * 1024.0 * 1024.0), "3.0 GB");
    }
}
