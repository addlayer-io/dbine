//! "Nuevo esquema…" / "Borrar esquema…" (`Driver::schema_spec`) for the
//! presets where a schema is a plain object the SQL creates and drops.
//!
//! Left out, with the reason in docs/soporte-por-motor.md: the generic ODBC
//! preset (unknown engine); engines where a schema is the owner user
//! (SAP ASE, SQL Anywhere, Informix, GBase 8s, Altibase, Ingres, OpenEdge,
//! Machbase), a database with its own space (Teradata), an implicit
//! qualifier (Db2 for z/OS, InterSystems IRIS / Caché, Virtuoso, Ignite 2);
//! the ones without schemas (CUBRID, Zen, Access, dBase, HeavyDB); and
//! read-only SuiteAnalytics Connect.

use crate::presets::Preset;
use crate::security::{grantee, ident};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{Error, Result, SchemaInfo, SchemaOwnerKinds, SchemaSpec};

/// How a preset's schemas are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    /// `CREATE SCHEMA s [AUTHORIZATION o]`, `DROP SCHEMA s RESTRICT` (Db2
    /// for LUW needs RESTRICT and has no CASCADE).
    Db2,
    /// `CREATE SCHEMA s [AUTHORIZATION o]`, `DROP SCHEMA s CASCADE|RESTRICT`.
    Authorization,
    /// `CREATE SCHEMA s` (owned by whoever runs it), `DROP SCHEMA s
    /// CASCADE|RESTRICT`.
    Plain,
    /// `CREATE SCHEMA s`, `DROP SCHEMA s` (only an empty one).
    PlainNoCascade,
    /// Exasol: `CREATE SCHEMA s` then `ALTER SCHEMA s CHANGE OWNER o`.
    ChangeOwner,
    /// Hive and Impala: a schema is a database; `ALTER DATABASE … SET OWNER
    /// USER|ROLE`.
    HiveDatabase,
    /// Spark SQL: a database, no owner.
    SparkDatabase,
}

fn style(p: &Preset) -> Option<Style> {
    Some(match p.id {
        "db2" => Style::Db2,
        "vertica" | "netezza" | "dameng" | "monetdb" => Style::Authorization,
        "db2i" | "mimer" | "maxdb" | "nuodb" | "ignite3" => Style::Plain,
        "sqream" | "ocient" => Style::PlainNoCascade,
        "exasol" => Style::ChangeOwner,
        "hive" | "cloudera" | "impala" => Style::HiveDatabase,
        "spark" | "kyuubi" => Style::SparkDatabase,
        _ => return None,
    })
}

/// Privileges are offered only where `security::script` writes the grant
/// on a schema (every such preset has a dialect there).
pub fn spec(p: &Preset) -> Option<SchemaSpec> {
    let st = style(p)?;
    let owner = matches!(st, Style::Db2 | Style::Authorization | Style::ChangeOwner | Style::HiveDatabase);
    let cascade = !matches!(st, Style::Db2 | Style::PlainNoCascade);
    let privileges = match p.id {
        "db2" => vec!["CREATEIN", "ALTERIN", "DROPIN", "SELECTIN", "INSERTIN", "UPDATEIN", "DELETEIN", "EXECUTEIN"],
        "vertica" => vec!["USAGE", "CREATE", "SELECT", "INSERT", "UPDATE", "DELETE", "REFERENCES", "TRUNCATE", "ALTER", "DROP", "ALL"],
        "exasol" => vec!["SELECT", "INSERT", "UPDATE", "DELETE", "ALTER", "REFERENCES", "EXECUTE"],
        "impala" => vec!["SELECT", "INSERT", "ALTER", "CREATE", "DROP", "REFRESH", "ALL"],
        "sqream" => vec!["USAGE", "CREATE", "DDL", "SUPERUSER"],
        // MaxDB's schema privileges are only these two.
        "maxdb" => vec!["CREATEIN", "DROPIN"],
        _ => vec![],
    };
    // Only a user owns a schema in Db2, Vertica, Netezza and Dameng
    // (`AUTHORIZATION` names a user); MonetDB, Exasol, Hive and Impala
    // take a role too.
    let owner_kinds = if user_owned(p, st) { SchemaOwnerKinds::Users } else { SchemaOwnerKinds::Both };
    // Exasol grants on objects, MaxDB on a schema and SQream at all
    // without the right to pass them on (see `security::script`).
    let grant_option = !matches!(p.id, "exasol" | "maxdb" | "sqream");
    Some(SchemaSpec { owner, owner_kinds, cascade, privileges, grant_option })
}

