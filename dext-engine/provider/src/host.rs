use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub fn user_home_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
}

pub fn dext_state_dir() -> PathBuf {
    if let Ok(p) = std::env::var("DEXT_HOME") {
        return PathBuf::from(p);
    }
    // Unit tests that skip DEXT_HOME must never touch the real user state
    // directory: live test agents would otherwise leak project/session
    // residue into ~/.dext/projects. Give the test process one throwaway
    // home under the OS temp root instead.
    #[cfg(any(test, feature = "test-support"))]
    {
        test_process_state_dir()
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        user_home_dir().join(".dext")
    }
}

#[cfg(any(test, feature = "test-support"))]
fn test_process_state_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        // Match temp_test_dir: a fixed OS temp root, not TMPDIR, so parallel
        // tests that override and then clean their own TMPDIR cannot relocate
        // or delete the shared test state home mid-run.
        #[cfg(unix)]
        let temp_root = PathBuf::from("/tmp");
        #[cfg(not(unix))]
        let temp_root = std::env::temp_dir();
        let dir = temp_root.join(format!("dext-test-home-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create test state home");
        std::fs::canonicalize(&dir).expect("canonical test state home")
    })
    .clone()
}

#[test]
fn shared_test_state_home_is_canonical() {
    let dir = test_process_state_dir();
    assert_eq!(dir, std::fs::canonicalize(&dir).unwrap());
}

fn temp_swap_path(path: &Path) -> PathBuf {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("state");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_file_name(format!(".{name}.tmp-{}-{stamp}", std::process::id()))
}

#[cfg(windows)]
pub fn replace_file_atomically(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let from_w = wide(from);
    let to_w = wide(to);
    let ok = unsafe {
        MoveFileExW(
            from_w.as_ptr(),
            to_w.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(5) {
            if std::fs::symlink_metadata(to)?.permissions().readonly() {
                return Err(error);
            }
            // Rust's handle-based rename supports readers that share deletion;
            // the legacy MoveFileEx path rejects an open destination instead.
            std::fs::rename(from, to)
        } else {
            Err(error)
        }
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
pub fn replace_file_atomically(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

fn atomic_write_bytes_with_mode(
    path: &Path,
    data: &[u8],
    secret: bool,
    permissions: Option<&std::fs::Permissions>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = temp_swap_path(path);
    let write_result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            if secret {
                options.mode(0o600);
            }
        }
        #[cfg(not(unix))]
        let _ = secret;

        let mut file = options.open(&tmp)?;
        #[cfg(unix)]
        if secret {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(data)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions.clone())?;
        }
        file.sync_all()?;
        drop(file);
        replace_file_atomically(&tmp, path)?;
        // The new name survives a power cut once its directory is flushed (Windows
        // moves with MOVEFILE_WRITE_THROUGH). Best effort, as SQLite does: some
        // filesystems cannot flush a directory.
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            let _ = std::fs::File::open(parent).and_then(|dir| dir.sync_all());
        }
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_result
}

pub fn atomic_write_bytes(path: &Path, data: &[u8]) -> std::io::Result<()> {
    atomic_write_bytes_with_mode(path, data, false, None)
}

pub fn atomic_write_bytes_with_permissions(
    path: &Path,
    data: &[u8],
    permissions: &std::fs::Permissions,
) -> std::io::Result<()> {
    atomic_write_bytes_with_mode(path, data, false, Some(permissions))
}

pub fn atomic_write_secret(path: &Path, data: &[u8]) -> std::io::Result<()> {
    atomic_write_bytes_with_mode(path, data, true, None)
}

pub fn unix_timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Captured output of an embedding-program command run on the provider's behalf.
pub struct CommandOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: i32,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.code == 0
    }
}

pub type SecretCommand = fn(&str) -> Option<String>;
pub type LimitedCommand =
    fn(std::process::Command, &str, std::time::Duration) -> Result<CommandOutput, String>;

static SECRET_COMMAND: OnceLock<SecretCommand> = OnceLock::new();
static LIMITED_COMMAND: OnceLock<LimitedCommand> = OnceLock::new();

/// Installs the embedding program's command hooks; later calls are ignored.
///
/// This library never spawns host commands itself. The embedding program decides
/// how secret commands and launchers run, including their sandboxing, timeouts,
/// and output caps.
pub fn install(secret: SecretCommand, limited: LimitedCommand) {
    let _ = SECRET_COMMAND.set(secret);
    let _ = LIMITED_COMMAND.set(limited);
}

pub(crate) fn run_secret_command(command: &str) -> Option<String> {
    SECRET_COMMAND
        .get()
        .and_then(|run_secret_command| run_secret_command(command))
}

pub(crate) fn run_command_limited(
    command: std::process::Command,
    label: &str,
    timeout: std::time::Duration,
) -> Result<CommandOutput, String> {
    let Some(run_command_limited) = LIMITED_COMMAND.get() else {
        return Err("no command runner installed".to_string());
    };
    run_command_limited(command, label, timeout)
}
