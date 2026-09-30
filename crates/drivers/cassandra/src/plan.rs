//! Cassandra / ScyllaDB have no optimizer plans. What matters is how a
//! statement reaches its data, which follows from the WHERE clause and the
//! table's keys:
//! - estimated: the access path read from the statement and the schema
//!   (partition key, clustering columns, secondary indexes);
//! - actual: the server's trace of the run (`system_traces`), one node per
//!   replica with its events and elapsed times.

use dbine_driver::PlanNode;

// ------------------------------------------------------------ estimated

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Quoted(String),
    Str,
    Num,
    Sym(String),
}

fn tokens(cql: &str) -> Vec<Tok> {
    let c: Vec<char> = cql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch == '\'' {
            i += 1;
            while i < c.len() {
                if c[i] == '\'' {
                    if c.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            out.push(Tok::Str);
        } else if ch == '"' {
            let mut s = String::new();
            i += 1;
            while i < c.len() {
                if c[i] == '"' {
                    if c.get(i + 1) == Some(&'"') {
                        s.push('"');
                        i += 2;
                        continue;
                    }
                    break;
                }
                s.push(c[i]);
                i += 1;
            }
            i += 1;
            out.push(Tok::Quoted(s));
        } else if ch == '$' && c.get(i + 1) == Some(&'$') {
            i += 2;
            while i + 1 < c.len() && !(c[i] == '$' && c[i + 1] == '$') {
                i += 1;
            }
            i += 2;
            out.push(Tok::Str);
        } else if ch.is_ascii_alphabetic() || ch == '_' {
            let start = i;
            while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '_') {
                i += 1;
            }
            out.push(Tok::Ident(c[start..i].iter().collect::<String>().to_ascii_lowercase()));
        } else if ch.is_ascii_digit() || (ch == '-' && c.get(i + 1).is_some_and(char::is_ascii_digit)) {
            i += 1;
            while i < c.len() && (c[i].is_ascii_alphanumeric() || matches!(c[i], '.' | '-' | ':')) {
                i += 1;
            }
            out.push(Tok::Num);
        } else if matches!(ch, '<' | '>' | '!') && c.get(i + 1) == Some(&'=') {
            out.push(Tok::Sym(format!("{ch}=")));
            i += 2;
        } else {
            out.push(Tok::Sym(ch.to_string()));
            i += 1;
        }
    }
    out
}

fn is_kw(t: Option<&Tok>, kw: &str) -> bool {
    matches!(t, Some(Tok::Ident(w)) if w == kw)
}

fn name(t: &Tok) -> Option<String> {
    match t {
        Tok::Ident(s) | Tok::Quoted(s) => Some(s.clone()),
        _ => None,
    }
}

/// A restriction of the WHERE clause.
#[derive(Debug, Clone, PartialEq)]
pub struct Cond {
    pub cols: Vec<String>,
    /// `=`, `in`, `<`, `contains`, `token`…
    pub op: String,
}

/// What a statement touches, as far as the access path goes.
#[derive(Debug, Default, PartialEq)]
pub struct Shape {
    /// `select`, `update`, `delete`, `insert`, or another first keyword.
    pub kind: String,
    pub keyspace: Option<String>,
    pub table: Option<String>,
    pub conds: Vec<Cond>,
    pub allow_filtering: bool,
    pub limit: bool,
}

