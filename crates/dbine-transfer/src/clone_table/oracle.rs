//! Oracle: what the reported structure doesn't carry and the clone must
//! have as the original does.
//!
//! - Each constraint's state: `DISABLE`, `ENABLE NOVALIDATE`, `DEFERRABLE
//!   INITIALLY DEFERRED / IMMEDIATE`, `RELY`. A CHECK or foreign key that
//!   came out enabled would reject rows the original keeps (after a wasted
//!   copy), and one that came out validated or not deferrable would behave
//!   differently afterwards. CHECKs in a state other than enabled and
//!   validated are added after the rows, already in it; primary keys are
//!   created in it (disabled while the rows load when not validated);
//!   unique constraints and foreign keys are added in it ([`adjust`]).
//! - The index behind a primary key or unique constraint when it isn't the
//!   one Oracle would make (`USING INDEX ux_…`: another name, not unique,
//!   more columns): created with the constraint, renamed like the others.
//! - A primary key column's NOT NULL: only when the original declares it
//!   (the key alone makes the column NOT NULL without a constraint of its
//!   own; the generated `NOT NULL` would add one).
//!
//! [`verify`] reads the clone's constraints back and compares them with the
//! original's.

use super::{exec, fit, fnv, limit_text, rename_capped, strings, ClonePlan, CloneOptions, Ddl, Rename};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{DdlParts, Driver, Error, Result, Session, TableSchema};

/// A constraint of the table and its state (ALL_CONSTRAINTS).
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Con {
    pub name: String,
    /// `C`, `P`, `U` or `R`.
    pub kind: String,
    pub enabled: bool,
    pub validated: bool,
    pub deferrable: bool,
    pub deferred: bool,
    pub rely: bool,
    /// Named by the server (`SYS_C…`).
    pub generated: bool,
    /// The index behind a primary key / unique constraint.
    pub index: Option<String>,
    pub columns: Vec<String>,
    /// A CHECK's condition.
    pub condition: String,
}

/// The index a primary key / unique constraint uses when it isn't the one
/// Oracle makes on its own: created with the constraint (`USING INDEX`).
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Backing {
    /// The constraint.
    pub constraint: String,
    pub name: String,
    pub unique: bool,
    pub columns: Vec<String>,
}

/// What the catalog says about the original.
#[derive(Debug, Clone, Default)]
pub(super) struct Original {
    pub cons: Vec<Con>,
    pub backing: Vec<Backing>,
    pub physical: Physical,
    /// The LOCAL indexes whose partitions aren't named like the table's:
    /// (index, its partitions' names in order).
    pub local: Vec<(String, Vec<String>)>,
}

/// Each LOCAL index of the table and its partitions' names, with the
/// table partition each one stands for: (index, [(index's, table's)]).
async fn local_partitions(s: &mut dyn Session, schema: Option<&str>, table: &str) -> Result<Vec<(String, Vec<(String, String)>)>> {
    let sql = format!(
        "SELECT i.index_name, ip.partition_name, tp.partition_name FROM all_indexes i \
         JOIN all_part_indexes pi ON pi.owner = i.owner AND pi.index_name = i.index_name \
         JOIN all_ind_partitions ip ON ip.index_owner = i.owner AND ip.index_name = i.index_name \
         JOIN all_tab_partitions tp ON tp.table_owner = i.table_owner AND tp.table_name = i.table_name AND tp.partition_position = ip.partition_position \
         WHERE i.table_owner = {o} AND i.table_name = {tn} AND pi.locality = 'LOCAL' ORDER BY i.index_name, ip.partition_position",
        o = owner(schema),
        tn = lit(table)
    );
    let rows = strings(s, &sql)
        .await
        .map_err(|_| Error::State("no se pudieron leer las particiones de los índices locales de la tabla; no se clona".into()))?;
    let mut out: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for r in rows {
        let v = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
        let (ix, part, tpart) = (v(0), v(1), v(2));
        match out.last_mut() {
            Some((n, parts)) if *n == ix => parts.push((part, tpart)),
            _ => out.push((ix, vec![(part, tpart)])),
        }
    }
    Ok(out)
}

/// A LOCAL index whose partitions have names of their own (not the table
/// partitions' ones, nor the server's).
fn own_partition_names(parts: &[(String, String)]) -> bool {
    parts.iter().any(|(i, t)| i != t && !server_named_partition(i))
}

/// `LOCAL (PARTITION "LP1", PARTITION, …)`: a partition the server named
/// left unnamed (the server names it after the table's).
fn local_clause(parts: &[String]) -> String {
    let parts: Vec<String> = parts
        .iter()
        .map(|n| if server_named_partition(n) { "PARTITION".to_string() } else { format!("PARTITION {}", quote_ident(Quote::Double, n)) })
        .collect();
    format!("LOCAL ({})", parts.join(", "))
}

/// The index's `CREATE … LOCAL [INVISIBLE];` with its partitions' names.
fn with_local_names(sql: &str, parts: &[String]) -> Option<String> {
    let i = sql.rfind(" LOCAL")?;
    let rest = &sql[i + " LOCAL".len()..];
    if !matches!(rest.trim_end(), "" | ";" | " INVISIBLE" | " INVISIBLE;") {
        return None;
    }
    Some(format!("{} {}{rest}", &sql[..i], local_clause(parts)))
}

/// How the table is stored, which the reported structure doesn't carry:
/// index-organized, partitioned, row movement.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Physical {
    /// A global temporary table's `ON COMMIT DELETE | PRESERVE ROWS`.
    pub temporary: Option<String>,
    /// The table's compression (`COMPRESS BASIC`, `ROW STORE COMPRESS
    /// ADVANCED`…; a partitioned table's default).
    pub compression: Option<String>,
    /// An index-organized table's options (`PCTTHRESHOLD n [COMPRESS k]`).
    pub iot: Option<String>,
    /// `PARTITION BY …` (partitions the server named left unnamed).
    pub partitioning: Option<String>,
    pub row_movement: bool,
}

impl Physical {
    /// What goes right after the CREATE's column list, and at its end.
    fn clauses(&self) -> (String, String) {
        let mut head = String::new();
        if let Some(t) = &self.temporary {
            head.push_str(&format!(" {t}"));
        }
        if self.iot.is_some() {
            head.push_str(" ORGANIZATION INDEX");
        }
        let mut tail = String::new();
        for c in [self.compression.as_deref(), self.iot.as_deref(), self.partitioning.as_deref(), self.row_movement.then_some("ENABLE ROW MOVEMENT")].into_iter().flatten() {
            tail.push(' ');
            tail.push_str(c);
        }
        (head, tail)
    }

    /// In Spanish.
    fn describe(&self) -> String {
        let mut out = Vec::new();
        match &self.temporary {
            Some(t) => out.push(format!("temporal global ({t})")),
            None => out.push("permanente".to_string()),
        }
        match &self.compression {
            Some(c) => out.push(format!("comprimida ({c})")),
            None => out.push("sin compresión".to_string()),
        }
        match &self.iot {
            Some(o) => out.push(format!("organizada por índice ({o})")),
            None => out.push("organizada como heap".to_string()),
        }
        match &self.partitioning {
            Some(p) => out.push(format!("particionada ({p})")),
            None => out.push("sin particionar".to_string()),
        }
        out.push(if self.row_movement { "con ROW MOVEMENT".into() } else { "sin ROW MOVEMENT".into() });
        out.join(", ")
    }
}

/// A number as the session returns it (`50`, `"50"`, `50.0`).
fn number(v: Option<&String>) -> Option<i64> {
    v.and_then(|s| s.trim().parse::<f64>().ok()).map(|f| f as i64)
}

