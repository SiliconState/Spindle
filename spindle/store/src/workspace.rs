//! Explicit workspace tree, not a host-filesystem snapshot or sandbox: regular files
//! (bytes and an executable bit, stored as objects), symbolic links (targets kept as
//! text, never followed by the store) and empty directories. Other directories are
//! implicit; owners, ACLs and timestamps are not kept.
//!
//! Manifest v1 (at most 256 regular files with portable ASCII paths) is still written
//! whenever a tree fits it, so existing roots keep their digests and older builds can
//! read them; any other tree is written as v2. Both decode canonically.

use crate::artifact::{Stage, object_digest, valid_digest};
use crate::{Error, Result, Store};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// Entries a tree may hold: files, links and empty directories.
pub const MAX_FILES: usize = 50_000;
/// Workspace byte bounds shared by native and embedded tool hosts.
pub const MAX_FILE: usize = 64 * 1024 * 1024;
pub const MAX_TREE: usize = 2 * 1024 * 1024 * 1024;
/// Longest path and path component (bytes), and longest link target.
pub const MAX_PATH: usize = 1024;
pub const MAX_NAME: usize = 255;
pub const MAX_LINK: usize = 4095;
const PREFIX: &[u8] = b"spindle-file-map-v1\0";
const PREFIX_V2: &[u8] = b"spindle-file-map-v2\0";
const V1_FILES: usize = 256;
const V1_PATH: usize = 240;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub path: String,
    pub object: String,
    pub executable: bool,
}

/// A symbolic link; its target is stored as written and never resolved by the store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Link {
    pub path: String,
    pub target: String,
}

/// A whole workspace tree.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Tree {
    pub files: Vec<Entry>,
    pub links: Vec<Link>,
    /// Directories with no entries.
    pub directories: Vec<String>,
}

impl Tree {
    pub fn len(&self) -> usize {
        self.files.len() + self.links.len() + self.directories.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A workspace path: any UTF-8 name Linux accepts (spaces, '@', '+', `.git`, ...),
/// relative, without empty, `.` or `..` components, at most MAX_NAME bytes per
/// component and MAX_PATH in all.
pub fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH
        && !path.contains('\0')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part.len() <= MAX_NAME && part != "." && part != "..")
}

/// Profile v1 paths: portable ASCII names, no Git metadata or Windows device names.
fn legacy_path(path: &str) -> bool {
    if path.is_empty() || path.len() > V1_PATH {
        return false;
    }
    path.split('/').all(|part| {
        !part.is_empty()
            && part.len() <= 100
            && part != "."
            && part != ".."
            && !part.ends_with('.')
            && !part.eq_ignore_ascii_case(".git")
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            && !matches!(
                part.split('.')
                    .next()
                    .unwrap_or("")
                    .to_ascii_uppercase()
                    .as_str(),
                "CON"
                    | "PRN"
                    | "AUX"
                    | "NUL"
                    | "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
            )
    })
}

/// Profile v1 also refuses paths that differ only by case.
fn legacy_files(entries: &[Entry]) -> bool {
    if entries.len() > V1_FILES {
        return false;
    }
    let mut files = BTreeSet::new();
    let mut directories = BTreeMap::new();
    for entry in entries {
        if !legacy_path(&entry.path) {
            return false;
        }
        let path = entry.path.to_ascii_lowercase();
        if !files.insert(path.clone()) || directories.contains_key(&path) {
            return false;
        }
        for (offset, _) in path.match_indices('/') {
            let directory = &path[..offset];
            if files.contains(directory) {
                return false;
            }
            let spelling = &entry.path[..offset];
            if directories
                .get(directory)
                .is_some_and(|old| old != spelling)
            {
                return false;
            }
            directories.insert(directory.to_owned(), spelling.to_owned());
        }
    }
    true
}

/// Valid unique paths, valid digests and link targets, no entry inside a file, link
/// or (necessarily empty) directory entry, and at most MAX_FILES entries.
pub(crate) fn validate_tree(tree: &Tree) -> Result<()> {
    if tree.len() > MAX_FILES {
        return Err(Error::Invalid);
    }
    let mut paths = HashSet::with_capacity(tree.len());
    let files = tree
        .files
        .iter()
        .map(|entry| (&entry.path, valid_digest(&entry.object)));
    let links = tree.links.iter().map(|link| {
        (
            &link.path,
            !link.target.is_empty() && link.target.len() <= MAX_LINK && !link.target.contains('\0'),
        )
    });
    let directories = tree.directories.iter().map(|path| (path, true));
    for (path, valid) in files.chain(links).chain(directories) {
        if !valid || !valid_path(path) || !paths.insert(path.as_str()) {
            return Err(Error::Invalid);
        }
    }
    for path in &paths {
        if path
            .match_indices('/')
            .any(|(offset, _)| paths.contains(&path[..offset]))
        {
            return Err(Error::Invalid);
        }
    }
    Ok(())
}

