//! "Nuevo esquema…" / "Borrar esquema…" (`Driver::schema_spec`). Grants on
//! the new schema go through `security::script` with kind "schema".
//!
//! - The owner: `ALTER SCHEMA … OWNER TO`, after the grants
//!   ([`owner_script`]), on PostgreSQL and the engines that keep its DDL
//!   (managed services, EDB, Fujitsu, KingbaseES, TimescaleDB, YugabyteDB,
//!   openGauss, Greenplum and its forks, Yellowbrick), Redshift, RisingWave
//!   and Materialize. Whoever isn't superuser can only grant on a schema
//!   while it's theirs: with `CREATE SCHEMA … AUTHORIZATION` the grants
//!   right after fail (PostgreSQL, when the creator doesn't inherit the
//!   owner's rights) or grant nothing without a word (RisingWave).
//!   CockroachDB, where every member of the owner holds its rights, and H2,
//!   which can't change an owner, keep `AUTHORIZATION`.
//! - `DROP SCHEMA … [CASCADE]` everywhere.
//! - Materialize: no `AUTHORIZATION`; given an owner anyway (an older DBine
//!   that doesn't ask [`owner_script`]), it's set right after the create.
//! - Redshift and RisingWave: the owner is a user (no groups or roles).
//!   H2 too: it takes a role, but then the database no longer opens.
//! - CrateDB: no `CREATE SCHEMA`; a schema exists while it holds a table.
//! - Denodo: no schemas (a virtual database holds its views directly).

use crate::Variant;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{Error, Result, SchemaOwnerKinds, SchemaSpec};

pub(crate) fn spec(v: Variant) -> Option<SchemaSpec> {
    let privileges = match v {
        Variant::CrateDb | Variant::Denodo => return None,
        // Grants on a schema reach its tables and views.
        Variant::H2 => vec!["SELECT", "INSERT", "UPDATE", "DELETE"],
        // openGauss adds the rights to alter, drop and comment the schema's objects.
        Variant::OpenGauss => vec!["USAGE", "CREATE", "ALTER", "DROP", "COMMENT"],
        _ => vec!["USAGE", "CREATE"],
    };
    let owner_kinds = match v {
        // H2 2.1 writes a schema before the roles when it saves the
        // database: one owned by a role leaves it unable to open again.
        Variant::Redshift | Variant::RisingWave | Variant::H2 => SchemaOwnerKinds::Users,
        _ => SchemaOwnerKinds::Both,
    };
    // Materialize and H2 have no WITH GRANT OPTION (see `security::script`).
    let grant_option = !matches!(v, Variant::Materialize | Variant::H2);
    Some(SchemaSpec { owner: true, owner_kinds, cascade: true, privileges, grant_option })
}

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

fn checked(v: Variant, name: &str) -> Result<String> {
    if spec(v).is_none() {
        return Err(Error::Unsupported(match v {
            Variant::CrateDb => "CrateDB no crea ni borra esquemas: un esquema existe mientras tenga alguna tabla".into(),
            _ => format!("{} no tiene esquemas", v.info().name),
        }));
    }
    if name.trim().is_empty() {
        return Err(Error::Query("el esquema necesita un nombre".into()));
    }
    Ok(q(name))
}

/// The owner, quoted; an error when the engine can't hand a schema to it.
fn owner_of(v: Variant, owner: &str) -> Result<String> {
    if v == Variant::Redshift && owner.starts_with("GROUP ") {
        return Err(Error::Query("en Redshift el dueño de un esquema tiene que ser un usuario, no un grupo".into()));
    }
    Ok(q(owner))
}

pub(crate) fn create_script(v: Variant, name: &str, owner: Option<&str>) -> Result<String> {
    let schema = checked(v, name)?;
    let Some(owner) = owner.map(str::trim).filter(|o| !o.is_empty()) else {
        return Ok(format!("CREATE SCHEMA {schema};"));
    };
    let owner = owner_of(v, owner)?;
    Ok(match v {
        Variant::Materialize => format!("CREATE SCHEMA {schema};\nALTER SCHEMA {schema} OWNER TO {owner};"),
        _ => format!("CREATE SCHEMA {schema} AUTHORIZATION {owner};"),
    })
}

/// The owner change that closes "Nuevo esquema…", after the grants (see
/// the module notes); `None` where the owner goes in the create.
pub(crate) fn owner_script(v: Variant, name: &str, owner: &str) -> Result<Option<String>> {
    if matches!(v, Variant::Cockroach | Variant::H2) {
        return Ok(None);
    }
    let schema = checked(v, name)?;
    let owner = owner.trim();
    if owner.is_empty() {
        return Err(Error::Query("falta el dueño del esquema".into()));
    }
    Ok(Some(format!("ALTER SCHEMA {schema} OWNER TO {};", owner_of(v, owner)?)))
}