/// A partition named by the server (`SYS_P123`): left unnamed in the clone.
fn server_named_partition(n: &str) -> bool {
    n.strip_prefix("SYS_P").is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// `PARTITION BY RANGE | LIST | HASH (keys) (PARTITION …, …)`.
/// Each partition: (name, bound, its own clauses: compression).
fn partition_clause(kind: &str, keys: &[String], parts: &[(String, String, String)]) -> String {
    let keys: Vec<String> = keys.iter().map(|k| quote_ident(Quote::Double, k)).collect();
    let parts: Vec<String> = parts
        .iter()
        .map(|(n, hv, extra)| {
            let name = if server_named_partition(n) { String::new() } else { format!(" {}", quote_ident(Quote::Double, n)) };
            let extra = if extra.is_empty() { String::new() } else { format!(" {extra}") };
            match kind {
                "RANGE" => format!("PARTITION{name} VALUES LESS THAN ({}){extra}", hv.trim()),
                "LIST" => format!("PARTITION{name} VALUES ({}){extra}", hv.trim()),
                _ => format!("PARTITION{name}{extra}"),
            }
        })
        .collect();
    format!("PARTITION BY {kind} ({}) ({})", keys.join(", "), parts.join(", "))
}

/// The compression clause for ALL_TABLES' COMPRESSION / COMPRESS_FOR (or
/// a partition's): `None` when not compressed, `Err` (in Spanish) when it's
/// a kind the clone can't be created with.
fn compression_clause(compression: &str, compress_for: &str) -> std::result::Result<Option<String>, String> {
    if compression != "ENABLED" {
        return Ok(None);
    }
    let f = compress_for.trim();
    let clause = match f {
        "" => "COMPRESS".to_string(),
        "BASIC" => "COMPRESS BASIC".to_string(),
        "OLTP" => "COMPRESS FOR OLTP".to_string(),
        "ADVANCED" => "ROW STORE COMPRESS ADVANCED".to_string(),
        _ if f.starts_with("QUERY ") || f.starts_with("ARCHIVE ") => format!("COLUMN STORE COMPRESS FOR {f}"),
        _ => return Err(format!("no se puede clonar: la tabla tiene una compresión ({f}) que el clon no puede recrear igual")),
    };
    Ok(Some(clause))
}

/// The table's storage (see [`Physical`]). Refused, in Spanish, what the
/// clone can't have the same way.
async fn physical(s: &mut dyn Session, schema: Option<&str>, table: &str) -> Result<Physical> {
    let fail = |what: &str| Error::State(format!("no se pudo leer {what} de la tabla; no se clona"));
    let (o, tn) = (owner(schema), lit(table));
    let sql = format!(
        "SELECT t.iot_type, t.partitioned, t.row_movement, i.pct_threshold, i.compression, i.prefix_length, \
         (SELECT COUNT(*) FROM all_tables o WHERE o.owner = t.owner AND o.iot_name = t.table_name), \
         t.temporary, t.duration, t.compression, t.compress_for \
         FROM all_tables t LEFT JOIN all_indexes i ON i.table_owner = t.owner AND i.table_name = t.table_name AND i.index_type = 'IOT - TOP' \
         WHERE t.owner = {o} AND t.table_name = {tn}"
    );
    let rows = strings(s, &sql).await.map_err(|_| fail("la organización física"))?;
    let Some(r) = rows.first() else { return Ok(Physical::default()) };
    let v = |i: usize| r.get(i).cloned().flatten().unwrap_or_default().trim().to_string();
    let mut p = Physical { row_movement: v(2) == "ENABLED", ..Default::default() };
    if v(7) == "Y" {
        p.temporary = Some(match v(8).as_str() {
            "SYS$SESSION" => "ON COMMIT PRESERVE ROWS".to_string(),
            "SYS$TRANSACTION" => "ON COMMIT DELETE ROWS".to_string(),
            d => return Err(Error::Unsupported(format!("no se puede clonar: la tabla es temporal con una duración ({d}) que el clon no puede recrear igual"))),
        });
    }
    if v(0) != "IOT" {
        p.compression = compression_clause(&v(9), &v(10)).map_err(Error::Unsupported)?;
    }
    if v(0) == "IOT" {
        if number(r.get(6).and_then(|x| x.as_ref())).unwrap_or(0) > 0 {
            return Err(Error::Unsupported(
                "no se puede clonar: la tabla está organizada por índice con un segmento de desbordamiento (OVERFLOW) o una tabla de mapeo, que el clon no puede recrear igual".into(),
            ));
        }
        let mut o = format!("PCTTHRESHOLD {}", number(r.get(3).and_then(|x| x.as_ref())).unwrap_or(50));
        if v(4) == "ENABLED" {
            o.push_str(&format!(" COMPRESS {}", number(r.get(5).and_then(|x| x.as_ref())).unwrap_or(1)));
        }
        p.iot = Some(o);
    }
    if v(1) != "YES" {
        return Ok(p);
    }
    let refuse = |why: &str| Error::Unsupported(format!("no se puede clonar: la tabla está particionada {why}, y el clon no puede recrear ese particionado igual"));
    let sql = format!(
        "SELECT partitioning_type, subpartitioning_type, interval, def_compression, def_compress_for FROM all_part_tables WHERE owner = {o} AND table_name = {tn}"
    );
    let rows = strings(s, &sql).await.map_err(|_| fail("el particionado"))?;
    let r = rows.first().ok_or_else(|| fail("el particionado"))?;
    let v = |i: usize| r.get(i).cloned().flatten().unwrap_or_default().trim().to_string();
    let kind = v(0);
    // The partitions' default (ALL_TABLES doesn't say it for a partitioned
    // table); a partition compressed otherwise says so on its own.
    let default = compression_clause(&v(3), &v(4)).map_err(Error::Unsupported)?;
    p.compression = default.clone();
    if !matches!(kind.as_str(), "RANGE" | "LIST" | "HASH") {
        return Err(refuse(&format!("por {kind}")));
    }
    if !matches!(v(1).as_str(), "" | "NONE") {
        return Err(refuse(&format!("con subparticiones ({kind}-{})", v(1))));
    }
    if !v(2).is_empty() {
        return Err(refuse(&format!("por intervalo (INTERVAL {})", v(2))));
    }
    // AUTOLIST (12.2 and later; the column isn't there before).
    let sql = format!("SELECT autolist FROM all_part_tables WHERE owner = {o} AND table_name = {tn}");
    if strings(s, &sql).await.ok().and_then(|r| r.first().and_then(|r| r.first().cloned().flatten())).as_deref() == Some("YES") {
        return Err(refuse("por lista automática (AUTOMATIC)"));
    }
    let sql = format!(
        "SELECT column_name FROM all_part_key_columns WHERE owner = {o} AND name = {tn} AND TRIM(object_type) = 'TABLE' ORDER BY column_position"
    );
    let keys: Vec<String> = strings(s, &sql).await.map_err(|_| fail("el particionado"))?.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
    // HIGH_VALUE is a LONG: last.
    let sql = format!(
        "SELECT partition_name, compression, compress_for, high_value FROM all_tab_partitions WHERE table_owner = {o} AND table_name = {tn} ORDER BY partition_position"
    );
    let mut parts: Vec<(String, String, String)> = Vec::new();
    for r in strings(s, &sql).await.map_err(|_| fail("las particiones"))? {
        let v = |i: usize| r.get(i).cloned().flatten().unwrap_or_default().trim().to_string();
        let own = compression_clause(&v(1), &v(2)).map_err(Error::Unsupported)?;
        let extra = match (&own, &default) {
            (a, b) if a == b => String::new(),
            (Some(c), _) => c.clone(),
            (None, Some(_)) => "NOCOMPRESS".to_string(),
            (None, None) => String::new(),
        };
        parts.push((v(0), r.get(3).cloned().flatten().unwrap_or_default(), extra));
    }
    if keys.is_empty() || parts.is_empty() {
        return Err(fail("el particionado"));
    }
    p.partitioning = Some(partition_clause(&kind, &keys, &parts));
    Ok(p)
}

/// The CREATE with the table's storage: `head` right after the column
/// list, `tail` at the end of the statement.
fn with_storage(create: &str, p: &Physical) -> Option<String> {
    let (head, tail) = p.clauses();
    if head.is_empty() && tail.is_empty() {
        return Some(create.to_string());
    }
    let create = match p.temporary {
        Some(_) => format!("CREATE GLOBAL TEMPORARY TABLE {}", create.strip_prefix("CREATE TABLE ")?),
        None => create.to_string(),
    };
    let close = create.find("\n)")? + 2;
    let rest = &create[close..];
    let end = rest.find('\n').unwrap_or(rest.len());
    let mid = rest[..end].strip_suffix(';')?;
    Some(format!("{}{head}{mid}{tail};{}", &create[..close], &rest[end..]))
}

/// A global temporary table's rows belong to each session: the clone is
/// created empty (the copy would take only the reading session's rows,
/// and then its indexes couldn't be created). The note, in Spanish.
pub(super) fn session_rows(o: &Original, options: &mut CloneOptions) -> Option<String> {
    let t = o.physical.temporary.as_deref()?;
    let copied = std::mem::replace(&mut options.with_data, false);
    Some(format!(
        "la tabla es temporal global ({t}): sus filas son de cada sesión, así que el clon se crea también temporal global{}",
        if copied { " y vacío" } else { "" }
    ))
}

fn lit(v: &str) -> String {
    format!("'{}'", v.replace('\'', "''"))
}

fn owner(schema: Option<&str>) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(x) => lit(x),
        None => "SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')".into(),
    }
}

