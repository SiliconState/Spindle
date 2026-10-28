//! Guided replacement of a spindle binary. A candidate must report its version and
//! pass its own `doctor` on copies of the given sessions before it is installed; the
//! replaced binary is kept beside the target so `rollback` can restore it. Every
//! replacement is a single rename, so a crash leaves either the old or the new binary.
// Candidate runs need native process-tree support off Unix; without it (a non-agent
// Windows build) the helpers of the contained run are compiled but unused.
#![cfg_attr(
    all(not(unix), not(all(windows, feature = "agent"))),
    allow(dead_code, unused_imports)
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, String>;

/// One candidate check: the session it ran against and the doctor's report.
pub struct Check {
    pub session: PathBuf,
    pub passed: bool,
    pub report: String,
}

/// The kept copy of the binary that an install replaced.
pub fn previous(target: &Path) -> PathBuf {
    sibling(target, "previous")
}

fn sibling(target: &Path, suffix: &str) -> PathBuf {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "spindle".into());
    target.with_file_name(format!(".{name}.{suffix}"))
}

/// Short content identity of a binary (first 12 hex digits of its SHA-256).
pub fn fingerprint(binary: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(binary).map_err(|error| format!("{}: {error}", binary.display()))?;
    Ok(Sha256::digest(bytes)
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Builds the spindle binary from a source checkout (its engine resolves through the
/// checkout's own path dependencies) and returns the built binary.
pub fn build(source: &Path) -> Result<PathBuf> {
    let manifest = source.join("store/Cargo.toml");
    if !manifest.is_file() {
        return Err(format!(
            "{} is not a spindle checkout (no store/Cargo.toml)",
            source.display()
        ));
    }
    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--locked",
            "-p",
            "spindle-store",
            "--features",
            "cli",
            "--bin",
            "spindle",
        ])
        .current_dir(source)
        .env_remove("CARGO_TARGET_DIR")
        .status()
        .map_err(|error| format!("cannot run cargo: {error}"))?;
    if !status.success() {
        return Err("the candidate did not build".into());
    }
    Ok(source
        .join("target/release")
        .join(format!("spindle{}", std::env::consts::EXE_SUFFIX)))
}

/// Runs `candidate version` and `candidate doctor` on a private copy of every
/// session, so a faulty candidate never touches real session state.
pub fn verify(candidate: &Path, sessions: &[PathBuf]) -> Result<(String, Vec<Check>)> {
    let (ok, version) = run(candidate, &["version".into()], Duration::from_secs(10))?;
    if !ok || !version.starts_with("spindle ") {
        return Err(format!(
            "{} does not identify as spindle: {}",
            candidate.display(),
            version.trim()
        ));
    }
    let scratch = Scratch(scratch_dir()?);
    let mut checks = vec![];
    let mut ready = Vec::new();
    let mut completed = Vec::new();
    for (index, session) in sessions.iter().enumerate() {
        let copy = scratch.0.join(format!("session-{index}"));
        match copy_session(session, &copy) {
            Ok(()) => {
                let mut pending = vec![(session.clone(), copy)];
                let mut count = 0usize;
                while let Some((session, copy)) = pending.pop() {
                    count += 1;
                    if count > 4096 {
                        return Err("candidate session tree exceeds 4096 stores".into());
                    }
                    for child in task_children(&copy)? {
                        let name = child.file_name().ok_or("task has no directory name")?;
                        pending.push((session.join("tasks").join(name), child.clone()));
                    }
                    ready.push((index, session, copy));
                }
            }
            Err(report) => completed.push((
                index,
                Check {
                    session: session.clone(),
                    passed: false,
                    report,
                },
            )),
        }
    }
    for (index, session, copy) in ready {
        let check = run(
            candidate,
            &[
                "--dir".into(),
                copy.to_string_lossy().into_owned(),
                "doctor".into(),
            ],
            Duration::from_secs(60),
        );
        completed.push((
            index,
            match check {
                Ok((passed, report)) => Check {
                    session,
                    passed,
                    report,
                },
                Err(report) => Check {
                    session,
                    passed: false,
                    report,
                },
            },
        ));
    }
    completed.sort_by_key(|(index, _)| *index);
    checks.extend(completed.into_iter().map(|(_, check)| check));
    Ok((version.trim().to_string(), checks))
}

/// Installs `candidate` as `target`, keeping the replaced binary as `previous(target)`.
/// On Unix the target is replaced by one rename. Windows cannot overwrite a running
/// executable but can rename it, so the current binary is first moved aside.
pub fn install(candidate: &Path, target: &Path) -> Result<()> {
    let staged = sibling(target, "new");
    copy_executable(candidate, &staged)?;
    if target.exists() {
        if cfg!(windows) {
            let _ = std::fs::remove_file(previous(target));
            rename(target, &previous(target))?;
            // The target is now missing; put the running binary back if the new one
            // cannot take its place, so a failed install never leaves no binary.
            return rename(&staged, target).inspect_err(|_| {
                let _ = std::fs::rename(previous(target), target);
            });
        }
        let kept = sibling(target, "previous-new");
        copy_executable(target, &kept)?;
        rename(&kept, &previous(target))?;
    }
    rename(&staged, target)
}

/// The schema a spindle binary reads, from its `version` line.
fn schema_of(binary: &Path) -> Option<u32> {
    let (true, version) = run(binary, &["version".into()], Duration::from_secs(10)).ok()? else {
        return None;
    };
    version
        .split_whitespace()
        .skip_while(|word| *word != "schema")
        .nth(1)
        .and_then(|number| number.parse::<u32>().ok())
}

/// Swaps `target` with the kept previous binary; running it twice undoes it. Stores
/// migrate forward on open, so a kept binary that reads an older schema than the one it
/// replaces (or does not say which) is refused unless `force`.
pub fn rollback(target: &Path, force: bool) -> Result<()> {
    let kept = previous(target);
    if !kept.is_file() {
        return Err(format!(
            "no previous binary is kept beside {}",
            target.display()
        ));
    }
    if !force {
        let current = schema_of(target).unwrap_or(crate::CAPABILITIES.schema);
        match schema_of(&kept) {
            None => {
                return Err(
                    "the kept binary did not report which schema it reads, so it may not open these sessions; `spindle rollback --force` restores it anyway".into(),
                );
            }
            Some(schema) if schema < current => {
                return Err(format!(
                    "the kept binary reads schema {schema}, but sessions opened by the current build are schema {current} and it could not open them. `spindle rollback --force` restores it anyway; then put back each migrated session's pre-migration copy, store/{}, as store/authority.sqlite",
                    crate::backup_name(i64::from(schema))
                ));
            }
            Some(_) => {}
        }
    }
    let current = sibling(target, "previous-new");
    if cfg!(windows) {
        let _ = std::fs::remove_file(&current);
        rename(target, &current)?;
        if let Err(error) = rename(&kept, target) {
            let _ = std::fs::rename(&current, target);
            return Err(error);
        }
    } else {
        copy_executable(target, &current)?;
        rename(&kept, target)?;
    }
    rename(&current, &kept)
}

fn rename(from: &Path, to: &Path) -> Result<()> {
    std::fs::rename(from, to)
        .map_err(|error| format!("cannot replace {}: {error}", to.display()))?;
    if let Some(parent) = to.parent() {
        #[cfg(unix)]
        let _ = std::fs::File::open(parent).and_then(|directory| directory.sync_all());
        #[cfg(not(unix))]
        let _ = parent;
    }
    Ok(())
}

fn copy_executable(from: &Path, to: &Path) -> Result<()> {
    use std::io::Write;
    let bytes = std::fs::read(from).map_err(|error| format!("{}: {error}", from.display()))?;
    let _ = std::fs::remove_file(to);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o755);
    }
    let mut file = options
        .open(to)
        .map_err(|error| format!("{}: {error}", to.display()))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("{}: {error}", to.display()))
}

