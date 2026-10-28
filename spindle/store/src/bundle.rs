//! Session bundles: one file carrying a session directory's portable
//! files, so a session moves where no directory is shared (a browser's OPFS, memory,
//! another machine). Each store travels as one image (`Store::image`), with the
//! settings mirror (`session.json`) and each task's identity record (`task.json`).
//!
//! The format is Spindle's own: the magic `spindle-session\n`, a format number (u32
//! little-endian), a manifest's length (u32) and the manifest (JSON: format, schema,
//! the session's head and identity, and each file's path, size and SHA-256), the
//! files' bytes in manifest order, and the SHA-256 of everything before it. Reading
//! checks all of it, and every store as a full open would, before a host writes any
//! file; a bundle names only paths a session directory has.
use crate::{ImageFacts, check_image};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// How a bundle begins.
pub const MAGIC: &[u8; 16] = b"spindle-session\n";
/// The bundle format this build writes and reads.
pub const FORMAT: u32 = 1;
/// A session's own store, as a bundle names it.
pub const STORE: &str = "store/authority.sqlite";
const MAX_MANIFEST: usize = 1024 * 1024;
const MAX_FILES: usize = 4096;
const MAX_SIDECAR: usize = 4 * 1024 * 1024;
/// Deepest task nesting a bundle carries (a session's `tasks.max_depth` is at most 16).
const MAX_NESTING: usize = 16;

/// One file of a session directory, by its path inside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// A checked bundle: the session it moves and its files, the session's own store last
/// (so a host that writes them in order makes the session appear only once the rest is
/// in place).
#[derive(Debug)]
pub struct Bundle {
    pub head: u64,
    pub identity: String,
    pub files: Vec<BundleFile>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether `path` is one a session directory holds and a bundle may carry:
/// `(tasks/<key>/)*` (a key is 20 lowercase hex digits), then `store/authority.sqlite`,
/// `session.json`, or (in a task) `task.json`.
pub fn valid_path(path: &str) -> bool {
    let mut rest = path;
    let mut depth = 0;
    while let Some(after) = rest.strip_prefix("tasks/") {
        let Some((key, tail)) = after.split_once('/') else {
            return false;
        };
        if key.len() != 20 || !key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return false;
        }
        depth += 1;
        if depth > MAX_NESTING {
            return false;
        }
        rest = tail;
    }
    rest == STORE || rest == "session.json" || (depth > 0 && rest == "task.json")
}

/// The directory part of `path` (`""` for the session's own files, `tasks/<key>/...`).
fn place_of(path: &str) -> &str {
    let name = match path.strip_suffix(STORE) {
        Some(_) => STORE,
        None => path.rsplit('/').next().unwrap_or(path),
    };
    &path[..path.len() - name.len()]
}

/// Orders files so each place's store comes after its other files, deepest tasks
/// first and the session's own store last.
fn order(files: &mut [BundleFile]) {
    files.sort_by(|a, b| {
        let key = |path: &str| {
            let place = place_of(path);
            (
                std::cmp::Reverse(place.matches("tasks/").count()),
                place.to_string(),
                path.ends_with(STORE),
                path.to_string(),
            )
        };
        key(&a.path).cmp(&key(&b.path))
    });
}

/// Checks a sidecar's bytes: JSON (an object for a task's identity record).
fn check_sidecar(file: &BundleFile) -> Result<(), String> {
    if file.bytes.len() > MAX_SIDECAR {
        return Err(format!("{} is too large for a sidecar", file.path));
    }
    let value: Value = serde_json::from_slice(&file.bytes)
        .map_err(|_| format!("{} in the bundle is not JSON", file.path))?;
    if file.path.ends_with("task.json") && !value.is_object() {
        return Err(format!(
            "{} in the bundle is not a task's identity",
            file.path
        ));
    }
    Ok(())
}

