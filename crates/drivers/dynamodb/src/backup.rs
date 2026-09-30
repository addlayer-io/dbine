//! Native backups (docs/backups.md): DynamoDB's on-demand backups, one per
//! table, through the API the driver already talks to (CreateBackup,
//! ListBackups, RestoreTableFromBackup, DeleteBackup). The scripts are
//! DBine's administration statements (see `admin`):
//!
//! ```text
//! CREATE BACKUP "name" FOR TABLE "t"
//! RESTORE TABLE "new" FROM BACKUP "arn"
//! DROP BACKUP "arn"
//! ```
//!
//! A "database" is the account and region, so the tab opens from the
//! connection and the table is an option. A restore always makes a new
//! table (DynamoDB doesn't restore over an existing one).

use crate::{err, DynamoSession};
use aws_sdk_dynamodb::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_dynamodb::primitives::{DateTime, DateTimeFormat};
use aws_sdk_dynamodb::types::{BackupStatus, BackupSummary, BackupType, BackupTypeFilter};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{BackupAction, BackupEntry, BackupSpec, Error, Field, FieldKind, Result};
use std::collections::BTreeMap;

pub fn spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new("table", "Tabla", FieldKind::Text)
                .required()
                .placeholder("Pedidos")
                .help("DynamoDB hace los backups por tabla."),
            Field::new("name", "Nombre del backup", FieldKind::Text)
                .placeholder("Pedidos-20240101T120000")
                .help("Letras, números, «_», «-» y «.», de 3 a 255. Vacío: la tabla y la fecha."),
        ],
        restore: true,
        restore_options: vec![Field::new("table", "Tabla nueva", FieldKind::Text)
            .placeholder("Pedidos_restaurada")
            .help("DynamoDB restaura siempre en una tabla que no existe. Vacío: la tabla del backup con «_restaurada».")],
        delete: true,
        history: true,
        server_wide: true,
        script_database: "",
        note: "Son los backups a pedido de DynamoDB (CreateBackup): quedan en la cuenta y la región, sin fecha de vencimiento, y se cobran por GB. Restaurar crea una tabla nueva; DynamoDB la termina en segundo plano. Los de AWS Backup se restauran desde AWS Backup. DynamoDB Local no tiene backups.",
    }
}

/// `err`, but an endpoint without the backup operations (DynamoDB Local
/// answers `UnknownOperationException`) says so.
pub(crate) fn backup_err<E, R>(e: SdkError<E, R>) -> Error
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug,
{
    if e.code() == Some("UnknownOperationException") {
        Error::Unsupported("Este endpoint no tiene backups de DynamoDB (DynamoDB Local no los implementa).".into())
    } else {
        err(e)
    }
}

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

fn opt<'a>(options: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    options.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// `<table>-<UTC date>`, within the name rules (3–255 of `[A-Za-z0-9_.-]`).
fn default_name(table: &str, now: DateTime) -> String {
    let stamp: String =
        now.fmt(DateTimeFormat::DateTime).unwrap_or_default().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let base: String = table.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')).take(200).collect();
    format!("{base}-{stamp}")
}

/// The table of a backup ARN (`…:table/<t>/backup/<id>`).
fn table_of_arn(arn: &str) -> Option<&str> {
    let rest = &arn[arn.find(":table/")? + ":table/".len()..];
    rest.split('/').next().filter(|t| !t.is_empty())
}

pub fn script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { options, .. } => {
            let table = opt(options, "table").ok_or_else(|| Error::Query("Elegí la tabla del backup.".into()))?;
            let name = match opt(options, "name") {
                Some(n) => n.to_string(),
                None => default_name(table, DateTime::from(std::time::SystemTime::now())),
            };
            Ok(format!("CREATE BACKUP {} FOR TABLE {}", q(&name), q(table)))
        }
        BackupAction::Restore { source, options, .. } => {
            let source = source.trim();
            if source.is_empty() {
                return Err(Error::Query("Falta el ARN del backup.".into()));
            }
            let target = match opt(options, "table") {
                Some(t) => t.to_string(),
                None => format!(
                    "{}_restaurada",
                    table_of_arn(source).ok_or_else(|| Error::Query("Indicá el nombre de la tabla nueva.".into()))?
                ),
            };
            Ok(format!("RESTORE TABLE {} FROM BACKUP {}", q(&target), q(source)))
        }
        BackupAction::Delete { source } => {
            let source = source.trim();
            if source.is_empty() {
                return Err(Error::Query("Falta el ARN del backup.".into()));
            }
            Ok(format!("DROP BACKUP {}", q(source)))
        }
    }
}

fn status_label(s: Option<&BackupStatus>) -> Option<String> {
    Some(
        match s? {
            BackupStatus::Available => "disponible",
            BackupStatus::Creating => "creándose",
            BackupStatus::Deleted => "borrado",
            other => other.as_str(),
        }
        .to_string(),
    )
}

