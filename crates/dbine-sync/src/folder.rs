//! A folder on disk as the backup's place: iCloud Drive, Dropbox, a network
//! share, or the desktop apps' Google Drive / OneDrive folders.

use crate::provider::{CloudStore, ProviderKind, RemoteMeta};
use crate::Result;
use async_trait::async_trait;
use std::path::PathBuf;

pub struct FolderStore {
    pub dir: PathBuf,
}

impl FolderStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

#[async_trait]
impl CloudStore for FolderStore {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Folder
    }

    async fn account(&self) -> Result<String> {
        if !tokio::fs::metadata(&self.dir).await.map(|m| m.is_dir()).unwrap_or(false) {
            return Err(crate::SyncError::Local(format!("la carpeta {} no existe", self.dir.display())));
        }
        Ok(self.dir.display().to_string())
    }

    async fn stat(&self, name: &str) -> Result<Option<RemoteMeta>> {
        match tokio::fs::metadata(self.dir.join(name)).await {
            Ok(m) => {
                let modified = m.modified().ok().map(|t| chrono::DateTime::<chrono::Utc>::from(t));
                let nanos = modified.map(|t| t.timestamp_nanos_opt().unwrap_or_default()).unwrap_or_default();
                Ok(Some(RemoteMeta {
                    revision: format!("{nanos}-{}", m.len()),
                    modified: modified.map(|t| t.to_rfc3339()),
                    size: Some(m.len()),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn download(&self, name: &str) -> Result<Option<Vec<u8>>> {
        match tokio::fs::read(self.dir.join(name)).await {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn upload(&self, name: &str, bytes: Vec<u8>) -> Result<RemoteMeta> {
        // Write aside and rename: a sync client never sees half a file.
        let tmp = self.dir.join(format!(".{name}.tmp"));
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, self.dir.join(name)).await?;
        self.stat(name).await?.ok_or_else(|| crate::SyncError::Local("el archivo no quedó escrito".into()))
    }

    async fn delete(&self, name: &str) -> Result<()> {
        match tokio::fs::remove_file(self.dir.join(name)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