/// The root digest a store gives `tree` when it is staged (its manifest's object
/// digest), computed without a store.
pub fn tree_digest(tree: &Tree) -> Result<String> {
    Ok(object_digest(&encode_tree(tree)?))
}

/// Canonical manifest bytes; at most one object (`artifact::MAX_OBJECT`).
pub(crate) fn encode_tree(tree: &Tree) -> Result<Vec<u8>> {
    validate_tree(tree)?;
    let mut files = tree.files.clone();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let mut bytes;
    if tree.links.is_empty() && tree.directories.is_empty() && legacy_files(&files) {
        bytes = PREFIX.to_vec();
        bytes.extend_from_slice(&(files.len() as u32).to_be_bytes());
        for entry in files {
            bytes.extend_from_slice(&(entry.path.len() as u16).to_be_bytes());
            bytes.extend_from_slice(entry.path.as_bytes());
            bytes.extend_from_slice(entry.object.as_bytes());
            bytes.push(u8::from(entry.executable));
        }
        return Ok(bytes);
    }
    enum Kind<'a> {
        File(&'a str, bool),
        Link(&'a str),
        Directory,
    }
    let mut all: Vec<(&str, Kind<'_>)> = files
        .iter()
        .map(|entry| {
            (
                entry.path.as_str(),
                Kind::File(&entry.object, entry.executable),
            )
        })
        .chain(
            tree.links
                .iter()
                .map(|link| (link.path.as_str(), Kind::Link(&link.target))),
        )
        .chain(
            tree.directories
                .iter()
                .map(|path| (path.as_str(), Kind::Directory)),
        )
        .collect();
    all.sort_by(|a, b| a.0.cmp(b.0));
    bytes = PREFIX_V2.to_vec();
    bytes.extend_from_slice(&(all.len() as u32).to_be_bytes());
    for (path, kind) in all {
        bytes.extend_from_slice(&(path.len() as u16).to_be_bytes());
        bytes.extend_from_slice(path.as_bytes());
        match kind {
            Kind::File(object, executable) => {
                bytes.push(u8::from(executable));
                bytes.extend_from_slice(object.as_bytes());
            }
            Kind::Link(target) => {
                bytes.push(2);
                bytes.extend_from_slice(&(target.len() as u16).to_be_bytes());
                bytes.extend_from_slice(target.as_bytes());
            }
            Kind::Directory => bytes.push(3),
        }
        if bytes.len() > crate::artifact::MAX_OBJECT {
            return Err(Error::Invalid);
        }
    }
    Ok(bytes)
}

fn take<'a>(bytes: &mut &'a [u8], count: usize) -> Result<&'a [u8]> {
    if bytes.len() < count {
        return Err(Error::Incompatible);
    }
    let (value, rest) = bytes.split_at(count);
    *bytes = rest;
    Ok(value)
}

fn take_text(bytes: &mut &[u8], limit: usize) -> Result<String> {
    let length = u16::from_be_bytes(
        take(bytes, 2)?
            .try_into()
            .map_err(|_| Error::Incompatible)?,
    ) as usize;
    if length > limit {
        return Err(Error::Incompatible);
    }
    std::str::from_utf8(take(bytes, length)?)
        .map(str::to_owned)
        .map_err(|_| Error::Incompatible)
}

