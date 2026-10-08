//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]):
//! what `information_schema` reports and `ALTER DATABASE` changes, per
//! engine.
//!
//! - Every engine with databases: size, tables and views (facts).
//! - MySQL (Aurora, Cloud SQL), MariaDB, TiDB and OceanBase: the default
//!   character set and collation (one combined `ALTER`, they depend on each
//!   other). MySQL 8.0.22+ adds `READ ONLY` and 8.0.16+ `DEFAULT
//!   ENCRYPTION` (self-managed only: it needs a keyring); MariaDB 10.5+
//!   its `COMMENT`; TiDB its `PLACEMENT POLICY`; OceanBase `READ ONLY` /
//!   `READ WRITE`. Each is offered only when the server reports its
//!   current value.
//! - SingleStore: synchronous or asynchronous replication.
//! - StarRocks and Doris (VeloDB): the data and replica quotas; Doris
//!   adds the transaction quota and `SET PROPERTIES`.
//! - GreptimeDB: the default TTL (`SET 'ttl'` / `UNSET 'ttl'`).
//! - Databend (its `ALTER DATABASE` only renames) and Manticore (no
//!   databases): none.
//!
//! Each change is one statement; a read-only database is made writable
//! first and read-only last, as nothing else can change while it is.

use crate::create_db::{check, properties, word};
use crate::session::{at, lit, named, MySqlSession};
use crate::{err, Variant};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{DatabaseProperties, Error, Field, FieldChoices, FieldKind, PropertyInfo, Result};
use mysql_async::prelude::Queryable;
use std::collections::BTreeMap;

/// Engines with "Propiedades".
pub(crate) fn supported(v: Variant) -> bool {
    !matches!(v.base(), Variant::Databend | Variant::Manticore)
}

fn yes(v: &str) -> bool {
    matches!(v.trim(), "true" | "1" | "ON" | "on" | "YES" | "Y")
}

fn flag(on: bool) -> String {
    if on { "true".into() } else { String::new() }
}

/// A StarRocks / Doris data quota: digits with an optional unit.
fn quota(v: &str) -> Result<String> {
    let s: String = v.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_uppercase();
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    let unit = &s[digits.len()..];
    let units = ["", "B", "K", "KB", "M", "MB", "G", "GB", "T", "TB", "P", "PB"];
    if !digits.is_empty() && digits.len() <= 18 && units.contains(&unit) {
        Ok(s)
    } else {
        Err(Error::Query(format!("cuota de datos: «{v}» no es un tamaño (500MB, 10GB, 1TB…)")))
    }
}

/// `SHOW PROC '/dbs'` prints quotas as `1024.000 TB`: as the field takes them.
fn shown_quota(v: &str) -> String {
    let v = v.trim();
    if let Some((n, unit)) = v.split_once(' ') {
        if let Ok(f) = n.parse::<f64>() {
            if f.fract() == 0.0 && unit.chars().all(|c| c.is_ascii_alphabetic()) {
                return format!("{}{unit}", f as u64);
            }
        }
    }
    v.to_string()
}

fn count(v: &str, what: &str) -> Result<()> {
    check(!v.is_empty() && v.len() <= 19 && v.parse::<u64>().is_ok(), what, v)
}

