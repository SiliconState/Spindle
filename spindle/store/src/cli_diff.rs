//! `spindle diff`: what changed in a session's workspace between two revisions, as a
//! patch `git apply` takes (and, for text files, `patch -p1`). Read from committed
//! revisions without the session's owner lock, so it works while a turn runs.
use serde_json::Value;
use spindle_store::api;
use std::collections::BTreeMap;
use std::path::Path;

/// Lines of unchanged context around each change.
const CONTEXT: usize = 3;
/// Edit distance past which a file is shown replaced whole rather than line by line
/// (the search's memory grows with its square).
const MAX_EDITS: usize = 2000;

/// One side of a path: its git mode and what identifies its content.
#[derive(Clone, PartialEq, Eq)]
struct Entry {
    mode: &'static str,
    /// A file's digest, or a link's target.
    key: String,
    link: bool,
}

/// What `diff` found: the patch, and paths left out of it with why.
pub(crate) struct Patch {
    pub text: String,
    pub skipped: Vec<(String, &'static str)>,
    pub stats: Vec<(String, Stat)>,
}

#[derive(Default, Clone)]
pub(crate) struct Stat {
    pub added: usize,
    pub removed: usize,
    pub binary: bool,
    /// `old new` when only the mode changed (or as well).
    pub mode: Option<String>,
}

/// The workspace's files and links at `revision` (the head when None), by path.
fn entries(dir: &Path, revision: Option<u64>) -> Result<(u64, BTreeMap<String, Entry>), String> {
    let form = api::files_at(dir, revision)?;
    let revision = form["revision"].as_u64().unwrap_or_default();
    let mut map = BTreeMap::new();
    for file in form["files"].as_array().into_iter().flatten() {
        let (Some(path), Some(digest)) = (file["path"].as_str(), file["digest"].as_str()) else {
            continue;
        };
        let mode = if file["executable"] == true {
            "100755"
        } else {
            "100644"
        };
        map.insert(
            path.to_string(),
            Entry {
                mode,
                key: digest.to_string(),
                link: false,
            },
        );
    }
    for link in form["links"].as_array().into_iter().flatten() {
        let (Some(path), Some(target)) = (link["path"].as_str(), link["target"].as_str()) else {
            continue;
        };
        map.insert(
            path.to_string(),
            Entry {
                mode: "120000",
                key: target.to_string(),
                link: true,
            },
        );
    }
    Ok((revision, map))
}

/// The bytes of `path` as `entry` holds it at `revision`: a link's target, or the file.
fn content(dir: &Path, revision: u64, path: &str, entry: &Entry) -> Result<Vec<u8>, String> {
    match entry.link {
        true => Ok(entry.key.clone().into_bytes()),
        false => api::read_at(dir, Some(revision), path),
    }
}

/// The patch from revision `from` to `to` (the head when None). Git metadata (`.git/`)
/// is left out (applying it would rewrite a repository), and so are binary files and
/// paths a patch cannot name; `skipped` says which.
pub(crate) fn diff(dir: &Path, from: u64, to: Option<u64>) -> Result<(u64, u64, Patch), String> {
    let (from, old) = entries(dir, Some(from))?;
    let (to, new) = entries(dir, to)?;
    let mut paths: Vec<&String> = old.keys().chain(new.keys()).collect();
    paths.sort_unstable();
    paths.dedup();
    let mut patch = Patch {
        text: String::new(),
        skipped: vec![],
        stats: vec![],
    };
    for path in paths {
        let (before, after) = (old.get(path), new.get(path));
        if before == after {
            continue;
        }
        if path == ".git" || path.starts_with(".git/") {
            if !patch.skipped.iter().any(|(skipped, _)| skipped == ".git/") {
                patch.skipped.push((".git/".into(), "git metadata"));
            }
            continue;
        }
        if path
            .chars()
            .any(|c| c.is_control() || c == '"' || c == '\\')
        {
            patch
                .skipped
                .push((path.clone(), "a name a patch cannot hold"));
            continue;
        }
        let old_bytes = match before {
            Some(entry) => Some(content(dir, from, path, entry)?),
            None => None,
        };
        let new_bytes = match after {
            Some(entry) => Some(content(dir, to, path, entry)?),
            None => None,
        };
        let text = |bytes: &Option<Vec<u8>>| {
            bytes
                .as_deref()
                .is_none_or(|bytes| !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok())
        };
        if !text(&old_bytes) || !text(&new_bytes) {
            patch.skipped.push((path.clone(), "binary"));
            patch.stats.push((
                path.clone(),
                Stat {
                    binary: true,
                    ..Stat::default()
                },
            ));
            continue;
        }
        let (old_bytes, new_bytes) = (
            old_bytes.as_deref().unwrap_or_default(),
            new_bytes.as_deref().unwrap_or_default(),
        );
        let stat = match (before, after) {
            // A link that became a file, or the other way: git applies such a change
            // only as a deletion and a creation, as it writes one itself.
            (Some(before), Some(after)) if before.link != after.link => {
                let gone = file_patch(
                    &mut patch.text,
                    path,
                    Some(before.mode),
                    None,
                    old_bytes,
                    b"",
                );
                let made = file_patch(
                    &mut patch.text,
                    path,
                    None,
                    Some(after.mode),
                    b"",
                    new_bytes,
                );
                Stat {
                    added: made.added,
                    removed: gone.removed,
                    binary: false,
                    mode: Some(format!("{} -> {}", before.mode, after.mode)),
                }
            }
            _ => file_patch(
                &mut patch.text,
                path,
                before.map(|entry| entry.mode),
                after.map(|entry| entry.mode),
                old_bytes,
                new_bytes,
            ),
        };
        patch.stats.push((path.clone(), stat));
    }
    Ok((from, to, patch))
}

/// Appends one path's part of a git patch to `out` and counts its lines.
fn file_patch(
    out: &mut String,
    path: &str,
    old_mode: Option<&str>,
    new_mode: Option<&str>,
    old: &[u8],
    new: &[u8],
) -> Stat {
    out.push_str(&format!("diff --git a/{path} b/{path}\n"));
    let mode = match (old_mode, new_mode) {
        (Some(before), Some(after)) if before != after => Some(format!("{before} -> {after}")),
        _ => None,
    };
    match (old_mode, new_mode) {
        (None, Some(mode)) => out.push_str(&format!("new file mode {mode}\n")),
        (Some(mode), None) => out.push_str(&format!("deleted file mode {mode}\n")),
        (Some(before), Some(after)) if before != after => {
            out.push_str(&format!("old mode {before}\nnew mode {after}\n"));
        }
        _ => {}
    }
    if old == new {
        return Stat {
            mode,
            ..Stat::default()
        };
    }
    let a = lines(old);
    let b = lines(new);
    out.push_str(&match old_mode {
        Some(_) => format!("--- a/{path}\n"),
        None => "--- /dev/null\n".into(),
    });
    out.push_str(&match new_mode {
        Some(_) => format!("+++ b/{path}\n"),
        None => "+++ /dev/null\n".into(),
    });
    let script = edits(&a, &b);
    let mut stat = Stat {
        mode,
        ..Stat::default()
    };
    for op in &script {
        match op {
            Op::Delete => stat.removed += 1,
            Op::Insert => stat.added += 1,
            Op::Equal => {}
        }
    }
    hunks(out, &a, &b, &script);
    stat
}

/// Lines with their endings; the last may lack one.
fn lines(bytes: &[u8]) -> Vec<&[u8]> {
    bytes.split_inclusive(|byte| *byte == b'\n').collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Equal,
    Delete,
    Insert,
}

/// A shortest edit script from `a` to `b` (Myers), after their common start and end;
/// one past `MAX_EDITS` replaces the middle whole.
fn edits(a: &[&[u8]], b: &[&[u8]]) -> Vec<Op> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (middle_a, middle_b) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut script = vec![Op::Equal; prefix];
    match myers(middle_a, middle_b) {
        Some(middle) => script.extend(middle),
        None => {
            script.extend(std::iter::repeat_n(Op::Delete, middle_a.len()));
            script.extend(std::iter::repeat_n(Op::Insert, middle_b.len()));
        }
    }
    script.extend(std::iter::repeat_n(Op::Equal, suffix));
    script
}