/// A whole manifest; only its one canonical encoding is accepted.
pub fn decode_tree(bytes: &[u8]) -> Result<Tree> {
    if bytes.len() > crate::artifact::MAX_OBJECT {
        return Err(Error::Incompatible);
    }
    let mut remaining = bytes;
    let v2 = match take(&mut remaining, PREFIX.len())? {
        prefix if prefix == PREFIX => false,
        prefix if prefix == PREFIX_V2 => true,
        _ => return Err(Error::Incompatible),
    };
    let count = u32::from_be_bytes(
        take(&mut remaining, 4)?
            .try_into()
            .map_err(|_| Error::Incompatible)?,
    ) as usize;
    if count > if v2 { MAX_FILES } else { V1_FILES } {
        return Err(Error::Incompatible);
    }
    let digest = |remaining: &mut &[u8]| -> Result<String> {
        std::str::from_utf8(take(remaining, 64)?)
            .map(str::to_owned)
            .map_err(|_| Error::Incompatible)
    };
    let mut tree = Tree::default();
    for _ in 0..count {
        let path = take_text(&mut remaining, if v2 { MAX_PATH } else { V1_PATH })?;
        if !v2 {
            let object = digest(&mut remaining)?;
            let executable = match take(&mut remaining, 1)?[0] {
                0 => false,
                1 => true,
                _ => return Err(Error::Incompatible),
            };
            tree.files.push(Entry {
                path,
                object,
                executable,
            });
            continue;
        }
        match take(&mut remaining, 1)?[0] {
            kind @ (0 | 1) => tree.files.push(Entry {
                path,
                object: digest(&mut remaining)?,
                executable: kind == 1,
            }),
            2 => tree.links.push(Link {
                path,
                target: take_text(&mut remaining, MAX_LINK)?,
            }),
            3 => tree.directories.push(path),
            _ => return Err(Error::Incompatible),
        }
    }
    if !remaining.is_empty() || encode_tree(&tree).map_err(|_| Error::Incompatible)? != bytes {
        return Err(Error::Incompatible);
    }
    Ok(tree)
}

/// Digests pinned by any stage of `scope`, in one pass over its pins.
pub(crate) fn scope_pins(db: &Connection, scope: &str) -> Result<HashSet<String>> {
    let mut query = db.prepare_cached(
        "SELECT p.digest FROM staging_pins p JOIN artifact_stages s ON s.id=p.stage WHERE s.scope=?1",
    )?;
    let digests = query.query_map([scope], |row| crate::bounded_label(row, 0))?;
    Ok(digests.collect::<rusqlite::Result<_>>()?)
}

/// Roots already proven complete on this connection. Objects and pins are immutable
/// until garbage collection, which clears this memo (verified object digests stay
/// true: an object is its digest).
#[cfg(feature = "workspace-tools")]
pub(crate) fn forget_validated_roots(db: &Connection) -> Result<()> {
    db.execute_batch("DROP TABLE IF EXISTS temp.spindle_valid_roots")?;
    Ok(())
}

pub(crate) fn validate_root(db: &Connection, scope: &str, root: &str) -> Result<Vec<Entry>> {
    validate_tree_root(db, scope, root).map(|tree| tree.files)
}

/// The tree of a workspace root: its manifest and every file object are pinned in
/// `scope` and match their digests. Each object is hashed once per connection (its
/// digest is then remembered), so a step only hashes the objects it newly reaches.
/// A connection starts out knowing the files of the last validated revision's
/// workspace (`known_files`), so a reopen hashes (and inflates) only the files written
/// since, never the whole workspace again.
pub(crate) fn validate_tree_root(db: &Connection, scope: &str, root: &str) -> Result<Tree> {
    if !valid_digest(root) {
        return Err(Error::Invalid);
    }
    let fresh: bool = db.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM sqlite_temp_schema WHERE name='spindle_valid_objects')",
        [],
        |row| row.get(0),
    )?;
    db.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS spindle_valid_roots(scope TEXT NOT NULL, root TEXT NOT NULL, PRIMARY KEY(scope,root));
         CREATE TEMP TABLE IF NOT EXISTS spindle_valid_objects(digest TEXT PRIMARY KEY);",
    )?;
    if fresh {
        known_files(db)?;
    }
    let known: bool = db
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM temp.spindle_valid_roots WHERE scope=?1 AND root=?2)",
        )?
        .query_row(params![scope, root], |row| row.get(0))?;
    let bytes: Option<Vec<u8>> = if known {
        db.prepare_cached(crate::artifact::STORED)?
            .query_row([root], |row| crate::artifact::object_at(row, 0))
            .optional()?
    } else {
        db.prepare_cached(crate::artifact::STORED_IN_SCOPE)?
            .query_row(params![root, scope], |row| {
                crate::artifact::object_at(row, 0)
            })
            .optional()?
    };
    let bytes = bytes.ok_or(Error::Invalid)?;
    if object_digest(&bytes) != root {
        return Err(Error::Incompatible);
    }
    let tree = decode_tree(&bytes)?;
    if !known {
        let pinned = scope_pins(db, scope)?;
        let mut verified = db.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM temp.spindle_valid_objects WHERE digest=?1)",
        )?;
        let mut stored = db.prepare_cached(crate::artifact::STORED)?;
        let mut remember =
            db.prepare_cached("INSERT OR IGNORE INTO temp.spindle_valid_objects VALUES(?1)")?;
        for entry in &tree.files {
            if !pinned.contains(&entry.object) {
                return Err(Error::Incompatible);
            }
            if verified.query_row([&entry.object], |row| row.get(0))? {
                continue;
            }
            let bytes =
                stored.query_row([&entry.object], |row| crate::artifact::object_at(row, 0))?;
            if object_digest(&bytes) != entry.object {
                return Err(Error::Incompatible);
            }
            remember.execute([&entry.object])?;
        }
        db.prepare_cached("INSERT OR IGNORE INTO temp.spindle_valid_roots VALUES(?1,?2)")?
            .execute(params![scope, root])?;
    }
    Ok(tree)
}

