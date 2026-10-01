//! "Nuevo esquema…" and "Borrar esquema…" in the explorer: the code that
//! creates a schema (with its grants and owner) or drops it, in the engine's
//! language. The UI shows it and runs it only on the user's click, through
//! the usual query execution.

use crate::commands::schema::driver_of;
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::cache::kinds as cache_kinds;
use dbine_driver::{Driver, Language, SchemaSpec};
use serde::Deserialize;
use tauri::State;

/// Most grants one "Nuevo esquema…" takes.
const MAX_GRANTS: usize = 100;

#[derive(Deserialize)]
pub struct ConnectionArgs {
    pub connection_id: String,
}

/// What the dialog offers for the connection's engine; `null`: no schema
/// creation from DBine.
#[tauri::command(rename_all = "camelCase")]
pub async fn schema_spec(state: State<'_, AppState>, args: ConnectionArgs) -> CommandResult<Option<SchemaSpec>> {
    Ok(driver_of(&state, &args.connection_id)?.schema_spec())
}

/// One grant on the new schema.
#[derive(Debug, Deserialize)]
pub struct SchemaGrant {
    pub principal: String,
    pub privileges: Vec<String>,
    #[serde(default)]
    pub grantable: bool,
}

#[derive(Deserialize)]
pub struct CreateArgs {
    pub connection_id: String,
    /// The database the menu was opened on (where the schema goes).
    #[serde(default)]
    pub database: Option<String>,
    pub name: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub grants: Vec<SchemaGrant>,
}

/// The driver and its spec, refused on read-only connections and engines
/// without schema creation.
fn writable(state: &AppState, connection_id: &str) -> CommandResult<(&'static dyn Driver, SchemaSpec)> {
    let conn = state.store.get_connection(connection_id)?.ok_or_else(|| CommandError::NotFound("conexión inexistente".into()))?;
    if conn.config.read_only {
        return Err(CommandError::BadRequest(format!("«{}» es de solo lectura: no se pueden cambiar esquemas", conn.name)));
    }
    let driver = driver_of(state, connection_id)?;
    let spec = driver.schema_spec().ok_or_else(|| CommandError::BadRequest("este motor no crea ni borra esquemas desde DBine".into()))?;
    Ok((driver.as_ref(), spec))
}

/// Create, one grant per row, then the owner change when the engine hands
/// the schema over after the grants (`Driver::schema_owner_script`: there
/// the creator may lose the right to grant once it isn't the owner), as one
/// script: each statement closed the engine's way (`;` or its batch
/// separator).
#[tauri::command(rename_all = "camelCase")]
pub async fn create_schema_script(state: State<'_, AppState>, args: CreateArgs) -> CommandResult<String> {
    let (driver, spec) = writable(&state, &args.connection_id)?;
    build_create(driver, &spec, &args)
}

fn build_create(driver: &dyn Driver, spec: &SchemaSpec, args: &CreateArgs) -> CommandResult<String> {
    let name = args.name.trim();
    if name.is_empty() {
        return Err(CommandError::BadRequest("el esquema necesita un nombre".into()));
    }
    let owner = args.owner.as_deref().map(str::trim).filter(|o| !o.is_empty());
    if owner.is_some() && !spec.owner {
        return Err(CommandError::BadRequest("en este motor los esquemas no tienen dueño".into()));
    }
    if !args.grants.is_empty() && spec.privileges.is_empty() {
        return Err(CommandError::BadRequest("este motor no otorga permisos sobre un esquema".into()));
    }
    if args.grants.len() > MAX_GRANTS {
        return Err(CommandError::BadRequest(format!("demasiados permisos: como mucho {MAX_GRANTS}")));
    }
    let database = db_of(&args.database);
    let owner_change = match owner {
        Some(o) => driver.schema_owner_script(database, name, o)?,
        None => None,
    };
    let mut parts = vec![driver.create_schema_script(database, name, if owner_change.is_some() { None } else { owner })?];
    for (i, g) in args.grants.iter().enumerate() {
        let to = g.principal.trim();
        if to.is_empty() {
            return Err(CommandError::BadRequest(format!("el permiso {} no dice a quién se otorga", i + 1)));
        }
        if g.privileges.is_empty() {
            return Err(CommandError::BadRequest(format!("el permiso para «{to}» no tiene privilegios")));
        }
        if let Some(p) = g.privileges.iter().find(|p| !spec.privileges.iter().any(|s| s.eq_ignore_ascii_case(p.trim()))) {
            return Err(CommandError::BadRequest(format!("«{p}» no es un privilegio de esquema en este motor")));
        }
        let privileges: Vec<String> = g.privileges.iter().map(|p| p.trim().to_string()).collect();
        parts.push(driver.schema_grant_script(database, name, &privileges, to, g.grantable)?);
    }
    parts.extend(owner_change);
    Ok(join(driver, &parts))
}

