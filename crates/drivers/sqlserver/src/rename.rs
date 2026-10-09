//! "Renombrar…": `sp_rename` for tables, modules, columns, indexes and
//! constraints. The app rewrites and puts back the code that names the
//! target (`CREATE OR ALTER`, or drop and create on Babelfish).
//!
//! - A module (view, procedure, function, trigger) keeps its old name in
//!   `sys.sql_modules` after `sp_rename`, so it's written again with its
//!   header renamed.
//! - SQL Server refuses to rename a column that a CHECK constraint, a
//!   computed column or a filtered index uses ("enforced dependencies"):
//!   the CHECKs and filtered indexes are dropped and created again around
//!   it; a computed column is refused.
//! - Babelfish (5.4) takes `sp_rename` for tables, views, procedures,
//!   functions and columns. It refuses constraints, crashes on triggers,
//!   loses an index once its table was renamed (the PostgreSQL index name
//!   carries a hash of the table's) and has no `CREATE OR ALTER` for
//!   procedures or functions.
//!   PostgreSQL underneath follows a renamed column in CHECKs and computed
//!   columns by itself.
//! - Fabric (not verified on a live warehouse): tables and columns only.
//!
//! A database is renamed with `ALTER DATABASE … MODIFY NAME`, run from
//! `master` (see [`database_script`]).

use crate::variant::{self, Variant};
use dbine_driver::rename::{rename_header, Fold, ReferenceStyle, RenameRequest, RenameSpec, RenameTarget, ReplaceStyle};
use dbine_driver::sql::{name_tokens, qualified_name, quote_ident, split_script, Quote, ScriptDialect, TokenKind};
use dbine_driver::{kinds, Error, IndexDef, ObjectRef, Result, SyncScript, TableSchema};

const MODULES: [&str; 4] = [kinds::VIEW, kinds::PROCEDURE, kinds::FUNCTION, kinds::TRIGGER];

pub(crate) fn spec(v: Variant) -> RenameSpec {
    let note = database_note(v);
    let base = RenameSpec {
        databases: note.is_some(),
        database_from: note.is_some().then(|| "master".to_string()),
        database_note: note.map(str::to_string),
        database_moves: false,
        replace: ReplaceStyle::CreateOrAlter,
        references: ReferenceStyle::Sql,
        fold: Fold::None,
        ..Default::default()
    };
    let kinds = |ks: &[&str]| ks.iter().map(|k| k.to_string()).collect::<Vec<_>>();
    match v {
        Variant::SqlServer | Variant::AzureSql => RenameSpec {
            kinds: kinds(&[kinds::TABLE, kinds::VIEW, kinds::PROCEDURE, kinds::FUNCTION, kinds::TRIGGER]),
            columns: true,
            indexes: true,
            constraints: true,
            transactional: true,
            note: Some(
                "Se renombra con sp_rename. Las claves foráneas, los índices y las restricciones siguen al objeto solos; el texto de las vistas, \
                 procedimientos, funciones y triggers que lo nombran no cambia, así que se reescribe y se repone con CREATE OR ALTER, que conserva \
                 los permisos."
                    .into(),
            ),
            ..base
        },
        Variant::Fabric => RenameSpec {
            kinds: kinds(&[kinds::TABLE]),
            columns: true,
            note: Some(
                "Fabric renombra tablas y columnas con sp_rename. El código que las nombra se repone con CREATE OR ALTER. El script no corre en \
                 una transacción: si una sentencia falla, lo anterior queda hecho."
                    .into(),
            ),
            ..base
        },
        Variant::Babelfish => RenameSpec {
            kinds: kinds(&[kinds::TABLE, kinds::VIEW, kinds::PROCEDURE, kinds::FUNCTION]),
            columns: true,
            replace: ReplaceStyle::DropCreate,
            // Views do take CREATE OR ALTER, and keep their grants.
            replace_kinds: [(kinds::VIEW.to_string(), ReplaceStyle::CreateOrAlter)].into(),
            transactional: true,
            note: Some(
                "Babelfish renombra con sp_rename. Las vistas que lo nombran se reponen con CREATE OR ALTER y conservan sus permisos, pero los \
                 procedimientos y las funciones no: se borran antes y se vuelven a crear después, y pierden los permisos otorgados. No renombra \
                 triggers, índices ni restricciones."
                    .into(),
            ),
            ..base
        },
    }
}

fn q(name: &str) -> String {
    quote_ident(Quote::Bracket, name)
}

