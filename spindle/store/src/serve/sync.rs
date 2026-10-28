//! Self-edit threads and the host source. A self-edit thread imports Spindle's source
//! when it is created; the host source moves on (other promotions, the operator's own
//! edits). Promotion three-way merges the thread's changes onto the current source,
//! against the source the thread last synced with: the import (r1) at first, and after
//! an **Update from host source** the host source that update merged in.
//!
//! A sync is recorded as the pure host-source snapshot, kept beside the thread in
//! `host-sync/ROOT`, where ROOT is the workspace root of the merged tree (the root the
//! update's turn starts from). It is written durably *before* that turn commits, so a
//! crash leaves at worst a record no revision uses (never found), never a committed
//! merge without its record. A promotion takes the newest record on the head's line
//! (rewinds followed; `View::line`): a workspace whose content is the merge of a
//! snapshot S already holds every change S made, so S is a correct base for it and for
//! everything the thread does after it. A rewind to before an update leaves the
//! update's revisions off the line, so the older base applies again; a retried update
//! turn starts from the same root and finds the same record. Choosing an older base
//! than the true one can only add conflicts (refusals), never revert the host.
use super::{Result, SourceFile, err, private_dir};
use sha2::{Digest, Sha256};
use spindle_store::view::View;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Sync records beside a thread's store: `host-sync/ROOT`.
const DIR: &str = "host-sync";
const MAGIC: &[u8] = b"spindle-host-sync-v1\0";
/// Paths listed in an update's prompt per kind; the rest are counted.
const LISTED: usize = 60;

/// Whether a path is git repository metadata (`.git`, or anything under a `.git`
/// directory). The host's repository is the host's: `git status` there rewrites its
/// index, and a model's `git diff` rewrites the thread's copy, so it is never merged,
/// recorded or promoted as a thread's change; the host's current one always wins.
pub(super) fn repository(path: &str) -> bool {
    path.split('/').any(|part| part == ".git")
}

fn valid_root(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// MAGIC, the payload's SHA-256, then the deflated payload: a count, then per file its
/// path length, path, executable bit, length and bytes (little-endian).
fn encode(files: &[SourceFile]) -> Vec<u8> {
    let mut payload = (files.len() as u32).to_le_bytes().to_vec();
    for (path, bytes, executable) in files {
        payload.extend_from_slice(&(path.len() as u32).to_le_bytes());
        payload.extend_from_slice(path.as_bytes());
        payload.push(u8::from(*executable));
        payload.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        payload.extend_from_slice(bytes);
    }
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&Sha256::digest(&payload));
    out.extend(miniz_oxide::deflate::compress_to_vec(&payload, 6));
    out
}

fn decode(bytes: &[u8]) -> Result<Vec<SourceFile>> {
    let damaged = || "damaged sync record".to_string();
    let rest = bytes.strip_prefix(MAGIC).ok_or_else(damaged)?;
    if rest.len() < 32 {
        return Err(damaged());
    }
    let (digest, packed) = rest.split_at(32);
    // A workspace holds at most 2 GiB of files; the framing adds little beside them.
    let limit = dext_native::strict::MAX_TREE + 64 * 1024 * 1024;
    let payload =
        miniz_oxide::inflate::decompress_to_vec_with_limit(packed, limit).map_err(|_| damaged())?;
    if Sha256::digest(&payload).as_slice() != digest {
        return Err(damaged());
    }
    let mut reader = Reader {
        bytes: &payload,
        at: 0,
    };
    let count = reader.number(4)? as usize;
    let mut files: Vec<SourceFile> = Vec::with_capacity(count.min(65_536));
    for _ in 0..count {
        let length = reader.number(4)? as usize;
        let path = std::str::from_utf8(reader.take(length)?)
            .map_err(|_| damaged())?
            .to_string();
        let executable = match reader.take(1)?[0] {
            0 => false,
            1 => true,
            _ => return Err(damaged()),
        };
        let size = usize::try_from(reader.number(8)?).map_err(|_| damaged())?;
        let bytes = reader.take(size)?.to_vec();
        if !spindle_store::workspace::valid_path(&path)
            || files.last().is_some_and(|last| last.0 >= path)
        {
            return Err(damaged());
        }
        files.push((path, bytes, executable));
    }
    if reader.at != payload.len() {
        return Err(damaged());
    }
    Ok(files)
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or("damaged sync record")?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }
    /// A little-endian number of `width` bytes.
    fn number(&mut self, width: usize) -> Result<u64> {
        Ok(self
            .take(width)?
            .iter()
            .rev()
            .fold(0u64, |n, b| n << 8 | u64::from(*b)))
    }
}