/// The menu's database, `None` when blank.
fn db_of(database: &Option<String>) -> Option<&str> {
    database.as_deref().map(str::trim).filter(|d| !d.is_empty())
}

/// The statements one after the other, each closed the engine's way.
fn join(driver: &dyn Driver, parts: &[String]) -> String {
    let sep = driver.script_separator();
    let sql = driver.info().language == Language::Sql;
    let mut out = String::new();
    for p in parts {
        let t = p.trim_end();
        if t.is_empty() {
            continue;
        }
        out.push_str(t);
        if !sep.is_empty() {
            out.push('\n');
            out.push_str(sep);
        } else if sql && !t.ends_with(';') {
            out.push(';');
        }
        out.push_str("\n\n");
    }
    out.trim_end().to_string() + "\n"
}

#[derive(Deserialize)]
pub struct DropArgs {
    pub connection_id: String,
    #[serde(default)]
    pub database: Option<String>,
    pub name: String,
    #[serde(default)]
    pub cascade: bool,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn drop_schema_script(state: State<'_, AppState>, args: DropArgs) -> CommandResult<String> {
    let (driver, spec) = writable(&state, &args.connection_id)?;
    let name = args.name.trim();
    if name.is_empty() {
        return Err(CommandError::BadRequest("falta el esquema a borrar".into()));
    }
    if args.cascade && !spec.cascade {
        return Err(CommandError::BadRequest("este motor no borra un esquema con su contenido".into()));
    }
    Ok(join(driver, &[driver.drop_schema_script(db_of(&args.database), name, args.cascade)?]))
}

#[derive(Deserialize)]
pub struct CountArgs {
    pub connection_id: String,
    pub database: String,
    pub schema: String,
}

/// How many objects the schema holds (read again from the server, not the
/// explorer's copy: it's shown before dropping it).
#[tauri::command(rename_all = "camelCase")]
pub async fn schema_object_count(state: State<'_, AppState>, args: CountArgs) -> CommandResult<u64> {
    let objects = state
        .meta_read(&args.connection_id, &args.database, crate::commands::explorer::META_LIMIT, |s| Box::pin(s.list_objects()))
        .await?;
    state.cache_put(&args.connection_id, &args.database, cache_kinds::OBJECTS, "", &objects);
    Ok(objects.iter().filter(|o| o.schema.as_deref() == Some(args.schema.as_str())).count() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{async_trait, ConnectionConfig, DriverInfo, Error, Family, Result, SecurityAction, Session};

    /// `.2`: the owner changes after the grants (`schema_owner_script`).
    struct Fake(DriverInfo, &'static str, bool);

    #[async_trait]
    impl Driver for Fake {
        fn info(&self) -> &DriverInfo {
            &self.0
        }
        fn script_separator(&self) -> &'static str {
            self.1
        }
        fn create_schema_script(&self, database: Option<&str>, name: &str, owner: Option<&str>) -> Result<String> {
            let name = match database {
                Some(d) => format!("{d}.{name}"),
                None => name.to_string(),
            };
            Ok(match owner {
                Some(o) => format!("CREATE SCHEMA {name} AUTHORIZATION {o}"),
                None => format!("CREATE SCHEMA {name}"),
            })
        }
        fn schema_owner_script(&self, _: Option<&str>, name: &str, owner: &str) -> Result<Option<String>> {
            Ok(self.2.then(|| format!("ALTER SCHEMA {name} OWNER TO {owner}")))
        }
        fn security_script(&self, action: &SecurityAction) -> Result<String> {
            match action {
                SecurityAction::Grant { privileges, object: Some(o), to, grantable } if o.kind == "schema" => {
                    Ok(format!("GRANT {} ON SCHEMA {} TO {to}{};", privileges.join(", "), o.name, if *grantable { " WITH GRANT OPTION" } else { "" }))
                }
                _ => Err(Error::Unsupported("x".into())),
            }
        }
        async fn connect(&self, _: &ConnectionConfig, _: Option<&str>) -> Result<Box<dyn Session>> {
            Err(Error::Unsupported("x".into()))
        }
    }

    fn fake(sep: &'static str) -> Fake {
        Fake(
            DriverInfo {
                id: "fake", name: "Fake", family: Family::Relational, language: Language::Sql, dialect: "standard",
                default_port: 0, fields: vec![], databases_label: "Bases", has_schemas: true, object_kinds: vec![],
            },
            sep,
            false,
        )
    }

    fn spec() -> SchemaSpec {
        SchemaSpec { owner: true, cascade: true, privileges: vec!["USAGE", "CREATE"], ..Default::default() }
    }

    fn args(owner: Option<&str>, grants: Vec<SchemaGrant>) -> CreateArgs {
        CreateArgs { connection_id: "c".into(), database: None, name: " ventas ".into(), owner: owner.map(Into::into), grants }
    }

    #[test]
    fn the_owner_changes_after_the_grants_where_the_engine_says() {
        let g = vec![SchemaGrant { principal: "bob".into(), privileges: vec!["USAGE".into()], grantable: false }];
        let after = Fake(fake("").0, "", true);
        let s = build_create(&after, &spec(), &args(Some("ana"), g)).unwrap();
        assert_eq!(s, "CREATE SCHEMA ventas;\n\nGRANT USAGE ON SCHEMA ventas TO bob;\n\nALTER SCHEMA ventas OWNER TO ana;\n");
        // Without an owner there's nothing to change.
        assert_eq!(build_create(&after, &spec(), &args(None, vec![])).unwrap(), "CREATE SCHEMA ventas;\n");
        // The menu's database reaches the driver; a blank one is none.
        let mut a = args(None, vec![]);
        a.database = Some("lake".into());
        assert_eq!(build_create(&fake(""), &spec(), &a).unwrap(), "CREATE SCHEMA lake.ventas;\n");
        a.database = Some(" ".into());
        assert_eq!(build_create(&fake(""), &spec(), &a).unwrap(), "CREATE SCHEMA ventas;\n");
    }

    #[test]
    fn create_then_each_grant() {
        let g = vec![
            SchemaGrant { principal: "ana".into(), privileges: vec!["usage".into()], grantable: false },
            SchemaGrant { principal: "bob".into(), privileges: vec!["USAGE".into(), "CREATE".into()], grantable: true },
        ];
        let s = build_create(&fake(""), &spec(), &args(Some("ana"), g)).unwrap();
        assert_eq!(
            s,
            "CREATE SCHEMA ventas AUTHORIZATION ana;\n\nGRANT usage ON SCHEMA ventas TO ana;\n\nGRANT USAGE, CREATE ON SCHEMA ventas TO bob WITH GRANT OPTION;\n"
        );
        let s = build_create(&fake("GO"), &spec(), &args(None, vec![])).unwrap();
        assert_eq!(s, "CREATE SCHEMA ventas\nGO\n");
    }

    #[test]
    fn refuses_what_the_engine_does_not_offer() {
        let no_owner = SchemaSpec { owner: false, ..spec() };
        assert!(build_create(&fake(""), &no_owner, &args(Some("ana"), vec![])).is_err());
        let bad = vec![SchemaGrant { principal: "ana".into(), privileges: vec!["DROP TABLE".into()], grantable: false }];
        assert!(build_create(&fake(""), &spec(), &args(None, bad)).is_err());
        let nobody = vec![SchemaGrant { principal: " ".into(), privileges: vec!["USAGE".into()], grantable: false }];
        assert!(build_create(&fake(""), &spec(), &args(None, nobody)).is_err());
        let empty = vec![SchemaGrant { principal: "ana".into(), privileges: vec![], grantable: false }];
        assert!(build_create(&fake(""), &spec(), &args(None, empty)).is_err());
        let none = SchemaSpec { privileges: vec![], ..spec() };
        let one = vec![SchemaGrant { principal: "ana".into(), privileges: vec!["USAGE".into()], grantable: false }];
        assert!(build_create(&fake(""), &none, &args(None, one)).is_err());
        let many = (0..=MAX_GRANTS).map(|_| SchemaGrant { principal: "ana".into(), privileges: vec!["USAGE".into()], grantable: false }).collect();
        assert!(build_create(&fake(""), &spec(), &args(None, many)).is_err());
        assert!(build_create(&fake(""), &spec(), &CreateArgs { name: "  ".into(), ..args(None, vec![]) }).is_err());
    }
}