/// Myers' O(ND) search, keeping each step's diagonals to walk back along.
fn myers(a: &[&[u8]], b: &[&[u8]]) -> Option<Vec<Op>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (n + m) as usize;
    let offset = max as isize + 1;
    let mut v = vec![0isize; 2 * max + 3];
    // trace[d]: the furthest x on diagonals -(d-1)..=(d-1) before step d.
    let mut trace: Vec<Vec<isize>> = vec![];
    for d in 0..=max {
        if d > MAX_EDITS {
            return None;
        }
        let d = d as isize;
        trace.push(match d {
            0 => vec![],
            _ => v[(offset - d + 1) as usize..(offset + d) as usize].to_vec(),
        });
        let mut k = -d;
        while k <= d {
            let at = |k: isize| (offset + k) as usize;
            let mut x = if k == -d || (k != d && v[at(k - 1)] < v[at(k + 1)]) {
                v[at(k + 1)]
            } else {
                v[at(k - 1)] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[at(k)] = x;
            if x >= n && y >= m {
                return Some(walk_back(&trace, n, m));
            }
            k += 2;
        }
    }
    None
}

fn walk_back(trace: &[Vec<isize>], n: isize, m: isize) -> Vec<Op> {
    let mut ops = vec![];
    let (mut x, mut y) = (n, m);
    for (d, v) in trace.iter().enumerate().rev() {
        let d = d as isize;
        // v holds diagonals -(d-1)..=(d-1).
        let get = |k: isize| v[(k + d - 1) as usize];
        let k = x - y;
        let previous = if k == -d || (k != d && get(k - 1) < get(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let (px, py) = if d == 0 {
            (0, 0)
        } else {
            let px = get(previous);
            (px, px - previous)
        };
        while x > px && y > py {
            ops.push(Op::Equal);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            ops.push(if x == px { Op::Insert } else { Op::Delete });
        }
        (x, y) = (px, py);
    }
    ops.reverse();
    ops
}

/// The script's changes as unified hunks with `CONTEXT` lines around them.
fn hunks(out: &mut String, a: &[&[u8]], b: &[&[u8]], script: &[Op]) {
    // Where each op stands in a and b.
    let mut at = Vec::with_capacity(script.len());
    let (mut i, mut j) = (0, 0);
    for op in script {
        at.push((i, j));
        match op {
            Op::Equal => (i, j) = (i + 1, j + 1),
            Op::Delete => i += 1,
            Op::Insert => j += 1,
        }
    }
    let changes: Vec<usize> = (0..script.len())
        .filter(|&index| script[index] != Op::Equal)
        .collect();
    let mut index = 0;
    while index < changes.len() {
        let first = changes[index];
        let mut last = first;
        while index + 1 < changes.len() && changes[index + 1] - last <= 2 * CONTEXT + 1 {
            index += 1;
            last = changes[index];
        }
        index += 1;
        let start = first.saturating_sub(CONTEXT);
        let end = (last + CONTEXT + 1).min(script.len());
        let (a_start, b_start) = at[start];
        let ops = &script[start..end];
        let a_len = ops.iter().filter(|op| **op != Op::Insert).count();
        let b_len = ops.iter().filter(|op| **op != Op::Delete).count();
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            range(a_start, a_len),
            range(b_start, b_len)
        ));
        for (offset, op) in ops.iter().enumerate() {
            let (i, j) = at[start + offset];
            let (mark, line) = match op {
                Op::Equal => (' ', a[i]),
                Op::Delete => ('-', a[i]),
                Op::Insert => ('+', b[j]),
            };
            out.push(mark);
            out.push_str(&String::from_utf8_lossy(line));
            if !line.ends_with(b"\n") {
                out.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
}

/// A hunk's range as git writes it: `start,len`, the start one-based (the line before
/// for an empty range), `,1` left out.
fn range(start: usize, len: usize) -> String {
    match len {
        0 => format!("{start},0"),
        1 => format!("{}", start + 1),
        _ => format!("{},{len}", start + 1),
    }
}

/// `--stat`: each changed path with its added and removed lines, then the totals.
pub(crate) fn stat_lines(stats: &[(String, Stat)]) -> String {
    let width = stats
        .iter()
        .map(|(path, _)| path.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    let (mut added, mut removed) = (0, 0);
    for (path, stat) in stats {
        added += stat.added;
        removed += stat.removed;
        let mut counts = match stat.binary {
            true => "binary".to_string(),
            false => format!("+{} -{}", stat.added, stat.removed),
        };
        if let Some(mode) = &stat.mode {
            counts.push_str(&format!(" (mode {mode})"));
        }
        out.push_str(&format!(" {path:<width$} | {counts}\n"));
    }
    out.push_str(&format!(
        " {} file(s) changed, {added} insertion(s)(+), {removed} deletion(s)(-)\n",
        stats.len()
    ));
    out
}

/// The patch as JSON: each changed path with its counts, and the patch itself.
pub(crate) fn json(from: u64, to: u64, patch: &Patch) -> Value {
    serde_json::json!({
        "from": from,
        "to": to,
        "files": patch.stats.iter().map(|(path, stat)| serde_json::json!({
            "path": path, "added": stat.added, "removed": stat.removed, "binary": stat.binary,
            "mode": stat.mode,
        })).collect::<Vec<_>>(),
        "skipped": patch.skipped.iter().map(|(path, why)| serde_json::json!({"path": path, "why": why})).collect::<Vec<_>>(),
        "patch": patch.text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(old: &str, new: &str) -> String {
        let mut out = String::new();
        file_patch(
            &mut out,
            "f.txt",
            Some("100644"),
            Some("100644"),
            old.as_bytes(),
            new.as_bytes(),
        );
        out
    }

    /// Applies `patch`'s hunks to `old` as `patch -p1` would, to check them.
    fn apply(old: &str, patch: &str) -> String {
        let old: Vec<&str> = old.split_inclusive('\n').collect();
        let mut out = String::new();
        let (mut next, mut in_hunk, mut emitted) = (0, false, false);
        for line in patch.lines() {
            if let Some(header) = line.strip_prefix("@@ -") {
                let range = header.split(' ').next().unwrap();
                let mut parts = range.split(',');
                let start: usize = parts.next().unwrap().parse().unwrap();
                let len: usize = parts.next().map_or(1, |len| len.parse().unwrap());
                let from = if len == 0 { start } else { start - 1 };
                for kept in &old[next..from] {
                    out.push_str(kept);
                }
                (next, in_hunk) = (from, true);
                continue;
            }
            if !in_hunk {
                continue;
            }
            if line == "\\ No newline at end of file" {
                if emitted {
                    out.pop();
                }
                continue;
            }
            let (mark, text) = line.split_at(1);
            match mark {
                " " | "+" => {
                    out.push_str(text);
                    out.push('\n');
                    next += usize::from(mark == " ");
                    emitted = true;
                }
                _ => {
                    next += 1;
                    emitted = false;
                }
            }
        }
        for kept in &old[next..] {
            out.push_str(kept);
        }
        out
    }

    #[test]
    fn a_patch_applied_gives_the_new_text() {
        let old: String = (1..=40).map(|n| format!("line {n}\n")).collect();
        let cases = [
            old.replace("line 5\n", "line five\n"),
            old.replace("line 5\n", "")
                .replace("line 30\n", "line 30\nextra\n"),
            format!("top\n{old}"),
            format!("{old}bottom"),
            old.replace("line 1\n", "").replace("line 40\n", ""),
            String::new(),
            "only\n".into(),
        ];
        for new in cases {
            let patch = patch(&old, &new);
            assert_eq!(apply(&old, &patch), new, "{patch}");
        }
        assert_eq!(apply("", &patch("", "a\nb\n")), "a\nb\n");
        assert!(patch("a\nb", "a\nc").contains("\\ No newline at end of file"));
    }

    #[test]
    fn hunks_are_git_shaped() {
        let old: String = (1..=20).map(|n| format!("{n}\n")).collect();
        let new = old.replace("10\n", "ten\n");
        let patch = patch(&old, &new);
        assert!(patch.starts_with("diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n"));
        assert!(
            patch.contains("@@ -7,7 +7,7 @@\n 7\n 8\n 9\n-10\n+ten\n 11\n"),
            "{patch}"
        );
        let mut created = String::new();
        file_patch(&mut created, "n.txt", None, Some("100755"), b"", b"x\n");
        assert_eq!(
            created,
            "diff --git a/n.txt b/n.txt\nnew file mode 100755\n--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1 @@\n+x\n"
        );
    }

    /// What `git apply` makes of patches for an empty file made and removed, a link
    /// that became a file, and a mode change (where git is installed).
    #[cfg(unix)]
    #[test]
    fn git_applies_the_patches_of_every_kind_of_change() {
        let work = std::env::temp_dir().join(format!("spindle-diff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&work)
                .output()
        };
        if git(&["init", "-q", "."]).is_err() {
            return;
        }
        std::fs::write(work.join("gone.txt"), b"").unwrap();
        std::fs::write(work.join("run.sh"), b"echo hi\n").unwrap();
        std::os::unix::fs::symlink("run.sh", work.join("link")).unwrap();
        let mut patch = String::new();
        file_patch(&mut patch, "made.txt", None, Some("100644"), b"", b"");
        file_patch(&mut patch, "gone.txt", Some("100644"), None, b"", b"");
        file_patch(
            &mut patch,
            "run.sh",
            Some("100644"),
            Some("100755"),
            b"echo hi\n",
            b"echo hi\n",
        );
        file_patch(&mut patch, "link", Some("120000"), None, b"run.sh", b"");
        file_patch(
            &mut patch,
            "link",
            None,
            Some("100644"),
            b"",
            b"now a file\n",
        );
        std::fs::write(work.join("p.patch"), &patch).unwrap();
        let applied = git(&["apply", "p.patch"]).unwrap();
        assert!(
            applied.status.success(),
            "{}\n{patch}",
            String::from_utf8_lossy(&applied.stderr)
        );
        assert!(work.join("made.txt").is_file() && !work.join("gone.txt").exists());
        assert_eq!(std::fs::read(work.join("link")).unwrap(), b"now a file\n");
        assert!(
            !std::fs::symlink_metadata(work.join("link"))
                .unwrap()
                .is_symlink()
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(work.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111);
        let _ = std::fs::remove_dir_all(work);
    }

    #[test]
    fn a_shortest_script_is_found() {
        let a: Vec<&[u8]> = ["a", "b", "c", "a", "b", "b", "a"]
            .map(str::as_bytes)
            .to_vec();
        let b: Vec<&[u8]> = ["c", "b", "a", "b", "a", "c"].map(str::as_bytes).to_vec();
        let script = myers(&a, &b).unwrap();
        let changes = script.iter().filter(|op| **op != Op::Equal).count();
        assert_eq!(changes, 5);
    }
}
