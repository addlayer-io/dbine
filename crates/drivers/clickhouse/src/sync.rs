//! Schema sync ("Comparar esquemas") for ClickHouse and Timeplus.
//!
//! Columns change with `ADD / DROP / MODIFY COLUMN` (nullability is part of
//! the type: `Nullable(T)`), comments with `COMMENT COLUMN`, defaults and
//! codecs that go away with `MODIFY COLUMN … REMOVE …`. Data-skipping
//! indexes and projections are dropped and added by name, and so are CHECK
//! and ASSUME constraints. The engine, the sorting key and
//! the partition key can't change in place: those only warn.

use crate::schema::{check_name, column_sql, full_type, index_clause, is_projection, q, qualified, string_literal, table_ddl, ASSUME};
use crate::Flavor;
use dbine_driver::{ColumnDef, DdlParts, IndexDef, Result, SyncScript, TableChange, TableSchema};

fn display(t: &TableSchema) -> String {
    match t.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

fn squash(s: &str) -> String {
    s.to_lowercase().split_whitespace().collect::<String>().replace('`', "")
}

fn opt<'a>(m: &'a std::collections::BTreeMap<String, String>, k: &str) -> Option<&'a str> {
    m.get(k).map(|s| s.trim()).filter(|s| !s.is_empty())
}

fn default_of(c: &ColumnDef) -> Option<String> {
    c.default_value.as_deref().map(str::trim).filter(|d| !d.is_empty()).map(str::to_string)
}

fn default_kind(c: &ColumnDef) -> String {
    opt(&c.options, "default_kind").unwrap_or("DEFAULT").to_uppercase()
}

/// `CODEC(ZSTD(1))` and `ZSTD(1)` are the same codec.
fn codec(c: &ColumnDef) -> Option<String> {
    let c = squash(opt(&c.options, "codec")?);
    Some(c.strip_prefix("codec(").and_then(|x| x.strip_suffix(')')).map(str::to_string).unwrap_or(c))
}

/// An index's type with the default granularity spelled out.
fn index_kind(ix: &IndexDef) -> String {
    let k = squash(ix.kind.as_deref().filter(|k| !k.trim().is_empty()).unwrap_or("minmax"));
    if k.contains("granularity") { k } else { format!("{k}granularity1") }
}

/// `old` is what the server reports; Timeplus reports `bloom_filter` for
/// `bloom_filter(0.01)`, so a type without arguments matches by name.
fn ix_same(old: &IndexDef, new: &IndexDef) -> bool {
    let cols = |x: &[String]| x.iter().map(|c| squash(c)).collect::<Vec<_>>();
    let (o, n) = (index_kind(old), index_kind(new));
    let base = |k: &str| k.split(['(', ' ']).next().unwrap_or("").to_string();
    let same_kind = o == n || (!o.contains('(') && n.contains('(') && base(&o).trim_end_matches("granularity1") == base(&n));
    cols(&old.columns) == cols(&new.columns) && same_kind
}

fn key_cols(t: &TableSchema) -> Vec<String> {
    t.primary_key.as_ref().map(|k| k.columns.iter().map(|c| squash(c)).collect()).unwrap_or_default()
}

#[derive(Default)]
struct Plan {
    drops: Vec<String>,
    pre: Vec<String>,
    columns: Vec<String>,
    post: Vec<String>,
    creates: Vec<String>,
    warnings: Vec<String>,
}

pub fn sync_script(flavor: Flavor, changes: &[TableChange]) -> Result<SyncScript> {
    let mut p = Plan::default();
    for ch in changes {
        match ch {
            TableChange::Create { table } => {
                p.creates.push(table_ddl(flavor, table, DdlParts { create: true, indexes: true, ..Default::default() }));
            }
            TableChange::Drop { table } => {
                p.warnings.push(format!("Se borra {} {} con todos sus datos.", noun(flavor), display(table)));
                p.drops.push(table_ddl(flavor, table, DdlParts { drop: true, ..Default::default() }));
            }
            TableChange::Alter { old, new } => alter(flavor, old, new, &mut p),
        }
    }
    let statements = [p.drops, p.pre, p.columns, p.post, p.creates].into_iter().flatten().filter(|s| !s.trim().is_empty()).collect();
    Ok(SyncScript { statements, warnings: p.warnings })
}