/// Records that the workspace root `root` holds a merge of `snapshot` (the pure host
/// source, in path order). Durable before it returns: call it before anything commits
/// a revision with that root.
pub(super) fn record(thread: &Path, root: &str, snapshot: &[SourceFile]) -> Result<()> {
    if !valid_root(root) {
        return Err("invalid workspace root".into());
    }
    let dir = thread.join(DIR);
    private_dir(&dir)?;
    let mut sorted: Vec<SourceFile> = snapshot
        .iter()
        .filter(|file| !repository(&file.0))
        .cloned()
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    spindle_store::durable::write(&dir.join(root), &encode(&sorted)).map_err(err)
}

/// The newest sync on `head`'s line: the revision whose workspace it was recorded for,
/// and the host-source snapshot. None: the thread never synced (its base is the import
/// at r1). A damaged record is passed over (an older base only adds conflicts), with a
/// warning.
pub(super) fn find(
    thread: &Path,
    head: u64,
    mut warn: impl FnMut(String),
) -> Result<Option<(u64, Vec<SourceFile>)>> {
    let dir = thread.join(DIR);
    let mut names = recorded(thread)?;
    while !names.is_empty() {
        let Some((revision, root)) = newest(thread, head, &names)? else {
            return Ok(None);
        };
        match std::fs::read(dir.join(&root))
            .map_err(err)
            .and_then(|bytes| decode(&bytes))
        {
            Ok(files) => return Ok(Some((revision, files))),
            Err(error) => {
                warn(format!(
                    "The sync record of r{revision} is unreadable ({error}); using an older base."
                ));
                names.remove(&root);
            }
        }
    }
    Ok(None)
}

/// The roots with a sync record beside a thread.
fn recorded(thread: &Path) -> Result<BTreeSet<String>> {
    match std::fs::read_dir(thread.join(DIR)) {
        Ok(entries) => Ok(entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str().map(String::from))
            .filter(|name| valid_root(name))
            .collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(error) => Err(err(error)),
    }
}

/// The newest revision on `head`'s line whose root is one of `names`, with that root.
fn newest(thread: &Path, head: u64, names: &BTreeSet<String>) -> Result<Option<(u64, String)>> {
    if names.is_empty() {
        return Ok(None);
    }
    let mut view = View::open(&thread.join("store")).map_err(err)?;
    let mut found = None;
    view.read(|view| {
        view.line(head, |revision, root| match root {
            Some(root) if names.contains(root) => {
                found = Some((revision, root.to_string()));
                true
            }
            _ => false,
        })
    })
    .map_err(err)?;
    Ok(found)
}

/// The snapshot recorded for `root` (one `latest` found).
pub(super) fn load(thread: &Path, root: &str) -> Result<Vec<SourceFile>> {
    if !valid_root(root) {
        return Err("invalid workspace root".into());
    }
    decode(&std::fs::read(thread.join(DIR).join(root)).map_err(err)?)
}

/// As `find`, without reading the record: the revision and root of the newest sync on
/// `head`'s line, if any.
pub(super) fn latest(thread: &Path, head: u64) -> Result<Option<(u64, String)>> {
    newest(thread, head, &recorded(thread)?)
}

