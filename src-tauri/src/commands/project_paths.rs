//! The path guard for project files: every path the UI sends is relative to
//! the project's root, and whatever it resolves to (symlinks included) stays
//! inside the root and out of `.git`.

use crate::error::{CommandError, CommandResult};
use std::path::{Component, Path, PathBuf};

const MAX_LEN: usize = 4096;

fn bad(msg: &str) -> CommandError {
    CommandError::BadRequest(msg.into())
}

/// The project's folder, canonical. It must exist and be a directory.
pub fn canonical_root(p: &Path) -> CommandResult<PathBuf> {
    let c = p.canonicalize().map_err(|_| bad("no se encuentra la carpeta del proyecto"))?;
    if !c.is_dir() {
        return Err(bad("no se encuentra la carpeta del proyecto"));
    }
    Ok(c)
}

/// The components of a project-relative path, checked: no absolute or drive
/// paths, no `.`/`..`, no NUL, nothing under `.git`. `\` counts as `/`.
pub fn rel_ok(rel: &str) -> CommandResult<Vec<String>> {
    if rel.len() > MAX_LEN {
        return Err(bad("la ruta es demasiado larga"));
    }
    if rel.contains('\0') {
        return Err(bad("la ruta no es válida"));
    }
    let norm = rel.replace('\\', "/");
    if norm.is_empty() {
        return Err(bad("falta la ruta"));
    }
    if norm.starts_with('/') {
        return Err(bad("la ruta tiene que ser relativa al proyecto"));
    }
    let parts: Vec<&str> = norm.split('/').collect();
    let first = parts[0].as_bytes();
    if first.len() >= 2 && first[1] == b':' && first[0].is_ascii_alphabetic() {
        return Err(bad("la ruta tiene que ser relativa al proyecto"));
    }
    let mut out = Vec::with_capacity(parts.len());
    for p in parts {
        match p {
            "" | "." | ".." => return Err(bad("la ruta no puede salir de la carpeta del proyecto")),
            p if p.eq_ignore_ascii_case(".git") => return Err(bad("no se puede tocar la carpeta .git")),
            p if cfg!(windows) && p.contains(':') => return Err(bad("la ruta no es válida")),
            p => out.push(p.to_string()),
        }
    }
    Ok(out)
}

/// `abs` is the root itself, or something inside it and outside its `.git`.
fn inside(root: &Path, abs: &Path) -> CommandResult<()> {
    let rest = abs.strip_prefix(root).map_err(|_| bad("la ruta sale de la carpeta del proyecto"))?;
    match rest.components().next() {
        Some(Component::Normal(first)) if first.to_string_lossy().eq_ignore_ascii_case(".git") => {
            Err(bad("no se puede tocar la carpeta .git"))
        }
        _ => Ok(()),
    }
}

fn missing(rel: &str) -> CommandError {
    CommandError::NotFound(format!("no existe «{rel}»"))
}

/// An existing file or folder, canonical (symlinks followed, and the target
/// must still be inside the root). `root` is canonical.
pub fn resolve_existing(root: &Path, rel: &str) -> CommandResult<PathBuf> {
    let parts = rel_ok(rel)?;
    let joined = parts.iter().fold(root.to_path_buf(), |p, c| p.join(c));
    std::fs::symlink_metadata(&joined).map_err(|_| missing(rel))?;
    let canon = joined.canonicalize().map_err(|_| missing(rel))?;
    inside(root, &canon)?;
    Ok(canon)
}

/// An existing entry itself, not what it points to: a symlink stays a
/// symlink (to rename or delete it). Its parent is resolved like
/// `resolve_existing`; a non-link entry is checked the same way.
pub fn resolve_entry(root: &Path, rel: &str) -> CommandResult<PathBuf> {
    let parts = rel_ok(rel)?;
    let (name, dirs) = parts.split_last().ok_or_else(|| bad("falta la ruta"))?;
    let parent = if dirs.is_empty() { root.to_path_buf() } else { resolve_existing(root, &dirs.join("/"))? };
    if !parent.is_dir() {
        return Err(missing(rel));
    }
    let entry = parent.join(name);
    let meta = std::fs::symlink_metadata(&entry).map_err(|_| missing(rel))?;
    if !meta.file_type().is_symlink() {
        inside(root, &entry.canonicalize().map_err(|_| missing(rel))?)?;
    }
    Ok(entry)
}