/// Copies consistent authority snapshots and operational files, recursively for tasks.
fn copy_session(session: &Path, copy: &Path) -> Result<()> {
    let mut pending = vec![(session.to_path_buf(), copy.to_path_buf(), 0usize)];
    let mut count = 0usize;
    while let Some((source, target, depth)) = pending.pop() {
        count += 1;
        if count > 4096 || depth > 32 {
            return Err("candidate session tree exceeds 4096 stores or 32 task levels".into());
        }
        if depth > 0 {
            let path = source.join("task.json");
            let meta = std::fs::symlink_metadata(&path).map_err(|error| {
                format!("cannot verify task identity {}: {error}", path.display())
            })?;
            if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 128 * 1024 {
                return Err(format!(
                    "{} is not a bounded regular task identity",
                    path.display()
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.nlink() != 1 {
                    return Err(format!("{} is a linked task identity", path.display()));
                }
            }
            #[cfg(feature = "dext-turn")]
            {
                let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
                let input: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
                    format!("invalid task identity {}: {error}", path.display())
                })?;
                if !input.is_object()
                    || !input["prompt"]
                        .as_str()
                        .is_some_and(|text| !text.trim().is_empty() && text.len() <= 65536)
                {
                    return Err(format!("invalid task identity {}", path.display()));
                }
            }
        }
        copy_store(&source, &target)?;
        for child in task_children(&source)? {
            let name = child.file_name().ok_or("task has no directory name")?;
            pending.push((child.clone(), target.join("tasks").join(name), depth + 1));
        }
    }
    Ok(())
}

