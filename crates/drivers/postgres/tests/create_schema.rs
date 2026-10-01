//! "Nuevo esquema…" / "Borrar esquema…" against real servers: create a
//! schema with an owner and grants (run by a login that isn't superuser
//! wherever the engine has one), read both back from the catalog, see it
//! listed while empty (`Session::list_schemas`), then drop it (refused with
//! contents unless CASCADE). Each test reads
//! `DBINE_TEST_<ENGINE>_URL` (`postgres://user:pass@host:port/db`) and is
//! skipped without it:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//! DBINE_TEST_H2_URL=postgres://sa:sa@localhost:25025/test \
//! DBINE_TEST_OPENGAUSS_URL='postgres://gaussdb:Dbine@1234@localhost:25020/postgres' \
//! DBINE_TEST_MATERIALIZE_URL=postgres://materialize@localhost:25024/materialize \
//! DBINE_TEST_RISINGWAVE_URL=postgres://root@localhost:4566/dev \
//! DBINE_TEST_TIMESCALEDB_URL=postgres://postgres:pw@localhost:25015/postgres \
//! DBINE_TEST_YUGABYTE_URL=postgres://yugabyte@localhost:25016/yugabyte \
//! DBINE_TEST_GREENGAGE_URL=postgres://gpadmin:pw@localhost:25018/postgres \
//!   cargo test -p dbine-driver-postgres --test create_schema -- --ignored --test-threads=1
//! ```

use dbine_driver::{ConnectionConfig, Driver, QueryOutcome, SecurityAction, Session};
use std::sync::Arc;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

/// Runs `sql`; whether it worked.
async fn attempt(s: &mut Box<dyn Session>, sql: &str) -> bool {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.is_ok() && out.error.is_none()
}

/// Every row of the query's first result, cells as text.
async fn rows(s: &mut Box<dyn Session>, sql: &str) -> Vec<Vec<String>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out.results
        .first()
        .map(|r| {
            r.rows
                .iter()
                .map(|row| row.iter().map(|c| c.as_str().map(str::to_string).unwrap_or_else(|| c.to_string())).collect())
                .collect()
        })
        .unwrap_or_default()
}

fn truthy(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "t" | "true" | "1")
}

/// How the test reads the catalog back.
#[derive(Clone, Copy, PartialEq)]
enum Catalog {
    /// `pg_namespace.nspowner` and `has_schema_privilege`.
    Pg,
    /// `pg_namespace` for the owner, `SHOW GRANTS ON SCHEMA` for grants.
    Cockroach,
    /// `mz_schemas` / `mz_roles` and `has_schema_privilege`.
    Materialize,
    /// `rw_schemas` / `rw_users` and `has_schema_privilege`.
    RisingWave,
    /// `INFORMATION_SCHEMA.SCHEMATA` / `RIGHTS`.
    H2,
}

/// A login without superuser rights that runs "Nuevo esquema…": allowed to
/// create schemas in the database and a member of the owner, but without
/// inheriting its rights where the engine has `NOINHERIT`, so it can only
/// grant while the schema is still its own.
#[derive(Clone, Copy, PartialEq)]
enum Creator {
    /// The admin of the URL runs everything (H2: only an admin creates
    /// schemas).
    Admin,
    /// `CREATE ROLE … LOGIN NOINHERIT PASSWORD`, member of the owner.
    NoInherit,
    /// The same with the password already hashed as MD5: openGauss stores
    /// SHA-256 otherwise, which tokio-postgres can't answer.
    NoInheritMd5,
    /// A user without password (insecure CockroachDB), member of the owner.
    Member,
    /// A role without password (the Materialize emulator lets any role in),
    /// member of the owner.
    MzRole,
    /// A plain user with a password; there are no roles to be a member of.
    User,
}

struct Case {
    id: &'static str,
    env: &'static str,
    catalog: Catalog,
    /// Principals are created as users with this password (None: roles
    /// without one).
    password: Option<&'static str>,
    grantable: bool,
    /// Names the server folds to upper case unless quoted (H2): the test
    /// uses upper-case names so both spellings agree.
    upper: bool,
    creator: Creator,
}

const CREATOR_PASSWORD: &str = "Dbine@1234";

