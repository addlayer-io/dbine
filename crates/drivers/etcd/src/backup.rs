//! Native backups (docs/backups.md): `snapshot save <file>`, like etcdctl's.
//! The Maintenance API streams the snapshot of the whole keyspace to the
//! client, so the file lands on this machine (not on the server). The
//! server keeps no list of snapshots, and restoring one is an offline
//! operation on a stopped member (`etcdutl snapshot restore`), so there's
//! neither history nor restore.

use crate::command::quote_arg;
use crate::{http_error, Conn};
use dbine_driver::{BackupAction, BackupSpec, Error, Field, FieldKind, Result};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

pub fn spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![Field::new("path", "Archivo", FieldKind::Text)
            .required()
            .placeholder("/Users/yo/backups/etcd.db")
            .help("Ruta en esta computadora donde se guarda el snapshot (lo manda el servidor por la conexión).")],
        restore: false,
        restore_options: Vec::new(),
        delete: false,
        history: false,
        server_wide: true,
        script_database: "",
        note: "El snapshot abarca todo el keyspace y se guarda en esta computadora, en el archivo que elijas. etcd no guarda una lista de snapshots, y restaurar uno se hace con el miembro detenido (etcdutl snapshot restore), fuera de DBine.",
    }
}

pub fn script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { options, .. } => {
            let path = options.get("path").map(|p| p.trim()).unwrap_or_default();
            if path.is_empty() {
                return Err(Error::Query("Falta el archivo donde guardar el snapshot".into()));
            }
            Ok(format!("snapshot save {}", quote_arg(path)))
        }
        BackupAction::Restore { .. } => Err(Error::Unsupported(
            "etcd restaura un snapshot sin el servidor corriendo (etcdutl snapshot restore): no se hace con una conexión".into(),
        )),
        BackupAction::Delete { .. } => Err(Error::Unsupported("etcd no guarda los snapshots: el archivo está en esta computadora".into())),
    }
}

/// Streams `/v3/maintenance/snapshot` (newline-delimited JSON messages,
/// each with a base64 `blob`) into `path`, through a `.part` file renamed
/// at the end. Returns the bytes written.
pub async fn save(conn: &Conn, path: &str) -> Result<u64> {
    let mut rb = conn.http.post(format!("{}/v3/maintenance/snapshot", conn.base)).json(&json!({}));
    if let Some(t) = conn.token.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        rb = rb.header("Authorization", t);
    }
    let mut resp = rb.send().await.map_err(http_error)?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| format!("HTTP {status}"));
        return Err(if status.as_u16() == 401 { Error::AuthFailed(msg) } else { Error::Query(msg) });
    }
    let part = format!("{path}.part");
    let io = |e: std::io::Error| Error::Query(format!("No se pudo escribir {path}: {e}"));
    let mut file = tokio::fs::File::create(&part).await.map_err(io)?;
    let result = async {
        let (mut buf, mut written) = (Vec::new(), 0u64);
        while let Some(chunk) = resp.chunk().await.map_err(http_error)? {
            buf.extend_from_slice(&chunk);
            while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                written += write_message(&mut file, &line).await?;
            }
        }
        written += write_message(&mut file, &buf).await?;
        file.flush().await.map_err(io)?;
        file.sync_all().await.map_err(io)?;
        if written == 0 {
            return Err(Error::Query("El servidor no mandó el snapshot".into()));
        }
        Ok(written)
    }
    .await;
    drop(file);
    match result {
        Ok(n) => {
            tokio::fs::rename(&part, path).await.map_err(io)?;
            Ok(n)
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&part).await;
            Err(e)
        }
    }
}

/// One streamed message: its decoded blob, appended to the file.
async fn write_message(file: &mut tokio::fs::File, line: &[u8]) -> Result<u64> {
    let blob = decode(line)?;
    file.write_all(&blob).await.map_err(|e| Error::Query(format!("No se pudo escribir el snapshot: {e}")))?;
    Ok(blob.len() as u64)
}

fn decode(line: &[u8]) -> Result<Vec<u8>> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    let v: Value = serde_json::from_slice(line).map_err(|e| Error::Query(format!("Respuesta inesperada del snapshot: {e}")))?;
    if let Some(err) = v.get("error") {
        let msg = err.get("message").and_then(Value::as_str).unwrap_or("error del servidor");
        return Err(Error::Query(format!("El snapshot falló: {msg}")));
    }
    Ok(crate::unb64(v.get("result").unwrap_or(&v).get("blob")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn scripts() {
        let options = BTreeMap::from([("path".to_string(), "/tmp/mis backups/etcd.db".to_string())]);
        assert_eq!(script(&BackupAction::Backup { database: None, options }).unwrap(), r#"snapshot save "/tmp/mis backups/etcd.db""#);
        assert!(script(&BackupAction::Backup { database: None, options: BTreeMap::new() }).is_err());
        assert!(matches!(script(&BackupAction::Delete { source: "x".into() }), Err(Error::Unsupported(_))));
    }

    #[test]
    fn stream_messages() {
        assert_eq!(decode(br#"{"result":{"remaining_bytes":"0","blob":"aGk="}}"#).unwrap(), b"hi");
        assert!(decode(b"\n").unwrap().is_empty());
        assert!(decode(br#"{"error":{"code":2,"message":"boom"}}"#).is_err());
    }
}
