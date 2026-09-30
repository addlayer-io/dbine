//! SQL Server: what the reported structure doesn't carry and the clone
//! must have as the original does.
//!
//! - CHECK and FOREIGN KEY constraints: disabled (`NOCHECK`), enabled but
//!   not trusted (added `WITH NOCHECK`), or trusted; and `NOT FOR
//!   REPLICATION`. The clone's get the original's state ([`constraints`]):
//!   a disabled CHECK that came out enabled would reject rows the
//!   original takes.
//! - The IDENTITY's counter when the rows copied leave it where SQL Server
//!   can't tell ([`resync_identity`]): `DBCC CHECKIDENT … RESEED, v` gives
//!   `v + increment` next on a table that ever had a row, but `v` itself on
//!   one that never did (an empty clone of a table whose rows were
//!   deleted).
//! - Sparse columns, the column set and system-versioned (temporal)
//!   tables, reported as plain columns ([`extras`], [`patch_sparse`],
//!   [`make_temporal`]).
//! - The full-text index's KEY INDEX: the clone's own key index
//!   ([`rename_key_index`], [`resolve_key_index`]); a secondary XML
//!   index's primary XML index: the clone's ([`rename_key_index`]).

use super::{exec, strings, Rename};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{DdlParts, Driver, Error, IndexDef, Result, Session, TableSchema};

/// The option of a full-text index that names the table's key index.
const KEY_INDEX: &str = "KEY INDEX";

fn is_fulltext(ix: &IndexDef) -> bool {
    ix.kind.as_deref().is_some_and(|k| k.trim().eq_ignore_ascii_case("FULLTEXT"))
}

/// SQL Server's full-text index: one per table, with no name of its own.
pub(super) fn unnamed_index(dialect: &str, ix: &IndexDef) -> bool {
    dialect == "mssql" && is_fulltext(ix)
}

/// The option of a secondary XML index that names its primary XML index.
const PRIMARY_XML_INDEX: &str = "primary_xml_index";

/// Options that name another index of the table (a full-text index's KEY
/// INDEX, a secondary XML index's primary XML index), renamed like the
/// index they name (a rename from a name collision included).
pub(super) fn rename_key_index(t: &mut TableSchema, renames: &[Rename]) {
    for ix in &mut t.indexes {
        let key = if is_fulltext(ix) { KEY_INDEX } else { PRIMARY_XML_INDEX };
        if let Some(k) = ix.options.get_mut(key) {
            if let Some(r) = renames.iter().find(|r| r.from.eq_ignore_ascii_case(k)) {
                *k = r.to.clone();
            }
        }
    }
}

/// A full-text index whose KEY INDEX is the original's primary key named
/// by the server (`PK__t__…`): the clone's is named by the server too, so
/// its name is read once the clone exists. True when it changed.
pub(super) async fn resolve_key_index(s: &mut dyn Session, clone: &mut TableSchema) -> Result<bool> {
    let known: Vec<String> = clone.indexes.iter().map(|i| i.name.clone()).chain(clone.primary_key.iter().filter_map(|p| p.name.clone())).collect();
    let pending = clone
        .indexes
        .iter()
        .any(|ix| is_fulltext(ix) && ix.options.get(KEY_INDEX).is_some_and(|k| !known.iter().any(|n| n.eq_ignore_ascii_case(k))));
    if !pending {
        return Ok(false);
    }
    let sql = format!(
        "SELECT name FROM sys.key_constraints WHERE parent_object_id = OBJECT_ID(N'{}') AND type = 'PK'",
        literal(clone)
    );
    let pk = strings(s, &sql).await.ok().and_then(|r| r.into_iter().next()).and_then(|r| r.into_iter().next().flatten());
    let Some(pk) = pk.filter(|_| clone.primary_key.as_ref().is_some_and(|p| p.name.is_none())) else {
        return Err(Error::State(
            "el índice de texto completo del original usa como clave un índice que el clon no tiene; no se clona".into(),
        ));
    };
    for ix in clone.indexes.iter_mut().filter(|ix| is_fulltext(ix)) {
        if let Some(k) = ix.options.get_mut(KEY_INDEX) {
            if !known.iter().any(|n| n.eq_ignore_ascii_case(k)) {
                *k = pk.clone();
            }
        }
    }
    Ok(true)
}

fn table_name(t: &TableSchema) -> String {
    qualified_name(Quote::Bracket, t.schema.as_deref().filter(|s| !s.is_empty()), &t.name)
}

fn literal(t: &TableSchema) -> String {
    table_name(t).replace('\'', "''")
}

/// The foreign keys' `ALTER TABLE … ADD`, as `WITH NOCHECK`: the original's
/// rows may not satisfy a disabled or untrusted key; the trusted ones are
/// validated afterwards by [`constraints`].
pub(super) fn foreign_keys_unchecked(sql: &str, clone: &TableSchema) -> String {
    let head = format!("ALTER TABLE {} ADD ", table_name(clone));
    let with = format!("ALTER TABLE {} WITH NOCHECK ADD ", table_name(clone));
    sql.replace(&head, &with)
}

/// Before the rows: the clone's CHECKs off, so rows the original keeps
/// against a disabled or untrusted CHECK load too. [`constraints`] puts
/// each one back as the original has it.
pub(super) async fn checks_off(s: &mut dyn Session, clone: &TableSchema) -> Result<()> {
    exec(s, &format!("ALTER TABLE {} NOCHECK CONSTRAINT ALL", table_name(clone)))
        .await
        .map_err(|_| Error::State("no se pudieron desactivar las restricciones CHECK del clon para copiar las filas; no se clona".into()))
}

/// A CHECK or FOREIGN KEY constraint and its state.
#[derive(Debug, Clone, PartialEq)]
struct Con {
    /// `C` or `F`.
    kind: String,
    name: String,
    system_named: bool,
    disabled: bool,
    untrusted: bool,
    not_for_replication: bool,
    /// What it is, name aside (a CHECK's definition and column; a key's
    /// columns and the table it points to, empty for the table itself).
    key: String,
    /// A CHECK's definition.
    definition: String,
}

fn constraints_sql(t: &TableSchema) -> String {
    let lit = literal(t);
    let flag = |c: &str| format!("CAST(CAST({c} AS int) AS nvarchar(1))");
    let cols = |col: &str| {
        format!(
            "STUFF((SELECT N',' + COL_NAME(c.parent_object_id, c.parent_column_id) + N'>' + COL_NAME(c.referenced_object_id, c.referenced_column_id) \
             FROM sys.foreign_key_columns c WHERE c.constraint_object_id = {col} ORDER BY c.constraint_column_id FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 1, N'')"
        )
    };
    format!(
        "SELECT N'C', name, {}, {}, {}, {}, definition + N'|' + ISNULL(COL_NAME(parent_object_id, parent_column_id), N''), definition \
         FROM sys.check_constraints WHERE parent_object_id = OBJECT_ID(N'{lit}') \
         UNION ALL SELECT N'F', fk.name, {}, {}, {}, {}, {} + N'@' + CASE WHEN fk.referenced_object_id = fk.parent_object_id THEN N'' \
         ELSE CAST(fk.referenced_object_id AS nvarchar(20)) END, N'' FROM sys.foreign_keys fk WHERE fk.parent_object_id = OBJECT_ID(N'{lit}')",
        flag("is_system_named"),
        flag("is_disabled"),
        flag("is_not_trusted"),
        flag("is_not_for_replication"),
        flag("fk.is_system_named"),
        flag("fk.is_disabled"),
        flag("fk.is_not_trusted"),
        flag("fk.is_not_for_replication"),
        cols("fk.object_id"),
    )
}

async fn read_constraints(s: &mut dyn Session, t: &TableSchema) -> Result<Vec<Con>> {
    let rows = strings(s, &constraints_sql(t))
        .await
        .map_err(|_| Error::State("no se pudo leer el estado de las restricciones CHECK y claves foráneas; no se clona".into()))?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let v = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            let b = |i: usize| v(i) == "1";
            (!v(1).is_empty()).then(|| Con {
                kind: v(0),
                name: v(1),
                system_named: b(2),
                disabled: b(3),
                untrusted: b(4),
                not_for_replication: b(5),
                key: v(6),
                definition: v(7),
            })
        })
        .collect())
}

/// Each original constraint and the clone's that stands for it: the
/// renamed name, or (named by the server) the same definition.
fn pair<'a>(original: &'a [Con], clone: &'a [Con], renames: &[Rename]) -> std::result::Result<Vec<(&'a Con, &'a Con)>, String> {
    let mut used = vec![false; clone.len()];
    let mut out = Vec::new();
    for o in original {
        let found = if o.system_named {
            clone.iter().enumerate().position(|(i, c)| !used[i] && c.system_named && c.kind == o.kind && c.key == o.key)
        } else {
            let to = renames.iter().find(|r| r.from == o.name).map(|r| r.to.as_str()).unwrap_or(&o.name);
            clone.iter().enumerate().position(|(i, c)| !used[i] && c.kind == o.kind && c.name.eq_ignore_ascii_case(to))
        };
        match found {
            Some(i) => {
                used[i] = true;
                out.push((o, &clone[i]));
            }
            None => return Err(o.name.clone()),
        }
    }
    Ok(out)
}

fn what(c: &Con) -> &'static str {
    if c.kind == "C" { "la restricción CHECK" } else { "la clave foránea" }
}

/// The statements that give `c` (the clone's) `o`'s state (enabled,
/// trusted); none when it already has it.
fn state_sql(table: &str, o: &Con, c: &Con) -> Option<String> {
    if (o.disabled, o.untrusted) == (c.disabled, c.untrusted) {
        return None;
    }
    let n = quote_ident(Quote::Bracket, &c.name);
    Some(match (o.disabled, o.untrusted) {
        (true, _) => format!("ALTER TABLE {table} NOCHECK CONSTRAINT {n}"),
        // Enabled without checking the rows: not trusted.
        (false, true) => format!("ALTER TABLE {table} NOCHECK CONSTRAINT {n};\nALTER TABLE {table} CHECK CONSTRAINT {n}"),
        (false, false) => format!("ALTER TABLE {table} WITH CHECK CHECK CONSTRAINT {n}"),
    })
}

/// A FOREIGN KEY of the clone, as `ADD` writes it (from the catalog).
async fn foreign_key_clause(s: &mut dyn Session, clone: &TableSchema, name: &str) -> Result<String> {
    let cols = |side: &str| {
        format!(
            "STUFF((SELECT N', ' + QUOTENAME(COL_NAME(c.{side}_object_id, c.{side}_column_id)) FROM sys.foreign_key_columns c \
             WHERE c.constraint_object_id = fk.object_id ORDER BY c.constraint_column_id FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, N'')"
        )
    };
    let sql = format!(
        "SELECT {}, QUOTENAME(OBJECT_SCHEMA_NAME(fk.referenced_object_id)) + N'.' + QUOTENAME(OBJECT_NAME(fk.referenced_object_id)), {}, \
         fk.delete_referential_action_desc, fk.update_referential_action_desc FROM sys.foreign_keys fk \
         WHERE fk.parent_object_id = OBJECT_ID(N'{}') AND fk.name = N'{}'",
        cols("parent"),
        cols("referenced"),
        literal(clone),
        name.replace('\'', "''")
    );
    let r = strings(s, &sql).await.ok().and_then(|r| r.into_iter().next()).unwrap_or_default();
    let v = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
    if v(0).is_empty() || v(1).is_empty() || v(2).is_empty() {
        return Err(Error::State(format!("no se pudo leer la clave foránea «{name}» del clon; no se clona")));
    }
    let action = |what: &str, a: String| match a.as_str() {
        "" | "NO_ACTION" => String::new(),
        a => format!(" ON {what} {}", a.replace('_', " ")),
    };
    Ok(format!("FOREIGN KEY ({}) REFERENCES {} ({}){}{}", v(0), v(1), v(2), action("DELETE", v(3)), action("UPDATE", v(4))))
}

