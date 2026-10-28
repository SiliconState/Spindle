//! Disk housekeeping for the host directory: removal that survives overlayfs leftovers,
//! usage and free-space figures for Settings > Host.
use std::path::Path;

/// Removes a tree even where a sandbox left directories without owner permissions
/// (overlayfs keeps `work/work` at mode 000, which plain remove_dir_all cannot enter).
/// Directories are opened up to u+rwx first; symlinks are never followed.
pub(super) fn remove_tree(path: &Path) -> std::io::Result<()> {
    open_up(path);
    match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn open_up(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if mode & 0o700 != 0o700 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o700));
        }
    }
    for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
        // DirEntry::file_type does not follow symlinks.
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            open_up(&entry.path());
        }
    }
}

/// Removes a tree, logging (not failing) when something stays behind.
pub(super) fn purge(path: &Path) {
    if let Err(error) = remove_tree(path) {
        eprintln!(
            "spindle serve: could not remove {}: {error}; it stays for operator cleanup",
            path.display()
        );
    }
}

/// Bytes a tree occupies on disk (allocated blocks), without following symlinks.
pub(super) fn usage(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    #[cfg(unix)]
    let own = {
        use std::os::unix::fs::MetadataExt;
        meta.blocks().saturating_mul(512)
    };
    #[cfg(not(unix))]
    let own = meta.len();
    if !meta.is_dir() {
        return own;
    }
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| usage(&entry.path()))
        .fold(own, u64::saturating_add)
}

/// Available and total bytes of the filesystem holding `path`.
#[allow(clippy::unnecessary_cast)]
pub(super) fn space(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: statvfs writes only into the zeroed struct we own; name is NUL-terminated.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(name.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        let unit = stat.f_frsize as u64;
        Some((
            (stat.f_bavail as u64).saturating_mul(unit),
            (stat.f_blocks as u64).saturating_mul(unit),
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

// Mode bits and free space are read only on Unix, where the host runs.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn removal_opens_mode_000_directories_and_keeps_symlink_targets() {
        let root = std::env::temp_dir().join(format!("spindle-disk-{}", std::process::id()));
        let _ = remove_tree(&root);
        let outside = root.join("outside");
        let tree = root.join("tree");
        let work = tree.join("overlay/work/work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(work.join("file"), b"x").unwrap();
        std::fs::write(tree.join("data"), vec![7u8; 8192]).unwrap();
        std::fs::write(outside.join("keep"), b"keep").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::os::unix::fs::symlink(&outside, tree.join("link")).unwrap();
            std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o000)).unwrap();
            std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o500)).unwrap();
        }
        assert!(usage(&tree) > 0);
        remove_tree(&tree).unwrap();
        assert!(!tree.exists());
        assert!(
            outside.join("keep").exists(),
            "symlinked directory untouched"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&outside).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o500, "never chmods through a symlink");
        }
        remove_tree(&tree).unwrap();
        let (free, total) = space(&root).unwrap();
        assert!(free <= total && total > 0);
        remove_tree(&root).unwrap();
    }
}