/// The table's constraints, one row each (SEARCH_CONDITION is a LONG: last).
fn constraints_sql(schema: Option<&str>, table: &str) -> String {
    format!(
        "SELECT c.constraint_name, c.constraint_type, c.status, c.validated, c.deferrable, c.deferred, c.rely, c.generated, \
         c.index_owner, c.index_name, (SELECT LISTAGG(cc.column_name, CHR(1)) WITHIN GROUP (ORDER BY cc.position) FROM all_cons_columns cc \
         WHERE cc.owner = c.owner AND cc.constraint_name = c.constraint_name AND cc.table_name = c.table_name), c.search_condition \
         FROM all_constraints c WHERE c.owner = {} AND c.table_name = {} AND c.constraint_type IN ('C', 'P', 'U', 'R')",
        owner(schema),
        lit(table)
    )
}

fn parse(rows: Vec<Vec<Option<String>>>) -> Vec<(Con, Option<String>)> {
    rows.into_iter()
        .filter_map(|r| {
            let v = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            let c = Con {
                name: v(0),
                kind: v(1),
                enabled: v(2) == "ENABLED",
                validated: v(3) == "VALIDATED",
                deferrable: v(4) == "DEFERRABLE",
                deferred: v(5) == "DEFERRED",
                rely: v(6) == "RELY",
                generated: v(7) == "GENERATED NAME",
                index: Some(v(9)).filter(|s| !s.is_empty()),
                columns: v(10).split('\u{1}').filter(|s| !s.is_empty()).map(str::to_string).collect(),
                condition: v(11).trim().to_string(),
            };
            (!c.name.is_empty()).then(|| (c, Some(v(8)).filter(|s| !s.is_empty())))
        })
        .collect()
}

async fn read(s: &mut dyn Session, schema: Option<&str>, table: &str) -> Result<Vec<(Con, Option<String>)>> {
    let rows = strings(s, &constraints_sql(schema, table))
        .await
        .map_err(|_| Error::State("no se pudo leer el estado de las restricciones de la tabla; no se clona".into()))?;
    Ok(parse(rows))
}

/// `"COL" IS NOT NULL`, as Oracle writes a column's NOT NULL constraint.
fn not_null_column(condition: &str) -> Option<&str> {
    let c = condition.trim().strip_suffix("IS NOT NULL")?.trim_end();
    let c = c.strip_prefix('"')?.strip_suffix('"')?;
    (!c.contains('"')).then_some(c)
}

/// The index Oracle makes for a key on its own: named like the constraint,
/// on its columns, unique unless the key is deferrable.
fn own_index(c: &Con, unique: bool, columns: &[String]) -> bool {
    c.index.as_deref() == Some(c.name.as_str()) && unique != c.deferrable && columns == c.columns.as_slice()
}

/// The original's constraints and the indexes its keys use. Refused, in
/// Spanish, what the clone can't have the same way.
pub(super) async fn inspect(s: &mut dyn Session, t: &TableSchema) -> Result<Original> {
    let found = read(s, t.schema.as_deref(), &t.name).await?;
    if let Some((c, _)) = found.iter().find(|(c, _)| !c.enabled && c.validated) {
        return Err(Error::Unsupported(format!(
            "no se puede clonar: la restricción «{}» está en estado DISABLE VALIDATE (la tabla no admite cambios de datos), así que el clon no podría recibir las filas",
            c.name
        )));
    }
    let mut backing = Vec::new();
    for (c, index_owner) in found.iter().filter(|(c, _)| matches!(c.kind.as_str(), "P" | "U") && c.enabled) {
        let Some(ix) = c.index.as_deref() else { continue };
        let sql = format!(
            "SELECT i.uniqueness, ic.column_name, ic.descend FROM all_indexes i JOIN all_ind_columns ic ON ic.index_owner = i.owner AND ic.index_name = i.index_name \
             WHERE i.owner = {} AND i.index_name = {} ORDER BY ic.column_position",
            owner(index_owner.as_deref().or(t.schema.as_deref())),
            lit(ix)
        );
        let rows = strings(s, &sql)
            .await
            .map_err(|_| Error::State(format!("no se pudo leer el índice «{ix}» de la restricción «{}»; no se clona", c.name)))?;
        let unique = rows.first().and_then(|r| r.first().cloned().flatten()).as_deref() == Some("UNIQUE");
        let columns: Vec<String> = rows.iter().filter_map(|r| r.get(1).cloned().flatten()).collect();
        if own_index(c, unique, &columns) {
            continue;
        }
        let expression = rows.iter().any(|r| r.get(2).cloned().flatten().as_deref() == Some("DESC"))
            || columns.iter().any(|c| c.starts_with("SYS_NC"));
        if expression || columns.is_empty() {
            return Err(Error::Unsupported(format!(
                "no se puede clonar: la restricción «{}» usa el índice «{ix}», con columnas descendentes o expresiones, que el clon no puede recrear igual",
                c.name
            )));
        }
        if index_owner.as_deref().is_some_and(|o| t.schema.as_deref().is_some_and(|s| !s.is_empty() && s != o)) {
            return Err(Error::Unsupported(format!(
                "no se puede clonar: la restricción «{}» usa el índice «{ix}», que está en otro esquema",
                c.name
            )));
        }
        backing.push(Backing { constraint: c.name.clone(), name: ix.to_string(), unique, columns });
    }
    let physical = physical(s, t.schema.as_deref(), &t.name).await?;
    let mut local = Vec::new();
    if physical.partitioning.is_some() {
        for (ix, parts) in local_partitions(s, t.schema.as_deref(), &t.name).await? {
            if own_partition_names(&parts) {
                local.push((ix, parts.into_iter().map(|(i, _)| i).collect()));
            }
        }
    }
    Ok(Original { cons: found.into_iter().map(|(c, _)| c).collect(), backing, physical, local })
}