/// The files of the last validated revision's workspace (the head an earlier open
/// validated, its files hashed then or by an open before it), remembered as verified
/// on this connection. Its manifest is still checked against its digest.
fn known_files(db: &Connection) -> Result<()> {
    let root: Option<String> = db
        .query_row(
            "SELECT r.workspace FROM validated v JOIN revisions r ON r.revision=v.revision WHERE v.id=1",
            [],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let Some(root) = root.filter(|root| valid_digest(root)) else {
        return Ok(());
    };
    let Some(bytes) = db
        .prepare_cached(crate::artifact::STORED)?
        .query_row([&root], |row| crate::artifact::object_at(row, 0))
        .optional()?
    else {
        return Err(Error::Incompatible);
    };
    if object_digest(&bytes) != root {
        return Err(Error::Incompatible);
    }
    let mut remember =
        db.prepare_cached("INSERT OR IGNORE INTO temp.spindle_valid_objects VALUES(?1)")?;
    for entry in decode_tree(&bytes)?.files {
        remember.execute([&entry.object])?;
    }
    Ok(())
}

impl Store {
    /// Caller supplies a coherent map; no directory scan or host import is implied.
    pub fn stage_file_map(&mut self, stage: &Stage, entries: &[Entry]) -> Result<String> {
        self.stage_tree(
            stage,
            &Tree {
                files: entries.to_vec(),
                ..Tree::default()
            },
        )
    }

    /// Stages a whole tree's manifest; every file object must already be pinned in the
    /// stage's scope.
    pub fn stage_tree(&mut self, stage: &Stage, tree: &Tree) -> Result<String> {
        if self.paused {
            return Err(Error::Paused);
        }
        let bytes = encode_tree(tree)?;
        if !tree.files.is_empty() {
            let pinned = scope_pins(&self.db, &stage.scope);
            let pinned = self.workspace_result(pinned)?;
            if tree
                .files
                .iter()
                .any(|entry| !pinned.contains(&entry.object))
            {
                return Err(Error::Invalid);
            }
        }
        self.put_workspace_object(stage, &bytes)
    }

    pub fn file_map(&mut self, scope: &str, root: &str) -> Result<Vec<Entry>> {
        self.tree_map(scope, root).map(|tree| tree.files)
    }

    /// The whole tree of an immutable root, links and empty directories included.
    pub fn tree_map(&mut self, scope: &str, root: &str) -> Result<Tree> {
        if self.paused {
            return Err(Error::Paused);
        }
        let result = validate_tree_root(&self.db, scope, root);
        self.workspace_result(result)
    }

    /// Reads exactly the selected immutable root, never the latest checkout path.
    pub fn read_versioned_file(&mut self, scope: &str, root: &str, path: &str) -> Result<Vec<u8>> {
        if !valid_path(path) {
            return Err(Error::Invalid);
        }
        let entries = self.file_map(scope, root)?;
        let entry = entries
            .iter()
            .find(|e| e.path == path)
            .ok_or(Error::Invalid)?;
        self.read_object(scope, &entry.object)
    }

    fn workspace_result<T>(&mut self, result: Result<T>) -> Result<T> {
        if matches!(
            result,
            Err(Error::Sql(_) | Error::Io(_) | Error::Locked | Error::Incompatible)
        ) {
            self.paused = true;
        }
        result
    }
}
