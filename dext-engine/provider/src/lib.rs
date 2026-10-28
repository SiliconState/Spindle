pub mod catalog;
pub mod claude_subscription;
#[cfg(feature = "native")]
pub mod host;
#[cfg(feature = "native")]
pub mod provider;
pub mod wire;

pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Serializes tests that mutate process-wide environment variables.
///
/// Poisoning is deliberately ignored. A panicking test can leave an env var it
/// set behind, but honouring the poison would turn that one real failure into a
/// cascade of unrelated ones across every module and bury the actual cause —
/// and every test here restores what it sets on the non-panicking path.
#[cfg(all(test, feature = "native"))]
fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    // Provider tests run without an embedding program, so they install the
    // launcher runner their OAuth browser coverage exercises.
    host::install(no_secret_command, test_command_runner);
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(all(test, feature = "native"))]
fn no_secret_command(_command: &str) -> Option<String> {
    None
}

#[cfg(all(test, feature = "native"))]
fn test_command_runner(
    mut command: std::process::Command,
    _label: &str,
    _timeout: std::time::Duration,
) -> Result<host::CommandOutput, String> {
    let output = command.output().map_err(|error| error.to_string())?;
    Ok(host::CommandOutput {
        stdout: output.stdout,
        stderr: output.stderr,
        code: output.status.code().unwrap_or(-1),
    })
}