/// The clone's names for the keys' own indexes, renamed like the
/// constraints (so they're checked against the schema too).
pub(super) fn complete(o: &Original, p: &mut ClonePlan, old: &str, new: &str, max: usize, reserved: &[String]) {
    // Unique constraints the server named (`SYS_C…`; the driver reports
    // them as indexes by that name): not renamed, the server names the
    // clone's too, as it does its other constraints (see `adjust`).
    let server_named: Vec<&str> = o.cons.iter().filter(|c| c.kind == "U" && c.generated).map(|c| c.name.as_str()).collect();
    for ix in &mut p.table.indexes {
        if let Some(r) = p.renames.iter().find(|r| r.to == ix.name && server_named.contains(&r.from.as_str())) {
            ix.name = r.from.clone();
        }
    }
    p.renames.retain(|r| !server_named.contains(&r.from.as_str()));
    // Named NOT NULL constraints the columns stand for (the driver writes
    // them as the columns' NOT NULL): renamed like the others.
    let not_null = o.cons.iter().filter(|c| c.kind == "C" && !c.generated && not_null_column(&c.condition).is_some()).map(|c| c.name.clone());
    for name in o.backing.iter().map(|b| b.name.clone()).chain(not_null) {
        if p.renames.iter().any(|r| r.from == name) {
            continue;
        }
        let (mut to, shortened) = rename_capped(&name, old, new, max, false, 0);
        let first = to.clone();
        let mut k = 0u32;
        while p.renames.iter().map(|r| &r.to).chain(reserved).any(|u| u.eq_ignore_ascii_case(&to)) {
            k += 1;
            let suffix = format!("_{:08x}", fnv(&format!("{name}#{k}")));
            let base = fit(&to, max.saturating_sub(suffix.len()), false, 0).to_string();
            to = format!("{base}{suffix}");
        }
        if shortened {
            p.notes.push(format!("nombre acortado para entrar en el límite de {} del motor: {name} → {to}", limit_text(max, false, 0)));
        }
        if reserved.iter().any(|u| u.eq_ignore_ascii_case(&first)) {
            p.notes.push(format!("nombre que ya usaba otro objeto del esquema, cambiado: {name} → {to}"));
        }
        p.renames.push(Rename { from: name, to, shortened });
    }
}

/// Anything but enabled, validated, not deferrable and NORELY.
fn special(c: &Con) -> bool {
    !c.enabled || !c.validated || c.deferrable || c.rely
}

fn renamed<'a>(renames: &'a [Rename], name: &'a str) -> &'a str {
    renames.iter().find(|r| r.from == name).map(|r| r.to.as_str()).unwrap_or(name)
}

fn table_name(t: &TableSchema) -> String {
    qualified_name(Quote::Double, t.schema.as_deref().filter(|s| !s.is_empty()), &t.name)
}

/// `CREATE [UNIQUE] INDEX …` for `USING INDEX (…)`, in the clone's schema.
fn backing_sql(b: &Backing, clone: &TableSchema, renames: &[Rename]) -> String {
    let cols: Vec<String> = b.columns.iter().map(|c| quote_ident(Quote::Double, c)).collect();
    format!(
        "CREATE {}INDEX {} ON {} ({})",
        if b.unique { "UNIQUE " } else { "" },
        qualified_name(Quote::Double, clone.schema.as_deref().filter(|s| !s.is_empty()), renamed(renames, &b.name)),
        table_name(clone),
        cols.join(", ")
    )
}

/// The constraint state clause: `[DEFERRABLE INITIALLY …] [RELY] [USING
/// INDEX (…)] [status]`.
fn state_clause(c: &Con, using: Option<&str>, status: Option<&str>) -> String {
    let mut out = Vec::new();
    if c.deferrable {
        out.push(format!("DEFERRABLE INITIALLY {}", if c.deferred { "DEFERRED" } else { "IMMEDIATE" }));
    }
    if c.rely {
        out.push("RELY".into());
    }
    if let Some(u) = using {
        out.push(format!("USING INDEX ({u})"));
    }
    if let Some(s) = status {
        out.push(s.into());
    }
    out.join(" ")
}

/// `DISABLE`, `ENABLE NOVALIDATE` or nothing (enabled and validated).
fn status(c: &Con) -> Option<&'static str> {
    match (c.enabled, c.validated) {
        (false, _) => Some("DISABLE"),
        (true, false) => Some("ENABLE NOVALIDATE"),
        _ => None,
    }
}

/// The original's constraint a (planned) one of the clone stands for: by
/// name, or (named by the server) by what it is.
fn find<'a>(o: &'a Original, kind: &str, name: Option<&str>, columns: &[String], condition: Option<&str>) -> Option<&'a Con> {
    match name.filter(|n| !n.is_empty()) {
        Some(n) => o.cons.iter().find(|c| c.kind == kind && c.name == n),
        None => o.cons.iter().find(|c| {
            c.kind == kind
                && c.generated
                && match condition {
                    Some(x) => c.condition == x.trim(),
                    None => c.columns == columns,
                }
        }),
    }
}

/// Statements run after the rows (and before indexes and foreign keys).
#[derive(Debug, Default)]
pub(super) struct AfterLoad {
    /// (constraint, statement).
    pub statements: Vec<(String, String)>,
    /// The clone's table, and the names its columns' NOT NULL constraints
    /// get (column, name): the server names them first.
    pub table: Option<TableSchema>,
    pub not_null: Vec<(String, String)>,
}