fn user_owned(p: &Preset, st: Style) -> bool {
    st == Style::Db2 || (st == Style::Authorization && p.id != "monetdb")
}

/// Hive and Impala take only letters, digits and `_` in a database name.
fn check_name(st: Style, schema: &str) -> Result<()> {
    if st == Style::HiveDatabase && !schema.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(Error::Query("en este motor el nombre del esquema solo puede tener letras, números y guiones bajos".into()));
    }
    Ok(())
}

/// `Driver::schema_owner_script`: Exasol, Hive and Impala hand the schema
/// over with an `ALTER` after the grants, because whoever creates it stops
/// being able to grant on it once it's someone else's. The `AUTHORIZATION`
/// engines (`None` here) put the owner in the create: only an
/// administrator creates a schema for someone else there, and keeps the
/// right to grant.
pub fn owner(p: &Preset, schema: &str, owner: &str) -> Result<Option<String>> {
    let Some(st) = style(p) else { return Ok(None) };
    if !matches!(st, Style::ChangeOwner | Style::HiveDatabase) {
        return Ok(None);
    }
    check_name(st, schema)?;
    let (role, o) = grantee(owner);
    if o.trim().is_empty() {
        return Err(Error::Query("falta el dueño del esquema".into()));
    }
    let (s, o) = (name(p, schema), name(p, o));
    Ok(Some(match st {
        Style::ChangeOwner => format!("ALTER SCHEMA {s} CHANGE OWNER {o};"),
        _ => format!("ALTER DATABASE {s} SET OWNER {} {o};", if role { "ROLE" } else { "USER" }),
    }))
}

/// How `Session::list_schemas` reads a preset's schemas: `None`, it
/// doesn't (the explorer derives them from the objects: presets without
/// "Nuevo esquema…", where a schema is a user or an implicit qualifier);
/// `Some(sql)`: the catalog query, whose first column is the name and the
/// second one, where `flag_col` says so, whether it's a system schema;
/// `Some(None)`: only SQLTables (SQL_ALL_SCHEMAS), the fallback of every
/// query too.
pub fn list_sql(p: &Preset) -> Option<Option<&'static str>> {
    style(p)?;
    Some(match p.id {
        "db2" => Some("SELECT SCHEMANAME, CASE WHEN OWNERTYPE = 'S' THEN 'Y' ELSE 'N' END FROM SYSCAT.SCHEMATA"),
        "db2i" => Some("SELECT SCHEMA_NAME FROM QSYS2.SYSSCHEMAS"),
        "vertica" => Some("SELECT schema_name, is_system_schema FROM v_catalog.schemata"),
        "exasol" => Some("SELECT SCHEMA_NAME FROM EXA_SCHEMAS"),
        "dameng" => Some("SELECT NAME FROM SYSOBJECTS WHERE TYPE$ = 'SCH'"),
        "monetdb" => Some("SELECT name, system FROM sys.schemas"),
        "mimer" => Some("SELECT SCHEMA_NAME FROM INFORMATION_SCHEMA.SCHEMATA"),
        "maxdb" => Some("SELECT SCHEMANAME FROM DOMAIN.SCHEMAS"),
        "nuodb" => Some("SELECT SCHEMA FROM SYSTEM.SCHEMAS"),
        "sqream" => Some("SELECT schema_name FROM sqream_catalog.schemas"),
        "hive" | "cloudera" | "impala" | "spark" | "kyuubi" => Some("SHOW DATABASES"),
        _ => None,
    })
}

/// The column of `list_sql`'s query that flags system schemas (Impala's
/// `SHOW DATABASES` has a comment there, not a flag).
pub fn flag_col(p: &Preset) -> Option<usize> {
    matches!(p.id, "db2" | "vertica" | "monetdb").then_some(1)
}

