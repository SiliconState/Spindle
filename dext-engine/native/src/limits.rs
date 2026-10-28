//! Workspace limits and path rules shared by every runner: the strict executor,
//! the store's in-process file tools and hosts without a sandbox (wasm). No I/O.

/// Entries a workspace tree may hold: regular files, symbolic links and empty
/// directories (other directories are implied by what they contain).
pub const MAX_FILES: usize = 50_000;
/// Largest workspace file.
pub const MAX_FILE: usize = 64 * 1024 * 1024;
/// Largest workspace: the sum of its file sizes.
pub const MAX_TREE: usize = 2 * 1024 * 1024 * 1024;
/// Longest workspace path and path component (Linux NAME_MAX), in bytes.
pub const MAX_PATH: usize = 1024;
pub const MAX_NAME: usize = 255;
/// Longest symbolic link target, in bytes.
pub const MAX_LINK: usize = 4095;
/// Output kept per stream; a run that prints more is stopped.
pub const MAX_OUTPUT: usize = 8 * 1024 * 1024;
/// Longest single run. Hosts choose their own per-call timeout within this bound.
pub const MAX_RUN_SECS: u64 = 1800;
/// Longest lifetime of a background job.
pub const MAX_JOB_SECS: u64 = 24 * 60 * 60;
/// Newest output a job keeps (stdout and stderr interleaved); older bytes are counted
/// and dropped.
pub const JOB_OUTPUT: usize = 4 * 1024 * 1024;

/// A workspace path: any UTF-8 name Linux accepts (spaces, '@', '+', `.git`, ...),
/// relative, without empty, `.` or `..` components, at most MAX_NAME bytes per
/// component and MAX_PATH in all. On Windows each name must also be one the
/// filesystem keeps as written (`windows_name`).
pub fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH
        && !path.contains('\0')
        && path.split('/').all(|part| {
            !part.is_empty()
                && part.len() <= MAX_NAME
                && part != "."
                && part != ".."
                && (!cfg!(windows) || windows_name(part))
        })
}

/// A name Windows keeps as written: no `<>:"\|?*` or control characters (separators,
/// drive and stream syntax), no trailing dot or space (dropped on create), no device
/// name (`CON`, `nul.txt`, `COM1`, ...) and no `~` before a digit, which could be
/// another entry's short name.
pub(crate) fn windows_name(name: &str) -> bool {
    let base = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    let numbered = base.get(..3).is_some_and(|head| {
        (head.eq_ignore_ascii_case("COM") || head.eq_ignore_ascii_case("LPT"))
            && matches!(
                &base[3..],
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
    });
    !name
        .chars()
        .any(|c| c < ' ' || matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*'))
        && !name.ends_with(['.', ' '])
        && !numbered
        && !["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"]
            .iter()
            .any(|device| base.eq_ignore_ascii_case(device))
        && !name
            .as_bytes()
            .windows(2)
            .any(|pair| pair[0] == b'~' && pair[1].is_ascii_digit())
}