async fn create_and_drop(c: Case) {
    let Some(cfg) = cfg(c.id, c.env) else {
        eprintln!("{} not set; skipping", c.env);
        return;
    };
    let d: Arc<dyn Driver> = dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == c.id).unwrap();
    let spec = d.schema_spec().expect("schema spec");
    let mut s = d.connect(&cfg, None).await.unwrap();
    let n = |x: &str| if c.upper { x.to_uppercase() } else { x.to_string() };
    let (schema, owner, reader, table, creator) =
        (n("dbine_sch_ventas"), n("dbine_sch_owner"), n("dbine_sch_reader"), n("facturas"), n("dbine_sch_creator"));
    let q = |x: &str| format!("\"{x}\"");
    let db = q(&cfg.database);

    // Leftovers of an interrupted run.
    for stmt in [
        format!("DROP SCHEMA IF EXISTS {} CASCADE", q(&schema)),
        format!("REVOKE CREATE ON DATABASE {db} FROM {}", q(&creator)),
        format!("DROP USER IF EXISTS {}", q(&creator)),
        format!("DROP ROLE IF EXISTS {}", q(&creator)),
        format!("DROP USER IF EXISTS {}", q(&owner)),
        format!("DROP USER IF EXISTS {}", q(&reader)),
        format!("DROP ROLE IF EXISTS {}", q(&owner)),
        format!("DROP ROLE IF EXISTS {}", q(&reader)),
    ] {
        attempt(&mut s, &stmt).await;
    }
    for p in [&owner, &reader] {
        let a = match c.password {
            Some(pw) => SecurityAction::CreateUser { name: p.clone(), password: Some(pw.into()) },
            None => SecurityAction::CreateRole { name: p.clone() },
        };
        run(&mut s, &d.security_script(&a).unwrap()).await;
    }

    // Who runs the script: the admin, or a login that isn't superuser.
    let member = format!("GRANT {} TO {}", q(&owner), q(&creator));
    let md5 = match c.creator {
        Creator::NoInheritMd5 => rows(&mut s, &format!("SELECT 'md5' || md5('{CREATOR_PASSWORD}{creator}')")).await[0][0].clone(),
        _ => String::new(),
    };
    let setup = match c.creator {
        Creator::Admin => vec![],
        Creator::NoInherit => vec![format!("CREATE ROLE {} LOGIN NOINHERIT PASSWORD '{CREATOR_PASSWORD}'", q(&creator)), member],
        Creator::NoInheritMd5 => vec![format!("CREATE ROLE {} LOGIN NOINHERIT PASSWORD '{md5}'", q(&creator)), member],
        Creator::Member => vec![format!("CREATE USER {}", q(&creator)), member],
        Creator::MzRole => vec![format!("CREATE ROLE {}", q(&creator)), member],
        Creator::User => vec![format!("CREATE USER {} WITH PASSWORD '{CREATOR_PASSWORD}'", q(&creator))],
    };
    let mut as_creator = if setup.is_empty() {
        None
    } else {
        for stmt in setup.iter().chain([&format!("GRANT CREATE ON DATABASE {db} TO {}", q(&creator))]) {
            run(&mut s, stmt).await;
        }
        let mut cc = cfg.clone();
        cc.username = Some(creator.clone());
        cc.password = matches!(c.creator, Creator::NoInherit | Creator::NoInheritMd5 | Creator::User).then(|| CREATOR_PASSWORD.into());
        Some(d.connect(&cc, None).await.unwrap_or_else(|e| panic!("{}: login as {creator}: {e}", c.id)))
    };

    // What the dialog builds (src-tauri commands/schemas.rs): the create,
    // the grants, then the owner change when the driver hands it over last.
    let privileges: Vec<String> = spec.privileges.iter().map(|p| p.to_string()).collect();
    let owner_change = d.schema_owner_script(None, &schema, &owner).unwrap();
    let mut parts = vec![d.create_schema_script(None, &schema, if owner_change.is_some() { None } else { Some(&owner) }).unwrap()];
    parts.push(d.schema_grant_script(None, &schema, &privileges, &reader, c.grantable).unwrap());
    parts.extend(owner_change);
    let script = parts.join("\n");
    eprintln!("{} (as {}):\n{script}", c.id, if as_creator.is_some() { creator.as_str() } else { "admin" });
    run(as_creator.as_mut().unwrap_or(&mut s), &script).await;

    let lit = |x: &str| format!("'{}'", x.replace('\'', "''"));
    let found_owner = match c.catalog {
        Catalog::Pg | Catalog::Cockroach => {
            rows(&mut s, &format!("SELECT pg_get_userbyid(nspowner)::text FROM pg_namespace WHERE nspname = {}", lit(&schema))).await
        }
        Catalog::Materialize => {
            rows(
                &mut s,
                &format!("SELECT r.name FROM mz_schemas s JOIN mz_roles r ON r.id = s.owner_id WHERE s.name = {}", lit(&schema)),
            )
            .await
        }
        Catalog::RisingWave => {
            rows(
                &mut s,
                &format!("SELECT u.name FROM rw_catalog.rw_schemas s JOIN rw_catalog.rw_users u ON u.id = s.owner WHERE s.name = {}", lit(&schema)),
            )
            .await
        }
        Catalog::H2 => {
            rows(&mut s, &format!("SELECT SCHEMA_OWNER FROM INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = {}", lit(&schema))).await
        }
    };
    assert_eq!(found_owner, vec![vec![owner.clone()]], "{}: owner", c.id);

    for p in &privileges {
        let held = match c.catalog {
            Catalog::Cockroach => {
                let g = rows(&mut s, &format!("SELECT privilege_type, is_grantable::text FROM [SHOW GRANTS ON SCHEMA {}] WHERE grantee = {}", q(&schema), lit(&reader))).await;
                g.iter().any(|r| r[0] == *p && truthy(&r[1]) == c.grantable)
            }
            Catalog::H2 => {
                let g = rows(
                    &mut s,
                    &format!(
                        "SELECT RIGHTS FROM INFORMATION_SCHEMA.RIGHTS WHERE GRANTEE = {} AND TABLE_SCHEMA = {} AND COALESCE(TABLE_NAME, '') = ''",
                        lit(&reader),
                        lit(&schema)
                    ),
                )
                .await;
                // All four together read back as ALL.
                g.iter().any(|r| r[0].split(',').any(|x| x.trim() == p || x.trim() == "ALL"))
            }
            _ => {
                let what = if c.grantable { format!("{p} WITH GRANT OPTION") } else { p.clone() };
                let g = rows(&mut s, &format!("SELECT has_schema_privilege({}, {}, {})::text", lit(&reader), lit(&schema), lit(&what))).await;
                truthy(&g[0][0])
            }
        };
        assert!(held, "{}: {reader} should hold {p} (grantable: {}) on {schema}", c.id, c.grantable);
    }

    // The explorer lists the new schema while it's empty, and tells the
    // engine's own from the user's.
    let listed = s.list_schemas().await.unwrap().expect("list_schemas");
    let names: Vec<&str> = listed.iter().map(|x| x.name.as_str()).collect();
    eprintln!("{}: {names:?}", c.id);
    assert!(listed.iter().any(|x| x.name == schema && !x.system), "{}: {schema} missing from {names:?}", c.id);
    assert!(listed.iter().any(|x| x.name.eq_ignore_ascii_case("information_schema") && x.system), "{}: information_schema not a system schema", c.id);
    for o in s.list_objects().await.unwrap() {
        if let Some(sch) = o.schema.as_deref() {
            assert!(listed.iter().any(|x| x.name == sch && !x.system), "{}: {sch} (of {}) not listed as a user schema", c.id, o.name);
        }
    }

    // Not empty: refused without CASCADE, dropped with it.
    run(&mut s, &format!("CREATE TABLE {}.{} (id int)", q(&schema), q(&table))).await;
    assert!(!attempt(&mut s, &d.drop_schema_script(None, &schema, false).unwrap()).await, "{}: dropped a schema with a table", c.id);
    assert!(spec.cascade);
    run(&mut s, &d.drop_schema_script(None, &schema, true).unwrap()).await;
    // Empty, without owner: dropped without CASCADE.
    run(&mut s, &d.create_schema_script(None, &schema, None).unwrap()).await;
    run(&mut s, &d.drop_schema_script(None, &schema, false).unwrap()).await;
    let left = match c.catalog {
        Catalog::H2 => rows(&mut s, &format!("SELECT COUNT(*) FROM INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = {}", lit(&schema))).await,
        Catalog::Materialize => rows(&mut s, &format!("SELECT count(*) FROM mz_schemas WHERE name = {}", lit(&schema))).await,
        Catalog::RisingWave => rows(&mut s, &format!("SELECT count(*) FROM rw_catalog.rw_schemas WHERE name = {}", lit(&schema))).await,
        _ => rows(&mut s, &format!("SELECT count(*) FROM pg_namespace WHERE nspname = {}", lit(&schema))).await,
    };
    assert_eq!(left, vec![vec!["0".to_string()]], "{}: schema left behind", c.id);

    drop(as_creator);
    if c.creator != Creator::Admin {
        run(&mut s, &format!("REVOKE CREATE ON DATABASE {db} FROM {}", q(&creator))).await;
        let kind = if c.creator == Creator::User { "USER" } else { "ROLE" };
        run(&mut s, &format!("DROP {kind} {}", q(&creator))).await;
    }
    for p in [&owner, &reader] {
        let kind = if c.password.is_some() { dbine_driver::PrincipalKind::User } else { dbine_driver::PrincipalKind::Role };
        let drop = d.security_script(&SecurityAction::Drop { name: p.clone(), kind }).unwrap();
        run(&mut s, &drop).await;
    }
}