fn task_children(session: &Path) -> Result<Vec<PathBuf>> {
    let tasks = session.join("tasks");
    match std::fs::symlink_metadata(&tasks) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(format!(
                "{} is not a regular task directory",
                tasks.display()
            ));
        }
        Err(error) => return Err(format!("{}: {error}", tasks.display())),
    }
    let mut children = Vec::new();
    for entry in std::fs::read_dir(&tasks).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let meta = entry.file_type().map_err(|error| error.to_string())?;
        if !meta.is_dir() || meta.is_symlink() {
            return Err(format!(
                "{} is not a regular child session directory",
                entry.path().display()
            ));
        }
        if children.len() >= 4096 {
            return Err("candidate session tree exceeds 4096 stores".into());
        }
        children.push(entry.path());
    }
    children.sort();
    Ok(children)
}

fn copy_store(session: &Path, copy: &Path) -> Result<()> {
    if !session.join("store").is_dir() {
        return Err(format!("{} is not a spindle session", session.display()));
    }
    for path in [
        session.to_path_buf(),
        session.join("store"),
        session.join("store/authority.sqlite"),
    ] {
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let linked = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                meta.is_file() && meta.nlink() != 1
            }
            #[cfg(not(unix))]
            {
                false
            }
        };
        if linked || meta.file_type().is_symlink() || (!meta.is_file() && !meta.is_dir()) {
            return Err(format!("{} is not a regular session path", path.display()));
        }
    }
    private_dir(&copy.join("store"))?;
    for name in [
        "session.json",
        "pending-settings.json",
        "output-limits.json",
        "usage.json",
        "task.json",
    ] {
        let path = session.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {
                std::fs::copy(&path, copy.join(name)).map_err(|error| error.to_string())?;
            }
            Ok(_) => return Err(format!("{} is not a regular session file", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    // A raw database/WAL copy races checkpoints and can lose acknowledged metadata.
    let source = rusqlite::Connection::open_with_flags(
        session.join("store/authority.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("cannot snapshot {}: {error}", session.display()))?;
    source
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| format!("cannot snapshot {}: {error}", session.display()))?;
    source
        .query_row("SELECT count(*) FROM sqlite_schema", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| format!("cannot snapshot {}: {error}", session.display()))?;
    // Online backup preserves rowids used by receipt validation/accounting watermarks.
    source
        .backup(rusqlite::MAIN_DB, copy.join("store/authority.sqlite"), None)
        .map_err(|error| format!("cannot snapshot {}: {error}", session.display()))?;
    std::fs::File::create(copy.join("store/owner.lock")).map_err(|error| error.to_string())?;
    Ok(())
}

/// A directory only this user can read; the store refuses others (on Unix).
fn private_dir(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))
}

fn scratch_dir() -> Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("spindle-upgrade-{}-{stamp}", std::process::id()));
    private_dir(&dir)?;
    Ok(dir)
}

/// Without native process-tree support (a non-Unix build without `agent`) no
/// candidate can be contained, so none runs.
#[cfg(all(not(unix), not(all(windows, feature = "agent"))))]
fn run(binary: &Path, _args: &[String], _timeout: Duration) -> Result<(bool, String)> {
    Err(format!(
        "candidate execution needs native process-tree support on this platform ({})",
        binary.display()
    ))
}