/// The statements that remake `c` with (or without) `NOT FOR
/// REPLICATION`, as `o` has it (added `WITH NOCHECK`; its state is set
/// after). The server names it again when it named the original's.
async fn replication_sql(s: &mut dyn Session, clone: &TableSchema, o: &Con, c: &Con) -> Result<String> {
    let table = table_name(clone);
    let named = if o.system_named { String::new() } else { format!("CONSTRAINT {} ", quote_ident(Quote::Bracket, &c.name)) };
    let body = if c.kind == "C" {
        format!("CHECK {}{}", if o.not_for_replication { "NOT FOR REPLICATION " } else { "" }, c.definition)
    } else {
        format!("{}{}", foreign_key_clause(s, clone, &c.name).await?, if o.not_for_replication { " NOT FOR REPLICATION" } else { "" })
    };
    Ok(format!(
        "ALTER TABLE {table} DROP CONSTRAINT {};\nALTER TABLE {table} WITH NOCHECK ADD {named}{body}",
        quote_ident(Quote::Bracket, &c.name)
    ))
}

/// The clone's CHECK and FOREIGN KEY constraints as the original's:
/// disabled, not trusted or trusted, and NOT FOR REPLICATION. Checked
/// afterwards; a clone that can't have them is refused.
pub(super) async fn constraints(src: &mut dyn Session, tgt: &mut dyn Session, original: &TableSchema, clone: &TableSchema, renames: &[Rename]) -> Result<()> {
    let wanted = read_constraints(src, original).await?;
    if wanted.is_empty() {
        return Ok(());
    }
    let missing = |n: String| Error::State(format!("el clon no quedó igual al original: le falta la restricción «{n}»; no se clona"));
    let table = table_name(clone);
    // NOT FOR REPLICATION: the constraint is made again.
    let have = read_constraints(tgt, clone).await?;
    let mut remake = Vec::new();
    for (o, c) in pair(&wanted, &have, renames).map_err(missing)? {
        if o.not_for_replication != c.not_for_replication {
            remake.push(replication_sql(tgt, clone, o, c).await?);
        }
    }
    for sql in remake {
        exec(tgt, &sql)
            .await
            .map_err(|_| Error::State("no se pudo dar a las restricciones del clon el NOT FOR REPLICATION del original; no se clona".into()))?;
    }
    // Enabled or not, trusted or not.
    let have = read_constraints(tgt, clone).await?;
    for (o, c) in pair(&wanted, &have, renames).map_err(missing)? {
        if let Some(sql) = state_sql(&table, o, c) {
            exec(tgt, &sql).await.map_err(|_| {
                Error::State(if !o.disabled && !o.untrusted {
                    format!(
                        "las filas del clon no cumplen {} «{}», que en el original está validada (¿cambiaron durante la copia los datos a los que apunta?); no se clona",
                        what(o),
                        c.name
                    )
                } else {
                    format!("no se pudo dejar {} «{}» del clon como la del original; no se clona", what(o), c.name)
                })
            })?;
        }
    }
    let have = read_constraints(tgt, clone).await?;
    for (o, c) in pair(&wanted, &have, renames).map_err(missing)? {
        let state = |x: &Con| (x.disabled, x.untrusted, x.not_for_replication);
        if state(o) != state(c) {
            let desc = |x: &Con| {
                let mut s = String::from(if x.disabled { "desactivada" } else if x.untrusted { "activa sin validar" } else { "activa y validada" });
                if x.not_for_replication {
                    s.push_str(", NOT FOR REPLICATION");
                }
                s
            };
            return Err(Error::State(format!(
                "el clon no quedó igual al original: {} «{}» está {} en el original y {} en el clon; no se clona",
                what(o),
                o.name,
                desc(o),
                desc(c)
            )));
        }
    }
    Ok(())
}

/// An IDENTITY's counter: the current value and whether the table ever
/// had a row (`last_value` is NULL until then).
async fn identity_state(s: &mut dyn Session, t: &TableSchema) -> Result<Option<(i128, i128, bool)>> {
    let lit = literal(t);
    let sql = format!(
        "SELECT CAST(IDENT_CURRENT(N'{lit}') AS nvarchar(50)), CAST(IDENT_INCR(N'{lit}') AS nvarchar(50)), \
         (SELECT CAST(COUNT(*) AS nvarchar(5)) FROM sys.identity_columns WHERE object_id = OBJECT_ID(N'{lit}') AND last_value IS NOT NULL)"
    );
    let r = strings(s, &sql).await.map_err(|_| Error::State("no se pudo leer el contador del IDENTITY; no se clona".into()))?;
    let Some(r) = r.into_iter().next() else { return Ok(None) };
    let v = |i: usize| r.get(i).cloned().flatten().map(|x| x.trim().to_string());
    let (Some(cur), Some(inc)) = (v(0), v(1)) else { return Ok(None) };
    let num = |x: &str| {
        x.parse::<i128>().map_err(|_| Error::State(format!("el motor informó un valor de IDENTITY que no se entiende («{x}»); no se clona")))
    };
    Ok(Some((num(&cur)?, num(&inc)?, v(2).as_deref() != Some("0"))))
}

/// The value the next insert gets: the current one itself on a table that
/// never had a row, the one after it otherwise.
fn next_value(current: i128, increment: i128, used: bool) -> Option<i128> {
    if used { current.checked_add(increment) } else { Some(current) }
}

/// The RESEED value that makes the clone's next insert `next`.
fn reseed_value(next: i128, increment: i128, clone_used: bool) -> Option<i128> {
    if clone_used { next.checked_sub(increment) } else { Some(next) }
}

/// The clone's IDENTITY continues where the original's does (past its
/// largest value after deletes, also when no row was copied). Checked
/// afterwards.
pub(super) async fn resync_identity(src: &mut dyn Session, tgt: &mut dyn Session, original: &TableSchema, clone: &TableSchema) -> Result<()> {
    let Some((current, increment, used)) = identity_state(src, original).await? else { return Ok(()) };
    let Some((_, _, clone_used)) = identity_state(tgt, clone).await? else { return Ok(()) };
    let next = next_value(current, increment, used);
    let reseed = next.and_then(|n| reseed_value(n, increment, clone_used));
    let failed = || {
        Error::State(format!(
            "no se pudo dejar el IDENTITY del clon donde está el del original (su valor actual es {current}, cerca del límite de su tipo); no se clona"
        ))
    };
    let Some(reseed) = reseed else { return Err(failed()) };
    exec(tgt, &format!("DBCC CHECKIDENT (N'{}', RESEED, {reseed}) WITH NO_INFOMSGS", literal(clone))).await.map_err(|_| failed())?;
    let Some((c2, i2, u2)) = identity_state(tgt, clone).await? else { return Err(failed()) };
    if next_value(c2, i2, u2) != next {
        let v = |n: Option<i128>| n.map(|n| n.to_string()).unwrap_or_else(|| "ninguno".into());
        return Err(Error::State(format!(
            "el clon no quedó igual al original: su IDENTITY daría {} a la próxima fila y el del original {}; no se clona",
            v(next_value(c2, i2, u2)),
            v(next)
        )));
    }
    Ok(())
}

/// The primary key's index as the catalog has it, name aside: what
/// `KeyDef` doesn't carry (clustering, each key column's order, the
/// index's options, compression and where it's stored).
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct PrimaryKey {
    /// `CLUSTERED`, `NONCLUSTERED`, `NONCLUSTERED HASH`…
    kind: String,
    /// Key columns in order, and whether each one is descending.
    keys: Vec<(String, bool)>,
    fill_factor: u8,
    padded: bool,
    ignore_dup_key: bool,
    row_locks: bool,
    page_locks: bool,
    no_recompute: bool,
    sequential_key: bool,
    /// Each partition's compression, in order.
    compression: Vec<String>,
    /// Filegroup or partition scheme (with its column) when it isn't the
    /// default filegroup.
    data_space: Option<(String, Option<String>)>,
}

impl PrimaryKey {
    /// The table's rows are the key's index (its `ON` is the table's).
    pub(super) fn clustered(&self) -> bool {
        self.kind == "CLUSTERED"
    }
}

fn is_rowstore(kind: &str) -> bool {
    matches!(kind, "CLUSTERED" | "NONCLUSTERED")
}

/// A table's primary key (`None`: it has none). An error when the catalog
/// can't be read.
pub(super) async fn primary_key(s: &mut dyn Session, t: &TableSchema) -> Result<Option<PrimaryKey>> {
    let lit = literal(t);
    let failed = || Error::State("no se pudo leer la definición de la clave primaria; no se clona".into());
    let flag = |c: &str| format!("CAST(CAST({c} AS int) AS nvarchar(1))");
    let join = "JOIN sys.key_constraints k ON k.parent_object_id = x.object_id AND k.unique_index_id = x.index_id AND k.type = 'PK'";
    let head = format!(
        "SELECT x.type_desc, CAST(x.fill_factor AS nvarchar(5)), {}, {}, {}, {}, {}, ds.name, ds.type, {}, \
         (SELECT TOP 1 CAST(t.type AS nvarchar(2)) FROM sys.indexes i0 JOIN sys.data_spaces t ON t.data_space_id = i0.data_space_id \
         WHERE i0.object_id = x.object_id AND i0.index_id IN (0, 1)) FROM sys.indexes x {join} \
         LEFT JOIN sys.stats st ON st.object_id = x.object_id AND st.stats_id = x.index_id \
         LEFT JOIN sys.data_spaces ds ON ds.data_space_id = x.data_space_id WHERE x.object_id = OBJECT_ID(N'{lit}')",
        flag("x.is_padded"),
        flag("x.ignore_dup_key"),
        flag("x.allow_row_locks"),
        flag("x.allow_page_locks"),
        flag("ISNULL(st.no_recompute, 0)"),
        flag("ISNULL(ds.is_default, 1)"),
    );
    let Some(r) = strings(s, &head).await.map_err(|_| failed())?.into_iter().next() else { return Ok(None) };
    let v = |i: usize| r.get(i).cloned().flatten().unwrap_or_default().trim().to_string();
    let b = |i: usize| v(i) == "1";
    let mut p = PrimaryKey {
        kind: v(0).replace('_', " "),
        fill_factor: v(1).parse().unwrap_or(0),
        padded: b(2),
        ignore_dup_key: b(3),
        row_locks: b(4),
        page_locks: b(5),
        no_recompute: b(6),
        ..Default::default()
    };
    // On a partitioned table, a key on the default filegroup says so: left
    // without ON, it would be partitioned with the table.
    let (space, scheme, default) = (v(7), v(8) == "PS", b(9) && v(10) != "PS");
    let cols = format!(
        "SELECT c.name, {}, CAST(x.key_ordinal AS nvarchar(5)), CAST(x.partition_ordinal AS nvarchar(5)) FROM sys.index_columns x {join} \
         JOIN sys.columns c ON c.object_id = x.object_id AND c.column_id = x.column_id \
         WHERE x.object_id = OBJECT_ID(N'{lit}') ORDER BY x.key_ordinal",
        flag("x.is_descending_key")
    );
    let mut partition_column = None;
    for r in strings(s, &cols).await.map_err(|_| failed())? {
        let v = |i: usize| r.get(i).cloned().flatten().unwrap_or_default().trim().to_string();
        if v(2) != "0" {
            p.keys.push((v(0), v(1) == "1"));
        }
        if v(3) == "1" {
            partition_column = Some(v(0));
        }
    }
    let parts = format!(
        "SELECT x.data_compression_desc FROM sys.partitions x {join} WHERE x.object_id = OBJECT_ID(N'{lit}') ORDER BY x.partition_number"
    );
    p.compression = strings(s, &parts).await.map_err(|_| failed())?.into_iter().map(|r| r.into_iter().next().flatten().unwrap_or_default()).collect();
    if !default && !space.is_empty() {
        p.data_space = Some((space, if scheme { Some(partition_column.unwrap_or_default()) } else { None }));
    }
    // SQL Server 2019 on: before, the column doesn't exist (and it's off).
    let seq = format!("SELECT {} FROM sys.indexes x {join} WHERE x.object_id = OBJECT_ID(N'{lit}')", flag("x.optimize_for_sequential_key"));
    p.sequential_key = strings(s, &seq).await.ok().and_then(|r| r.into_iter().next()).and_then(|r| r.into_iter().next().flatten()).as_deref() == Some("1");
    Ok(Some(p))
}