/// `N'…'`.
fn nlit(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

fn qn(schema: Option<&str>, name: &str) -> String {
    qualified_name(Quote::Bracket, schema.filter(|s| !s.is_empty()), name)
}

fn sp_rename(objname: &str, new_name: &str, objtype: &str) -> String {
    // @newname is taken literally: brackets there would end up in the name.
    format!("EXEC sp_rename {}, {}, N'{objtype}';", nlit(objname), nlit(new_name))
}

/// Why `target` isn't renamed on `v`.
fn refusal(v: Variant, target: &RenameTarget) -> Error {
    let engine = variant::info(v).name;
    Error::Unsupported(match target {
        RenameTarget::Schema { .. } => format!("{engine} no renombra esquemas: hay que crear el esquema nuevo y pasarle los objetos con ALTER SCHEMA … TRANSFER"),
        RenameTarget::Object { object, .. } if object.kind == kinds::SYNONYM => format!("{engine} no renombra sinónimos: hay que borrarlo y crearlo con el nombre nuevo"),
        RenameTarget::Object { object, .. } if object.kind == kinds::TRIGGER => format!("{engine} no renombra triggers"),
        RenameTarget::Object { .. } => format!("{engine} no renombra ese tipo de objeto"),
        RenameTarget::Column { .. } => format!("{engine} no renombra columnas"),
        RenameTarget::Index { .. } => format!("{engine} no renombra índices"),
        RenameTarget::Constraint { .. } => format!("{engine} no renombra restricciones"),
    })
}

pub(crate) fn script(v: Variant, req: &RenameRequest) -> Result<SyncScript> {
    if !spec(v).allows(&req.target) {
        return Err(refusal(v, &req.target));
    }
    let new = req.new_name.as_str();
    match &req.target {
        RenameTarget::Object { object, .. } if object.kind == kinds::TABLE || !MODULES.contains(&object.kind.as_str()) => {
            Ok(SyncScript { statements: vec![sp_rename(&qn(object.schema(), &object.name), new, "OBJECT")], warnings: Vec::new() })
        }
        RenameTarget::Object { object, .. } => module(v, object, req),
        RenameTarget::Column { table, column } => {
            if matches!(v, Variant::SqlServer | Variant::AzureSql) {
                column_with_dependencies(table, column, new, req.table.as_ref())
            } else {
                Ok(SyncScript { statements: vec![sp_rename(&format!("{}.{}", qn(table.schema(), &table.name), q(column)), new, "COLUMN")], warnings: Vec::new() })
            }
        }
        RenameTarget::Index { table, index } => {
            Ok(SyncScript { statements: vec![sp_rename(&format!("{}.{}", qn(table.schema(), &table.name), q(index)), new, "INDEX")], warnings: Vec::new() })
        }
        // Constraints live in the table's schema, as objects.
        RenameTarget::Constraint { table, constraint } => Ok(SyncScript { statements: vec![sp_rename(&qn(table.schema(), constraint), new, "OBJECT")], warnings: Vec::new() }),
        RenameTarget::Schema { .. } => Err(refusal(v, &req.target)),
    }
}

/// A view, procedure, function or trigger: `sp_rename`, then its text with
/// the new name in its header (`CREATE OR ALTER`, which keeps permissions).
/// Babelfish has no `CREATE OR ALTER` for procedures and functions: those
/// are dropped and created.
fn module(v: Variant, object: &ObjectRef, req: &RenameRequest) -> Result<SyncScript> {
    let d = ScriptDialect::tsql();
    let definition = req.definition.as_deref().filter(|s| !s.trim().is_empty()).ok_or_else(|| {
        Error::Unsupported(format!(
            "no se pudo leer la definición de «{}» (¿está cifrada?): sp_rename no cambia el texto guardado, así que no se renombra desde DBine",
            object.name
        ))
    })?;
    let renamed = rename_header(definition, &d, Fold::None, &req.new_name)
        .ok_or_else(|| Error::Unsupported(format!("no se encontró el encabezado CREATE de «{}» en su definición", object.name)))?;
    let renamed = qualify(&renamed, object.schema());
    let old = qn(object.schema(), &object.name);
    if v == Variant::Babelfish && object.kind != kinds::VIEW {
        let kind = if object.kind == kinds::PROCEDURE { "PROCEDURE" } else { "FUNCTION" };
        return Ok(SyncScript {
            statements: vec![format!("DROP {kind} {old};"), one_batch(create_as(&renamed, &d, false), &object.name, &d)?],
            warnings: vec![format!(
                "Babelfish no repone procedimientos ni funciones con CREATE OR ALTER: «{}» se borra y se crea con el nombre nuevo, y pierde los permisos otorgados sobre él.",
                object.name
            )],
        });
    }
    Ok(SyncScript { statements: vec![sp_rename(&old, &req.new_name, "OBJECT"), one_batch(create_as(&renamed, &d, true), &object.name, &d)?], warnings: Vec::new() })
}

/// A stored definition put back must be one batch: a line reading just `GO`
/// inside it (in a string, a comment or the code) would cut it into
/// batches that run on their own. Such a module isn't renamed from DBine.
fn one_batch(sql: String, name: &str, d: &ScriptDialect) -> Result<String> {
    if split_script(&sql, d).len() > 1 {
        return Err(Error::Unsupported(format!(
            "la definición de «{name}» tiene una línea que dice solo GO: al volver a crearla se partiría en varios lotes, así que no se renombra desde DBine"
        )));
    }
    Ok(sql)
}

fn create_as(definition: &str, d: &ScriptDialect, or_alter: bool) -> String {
    let style = if or_alter { ReplaceStyle::CreateOrAlter } else { ReplaceStyle::DropCreate };
    dbine_driver::rename::with_create_style(definition, d, style).trim().to_string()
}

/// The header's name with its schema: an unqualified `CREATE VIEW v` would
/// otherwise go to the default schema of whoever runs the script.
fn qualify(definition: &str, schema: Option<&str>) -> String {
    let Some(schema) = schema.filter(|s| !s.is_empty()) else { return definition.to_string() };
    let toks = name_tokens(definition, &ScriptDialect::tsql());
    let word = |i: usize, ws: &[&str]| toks.get(i).is_some_and(|t| t.kind == TokenKind::Name && ws.iter().any(|w| t.text.eq_ignore_ascii_case(w)));
    let Some(c) = (0..toks.len()).find(|&i| word(i, &["create"])) else { return definition.to_string() };
    let mut i = c + 1;
    if word(i, &["or"]) && word(i + 1, &["alter"]) {
        i += 2;
    }
    if !word(i, &["view", "proc", "procedure", "function", "trigger"]) {
        return definition.to_string();
    }
    let name = i + 1;
    let dotted = toks.get(name + 1).is_some_and(|t| t.kind == TokenKind::Punct && t.text == ".");
    match toks.get(name) {
        Some(t) if t.kind == TokenKind::Name && !dotted => format!("{}{}.{}", &definition[..t.start], q(schema), &definition[t.start..]),
        _ => definition.to_string(),
    }
}

/// `[old]` → `[new]` in an expression as SQL Server stores it (CHECKs,
/// computed columns and index filters come back with every column in
/// brackets). ASCII case-insensitive, like the usual collations.
fn swap_column(expr: &str, old: &str, new: &str) -> String {
    let needle = q(old).to_ascii_lowercase();
    let hay = expr.to_ascii_lowercase();
    let mut out = String::with_capacity(expr.len());
    let mut at = 0;
    while let Some(p) = hay[at..].find(&needle) {
        out.push_str(&expr[at..at + p]);
        out.push_str(&q(new));
        at += p + needle.len();
    }
    out.push_str(&expr[at..]);
    out
}

fn uses(expr: &str, column: &str) -> bool {
    expr.to_ascii_lowercase().contains(&q(column).to_ascii_lowercase())
}

/// SQL Server and Azure SQL: a column used by a CHECK constraint, a
/// computed column or a filtered index can't be renamed. The CHECKs (read
/// from the catalog when it runs: `database_schema` doesn't list them) and
/// the filtered indexes are dropped and created again around `sp_rename`,
/// in one batch that undoes itself if anything fails.
fn column_with_dependencies(table: &ObjectRef, column: &str, new: &str, schema: Option<&TableSchema>) -> Result<SyncScript> {
    let owner = qn(table.schema(), &table.name);
    let computed: Vec<&str> = schema
        .map(|t| t.columns.iter().filter(|c| c.data_type.trim_start().to_ascii_uppercase().starts_with("AS ") && uses(&c.data_type, column)).map(|c| c.name.as_str()).collect())
        .unwrap_or_default();
    if !computed.is_empty() {
        let names = computed.iter().map(|c| format!("«{c}»")).collect::<Vec<_>>().join(", ");
        return Err(Error::Unsupported(format!(
            "SQL Server no deja renombrar «{column}» porque la usa la columna calculada {names}: hay que quitar la columna calculada, renombrar y volver a crearla"
        )));
    }
    let filtered: Vec<&IndexDef> = schema.map(|t| t.indexes.iter().filter(|i| i.filter.as_deref().is_some_and(|f| uses(f, column))).collect()).unwrap_or_default();
    let mut warnings = vec![format!(
        "Las restricciones CHECK que usan «{column}» se borran antes de renombrarla y se vuelven a crear con el nombre nuevo: SQL Server no deja renombrar una columna que usan."
    )];
    let mut drop_ix = String::new();
    let mut create_ix = String::new();
    for ix in &filtered {
        drop_ix.push_str(&format!("    DROP INDEX {} ON {owner};\n", q(&ix.name)));
        let rename = |cols: &[String]| cols.iter().map(|c| if c.eq_ignore_ascii_case(column) { new.to_string() } else { c.clone() }).collect::<Vec<_>>();
        let moved = IndexDef {
            columns: rename(&ix.columns),
            include: rename(&ix.include),
            filter: ix.filter.as_deref().map(|f| swap_column(f, column, new)),
            ..(*ix).clone()
        };
        let one = TableSchema { schema: table.schema.clone(), name: table.name.clone(), indexes: vec![moved], ..Default::default() };
        for s in crate::structure::index_statements(&owner, &one, false) {
            let s = s.trim().trim_end_matches(';');
            // Through EXEC: the batch is compiled before sp_rename runs,
            // when the new column doesn't exist yet.
            create_ix.push_str(&format!("    EXEC ({});\n", nlit(s)));
        }
        warnings.push(format!("El índice filtrado «{}» usa la columna en su filtro: se borra antes de renombrarla y se vuelve a crear después.", ix.name));
    }
    let ck_owner = owner.replace('\'', "''");
    let batch = format!(
        "BEGIN TRY
    BEGIN TRANSACTION;
    DECLARE @checks TABLE (name sysname, definition nvarchar(max), disabled bit, untrusted bit, nfr bit);
    INSERT INTO @checks
    SELECT cc.name, cc.definition, cc.is_disabled, cc.is_not_trusted, cc.is_not_for_replication
      FROM sys.check_constraints cc
     WHERE cc.parent_object_id = OBJECT_ID({obj})
       AND EXISTS (SELECT 1 FROM sys.sql_expression_dependencies d
                    WHERE d.referencing_id = cc.object_id AND d.referenced_id = cc.parent_object_id
                      AND d.referenced_minor_id = COLUMNPROPERTY(cc.parent_object_id, {col}, 'ColumnId'));
    DECLARE @sql nvarchar(max) = N'';
    SELECT @sql += N'ALTER TABLE {ck_owner} DROP CONSTRAINT ' + QUOTENAME(name) + N';' FROM @checks;
    EXEC (@sql);
{drop_ix}    {rename}
{create_ix}    SET @sql = N'';
    SELECT @sql += N'ALTER TABLE {ck_owner} WITH ' + IIF(untrusted = 1, N'NOCHECK', N'CHECK') + N' ADD CONSTRAINT ' + QUOTENAME(name)
                 + N' CHECK ' + IIF(nfr = 1, N'NOT FOR REPLICATION ', N'') + REPLACE(definition, {old_b}, {new_b}) + N';'
                 + IIF(disabled = 1, N'ALTER TABLE {ck_owner} NOCHECK CONSTRAINT ' + QUOTENAME(name) + N';', N'')
      FROM @checks;
    EXEC (@sql);
    COMMIT TRANSACTION;
END TRY
BEGIN CATCH
    IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;
    THROW;
END CATCH;",
        obj = nlit(&owner),
        col = nlit(column),
        rename = sp_rename(&format!("{owner}.{}", q(column)), new, "COLUMN"),
        old_b = nlit(&q(column)),
        new_b = nlit(&q(new)),
    );
    Ok(SyncScript { statements: vec![batch], warnings })
}

/// The system databases, never renamed (Babelfish has no `model`).
const SYSTEM_DATABASES: [&str; 4] = ["master", "model", "msdb", "tempdb"];

/// What the dialog says first about renaming a database; `None` where it
/// isn't offered (a Fabric warehouse is renamed in the Fabric portal).
fn database_note(v: Variant) -> Option<&'static str> {
    match v {
        Variant::SqlServer => Some(
            "Se renombra con ALTER DATABASE … MODIFY NAME, desde master. Antes la base pasa a SINGLE_USER WITH ROLLBACK IMMEDIATE, que deshace \
             las transacciones abiertas y corta todas las demás sesiones conectadas a ella, y al final vuelve a MULTI_USER. El código de otras \
             bases, los jobs del Agente, los servidores vinculados, los sinónimos y las cadenas de conexión que usan el nombre viejo no cambian. \
             No es atómico: si una sentencia falla, lo anterior queda hecho.",
        ),
        Variant::AzureSql => Some(
            "Se renombra con ALTER DATABASE … MODIFY NAME, desde master. Azure SQL Database no admite SINGLE_USER: las demás sesiones \
             conectadas a la base se cortan al renombrarla, sin aviso previo. El código de otras bases, las consultas elásticas, las \
             reglas de firewall de la base y las cadenas de conexión que usan el nombre viejo no cambian. No es atómico: si una sentencia \
             falla, lo anterior queda hecho.",
        ),
        Variant::Babelfish => Some(
            "Se renombra con ALTER DATABASE … MODIFY NAME, desde master. Babelfish no admite SINGLE_USER ni corta las demás sesiones: \
             mientras haya otra conectada a la base, se niega («The database could not be exclusively locked»), así que hay que cerrarlas \
             antes. El código de otras bases y las cadenas de conexión que usan el nombre viejo no cambian. No es atómico: si una sentencia \
             falla, lo anterior queda hecho.",
        ),
        Variant::Fabric => None,
    }
}