/// The clone's DDL with the original's constraint states and key indexes
/// (see the module's docs). `source` is the original as reported; `plan`
/// its clone (same order of checks, indexes and keys).
pub(super) fn adjust(driver: &dyn Driver, o: &Original, source: &TableSchema, plan: &ClonePlan, options: &CloneOptions, ddl: &mut Ddl) -> Result<AfterLoad> {
    let clone = &plan.table;
    let name = table_name(clone);
    let using = |c: &Con| o.backing.iter().find(|b| b.constraint == c.name).map(|b| backing_sql(b, clone, &plan.renames));
    let mut after = AfterLoad::default();
    // Named NOT NULL constraints written as the column's NOT NULL (the
    // column is in the CREATE as NOT NULL): renamed once created.
    for c in o.cons.iter().filter(|c| c.kind == "C" && !c.generated && !special(c)) {
        let Some(col) = not_null_column(&c.condition) else { continue };
        let in_checks = source.checks.iter().any(|k| k.name.as_deref() == Some(c.name.as_str()));
        if !in_checks && clone.columns.iter().any(|x| x.name == col) {
            after.not_null.push((col.to_string(), renamed(&plan.renames, &c.name).to_string()));
        }
    }
    after.table = Some(TableSchema { name: clone.name.clone(), schema: clone.schema.clone(), ..Default::default() });

    // CHECKs in another state: after the rows, already in it.
    let mut t = clone.clone();
    let mut keep = Vec::new();
    for (i, ck) in clone.checks.iter().enumerate() {
        let original = source.checks.get(i).and_then(|s| find(o, "C", s.name.as_deref(), &[], Some(&s.expression)));
        match original.filter(|c| special(c)) {
            Some(c) => {
                let head = match ck.name.as_deref().filter(|n| !n.is_empty()) {
                    Some(n) => format!("CONSTRAINT {} ", quote_ident(Quote::Double, n)),
                    None => String::new(),
                };
                let sql = format!("ALTER TABLE {name} ADD {head}CHECK ({}) {}", ck.expression.trim(), state_clause(c, None, status(c)));
                after.statements.push((ck.name.clone().unwrap_or_else(|| c.name.clone()), sql.trim_end().to_string()));
            }
            None => keep.push(ck.clone()),
        }
    }
    t.checks = keep;
    let mut create = driver.table_ddl(&t, DdlParts { create: true, ..Default::default() })?;

    if let Some(pk) = &clone.primary_key {
        let source_pk = source.primary_key.as_ref();
        let original = find(o, "P", source_pk.and_then(|k| k.name.as_deref()), &pk.columns, None)
            .or_else(|| o.cons.iter().find(|c| c.kind == "P"));
        // A key column's NOT NULL only when the original declares one.
        for col in &pk.columns {
            let declared = o.cons.iter().any(|c| c.kind == "C" && not_null_column(&c.condition) == Some(col.as_str()));
            if !declared {
                create = strip_not_null(&create, col);
            }
        }
        if let Some(c) = original {
            let cols: Vec<String> = pk.columns.iter().map(|c| quote_ident(Quote::Double, c)).collect();
            let key = format!("PRIMARY KEY ({})", cols.join(", "));
            let u = using(c);
            if special(c) || u.is_some() {
                // Not validated: disabled while the rows load, then
                // ENABLE NOVALIDATE (with its index).
                let clause = if c.enabled && c.validated {
                    state_clause(c, u.as_deref(), None)
                } else {
                    state_clause(c, None, Some("DISABLE"))
                };
                if !create.contains(&key) {
                    return Err(Error::State("no se pudo preparar la clave primaria del clon con el estado del original; no se clona".into()));
                }
                create = create.replacen(&key, &format!("{key} {clause}"), 1);
                if c.enabled && !c.validated {
                    let using = u.map(|u| format!(" USING INDEX ({u})")).unwrap_or_default();
                    after.statements.push((
                        pk.name.clone().unwrap_or_else(|| c.name.clone()),
                        format!("ALTER TABLE {name} ENABLE NOVALIDATE PRIMARY KEY{using}"),
                    ));
                }
            }
        }
    }
    // Index-organized, partitioned, row movement: as the original.
    ddl.create = with_storage(&create, &o.physical).ok_or_else(|| {
        Error::State(format!("no se pudo preparar el clon {}; no se clona", o.physical.describe()))
    })?;

    // Unique constraints: added in their state, with their index.
    if options.with_indexes && ddl.indexes.is_some() {
        let mut out = Vec::new();
        for (i, ix) in clone.indexes.iter().enumerate() {
            let one = TableSchema { name: clone.name.clone(), schema: clone.schema.clone(), indexes: vec![ix.clone()], ..Default::default() };
            let mut s = driver.table_ddl(&one, DdlParts { indexes: true, ..Default::default() })?;
            // A LOCAL index's partitions with the original's names.
            if let Some((_, parts)) = source.indexes.get(i).and_then(|x| o.local.iter().find(|(n, _)| *n == x.name)) {
                s = with_local_names(&s, parts).ok_or_else(|| {
                    Error::State(format!("no se pudo preparar el índice local «{}» del clon con los nombres de sus particiones; no se clona", ix.name))
                })?;
            }
            let original = source.indexes.get(i).filter(|_| ix.unique).and_then(|x| o.cons.iter().find(|c| c.kind == "U" && c.name == x.name));
            if let Some(c) = original {
                if c.generated {
                    // Named by the server in the original: the clone's too.
                    let named = format!("ADD CONSTRAINT {} UNIQUE", quote_ident(Quote::Double, &ix.name));
                    if !s.contains(&named) {
                        return Err(Error::State(format!(
                            "no se pudo preparar la restricción única sobre ({}) del clon con un nombre del sistema, como en el original; no se clona",
                            ix.columns.join(", ")
                        )));
                    }
                    s = s.replacen(&named, "ADD UNIQUE", 1);
                }
                let u = using(c);
                if (special(c) || u.is_some()) && s.trim_end().ends_with(';') && !s.contains("EXECUTE IMMEDIATE") {
                    let clause = state_clause(c, u.as_deref().filter(|_| c.enabled), status(c));
                    s = format!("{} {clause};", s.trim_end().trim_end_matches(';'));
                }
            }
            out.push(s);
        }
        ddl.indexes = Some(out.join("\n")).filter(|s| !s.trim().is_empty());
    }

    // Foreign keys: added in their state (a disabled one takes the rows
    // that break it, as the original does).
    if ddl.foreign_keys.is_some() {
        let mut out = Vec::new();
        for (i, fk) in clone.foreign_keys.iter().enumerate() {
            let one = TableSchema { name: clone.name.clone(), schema: clone.schema.clone(), foreign_keys: vec![fk.clone()], ..Default::default() };
            let mut s = driver.table_ddl(&one, DdlParts { foreign_keys: true, ..Default::default() })?;
            let original = source.foreign_keys.get(i).and_then(|x| find(o, "R", x.name.as_deref(), &x.columns, None));
            if let Some(c) = original.filter(|c| special(c)) {
                if s.trim_end().ends_with(';') && !s.contains("EXECUTE IMMEDIATE") {
                    s = format!("{} {};", s.trim_end().trim_end_matches(';'), state_clause(c, None, status(c)));
                }
            }
            out.push(s);
        }
        ddl.foreign_keys = Some(out.join("\n")).filter(|s| !s.trim().is_empty());
    }
    Ok(after)
}