/// `DATA_COMPRESSION = …` options for each partition's compression (one
/// for all when they're the same; none when nothing is compressed).
fn compression_options(compression: &[String]) -> Vec<String> {
    let compressed: Vec<(usize, &String)> = compression.iter().enumerate().filter(|(_, c)| c.as_str() != "NONE").collect();
    if compressed.is_empty() {
        Vec::new()
    } else if compression.iter().all(|c| *c == compression[0]) {
        vec![format!("DATA_COMPRESSION = {}", compression[0])]
    } else {
        compressed.iter().map(|(i, c)| format!("DATA_COMPRESSION = {c} ON PARTITIONS ({})", i + 1)).collect()
    }
}

/// `PRIMARY KEY …` as `p` is (without `CONSTRAINT name`).
fn key_clause(p: &PrimaryKey) -> String {
    let q = |c: &str| quote_ident(Quote::Bracket, c);
    let keys: Vec<String> = p.keys.iter().map(|(c, d)| format!("{}{}", q(c), if *d { " DESC" } else { "" })).collect();
    let mut with: Vec<String> = Vec::new();
    if p.padded {
        with.push("PAD_INDEX = ON".into());
    }
    if p.fill_factor > 0 {
        with.push(format!("FILLFACTOR = {}", p.fill_factor));
    }
    if p.ignore_dup_key {
        with.push("IGNORE_DUP_KEY = ON".into());
    }
    if p.no_recompute {
        with.push("STATISTICS_NORECOMPUTE = ON".into());
    }
    if !p.row_locks {
        with.push("ALLOW_ROW_LOCKS = OFF".into());
    }
    if !p.page_locks {
        with.push("ALLOW_PAGE_LOCKS = OFF".into());
    }
    if p.sequential_key {
        with.push("OPTIMIZE_FOR_SEQUENTIAL_KEY = ON".into());
    }
    with.extend(compression_options(&p.compression));
    let with = if with.is_empty() { String::new() } else { format!(" WITH ({})", with.join(", ")) };
    let on = match &p.data_space {
        Some((ds, Some(col))) => format!(" ON {}({})", q(ds), q(col)),
        Some((ds, None)) => format!(" ON {}", q(ds)),
        None => String::new(),
    };
    format!("PRIMARY KEY {} ({}){with}{on}", p.kind, keys.join(", "))
}

/// A primary key the clone can't be made with: refused before creating
/// anything.
pub(super) fn check_primary_key(p: &PrimaryKey) -> Result<()> {
    if !is_rowstore(&p.kind) {
        return Err(Error::Unsupported(format!(
            "la clave primaria del original es de tipo {}, que el clon no puede reproducir; no se clona",
            p.kind
        )));
    }
    if matches!(&p.data_space, Some((_, Some(c))) if c.is_empty()) {
        return Err(Error::Unsupported(
            "no se pudo leer la columna de partición de la clave primaria del original; no se clona".into(),
        ));
    }
    Ok(())
}

/// The clone's CREATE with the original's primary key as it is: the
/// generic `PRIMARY KEY (…)` line becomes `PRIMARY KEY CLUSTERED |
/// NONCLUSTERED (… DESC) WITH (…) ON …`. False when the CREATE has no
/// such line (Fabric adds its keys apart): the check after creating says
/// whether the clone's came out the same.
pub(super) fn patch_primary_key(create: &mut String, clone: &TableSchema, p: &PrimaryKey) -> Result<bool> {
    let Some(k) = clone.primary_key.as_ref().filter(|k| !k.columns.is_empty()) else { return Ok(false) };
    let same = k.columns.len() == p.keys.len() && k.columns.iter().zip(&p.keys).all(|(a, (b, _))| a.eq_ignore_ascii_case(b));
    if !same {
        return Err(Error::State("las columnas de la clave primaria del original no coinciden con las que informa el catálogo; no se clona".into()));
    }
    let q = |c: &str| quote_ident(Quote::Bracket, c);
    let cols: Vec<String> = k.columns.iter().map(|c| q(c)).collect();
    let prefix = match k.name.as_deref().filter(|n| !n.is_empty()) {
        Some(n) => format!("\n    CONSTRAINT {} ", q(n)),
        None => "\n    ".into(),
    };
    let mut found: Vec<(usize, usize)> = Vec::new();
    for kind in ["", "NONCLUSTERED "] {
        let line = format!("{prefix}PRIMARY KEY {kind}({})", cols.join(", "));
        for (i, _) in create.match_indices(&line) {
            let rest = &create[i + line.len()..];
            if rest.starts_with(",\n") || rest.starts_with("\n)") {
                found.push((i, line.len()));
            }
        }
    }
    match found.as_slice() {
        [] => Ok(false),
        [(i, len)] => {
            create.replace_range(*i..*i + *len, &format!("{prefix}{}", key_clause(p)));
            Ok(true)
        }
        _ => Err(Error::State("no se pudo ubicar la clave primaria en el CREATE del clon; no se clona".into())),
    }
}

/// The clone's primary key, once created, against the original's.
pub(super) async fn verify_primary_key(s: &mut dyn Session, clone: &TableSchema, original: &PrimaryKey) -> Result<()> {
    let have = primary_key(s, clone).await?;
    let o = original;
    let mut c = have.clone().unwrap_or_default();
    // Column names as the catalog spells them (the clone's are the same).
    for ((a, _), (b, _)) in o.keys.iter().zip(c.keys.iter_mut()) {
        if a.eq_ignore_ascii_case(b) {
            *b = a.clone();
        }
    }
    if have.is_none() || *o != c {
        return Err(Error::State(format!(
            "el clon no quedó igual al original: su clave primaria es «{}» y la del original «{}»; no se clona",
            if have.is_some() { key_clause(&c) } else { "ninguna".into() },
            key_clause(&o)
        )));
    }
    Ok(())
}

// --- Where the table and its indexes are stored --------------------------
//
// `IndexDef` doesn't say where an index is (filegroup, partition scheme),
// nor the table where its heap is or its LOB columns: the clone's CREATE
// and each index's statement get the original's `ON …` ([`place_table`],
// [`place_indexes`]), and the clone is checked against the original
// ([`verify_layout`]). The primary key's is [`PrimaryKey::data_space`].

/// A filegroup, or a partition scheme with its partitioning column.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Space {
    name: String,
    /// The partitioning column, on a partition scheme.
    column: Option<String>,
    /// The database's default filegroup.
    default: bool,
}

impl Space {
    fn on(&self) -> String {
        let q = |c: &str| quote_ident(Quote::Bracket, c);
        match &self.column {
            Some(c) => format!(" ON {}({})", q(&self.name), q(c)),
            None => format!(" ON {}", q(&self.name)),
        }
    }

    fn describe(&self) -> String {
        match &self.column {
            Some(c) => format!("el esquema de partición {}({c})", self.name),
            None if self.name.is_empty() => "el filegroup por omisión".into(),
            None => format!("el filegroup {}", self.name),
        }
    }

    fn same(&self, o: &Space) -> bool {
        self.name.eq_ignore_ascii_case(&o.name) && self.column.as_deref().map(str::to_lowercase) == o.column.as_deref().map(str::to_lowercase)
    }
}

/// An index other than the primary key's: its position in the table's
/// `indexes`, its type (`sys.indexes.type`), where it is and each
/// partition's compression.
#[derive(Debug, Clone, PartialEq)]
struct Placed {
    at: usize,
    kind: u8,
    space: Space,
    compression: Vec<String>,
}

/// Where a table and its indexes are stored.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Layout {
    /// The heap's or the clustered index's.
    table: Option<Space>,
    /// The heap's compression, each partition's (empty: the table has a
    /// clustered index).
    heap_compression: Vec<String>,
    /// Where the LOB columns are (`TEXTIMAGE_ON`), when it has any.
    lob: Option<Space>,
    indexes: Vec<Placed>,
    /// Memory-optimized: none of this applies.
    memory: bool,
}

impl Layout {
    fn partitioned(&self) -> bool {
        self.table.as_ref().is_some_and(|s| s.column.is_some())
    }
}

/// How the clone's CREATE ends: `ON …` (the heap, or the clustered index
/// the heap becomes; not when the clustered primary key says it),
/// `TEXTIMAGE_ON …` and the heap's `WITH (DATA_COMPRESSION = …)`.
fn table_tail(l: &Layout, pk_clustered: bool) -> String {
    if l.memory {
        return String::new();
    }
    let mut tail = String::new();
    if let Some(t) = l.table.as_ref().filter(|t| !t.default && !t.name.is_empty() && !pk_clustered) {
        tail.push_str(&t.on());
    }
    let table_default = l.table.as_ref().is_none_or(|t| t.default);
    if let Some(lob) = l.lob.as_ref().filter(|b| !b.name.is_empty() && b.column.is_none() && !l.partitioned() && (!b.default || !table_default)) {
        tail.push_str(&format!(" TEXTIMAGE_ON {}", quote_ident(Quote::Bracket, &lob.name)));
    }
    let with = compression_options(&l.heap_compression);
    if !with.is_empty() {
        tail.push_str(&format!(" WITH ({})", with.join(", ")));
    }
    tail
}

/// The `ON …` of each index the clone creates with one: the original's
/// filegroup or scheme when it isn't the default filegroup, and always on
/// a partitioned table (left without it, an index follows the table's
/// scheme). A spatial index takes only a filegroup.
fn index_ons(l: &Layout) -> Vec<(usize, String)> {
    if l.memory {
        return Vec::new();
    }
    l.indexes
        .iter()
        .filter(|p| !p.space.name.is_empty() && (!p.space.default || l.partitioned()))
        .filter(|p| matches!(p.kind, 1 | 2 | 5 | 6) || (p.kind == 4 && p.space.column.is_none()))
        .map(|p| (p.at, p.space.on()))
        .collect()
}