/// Where a new file or folder goes: its parent folders are resolved, or
/// created one by one (each checked), and the final name is appended. Whether
/// the final path may already exist is the caller's call.
pub fn resolve_new(root: &Path, rel: &str) -> CommandResult<PathBuf> {
    let parts = rel_ok(rel)?;
    let (name, dirs) = parts.split_last().ok_or_else(|| bad("falta la ruta"))?;
    let mut cur = root.to_path_buf();
    for d in dirs {
        let next = cur.join(d);
        if std::fs::symlink_metadata(&next).is_ok() {
            let canon = next.canonicalize().map_err(|_| bad("la ruta no es válida"))?;
            inside(root, &canon)?;
            if !canon.is_dir() {
                return Err(bad("una parte de la ruta no es una carpeta"));
            }
            cur = canon;
        } else {
            std::fs::create_dir(&next).map_err(|e| CommandError::Internal(format!("no se pudo crear la carpeta «{d}»: {e}")))?;
            cur = next;
        }
    }
    Ok(cur.join(name))
}

/// `abs` relative to `root`, with `/`.
pub fn to_rel(root: &Path, abs: &Path) -> String {
    abs.strip_prefix(root)
        .unwrap_or(abs)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("dbine-paths-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        canonical_root(&d).unwrap()
    }

    #[test]
    fn relative_paths_are_checked() {
        for bad in ["", "../x", "a/../../x", "./a", "a//b", "/etc/passwd", "C:\\x", "c:/x", "\\\\srv\\share", ".git/config", "a/.GIT/x", ".Git", "a\0b"] {
            assert!(rel_ok(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(rel_ok(&"a/".repeat(3000)).is_err(), "too long");
        assert_eq!(rel_ok("consultas/ventas.sql").unwrap(), ["consultas", "ventas.sql"]);
        assert_eq!(rel_ok("consultas\\ñandú año.sql").unwrap(), ["consultas", "ñandú año.sql"]);
        assert_eq!(rel_ok(".gitignore").unwrap(), [".gitignore"]);
        assert!(rel_ok("a/..b/c..").is_ok(), "dots inside a name are fine");
    }

    #[test]
    fn resolving_stays_inside_the_root() {
        let root = tmp();
        std::fs::create_dir_all(root.join("sql")).unwrap();
        std::fs::write(root.join("sql/a.sql"), "x").unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/config"), "x").unwrap();
        assert_eq!(resolve_existing(&root, "sql/a.sql").unwrap(), root.join("sql/a.sql"));
        assert!(matches!(resolve_existing(&root, "sql/nope.sql"), Err(CommandError::NotFound(_))));
        assert!(resolve_existing(&root, "../x").is_err());
        assert!(resolve_existing(&root, ".git/config").is_err());
        assert_eq!(to_rel(&root, &root.join("sql/a.sql")), "sql/a.sql");

        // New paths: parents are created, the name isn't.
        let p = resolve_new(&root, "nuevo/sub/ñ.sql").unwrap();
        assert_eq!(p, root.join("nuevo/sub/ñ.sql"));
        assert!(root.join("nuevo/sub").is_dir() && !p.exists());
        // A file in the middle of the path isn't a folder.
        assert!(resolve_new(&root, "sql/a.sql/x.sql").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_leave_the_root() {
        let root = tmp();
        let outside = tmp();
        std::fs::write(outside.join("secret"), "x").unwrap();
        std::fs::create_dir_all(root.join("sql")).unwrap();
        std::fs::write(root.join("sql/a.sql"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
        std::os::unix::fs::symlink(root.join("sql/a.sql"), root.join("alias.sql")).unwrap();
        std::os::unix::fs::symlink(root.join(".git"), root.join("g")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();

        // Out of the root: rejected, to read or to create under.
        assert!(resolve_existing(&root, "out/secret").is_err());
        assert!(resolve_existing(&root, "out").is_err());
        assert!(resolve_new(&root, "out/new.sql").is_err());
        assert!(resolve_new(&root, "out/sub/new.sql").is_err());
        assert!(!outside.join("sub").exists());
        // Into .git through a link: rejected too.
        assert!(resolve_existing(&root, "g").is_err());
        // A link to a sibling inside the root: allowed, resolved to its target.
        assert_eq!(resolve_existing(&root, "alias.sql").unwrap(), root.join("sql/a.sql"));
        // The link itself, to delete or rename it, even when it points out.
        assert_eq!(resolve_entry(&root, "out").unwrap(), root.join("out"));
        assert!(resolve_entry(&root, "out/secret").is_err());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