fn noun(flavor: Flavor) -> &'static str {
    match flavor {
        Flavor::ClickHouse => "la tabla",
        Flavor::Timeplus => "el stream",
    }
}

fn alter(flavor: Flavor, old: &TableSchema, new: &TableSchema, p: &mut Plan) {
    let what = match flavor {
        Flavor::ClickHouse => "TABLE",
        Flavor::Timeplus => "STREAM",
    };
    let name = qualified(new.schema.as_deref(), &new.name);
    let head = format!("ALTER {what} {name}");
    let tname = display(new);
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);

    // What ALTER can't change.
    let key = key_cols(old);
    if !key_cols(new).is_empty() && key_cols(new) != key {
        p.warnings.push(format!("{tname}: la clave primaria / de ordenamiento no se cambia con ALTER; hay que recrear {} y copiar los datos.", noun(flavor)));
    }
    for (k, label) in [("engine", "el motor"), ("order_by", "ORDER BY"), ("partition_by", "PARTITION BY"), ("sample_by", "SAMPLE BY"), ("mode", "el modo")] {
        if let Some(n) = opt(&new.options, k) {
            if opt(&old.options, k).map(squash) != Some(squash(n)) {
                p.warnings.push(format!("{tname}: {label} no se cambia con ALTER; hay que recrear {} y copiar los datos.", noun(flavor)));
            }
        }
    }
    let in_key = |c: &str| key.contains(&squash(c)) || opt(&old.options, "order_by").is_some_and(|o| squash(o).contains(&squash(c)));

    let dropped: Vec<&ColumnDef> = old.columns.iter().filter(|c| !new.columns.iter().any(|n| eq(&n.name, &c.name))).collect();
    // Timeplus streams only add columns: DROP / MODIFY / COMMENT COLUMN are "not allowed".
    let stream = flavor == Flavor::Timeplus;
    let dropped_ok: Vec<&&ColumnDef> = dropped.iter().filter(|c| !stream && !in_key(&c.name)).collect();
    for c in &dropped {
        if stream {
            p.warnings.push(format!("{tname}.{}: Timeplus no borra columnas de un stream; se deja.", c.name));
        } else if in_key(&c.name) {
            p.warnings.push(format!("{tname}.{} es parte de la clave: no se puede borrar sin recrear {}; se deja.", c.name, noun(flavor)));
        } else {
            p.warnings.push(format!("Se borra la columna {tname}.{} con sus datos.", c.name));
        }
    }

    // Indexes: by name; changed ones and the ones on dropped columns go first.
    for o in &old.indexes {
        let same = new.indexes.iter().find(|n| eq(&n.name, &o.name));
        let on_dropped = o.columns.iter().any(|c| dropped_ok.iter().any(|d| squash(c).contains(&squash(&d.name))));
        if same.is_none_or(|n| !ix_same(o, n)) || on_dropped {
            p.pre.push(format!("{head} DROP {} {};", if is_projection(o) { "PROJECTION" } else { "INDEX" }, q(&o.name)));
        }
    }
    // Constraints: CHECKs by name (and condition), ASSUMEs by name.
    let checks = |t: &TableSchema| -> Vec<(String, String)> {
        t.checks.iter().enumerate().map(|(i, c)| (check_name(t, i, c), dbine_driver::alter::check_expr(&c.expression))).collect()
    };
    let (old_ck, new_ck) = (checks(old), checks(new));
    let assumes = |t: &TableSchema| -> Vec<(String, String)> {
        t.options.iter().filter_map(|(k, v)| k.strip_prefix(ASSUME).map(|n| (n.to_string(), dbine_driver::alter::check_expr(v)))).collect()
    };
    let (old_as, new_as) = (assumes(old), assumes(new));
    for (n, e) in old_ck.iter().chain(&old_as) {
        if !new_ck.iter().chain(&new_as).any(|(m, f)| eq(m, n) && f == e) {
            p.pre.push(format!("{head} DROP CONSTRAINT {};", q(n)));
        }
    }

    for c in &dropped_ok {
        p.columns.push(format!("{head} DROP COLUMN {};", q(&c.name)));
    }
    for (i, c) in new.columns.iter().enumerate() {
        if old.columns.iter().any(|o| eq(&o.name, &c.name)) {
            continue;
        }
        let place = match i {
            _ if stream => String::new(),
            0 => " FIRST".to_string(),
            _ => format!(" AFTER {}", q(&new.columns[i - 1].name)),
        };
        p.columns.push(format!("{head} ADD COLUMN {}{place};", column_sql(flavor, c)));
    }
    for n in &new.columns {
        let Some(o) = old.columns.iter().find(|o| eq(&o.name, &n.name)) else { continue };
        let (ot, nt) = (full_type(flavor, o), full_type(flavor, n));
        let ty = squash(&ot) != squash(&nt);
        let default = default_of(o) != default_of(n) || (default_of(n).is_some() && default_kind(o) != default_kind(n));
        let codec_changed = codec(o) != codec(n);
        let comment = o.comment.as_deref().unwrap_or("") != n.comment.as_deref().unwrap_or("");
        let col = q(&n.name);
        if stream {
            if ty || default || codec_changed || comment {
                p.warnings.push(format!("{tname}.{}: Timeplus no modifica columnas de un stream; se deja como está.", n.name));
            }
            continue;
        }
        if ty {
            p.warnings.push(format!(
                "{tname}.{}: {ot} → {nt}. ClickHouse reescribe la columna (mutación): falla si algún valor no se convierte.",
                n.name
            ));
            if o.nullable && !n.nullable && default_of(n).is_none() {
                p.warnings.push(format!("{tname}.{} deja de admitir NULL: los NULL que haya pasan a ser el valor por defecto del tipo.", n.name));
            }
            if in_key(&n.name) {
                p.warnings.push(format!("{tname}.{} es parte de la clave: ClickHouse solo acepta cambios de tipo que no alteren los datos.", n.name));
            }
        }
        if default_of(o).is_some() && default_of(n).is_none() {
            p.columns.push(format!("{head} MODIFY COLUMN {col} REMOVE {};", default_kind(o)));
        }
        if codec(o).is_some() && codec(n).is_none() {
            p.columns.push(format!("{head} MODIFY COLUMN {col} REMOVE CODEC;"));
        }
        if ty || (default && default_of(n).is_some()) || (codec_changed && codec(n).is_some()) {
            // Nullable(T) → T needs a DEFAULT to fill the NULLs: the type's own, then removed.
            let fill = o.nullable && !n.nullable && default_of(n).is_none();
            let mut bare = ColumnDef { comment: None, ..n.clone() };
            if fill {
                bare.default_value = Some(format!("defaultValueOfTypeName({})", string_literal(&nt)));
                bare.options.remove("default_kind");
            }
            p.columns.push(format!("{head} MODIFY COLUMN {};", column_sql(flavor, &bare)));
            if fill {
                p.columns.push(format!("{head} MODIFY COLUMN {col} REMOVE DEFAULT;"));
            }
        }
        if comment {
            p.columns.push(format!("{head} COMMENT COLUMN {col} {};", string_literal(n.comment.as_deref().unwrap_or(""))));
        }
    }

    // New and changed indexes; they cover existing parts after MATERIALIZE.
    for n in &new.indexes {
        let same = old.indexes.iter().find(|o| eq(&o.name, &n.name));
        let on_dropped = same.is_some_and(|o| o.columns.iter().any(|c| dropped_ok.iter().any(|d| squash(c).contains(&squash(&d.name)))));
        if same.is_none_or(|o| !ix_same(o, n)) || on_dropped {
            p.post.push(format!("{head} ADD {};", index_clause(n)));
            p.post.push(format!("{head} MATERIALIZE {} {};", if is_projection(n) { "PROJECTION" } else { "INDEX" }, q(&n.name)));
        }
    }
    for (i, c) in new.checks.iter().enumerate() {
        let (n, e) = &new_ck[i];
        if !old_ck.iter().any(|(m, f)| eq(m, n) && f == e) {
            p.post.push(format!("{head} ADD CONSTRAINT {} CHECK {};", q(n), c.expression.trim()));
            p.warnings.push(format!("{tname}: la restricción CHECK {n} solo se verifica en las filas que se inserten desde ahora."));
        }
    }
    for (k, v) in &new.options {
        let Some(n) = k.strip_prefix(ASSUME) else { continue };
        if !old_as.iter().any(|(m, f)| eq(m, n) && *f == dbine_driver::alter::check_expr(v)) {
            p.post.push(format!("{head} ADD CONSTRAINT {} ASSUME {};", q(n), v.trim()));
        }
    }
    if old.comment.as_deref().unwrap_or("") != new.comment.as_deref().unwrap_or("") {
        p.post.push(format!("{head} MODIFY COMMENT {};", string_literal(new.comment.as_deref().unwrap_or(""))));
    }
    match (opt(&old.options, "ttl"), opt(&new.options, "ttl")) {
        (o, Some(n)) if o.map(squash) != Some(squash(n)) => p.post.push(format!("{head} MODIFY TTL {n};")),
        (Some(_), None) => p.post.push(format!("{head} REMOVE TTL;")),
        _ => {}
    }
    if opt(&new.options, "settings").is_some_and(|n| opt(&old.options, "settings").map(squash) != Some(squash(n))) {
        p.warnings.push(format!("{tname}: los SETTINGS de {} no se sincronizan; revisalos a mano (MODIFY SETTING).", noun(flavor)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::KeyDef;

    fn col(name: &str, ty: &str, nullable: bool) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable, ..Default::default() }
    }

    fn t() -> TableSchema {
        TableSchema {
            kind: "table".into(),
            schema: Some("db".into()),
            name: "ev".into(),
            columns: vec![col("id", "UInt64", false), col("nombre", "String", true), col("baja", "Date", true)],
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }),
            options: [("engine".to_string(), "MergeTree".to_string())].into(),
            indexes: vec![IndexDef { name: "ix_n".into(), columns: vec!["nombre".into()], kind: Some("bloom_filter(0.01)".into()), ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn alter_columns_and_indexes() {
        let old = t();
        let mut new = t();
        new.columns[1].nullable = false;
        new.columns[1].data_type = "LowCardinality(String)".into();
        new.columns[1].default_value = Some("''".into());
        new.columns[1].comment = Some("it's".into());
        new.columns.remove(2);
        new.columns.push(col("email", "String", true));
        new.indexes[0].kind = Some("set(100)".into());
        new.comment = Some("Eventos".into());
        let s = sync_script(Flavor::ClickHouse, &[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE `db`.`ev` DROP INDEX `ix_n`;",
                "ALTER TABLE `db`.`ev` DROP COLUMN `baja`;",
                "ALTER TABLE `db`.`ev` ADD COLUMN `email` Nullable(String) AFTER `nombre`;",
                "ALTER TABLE `db`.`ev` MODIFY COLUMN `nombre` LowCardinality(String) DEFAULT '';",
                "ALTER TABLE `db`.`ev` COMMENT COLUMN `nombre` 'it\\'s';",
                "ALTER TABLE `db`.`ev` ADD INDEX `ix_n` nombre TYPE set(100) GRANULARITY 1;",
                "ALTER TABLE `db`.`ev` MATERIALIZE INDEX `ix_n`;",
                "ALTER TABLE `db`.`ev` MODIFY COMMENT 'Eventos';",
            ]
        );
        assert_eq!(s.warnings.len(), 2, "{:?}", s.warnings);
    }

    #[test]
    fn nullability_default_and_key() {
        let mut old = t();
        old.columns[1].default_value = Some("'x'".into());
        old.columns[1].options.insert("codec".into(), "CODEC(ZSTD(1))".into());
        let mut new = old.clone();
        new.columns[1].default_value = None;
        new.columns[1].options.insert("codec".into(), "ZSTD(1)".into());
        new.columns[2].nullable = false;
        new.columns.remove(0);
        new.primary_key = Some(KeyDef { name: None, columns: vec!["nombre".into()] });
        let s = sync_script(Flavor::ClickHouse, &[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE `db`.`ev` MODIFY COLUMN `nombre` REMOVE DEFAULT;",
                "ALTER TABLE `db`.`ev` MODIFY COLUMN `baja` Date DEFAULT defaultValueOfTypeName('Date');",
                "ALTER TABLE `db`.`ev` MODIFY COLUMN `baja` REMOVE DEFAULT;",
            ]
        );
        let w = s.warnings.join("\n");
        assert!(w.contains("clave primaria") && w.contains("no se puede borrar") && w.contains("NULL"), "{w}");
    }

    #[test]
    fn constraints_and_projections() {
        use dbine_driver::CheckDef;
        let proj = |q: &str| IndexDef { name: "p".into(), columns: vec![q.into()], kind: Some("PROJECTION".into()), ..Default::default() };
        let mut old = t();
        old.checks = vec![CheckDef { name: Some("c1".into()), expression: "id > 0".into() }, CheckDef { name: Some("c2".into()), expression: "id < 9".into() }];
        old.options.insert("assume:a1".into(), "id > 1".into());
        old.indexes.push(proj("(SELECT nombre ORDER BY baja)"));
        let mut new = old.clone();
        new.checks[1].expression = "(id < 10)".into();
        new.checks[0].expression = "(id > 0)".into();
        new.options.insert("assume:a1".into(), "id > 2".into());
        new.indexes[1] = proj("(SELECT nombre, count() GROUP BY nombre)");
        let s = sync_script(Flavor::ClickHouse, &[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE `db`.`ev` DROP PROJECTION `p`;",
                "ALTER TABLE `db`.`ev` DROP CONSTRAINT `c2`;",
                "ALTER TABLE `db`.`ev` DROP CONSTRAINT `a1`;",
                "ALTER TABLE `db`.`ev` ADD PROJECTION `p` (SELECT nombre, count() GROUP BY nombre);",
                "ALTER TABLE `db`.`ev` MATERIALIZE PROJECTION `p`;",
                "ALTER TABLE `db`.`ev` ADD CONSTRAINT `c2` CHECK (id < 10);",
                "ALTER TABLE `db`.`ev` ADD CONSTRAINT `a1` ASSUME id > 2;",
            ]
        );
    }

    #[test]
    fn index_types_without_arguments_match() {
        let ix = |k: &str| IndexDef { name: "i".into(), columns: vec!["c".into()], kind: Some(k.into()), ..Default::default() };
        assert!(ix_same(&ix("bloom_filter"), &ix("bloom_filter(0.01)")));
        assert!(ix_same(&ix("minmax"), &ix("minmax GRANULARITY 1")));
        assert!(!ix_same(&ix("minmax"), &ix("set(100)")));
        assert!(!ix_same(&ix("bloom_filter(0.05)"), &ix("bloom_filter(0.01)")));
    }

    #[test]
    fn create_drop_and_timeplus() {
        let mut other = t();
        other.name = "vieja".into();
        let s = sync_script(Flavor::ClickHouse, &[TableChange::Create { table: t() }, TableChange::Drop { table: other }]).unwrap();
        assert_eq!(s.statements[0], "DROP TABLE `db`.`vieja`;");
        assert!(s.statements[1].starts_with("CREATE TABLE `db`.`ev`") && s.statements[1].contains("INDEX `ix_n`"), "{}", s.statements[1]);

        let old = TableSchema { kind: "stream".into(), options: Default::default(), indexes: vec![], primary_key: None, ..t() };
        let mut new = old.clone();
        new.columns[1].data_type = "string".into();
        new.columns[1].nullable = false;
        new.columns.remove(2);
        new.columns.push(col("email", "string", true));
        let s = sync_script(Flavor::Timeplus, &[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(s.statements, vec!["ALTER STREAM `db`.`ev` ADD COLUMN `email` nullable(string);"]);
        assert_eq!(s.warnings.len(), 2, "{:?}", s.warnings);
    }
}