async fn read_layout(s: &mut dyn Session, t: &TableSchema) -> Result<Layout> {
    let lit = literal(t);
    let failed = || Error::State("no se pudo leer dónde se guardan la tabla y sus índices (filegroups, esquemas de partición); no se clona".into());
    let mut l = Layout::default();
    // SQL Server 2014 on: is_memory_optimized.
    let table = |memory: bool| {
        format!(
            "SELECT ds.name, ds.type, CAST(ds.is_default AS nvarchar(1)), {} FROM sys.tables t \
             LEFT JOIN sys.data_spaces ds ON ds.data_space_id = t.lob_data_space_id AND t.lob_data_space_id <> 0 WHERE t.object_id = OBJECT_ID(N'{lit}')",
            if memory { "CAST(t.is_memory_optimized AS nvarchar(1))" } else { "N'0'" }
        )
    };
    let r = match strings(s, &table(true)).await {
        Ok(r) => r,
        Err(_) => strings(s, &table(false)).await.map_err(|_| failed())?,
    };
    if let Some(r) = r.into_iter().next() {
        let v = |i: usize| r.get(i).cloned().flatten().unwrap_or_default().trim().to_string();
        l.memory = v(3) == "1";
        if !v(0).is_empty() {
            l.lob = Some(Space { name: v(0), column: (v(1) == "PS").then(String::new), default: v(2) == "1" });
        }
    }
    let sql = format!(
        "SELECT x.name, CAST(x.index_id AS nvarchar(10)), CAST(x.type AS nvarchar(3)), CAST(x.is_primary_key AS nvarchar(1)), \
         ds.name, ds.type, CAST(ds.is_default AS nvarchar(1)), \
         (SELECT TOP 1 c.name FROM sys.index_columns ic JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE ic.object_id = x.object_id AND ic.index_id = x.index_id AND ic.partition_ordinal = 1), \
         STUFF((SELECT N',' + p.data_compression_desc FROM sys.partitions p WHERE p.object_id = x.object_id AND p.index_id = x.index_id \
         ORDER BY p.partition_number FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 1, N'') \
         FROM sys.indexes x LEFT JOIN sys.data_spaces ds ON ds.data_space_id = x.data_space_id \
         WHERE x.object_id = OBJECT_ID(N'{lit}') AND x.is_hypothetical = 0 ORDER BY x.index_id"
    );
    for r in strings(s, &sql).await.map_err(|_| failed())? {
        let v = |i: usize| r.get(i).cloned().flatten().unwrap_or_default().trim().to_string();
        let (id, kind): (u32, u8) = (v(1).parse().unwrap_or(u32::MAX), v(2).parse().unwrap_or(u8::MAX));
        let space = Space { name: v(4), column: (v(5) == "PS").then(|| v(7)), default: v(6) == "1" || v(4).is_empty() };
        let compression: Vec<String> = v(8).split(',').map(str::trim).filter(|c| !c.is_empty()).map(str::to_string).collect();
        if id <= 1 {
            l.table = Some(space.clone());
            if id == 0 {
                l.heap_compression = compression;
                continue;
            }
            if v(3) == "1" {
                continue;
            }
        } else if v(3) == "1" {
            continue;
        }
        if let Some(at) = t.indexes.iter().position(|i| i.name == v(0)) {
            l.indexes.push(Placed { at, kind, space, compression });
        }
    }
    // A partitioned table's LOB columns follow its scheme.
    if let (Some(lob), Some(c)) = (l.lob.as_mut(), l.table.as_ref().and_then(|t| t.column.clone())) {
        if lob.column.is_some() {
            lob.column = Some(c);
        }
    }
    Ok(l)
}

/// The original's layout (`None`: the catalog can't be read), refused
/// when the clone can't have it.
pub(super) async fn layout(s: &mut dyn Session, t: &TableSchema) -> Result<Option<Layout>> {
    let Ok(l) = read_layout(s, t).await else { return Ok(None) };
    check_layout(&l, t)?;
    Ok(Some(l))
}

fn check_layout(l: &Layout, t: &TableSchema) -> Result<()> {
    if l.memory {
        return Ok(());
    }
    let unread = |what: &str| {
        Error::Unsupported(format!("no se pudo leer la columna de partición de {what} del original; no se clona"))
    };
    if l.table.as_ref().is_some_and(|s| s.column.as_deref() == Some("")) {
        return Err(unread("la tabla"));
    }
    for p in &l.indexes {
        let name = t.indexes.get(p.at).map(|i| i.name.as_str()).unwrap_or_default();
        if p.space.column.as_deref() == Some("") {
            return Err(unread(&format!("el índice «{name}»")));
        }
        // A spatial index takes only a filegroup: on a scheme, only the
        // table's (it follows the table's partitioning).
        if p.kind == 4 && p.space.column.is_some() && !l.table.as_ref().is_some_and(|t| t.same(&p.space)) {
            return Err(Error::Unsupported(format!(
                "el índice espacial «{name}» del original está en {}, distinto del de la tabla, y el clon no puede reproducirlo; no se clona",
                p.space.describe()
            )));
        }
    }
    Ok(())
}

/// The clone's CREATE ends with the original's storage ([`table_tail`]).
pub(super) fn place_table(create: &mut String, l: &Layout, pk_clustered: bool) -> Result<()> {
    let tail = table_tail(l, pk_clustered);
    if tail.is_empty() {
        return Ok(());
    }
    let Some(i) = create.find("\n);") else {
        return Err(Error::State("no se pudo ubicar el final del CREATE TABLE del clon para indicar dónde se guarda; no se clona".into()));
    };
    create.replace_range(i..i + 3, &format!("\n){tail};"));
    Ok(())
}

/// Each index's statement in `sql` (the clone's, as the driver writes
/// them) gets the original's `ON …`.
pub(super) fn place_indexes(driver: &dyn Driver, sql: &mut String, clone: &TableSchema, l: &Layout) -> Result<()> {
    for (at, on) in index_ons(l) {
        let Some(ix) = clone.indexes.get(at) else { continue };
        let lost = || Error::State(format!("no se pudo indicar dónde se guarda el índice «{}» del clon; no se clona", ix.name));
        let one = TableSchema { schema: clone.schema.clone(), name: clone.name.clone(), indexes: vec![ix.clone()], ..Default::default() };
        let stmt = driver.table_ddl(&one, DdlParts { indexes: true, ..Default::default() })?;
        let stmt = stmt.trim();
        let Some(body) = stmt.strip_suffix(';') else { return Err(lost()) };
        if sql.matches(stmt).count() != 1 {
            return Err(lost());
        }
        *sql = sql.replacen(stmt, &format!("{body}{on};"), 1);
    }
    Ok(())
}

/// The clone's layout against the original's: the table's (heap or
/// clustered index, LOB columns) always, its indexes' when it has them.
pub(super) async fn verify_layout(s: &mut dyn Session, clone: &TableSchema, original: &Layout, indexes: bool) -> Result<()> {
    if original.memory {
        return Ok(());
    }
    let have = read_layout(s, clone).await?;
    let differ = |what: String, o: String, c: String| {
        Err(Error::State(format!("el clon no quedó igual al original: {what} está en {o} en el original y en {c} en el clon; no se clona")))
    };
    let space = |s: Option<&Space>| s.map(Space::describe).unwrap_or_else(|| "el filegroup por omisión".into());
    let same = |a: Option<&Space>, b: Option<&Space>| match (a, b) {
        (Some(a), Some(b)) => a.same(b),
        (None, None) => true,
        _ => false,
    };
    if !same(original.table.as_ref(), have.table.as_ref()) {
        return differ("la tabla".into(), space(original.table.as_ref()), space(have.table.as_ref()));
    }
    if !same(original.lob.as_ref(), have.lob.as_ref()) {
        return differ("el contenido de sus columnas grandes (TEXTIMAGE_ON)".into(), space(original.lob.as_ref()), space(have.lob.as_ref()));
    }
    let list = |c: &[String]| c.join(", ");
    if !original.heap_compression.is_empty() && !have.heap_compression.is_empty() && original.heap_compression != have.heap_compression {
        return Err(Error::State(format!(
            "el clon no quedó igual al original: la compresión de sus particiones es {} en el original y {} en el clon; no se clona",
            list(&original.heap_compression),
            list(&have.heap_compression)
        )));
    }
    if !indexes {
        return Ok(());
    }
    for o in &original.indexes {
        let name = clone.indexes.get(o.at).map(|i| i.name.clone()).unwrap_or_default();
        let Some(c) = have.indexes.iter().find(|c| c.at == o.at) else {
            return Err(Error::State(format!("el clon no quedó igual al original: le falta el índice «{name}»; no se clona")));
        };
        if !o.space.same(&c.space) {
            return differ(format!("el índice «{name}»"), o.space.describe(), c.space.describe());
        }
        if o.compression != c.compression {
            return Err(Error::State(format!(
                "el clon no quedó igual al original: la compresión de las particiones del índice «{name}» es {} en el original y {} en el clon; no se clona",
                list(&o.compression),
                list(&c.compression)
            )));
        }
    }
    Ok(())
}

/// A UNIQUE constraint of the original named by the server
/// (`UQ__t__…`): the clone's is named by the server too.
///
/// The clone's indexes but the full-text one are created now, with those
/// constraints added without a name; each one's server name is read back
/// (by its key columns) and put in the plan, and in the full-text index's
/// KEY INDEX when it names one of them. Returns the full-text index's
/// statement, still to run.
pub(super) async fn server_named_uniques(
    driver: &dyn dbine_driver::Driver,
    s: &mut dyn Session,
    clone: &mut TableSchema,
    placeholders: &[String],
    layout: Option<&Layout>,
) -> Result<Option<String>> {
    let owner = table_name(clone);
    let rest = TableSchema { indexes: clone.indexes.iter().filter(|i| !is_fulltext(i)).cloned().collect(), ..clone.clone() };
    let mut sql = driver.table_ddl(&rest, DdlParts { indexes: true, ..Default::default() })?;
    if let Some(l) = layout {
        place_indexes(driver, &mut sql, clone, l)?;
    }
    for n in placeholders {
        let named = format!("ALTER TABLE {owner} ADD CONSTRAINT {} UNIQUE", quote_ident(Quote::Bracket, n));
        if !sql.contains(&named) {
            return Err(Error::State(format!("no se pudo crear sin nombre la restricción única «{n}» del clon; no se clona")));
        }
        sql = sql.replace(&named, &format!("ALTER TABLE {owner} ADD UNIQUE"));
    }
    if !sql.trim().is_empty() {
        exec(s, &sql).await.map_err(|e| Error::Query(format!("índices: {}", super::in_spanish(e))))?;
    }
    // The clone's server-named UNIQUE constraints and their key columns.
    let lit = literal(clone);
    let read = format!(
        "SELECT k.name, STUFF((SELECT N',' + LOWER(COL_NAME(x.object_id, x.column_id)) + CASE WHEN x.is_descending_key = 1 THEN N' DESC' ELSE N'' END \
         FROM sys.index_columns x WHERE x.object_id = k.parent_object_id AND x.index_id = k.unique_index_id AND x.key_ordinal > 0 \
         ORDER BY x.key_ordinal FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 1, N'') \
         FROM sys.key_constraints k WHERE k.parent_object_id = OBJECT_ID(N'{lit}') AND k.type = 'UQ' AND k.is_system_named = 1"
    );
    let mut made: Vec<(String, String)> = strings(s, &read)
        .await
        .map_err(|_| Error::State("no se pudieron leer las restricciones únicas del clon; no se clona".into()))?
        .into_iter()
        .filter_map(|r| match r.as_slice() {
            [Some(n), Some(k)] => Some((n.clone(), k.clone())),
            _ => None,
        })
        .collect();
    let mut given: Vec<(String, String)> = Vec::new();
    for n in placeholders {
        let Some(ix) = clone.indexes.iter_mut().find(|i| i.name == *n) else { continue };
        let desc: Vec<String> = ix.options.get("desc").map(|d| d.split(',').map(|c| c.trim().to_lowercase()).collect()).unwrap_or_default();
        let key = ix
            .columns
            .iter()
            .map(|c| format!("{}{}", c.to_lowercase(), if desc.contains(&c.to_lowercase()) { " DESC" } else { "" }))
            .collect::<Vec<_>>()
            .join(",");
        let Some(at) = made.iter().position(|(_, k)| *k == key) else {
            return Err(Error::State(format!(
                "el clon no quedó igual al original: le falta la restricción única sobre ({}); no se clona",
                ix.columns.join(", ")
            )));
        };
        let (server, _) = made.remove(at);
        given.push((n.clone(), server.clone()));
        ix.name = server;
    }
    for ix in clone.indexes.iter_mut().filter(|i| is_fulltext(i)) {
        if let Some(k) = ix.options.get_mut(KEY_INDEX) {
            if let Some((_, server)) = given.iter().find(|(n, _)| n == k) {
                *k = server.clone();
            }
        }
    }
    let fulltext = TableSchema { indexes: clone.indexes.iter().filter(|i| is_fulltext(i)).cloned().collect(), ..clone.clone() };
    if fulltext.indexes.is_empty() {
        return Ok(None);
    }
    Ok(Some(driver.table_ddl(&fulltext, DdlParts { indexes: true, ..Default::default() })?).filter(|s| !s.trim().is_empty()))
}