/// The statements for `changes`, in a safe order: back to read-write
/// first, the settings, read-only last.
pub(crate) fn alter(v: Variant, database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    let v = v.base();
    if !supported(v) {
        return Err(Error::Unsupported("este motor no modifica las propiedades de una base".into()));
    }
    let db = quote_ident(Quote::Backtick, database);
    let sql_family = matches!(v, Variant::MySql | Variant::MariaDb | Variant::TiDb | Variant::OceanBase);
    let olap = matches!(v, Variant::StarRocks | Variant::Doris);
    let (mut first, mut out, mut last) = (Vec::new(), Vec::new(), Vec::new());
    let mut charset = Vec::new();
    let unknown = |k: &str| Error::Query(format!("propiedad desconocida: {k}"));
    for (key, value) in changes {
        let value = value.trim();
        match key.as_str() {
            "charset" if sql_family => {
                check(word(value), "juego de caracteres", value)?;
                charset.insert(0, format!("CHARACTER SET = {value}"));
            }
            "collation" if sql_family => {
                check(word(value), "intercalación", value)?;
                charset.push(format!("COLLATE = {value}"));
            }
            "read_only" if v == Variant::MySql => {
                if yes(value) {
                    last.push(format!("ALTER DATABASE {db} READ ONLY = 1"));
                } else {
                    first.push(format!("ALTER DATABASE {db} READ ONLY = 0"));
                }
            }
            "read_only" if v == Variant::OceanBase => {
                if yes(value) {
                    last.push(format!("ALTER DATABASE {db} READ ONLY"));
                } else {
                    first.push(format!("ALTER DATABASE {db} READ WRITE"));
                }
            }
            "encryption" if v == Variant::MySql => {
                out.push(format!("ALTER DATABASE {db} DEFAULT ENCRYPTION = '{}'", if yes(value) { "Y" } else { "N" }));
            }
            // A MySQL connection can reach a MariaDB server.
            "comment" if matches!(v, Variant::MariaDb | Variant::MySql) => out.push(format!("ALTER DATABASE {db} COMMENT = {}", lit(value))),
            "placement_policy" if v == Variant::TiDb => {
                let p = if value.is_empty() { "DEFAULT".to_string() } else { quote_ident(Quote::Backtick, value) };
                out.push(format!("ALTER DATABASE {db} PLACEMENT POLICY = {p}"));
            }
            "replication" if v == Variant::SingleStore => {
                check(matches!(value, "SYNC" | "ASYNC"), "replicación", value)?;
                out.push(format!("ALTER DATABASE {db} SET {value} REPLICATION"));
            }
            "data_quota" if olap => out.push(format!("ALTER DATABASE {db} SET DATA QUOTA {}", quota(value)?)),
            "replica_quota" if olap => {
                count(value, "cuota de réplicas")?;
                out.push(format!("ALTER DATABASE {db} SET REPLICA QUOTA {value}"));
            }
            "transaction_quota" if v == Variant::Doris => {
                count(value, "cuota de transacciones")?;
                out.push(format!("ALTER DATABASE {db} SET TRANSACTION QUOTA {value}"));
            }
            "properties" if v == Variant::Doris => {
                let o: BTreeMap<String, String> = [("properties".to_string(), value.to_string())].into();
                let p = properties(None, &o)?;
                if p.is_empty() {
                    return Err(Error::Query("propiedades: no se pueden quitar desde aquí; escribí las que querés cambiar".into()));
                }
                out.push(format!("ALTER DATABASE {db} SET{}", p.replacen('\n', " ", 1)));
            }
            "ttl" if v == Variant::GreptimeDb => {
                if value.is_empty() {
                    out.push(format!("ALTER DATABASE {db} UNSET 'ttl'"));
                } else {
                    check(value.chars().all(|c| c.is_ascii_alphanumeric() || c == ' '), "retención", value)?;
                    out.push(format!("ALTER DATABASE {db} SET 'ttl' = {}", lit(value)));
                }
            }
            k => return Err(unknown(k)),
        }
    }
    if !charset.is_empty() {
        out.insert(0, format!("ALTER DATABASE {db} {}", charset.join(" ")));
    }
    Ok(first.into_iter().chain(out).chain(last).collect())
}

pub(crate) fn script(v: Variant, database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(v, database, changes)?.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n"))
}

/// `1.5 GB`.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// The `PROPERTIES` of a Doris `SHOW CREATE DATABASE`, one `key=value`
/// per line.
fn create_properties(ddl: &str) -> String {
    crate::structure::olap_properties(ddl).iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n")
}

/// The `ttl` of a GreptimeDB `SHOW CREATE DATABASE` (`WITH(ttl = '7d')`).
fn create_ttl(ddl: &str) -> Option<String> {
    let i = ddl.find("ttl")?;
    let rest = ddl[i + 3..].trim_start_matches(['\'', '"', ' ']).strip_prefix('=')?.trim_start();
    let q = rest.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    rest[1..].split(q).next().map(str::to_string)
}

fn info(group: &str, label: &str, value: String) -> PropertyInfo {
    PropertyInfo { group: group.into(), label: label.into(), value }
}

impl MySqlSession {
    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let v = self.variant;
        if !supported(v) {
            return Err(Error::Unsupported("este motor no muestra las propiedades de una base".into()));
        }
        let name = lit(database);
        let sql_family = matches!(v, Variant::MySql | Variant::MariaDb | Variant::TiDb | Variant::OceanBase);
        let mut fields = Vec::new();
        let mut values = BTreeMap::new();
        let mut info_rows = Vec::new();
        let mut warnings = BTreeMap::new();