#[tokio::test]
#[ignore]
async fn postgres() {
    create_and_drop(Case { id: "postgres", env: "DBINE_TEST_POSTGRES_URL", catalog: Catalog::Pg, password: None, grantable: true, upper: false, creator: Creator::NoInherit }).await;
}

#[tokio::test]
#[ignore]
async fn timescaledb() {
    create_and_drop(Case { id: "timescaledb", env: "DBINE_TEST_TIMESCALEDB_URL", catalog: Catalog::Pg, password: None, grantable: true, upper: false, creator: Creator::NoInherit })
        .await;
}

#[tokio::test]
#[ignore]
async fn yugabytedb() {
    create_and_drop(Case { id: "yugabytedb", env: "DBINE_TEST_YUGABYTE_URL", catalog: Catalog::Pg, password: None, grantable: true, upper: false, creator: Creator::NoInherit })
        .await;
}

/// The image's user is SYSADMIN.
#[tokio::test]
#[ignore]
async fn opengauss() {
    create_and_drop(Case {
        id: "opengauss",
        env: "DBINE_TEST_OPENGAUSS_URL",
        catalog: Catalog::Pg,
        password: None,
        grantable: true,
        upper: false,
        creator: Creator::NoInheritMd5,
    })
    .await;
}

/// Members hold the owner's rights: the owner goes in the create.
#[tokio::test]
#[ignore]
async fn cockroachdb() {
    create_and_drop(Case { id: "cockroachdb", env: "DBINE_TEST_COCKROACH_URL", catalog: Catalog::Cockroach, password: None, grantable: true, upper: false, creator: Creator::Member })
        .await;
}