/// The CREATE's `"COL" <type> NOT NULL,` line without its NOT NULL.
fn strip_not_null(create: &str, col: &str) -> String {
    let head = format!("    {} ", quote_ident(Quote::Double, col));
    create
        .split('\n')
        .map(|l| match l.strip_suffix(" NOT NULL,") {
            Some(rest) if l.starts_with(&head) => format!("{rest},"),
            _ => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The ORA- code in an error, if any.
fn ora_code(e: &str) -> Option<&str> {
    let i = e.find("ORA-")?;
    let code = &e[i..];
    Some(&code[..code.find(|c: char| c != '-' && !c.is_ascii_alphanumeric()).unwrap_or(code.len())])
}

/// After the rows: the CHECKs in another state, the primary key's ENABLE
/// NOVALIDATE.
pub(super) async fn after_load(s: &mut dyn Session, a: &AfterLoad) -> Result<()> {
    if let (Some(t), false) = (&a.table, a.not_null.is_empty()) {
        let found = read(s, t.schema.as_deref(), &t.name).await?;
        for (col, to) in &a.not_null {
            let Some((c, _)) = found.iter().find(|(c, _)| c.kind == "C" && c.generated && not_null_column(&c.condition) == Some(col.as_str())) else {
                continue;
            };
            let sql = format!("ALTER TABLE {} RENAME CONSTRAINT {} TO {}", table_name(t), quote_ident(Quote::Double, &c.name), quote_ident(Quote::Double, to));
            if exec(s, &sql).await.is_err() {
                return Err(Error::State(format!("no se pudo dar al NOT NULL de «{col}» en el clon el nombre «{to}»; no se clona")));
            }
        }
    }
    for (constraint, sql) in &a.statements {
        if let Err(e) = exec(s, sql).await {
            let m = e.to_string();
            return Err(Error::State(format!(
                "no se pudo crear en el clon la restricción «{constraint}» con el estado que tiene en el original{}; no se clona",
                ora_code(&m).map(|c| format!(" (el motor respondió {c})")).unwrap_or_default()
            )));
        }
    }
    Ok(())
}

/// A constraint's state, in Spanish.
fn describe(c: &Con) -> String {
    let mut s = match (c.enabled, c.validated) {
        (false, _) => "desactivada".to_string(),
        (true, false) => "activada sin validar (NOVALIDATE)".to_string(),
        _ => "activada y validada".to_string(),
    };
    if c.deferrable {
        s.push_str(if c.deferred { ", diferible (INITIALLY DEFERRED)" } else { ", diferible (INITIALLY IMMEDIATE)" });
    }
    if c.rely {
        s.push_str(", RELY");
    }
    s
}

fn kind_text(kind: &str) -> &'static str {
    match kind {
        "P" => "la clave primaria",
        "U" => "la restricción única",
        "R" => "la clave foránea",
        _ => "la restricción",
    }
}

/// Each original constraint and the clone's that stands for it; the
/// differences in Spanish.
fn differences(original: &[Con], clone: &[Con], renames: &[Rename], with_indexes: bool) -> Vec<String> {
    let mut used = vec![false; clone.len()];
    let mut out = Vec::new();
    for o in original.iter().filter(|o| with_indexes || o.kind != "U") {
        let nn = (o.kind == "C").then(|| not_null_column(&o.condition)).flatten();
        let to = renames.iter().find(|r| r.from == o.name).map(|r| r.to.as_str());
        let found = clone.iter().enumerate().position(|(i, c)| {
            !used[i]
                && c.kind == o.kind
                && match (nn, to) {
                    // NOT NULL: by column (the driver writes it on the column).
                    (Some(col), _) => not_null_column(&c.condition) == Some(col),
                    // Named by the server: the clone's must be too.
                    _ if o.generated => c.generated && same(o, c),
                    (None, Some(t)) => c.name.eq_ignore_ascii_case(t),
                    (None, None) => c.name == o.name,
                }
        });
        let Some(i) = found else {
            match clone.iter().enumerate().find(|(i, c)| !used[*i] && o.generated && c.kind == o.kind && !c.generated && same(o, c)) {
                Some((i, c)) => {
                    used[i] = true;
                    out.push(format!("{} «{}» tiene un nombre del sistema en el original y se llama «{}» en el clon", kind_text(&o.kind), o.name, c.name));
                }
                None => out.push(format!("{} «{}» del original no está en el clon", kind_text(&o.kind), o.name)),
            }
            continue;
        };
        used[i] = true;
        let c = &clone[i];
        if nn.is_some() && !o.generated && !c.name.eq_ignore_ascii_case(to.unwrap_or(&o.name)) {
            out.push(format!("la restricción NOT NULL «{}» se llama «{}» en el clon", o.name, c.name));
        }
        if (o.enabled, o.validated, o.deferrable, o.deferred, o.rely) != (c.enabled, c.validated, c.deferrable, c.deferred, c.rely) {
            out.push(format!("{} «{}» está {} en el original y {} en el clon", kind_text(&o.kind), o.name, describe(o), describe(c)));
        }
        if matches!(o.kind.as_str(), "P" | "U") && !o.generated {
            let expected = o.index.as_deref().map(|ix| renamed(renames, ix));
            if expected.map(str::to_ascii_uppercase) != c.index.as_deref().map(str::to_ascii_uppercase) {
                out.push(format!(
                    "{} «{}» usa el índice «{}» en el original y «{}» en el clon",
                    kind_text(&o.kind),
                    o.name,
                    o.index.as_deref().unwrap_or("-"),
                    c.index.as_deref().unwrap_or("-")
                ));
            }
        }
    }
    // A NOT NULL the original doesn't have (a key column's), or any other
    // constraint.
    for c in clone.iter().enumerate().filter(|(i, c)| !used[*i] && (with_indexes || c.kind != "U")).map(|(_, c)| c) {
        match (c.kind.as_str(), not_null_column(&c.condition)) {
            ("C", Some(col)) => out.push(format!("el clon tiene una restricción NOT NULL sobre «{col}» que el original no tiene")),
            _ => out.push(format!("el clon tiene {} «{}», que el original no tiene", kind_text(&c.kind), c.name)),
        }
    }
    out
}

/// The LOCAL indexes' partition names: the clone's (renamed index) must
/// be the original's, but for the ones the server named.
fn local_differences(original: &[(String, Vec<String>)], clone: &[(String, Vec<(String, String)>)], renames: &[Rename]) -> Vec<String> {
    let mut out = Vec::new();
    for (ix, parts) in original {
        let to = renamed(renames, ix);
        let got: Vec<&str> = clone.iter().find(|(n, _)| n.eq_ignore_ascii_case(to)).map(|(_, p)| p.iter().map(|(i, _)| i.as_str()).collect()).unwrap_or_default();
        let same = got.len() == parts.len() && parts.iter().zip(&got).all(|(a, b)| server_named_partition(a) || a == b);
        if !same {
            out.push(format!("las particiones del índice local «{ix}» se llaman {} en el original y {} en el clon", parts.join(", "), if got.is_empty() { "-".to_string() } else { got.join(", ") }));
        }
    }
    out
}

/// The same constraint by what it is: a CHECK's condition, a key's columns.
fn same(o: &Con, c: &Con) -> bool {
    if o.kind == "C" {
        c.condition == o.condition
    } else {
        c.columns == o.columns
    }
}

/// The clone's constraints against the original's: same states, same key
/// indexes (renamed). Refused, in Spanish, when they differ.
pub(super) async fn verify(s: &mut dyn Session, o: &Original, plan: &ClonePlan, with_indexes: bool) -> Result<()> {
    let clone: Vec<Con> = read(s, plan.table.schema.as_deref(), &plan.table.name).await?.into_iter().map(|(c, _)| c).collect();
    let mut diffs = differences(&o.cons, &clone, &plan.renames, with_indexes);
    let storage = physical(s, plan.table.schema.as_deref(), &plan.table.name).await?;
    if storage != o.physical {
        diffs.push(format!("el original está {} y el clon {}", o.physical.describe(), storage.describe()));
    }
    if with_indexes && !o.local.is_empty() {
        let clone = local_partitions(s, plan.table.schema.as_deref(), &plan.table.name).await?;
        diffs.extend(local_differences(&o.local, &clone, &plan.renames));
    }
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(Error::State(format!("el clon no quedó igual al original ({}); no se clona", diffs.join("; "))))
    }
}

/// Oracle errors a clone can still meet, in Spanish (the constraint named);
/// any other error as it came.
pub(super) fn in_spanish(e: Error) -> Error {
    let m = e.to_string();
    let named = |code: &str| -> Option<String> {
        let i = m.find(code)?;
        let r = &m[i..];
        let open = r.find('(')?;
        let close = r[open..].find(')')?;
        Some(r[open + 1..open + close].to_string())
    };
    let text = match ora_code(&m) {
        Some("ORA-02290") => named("ORA-02290").map(|n| format!("una fila no cumple la restricción CHECK {n} (ORA-02290)")),
        Some("ORA-02293") => named("ORA-02293").map(|n| format!("las filas no cumplen la restricción CHECK {n} (ORA-02293)")),
        Some("ORA-02298") => named("ORA-02298").map(|n| format!("hay filas sin su fila padre para la clave foránea {n} (ORA-02298)")),
        Some("ORA-02291") => named("ORA-02291").map(|n| format!("una fila no tiene su fila padre para la clave foránea {n} (ORA-02291)")),
        Some("ORA-02299") => named("ORA-02299").map(|n| format!("hay valores repetidos que impiden crear la restricción {n} (ORA-02299)")),
        Some("ORA-00001") => named("ORA-00001").map(|n| format!("hay valores repetidos para la restricción única {n} (ORA-00001)")),
        Some("ORA-01452") => Some("hay valores repetidos que impiden crear un índice único (ORA-01452)".into()),
        _ => None,
    };
    match text {
        Some(t) => Error::State(t),
        None => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn con(name: &str, kind: &str) -> Con {
        Con { name: name.into(), kind: kind.into(), enabled: true, validated: true, ..Default::default() }
    }

    #[test]
    fn catalog_rows_are_read() {
        let row = |v: &[Option<&str>]| v.iter().map(|x| x.map(str::to_string)).collect::<Vec<_>>();
        let got = parse(vec![
            row(&[Some("CK_POS"), Some("C"), Some("DISABLED"), Some("NOT VALIDATED"), Some("NOT DEFERRABLE"), Some("IMMEDIATE"), None, Some("USER NAME"), None, None, Some("V"), Some(" v > 0 ")]),
            row(&[Some("PK_X"), Some("P"), Some("ENABLED"), Some("VALIDATED"), Some("DEFERRABLE"), Some("DEFERRED"), Some("RELY"), Some("USER NAME"), Some("DBINE"), Some("UXI_X"), Some("A\u{1}B"), None]),
        ]);
        assert_eq!(got[0].0, Con { name: "CK_POS".into(), kind: "C".into(), columns: vec!["V".into()], condition: "v > 0".into(), ..Default::default() });
        assert!(got[1].0.deferrable && got[1].0.deferred && got[1].0.rely && got[1].0.enabled && got[1].0.validated);
        assert_eq!(got[1].0.index.as_deref(), Some("UXI_X"));
        assert_eq!(got[1].0.columns, vec!["A".to_string(), "B".to_string()]);
        assert_eq!(got[1].1.as_deref(), Some("DBINE"));
        assert!(constraints_sql(None, "T'X").contains("c.table_name = 'T''X'"));
    }

    #[test]
    fn state_clauses() {
        let mut c = con("CK", "C");
        assert!(!special(&c));
        c.enabled = false;
        c.validated = false;
        assert_eq!(state_clause(&c, None, status(&c)), "DISABLE");
        c.enabled = true;
        assert_eq!(state_clause(&c, None, status(&c)), "ENABLE NOVALIDATE");
        c.validated = true;
        c.deferrable = true;
        c.deferred = true;
        c.rely = true;
        assert_eq!(state_clause(&c, Some("CREATE INDEX \"I\" ON \"T\" (\"A\")"), status(&c)), "DEFERRABLE INITIALLY DEFERRED RELY USING INDEX (CREATE INDEX \"I\" ON \"T\" (\"A\"))");
        assert_eq!(not_null_column("\"ID\" IS NOT NULL"), Some("ID"));
        assert_eq!(not_null_column("id IS NOT NULL"), None);
        assert_eq!(ora_code("x ORA-02290: check constraint (A.B) violated"), Some("ORA-02290"));
    }

    #[test]
    fn a_key_column_keeps_only_a_declared_not_null() {
        let create = "CREATE TABLE \"T\" (\n    \"ID\" NUMBER NOT NULL,\n    \"V\" NUMBER NOT NULL,\n    CONSTRAINT \"PK\" PRIMARY KEY (\"ID\")\n)";
        assert_eq!(strip_not_null(create, "ID"), "CREATE TABLE \"T\" (\n    \"ID\" NUMBER,\n    \"V\" NUMBER NOT NULL,\n    CONSTRAINT \"PK\" PRIMARY KEY (\"ID\")\n)");
    }

    #[test]
    fn states_and_key_indexes_are_compared() {
        let renames = vec![
            Rename { from: "CK_POS".into(), to: "CK_POS_C".into(), shortened: false },
            Rename { from: "PK_X".into(), to: "PK_X_C".into(), shortened: false },
            Rename { from: "UXI_X".into(), to: "UXI_X_C".into(), shortened: false },
        ];
        let mut ck = con("CK_POS", "C");
        ck.enabled = false;
        ck.validated = false;
        let mut pk = con("PK_X", "P");
        pk.index = Some("UXI_X".into());
        let nn = Con { condition: "\"V\" IS NOT NULL".into(), generated: true, ..con("SYS_C1", "C") };
        let original = vec![ck.clone(), pk.clone(), nn.clone()];
        let mut clone = vec![
            Con { name: "CK_POS_C".into(), ..ck.clone() },
            Con { name: "PK_X_C".into(), index: Some("UXI_X_C".into()), ..pk.clone() },
            Con { name: "SYS_C9".into(), ..nn.clone() },
        ];
        assert!(differences(&original, &clone, &renames, true).is_empty());
        clone[0].enabled = true;
        clone[0].validated = true;
        clone[1].index = Some("PK_X_C".into());
        clone.push(Con { name: "SYS_C10".into(), condition: "\"ID\" IS NOT NULL".into(), generated: true, ..con("", "C") });
        let d = differences(&original, &clone, &renames, true);
        assert_eq!(d.len(), 3, "{d:?}");
        assert!(d[0].contains("desactivada en el original y activada y validada en el clon"), "{d:?}");
        assert!(d[1].contains("usa el índice «UXI_X»"), "{d:?}");
        assert!(d[2].contains("NOT NULL sobre «ID»"), "{d:?}");
    }

    #[test]
    fn a_server_named_unique_must_stay_server_named() {
        let mut u = con("SYS_C0010212", "U");
        u.generated = true;
        u.columns = vec!["U".into()];
        u.index = Some("SYS_C0010212".into());
        let original = vec![u.clone()];
        // The clone's, named by the server too: the same.
        let clone = vec![Con { name: "SYS_C0010999".into(), index: Some("SYS_C0010999".into()), ..u.clone() }];
        assert!(differences(&original, &clone, &[], true).is_empty());
        // Named after the clone (what round 3 found): a difference.
        let clone = vec![Con { name: "CLONV_A1_C_SYS_C0010212".into(), generated: false, index: Some("CLONV_A1_C_SYS_C0010212".into()), ..u.clone() }];
        let renames = vec![Rename { from: "SYS_C0010212".into(), to: "CLONV_A1_C_SYS_C0010212".into(), shortened: false }];
        let d = differences(&original, &clone, &renames, true);
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].contains("nombre del sistema en el original y se llama «CLONV_A1_C_SYS_C0010212»"), "{d:?}");
        // A constraint the original doesn't have.
        let extra = vec![Con { name: "SYS_C0010999".into(), index: Some("SYS_C0010999".into()), ..u.clone() }, Con { columns: vec!["V".into()], ..con("UQ_X", "U") }];
        let d = differences(&original, &extra, &[], true);
        assert_eq!(d, vec!["el clon tiene la restricción única «UQ_X», que el original no tiene".to_string()]);
    }

    #[test]
    fn a_server_named_unique_is_not_renamed_and_is_added_unnamed() {
        let mut u = con("SYS_C0010212", "U");
        u.generated = true;
        u.columns = vec!["U".into()];
        let o = Original { cons: vec![u], ..Default::default() };
        let mut p = ClonePlan {
            table: TableSchema {
                name: "T_C".into(),
                indexes: vec![dbine_driver::IndexDef { name: "T_C_SYS_C0010212".into(), columns: vec!["U".into()], unique: true, ..Default::default() }],
                ..Default::default()
            },
            renames: vec![Rename { from: "SYS_C0010212".into(), to: "T_C_SYS_C0010212".into(), shortened: false }],
            notes: vec![],
        };
        complete(&o, &mut p, "T", "T_C", 128, &[]);
        assert!(p.renames.is_empty(), "{:?}", p.renames);
        assert_eq!(p.table.indexes[0].name, "SYS_C0010212");
    }

    #[test]
    fn storage_goes_into_the_create() {
        let create = "CREATE TABLE \"T\" (\n    \"ID\" NUMBER,\n    CONSTRAINT \"PK\" PRIMARY KEY (\"ID\")\n);\nCOMMENT ON TABLE \"T\" IS 'x';";
        assert_eq!(with_storage(create, &Physical::default()).as_deref(), Some(create));
        let iot = Physical { iot: Some("PCTTHRESHOLD 50".into()), ..Default::default() };
        assert_eq!(
            with_storage(create, &iot).unwrap(),
            "CREATE TABLE \"T\" (\n    \"ID\" NUMBER,\n    CONSTRAINT \"PK\" PRIMARY KEY (\"ID\")\n) ORGANIZATION INDEX PCTTHRESHOLD 50;\nCOMMENT ON TABLE \"T\" IS 'x';"
        );
        let parts = partition_clause(
            "RANGE",
            &["D".into()],
            &[("P1".into(), "TO_DATE(' 2024-01-01 00:00:00', 'SYYYY-MM-DD HH24:MI:SS', 'NLS_CALENDAR=GREGORIAN')".into(), String::new()), ("SYS_P41".into(), "MAXVALUE".into(), String::new())],
        );
        assert_eq!(
            parts,
            "PARTITION BY RANGE (\"D\") (PARTITION \"P1\" VALUES LESS THAN (TO_DATE(' 2024-01-01 00:00:00', 'SYYYY-MM-DD HH24:MI:SS', 'NLS_CALENDAR=GREGORIAN')), PARTITION VALUES LESS THAN (MAXVALUE))"
        );
        let both = Physical {
            iot: Some("PCTTHRESHOLD 50 COMPRESS 1".into()),
            partitioning: Some("PARTITION BY HASH (\"ID\") (PARTITION \"A\")".into()),
            row_movement: true,
            ..Default::default()
        };
        let with_ts = "CREATE TABLE \"T\" (\n    \"ID\" NUMBER\n) TABLESPACE \"USERS\";";
        assert_eq!(
            with_storage(with_ts, &both).unwrap(),
            "CREATE TABLE \"T\" (\n    \"ID\" NUMBER\n) ORGANIZATION INDEX TABLESPACE \"USERS\" PCTTHRESHOLD 50 COMPRESS 1 PARTITION BY HASH (\"ID\") (PARTITION \"A\") ENABLE ROW MOVEMENT;"
        );
        assert_eq!(partition_clause("LIST", &["K".into()], &[("PD".into(), "DEFAULT".into(), String::new())]), "PARTITION BY LIST (\"K\") (PARTITION \"PD\" VALUES (DEFAULT))");
        assert!(!server_named_partition("SYS_PX") && server_named_partition("SYS_P1234"));
    }

    #[test]
    fn a_global_temporary_table_stays_temporary_and_empty() {
        let create = "CREATE TABLE \"S\".\"T\" (\n    \"ID\" NUMBER,\n    PRIMARY KEY (\"ID\")\n);\nCOMMENT ON TABLE \"S\".\"T\" IS 'x';";
        let gtt = Physical { temporary: Some("ON COMMIT PRESERVE ROWS".into()), ..Default::default() };
        assert_eq!(
            with_storage(create, &gtt).unwrap(),
            "CREATE GLOBAL TEMPORARY TABLE \"S\".\"T\" (\n    \"ID\" NUMBER,\n    PRIMARY KEY (\"ID\")\n) ON COMMIT PRESERVE ROWS;\nCOMMENT ON TABLE \"S\".\"T\" IS 'x';"
        );
        let o = Original { physical: gtt.clone(), ..Default::default() };
        let mut options = CloneOptions::default();
        let note = session_rows(&o, &mut options).unwrap();
        assert!(!options.with_data && note.contains("temporal global (ON COMMIT PRESERVE ROWS)") && note.ends_with("y vacío"), "{note}");
        let mut options = CloneOptions::default();
        assert!(session_rows(&Original::default(), &mut options).is_none() && options.with_data);
        assert!(gtt.describe().contains("temporal global") && Physical::default().describe().contains("permanente"));
    }

    #[test]
    fn table_compression_is_carried() {
        assert_eq!(compression_clause("DISABLED", ""), Ok(None));
        assert_eq!(compression_clause("NONE", ""), Ok(None));
        assert_eq!(compression_clause("ENABLED", "BASIC").unwrap().as_deref(), Some("COMPRESS BASIC"));
        assert_eq!(compression_clause("ENABLED", "ADVANCED").unwrap().as_deref(), Some("ROW STORE COMPRESS ADVANCED"));
        assert_eq!(compression_clause("ENABLED", "QUERY LOW").unwrap().as_deref(), Some("COLUMN STORE COMPRESS FOR QUERY LOW"));
        assert!(compression_clause("ENABLED", "RARA").unwrap_err().starts_with("no se puede clonar: la tabla tiene una compresión (RARA)"));
        let create = "CREATE TABLE \"T\" (\n    \"ID\" NUMBER\n) TABLESPACE \"USERS\";";
        let c = Physical { compression: Some("COMPRESS BASIC".into()), partitioning: Some("PARTITION BY HASH (\"ID\") (PARTITION \"A\" NOCOMPRESS)".into()), ..Default::default() };
        assert_eq!(
            with_storage(create, &c).unwrap(),
            "CREATE TABLE \"T\" (\n    \"ID\" NUMBER\n) TABLESPACE \"USERS\" COMPRESS BASIC PARTITION BY HASH (\"ID\") (PARTITION \"A\" NOCOMPRESS);"
        );
        assert_ne!(c, Physical { compression: None, ..c.clone() });
        assert!(c.describe().contains("comprimida (COMPRESS BASIC)"));
        assert_eq!(
            partition_clause("RANGE", &["Y".into()], &[("P1".into(), "2024".into(), "ROW STORE COMPRESS ADVANCED".into())]),
            "PARTITION BY RANGE (\"Y\") (PARTITION \"P1\" VALUES LESS THAN (2024) ROW STORE COMPRESS ADVANCED)"
        );
    }

    #[test]
    fn a_local_index_keeps_its_partitions_names() {
        let p = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert!(own_partition_names(&[p("LP1", "P2023"), p("LP2", "SYS_P874")]));
        // Named like the table's partitions (or by the server): nothing to carry.
        assert!(!own_partition_names(&[p("P2023", "P2023"), p("SYS_P900", "SYS_P874")]));
        let parts = vec!["LP1".to_string(), "SYS_P901".to_string(), "Lp 3".to_string()];
        assert_eq!(
            with_local_names("CREATE INDEX \"IX_C\" ON \"T_C\" (\"Y\") LOCAL;", &parts).unwrap(),
            "CREATE INDEX \"IX_C\" ON \"T_C\" (\"Y\") LOCAL (PARTITION \"LP1\", PARTITION, PARTITION \"Lp 3\");"
        );
        assert_eq!(
            with_local_names("CREATE BITMAP INDEX \"IX_C\" ON \"T_C\" (\"Y\") LOCAL INVISIBLE;", &parts[..1]).unwrap(),
            "CREATE BITMAP INDEX \"IX_C\" ON \"T_C\" (\"Y\") LOCAL (PARTITION \"LP1\") INVISIBLE;"
        );
        assert!(with_local_names("CREATE INDEX \"IX_C\" ON \"T_C\" (\"Y\");", &parts).is_none());
        let renames = vec![Rename { from: "IX_L".into(), to: "IX_L_C".into(), shortened: false }];
        let original = vec![("IX_L".to_string(), vec!["LP1".to_string(), "SYS_P1".to_string()])];
        let good = vec![("IX_L_C".to_string(), vec![p("LP1", "P1"), p("SYS_P9", "SYS_P9")])];
        assert!(local_differences(&original, &good, &renames).is_empty());
        let bad = vec![("IX_L_C".to_string(), vec![p("P1", "P1"), p("SYS_P9", "SYS_P9")])];
        let d = local_differences(&original, &bad, &renames);
        assert_eq!(d, vec!["las particiones del índice local «IX_L» se llaman LP1, SYS_P1 en el original y P1, SYS_P9 en el clon".to_string()]);
    }

    #[test]
    fn oracle_errors_in_spanish() {
        let e = in_spanish(Error::Query("ORA-02290: check constraint (DBINE.CK_X) violated".into()));
        assert!(e.to_string().contains("no cumple la restricción CHECK DBINE.CK_X"), "{e}");
        let e = in_spanish(Error::Query("claves: ORA-02298: cannot validate (DBINE.FK_X) - parent keys not found".into()));
        assert!(e.to_string().contains("clave foránea DBINE.FK_X"), "{e}");
    }
}