        let schemata = format!("SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = {name}");
        let schema = if sql_family { self.rows(&schemata).await? } else { self.optional_rows(&schemata).await };
        if sql_family && schema.is_empty() {
            return Err(Error::Query(format!("no existe la base «{database}»")));
        }
        let (charset, collation) = schema.first().map_or((None, None), |r| (at(r, 0), at(r, 1)));

        // Facts: size, tables and views.
        let sizes = self
            .optional_rows(&format!(
                "SELECT COUNT(CASE WHEN TABLE_TYPE <> 'VIEW' THEN 1 END), COUNT(CASE WHEN TABLE_TYPE = 'VIEW' THEN 1 END),
                        SUM(COALESCE(DATA_LENGTH, 0) + COALESCE(INDEX_LENGTH, 0))
                 FROM information_schema.TABLES WHERE TABLE_SCHEMA = {name}"
            ))
            .await;
        if let Some(r) = sizes.first() {
            let n = |i: usize| at(r, i).and_then(|s| s.split('.').next().and_then(|s| s.parse::<u64>().ok())).unwrap_or(0);
            info_rows.push(info("", "Tamaño (datos e índices)", human(n(2))));
            info_rows.push(info("", "Tablas", n(0).to_string()));
            info_rows.push(info("", "Vistas", n(1).to_string()));
        }

        if sql_family {
            values.insert("charset".into(), charset.clone().unwrap_or_default());
            values.insert("collation".into(), collation.clone().unwrap_or_default());
            fields.push(Field::new("charset", "Juego de caracteres (character set)", FieldKind::Text).help("El que toman las tablas nuevas."));
            fields.push(Field::new("collation", "Intercalación (collation)", FieldKind::Text).help("Define cómo se ordenan y comparan los textos de las tablas nuevas."));
            let note = "Solo cambia el valor por defecto de las tablas nuevas: las tablas y columnas existentes conservan el suyo.";
            warnings.insert("charset".into(), note.to_string());
            warnings.insert("collation".into(), note.to_string());
        } else {
            if let Some(c) = charset {
                info_rows.push(info("", "Juego de caracteres", c));
            }
            if let Some(c) = collation {
                info_rows.push(info("", "Intercalación", c));
            }
        }