// --- Graph tables (AS NODE / AS EDGE) ------------------------------------

/// Why a graph table (`AS NODE` / `AS EDGE`, SQL Server 2017 on) can't be
/// cloned: its `$node_id` / `$from_id` / `$to_id` are the engine's own
/// (internal columns no SELECT reads, ids no INSERT sets), so the clone's
/// nodes would be others and its edges would point at nothing.
pub(super) fn graph_reason(node: bool, edge: bool) -> Option<String> {
    let what = match (node, edge) {
        (true, _) => "de nodos (AS NODE)",
        (_, true) => "de aristas (AS EDGE)",
        _ => return None,
    };
    Some(format!(
        "no se puede clonar: es una tabla de grafo {what} de SQL Server; sus identificadores de nodo y arista ($node_id, $from_id, $to_id) los asigna el motor y el clon no puede reproducirlos"
    ))
}

/// Refuses a graph table before anything is read (the read itself fails
/// on its internal columns). A catalog without `is_node` (before 2017,
/// Babelfish, Fabric) has none.
pub(super) async fn refuse_graph(s: &mut dyn Session, schema: Option<&str>, name: &str) -> Result<()> {
    let lit = qualified_name(Quote::Bracket, schema, name).replace('\'', "''");
    let sql = format!(
        "SELECT CAST(t.is_node AS nvarchar(5)), CAST(t.is_edge AS nvarchar(5)) FROM sys.tables t WHERE t.object_id = OBJECT_ID(N'{lit}')"
    );
    let Some(r) = strings(s, &sql).await.ok().and_then(|r| r.into_iter().next()) else { return Ok(()) };
    let b = |i: usize| r.get(i).cloned().flatten().is_some_and(|v| v.trim() == "1");
    match graph_reason(b(0), b(1)) {
        Some(m) => Err(Error::Unsupported(m)),
        None => Ok(()),
    }
}

// --- Sparse columns, column set, system-versioned (temporal) tables ------
//
// `database_schema` reports a sparse column, a column set and a period
// column as plain ones: the clone would come out with ordinary columns,
// the column set frozen as xml, and no versioning. [`extras`] reads them;
// the CREATE gets SPARSE / COLUMN_SET ([`patch_sparse`]), the column set
// isn't loaded (the sparse columns carry its values), and a temporal
// table's clone gets its PERIOD and SYSTEM_VERSIONING after the rows
// ([`make_temporal`]), with a history table of its own that takes the
// original's history rows when the data is copied.

/// What SQL Server's catalog has and `database_schema` doesn't.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Extras {
    pub sparse: Vec<String>,
    pub column_set: Option<String>,
    pub period: Option<Period>,
}

/// `PERIOD FOR SYSTEM_TIME` and, when versioned, its history table.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Period {
    pub start: String,
    pub end: String,
    /// Period columns marked HIDDEN.
    pub hidden: Vec<String>,
    /// SYSTEM_VERSIONING = ON: the original's history table (schema, name).
    pub history: Option<(String, String)>,
    /// `6 MONTHS`… (`None`: infinite, the default).
    pub retention: Option<String>,
    /// The clone's history table (schema, name); `None`: named by the server.
    pub clone_history: Option<(String, String)>,
}

/// The server's own name for a history table (`MSSQL_TemporalHistoryFor_<id>`).
const AUTO_HISTORY: &str = "MSSQL_TemporalHistoryFor_";

/// The original's sparse columns, column set and period. Columns the
/// engine generates other than a period's (a ledger table's) are refused.
pub(super) async fn extras(s: &mut dyn Session, t: &TableSchema) -> Result<Extras> {
    let lit = literal(t);
    let failed = || Error::State("no se pudo leer si la tabla tiene columnas dispersas (SPARSE) o versiones del sistema (tabla temporal); no se clona".into());
    let n = |c: &str| format!("CAST({c} AS nvarchar(5))");
    // SQL Server 2016 on: generated_always_type and is_hidden.
    let full = format!(
        "SELECT c.name, {}, {}, {}, {} FROM sys.columns c WHERE c.object_id = OBJECT_ID(N'{lit}') ORDER BY c.column_id",
        n("c.is_sparse"),
        n("c.is_column_set"),
        n("c.generated_always_type"),
        n("c.is_hidden")
    );
    let rows = match strings(s, &full).await {
        Ok(r) => r,
        Err(_) => {
            let old = format!(
                "SELECT c.name, {}, {} FROM sys.columns c WHERE c.object_id = OBJECT_ID(N'{lit}') ORDER BY c.column_id",
                n("c.is_sparse"),
                n("c.is_column_set")
            );
            // A catalog without them (Fabric, Babelfish): an engine
            // without sparse columns nor temporal tables.
            match strings(s, &old).await {
                Ok(r) => r,
                Err(_) => return Ok(Extras::default()),
            }
        }
    };
    let mut x = Extras::default();
    let mut hidden = Vec::new();
    for r in &rows {
        let v = |i: usize| r.get(i).cloned().flatten().unwrap_or_default().trim().to_string();
        let name = v(0);
        if v(2) == "1" {
            x.column_set = Some(name.clone());
        } else if v(1) == "1" {
            x.sparse.push(name.clone());
        }
        let generated = v(3);
        if !matches!(generated.as_str(), "" | "0" | "1" | "2") {
            return Err(Error::Unsupported(format!(
                "no se puede clonar: la columna «{name}» la genera el motor (tabla de libro de contabilidad, LEDGER, o similar) y el clon no puede reproducirla"
            )));
        }
        if v(4) == "1" {
            hidden.push(name);
        }
    }
    let periods = format!(
        "SELECT COL_NAME(p.object_id, p.start_column_id), COL_NAME(p.object_id, p.end_column_id) FROM sys.periods p WHERE p.object_id = OBJECT_ID(N'{lit}')"
    );
    // Before 2016 there's no sys.periods (nor temporal tables).
    let Some(p) = strings(s, &periods).await.ok().and_then(|r| r.into_iter().next()) else { return Ok(x) };
    let v = |i: usize| p.get(i).cloned().flatten().unwrap_or_default();
    let mut period = Period { start: v(0), end: v(1), ..Default::default() };
    if period.start.is_empty() || period.end.is_empty() {
        return Err(failed());
    }
    period.hidden = hidden.into_iter().filter(|h| *h == period.start || *h == period.end).collect();
    let table = |retention: bool| {
        format!(
            "SELECT {}, OBJECT_SCHEMA_NAME(t.history_table_id), OBJECT_NAME(t.history_table_id){} FROM sys.tables t WHERE t.object_id = OBJECT_ID(N'{lit}')",
            n("t.temporal_type"),
            if retention { ", CAST(t.history_retention_period AS nvarchar(20)), t.history_retention_period_unit_desc" } else { "" }
        )
    };
    let r = match strings(s, &table(true)).await {
        Ok(r) => r,
        // SQL Server 2016: no retention.
        Err(_) => strings(s, &table(false)).await.map_err(|_| failed())?,
    };
    let r = r.into_iter().next().ok_or_else(failed)?;
    let v = |i: usize| r.get(i).cloned().flatten().unwrap_or_default().trim().to_string();
    if v(0) == "2" {
        if v(1).is_empty() || v(2).is_empty() {
            return Err(failed());
        }
        period.history = Some((v(1), v(2)));
        period.retention = retention(&v(3), &v(4));
    }
    x.period = Some(period);
    Ok(x)
}

/// `HISTORY_RETENTION_PERIOD` as T-SQL takes it; `None` for infinite.
fn retention(period: &str, unit: &str) -> Option<String> {
    let n: i64 = period.trim().parse().ok().filter(|n| *n > 0)?;
    let unit = unit.trim().to_ascii_uppercase();
    if !matches!(unit.as_str(), "DAY" | "WEEK" | "MONTH" | "YEAR") {
        return None;
    }
    Some(format!("{n} {unit}{}", if n == 1 { "" } else { "S" }))
}

/// The clone's history table name: the original's with the table's name
/// replaced (`t_hist` → `t_c_hist`), else `<clone>_history`; `None` when
/// the server named the original's (it names the clone's too).
pub(super) fn history_name(history: &str, original: &str, clone: &str) -> Option<String> {
    if history.starts_with(AUTO_HISTORY) {
        return None;
    }
    let base = if !original.is_empty() && history.contains(original) { history.replacen(original, clone, 1) } else { format!("{clone}_history") };
    Some(base.chars().take(128).collect())
}

/// The clone's history table's schema: the original's history's when it
/// has one of its own (not the original table's), else the clone's.
pub(super) fn history_schema(history_schema: &str, original: &TableSchema, clone: &TableSchema) -> String {
    let own = |t: &TableSchema| t.schema.clone().filter(|s| !s.is_empty());
    match own(original) {
        Some(o) if !o.eq_ignore_ascii_case(history_schema) => history_schema.to_string(),
        _ => own(clone).unwrap_or_else(|| history_schema.to_string()),
    }
}

/// Picks the clone's history table name, free in its schema (`_2`, `_3`…
/// when it's taken).
pub(super) async fn plan_history(s: &mut dyn Session, x: &mut Extras, original: &TableSchema, clone: &TableSchema) -> Result<()> {
    let Some(p) = x.period.as_mut() else { return Ok(()) };
    let Some((hs, hn)) = p.history.clone() else { return Ok(()) };
    let Some(base) = history_name(&hn, &original.name, &clone.name) else { return Ok(()) };
    let schema = history_schema(&hs, original, clone);
    for i in 1..=20 {
        let name = if i == 1 {
            base.clone()
        } else {
            let tail = format!("_{i}");
            format!("{}{tail}", base.chars().take(128 - tail.len()).collect::<String>())
        };
        let q = qualified_name(Quote::Bracket, Some(&schema), &name).replace('\'', "''");
        let free = strings(s, &format!("SELECT CAST(CASE WHEN OBJECT_ID(N'{q}') IS NULL THEN 1 ELSE 0 END AS nvarchar(1))"))
            .await
            .ok()
            .and_then(|r| r.into_iter().next())
            .and_then(|r| r.into_iter().next().flatten());
        match free.as_deref() {
            Some("1") if name != clone.name || Some(&schema) != clone.schema.as_ref() => {
                p.clone_history = Some((schema, name));
                return Ok(());
            }
            Some(_) => continue,
            None => break,
        }
    }
    Err(Error::State(
        "no se pudo elegir un nombre libre para la tabla de historial del clon (la tabla es temporal, con SYSTEM_VERSIONING); elegí otro nombre para la tabla".into(),
    ))
}