/// A three-way text merge of one file.
pub(super) enum Merged {
    Clean(Vec<u8>),
    /// Overlapping changes, written with diff3 conflict markers.
    Conflicted(Vec<u8>),
    /// Not UTF-8 text (or holds NUL bytes): no line merge.
    NotText,
}

/// `git merge-file` of `ours` (the host source) and `theirs` (this thread) against
/// `base` (the last sync). With `diff3` the markers show the base lines too (for the
/// model); git then merges less eagerly, so a promotion, which only needs to know
/// whether a merge is clean, uses the default style.
pub(super) fn merge_file(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    scratch: &Path,
    diff3: bool,
) -> Result<Merged> {
    if [base, ours, theirs]
        .iter()
        .any(|bytes| std::str::from_utf8(bytes).is_err() || bytes.contains(&0))
    {
        return Ok(Merged::NotText);
    }
    let dir = scratch.join("merge");
    private_dir(&dir)?;
    let names = ["ours", "base", "theirs"];
    for (name, bytes) in names.iter().zip([ours, base, theirs]) {
        std::fs::write(dir.join(name), bytes).map_err(err)?;
    }
    let output = std::process::Command::new("git")
        .args(["merge-file", "-p"])
        .args(diff3.then_some("--diff3"))
        .args([
            "-L",
            "host source",
            "-L",
            "last sync",
            "-L",
            "this thread",
            "--",
        ])
        .args(names.iter().map(|name| dir.join(name)))
        .output()
        .map_err(|error| format!("git merge-file unavailable: {error}"))?;
    let _ = std::fs::remove_dir_all(&dir);
    match output.status.code() {
        Some(0) => Ok(Merged::Clean(output.stdout)),
        // The exit status counts the conflicts (at most 127); above that is an error.
        Some(1..=127) => Ok(Merged::Conflicted(output.stdout)),
        _ => Err(format!(
            "git merge-file failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// The host source merged into a thread's workspace (`merge`).
#[derive(Debug, Default)]
pub(super) struct Update {
    /// The merged workspace files, in path order.
    pub files: Vec<SourceFile>,
    /// Paths that took the host source's change (edited, added or removed there).
    pub from_host: Vec<String>,
    /// Paths both sides changed that merged cleanly.
    pub merged: Vec<String>,
    /// Paths both sides changed in overlapping lines: written with conflict markers.
    pub conflicts: Vec<String>,
    /// Paths that could not be merged line by line, and what the merge kept.
    pub decisions: Vec<(String, &'static str)>,
    /// The thread's git metadata differed from the host's and was replaced by it.
    pub repository: bool,
}

impl Update {
    /// Whether the merge changed nothing in the thread's files.
    pub fn unchanged(&self, theirs: &[SourceFile]) -> bool {
        same_files(&self.files, theirs)
    }
}

/// Whether two file lists hold the same files, in whatever order, apart from git
/// metadata (`repository`).
pub(super) fn same_files(a: &[SourceFile], b: &[SourceFile]) -> bool {
    fn index(files: &[SourceFile]) -> BTreeMap<&str, (&[u8], bool)> {
        files
            .iter()
            .filter(|file| !repository(&file.0))
            .map(|(path, bytes, executable)| (path.as_str(), (bytes.as_slice(), *executable)))
            .collect()
    }
    index(a) == index(b)
}

/// Merges the current host source (`ours`) into a thread's files (`theirs`) against
/// the source the thread last synced with (`base`). A file only one side changed takes
/// that side; text both changed is merged, and overlapping changes are written with
/// conflict markers for the model to resolve. What cannot be merged line by line is
/// decided so the model can see both: a binary file both changed keeps the thread's
/// version, a file the host deleted but the thread changed is kept, and a file the
/// thread deleted but the host changed comes back with the host's version. Git
/// metadata is the host's (`repository`).
pub(super) fn merge(
    base: &[SourceFile],
    ours: &[SourceFile],
    theirs: &[SourceFile],
    scratch: &Path,
) -> Result<Update> {
    let index = |files: &[SourceFile]| {
        files
            .iter()
            .map(|(path, bytes, executable)| (path.clone(), (bytes.clone(), *executable)))
            .collect::<BTreeMap<_, _>>()
    };
    let (base, ours, theirs) = (index(base), index(ours), index(theirs));
    let paths: BTreeSet<&String> = base
        .keys()
        .chain(ours.keys())
        .chain(theirs.keys())
        .collect();
    let mut update = Update::default();
    for path in paths {
        let (b, o, t) = (base.get(path), ours.get(path), theirs.get(path));
        let chosen = if repository(path) {
            update.repository |= o != t;
            o.cloned()
        } else if o == t {
            o.cloned()
        } else if t == b {
            update.from_host.push(path.clone());
            o.cloned()
        } else if o == b {
            t.cloned()
        } else {
            match (b, o, t) {
                (_, Some(o), Some(t)) => {
                    let empty = (vec![], false);
                    let b = b.unwrap_or(&empty);
                    // An executable bit only one side changed follows that side.
                    let executable = if o.1 == b.1 { t.1 } else { o.1 };
                    match merge_file(&b.0, &o.0, &t.0, scratch, true)? {
                        Merged::Clean(bytes) => {
                            update.merged.push(path.clone());
                            Some((bytes, executable))
                        }
                        Merged::Conflicted(bytes) => {
                            update.conflicts.push(path.clone());
                            Some((bytes, executable))
                        }
                        Merged::NotText => {
                            update.decisions.push((
                                path.clone(),
                                "not text and changed on both sides: kept this thread's version; the host's change is not in it",
                            ));
                            Some(t.clone())
                        }
                    }
                }
                (_, None, Some(t)) => {
                    update.decisions.push((
                        path.clone(),
                        "the host source deleted it and this thread changed it: kept this thread's version; delete it if the host's removal should stand",
                    ));
                    Some(t.clone())
                }
                (_, Some(o), None) => {
                    update.decisions.push((
                        path.clone(),
                        "this thread deleted it and the host source changed it: restored the host's version; delete it again if this thread's removal should stand",
                    ));
                    Some(o.clone())
                }
                (_, None, None) => None,
            }
        };
        if let Some((bytes, executable)) = chosen {
            update.files.push((path.clone(), bytes, executable));
        }
    }
    Ok(update)
}

fn listing(paths: impl ExactSizeIterator<Item = String>) -> String {
    let total = paths.len();
    let mut text: String = paths
        .take(LISTED)
        .map(|path| format!("\n- {path}"))
        .collect();
    if total > LISTED {
        text.push_str(&format!("\n- … and {} more", total - LISTED));
    }
    text
}

/// The prompt of an update's turn: what changed, and what the model must resolve.
/// `prefix` is where the thread keeps the source ("" or "spindle/"); `tool` words it
/// as a `spindle_source` result rather than a turn's prompt.
pub(super) fn prompt(update: &Update, since: &str, prefix: &str, tool: bool) -> String {
    let at = |path: &String| format!("{prefix}{path}");
    let mut text = format!(
        "{} The host's Spindle source changed since this thread last \
         synced ({since}). Spindle merged the current host source into {}: {} \
         file(s) took the host's version",
        if tool {
            "[spindle_source update]"
        } else {
            "[Update from host source]"
        },
        if prefix.is_empty() {
            "this workspace".to_string()
        } else {
            format!("{prefix} in this workspace")
        },
        update.from_host.len()
    );
    if !update.merged.is_empty() {
        text.push_str(&format!(
            ", {} merged cleanly with this thread's changes",
            update.merged.len()
        ));
    }
    text.push('.');
    if !update.from_host.is_empty() {
        text.push_str("\n\nChanged by the host:");
        text.push_str(&listing(update.from_host.iter().map(at)));
    }
    if !update.conflicts.is_empty() {
        text.push_str(
            "\n\nConflicts to resolve. Both sides changed the same lines; each file below \
             holds markers:\n<<<<<<< host source\n(the host's lines)\n||||||| last sync\n\
             (the lines both started from)\n=======\n(this thread's lines)\n>>>>>>> this thread",
        );
        text.push_str(&listing(update.conflicts.iter().map(at)));
    }
    if update.repository {
        text.push_str(
            "\n\nThe workspace's .git is now the host's repository metadata; this thread's \
             own .git changes were not kept (its files were).",
        );
    }
    if !update.decisions.is_empty() {
        text.push_str("\n\nNot mergeable line by line; check each:");
        text.push_str(&listing(
            update
                .decisions
                .iter()
                .map(|(path, why)| format!("{prefix}{path}: {why}")),
        ));
    }
    text.push_str(
        if update.conflicts.is_empty() && update.decisions.is_empty() {
            "\n\nNothing conflicted. Check that the workspace still builds and its tests pass \
         (cargo fmt, cargo clippy --all-targets --all-features, cargo test), fix anything \
         the merge broke without undoing the host's changes, and reply with a short summary."
        } else {
            "\n\nResolve every conflict so that both the host's upgrades and this thread's own \
         work survive (never drop the host's changes), remove every marker (searching for \
         `<<<<<<<` must find nothing), then check that it builds and its tests pass (cargo \
         fmt, cargo clippy --all-targets --all-features, cargo test) and reply with a short \
         summary of how you resolved each file."
        },
    );
    text.push_str(
        " Nothing is installed until you call spindle_install and the user taps Install on \
         the Spindle change card in the chat (or types /install).",
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, text: &str) -> SourceFile {
        (path.to_string(), text.as_bytes().to_vec(), false)
    }
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("spindle-sync-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        private_dir(&dir).unwrap();
        dir
    }
    fn text<'a>(update: &'a Update, path: &str) -> Option<&'a str> {
        update
            .files
            .iter()
            .find(|f| f.0 == path)
            .map(|f| std::str::from_utf8(&f.1).unwrap())
    }

    #[test]
    fn records_round_trip_and_damage_is_detected() {
        let files = vec![
            file("a", "one\n"),
            ("bin/run".into(), vec![0, 1, 2, 255], true),
            file("empty", ""),
        ];
        assert_eq!(decode(&encode(&files)).unwrap(), files);
        let mut damaged = encode(&files);
        let last = damaged.len() - 1;
        damaged[last] ^= 1;
        assert!(decode(&damaged).is_err());
        assert!(decode(b"spindle-host-sync-v1\0short").is_err());
        assert!(decode(b"something else").is_err());
        // Unsorted, duplicate or invalid paths are refused even with a good digest.
        for bad in [
            vec![file("b", ""), file("a", "")],
            vec![file("a", ""), file("a", "")],
            vec![file("../x", "")],
        ] {
            assert!(decode(&encode(&bad)).is_err());
        }
        assert!(record(&scratch("bad-root"), "../escape", &files).is_err());
    }

    #[test]
    fn a_merge_keeps_both_sides_and_marks_overlaps() {
        let dir = scratch("merge");
        let base = vec![
            file("a", "1\n2\n3\n"),
            file("b", "b\n"),
            file("c", "c\n"),
            file("d", "d\n"),
            file("gone-host", "x\n"),
            file("gone-thread", "y\n"),
            file("same", "s\n"),
            file("z", "1\n2\n3\n4\n5\n"),
        ];
        let ours = vec![
            file("a", "1\n2\nhost\n"),
            file("b", "b host\n"),
            file("c", "c\n"),
            file("d", "d\n"),
            file("gone-thread", "y host\n"),
            file("new-host", "n\n"),
            file("same", "s\n"),
            file("z", "host\n2\n3\n4\n5\n"),
        ];
        let theirs = vec![
            file("a", "1\n2\nthread\n"),
            file("b", "b\n"),
            file("c", "c thread\n"),
            file("gone-host", "x thread\n"),
            file("new-thread", "t\n"),
            file("same", "s\n"),
            file("z", "1\n2\n3\n4\nthread\n"),
        ];
        let update = merge(&base, &ours, &theirs, &dir).unwrap();
        assert_eq!(text(&update, "b"), Some("b host\n"), "host-only change");
        assert_eq!(text(&update, "c"), Some("c thread\n"), "thread-only change");
        assert_eq!(text(&update, "d"), None, "the thread deleted d");
        assert_eq!(text(&update, "new-host"), Some("n\n"));
        assert_eq!(text(&update, "new-thread"), Some("t\n"));
        assert_eq!(
            text(&update, "z"),
            Some("host\n2\n3\n4\nthread\n"),
            "clean merge"
        );
        assert_eq!(update.merged, ["z"]);
        assert_eq!(update.conflicts, ["a"]);
        let a = text(&update, "a").unwrap();
        for marker in [
            "<<<<<<< host source\nhost\n",
            "||||||| last sync\n3\n",
            "=======\nthread\n>>>>>>> this thread\n",
        ] {
            assert!(a.contains(marker), "{a}");
        }
        assert_eq!(text(&update, "gone-host"), Some("x thread\n"));
        assert_eq!(text(&update, "gone-thread"), Some("y host\n"));
        assert_eq!(
            update
                .decisions
                .iter()
                .map(|d| d.0.as_str())
                .collect::<Vec<_>>(),
            ["gone-host", "gone-thread"]
        );
        assert_eq!(update.from_host, ["b", "new-host"]);
        assert!(!update.unchanged(&theirs));
        let text = prompt(&update, "the import at r1", "", false);
        for needed in [
            "- a",
            "<<<<<<< host source",
            "gone-host:",
            "Spindle change card",
        ] {
            assert!(text.contains(needed), "{text}");
        }
        // Nothing moved on the host: the thread's files come back as they were.
        let same = merge(&base, &base, &theirs, &dir).unwrap();
        assert!(same.unchanged(&theirs));
        assert!(same.conflicts.is_empty() && same.from_host.is_empty());
        let binary = merge(
            &[("p".into(), vec![0, 1], false)],
            &[("p".into(), vec![0, 2], false)],
            &[("p".into(), vec![0, 3], false)],
            &dir,
        )
        .unwrap();
        assert_eq!(binary.files[0].1, [0, 3]);
        assert_eq!(binary.decisions.len(), 1);
        // Git metadata both rewrote (index refreshes) is the host's, never a conflict.
        let git = |index: u8| ("repo/.git/index".to_string(), vec![0, index], false);
        let metadata = merge(
            &[git(1), file("x", "x\n")],
            &[git(2), file("x", "x\n")],
            &[
                git(3),
                file(".git", "gitdir: elsewhere\n"),
                file("x", "x\n"),
            ],
            &dir,
        )
        .unwrap();
        assert_eq!(metadata.files, [git(2), file("x", "x\n")]);
        assert!(
            metadata.repository && metadata.decisions.is_empty() && metadata.conflicts.is_empty()
        );
        assert!(
            metadata.unchanged(&[git(3), file("x", "x\n")]),
            "only metadata differs"
        );
        assert!(prompt(&metadata, "r1", "", false).contains(".git is now the host's"));
        let scoped = prompt(&update, "r3", "spindle/", true);
        assert!(
            scoped.starts_with("[spindle_source update]")
                && scoped.contains("into spindle/ in this workspace")
                && scoped.contains("\n- spindle/a\n")
                && scoped.contains("spindle/gone-host:")
                && scoped.contains("taps Install on the Spindle change card"),
            "{scoped}"
        );
        let thread = scratch("record");
        record(&thread, &"a".repeat(64), &[git(2), file("x", "x\n")]).unwrap();
        let kept = decode(&std::fs::read(thread.join(DIR).join("a".repeat(64))).unwrap()).unwrap();
        assert_eq!(kept, [file("x", "x\n")], "records leave git metadata out");
        let _ = std::fs::remove_dir_all(thread);
        let _ = std::fs::remove_dir_all(dir);
    }
}