        let maria = v == Variant::MariaDb || (v == Variant::MySql && self.server_version_text().await.contains("MariaDB"));
        match v {
            Variant::MySql if !maria => {
                // MySQL 8.0.22+: READ ONLY in SCHEMATA_EXTENSIONS.
                let r = self.optional_rows(&format!("SELECT OPTIONS FROM information_schema.SCHEMATA_EXTENSIONS WHERE SCHEMA_NAME = {name}")).await;
                if let Some(r) = r.first() {
                    values.insert("read_only".into(), flag(at(r, 0).is_some_and(|o| o.to_ascii_uppercase().contains("READ ONLY=1"))));
                    fields.push(Field::new("read_only", "Solo lectura (READ ONLY)", FieldKind::Bool).group("Estado"));
                    warnings.insert(
                        "read_only".into(),
                        "En solo lectura se rechaza toda escritura y todo DDL en la base, también para los administradores; el cambio espera a que terminen las transacciones que la modifican.".into(),
                    );
                }
                // MySQL 8.0.16+: DEFAULT_ENCRYPTION; managed services have no keyring of yours.
                if !matches!(self.product, Variant::AuroraMySql | Variant::CloudSqlMySql) {
                    let r = self.optional_rows(&format!("SELECT DEFAULT_ENCRYPTION FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = {name}")).await;
                    if let Some(r) = r.first() {
                        values.insert("encryption".into(), flag(at(r, 0).is_some_and(|e| yes(&e))));
                        fields.push(
                            Field::new("encryption", "Cifrado por defecto (DEFAULT ENCRYPTION)", FieldKind::Bool)
                                .help("Cifra las tablas nuevas. Requiere un keyring configurado en el servidor.")
                                .group("Seguridad"),
                        );
                        warnings.insert(
                            "encryption".into(),
                            "Solo afecta a las tablas nuevas; desactivarlo permite crear tablas sin cifrar en esta base.".into(),
                        );
                    }
                }
            }
            Variant::OceanBase => {
                let r = self.optional_rows(&format!("SELECT READ_ONLY FROM oceanbase.DBA_OB_DATABASES WHERE DATABASE_NAME = {name}")).await;
                if let Some(r) = r.first() {
                    values.insert("read_only".into(), flag(at(r, 0).is_some_and(|o| yes(&o))));
                    fields.push(Field::new("read_only", "Solo lectura (READ ONLY)", FieldKind::Bool).group("Estado"));
                    warnings.insert("read_only".into(), "En solo lectura se rechaza toda escritura en la base.".into());
                }
            }
            Variant::TiDb => {
                let r = self.optional_rows(&format!("SELECT TIDB_PLACEMENT_POLICY_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = {name}")).await;
                if let Some(r) = r.first() {
                    values.insert("placement_policy".into(), at(r, 0).unwrap_or_default());
                    fields.push(
                        Field::new("placement_policy", "Política de ubicación (placement policy)", FieldKind::Text)
                            .help("Vacía: sin política. La toman las tablas que no tengan una propia.")
                            .group("Ubicación"),
                    );
                    warnings.insert(
                        "placement_policy".into(),
                        "Las réplicas de las tablas sin política propia se mueven a donde indique la nueva: puede generar mucho tráfico entre nodos.".into(),
                    );
                }
            }
            Variant::SingleStore => {
                let r = self.optional_rows(&format!("SELECT * FROM information_schema.DISTRIBUTED_DATABASES WHERE DATABASE_NAME = {name}")).await;
                if let Some(r) = r.first() {
                    if let Some(p) = named(r, &["NUM_PARTITIONS"]) {
                        info_rows.push(info("", "Particiones", p));
                    }
                    if let Some(sync) = named(r, &["IS_SYNC"]) {
                        values.insert("replication".into(), if yes(&sync) { "SYNC".into() } else { "ASYNC".into() });
                        fields.push(
                            Field::new(
                                "replication",
                                "Replicación",
                                FieldKind::Select(vec![("SYNC", "Sincrónica (SYNC)"), ("ASYNC", "Asincrónica (ASYNC)")]),
                            )
                            .group("Replicación"),
                        );
                        warnings.insert(
                            "replication".into(),
                            "Con replicación asincrónica, si cae una hoja maestra se pueden perder transacciones ya confirmadas.".into(),
                        );
                    }
                }
            }
            Variant::StarRocks | Variant::Doris => self.olap_properties(database, &mut fields, &mut values, &mut info_rows, &mut warnings).await,
            Variant::GreptimeDb => {
                let r = self.optional_rows(&format!("SHOW CREATE DATABASE {}", quote_ident(Quote::Backtick, database))).await;
                if let Some(ddl) = r.first().and_then(|r| at(r, 1)) {
                    values.insert("ttl".into(), create_ttl(&ddl).unwrap_or_default());
                    fields.push(
                        Field::new("ttl", "Retención (TTL)", FieldKind::Text)
                            .placeholder("7d, 24h, forever")
                            .help("Vacía: sin vencimiento. La toman las tablas que no indiquen otra."),
                    );
                    warnings.insert(
                        "ttl".into(),
                        "Con una retención más corta, los datos más viejos se borran en las próximas compactaciones y no se recuperan.".into(),
                    );
                }
            }
            _ => {}
        }
        if maria {
            let r = self.optional_rows(&format!("SELECT SCHEMA_COMMENT FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = {name}")).await;
            if let Some(r) = r.first() {
                values.insert("comment".into(), at(r, 0).unwrap_or_default());
                fields.push(Field::new("comment", "Comentario", FieldKind::Text));
            }
        }

