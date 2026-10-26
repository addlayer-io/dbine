//! A folder on disk as the backup's place.

use dbine_sync::folder::FolderStore;
use dbine_sync::{CloudStore, ProviderKind, SyncError};

#[tokio::test]
async fn files_by_name_in_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let store = FolderStore::new(dir.path());
    assert_eq!(store.kind(), ProviderKind::Folder);
    assert_eq!(
        store.account().await.unwrap(),
        dir.path().display().to_string()
    );

    assert!(store.stat("b.json").await.unwrap().is_none());
    assert!(store.download("b.json").await.unwrap().is_none());

    let m1 = store.upload("b.json", b"uno".to_vec()).await.unwrap();
    assert_eq!(m1.size, Some(3));
    assert!(m1.modified.is_some());
    assert_eq!(store.stat("b.json").await.unwrap(), Some(m1.clone()));
    assert_eq!(store.download("b.json").await.unwrap().unwrap(), b"uno");

    // Another size: another revision. Nothing is left aside.
    let m2 = store.upload("b.json", b"dos!".to_vec()).await.unwrap();
    assert_ne!(m1.revision, m2.revision);
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["b.json"]);

    store.delete("b.json").await.unwrap();
    assert!(store.stat("b.json").await.unwrap().is_none());
    // Deleting what isn't there is fine.
    store.delete("b.json").await.unwrap();
}

#[tokio::test]
async fn a_folder_that_isnt_there() {
    let dir = tempfile::tempdir().unwrap();
    let gone = dir.path().join("gone");
    let store = FolderStore::new(&gone);
    match store.account().await {
        Err(SyncError::Local(m)) => assert!(m.contains("no existe"), "{m}"),
        other => panic!("expected a local error, got {other:?}"),
    }
    // A file isn't a folder either.
    let file = dir.path().join("file");
    std::fs::write(&file, b"x").unwrap();
    assert!(FolderStore::new(&file).account().await.is_err());
    // Writing into it fails as a local error.
    assert!(matches!(
        store.upload("b.json", b"x".to_vec()).await,
        Err(SyncError::Local(_))
    ));
    // Reading a "file" that's a folder fails too.
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let parent = FolderStore::new(dir.path());
    assert!(parent.download("sub").await.is_err());
    assert!(parent.delete("sub").await.is_err());
}