/// No WITH GRANT OPTION on Materialize.
#[tokio::test]
#[ignore]
async fn materialize() {
    create_and_drop(Case {
        id: "materialize",
        env: "DBINE_TEST_MATERIALIZE_URL",
        catalog: Catalog::Materialize,
        password: None,
        grantable: false,
        upper: false,
        creator: Creator::MzRole,
    })
    .await;
}

/// Users only (no roles).
#[tokio::test]
#[ignore]
async fn risingwave() {
    create_and_drop(Case {
        id: "risingwave",
        env: "DBINE_TEST_RISINGWAVE_URL",
        catalog: Catalog::RisingWave,
        password: Some("pw"),
        grantable: true,
        upper: false,
        creator: Creator::User,
    })
    .await;
}

/// No WITH GRANT OPTION on H2; users need a password; only an admin
/// creates schemas.
#[tokio::test]
#[ignore]
async fn h2() {
    create_and_drop(Case { id: "h2", env: "DBINE_TEST_H2_URL", catalog: Catalog::H2, password: Some("pw"), grantable: false, upper: false, creator: Creator::Admin }).await;
}

#[tokio::test]
#[ignore]
async fn greenplum_family() {
    for (id, env) in [("cloudberry", "DBINE_TEST_CLOUDBERRY_URL"), ("greengage", "DBINE_TEST_GREENGAGE_URL")] {
        create_and_drop(Case { id, env, catalog: Catalog::Pg, password: None, grantable: true, upper: false, creator: Creator::NoInherit }).await;
    }
}