        let mut choices: Vec<FieldChoices> = if sql_family { self.create_database_choices_impl().await.unwrap_or_default() } else { Vec::new() };
        choices.retain(|c| matches!(c.key.as_str(), "charset" | "collation" | "placement_policy"));
        for c in &mut choices {
            c.default = None;
        }
        Ok(DatabaseProperties { fields, values, info: info_rows, choices, warnings })
    }

    /// StarRocks / Doris: quotas from `SHOW PROC '/dbs'` (admin only) and,
    /// in Doris, the `PROPERTIES` of `SHOW CREATE DATABASE`.
    async fn olap_properties(
        &mut self,
        database: &str,
        fields: &mut Vec<Field>,
        values: &mut BTreeMap<String, String>,
        info_rows: &mut Vec<PropertyInfo>,
        warnings: &mut BTreeMap<String, String>,
    ) {
        let doris = self.variant == Variant::Doris;
        let dbs = self.optional_rows("SHOW PROC '/dbs'").await;
        // Older Doris names them `default_cluster:db`.
        let row = dbs.iter().find(|r| named(r, &["DbName"]).is_some_and(|n| n == database || n.rsplit(':').next() == Some(database)));
        if let Some(r) = row {
            for (col, label) in [("ReplicaCount", "Réplicas"), ("RunningTransactionNum", "Transacciones en curso")] {
                if let Some(n) = named(r, &[col]) {
                    info_rows.push(info("Cuotas", label, n));
                }
            }
            let quota_warning = "Si la base ya supera la nueva cuota, se rechazan las cargas e inserciones nuevas hasta liberar espacio.";
            if let Some(q) = named(r, &["Quota", "DataQuota"]) {
                values.insert("data_quota".into(), shown_quota(&q));
                fields.push(Field::new("data_quota", "Cuota de datos (DATA QUOTA)", FieldKind::Text).placeholder("500MB, 10GB, 1TB").group("Cuotas"));
                warnings.insert("data_quota".into(), quota_warning.into());
            }
            if let Some(q) = named(r, &["ReplicaQuota"]) {
                values.insert("replica_quota".into(), q);
                fields.push(Field::new("replica_quota", "Cuota de réplicas (REPLICA QUOTA)", FieldKind::Number).group("Cuotas"));
                warnings.insert("replica_quota".into(), quota_warning.into());
            }
            if doris {
                if let Some(q) = named(r, &["TransactionQuota"]) {
                    values.insert("transaction_quota".into(), q);
                    fields.push(
                        Field::new("transaction_quota", "Cuota de transacciones (TRANSACTION QUOTA)", FieldKind::Number)
                            .help("Cuántas transacciones de carga pueden correr a la vez.")
                            .group("Cuotas"),
                    );
                }
            }
        }
        if doris {
            let r = self.optional_rows(&format!("SHOW CREATE DATABASE {}", quote_ident(Quote::Backtick, database))).await;
            if let Some(ddl) = r.first().and_then(|r| at(r, 1)) {
                values.insert("properties".into(), create_properties(&ddl));
                fields.push(
                    Field::new("properties", "Propiedades (PROPERTIES)", FieldKind::Textarea)
                        .placeholder("clave=valor (una por línea)")
                        .help("Van en SET PROPERTIES tal como las escribís. Las que borres no se quitan de la base.")
                        .group("Propiedades"),
                );
                warnings.insert(
                    "properties".into(),
                    "Las propiedades de la base (como replication_allocation) son las de las tablas nuevas; algunas, como binlog.enable, afectan también a las existentes.".into(),
                );
            }
        }
    }

    async fn server_version_text(&mut self) -> String {
        self.optional_rows("SELECT VERSION()").await.first().and_then(|r| at(r, 0)).unwrap_or_default()
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        let statements = alter(self.variant, database, changes)?;
        for (i, sql) in statements.iter().enumerate() {
            if let Err(e) = self.conn.query_drop(sql.as_str()).await {
                let e = err(e);
                return Err(if i == 0 { e } else { Error::Query(format!("se aplicaron {i} de {} cambios; falló: {sql}\n{e}", statements.len())) });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn mysql_order_and_combined_charset() {
        let s = script(
            Variant::MySql,
            "ven`tas",
            &c(&[("read_only", "true"), ("collation", "utf8mb4_bin"), ("charset", "utf8mb4"), ("encryption", "true")]),
        )
        .unwrap();
        assert_eq!(
            s,
            "ALTER DATABASE `ven``tas` CHARACTER SET = utf8mb4 COLLATE = utf8mb4_bin;
ALTER DATABASE `ven``tas` DEFAULT ENCRYPTION = 'Y';
ALTER DATABASE `ven``tas` READ ONLY = 1;"
        );
        // Writable first, so the other changes can run.
        let s = script(Variant::AuroraMySql, "v", &c(&[("read_only", ""), ("collation", "utf8mb4_0900_ai_ci")])).unwrap();
        assert_eq!(s, "ALTER DATABASE `v` READ ONLY = 0;\nALTER DATABASE `v` COLLATE = utf8mb4_0900_ai_ci;");
    }

    #[test]
    fn per_engine_settings() {
        assert_eq!(script(Variant::MariaDb, "v", &c(&[("comment", "it's")])).unwrap(), "ALTER DATABASE `v` COMMENT = 'it''s';");
        assert_eq!(script(Variant::TiDb, "v", &c(&[("placement_policy", "p1")])).unwrap(), "ALTER DATABASE `v` PLACEMENT POLICY = `p1`;");
        assert_eq!(script(Variant::TiDb, "v", &c(&[("placement_policy", "")])).unwrap(), "ALTER DATABASE `v` PLACEMENT POLICY = DEFAULT;");
        assert_eq!(script(Variant::OceanBase, "v", &c(&[("read_only", "")])).unwrap(), "ALTER DATABASE `v` READ WRITE;");
        assert_eq!(script(Variant::SingleStore, "v", &c(&[("replication", "ASYNC")])).unwrap(), "ALTER DATABASE `v` SET ASYNC REPLICATION;");
        assert_eq!(
            script(Variant::VeloDb, "v", &c(&[("data_quota", "10 gb"), ("replica_quota", "1024"), ("transaction_quota", "100")])).unwrap(),
            "ALTER DATABASE `v` SET DATA QUOTA 10GB;\nALTER DATABASE `v` SET REPLICA QUOTA 1024;\nALTER DATABASE `v` SET TRANSACTION QUOTA 100;"
        );
        assert_eq!(
            script(Variant::Doris, "v", &c(&[("properties", "replication_allocation=tag.location.default: 1")])).unwrap(),
            "ALTER DATABASE `v` SET PROPERTIES (\"replication_allocation\" = \"tag.location.default: 1\");"
        );
        assert_eq!(script(Variant::GreptimeDb, "v", &c(&[("ttl", "7d")])).unwrap(), "ALTER DATABASE `v` SET 'ttl' = '7d';");
        assert_eq!(script(Variant::GreptimeDb, "v", &c(&[("ttl", " ")])).unwrap(), "ALTER DATABASE `v` UNSET 'ttl';");
    }

    #[test]
    fn values_and_keys_are_checked() {
        for bad in [("charset", "utf8; DROP"), ("collation", "a b"), ("charset", ""), ("nope", "1"), ("placement_policy", "p")] {
            assert!(script(Variant::MySql, "v", &c(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script(Variant::MariaDb, "v", &c(&[("read_only", "true")])).is_err());
        assert!(script(Variant::SingleStore, "v", &c(&[("replication", "SYNC;")])).is_err());
        assert!(script(Variant::StarRocks, "v", &c(&[("data_quota", "10XB")])).is_err());
        assert!(script(Variant::StarRocks, "v", &c(&[("replica_quota", "-1")])).is_err());
        assert!(script(Variant::StarRocks, "v", &c(&[("transaction_quota", "1")])).is_err());
        assert!(script(Variant::Doris, "v", &c(&[("properties", "")])).is_err());
        assert!(script(Variant::Doris, "v", &c(&[("properties", "a=b\\\\c")])).is_err());
        assert!(script(Variant::GreptimeDb, "v", &c(&[("ttl", "7d')")])).is_err());
        assert!(script(Variant::Databend, "v", &c(&[])).is_err());
        assert!(script(Variant::Manticore, "v", &c(&[])).is_err());
    }

    #[test]
    fn reading_helpers() {
        assert_eq!(shown_quota("1024.000 TB"), "1024TB");
        assert_eq!(shown_quota("1.500 GB"), "1.500 GB");
        assert_eq!(create_properties("CREATE DATABASE `d`\nPROPERTIES (\n\"replication_allocation\" = \"tag.location.default: 1\",\n\"a\" = \"b\"\n)"), "a=b\nreplication_allocation=tag.location.default: 1");
        assert_eq!(create_properties("CREATE DATABASE `d`"), "");
        assert_eq!(create_ttl("CREATE DATABASE IF NOT EXISTS d\nWITH(\n  ttl = '7d'\n)").as_deref(), Some("7d"));
        assert_eq!(create_ttl("CREATE DATABASE d WITH('ttl'='1h')").as_deref(), Some("1h"));
        assert_eq!(create_ttl("CREATE DATABASE d"), None);
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(10), "10 B");
    }
}
