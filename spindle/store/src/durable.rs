//! Crash-safe creation of the files and directories Spindle keeps outside SQLite.
//!
//! A file is replaced whole: written to a unique temporary name beside it, flushed,
//! renamed over the old one, and then the directory is flushed, so after a power cut
//! the name holds the old or the new content, never a torn or empty file, and a
//! completed write is not undone. New directories have their entry flushed in their
//! parent. Rust's `sync_all` is `fsync` on Linux and `F_FULLFSYNC` on Apple targets.
//! Windows cannot flush a directory; NTFS journals the rename itself.
use crate::Error;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Flushes a directory's entries (created, removed and renamed names) to disk.
pub fn sync_dir(path: &Path) -> std::io::Result<()> {
    crate::sync_dir(path)
}

/// Flushes the entry of `path` in its parent directory.
pub fn sync_parent(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => sync_dir(parent),
        _ => sync_dir(Path::new(".")),
    }
}

/// Replaces `path` with `bytes` atomically and durably (owner-only on Unix).
pub fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let pending = path.with_file_name(format!(
        ".{}.{}-{}.tmp",
        name.to_string_lossy(),
        crate::process_id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        {
            let mut file = options.open(&pending)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(&pending, path)?;
        sync_parent(path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&pending);
    }
    result
}

/// Creates `path` and any missing parents (0700 on Unix), flushing each new entry into
/// its parent so the directory survives a power cut once this returns.
pub fn create_dir(path: &Path) -> std::io::Result<()> {
    let mut missing = vec![];
    let mut next = Some(path);
    while let Some(dir) = next {
        if dir.as_os_str().is_empty() || std::fs::symlink_metadata(dir).is_ok() {
            break;
        }
        missing.push(dir);
        next = dir.parent();
    }
    for dir in missing.into_iter().rev() {
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(dir) {
            Ok(()) => sync_parent(dir)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    if std::fs::metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(std::io::ErrorKind::AlreadyExists.into())
    }
}

/// On macOS `fsync` leaves writes in the drive's cache; SQLite flushes them with
/// `F_FULLFSYNC` only when asked (commits and checkpoints). Elsewhere a no-op.
pub fn full_flushes(db: &rusqlite::Connection) -> rusqlite::Result<()> {
    #[cfg(target_os = "macos")]
    {
        db.pragma_update(None, "fullfsync", "ON")?;
        db.pragma_update(None, "checkpoint_fullfsync", "ON")?;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = db;
    Ok(())
}

impl Error {
    /// Whether this failure means the disk (or the user's quota) is full: SQLite's
    /// `SQLITE_FULL` or an `ENOSPC`/`EDQUOT` write. Like any storage failure it fences
    /// the handle and is ambiguous: SQLite can report it after the step's commit record
    /// was written, so reopen and repeat the step with the same identity once space is
    /// freed.
    pub fn is_disk_full(&self) -> bool {
        match self {
            Self::Sql(rusqlite::Error::SqliteFailure(error, _)) => {
                error.code == rusqlite::ErrorCode::DiskFull
            }
            Self::Io(error) => is_disk_full(error),
            _ => false,
        }
    }
}

/// Whether an I/O error means the disk or quota is full.
pub fn is_disk_full(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::StorageFull
        || error.kind() == std::io::ErrorKind::QuotaExceeded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_replace_whole_files_and_leave_no_temporaries() {
        let root = std::env::temp_dir().join(format!("spindle-durable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("a/b");
        create_dir(&dir).unwrap();
        create_dir(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join("a"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        let file = dir.join("settings.json");
        write(&file, b"old").unwrap();
        write(&file, b"new").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"new");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["settings.json"]);
        assert!(create_dir(&file).is_err(), "a file is not a directory");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn full_disk_errors_are_recognised() {
        let full = Error::Sql(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            None,
        ));
        assert!(full.is_disk_full());
        assert!(Error::Io(std::io::ErrorKind::StorageFull.into()).is_disk_full());
        assert!(!Error::Io(std::io::ErrorKind::NotFound.into()).is_disk_full());
        assert!(!Error::Paused.is_disk_full());
    }
}