/// Renaming `database` to `new_name`, run from `master`:
///
/// - SQL Server: `SET SINGLE_USER WITH ROLLBACK IMMEDIATE` (as SSMS's
///   "close existing connections"), `MODIFY NAME`, then `SET MULTI_USER`
///   on the new name.
/// - Azure SQL Database and Babelfish: `MODIFY NAME` alone; neither takes
///   `SINGLE_USER`. Babelfish 5.4 answers "'ALTER DATABASE' is not
///   currently supported" to it, and refuses `MODIFY NAME` while another
///   session is connected to the database ("could not be exclusively
///   locked").
pub(crate) fn database_script(v: Variant, database: &str, new_name: &str) -> Result<SyncScript> {
    let engine = variant::info(v).name;
    if database_note(v).is_none() {
        return Err(Error::Unsupported(format!("{engine} no renombra bases desde T-SQL: un warehouse se renombra desde el portal de Fabric")));
    }
    let new = new_name.trim();
    if database.trim().is_empty() || new.is_empty() {
        return Err(Error::Query("falta el nombre de la base".into()));
    }
    if new.chars().count() > 128 {
        return Err(Error::Query("el nombre de una base no puede pasar de 128 caracteres".into()));
    }
    if new == database {
        return Err(Error::Query("el nombre nuevo es igual al actual".into()));
    }
    for name in [database, new] {
        if SYSTEM_DATABASES.iter().any(|s| s.eq_ignore_ascii_case(name)) {
            return Err(Error::Unsupported(format!("«{name}» es una base del sistema: no se renombra ni se usa como nombre nuevo")));
        }
    }
    let (old_q, new_q) = (q(database), q(new));
    let rename = format!("ALTER DATABASE {old_q} MODIFY NAME = {new_q}");
    let mut warnings = Vec::new();
    let statements = if v == Variant::SqlServer {
        warnings.push(format!(
            "SINGLE_USER WITH ROLLBACK IMMEDIATE deshace las transacciones abiertas en «{database}» y corta todas las demás sesiones conectadas a ella."
        ));
        warnings.push(format!(
            "Si el cambio de nombre falla (por ejemplo, otra sesión ocupó el único lugar de SINGLE_USER, o la base está en un grupo de \
             disponibilidad o en un mirroring), «{database}» queda en SINGLE_USER: se vuelve con ALTER DATABASE {old_q} SET MULTI_USER."
        ));
        warnings.push("Al final la base queda en MULTI_USER, aunque antes estuviera en RESTRICTED_USER.".into());
        vec![format!("ALTER DATABASE {old_q} SET SINGLE_USER WITH ROLLBACK IMMEDIATE"), rename, format!("ALTER DATABASE {new_q} SET MULTI_USER")]
    } else {
        warnings.push(match v {
            Variant::AzureSql => format!("Azure SQL Database corta las demás sesiones conectadas a «{database}» al renombrarla."),
            _ => format!("Babelfish no corta las demás sesiones: si queda alguna conectada a «{database}», el cambio de nombre se niega."),
        });
        vec![rename]
    };
    if v == Variant::SqlServer {
        warnings.push("Los nombres lógicos de los archivos y los archivos de datos y de log conservan el nombre viejo.".into());
        warnings.push(format!(
            "El código de otras bases, los jobs del Agente, los servidores vinculados, los sinónimos y las cadenas de conexión que nombran «{database}» no cambian."
        ));
    } else {
        warnings.push(format!("El código de otras bases y las cadenas de conexión que nombran «{database}» no cambian."));
    }
    Ok(SyncScript { statements, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ColumnDef;

    fn obj(kind: &str, schema: &str, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: Some(schema.into()), name: name.into() }
    }

    fn req(target: RenameTarget, new: &str) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table: None, definition: None }
    }

    fn object(kind: &str, name: &str) -> RenameTarget {
        RenameTarget::Object { object: obj(kind, "ventas", name), parent: None }
    }

    #[test]
    fn table_is_sp_rename_with_the_new_name_literal() {
        let s = script(Variant::SqlServer, &req(object(kinds::TABLE, "Clientes"), "Clientes Viejos")).unwrap();
        assert_eq!(s.statements, ["EXEC sp_rename N'[ventas].[Clientes]', N'Clientes Viejos', N'OBJECT';"]);
        // Quotes and brackets: the old name bracketed, the new one as is.
        let s = script(Variant::AzureSql, &req(object(kinds::TABLE, "a]b"), "O'Brien")).unwrap();
        assert_eq!(s.statements, ["EXEC sp_rename N'[ventas].[a]]b]', N'O''Brien', N'OBJECT';"]);
        // Case kept (the engine stores what it's given).
        let s = script(Variant::Fabric, &req(object(kinds::TABLE, "t"), "NuevoNombre")).unwrap();
        assert_eq!(s.statements, ["EXEC sp_rename N'[ventas].[t]', N'NuevoNombre', N'OBJECT';"]);
    }

    #[test]
    fn modules_are_renamed_then_written_again() {
        let mut r = req(object(kinds::VIEW, "vClientes"), "vNuevo");
        r.definition = Some("-- reads Clientes\nCREATE VIEW [ventas].[vClientes] WITH SCHEMABINDING AS SELECT id FROM ventas.Clientes".into());
        let s = script(Variant::SqlServer, &r).unwrap();
        assert_eq!(
            s.statements,
            [
                "EXEC sp_rename N'[ventas].[vClientes]', N'vNuevo', N'OBJECT';",
                "-- reads Clientes\nCREATE OR ALTER VIEW [ventas].[vNuevo] WITH SCHEMABINDING AS SELECT id FROM ventas.Clientes",
            ]
        );
        // Unqualified header: the schema is added; a name needing quotes is bracketed.
        let mut r = req(object(kinds::PROCEDURE, "p"), "p nuevo");
        r.definition = Some("create proc p @a int as select @a".into());
        let s = script(Variant::SqlServer, &r).unwrap();
        assert_eq!(s.statements[1], "CREATE OR ALTER proc [ventas].[p nuevo] @a int as select @a");
        // Already CREATE OR ALTER, a function.
        let mut r = req(object(kinds::FUNCTION, "f"), "g");
        r.definition = Some("CREATE OR ALTER FUNCTION ventas.f() RETURNS int AS BEGIN RETURN 1 END".into());
        assert_eq!(script(Variant::AzureSql, &r).unwrap().statements[1], "CREATE OR ALTER FUNCTION ventas.g() RETURNS int AS BEGIN RETURN 1 END");
        // A trigger keeps its ON clause.
        let mut r = RenameRequest {
            target: RenameTarget::Object { object: obj(kinds::TRIGGER, "dbo", "trg"), parent: Some("T".into()) },
            new_name: "trg2".into(),
            table: None,
            definition: Some("CREATE TRIGGER dbo.trg ON dbo.T AFTER INSERT AS SELECT 1".into()),
        };
        let s = script(Variant::SqlServer, &r).unwrap();
        assert_eq!(s.statements, ["EXEC sp_rename N'[dbo].[trg]', N'trg2', N'OBJECT';", "CREATE OR ALTER TRIGGER dbo.trg2 ON dbo.T AFTER INSERT AS SELECT 1"]);
        // Without its text (encrypted): refused.
        r.definition = None;
        assert!(matches!(script(Variant::SqlServer, &r), Err(Error::Unsupported(m)) if m.contains("cifrada")));
    }

    #[test]
    fn babelfish_drops_and_creates_routines() {
        let mut r = req(object(kinds::PROCEDURE, "p"), "p2");
        r.definition = Some("CREATE PROCEDURE ventas.p AS SELECT 1".into());
        let s = script(Variant::Babelfish, &r).unwrap();
        assert_eq!(s.statements, ["DROP PROCEDURE [ventas].[p];", "CREATE PROCEDURE ventas.p2 AS SELECT 1"]);
        assert!(s.warnings[0].contains("permisos"));
        let mut r = req(object(kinds::VIEW, "v"), "v2");
        r.definition = Some("CREATE VIEW ventas.v AS SELECT 1 AS a".into());
        let s = script(Variant::Babelfish, &r).unwrap();
        assert_eq!(s.statements, ["EXEC sp_rename N'[ventas].[v]', N'v2', N'OBJECT';", "CREATE OR ALTER VIEW ventas.v2 AS SELECT 1 AS a"]);
        assert!(s.warnings.is_empty());
    }

    #[test]
    fn index_and_constraint() {
        let t = obj(kinds::TABLE, "dbo", "T");
        let s = script(Variant::SqlServer, &req(RenameTarget::Index { table: t.clone(), index: "IX_Pepe".into() }, "IX_Nuevo")).unwrap();
        assert_eq!(s.statements, ["EXEC sp_rename N'[dbo].[T].[IX_Pepe]', N'IX_Nuevo', N'INDEX';"]);
        let s = script(Variant::SqlServer, &req(RenameTarget::Constraint { table: t.clone(), constraint: "CK_Pepe".into() }, "CK_Nuevo")).unwrap();
        assert_eq!(s.statements, ["EXEC sp_rename N'[dbo].[CK_Pepe]', N'CK_Nuevo', N'OBJECT';"]);
    }

    #[test]
    fn column_keeps_checks_and_filtered_indexes() {
        let t = obj(kinds::TABLE, "dbo", "T");
        let target = RenameTarget::Column { table: t.clone(), column: "pepe".into() };
        // Babelfish and Fabric: just sp_rename.
        for v in [Variant::Babelfish, Variant::Fabric] {
            let s = script(v, &req(target.clone(), "Nuevo")).unwrap();
            assert_eq!(s.statements, ["EXEC sp_rename N'[dbo].[T].[pepe]', N'Nuevo', N'COLUMN';"]);
        }
        let mut r = req(target.clone(), "Nuevo");
        r.table = Some(TableSchema {
            schema: Some("dbo".into()),
            name: "T".into(),
            columns: vec![ColumnDef { name: "pepe".into(), data_type: "int".into(), ..Default::default() }],
            indexes: vec![
                IndexDef { name: "IX_f".into(), columns: vec!["id".into()], include: vec!["Pepe".into()], filter: Some("([PEPE]>(5))".into()), ..Default::default() },
                IndexDef { name: "IX_plain".into(), columns: vec!["pepe".into()], ..Default::default() },
            ],
            ..Default::default()
        });
        let s = script(Variant::SqlServer, &r).unwrap();
        assert_eq!(s.statements.len(), 1);
        let b = &s.statements[0];
        assert!(b.contains("EXEC sp_rename N'[dbo].[T].[pepe]', N'Nuevo', N'COLUMN';"), "{b}");
        assert!(b.contains("COLUMNPROPERTY(cc.parent_object_id, N'pepe', 'ColumnId')"), "{b}");
        assert!(b.contains("REPLACE(definition, N'[pepe]', N'[Nuevo]')"), "{b}");
        assert!(b.contains("DROP INDEX [IX_f] ON [dbo].[T];"), "{b}");
        assert!(b.contains("EXEC (N'CREATE INDEX [IX_f] ON [dbo].[T] ([id]) INCLUDE ([Nuevo]) WHERE ([Nuevo]>(5))');"), "{b}");
        assert!(!b.contains("IX_plain"), "{b}");
        let (drop, rename, create) = (b.find("DROP INDEX").unwrap(), b.find("sp_rename").unwrap(), b.find("CREATE").unwrap());
        assert!(drop < rename && rename < create, "{b}");
        assert_eq!(s.warnings.len(), 2);
        // A computed column using it: refused.
        r.table.as_mut().unwrap().columns.push(ColumnDef { name: "doble".into(), data_type: "AS ([pepe]*(2))".into(), ..Default::default() });
        assert!(matches!(script(Variant::SqlServer, &r), Err(Error::Unsupported(m)) if m.contains("«doble»")));
    }

    #[test]
    fn swaps_only_the_bracketed_column() {
        assert_eq!(swap_column("([pepe]>(0) AND [pepe2]<[Pepe])", "pepe", "x"), "([x]>(0) AND [pepe2]<[x])");
        assert_eq!(swap_column("([a]]b]=(1))", "a]b", "c"), "([c]=(1))");
    }

    #[test]
    fn what_each_variant_refuses() {
        let t = obj(kinds::TABLE, "dbo", "T");
        let schema = RenameTarget::Schema { database: None, schema: "ventas".into() };
        for v in Variant::ALL {
            assert!(matches!(script(v, &req(schema.clone(), "x")), Err(Error::Unsupported(m)) if m.contains("esquemas")), "{v:?}");
            assert!(matches!(script(v, &req(object(kinds::SYNONYM, "s"), "x")), Err(Error::Unsupported(m)) if m.contains("sinónimos")), "{v:?}");
        }
        let trigger = RenameTarget::Object { object: obj(kinds::TRIGGER, "dbo", "trg"), parent: Some("T".into()) };
        assert!(matches!(script(Variant::Babelfish, &req(trigger, "x")), Err(Error::Unsupported(m)) if m.ends_with("no renombra triggers")));
        let ck = RenameTarget::Constraint { table: t.clone(), constraint: "CK".into() };
        assert!(matches!(script(Variant::Babelfish, &req(ck.clone(), "x")), Err(Error::Unsupported(m)) if m.ends_with("no renombra restricciones")));
        assert!(script(Variant::Fabric, &req(ck, "x")).is_err());
        assert!(matches!(script(Variant::Babelfish, &req(RenameTarget::Index { table: t.clone(), index: "ix".into() }, "x")), Err(Error::Unsupported(m)) if m.ends_with("no renombra índices")));
        assert!(script(Variant::Fabric, &req(RenameTarget::Index { table: t, index: "ix".into() }, "x")).is_err());
        assert!(script(Variant::Fabric, &req(object(kinds::VIEW, "v"), "x")).is_err());
        let s = spec(Variant::SqlServer);
        assert_eq!((s.replace, s.fold, s.transactional, s.schemas), (ReplaceStyle::CreateOrAlter, Fold::None, true, false));
        assert!(s.tracked.is_empty());
        assert_eq!(spec(Variant::Babelfish).replace, ReplaceStyle::DropCreate);
        assert_eq!(spec(Variant::Babelfish).replace_for(kinds::VIEW), ReplaceStyle::CreateOrAlter);
        assert_eq!(spec(Variant::Babelfish).replace_for(kinds::PROCEDURE), ReplaceStyle::DropCreate);
        assert!(!spec(Variant::Fabric).transactional);
    }

    #[test]
    fn database_rename_brackets_and_closes_sessions_on_sql_server() {
        let s = database_script(Variant::SqlServer, "Ventas", "Ventas 2024").unwrap();
        assert_eq!(
            s.statements,
            [
                "ALTER DATABASE [Ventas] SET SINGLE_USER WITH ROLLBACK IMMEDIATE",
                "ALTER DATABASE [Ventas] MODIFY NAME = [Ventas 2024]",
                "ALTER DATABASE [Ventas 2024] SET MULTI_USER",
            ]
        );
        assert!(s.warnings.iter().any(|w| w.contains("SET MULTI_USER")));
        assert!(s.warnings.iter().any(|w| w.contains("jobs del Agente")));
        // `]` is doubled in both names.
        let s = database_script(Variant::SqlServer, "a]b", "c]]d").unwrap();
        assert_eq!(s.statements[1], "ALTER DATABASE [a]]b] MODIFY NAME = [c]]]]d]");
        assert_eq!(s.statements[2], "ALTER DATABASE [c]]]]d] SET MULTI_USER");
    }

    #[test]
    fn database_rename_on_azure_and_babelfish_is_modify_name_alone() {
        for v in [Variant::AzureSql, Variant::Babelfish] {
            let s = database_script(v, "app", "app_old").unwrap();
            assert_eq!(s.statements, ["ALTER DATABASE [app] MODIFY NAME = [app_old]"], "{v:?}");
            assert!(!s.warnings.iter().any(|w| w.contains("SINGLE_USER")), "{v:?}");
        }
        assert!(database_script(Variant::AzureSql, "app", "x").unwrap().warnings[0].contains("corta"));
        assert!(database_script(Variant::Babelfish, "app", "x").unwrap().warnings[0].contains("se niega"));
    }

    #[test]
    fn database_rename_refusals() {
        for v in [Variant::SqlServer, Variant::AzureSql, Variant::Babelfish] {
            for sys in ["master", "MODEL", "msdb", "TempDB"] {
                assert!(matches!(database_script(v, sys, "x"), Err(Error::Unsupported(m)) if m.contains("del sistema")), "{v:?} {sys}");
                assert!(matches!(database_script(v, "app", sys), Err(Error::Unsupported(_))), "{v:?} {sys}");
            }
            assert!(database_script(v, "app", "app").is_err());
            assert!(database_script(v, "app", "  ").is_err());
            assert!(database_script(v, "app", &"x".repeat(129)).is_err());
            assert!(database_script(v, "app", &"x".repeat(128)).is_ok());
        }
        assert!(matches!(database_script(Variant::Fabric, "wh", "x"), Err(Error::Unsupported(m)) if m.contains("portal")));
    }

    #[test]
    fn database_rename_spec() {
        for v in [Variant::SqlServer, Variant::AzureSql, Variant::Babelfish] {
            let s = spec(v);
            assert!(s.databases && !s.database_moves, "{v:?}");
            assert_eq!(s.database_from.as_deref(), Some("master"), "{v:?}");
            let note = s.database_note.unwrap();
            assert!(note.contains("No es atómico") && note.contains("no cambian"), "{v:?}");
        }
        let note = spec(Variant::SqlServer).database_note.unwrap();
        assert!(note.contains("SINGLE_USER WITH ROLLBACK IMMEDIATE") && note.contains("MULTI_USER"));
        assert!(spec(Variant::AzureSql).database_note.unwrap().contains("no admite SINGLE_USER"));
        let f = spec(Variant::Fabric);
        assert!(!f.databases && f.database_from.is_none() && f.database_note.is_none());
    }

    #[test]
    fn a_module_with_a_go_line_is_not_put_back() {
        let d = ScriptDialect::tsql();
        assert!(one_batch("CREATE OR ALTER PROCEDURE dbo.p AS SELECT 1".into(), "p", &d).is_ok());
        // SQL Server stores this (`GO` is the column's alias); a GO-splitting
        // runner would send the GRANT as a batch of its own.
        let body = "CREATE OR ALTER PROCEDURE dbo.p AS\nSELECT 1\nGO\nGRANT CONTROL TO public".to_string();
        assert!(one_batch(body, "p", &d).is_err());
        // Inside a string the splitter doesn't cut, so that one is put back.
        assert!(one_batch("CREATE OR ALTER PROCEDURE dbo.p AS\nSELECT 'x\nGO\ny'".into(), "p", &d).is_ok());
        assert!(one_batch("CREATE OR ALTER VIEW dbo.v AS SELECT 1 AS go_live".into(), "v", &d).is_ok());
    }
}