pub(crate) fn drop_script(v: Variant, name: &str, cascade: bool) -> Result<String> {
    let schema = checked(v, name)?;
    Ok(format!("DROP SCHEMA {schema}{};", if cascade { " CASCADE" } else { "" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ObjectRef, SecurityAction};

    #[test]
    fn create_quotes_and_takes_an_owner() {
        let pg = |o| create_script(Variant::Postgres, "ven\"tas", o).unwrap();
        assert_eq!(pg(None), "CREATE SCHEMA \"ven\"\"tas\";");
        assert_eq!(pg(Some("ana")), "CREATE SCHEMA \"ven\"\"tas\" AUTHORIZATION \"ana\";");
        assert_eq!(pg(Some("  ")), "CREATE SCHEMA \"ven\"\"tas\";");
        for v in [Variant::Cockroach, Variant::Redshift, Variant::RisingWave, Variant::H2, Variant::OpenGauss, Variant::Greenplum, Variant::Yellowbrick] {
            assert_eq!(create_script(v, "s", Some("a\"b")).unwrap(), "CREATE SCHEMA \"s\" AUTHORIZATION \"a\"\"b\";", "{v:?}");
        }
        assert_eq!(
            create_script(Variant::Materialize, "s", Some("ana")).unwrap(),
            "CREATE SCHEMA \"s\";\nALTER SCHEMA \"s\" OWNER TO \"ana\";"
        );
        assert!(create_script(Variant::Redshift, "s", Some("GROUP ventas")).is_err());
        assert!(create_script(Variant::Postgres, " ", None).is_err());
    }

    #[test]
    fn the_owner_changes_after_the_grants_except_on_cockroach_and_h2() {
        for v in Variant::ALL {
            let Some(spec) = spec(v) else { continue };
            let got = owner_script(v, "ven\"tas", "a\"b").unwrap();
            if matches!(v, Variant::Cockroach | Variant::H2) {
                assert_eq!(got, None, "{v:?}");
            } else {
                assert_eq!(got.as_deref(), Some("ALTER SCHEMA \"ven\"\"tas\" OWNER TO \"a\"\"b\";"), "{v:?}");
            }
            let users_only = matches!(v, Variant::Redshift | Variant::RisingWave | Variant::H2);
            assert_eq!(spec.owner_kinds, if users_only { SchemaOwnerKinds::Users } else { SchemaOwnerKinds::Both }, "{v:?}");
        }
        // An owner the engine can't take is an error, not "unsupported".
        assert!(matches!(owner_script(Variant::Redshift, "s", "GROUP ventas"), Err(Error::Query(_))));
        assert!(matches!(owner_script(Variant::Postgres, "s", " "), Err(Error::Query(_))));
        assert!(matches!(owner_script(Variant::Postgres, "", "ana"), Err(Error::Query(_))));
    }

    #[test]
    fn drop_with_or_without_contents() {
        assert_eq!(drop_script(Variant::Postgres, "s", false).unwrap(), "DROP SCHEMA \"s\";");
        assert_eq!(drop_script(Variant::Cockroach, "s", true).unwrap(), "DROP SCHEMA \"s\" CASCADE;");
    }

    #[test]
    fn engines_without_schemas_say_why() {
        for v in [Variant::CrateDb, Variant::Denodo] {
            assert!(spec(v).is_none());
            assert!(matches!(create_script(v, "s", None), Err(Error::Unsupported(_))));
            assert!(matches!(drop_script(v, "s", true), Err(Error::Unsupported(_))));
        }
    }

    #[test]
    fn every_offered_privilege_can_be_granted_on_the_schema() {
        let schema = || Some(ObjectRef { kind: "schema".into(), schema: None, name: "ven\"tas".into() });
        for v in Variant::ALL {
            let Some(spec) = spec(v) else { continue };
            assert!(spec.owner && spec.cascade && !spec.privileges.is_empty(), "{v:?}");
            let privileges: Vec<String> = spec.privileges.iter().map(|p| p.to_string()).collect();
            let grant = SecurityAction::Grant { privileges: privileges.clone(), object: schema(), to: "ana".into(), grantable: false };
            let sql = crate::security::script(v, &grant).unwrap_or_else(|e| panic!("{v:?}: {e}"));
            assert_eq!(sql, format!("GRANT {} ON SCHEMA \"ven\"\"tas\" TO \"ana\";", privileges.join(", ")), "{v:?}");
            let revoke = SecurityAction::Revoke { privileges, object: schema(), from: "ana".into() };
            assert!(crate::security::script(v, &revoke).unwrap().contains(" ON SCHEMA \"ven\"\"tas\" FROM \"ana\""), "{v:?}");
        }
        let with_option = SecurityAction::Grant { privileges: vec!["USAGE".into()], object: schema(), to: "GROUP g".into(), grantable: true };
        assert_eq!(
            crate::security::script(Variant::Redshift, &with_option).unwrap(),
            "GRANT USAGE ON SCHEMA \"ven\"\"tas\" TO GROUP \"g\" WITH GRANT OPTION;"
        );
    }
}