pub fn shape(cql: &str) -> Shape {
    let t = tokens(cql);
    let mut s = Shape { kind: match t.first() { Some(Tok::Ident(w)) => w.clone(), _ => String::new() }, ..Default::default() };
    // The table: after FROM (SELECT, DELETE), UPDATE, INSERT INTO.
    let table_at = match s.kind.as_str() {
        "select" | "delete" => {
            let mut depth = 0i32;
            t.iter().position(|x| {
                match x {
                    Tok::Sym(p) if p == "(" => depth += 1,
                    Tok::Sym(p) if p == ")" => depth -= 1,
                    _ => {}
                }
                depth == 0 && matches!(x, Tok::Ident(w) if w == "from")
            })
            .map(|p| p + 1)
        }
        "update" => Some(1),
        "insert" => is_kw(t.get(1), "into").then_some(2),
        _ => None,
    };
    let mut i = match table_at {
        Some(p) => p,
        None => return s,
    };
    if let Some(first) = t.get(i).and_then(name) {
        if matches!(t.get(i + 1), Some(Tok::Sym(p)) if p == ".") {
            s.keyspace = Some(first);
            s.table = t.get(i + 2).and_then(name);
            i += 3;
        } else {
            s.table = Some(first);
            i += 1;
        }
    }
    let Some(w) = t[i.min(t.len())..].iter().position(|x| matches!(x, Tok::Ident(k) if k == "where")) else {
        s.allow_filtering = has_allow_filtering(&t[i.min(t.len())..]);
        s.limit = t.iter().any(|x| matches!(x, Tok::Ident(k) if k == "limit"));
        return s;
    };
    let rest = &t[i + w + 1..];
    // The WHERE clause runs until the next clause at depth 0.
    let mut depth = 0i32;
    let mut end = rest.len();
    for (k, x) in rest.iter().enumerate() {
        match x {
            Tok::Sym(p) if p == "(" => depth += 1,
            Tok::Sym(p) if p == ")" => depth -= 1,
            Tok::Ident(kw) if depth == 0 && matches!(kw.as_str(), "order" | "limit" | "allow" | "group" | "per" | "if" | "bypass") => {
                end = k;
                break;
            }
            _ => {}
        }
    }
    s.allow_filtering = has_allow_filtering(&rest[end..]);
    s.limit = rest[end..].iter().any(|x| matches!(x, Tok::Ident(k) if k == "limit"));
    // Conditions split on AND at depth 0.
    let mut depth = 0i32;
    let mut cur: Vec<&Tok> = Vec::new();
    let mut groups: Vec<Vec<&Tok>> = Vec::new();
    for x in &rest[..end] {
        match x {
            Tok::Sym(p) if p == "(" => depth += 1,
            Tok::Sym(p) if p == ")" => depth -= 1,
            Tok::Ident(k) if depth == 0 && k == "and" => {
                groups.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(x);
    }
    groups.push(cur);
    for g in groups.into_iter().filter(|g| !g.is_empty()) {
        if let Some(c) = condition(&g) {
            s.conds.push(c);
        }
    }
    s
}

fn has_allow_filtering(t: &[Tok]) -> bool {
    t.windows(2).any(|w| is_kw(Some(&w[0]), "allow") && is_kw(Some(&w[1]), "filtering"))
}

fn condition(g: &[&Tok]) -> Option<Cond> {
    let (cols, rest): (Vec<String>, &[&Tok]) = match g.first()? {
        Tok::Ident(w) if w == "token" && matches!(g.get(1), Some(Tok::Sym(p)) if p == "(") => {
            let close = g.iter().position(|x| matches!(x, Tok::Sym(p) if p == ")"))?;
            let cols = g[2..close].iter().filter_map(|x| name(x)).collect();
            return Some(Cond { cols, op: "token".into() });
        }
        Tok::Sym(p) if p == "(" => {
            let close = g.iter().position(|x| matches!(x, Tok::Sym(p) if p == ")"))?;
            (g[1..close].iter().filter_map(|x| name(x)).collect(), &g[close + 1..])
        }
        first => (vec![name(first)?], &g[1..]),
    };
    let op = match rest.first()? {
        Tok::Sym(p) => p.clone(),
        Tok::Ident(w) if w == "contains" && is_kw(rest.get(1).copied(), "key") => "contains key".into(),
        Tok::Ident(w) => w.clone(),
        _ => return None,
    };
    Some(Cond { cols, op })
}

/// The table's keys and indexed columns.
#[derive(Debug, Default)]
pub struct Keys {
    pub partition: Vec<String>,
    pub clustering: Vec<String>,
    pub indexed: Vec<String>,
}

/// The access path of a statement over a table with these keys.
pub fn access(s: &Shape, keys: Option<&Keys>) -> PlanNode {
    let object = s.table.as_ref().map(|t| match &s.keyspace {
        Some(k) => format!("{k}.{t}"),
        None => t.clone(),
    });
    let mut n = PlanNode { object, ..Default::default() };
    let kind = s.kind.to_uppercase();
    let Some(keys) = keys.filter(|_| matches!(s.kind.as_str(), "select" | "update" | "delete" | "insert")) else {
        n.op = if kind.is_empty() { "Sentencia".into() } else { kind };
        n.detail = "Sin acceso a datos de tablas".into();
        return n;
    };
    n.props.push(("Clave de partición".into(), keys.partition.join(", ")));
    if !keys.clustering.is_empty() {
        n.props.push(("Columnas de clustering".into(), keys.clustering.join(", ")));
    }
    if !keys.indexed.is_empty() {
        n.props.push(("Columnas indexadas".into(), keys.indexed.join(", ")));
    }
    let verb = match s.kind.as_str() {
        "select" => "Lectura",
        "delete" => "Borrado",
        _ => "Escritura",
    };
    if s.kind == "insert" {
        n.op = "Escritura por clave de partición".into();
        n.detail = "Una partición".into();
        return n;
    }
    let restricted = |col: &str, ops: &[&str]| s.conds.iter().any(|c| c.cols.iter().any(|x| x == col) && ops.contains(&c.op.as_str()));
    let pk_eq = !keys.partition.is_empty() && keys.partition.iter().all(|k| restricted(k, &["=", "in"]));
    let pk_in = keys.partition.iter().any(|k| restricted(k, &["in"]));
    let token = s.conds.iter().any(|c| c.op == "token");
    let key_cols: Vec<&String> = keys.partition.iter().chain(&keys.clustering).collect();
    let non_key: Vec<&Cond> = s.conds.iter().filter(|c| c.op != "token" && !c.cols.iter().all(|x| key_cols.contains(&x))).collect();
    let on_index = non_key.iter().any(|c| c.cols.iter().any(|x| keys.indexed.contains(x)));
    let ck_used: Vec<&String> = keys.clustering.iter().filter(|k| s.conds.iter().any(|c| c.cols.contains(k))).collect();
    if pk_eq {
        n.op = format!("{verb} por clave de partición");
        n.detail = if pk_in { "Varias particiones (IN)".into() } else { "Una partición".into() };
        if pk_in {
            n.warnings.push("IN sobre la clave de partición: el coordinador consulta varias particiones".into());
        }
        if !ck_used.is_empty() {
            let range = s.conds.iter().any(|c| c.cols.iter().any(|x| keys.clustering.contains(x)) && !matches!(c.op.as_str(), "=" | "in"));
            n.props.push((
                if range { "Rango de clustering" } else { "Filas por clustering" }.into(),
                ck_used.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
            ));
        } else if s.kind == "delete" {
            n.props.push(("Alcance".into(), "Borra la partición completa".into()));
        }
        if !non_key.is_empty() {
            if on_index {
                n.props.push(("Índice".into(), "Filtra con un índice secundario dentro de la partición".into()));
            } else {
                n.warnings.push("Filtra columnas que no son clave dentro de la partición (ALLOW FILTERING)".into());
            }
        }
    } else if token {
        n.op = format!("{verb} por rango de tokens");
        n.warnings.push("Recorre un rango de particiones".into());
    } else if s.conds.is_empty() {
        n.op = format!("{verb}: recorrido completo de la tabla");
        n.warnings.push("Lee todas las particiones del clúster".into());
        if s.kind == "select" && !s.limit {
            n.warnings.push("Sin LIMIT: puede devolver la tabla entera".into());
        }
    } else if on_index {
        n.op = format!("{verb} por índice secundario");
        n.warnings.push("Índice secundario sin clave de partición: consulta a todos los nodos".into());
    } else if s.allow_filtering {
        n.op = format!("{verb}: recorrido con filtro");
        n.warnings.push("ALLOW FILTERING: recorre todas las particiones".into());
    } else {
        n.op = format!("{verb} sin la clave de partición completa");
        n.warnings.push("El servidor la rechazará: falta la clave de partición completa (usá la clave, un índice o ALLOW FILTERING)".into());
    }
    n
}

// --------------------------------------------------------------- actual

/// One `system_traces.events` row.
#[derive(Debug, Clone, Default)]
pub struct Event {
    pub activity: String,
    pub source: String,
    pub elapsed_us: Option<i32>,
    pub thread: String,
}

/// `system_traces.sessions` plus its events.
#[derive(Debug, Clone, Default)]
pub struct Trace {
    pub request: String,
    pub coordinator: String,
    pub duration_us: Option<i32>,
    pub params: Vec<(String, String)>,
    pub events: Vec<Event>,
}

pub fn trace_tree(t: &Trace) -> PlanNode {
    let mut root = PlanNode {
        op: if t.request.is_empty() { "Consulta".into() } else { t.request.clone() },
        actual_ms: t.duration_us.map(|d| f64::from(d) / 1000.0),
        ..Default::default()
    };
    if !t.coordinator.is_empty() {
        root.props.push(("Coordinador".into(), t.coordinator.clone()));
    }
    root.props.extend(t.params.iter().filter(|(k, _)| k != "query").cloned());
    // One child per node, in order of appearance.
    let mut sources: Vec<String> = Vec::new();
    for e in &t.events {
        if !sources.contains(&e.source) {
            sources.push(e.source.clone());
        }
    }
    for src in sources {
        let mut node = PlanNode {
            op: if src == t.coordinator { format!("Coordinador {src}") } else { format!("Réplica {src}") },
            ..Default::default()
        };
        let mut prev = 0i32;
        for e in t.events.iter().filter(|e| e.source == src) {
            let (activity, shard) = match e.activity.rsplit_once(" [") {
                Some((a, s)) if s.ends_with(']') => (a.to_string(), s.trim_end_matches(']').to_string()),
                _ => (e.activity.clone(), String::new()),
            };
            let el = e.elapsed_us.unwrap_or(prev);
            let mut ev = PlanNode {
                op: activity.clone(),
                detail: if shard.is_empty() { e.thread.clone() } else { shard },
                actual_ms: Some(f64::from((el - prev).max(0)) / 1000.0),
                ..Default::default()
            };
            ev.props.push(("Transcurrido en el nodo (µs)".into(), el.to_string()));
            let lower = activity.to_ascii_lowercase();
            if let Some(n) = number_before(&lower, " live rows") {
                ev.actual_rows = Some(n);
            }
            if number_before(&lower, " tombstone").is_some_and(|n| n > 0.0) {
                ev.warnings.push("Lee lápidas (tombstones)".into());
            }
            if lower.contains("seq scan") || lower.contains("range slice") || lower.contains("scanning") {
                ev.warnings.push("Recorre un rango de particiones".into());
            }
            prev = el;
            node.actual_ms = Some(f64::from(el) / 1000.0);
            node.children.push(ev);
        }
        root.children.push(node);
    }
    root
}

/// `read 3 live rows` → 3 (the number right before `what`).
fn number_before(text: &str, what: &str) -> Option<f64> {
    let i = text.find(what)?;
    text[..i].split_whitespace().last()?.parse().ok()
}

/// The trace as text (the plan's raw form).
pub fn trace_text(t: &Trace) -> String {
    let mut out = format!("{} ({} µs)\n", t.request, t.duration_us.unwrap_or_default());
    for e in &t.events {
        out.push_str(&format!("{:>8} µs  {:<15} {}\n", e.elapsed_us.unwrap_or_default(), e.source, e.activity));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Keys {
        Keys {
            partition: vec!["tenant".into(), "day".into()],
            clustering: vec!["ts".into()],
            indexed: vec!["email".into()],
        }
    }

    #[test]
    fn statements_are_read() {
        let s = shape("SELECT * FROM app.\"Events\" WHERE tenant = 'a' AND day IN (1, 2) AND ts > ? LIMIT 10");
        assert_eq!(s.keyspace.as_deref(), Some("app"));
        assert_eq!(s.table.as_deref(), Some("Events"));
        assert_eq!(s.conds, vec![
            Cond { cols: vec!["tenant".into()], op: "=".into() },
            Cond { cols: vec!["day".into()], op: "in".into() },
            Cond { cols: vec!["ts".into()], op: ">".into() },
        ]);
        assert!(s.limit && !s.allow_filtering);
        let s = shape("select * from t where token(tenant, day) > 5 allow filtering");
        assert_eq!(s.conds[0].op, "token");
        assert!(s.allow_filtering);
        let s = shape("DELETE FROM t WHERE tenant = 'x' AND day = 1 IF EXISTS");
        assert_eq!(s.conds.len(), 2);
        let s = shape("UPDATE ks.t USING TTL 5 SET v = 1 WHERE tenant = 'a' AND day = 2 AND (ts) = (3)");
        assert_eq!((s.keyspace.as_deref(), s.table.as_deref()), (Some("ks"), Some("t")));
        assert_eq!(s.conds[2].cols, ["ts"]);
    }

    #[test]
    fn access_paths() {
        let k = keys();
        let a = access(&shape("SELECT * FROM t WHERE tenant = 'a' AND day = 1 AND ts > 3"), Some(&k));
        assert_eq!(a.op, "Lectura por clave de partición");
        assert!(a.warnings.is_empty());
        assert!(a.props.iter().any(|(k, _)| k == "Rango de clustering"));

        let a = access(&shape("SELECT * FROM t WHERE tenant = 'a' AND day IN (1, 2)"), Some(&k));
        assert_eq!(a.detail, "Varias particiones (IN)");
        assert_eq!(a.warnings.len(), 1);

        let a = access(&shape("SELECT * FROM t"), Some(&k));
        assert_eq!(a.op, "Lectura: recorrido completo de la tabla");
        assert_eq!(a.warnings.len(), 2);

        let a = access(&shape("SELECT * FROM t WHERE email = 'x'"), Some(&k));
        assert_eq!(a.op, "Lectura por índice secundario");

        let a = access(&shape("SELECT * FROM t WHERE v = 1 ALLOW FILTERING"), Some(&k));
        assert_eq!(a.warnings, ["ALLOW FILTERING: recorre todas las particiones"]);

        let a = access(&shape("SELECT * FROM t WHERE tenant = 'a'"), Some(&k));
        assert!(a.op.contains("sin la clave de partición completa"));

        let a = access(&shape("DELETE FROM t WHERE tenant = 'a' AND day = 1"), Some(&k));
        assert_eq!(a.op, "Borrado por clave de partición");
        assert!(a.props.iter().any(|(_, v)| v == "Borra la partición completa"));

        let a = access(&shape("CREATE TABLE x (a int PRIMARY KEY)"), None);
        assert_eq!(a.op, "CREATE");
    }

    #[test]
    fn traces_group_by_node() {
        let ev = |a: &str, s: &str, e: i32| Event { activity: a.into(), source: s.into(), elapsed_us: Some(e), thread: String::new() };
        let t = Trace {
            request: "Execute CQL3 query".into(),
            coordinator: "10.0.0.1".into(),
            duration_us: Some(1500),
            params: vec![("consistency_level".into(), "ONE".into()), ("query".into(), "SELECT".into())],
            events: vec![
                ev("Parsing a statement [shard 0]", "10.0.0.1", 10),
                ev("Sending a read to 10.0.0.2 [shard 0]", "10.0.0.1", 50),
                ev("Read 3 live rows and 2 tombstone cells [shard 1]", "10.0.0.2", 400),
                ev("Done processing - preparing a result [shard 0]", "10.0.0.1", 900),
            ],
        };
        let root = trace_tree(&t);
        assert_eq!(root.actual_ms, Some(1.5));
        assert_eq!(root.props, vec![("Coordinador".to_string(), "10.0.0.1".to_string()), ("consistency_level".to_string(), "ONE".to_string())]);
        assert_eq!(root.children.len(), 2);
        let coord = &root.children[0];
        assert_eq!(coord.op, "Coordinador 10.0.0.1");
        assert_eq!(coord.children[1].actual_ms, Some(0.04));
        assert_eq!(coord.children[0].detail, "shard 0");
        let replica = &root.children[1];
        assert_eq!(replica.children[0].actual_rows, Some(3.0));
        assert_eq!(replica.children[0].warnings, ["Lee lápidas (tombstones)"]);
        assert!(trace_text(&t).contains("Execute CQL3 query (1500 µs)"));
    }
}