/// Candidate output is polled under the same deadline as the process, including
/// after root exit. No reader thread can outlive a timeout or hold an unbounded reply.
#[cfg(any(unix, all(windows, feature = "agent")))]
fn run(binary: &Path, args: &[String], timeout: Duration) -> Result<(bool, String)> {
    const OUTPUT_LIMIT: usize = 1024 * 1024;
    let deadline = Instant::now() + timeout;
    let mut command = Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(all(windows, feature = "agent"))]
    dext_native::process_tree::configure_std_process_group(&mut command);
    // A candidate just written can be "busy" for a moment: a thread forking meanwhile
    // holds its write descriptor until that child execs.
    let mut tries = 0;
    let child = loop {
        match command.spawn() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 50 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            spawned => {
                break spawned
                    .map_err(|error| format!("cannot run {}: {error}", binary.display()))?;
            }
        }
    };
    let mut child = CandidateChild { child };
    #[cfg(all(windows, feature = "agent"))]
    let tree = dext_native::process_tree::ChildProcessTree::for_std(&child.child)
        .map_err(|error| format!("cannot contain candidate: {error}"))?;
    let mut stdout = child.child.stdout.take().ok_or("candidate has no stdout")?;
    let mut stderr = child.child.stderr.take().ok_or("candidate has no stderr")?;
    let mut output = [Vec::new(), Vec::new()];
    let mut eof = [false; 2];
    let mut status = None;
    let mut buffer = [0; 8192];
    loop {
        if Instant::now() >= deadline {
            return Err(format!("{} did not finish in time", binary.display()));
        }
        for index in 0..2 {
            if eof[index] {
                continue;
            }
            let read = if index == 0 {
                stdout.poll_read(&mut buffer)
            } else {
                stderr.poll_read(&mut buffer)
            }
            .map_err(|error| format!("cannot read candidate output: {error}"))?;
            if let Some(count) = read {
                eof[index] = count == 0;
                if output[0].len() + output[1].len() + count > OUTPUT_LIMIT {
                    return Err(format!(
                        "{} exceeded the 1 MiB output limit",
                        binary.display()
                    ));
                }
                output[index].extend_from_slice(&buffer[..count]);
            }
        }
        if status.is_none() {
            status = child
                .child
                .try_wait()
                .map_err(|error| format!("cannot wait for {}: {error}", binary.display()))?;
            if status.is_some() {
                child.stop_tree();
                #[cfg(all(windows, feature = "agent"))]
                tree.terminate_after_root_exit();
            }
        }
        if let Some(status) = status
            && eof.iter().all(|done| *done)
        {
            let stderr = std::mem::take(&mut output[1]);
            output[0].extend_from_slice(&stderr);
            return Ok((
                status.success(),
                String::from_utf8_lossy(&output[0]).into_owned(),
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

trait CandidatePipe: std::io::Read {
    fn poll_read(&mut self, buffer: &mut [u8]) -> std::io::Result<Option<usize>>;
}

#[cfg(unix)]
impl<T: std::io::Read + std::os::fd::AsRawFd> CandidatePipe for T {
    fn poll_read(&mut self, buffer: &mut [u8]) -> std::io::Result<Option<usize>> {
        let fd = self.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        match self.read(buffer) {
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            result => result.map(Some),
        }
    }
}

#[cfg(windows)]
impl<T: std::io::Read + std::os::windows::io::AsRawHandle> CandidatePipe for T {
    fn poll_read(&mut self, buffer: &mut [u8]) -> std::io::Result<Option<usize>> {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn PeekNamedPipe(
                handle: *mut std::ffi::c_void,
                buffer: *mut std::ffi::c_void,
                size: u32,
                read: *mut u32,
                available: *mut u32,
                left: *mut u32,
            ) -> i32;
        }
        let mut available = 0;
        if unsafe {
            PeekNamedPipe(
                self.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        } == 0
        {
            let error = std::io::Error::last_os_error();
            return if error.raw_os_error() == Some(109) {
                Ok(Some(0))
            } else {
                Err(error)
            };
        }
        let count = buffer.len().min(available as usize);
        if count == 0 {
            Ok(None)
        } else {
            self.read(&mut buffer[..count]).map(Some)
        }
    }
}

#[cfg(not(any(unix, windows)))]
impl<T: std::io::Read> CandidatePipe for T {
    fn poll_read(&mut self, _: &mut [u8]) -> std::io::Result<Option<usize>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "candidate pipes unsupported",
        ))
    }
}

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct CandidateChild {
    child: std::process::Child,
}
impl CandidateChild {
    fn stop_tree(&self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL);
        }
    }
}
impl Drop for CandidateChild {
    fn drop(&mut self) {
        self.stop_tree();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "spindle-upgrade-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn candidate_copy_keeps_wal_metadata_and_legacy_operational_sidecars() {
        let dir = temp("copy");
        let source = dir.join("source");
        private_dir(&source).unwrap();
        let mut owner = crate::Store::open(&source.join("store")).unwrap();
        owner
            .set_metadata("settings", b"metadata settings")
            .unwrap();
        let epoch = owner.epoch();
        for name in [
            "session.json",
            "pending-settings.json",
            "output-limits.json",
            "usage.json",
        ] {
            std::fs::write(source.join(name), name).unwrap();
        }
        let copy = dir.join("copy");
        copy_session(&source, &copy).unwrap();
        assert_eq!(owner.epoch(), epoch);
        for name in [
            "session.json",
            "pending-settings.json",
            "output-limits.json",
            "usage.json",
        ] {
            assert_eq!(std::fs::read(copy.join(name)).unwrap(), name.as_bytes());
        }
        let mut candidate = crate::Store::open(&copy.join("store")).unwrap();
        assert_eq!(
            candidate.metadata("settings").unwrap().unwrap(),
            b"metadata settings"
        );
        candidate.check_integrity().unwrap();
        drop(candidate);
        drop(owner);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn candidate_copy_preserves_rowid_watermarks() {
        let dir = temp("rowids");
        let source = dir.join("source");
        private_dir(&source.join("store")).unwrap();
        let db = rusqlite::Connection::open(source.join("store/authority.sqlite")).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE receipts(attempt TEXT PRIMARY KEY,result BLOB); INSERT INTO receipts(rowid,attempt,result) VALUES(7,'first',x'01'),(42,'second',x'02');").unwrap();
        let copy = dir.join("copy");
        copy_session(&source, &copy).unwrap();
        let db_copy = rusqlite::Connection::open(copy.join("store/authority.sqlite")).unwrap();
        let ids = db_copy
            .prepare("SELECT rowid FROM receipts ORDER BY rowid")
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(ids, [7, 42]);
        drop(db_copy);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn candidate_deadline_output_bound_and_descendant_cleanup() {
        let shell = Path::new("/bin/sh");
        let before = Instant::now();
        let result = run(
            shell,
            &["-c".into(), "sleep 10 & echo spindle test; exit 0".into()],
            Duration::from_millis(300),
        )
        .unwrap();
        assert!(result.0);
        assert!(result.1.starts_with("spindle test"));
        assert!(before.elapsed() < Duration::from_secs(2));
        let before = Instant::now();
        let error = run(
            shell,
            &["-c".into(), "sleep 10".into()],
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(error.contains("did not finish"), "{error}");
        assert!(before.elapsed() < Duration::from_secs(2));
        let error = run(
            shell,
            &["-c".into(), "head -c 2097152 /dev/zero".into()],
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(error.contains("output limit"), "{error}");
    }

    #[test]
    fn candidate_copy_includes_descendants_and_identity_without_touching_owners() {
        let dir = temp("descendants");
        let source = dir.join("source");
        let child = source.join("tasks/one");
        let grandchild = child.join("tasks/two");
        let owners: Vec<_> = [&source, &child, &grandchild]
            .iter()
            .map(|path| {
                private_dir(path).unwrap();
                std::fs::write(path.join("task.json"), b"{\"prompt\":\"test\"}").unwrap();
                let mut owner = crate::Store::open(&path.join("store")).unwrap();
                owner
                    .set_metadata("settings", path.to_string_lossy().as_bytes())
                    .unwrap();
                owner
            })
            .collect();
        let copy = dir.join("copy");
        copy_session(&source, &copy).unwrap();
        for (original, relative) in [
            (&source, ""),
            (&child, "tasks/one"),
            (&grandchild, "tasks/one/tasks/two"),
        ] {
            let target = copy.join(relative);
            assert_eq!(
                std::fs::read(target.join("task.json")).unwrap(),
                b"{\"prompt\":\"test\"}"
            );
            let candidate = crate::Store::open(&target.join("store")).unwrap();
            assert_eq!(
                candidate.metadata("settings").unwrap().unwrap(),
                original.to_string_lossy().as_bytes()
            );
        }
        #[cfg(unix)]
        {
            let linked = dir.join("identity-link");
            std::fs::hard_link(child.join("task.json"), &linked).unwrap();
            assert!(
                copy_session(&source, &dir.join("linked-copy"))
                    .unwrap_err()
                    .contains("linked task identity")
            );
            std::fs::remove_file(linked).unwrap();
            std::os::unix::fs::symlink(&child, source.join("tasks/link")).unwrap();
            assert!(
                copy_session(&source, &dir.join("bad-copy"))
                    .unwrap_err()
                    .contains("regular child")
            );
        }
        drop(owners);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn candidate_doctor_runs_for_parent_and_every_descendant() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("recursive-doctor");
        let source = dir.join("source");
        let child = source.join("tasks/child");
        for path in [&source, &child] {
            private_dir(path).unwrap();
            std::fs::write(path.join("task.json"), b"{\"prompt\":\"test\"}").unwrap();
            let _owner = crate::Store::open(&path.join("store")).unwrap();
        }
        let binary = dir.join("candidate");
        std::fs::write(&binary, "#!/bin/sh\nif [ \"$1\" = version ]; then echo 'spindle test'; exit 0; fi\ncase \"$2\" in */tasks/child) echo child-failed; exit 1;; *) echo parent-passed;; esac\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (_, checks) = verify(&binary, std::slice::from_ref(&source)).unwrap();
        assert_eq!(checks.len(), 2);
        assert!(
            checks
                .iter()
                .any(|check| check.session == source && check.passed)
        );
        assert!(
            checks
                .iter()
                .any(|check| check.session == child && !check.passed)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn install_keeps_the_previous_binary_and_rollback_swaps_back() {
        let dir = temp("swap");
        let target = dir.join("spindle");
        let candidate = dir.join("candidate");
        std::fs::write(&target, "old").unwrap();
        std::fs::write(&candidate, "new").unwrap();
        install(&candidate, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(std::fs::read(previous(&target)).unwrap(), b"old");
        // These stand-ins cannot report a schema, so only a forced rollback swaps them.
        assert!(rollback(&target, false).is_err());
        rollback(&target, true).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert_eq!(std::fs::read(previous(&target)).unwrap(), b"new");
        rollback(&target, true).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 3, "no staging files remain: {names:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rollback_refuses_a_binary_that_reads_an_older_schema() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("older");
        let target = dir.join("spindle");
        std::fs::write(&target, "current").unwrap();
        let kept = previous(&target);
        std::fs::write(
            &kept,
            "#!/bin/sh\necho 'spindle 0.1.0 schema 14 engine dext-turn-v1 build old'\n",
        )
        .unwrap();
        std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = rollback(&target, false).unwrap_err();
        assert!(error.contains("schema 14"), "{error}");
        assert_eq!(std::fs::read(&target).unwrap(), b"current");
        rollback(&target, true).unwrap();
        assert_eq!(std::fs::read(previous(&target)).unwrap(), b"current");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn verify_refuses_a_binary_that_is_not_spindle_and_reports_failing_sessions() {
        let dir = temp("verify");
        let fake = dir.join("fake");
        std::fs::write(&fake, "#!/bin/sh\necho hello\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(verify(&fake, &[]).is_err());
        std::fs::write(
            &fake,
            "#!/bin/sh\ncase \"$1\" in version) echo 'spindle 9 schema 14';; *) echo broken; exit 1;; esac\n",
        )
        .unwrap();
        let session = dir.join("session");
        std::fs::create_dir_all(session.join("store")).unwrap();
        std::fs::write(session.join("store/authority.sqlite"), "x").unwrap();
        let (version, checks) = verify(&fake, &[session.clone(), dir.join("missing")]).unwrap();
        assert_eq!(version, "spindle 9 schema 14");
        assert!(!checks[0].passed, "{}", checks[0].report);
        assert!(!checks[1].passed && checks[1].report.contains("not a spindle session"));
        assert_eq!(
            std::fs::read(session.join("store/authority.sqlite")).unwrap(),
            b"x"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