/// The CREATE with the sparse columns `SPARSE` and the column set as
/// `xml COLUMN_SET FOR ALL_SPARSE_COLUMNS`. An error when a column's line
/// isn't found (the clone would lose it).
pub(super) fn patch_sparse(create: &mut String, clone: &TableSchema, x: &Extras) -> Result<()> {
    let q = |c: &str| quote_ident(Quote::Bracket, c);
    let lost = |c: &str| Error::State(format!("no se pudo reproducir la columna dispersa «{c}» (SPARSE / COLUMN_SET) en el clon; no se clona"));
    let mut lines: Vec<String> = create.split('\n').map(str::to_string).collect();
    let find = |lines: &[String], c: &str| {
        let col = clone.columns.iter().find(|d| d.name == c)?;
        let head = format!("    {} {}", q(c), col.data_type);
        lines.iter().position(|l| l.starts_with(&format!("{head} ")) || l == &head || l.starts_with(&format!("{head},")))
    };
    for c in &x.sparse {
        let i = find(&lines, c).ok_or_else(|| lost(c))?;
        let l = &mut lines[i];
        let (body, comma) = match l.strip_suffix(',') {
            Some(b) => (b.to_string(), ","),
            None => (l.clone(), ""),
        };
        let Some(b) = body.strip_suffix(" NULL").filter(|b| !b.ends_with(" NOT")) else { return Err(lost(c)) };
        *l = format!("{b} SPARSE NULL{comma}");
    }
    if let Some(c) = &x.column_set {
        let i = find(&lines, c).ok_or_else(|| lost(c))?;
        let comma = if lines[i].ends_with(',') { "," } else { "" };
        lines[i] = format!("    {} xml COLUMN_SET FOR ALL_SPARSE_COLUMNS{comma}", q(c));
    }
    *create = lines.join("\n");
    Ok(())
}

/// Once created, the clone's sparse columns and column set against the
/// original's (before the rows).
pub(super) async fn verify_sparse(s: &mut dyn Session, clone: &TableSchema, original: &Extras) -> Result<()> {
    let have = extras(s, clone).await?;
    if have.sparse != original.sparse || have.column_set != original.column_set {
        let list = |v: &[String]| if v.is_empty() { "ninguna".to_string() } else { v.join(", ") };
        return Err(Error::State(format!(
            "el clon no quedó igual al original: columnas dispersas (SPARSE) {} / {}, conjunto de columnas {} / {}; no se clona",
            list(&original.sparse),
            list(&have.sparse),
            original.column_set.as_deref().unwrap_or("ninguno"),
            have.column_set.as_deref().unwrap_or("ninguno"),
        )));
    }
    Ok(())
}

/// Drops the clone's history table with it (a versioned table can't be
/// dropped as it is): what goes before the clone's DROP TABLE.
pub(super) fn drop_temporal_sql(clone: &TableSchema) -> String {
    let name = table_name(clone);
    let lit = literal(clone);
    let off = format!("ALTER TABLE {name} SET (SYSTEM_VERSIONING = OFF)").replace('\'', "''");
    format!(
        "IF OBJECTPROPERTY(OBJECT_ID(N'{lit}'), 'TableTemporalType') = 2\nBEGIN\n    \
         DECLARE @dbine_h nvarchar(600) = (SELECT QUOTENAME(OBJECT_SCHEMA_NAME(history_table_id)) + N'.' + QUOTENAME(OBJECT_NAME(history_table_id)) FROM sys.tables WHERE object_id = OBJECT_ID(N'{lit}'));\n    \
         EXEC(N'{off}');\n    \
         EXEC(N'DROP TABLE ' + @dbine_h);\nEND;\n"
    )
}

/// The clone becomes temporal as the original: PERIOD (its columns
/// become GENERATED ALWAYS), HIDDEN, SYSTEM_VERSIONING with its own
/// history table, which gets the original's history rows when
/// `with_data`. Returns the note for the report.
pub(super) async fn make_temporal(s: &mut dyn Session, clone: &TableSchema, x: &Extras, with_data: bool) -> Result<Option<String>> {
    let Some(p) = &x.period else { return Ok(None) };
    let name = table_name(clone);
    let fail = |what: &str, e: Error| Error::Query(format!("tabla temporal: no se pudo {what} en el clon ({e}); no se clona"));
    exec(s, &format!("ALTER TABLE {name} ADD PERIOD FOR SYSTEM_TIME ({}, {})", quote_ident(Quote::Bracket, &p.start), quote_ident(Quote::Bracket, &p.end)))
        .await
        .map_err(|e| fail("agregar el período PERIOD FOR SYSTEM_TIME", e))?;
    for h in &p.hidden {
        exec(s, &format!("ALTER TABLE {name} ALTER COLUMN {} ADD HIDDEN", quote_ident(Quote::Bracket, h)))
            .await
            .map_err(|e| fail("ocultar (HIDDEN) la columna del período", e))?;
    }
    let Some((ohs, ohn)) = &p.history else {
        return Ok(Some(format!("el clon tiene el período PERIOD FOR SYSTEM_TIME ({}, {}) del original", p.start, p.end)));
    };
    let retention = p.retention.as_deref().map(|r| format!(", HISTORY_RETENTION_PERIOD = {r}")).unwrap_or_default();
    let on = |history: Option<&str>| {
        let h = history.map(|h| format!("HISTORY_TABLE = {h}, ")).unwrap_or_default();
        format!("ALTER TABLE {name} SET (SYSTEM_VERSIONING = ON ({h}DATA_CONSISTENCY_CHECK = ON{retention}))")
    };
    let wanted = p.clone_history.as_ref().map(|(hs, hn)| qualified_name(Quote::Bracket, Some(hs), hn));
    exec(s, &on(wanted.as_deref())).await.map_err(|e| fail("activar las versiones del sistema (SYSTEM_VERSIONING)", e))?;
    // The history table the server made (or named).
    let lit = literal(clone);
    let made = strings(
        s,
        &format!("SELECT OBJECT_SCHEMA_NAME(history_table_id), OBJECT_NAME(history_table_id) FROM sys.tables WHERE object_id = OBJECT_ID(N'{lit}')"),
    )
    .await?
    .into_iter()
    .next()
    .and_then(|r| Some((r.first().cloned().flatten()?, r.get(1).cloned().flatten()?)))
    .ok_or_else(|| Error::State("tabla temporal: no se encontró la tabla de historial del clon; no se clona".into()))?;
    let history = qualified_name(Quote::Bracket, Some(&made.0), &made.1);
    let source_history = qualified_name(Quote::Bracket, Some(ohs), ohn);
    let mut copied = 0i64;
    if with_data {
        let olit = source_history.replace('\'', "''");
        let count = strings(s, &format!("SELECT CAST(COUNT_BIG(*) AS nvarchar(20)) FROM {source_history}")).await?;
        let total: i64 = count.first().and_then(|r| r.first().cloned().flatten()).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        if total > 0 {
            let cols: Vec<String> = strings(
                s,
                &format!("SELECT c.name FROM sys.columns c WHERE c.object_id = OBJECT_ID(N'{olit}') AND c.is_computed = 0 AND c.is_column_set = 0 ORDER BY c.column_id"),
            )
            .await?
            .into_iter()
            .filter_map(|r| r.into_iter().next().flatten())
            .map(|c| quote_ident(Quote::Bracket, &c))
            .collect();
            if cols.is_empty() {
                return Err(Error::State("tabla temporal: no se pudieron leer las columnas de la tabla de historial del original; no se clona".into()));
            }
            let list = cols.join(", ");
            exec(s, &format!("ALTER TABLE {name} SET (SYSTEM_VERSIONING = OFF)")).await.map_err(|e| fail("copiar el historial", e))?;
            let loaded = exec(s, &format!("INSERT INTO {history} ({list}) SELECT {list} FROM {source_history}")).await;
            let back = exec(s, &on(Some(&history))).await;
            if let Err(e) = loaded.and(back) {
                // Unlinked, the cleanup wouldn't find it.
                let _ = exec(s, &format!("DROP TABLE IF EXISTS {history}")).await;
                return Err(fail("copiar las filas de historial del original", e));
            }
            copied = total;
        }
    }
    // The clone's versioning against the original's.
    let have = extras(s, clone).await?;
    let ok = have.period.as_ref().is_some_and(|h| h.start == p.start && h.end == p.end && h.hidden == p.hidden && h.history.is_some() && h.retention == p.retention);
    if !ok {
        return Err(Error::State("el clon no quedó igual al original: sus versiones del sistema (tabla temporal) son otras; no se clona".into()));
    }
    Ok(Some(format!(
        "tabla temporal (SYSTEM_VERSIONING): el clon también lo es, con su propia tabla de historial «{}.{}»{}",
        made.0,
        made.1,
        if with_data { format!(" ({copied} filas de historial copiadas de «{ohs}.{ohn}»)") } else { String::new() }
    )))
}

/// Two indexes that are the same but for their names.
fn same_index(a: &IndexDef, b: &IndexDef) -> bool {
    let kind = |i: &IndexDef| i.kind.as_deref().map(|k| k.trim().to_ascii_uppercase()).unwrap_or_default();
    a.columns == b.columns && a.unique == b.unique && kind(a) == kind(b) && a.filter == b.filter && a.include == b.include && a.options == b.options
}

/// What the clone's history table's indexes need to be the original
/// history's: the server's own ones the original's doesn't have, dropped;
/// the original's the clone's doesn't have, created (named with the
/// clone's history table's name where they carry the original's).
pub(super) fn history_index_plan(original: &TableSchema, clone: &TableSchema) -> (Vec<String>, Vec<IndexDef>) {
    let drop = clone.indexes.iter().filter(|h| !original.indexes.iter().any(|o| same_index(o, h))).map(|h| h.name.clone()).collect();
    let create = original
        .indexes
        .iter()
        .filter(|o| !clone.indexes.iter().any(|h| same_index(o, h)))
        .map(|o| {
            let name = if o.name.contains(&original.name) { o.name.replacen(&original.name, &clone.name, 1) } else { o.name.clone() };
            IndexDef { name: name.chars().take(128).collect(), ..o.clone() }
        })
        .collect();
    (drop, create)
}