fn entry(b: &BackupSummary) -> BackupEntry {
    let mut details = Vec::new();
    if let Some(n) = b.backup_name() {
        details.push(("Nombre".to_string(), n.to_string()));
    }
    if let Some(t) = b.backup_expiry_date_time() {
        details.push(("Vence".to_string(), t.to_string()));
    }
    if let Some(a) = b.table_arn() {
        details.push(("ARN de la tabla".to_string(), a.to_string()));
    }
    let aws_backup = b.backup_type() == Some(&BackupType::AwsBackup);
    if aws_backup {
        details.push(("Administrado por".to_string(), "AWS Backup".to_string()));
    }
    BackupEntry {
        id: b.backup_arn().unwrap_or_default().to_string(),
        database: b.table_name().map(str::to_string),
        kind: b.backup_type().map(|t| t.as_str().to_string()),
        started: b.backup_creation_date_time().map(|t| t.to_string()),
        finished: None,
        size: b.backup_size_bytes().and_then(|s| u64::try_from(s).ok()),
        location: None,
        status: status_label(b.backup_status()),
        details,
        restorable: b.backup_status() == Some(&BackupStatus::Available) && !aws_backup,
    }
}

/// ListBackups, every page, newest first. `database` is a table here.
pub async fn history(s: &DynamoSession, table: Option<&str>) -> Result<Vec<BackupEntry>> {
    let mut entries = Vec::new();
    let mut start: Option<String> = None;
    loop {
        let resp = s
            .client
            .list_backups()
            .backup_type(BackupTypeFilter::All)
            .set_table_name(table.filter(|t| !t.is_empty() && *t != "default").map(str::to_string))
            .set_exclusive_start_backup_arn(start.take())
            .send()
            .await
            .map_err(backup_err)?;
        entries.extend(resp.backup_summaries().iter().map(entry));
        match resp.last_evaluated_backup_arn() {
            Some(a) if !a.is_empty() => start = Some(a.to_string()),
            _ => break,
        }
    }
    // ISO 8601 in UTC sorts as text.
    entries.sort_by(|a, b| b.started.cmp(&a.started));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::{parse_admin, Admin};

    fn o(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    const ARN: &str = "arn:aws:dynamodb:us-east-1:123456789012:table/Pe\"didos/backup/01700000000000-abcdef12";

    #[test]
    fn scripts_parse_back() {
        let b = script(&BackupAction::Backup { database: None, options: o(&[("table", "Pe\"didos"), ("name", "semanal")]) })
            .unwrap();
        assert_eq!(b, r#"CREATE BACKUP "semanal" FOR TABLE "Pe""didos""#);
        assert_eq!(
            parse_admin(&b).unwrap(),
            Some(Admin::CreateBackup { name: "semanal".into(), table: "Pe\"didos".into() })
        );

        let r = script(&BackupAction::Restore { source: ARN.into(), database: None, options: o(&[]) }).unwrap();
        assert_eq!(
            parse_admin(&r).unwrap(),
            Some(Admin::RestoreBackup { table: "Pe\"didos_restaurada".into(), arn: ARN.into() })
        );
        let r = script(&BackupAction::Restore { source: ARN.into(), database: None, options: o(&[("table", "Nueva")]) })
            .unwrap();
        assert!(r.starts_with(r#"RESTORE TABLE "Nueva" FROM BACKUP "arn:aws"#), "{r}");

        let d = script(&BackupAction::Delete { source: ARN.into() }).unwrap();
        assert_eq!(parse_admin(&d).unwrap(), Some(Admin::DropBackup { arn: ARN.into() }));

        assert!(script(&BackupAction::Backup { database: None, options: o(&[("table", " ")]) }).is_err());
        assert!(script(&BackupAction::Restore { source: "x".into(), database: None, options: o(&[]) }).is_err());
        assert!(script(&BackupAction::Delete { source: "".into() }).is_err());
    }

    #[test]
    fn default_names_follow_the_rules() {
        let n = default_name("Mis pedidos!", DateTime::from_secs(1_704_110_400));
        assert_eq!(n, "Mispedidos-20240101T120000Z");
        let b = script(&BackupAction::Backup { database: None, options: o(&[("table", "t")]) }).unwrap();
        assert!(b.starts_with("CREATE BACKUP \"t-20") && b.ends_with("\" FOR TABLE \"t\""), "{b}");
    }

    #[test]
    fn summaries_as_entries() {
        let b = BackupSummary::builder()
            .backup_arn(ARN)
            .backup_name("semanal")
            .table_name("Pedidos")
            .backup_type(BackupType::User)
            .backup_status(BackupStatus::Available)
            .backup_size_bytes(2048)
            .backup_creation_date_time(DateTime::from_secs(1_704_110_400))
            .build();
        let e = entry(&b);
        assert_eq!(e.id, ARN);
        assert_eq!(e.database.as_deref(), Some("Pedidos"));
        assert_eq!(e.kind.as_deref(), Some("USER"));
        assert_eq!(e.started.as_deref(), Some("2024-01-01T12:00:00Z"));
        assert_eq!(e.size, Some(2048));
        assert_eq!(e.status.as_deref(), Some("disponible"));
        assert!(e.restorable);
        let aws = BackupSummary::builder()
            .backup_arn(ARN)
            .backup_type(BackupType::AwsBackup)
            .backup_status(BackupStatus::Available)
            .build();
        assert!(!entry(&aws).restorable);
    }
}