/// Checks `files` (paths, sidecars, every store) and returns the session store's facts.
/// Each place's `session.json` is its store's own settings (the store is what every
/// host reads; the file only mirrors it): one is filled in, or set right, from them.
fn check_files(files: &mut Vec<BundleFile>) -> Result<ImageFacts, String> {
    if files.len() > MAX_FILES {
        return Err(format!("a bundle holds at most {MAX_FILES} files"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for file in files.iter() {
        if !valid_path(&file.path) {
            return Err(format!("`{}` is not a path a session holds", file.path));
        }
        if !seen.insert(file.path.clone()) {
            return Err(format!("`{}` is in the bundle twice", file.path));
        }
    }
    let mut main = None;
    let mut mirrors = Vec::new();
    for file in files.iter().filter(|file| file.path.ends_with(STORE)) {
        let facts = check_image(&file.bytes).map_err(|error| {
            format!("{} is not a store this build opens ({error:?})", file.path)
        })?;
        if let Some(settings) = &facts.settings {
            mirrors.push(BundleFile {
                path: format!("{}session.json", place_of(&file.path)),
                bytes: settings.clone(),
            });
        }
        if file.path == STORE {
            main = Some(facts);
        }
    }
    let main = main.ok_or("a bundle carries the session's own store")?;
    for mirror in mirrors {
        match files.iter_mut().find(|file| file.path == mirror.path) {
            Some(file) => file.bytes = mirror.bytes,
            None => files.push(mirror),
        }
    }
    for file in files.iter() {
        if file.path.ends_with(STORE) {
            continue;
        }
        check_sidecar(file)?;
        // A task's place holds its store: a sidecar alone moves no task.
        let place = place_of(&file.path);
        if !place.is_empty() && !seen.contains(&format!("{place}{STORE}")) {
            return Err(format!("{} has no store beside it", file.path));
        }
    }
    Ok(main)
}

/// A written bundle: its bytes, the session store's facts (head, identity) and how many
/// files it holds.
pub struct Written {
    pub bytes: Vec<u8>,
    pub facts: ImageFacts,
    pub files: usize,
}

/// Writes `files` (a session directory's portable files) as a bundle.
pub fn write(mut files: Vec<BundleFile>) -> Result<Written, String> {
    let main = check_files(&mut files)?;
    order(&mut files);
    let manifest = json!({
        "format": FORMAT,
        "schema": crate::CAPABILITIES.schema,
        "head": main.head,
        "identity": main.identity,
        "files": files.iter().map(|file| json!({
            "path": file.path,
            "size": file.bytes.len(),
            "sha256": hex(&Sha256::digest(&file.bytes)),
        })).collect::<Vec<_>>(),
    })
    .to_string();
    let size = MAGIC.len()
        + 8
        + manifest.len()
        + files.iter().map(|file| file.bytes.len()).sum::<usize>()
        + 32;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT.to_le_bytes());
    out.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
    out.extend_from_slice(manifest.as_bytes());
    for file in &files {
        out.extend_from_slice(&file.bytes);
    }
    let sum = Sha256::digest(&out);
    out.extend_from_slice(&sum);
    Ok(Written {
        bytes: out,
        facts: main,
        files: files.len(),
    })
}

/// Reads and checks a bundle: its framing and digests, its manifest, every path and
/// every store. Nothing is written anywhere.
pub fn read(bytes: &[u8]) -> Result<Bundle, String> {
    let refused = |why: &str| format!("not a session bundle this build reads: {why}");
    if bytes.len() < MAGIC.len() + 8 + 32 || !bytes.starts_with(MAGIC) {
        return Err(refused("no bundle header"));
    }
    let (body, sum) = bytes.split_at(bytes.len() - 32);
    if Sha256::digest(body).as_slice() != sum {
        return Err(refused(
            "its checksum does not match (damaged or cut short)",
        ));
    }
    let word = |at: usize| u32::from_le_bytes(body[at..at + 4].try_into().expect("4 bytes"));
    let format = word(MAGIC.len());
    if format != FORMAT {
        return Err(refused(&format!(
            "format {format} (this build reads {FORMAT})"
        )));
    }
    let length = word(MAGIC.len() + 4) as usize;
    let start = MAGIC.len() + 8;
    if length > MAX_MANIFEST || start + length > body.len() {
        return Err(refused("its manifest does not fit"));
    }
    let manifest: Value = serde_json::from_slice(&body[start..start + length])
        .map_err(|_| refused("its manifest is not JSON"))?;
    let schema = manifest["schema"].as_i64().unwrap_or(-1);
    if schema != i64::from(crate::CAPABILITIES.schema) {
        return Err(format!(
            "the bundle holds schema {schema} stores; this build reads schema {}",
            crate::CAPABILITIES.schema
        ));
    }
    let listed = manifest["files"]
        .as_array()
        .ok_or_else(|| refused("its manifest lists no files"))?;
    if listed.len() > MAX_FILES {
        return Err(refused("too many files"));
    }
    let mut at = start + length;
    let mut files = Vec::with_capacity(listed.len());
    for entry in listed {
        let (Some(path), Some(size), Some(digest)) = (
            entry["path"].as_str(),
            entry["size"]
                .as_u64()
                .and_then(|size| usize::try_from(size).ok()),
            entry["sha256"].as_str(),
        ) else {
            return Err(refused("a file entry is incomplete"));
        };
        let end = at
            .checked_add(size)
            .filter(|end| *end <= body.len())
            .ok_or_else(|| refused("a file runs past the end"))?;
        let bytes = body[at..end].to_vec();
        if hex(&Sha256::digest(&bytes)) != digest {
            return Err(refused(&format!("{path} does not match its digest")));
        }
        files.push(BundleFile {
            path: path.into(),
            bytes,
        });
        at = end;
    }
    if at != body.len() {
        return Err(refused("bytes follow its last file"));
    }
    let main = check_files(&mut files)?;
    if manifest["head"].as_u64() != Some(main.head)
        || manifest["identity"].as_str() != Some(main.identity.as_str())
    {
        return Err(refused("its manifest does not match the session's store"));
    }
    order(&mut files);
    Ok(Bundle {
        head: main.head,
        identity: main.identity,
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_session_directorys_own_paths_are_carried() {
        let key = "0123456789abcdef0123";
        for good in [
            STORE.to_string(),
            "session.json".into(),
            format!("tasks/{key}/{STORE}"),
            format!("tasks/{key}/task.json"),
            format!("tasks/{key}/tasks/{key}/session.json"),
        ] {
            assert!(valid_path(&good), "{good}");
        }
        for bad in [
            "task.json".to_string(),
            "store/authority.sqlite-wal".into(),
            "../store/authority.sqlite".into(),
            "/session.json".into(),
            "tasks/../session.json".into(),
            format!("tasks/{}/session.json", key.to_uppercase()),
            "tasks/0123/session.json".into(),
            format!("tasks/{key}/usage.json"),
            format!("{}{STORE}", format!("tasks/{key}/").repeat(17)),
        ] {
            assert!(!valid_path(&bad), "{bad}");
        }
    }

    #[test]
    fn files_are_ordered_deepest_first_with_each_store_after_its_sidecars() {
        let key = "0123456789abcdef0123";
        let mut files: Vec<BundleFile> = [
            STORE.to_string(),
            "session.json".into(),
            format!("tasks/{key}/{STORE}"),
            format!("tasks/{key}/task.json"),
            format!("tasks/{key}/session.json"),
        ]
        .into_iter()
        .map(|path| BundleFile {
            path,
            bytes: vec![],
        })
        .collect();
        order(&mut files);
        let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                format!("tasks/{key}/session.json").as_str(),
                &format!("tasks/{key}/task.json"),
                &format!("tasks/{key}/{STORE}"),
                "session.json",
                STORE,
            ]
        );
    }
}