/// The schemas from `list_sql`'s rows (or SQLTables', name in column
/// `name_col`), sorted, each marked system when the catalog says so or
/// the preset hides it.
pub fn infos(p: &Preset, rows: &[Vec<Option<String>>], name_col: usize, flag_col: Option<usize>) -> Vec<SchemaInfo> {
    let mut out: Vec<SchemaInfo> = rows
        .iter()
        .filter_map(|r| {
            let name = r.get(name_col).cloned().flatten()?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            let flagged = flag_col.and_then(|i| r.get(i).cloned().flatten()).is_some_and(|v| {
                matches!(v.trim().to_ascii_lowercase().as_str(), "y" | "yes" | "t" | "true" | "1")
            });
            Some(SchemaInfo { system: flagged || p.is_system_schema(&name), name })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

/// The dialect of `security` where the preset has one (same quoting as
/// its grants on the schema), else the preset's quote.
fn name(p: &Preset, n: &str) -> String {
    match crate::security::dialect(p) {
        Some(d) => ident(d, n),
        None => quote_ident(p.quote.unwrap_or(Quote::Double), n),
    }
}

pub fn create(p: &Preset, schema: &str, owner: Option<&str>) -> Result<String> {
    let st = style(p).ok_or_else(|| Error::Unsupported("este motor no crea esquemas desde DBine".into()))?;
    check_name(st, schema)?;
    let s = name(p, schema);
    // Principals may come as `role:<name>` (see `security`); the spec's
    // `owner_kinds` already keeps roles out where only a user owns one.
    if user_owned(p, st) && owner.is_some_and(|o| grantee(o).0) {
        return Err(Error::Query("en este motor el dueño de un esquema tiene que ser un usuario, no un rol".into()));
    }
    let owner = owner.map(|o| (grantee(o).0, name(p, grantee(o).1)));
    Ok(match (st, owner) {
        (Style::Db2 | Style::Authorization, Some((_, o))) => format!("CREATE SCHEMA {s} AUTHORIZATION {o};"),
        (Style::ChangeOwner, Some((_, o))) => format!("CREATE SCHEMA {s};\nALTER SCHEMA {s} CHANGE OWNER {o};"),
        (Style::HiveDatabase, Some((role, o))) => {
            format!("CREATE DATABASE {s};\nALTER DATABASE {s} SET OWNER {} {o};", if role { "ROLE" } else { "USER" })
        }
        (Style::HiveDatabase | Style::SparkDatabase, _) => format!("CREATE DATABASE {s};"),
        _ => format!("CREATE SCHEMA {s};"),
    })
}

pub fn drop(p: &Preset, schema: &str, cascade: bool) -> Option<String> {
    let st = style(p)?;
    let s = name(p, schema);
    let how = if cascade { "CASCADE" } else { "RESTRICT" };
    Some(match st {
        Style::Db2 => format!("DROP SCHEMA {s} RESTRICT;"),
        Style::PlainNoCascade => format!("DROP SCHEMA {s};"),
        Style::HiveDatabase | Style::SparkDatabase => format!("DROP DATABASE {s} {how};"),
        _ => format!("DROP SCHEMA {s} {how};"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ObjectRef, SecurityAction};

    fn preset(id: &str) -> &'static Preset {
        crate::PRESETS.iter().find(|p| p.id == id).unwrap()
    }

    fn c(id: &str, s: &str, o: Option<&str>) -> String {
        create(preset(id), s, o).unwrap()
    }

    fn d(id: &str, s: &str, cascade: bool) -> String {
        drop(preset(id), s, cascade).unwrap()
    }

    #[test]
    fn which_presets() {
        let with: Vec<&str> = crate::PRESETS.iter().filter(|p| spec(p).is_some()).map(|p| p.id).collect();
        assert_eq!(
            with,
            ["db2", "db2i", "hive", "impala", "vertica", "exasol", "netezza", "dameng", "ocient", "spark", "kyuubi", "cloudera", "monetdb", "mimer", "sqream", "maxdb", "nuodb", "ignite3"]
        );
        // Only engines with schemas in the explorer.
        assert!(crate::PRESETS.iter().filter(|p| spec(p).is_some()).all(|p| p.has_schemas));
        for id in ["odbc", "db2zos", "sybase", "sqlanywhere", "informix", "gbase8s", "teradata", "altibase", "iris", "cache", "virtuoso", "ingres", "openedge", "machbase", "ignite", "netsuite", "cubrid", "zen", "access", "dbase", "heavydb"] {
            assert!(spec(preset(id)).is_none(), "{id}");
            assert!(create(preset(id), "x", None).is_err(), "{id}");
        }
    }

    #[test]
    fn db2() {
        let s = spec(preset("db2")).unwrap();
        assert!(s.owner && !s.cascade);
        assert_eq!(c("db2", "VENTAS", Some("ANA")), "CREATE SCHEMA VENTAS AUTHORIZATION ANA;");
        // Db2 folds to upper case: a lower-case name is quoted to keep it.
        assert_eq!(c("db2", "ventas", None), "CREATE SCHEMA \"ventas\";");
        assert_eq!(d("db2", "VENTAS", false), "DROP SCHEMA VENTAS RESTRICT;");
        assert_eq!(c("db2i", "VENTAS", None), "CREATE SCHEMA VENTAS;");
        assert_eq!(d("db2i", "VENTAS", true), "DROP SCHEMA VENTAS CASCADE;");
        assert!(!spec(preset("db2i")).unwrap().owner);
    }

    #[test]
    fn owners() {
        assert_eq!(c("vertica", "ventas", Some("ana")), "CREATE SCHEMA ventas AUTHORIZATION ana;");
        assert_eq!(c("netezza", "VENTAS", Some("ADMINS")), "CREATE SCHEMA VENTAS AUTHORIZATION ADMINS;");
        // Only a user owns a schema there; MonetDB takes a role too.
        for id in ["db2", "vertica", "netezza", "dameng"] {
            assert!(create(preset(id), "VENTAS", Some("role:ADMINS")).is_err(), "{id}");
        }
        assert_eq!(c("monetdb", "ventas", Some("role:lect")), "CREATE SCHEMA ventas AUTHORIZATION lect;");
        // Hive and Impala database names: letters, digits and `_` only.
        for id in ["hive", "impala", "cloudera"] {
            assert!(create(preset(id), "Mi Esquema", None).is_err(), "{id}");
        }
        assert_eq!(c("spark", "Mi Esquema", None), "CREATE DATABASE `Mi Esquema`;");
        assert_eq!(c("monetdb", "ventas", Some("Ana")), "CREATE SCHEMA ventas AUTHORIZATION \"Ana\";");
        assert_eq!(c("dameng", "VENTAS", Some("ANA")), "CREATE SCHEMA VENTAS AUTHORIZATION ANA;");
        assert_eq!(c("exasol", "VENTAS", Some("ANA")), "CREATE SCHEMA VENTAS;\nALTER SCHEMA VENTAS CHANGE OWNER ANA;");
        assert_eq!(c("exasol", "VENTAS", None), "CREATE SCHEMA VENTAS;");
        assert_eq!(c("hive", "ventas", Some("ana")), "CREATE DATABASE ventas;\nALTER DATABASE ventas SET OWNER USER ana;");
        assert_eq!(c("impala", "ventas", Some("role:lect")), "CREATE DATABASE ventas;\nALTER DATABASE ventas SET OWNER ROLE lect;");
        assert_eq!(c("cloudera", "Ventas_2", None), "CREATE DATABASE Ventas_2;");
        assert_eq!(c("spark", "ventas", None), "CREATE DATABASE `ventas`;");
        for id in ["mimer", "maxdb", "nuodb", "ignite3", "sqream", "ocient", "spark", "kyuubi", "db2i"] {
            assert!(!spec(preset(id)).unwrap().owner, "{id}");
        }
    }

    #[test]
    fn drops() {
        assert_eq!(d("vertica", "ventas", true), "DROP SCHEMA ventas CASCADE;");
        assert_eq!(d("vertica", "ventas", false), "DROP SCHEMA ventas RESTRICT;");
        assert_eq!(d("hive", "ventas", true), "DROP DATABASE ventas CASCADE;");
        assert_eq!(d("spark", "ventas", false), "DROP DATABASE `ventas` RESTRICT;");
        assert_eq!(d("mimer", "VENTAS", true), "DROP SCHEMA VENTAS CASCADE;");
        assert_eq!(d("ignite3", "Ventas", false), "DROP SCHEMA \"Ventas\" RESTRICT;");
        assert_eq!(d("sqream", "ventas", false), "DROP SCHEMA ventas;");
        assert_eq!(d("ocient", "ventas", false), "DROP SCHEMA ventas;");
        for id in ["sqream", "ocient", "db2"] {
            assert!(!spec(preset(id)).unwrap().cascade, "{id}");
        }
    }

    /// Every privilege a spec offers makes a valid grant on the schema.
    #[test]
    fn grants_on_the_new_schema() {
        let object = || Some(ObjectRef { kind: "schema".into(), schema: None, name: "VENTAS".into() });
        let expect = [
            ("db2", "ANA", false, "GRANT CREATEIN, ALTERIN, DROPIN, SELECTIN, INSERTIN, UPDATEIN, DELETEIN, EXECUTEIN ON SCHEMA VENTAS TO ANA;"),
            ("vertica", "ana", true, "GRANT USAGE, CREATE, SELECT, INSERT, UPDATE, DELETE, REFERENCES, TRUNCATE, ALTER, DROP ON SCHEMA VENTAS TO ana WITH GRANT OPTION;"),
            ("exasol", "ANA", false, "GRANT SELECT, INSERT, UPDATE, DELETE, ALTER, REFERENCES, EXECUTE ON SCHEMA VENTAS TO ANA;"),
            (
                "impala",
                "role:lect",
                true,
                "GRANT SELECT ON DATABASE VENTAS TO ROLE lect WITH GRANT OPTION;\nGRANT INSERT ON DATABASE VENTAS TO ROLE lect WITH GRANT OPTION;\nGRANT ALTER ON DATABASE VENTAS TO ROLE lect WITH GRANT OPTION;\nGRANT CREATE ON DATABASE VENTAS TO ROLE lect WITH GRANT OPTION;\nGRANT DROP ON DATABASE VENTAS TO ROLE lect WITH GRANT OPTION;\nGRANT REFRESH ON DATABASE VENTAS TO ROLE lect WITH GRANT OPTION;",
            ),
            ("sqream", "lect", false, "GRANT USAGE, CREATE, DDL, SUPERUSER ON SCHEMA \"VENTAS\" TO lect;"),
            ("maxdb", "ANA", false, "GRANT CREATEIN, DROPIN ON VENTAS TO ANA;"),
        ];
        for (id, to, grantable, sql) in expect {
            let p = preset(id);
            // ALL goes alone (checked below).
            let privileges = spec(p).unwrap().privileges.iter().filter(|x| **x != "ALL").map(|x| x.to_string()).collect();
            let d = crate::security::dialect(p).unwrap();
            let got = crate::security::script(d, &SecurityAction::Grant { privileges, object: object(), to: to.into(), grantable }).unwrap();
            assert_eq!(got, sql, "{id}");
        }
        // ALL alone works; mixed with others it's refused.
        let d = crate::security::dialect(preset("vertica")).unwrap();
        let g = |p: &[&str]| {
            let privileges = p.iter().map(|x| x.to_string()).collect();
            crate::security::script(d, &SecurityAction::Grant { privileges, object: object(), to: "ana".into(), grantable: false })
        };
        assert_eq!(g(&["ALL"]).unwrap(), "GRANT ALL ON SCHEMA VENTAS TO ana;");
        assert!(g(&["USAGE", "ALL"]).is_err());
        let i = crate::security::dialect(preset("impala")).unwrap();
        let all_and = SecurityAction::Grant { privileges: vec!["ALL".into(), "SELECT".into()], object: object(), to: "role:r".into(), grantable: false };
        assert!(crate::security::script(i, &all_and).is_err());
        // MaxDB's schema privileges can't be granted with grant option.
        let m = crate::security::dialect(preset("maxdb")).unwrap();
        let opt = SecurityAction::Grant { privileges: vec!["CREATEIN".into()], object: object(), to: "ANA".into(), grantable: true };
        assert!(crate::security::script(m, &opt).is_err());
        // The rest offer no grants at creation.
        for p in crate::PRESETS.iter().filter(|p| spec(p).is_some_and(|s| s.privileges.is_empty())) {
            assert!(!["db2", "vertica", "exasol", "impala", "sqream", "maxdb"].contains(&p.id));
        }
    }
    #[test]
    fn owner_kinds_and_owner_change() {
        for id in ["db2", "vertica", "netezza", "dameng"] {
            assert_eq!(spec(preset(id)).unwrap().owner_kinds, SchemaOwnerKinds::Users, "{id}");
        }
        for id in ["monetdb", "exasol", "hive", "impala", "cloudera"] {
            assert_eq!(spec(preset(id)).unwrap().owner_kinds, SchemaOwnerKinds::Both, "{id}");
        }
        // Exasol, Hive and Impala hand the schema over after the grants.
        let o = |id: &str, s: &str, who: &str| owner(preset(id), s, who).unwrap();
        assert_eq!(o("exasol", "VENTAS", "ANA").as_deref(), Some("ALTER SCHEMA VENTAS CHANGE OWNER ANA;"));
        assert_eq!(o("exasol", "VENTAS", "role:Lect").as_deref(), Some("ALTER SCHEMA VENTAS CHANGE OWNER \"Lect\";"));
        assert_eq!(o("hive", "ventas", "ana").as_deref(), Some("ALTER DATABASE ventas SET OWNER USER ana;"));
        assert_eq!(o("impala", "ventas", "role:lect").as_deref(), Some("ALTER DATABASE ventas SET OWNER ROLE lect;"));
        assert!(owner(preset("hive"), "Mi Esquema", "ana").is_err());
        assert!(owner(preset("exasol"), "VENTAS", " ").is_err());
        // The AUTHORIZATION engines keep it in the create.
        for id in ["db2", "vertica", "netezza", "dameng", "monetdb", "mimer", "spark", "odbc", "informix"] {
            assert_eq!(o(id, "VENTAS", "ANA"), None, "{id}");
        }
    }

    /// Reserved words are quoted even when the engine would fold them.
    #[test]
    fn reserved_names() {
        assert_eq!(c("db2", "SELECT", Some("USER")), "CREATE SCHEMA \"SELECT\" AUTHORIZATION \"USER\";");
        assert_eq!(c("exasol", "USER", None), "CREATE SCHEMA \"USER\";");
        assert_eq!(d("vertica", "select", true), "DROP SCHEMA \"select\" CASCADE;");
        assert_eq!(c("monetdb", "user", Some("role:table")), "CREATE SCHEMA \"user\" AUTHORIZATION \"table\";");
        assert_eq!(c("hive", "select", None), "CREATE DATABASE `select`;");
        assert_eq!(d("mimer", "USER", false), "DROP SCHEMA \"USER\" RESTRICT;");
        // Not reserved: bare.
        assert_eq!(c("db2", "VENTAS", Some("PUBLICO")), "CREATE SCHEMA VENTAS AUTHORIZATION PUBLICO;");
    }

    #[test]
    fn listing() {
        for p in crate::PRESETS {
            assert_eq!(list_sql(p).is_some(), spec(p).is_some(), "{}", p.id);
        }
        assert_eq!(list_sql(preset("ocient")), Some(None));
        let row = |n: &str, f: Option<&str>| vec![Some(n.to_string()), f.map(str::to_string)];
        // Db2: OWNERTYPE 'S', or the preset's SYS*/NULLID; CHAR padding trimmed.
        let rows = [row("VENTAS  ", Some("N")), row("SYSIBM", Some("Y")), row("NULLID", Some("N")), row("APP", Some("N")), row("VENTAS", Some("N"))];
        let got = infos(preset("db2"), &rows, 0, flag_col(preset("db2")));
        let want = [("APP", false), ("NULLID", true), ("SYSIBM", true), ("VENTAS", false)];
        assert_eq!(got.iter().map(|s| (s.name.as_str(), s.system)).collect::<Vec<_>>(), want);
        // Vertica's flag is a boolean; v_* is hidden by the preset.
        let rows = [row("public", Some("f")), row("v_monitor", Some("f")), row("v_internal", Some("t")), row("ventas", Some("false"))];
        let got = infos(preset("vertica"), &rows, 0, flag_col(preset("vertica")));
        assert_eq!(got.iter().filter(|s| s.system).count(), 2);
        assert!(got.iter().any(|s| s.name == "ventas" && !s.system));
        // Impala: the second column is a comment, never a flag.
        assert_eq!(flag_col(preset("impala")), None);
        let rows = [row("_impala_builtins", Some("System database")), row("ventas", Some("1"))];
        let got = infos(preset("impala"), &rows, 0, flag_col(preset("impala")));
        assert_eq!(got, [SchemaInfo { name: "_impala_builtins".into(), system: true }, SchemaInfo { name: "ventas".into(), system: false }]);
        // SQLTables (SQL_ALL_SCHEMAS): the name in TABLE_SCHEM; blanks skipped.
        let rows = [vec![None, Some("SYS".into()), None], vec![None, Some("".into()), None], vec![None, Some("VENTAS".into()), None]];
        let got = infos(preset("exasol"), &rows, 1, None);
        assert_eq!(got, [SchemaInfo { name: "SYS".into(), system: true }, SchemaInfo { name: "VENTAS".into(), system: false }]);
    }
}