/// The clone's history table gets the original history's indexes (the
/// ones added to it, with their options): the server makes it with only
/// its own clustered index. Returns the notes for the report.
pub(super) async fn history_indexes(driver: &dyn Driver, s: &mut dyn Session, clone: &TableSchema, x: &Extras, with_indexes: bool) -> Result<Vec<String>> {
    let Some((ohs, ohn)) = x.period.as_ref().and_then(|p| p.history.clone()) else { return Ok(Vec::new()) };
    let failed = || Error::State("tabla temporal: no se pudieron leer los índices de las tablas de historial; no se clona".into());
    let lit = literal(clone);
    let made = strings(
        s,
        &format!("SELECT OBJECT_SCHEMA_NAME(history_table_id), OBJECT_NAME(history_table_id) FROM sys.tables WHERE object_id = OBJECT_ID(N'{lit}')"),
    )
    .await?
    .into_iter()
    .next()
    .and_then(|r| Some((r.first().cloned().flatten()?, r.get(1).cloned().flatten()?)))
    .ok_or_else(failed)?;
    let read = |all: &[TableSchema], sc: &str, n: &str| all.iter().find(|t| t.schema.as_deref() == Some(sc) && t.name == n).cloned();
    let all = s.database_schema().await.map_err(|_| failed())?;
    let (Some(original), Some(have)) = (read(&all, &ohs, &ohn), read(&all, &made.0, &made.1)) else { return Err(failed()) };
    let (drop, create) = history_index_plan(&original, &have);
    if create.is_empty() && drop.is_empty() {
        return Ok(Vec::new());
    }
    if !with_indexes {
        return Ok(vec![format!(
            "sin índices: la tabla de historial del clon «{}.{}» tiene solo el índice que crea el servidor, no los de «{ohs}.{ohn}»",
            made.0, made.1
        )]);
    }
    let history = qualified_name(Quote::Bracket, Some(&made.0), &made.1);
    let fail = |e: Error| Error::Query(format!("tabla temporal: no se pudieron crear en la tabla de historial del clon los índices de la del original ({e}); no se clona"));
    for d in &drop {
        exec(s, &format!("DROP INDEX {} ON {history}", quote_ident(Quote::Bracket, d))).await.map_err(fail)?;
    }
    let names: Vec<String> = create.iter().map(|i| format!("«{}»", i.name)).collect();
    if !create.is_empty() {
        let t = TableSchema { schema: Some(made.0.clone()), name: made.1.clone(), indexes: create, ..Default::default() };
        let sql = driver.table_ddl(&t, DdlParts { indexes: true, ..Default::default() })?;
        if !sql.trim().is_empty() {
            exec(s, &sql).await.map_err(fail)?;
        }
    }
    // Checked against the original's.
    let all = s.database_schema().await.map_err(|_| failed())?;
    let have = read(&all, &made.0, &made.1).ok_or_else(failed)?;
    let (d, c) = history_index_plan(&original, &have);
    if !d.is_empty() || !c.is_empty() {
        return Err(Error::State(format!(
            "el clon no quedó igual al original: los índices de su tabla de historial «{}.{}» no son los de «{ohs}.{ohn}»; no se clona",
            made.0, made.1
        )));
    }
    Ok(if names.is_empty() {
        Vec::new()
    } else {
        vec![format!("la tabla de historial del clon «{}.{}» tiene los índices de «{ohs}.{ohn}»: {}", made.0, made.1, names.join(", "))]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn con(kind: &str, name: &str, system: bool, key: &str) -> Con {
        Con {
            kind: kind.into(),
            name: name.into(),
            system_named: system,
            disabled: false,
            untrusted: false,
            not_for_replication: false,
            key: key.into(),
            definition: String::new(),
        }
    }

    #[test]
    fn identity_counter_on_a_fresh_or_used_clone() {
        // IDENTITY(50,10): two rows, all deleted (current 60): next 70.
        let next = next_value(60, 10, true).unwrap();
        assert_eq!(next, 70);
        // An empty clone never had a row: RESEED takes the value itself.
        assert_eq!(reseed_value(next, 10, false), Some(70));
        // A clone with rows: RESEED to the current one.
        assert_eq!(reseed_value(next, 10, true), Some(60));
        // An original that never had a row starts where it's seeded.
        assert_eq!(next_value(50, 10, false), Some(50));
        assert_eq!(reseed_value(50, 10, false), Some(50));
        // Descending.
        assert_eq!(reseed_value(next_value(-9, -3, true).unwrap(), -3, false), Some(-12));
        // decimal(38,0) near its end.
        assert_eq!(next_value(i128::MAX, 1, true), None);
    }

    #[test]
    fn constraint_states() {
        let t = "[s].[c]";
        let mut o = con("C", "CK_t_v", false, "");
        let mut c = con("C", "CK_c_v", false, "");
        assert_eq!(state_sql(t, &o, &c), None);
        o.disabled = true;
        o.untrusted = true;
        assert_eq!(state_sql(t, &o, &c).unwrap(), "ALTER TABLE [s].[c] NOCHECK CONSTRAINT [CK_c_v]");
        o.disabled = false;
        assert_eq!(state_sql(t, &o, &c).unwrap(), "ALTER TABLE [s].[c] NOCHECK CONSTRAINT [CK_c_v];\nALTER TABLE [s].[c] CHECK CONSTRAINT [CK_c_v]");
        o.untrusted = false;
        c.disabled = true;
        c.untrusted = true;
        assert_eq!(state_sql(t, &o, &c).unwrap(), "ALTER TABLE [s].[c] WITH CHECK CHECK CONSTRAINT [CK_c_v]");
    }

    #[test]
    fn constraints_pair_by_new_name_or_definition() {
        let renames = vec![Rename { from: "CK_t_v".into(), to: "CK_c_v".into(), shortened: false }];
        let original = vec![con("C", "CK_t_v", false, "a"), con("C", "CK__t__v__1", true, "([v]>(0))|v"), con("F", "FK__t__2", true, "p>id@")];
        let clone = vec![con("F", "FK__c__9", true, "p>id@"), con("C", "CK__c__v__8", true, "([v]>(0))|v"), con("C", "CK_c_v", false, "a")];
        let p = pair(&original, &clone, &renames).unwrap();
        let names: Vec<(&str, &str)> = p.iter().map(|(o, c)| (o.name.as_str(), c.name.as_str())).collect();
        assert_eq!(names, vec![("CK_t_v", "CK_c_v"), ("CK__t__v__1", "CK__c__v__8"), ("FK__t__2", "FK__c__9")]);
        assert_eq!(pair(&original, &clone[..2], &renames).unwrap_err(), "CK_t_v");
    }

    #[test]
    fn foreign_keys_are_added_unchecked() {
        let t = TableSchema { schema: Some("ventas odd".into()), name: "hijo_c".into(), ..Default::default() };
        assert_eq!(
            foreign_keys_unchecked("ALTER TABLE [ventas odd].[hijo_c] ADD CONSTRAINT [fk] FOREIGN KEY ([p]) REFERENCES [x] ([id]);", &t),
            "ALTER TABLE [ventas odd].[hijo_c] WITH NOCHECK ADD CONSTRAINT [fk] FOREIGN KEY ([p]) REFERENCES [x] ([id]);"
        );
    }

    fn pk(kind: &str, keys: &[(&str, bool)]) -> PrimaryKey {
        PrimaryKey {
            kind: kind.into(),
            keys: keys.iter().map(|(c, d)| (c.to_string(), *d)).collect(),
            row_locks: true,
            page_locks: true,
            compression: vec!["NONE".into()],
            ..Default::default()
        }
    }

    fn keyed(name: Option<&str>, cols: &[&str]) -> TableSchema {
        TableSchema {
            schema: Some("ventas odd".into()),
            name: "pkopt_c".into(),
            primary_key: Some(dbine_driver::KeyDef { name: name.map(str::to_string), columns: cols.iter().map(|c| c.to_string()).collect() }),
            ..Default::default()
        }
    }

    #[test]
    fn primary_key_clause_carries_order_and_options() {
        // PRIMARY KEY NONCLUSTERED (id DESC) WITH (DATA_COMPRESSION = ROW, FILLFACTOR = 80)
        let mut p = pk("NONCLUSTERED", &[("id", true)]);
        p.fill_factor = 80;
        p.compression = vec!["ROW".into()];
        assert_eq!(key_clause(&p), "PRIMARY KEY NONCLUSTERED ([id] DESC) WITH (FILLFACTOR = 80, DATA_COMPRESSION = ROW)");
        // Nothing but the defaults: no WITH.
        assert_eq!(key_clause(&pk("CLUSTERED", &[("a", false), ("b c", true)])), "PRIMARY KEY CLUSTERED ([a], [b c] DESC)");
        // Every option, per-partition compression and a partition scheme.
        let mut p = pk("CLUSTERED", &[("id", false)]);
        (p.padded, p.fill_factor, p.ignore_dup_key, p.no_recompute, p.row_locks, p.page_locks, p.sequential_key) =
            (true, 50, true, true, false, false, true);
        p.compression = vec!["ROW".into(), "NONE".into(), "PAGE".into()];
        p.data_space = Some(("ps_fix".into(), Some("id".into())));
        assert_eq!(
            key_clause(&p),
            "PRIMARY KEY CLUSTERED ([id]) WITH (PAD_INDEX = ON, FILLFACTOR = 50, IGNORE_DUP_KEY = ON, STATISTICS_NORECOMPUTE = ON, \
             ALLOW_ROW_LOCKS = OFF, ALLOW_PAGE_LOCKS = OFF, OPTIMIZE_FOR_SEQUENTIAL_KEY = ON, DATA_COMPRESSION = ROW ON PARTITIONS (1), \
             DATA_COMPRESSION = PAGE ON PARTITIONS (3)) ON [ps_fix]([id])"
        );
        p.data_space = Some(("FG 2".into(), None));
        assert!(key_clause(&p).ends_with(") ON [FG 2]"));
    }

    #[test]
    fn the_create_gets_the_original_key() {
        let mut p = pk("NONCLUSTERED", &[("id", true)]);
        p.fill_factor = 80;
        // Named, last line.
        let t = keyed(Some("PK_pkopt_c"), &["id"]);
        let mut create = "CREATE TABLE [ventas odd].[pkopt_c] (\n    [id] int NOT NULL,\n    CONSTRAINT [PK_pkopt_c] PRIMARY KEY ([id])\n);".to_string();
        assert!(patch_primary_key(&mut create, &t, &p).unwrap());
        assert_eq!(
            create,
            "CREATE TABLE [ventas odd].[pkopt_c] (\n    [id] int NOT NULL,\n    CONSTRAINT [PK_pkopt_c] PRIMARY KEY NONCLUSTERED ([id] DESC) WITH (FILLFACTOR = 80)\n);"
        );
        // Server-named, made NONCLUSTERED by the driver, a CHECK after it.
        let t = keyed(None, &["a", "b"]);
        let p = pk("CLUSTERED", &[("A", false), ("B", true)]);
        let mut create = "CREATE TABLE [t] (\n    [a] int NOT NULL,\n    [b] int NOT NULL,\n    PRIMARY KEY NONCLUSTERED ([a], [b]),\n    CHECK ([a]>(0))\n);".to_string();
        assert!(patch_primary_key(&mut create, &t, &p).unwrap());
        assert!(create.contains("\n    PRIMARY KEY CLUSTERED ([A], [B] DESC),\n    CHECK"), "{create}");
        // No such line (Fabric adds its keys apart): left, checked after.
        let mut fabric = "CREATE TABLE [t] (\n    [a] int NOT NULL\n);\nALTER TABLE [t] ADD CONSTRAINT [PK_t] PRIMARY KEY NONCLUSTERED ([a]) NOT ENFORCED;".to_string();
        assert!(!patch_primary_key(&mut fabric, &keyed(Some("PK_t"), &["a"]), &pk("NONCLUSTERED", &[("a", false)])).unwrap());
        // Columns that don't match the catalog's: refused.
        let mut c = create.clone();
        assert!(patch_primary_key(&mut c, &keyed(None, &["a"]), &pk("CLUSTERED", &[("b", false)])).is_err());
    }

    #[test]
    fn keys_the_clone_cant_have_are_refused() {
        assert!(check_primary_key(&pk("CLUSTERED", &[("id", false)])).is_ok());
        let e = check_primary_key(&pk("NONCLUSTERED HASH", &[("id", false)])).unwrap_err().to_string();
        assert!(e.contains("la clave primaria del original es de tipo NONCLUSTERED HASH"), "{e}");
        let mut p = pk("CLUSTERED", &[("id", false)]);
        p.data_space = Some(("ps".into(), Some(String::new())));
        assert!(check_primary_key(&p).unwrap_err().to_string().contains("columna de partición"));
    }

    #[test]
    fn the_clone_is_stored_where_the_original_is() {
        let fg = |n: &str, default: bool| Space { name: n.into(), column: None, default };
        let ps = |n: &str, c: &str| Space { name: n.into(), column: Some(c.into()), default: false };
        let placed = |at: usize, kind: u8, space: Space| Placed { at, kind, space, compression: vec!["NONE".into()] };
        // A partitioned heap, compressed per partition, with a clustered
        // index on the scheme, an aligned index, one on FG2 and one left on
        // PRIMARY (said explicitly: without ON it would be aligned).
        let l = Layout {
            table: Some(ps("ps_r3", "id")),
            heap_compression: vec!["NONE".into(), "ROW".into(), "NONE".into()],
            lob: Some(Space { name: "ps_r3".into(), column: Some("id".into()), default: false }),
            indexes: vec![placed(0, 1, ps("ps_r3", "id")), placed(1, 2, ps("ps_r3", "id")), placed(2, 2, fg("FG2", false)), placed(3, 2, fg("PRIMARY", true))],
            memory: false,
        };
        assert_eq!(table_tail(&l, false), " ON [ps_r3]([id]) WITH (DATA_COMPRESSION = ROW ON PARTITIONS (2))");
        // The clustered primary key carries the table's ON.
        assert_eq!(table_tail(&l, true), " WITH (DATA_COMPRESSION = ROW ON PARTITIONS (2))");
        let ons: Vec<(usize, String)> = index_ons(&l);
        assert_eq!(
            ons,
            [(0, " ON [ps_r3]([id])".to_string()), (1, " ON [ps_r3]([id])".into()), (2, " ON [FG2]".into()), (3, " ON [PRIMARY]".into())]
        );
        let mut create = "CREATE TABLE [dbo].[cx_c] (\n    [id] int NOT NULL\n);\nEXEC sp_addextendedproperty 'x';".to_string();
        place_table(&mut create, &l, false).unwrap();
        assert!(create.starts_with("CREATE TABLE [dbo].[cx_c] (\n    [id] int NOT NULL\n) ON [ps_r3]([id]) WITH (DATA_COMPRESSION = ROW ON PARTITIONS (2));\nEXEC"), "{create}");
        // On the default filegroup, not partitioned: nothing to say.
        let plain = Layout { table: Some(fg("PRIMARY", true)), indexes: vec![placed(0, 2, fg("PRIMARY", true))], ..Default::default() };
        assert_eq!(table_tail(&plain, false), "");
        assert!(index_ons(&plain).is_empty());
        let mut create = "CREATE TABLE [t] (\n    [id] int\n);".to_string();
        place_table(&mut create, &plain, false).unwrap();
        assert_eq!(create, "CREATE TABLE [t] (\n    [id] int\n);");
        // A table on FG2 with its LOB columns on PRIMARY.
        let lob = Layout { table: Some(fg("FG2", false)), lob: Some(fg("PRIMARY", true)), ..Default::default() };
        assert_eq!(table_tail(&lob, false), " ON [FG2] TEXTIMAGE_ON [PRIMARY]");
        // Memory-optimized: left alone.
        assert_eq!(table_tail(&Layout { memory: true, ..l.clone() }, false), "");
    }

    #[test]
    fn layouts_the_clone_cant_have_are_refused() {
        let t = TableSchema { indexes: vec![IndexDef { name: "SIX".into(), ..Default::default() }], ..Default::default() };
        let ps = |n: &str, c: &str| Space { name: n.into(), column: Some(c.into()), default: false };
        let spatial = |s: Space| Placed { at: 0, kind: 4, space: s, compression: Vec::new() };
        let ok = Layout { table: Some(ps("ps", "id")), indexes: vec![spatial(ps("ps", "id"))], ..Default::default() };
        assert!(check_layout(&ok, &t).is_ok());
        let e = check_layout(&Layout { indexes: vec![spatial(ps("otro", "id"))], ..ok.clone() }, &t).unwrap_err().to_string();
        assert!(e.contains("el índice espacial «SIX»") && e.contains("no se clona"), "{e}");
        let e = check_layout(&Layout { table: Some(ps("ps", "")), ..Default::default() }, &t).unwrap_err().to_string();
        assert!(e.contains("columna de partición de la tabla"), "{e}");
    }

    #[test]
    fn key_index_follows_its_index() {
        let mut t = TableSchema {
            indexes: vec![IndexDef {
                name: "fulltext".into(),
                kind: Some("FULLTEXT".into()),
                options: [(KEY_INDEX.to_string(), "UX_docs".to_string())].into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        rename_key_index(&mut t, &[Rename { from: "UX_docs".into(), to: "UX_docs_c".into(), shortened: false }]);
        assert_eq!(t.indexes[0].options.get(KEY_INDEX).map(String::as_str), Some("UX_docs_c"));
        assert!(unnamed_index("mssql", &t.indexes[0]) && !unnamed_index("mysql", &t.indexes[0]));
    }

    #[test]
    fn secondary_xml_index_follows_its_primary() {
        let mut t = TableSchema {
            indexes: vec![
                IndexDef { name: "PXML_kinds".into(), kind: Some("XML".into()), ..Default::default() },
                IndexDef {
                    name: "SXML_kinds".into(),
                    kind: Some("XML PATH".into()),
                    options: [(PRIMARY_XML_INDEX.to_string(), "PXML_kinds".to_string())].into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        // A rename from a name collision (hash suffix), and the option in another case.
        t.indexes[1].options.insert(PRIMARY_XML_INDEX.into(), "pxml_KINDS".into());
        rename_key_index(
            &mut t,
            &[
                Rename { from: "PXML_kinds".into(), to: "PXML_kc_0badc0de".into(), shortened: false },
                Rename { from: "SXML_kinds".into(), to: "SXML_kc".into(), shortened: false },
            ],
        );
        assert_eq!(t.indexes[1].options.get(PRIMARY_XML_INDEX).map(String::as_str), Some("PXML_kc_0badc0de"));
    }
    fn sparse_table() -> TableSchema {
        let col = |n: &str, ty: &str| dbine_driver::ColumnDef { name: n.into(), data_type: ty.into(), nullable: true, ..Default::default() };
        TableSchema {
            schema: Some("ventas odd".into()),
            name: "esparsa_c".into(),
            columns: vec![col("id", "int"), col("s1", "int"), col("s2", "nvarchar(20) COLLATE Latin1_General_CS_AS"), col("cs", "xml")],
            ..Default::default()
        }
    }

    #[test]
    fn sparse_columns_and_the_column_set_go_into_the_create() {
        let t = sparse_table();
        let x = Extras { sparse: vec!["s1".into(), "s2".into()], column_set: Some("cs".into()), period: None };
        let mut create = "CREATE TABLE [ventas odd].[esparsa_c] (\n    [id] int NOT NULL,\n    [s1] int NULL,\n    [s2] nvarchar(20) COLLATE Latin1_General_CS_AS NULL,\n    [cs] xml NULL,\n    PRIMARY KEY ([id])\n);".to_string();
        patch_sparse(&mut create, &t, &x).unwrap();
        assert_eq!(
            create,
            "CREATE TABLE [ventas odd].[esparsa_c] (\n    [id] int NOT NULL,\n    [s1] int SPARSE NULL,\n    [s2] nvarchar(20) COLLATE Latin1_General_CS_AS SPARSE NULL,\n    [cs] xml COLUMN_SET FOR ALL_SPARSE_COLUMNS,\n    PRIMARY KEY ([id])\n);"
        );
        // Last line, no comma.
        let mut create = "CREATE TABLE [t] (\n    [s1] int NULL,\n    [cs] xml NULL\n);".to_string();
        patch_sparse(&mut create, &t, &x.clone()).unwrap_err();
        let x1 = Extras { sparse: vec!["s1".into()], column_set: Some("cs".into()), period: None };
        patch_sparse(&mut create, &t, &x1).unwrap();
        assert_eq!(create, "CREATE TABLE [t] (\n    [s1] int SPARSE NULL,\n    [cs] xml COLUMN_SET FOR ALL_SPARSE_COLUMNS\n);");
        // A column the CREATE doesn't have, or NOT NULL: refused, not lost.
        let mut create = "CREATE TABLE [t] (\n    [s1] int NOT NULL\n);".to_string();
        assert!(patch_sparse(&mut create, &t, &Extras { sparse: vec!["s1".into()], ..Default::default() }).is_err());
        // Nothing sparse: untouched.
        let mut create = "CREATE TABLE [t] (\n    [s1] int NULL\n);".to_string();
        patch_sparse(&mut create, &t, &Extras::default()).unwrap();
        assert_eq!(create, "CREATE TABLE [t] (\n    [s1] int NULL\n);");
    }

    #[test]
    fn the_clone_gets_its_own_history_table() {
        assert_eq!(history_name("temporal_hist", "temporal", "temporal_c").as_deref(), Some("temporal_c_hist"));
        assert_eq!(history_name("HistorialDeX", "temporal", "temporal_c").as_deref(), Some("temporal_c_history"));
        // Named by the server: the clone's too.
        assert_eq!(history_name("MSSQL_TemporalHistoryFor_1234567", "t", "t_c"), None);
        assert_eq!(history_name(&format!("x{}", "h".repeat(200)), "x", "y").map(|n| n.chars().count()), Some(128));
        assert_eq!(retention("6", "MONTH").as_deref(), Some("6 MONTHS"));
        assert_eq!(retention("1", "YEAR").as_deref(), Some("1 YEAR"));
        assert_eq!(retention("-1", "INFINITE"), None);
        assert_eq!(retention("", ""), None);
    }

    #[test]
    fn a_temporal_clone_is_dropped_with_its_history() {
        let t = TableSchema { schema: Some("ventas odd".into()), name: "temp'c".into(), ..Default::default() };
        let sql = drop_temporal_sql(&t);
        assert!(sql.starts_with("IF OBJECTPROPERTY(OBJECT_ID(N'[ventas odd].[temp''c]'), 'TableTemporalType') = 2\nBEGIN\n"), "{sql}");
        assert!(sql.contains("EXEC(N'ALTER TABLE [ventas odd].[temp''c] SET (SYSTEM_VERSIONING = OFF)');"), "{sql}");
        assert!(sql.contains("EXEC(N'DROP TABLE ' + @dbine_h);\nEND;\n"), "{sql}");
    }

    #[test]
    fn graph_tables_are_refused_in_spanish() {
        assert!(graph_reason(false, false).is_none());
        assert!(graph_reason(true, false).unwrap().contains("tabla de grafo de nodos (AS NODE)"));
        assert!(graph_reason(false, true).unwrap().contains("tabla de grafo de aristas (AS EDGE)"));
    }

    #[test]
    fn the_history_table_keeps_its_own_schema() {
        let t = |s: &str, n: &str| TableSchema { schema: Some(s.into()), name: n.into(), ..Default::default() };
        // The original's history in a schema of its own: the clone's too.
        assert_eq!(history_schema("hist ñ", &t("ventas odd", "tv3"), &t("ventas odd", "tv3 c")), "hist ñ");
        // In the original's schema: in the clone's.
        assert_eq!(history_schema("ventas odd", &t("ventas odd", "tv3"), &t("otro", "tv3 c")), "otro");
    }

    #[test]
    fn the_history_table_gets_the_originals_indexes() {
        let ix = |name: &str, cols: &[&str], kind: &str| IndexDef {
            name: name.into(),
            columns: cols.iter().map(|c| c.to_string()).collect(),
            kind: Some(kind.into()),
            ..Default::default()
        };
        let mut extra = ix("IX_tv3_hist_extra", &["code"], "NONCLUSTERED");
        extra.options.insert("DATA_COMPRESSION".into(), "ROW".into());
        let original = TableSchema {
            name: "tv3 hist".into(),
            indexes: vec![ix("ix_tv3 hist", &["vt", "vf"], "CLUSTERED"), extra.clone(), ix("IX_x", &["a"], "NONCLUSTERED")],
            ..Default::default()
        };
        let clone = TableSchema { name: "tv3 c hist".into(), indexes: vec![ix("ix_tv3 c hist", &["vt", "vf"], "CLUSTERED")], ..Default::default() };
        let (drop, create) = history_index_plan(&original, &clone);
        assert!(drop.is_empty());
        let names: Vec<&str> = create.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["IX_tv3_hist_extra", "IX_x"]);
        assert_eq!(create[0].options.get("DATA_COMPRESSION").map(String::as_str), Some("ROW"));
        // A clustered index other than the server's: the server's goes.
        let original = TableSchema { indexes: vec![ix("ix_tv3 hist", &["code"], "CLUSTERED")], ..original };
        let (drop, create) = history_index_plan(&original, &clone);
        assert_eq!(drop, ["ix_tv3 c hist"]);
        assert_eq!(create[0].name, "ix_tv3 c hist");
    }
}
