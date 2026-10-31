//! Where the package keeps sessions: in memory; (Node, Electron) in directories laid
//! out as a native host lays them out (`<dir>/store/authority.sqlite` with its
//! `owner.lock`, `<dir>/session.json`, a task's in `<dir>/tasks/<task>/` with its
//! `task.json`), each store behind its own lock file, so a native host opens them in
//! place; or (browser) in OPFS under the same paths, keeping only stores and task
//! identities (sessions leave OPFS by bundle), owned by the session's Web Lock.
use spindle_store::embedded::{MemoryStorage, Places, Storage};

/// Memory sessions: they last while the app keeps them open.
pub struct MemoryPlaces;

impl Places for MemoryPlaces {
    fn storage(&self, dir: &str) -> Result<Box<dyn Storage>, String> {
        if dir.is_empty() || dir.contains('#') {
            return Err("`dir` names a session (no `#`)".into());
        }
        Ok(Box::new(MemoryStorage::new(dir)))
    }
    fn exists(&self, _: &str) -> bool {
        false
    }
}

#[cfg(target_family = "wasm")]
pub use node::{NodePlaces, OpfsPlaces, SharedOpfs};

#[cfg(target_family = "wasm")]
mod node {
    use super::*;
    use crate::node;
    use spindle_store::{Initialization, JournalMode, Store};

    /// Sessions in directories through the Node VFS. `dir` is an absolute path (the
    /// client resolves it); `note` names this owner in its lock files.
    pub struct NodePlaces {
        pub journal: JournalMode,
        pub note: String,
    }

    struct NodeStorage {
        /// The session directory.
        dir: String,
        journal: JournalMode,
        note: String,
        /// The seal its open store's lock file takes when it closes (`Storage::seal`).
        seal: node::Seal,
    }

    impl NodeStorage {
        fn database(&self) -> String {
            format!("{}/store/authority.sqlite", self.dir)
        }
    }

    /// The task keys under `dir/tasks` whose store is there.
    fn task_keys(dir: &str) -> Vec<String> {
        node::read_dir(&format!("{dir}/tasks"))
            .into_iter()
            .filter(|key| {
                key.len() == 20
                    && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                    && node::exists(&format!("{dir}/tasks/{key}/store/authority.sqlite"))
            })
            .collect()
    }

    /// Writes `image` as the store in `dir/store` (refused where one is), with the
    /// `owner.lock` a native store directory has when `native` (a directory a native
    /// host may open in place).
    fn write_store(dir: &str, image: &[u8], native: bool) -> spindle_store::Result<()> {
        let store = format!("{dir}/store");
        if node::exists(&format!("{store}/authority.sqlite")) {
            return Err(spindle_store::Error::Conflict);
        }
        node::make_dirs(&store)?;
        if native {
            node::ensure_file(&format!("{store}/owner.lock"))?;
        }
        node::write_file(&format!("{store}/authority.sqlite"), image)
    }

    /// Compacts the store at `database` (closed): `open` opens it with what keeps every
    /// other owner out (`G`) until the copy has its name, even once the store is closed;
    /// `open_copy` opens a file beside it as a store. The copy is written by VACUUM INTO,
    /// synced (SQLite does not), opened alone and checked whole (every page and record,
    /// its head the store's), and only then renamed over the store; a crash before the
    /// rename leaves the store as it was (and a copy the next compaction removes), one
    /// during it the copy or the store whole under the name. Returns the store's size
    /// before and after, in bytes.
    fn compact_store<G>(
        database: &str,
        open: impl FnOnce() -> spindle_store::Result<(Store, G)>,
        open_copy: impl Fn(&str) -> spindle_store::Result<Store>,
    ) -> spindle_store::Result<(u64, u64)> {
        let copy = format!("{database}.compact");
        let leftovers =
            |path: &str| ["", "-wal", "-journal", ".lock"].map(|end| format!("{path}{end}"));
        let (mut store, _held) = open()?;
        // What a compaction cut short left (the store is ours now): never the store, as
        // its swap is one rename.
        for path in leftovers(&copy) {
            node::remove_file(&path)?;
        }
        let (page, pages, free) = store.pages()?;
        let before = page * pages;
        if free == 0 {
            return Ok((before, before));
        }
        let head = store.snapshot()?;
        let written = store.compact_into(&copy);
        // Closed before the copy is read: its log goes back into it and is removed.
        drop(store);
        let checked = written.and_then(|()| {
            node::sync_file(&copy)?;
            let mut copied = open_copy(&copy)?;
            copied.check_integrity()?;
            if copied.snapshot()? != head {
                return Err(spindle_store::Error::Invalid);
            }
            let (page, pages, _) = copied.pages()?;
            drop(copied);
            // A store not closed cleanly keeps a log or journal that would be read into
            // the copy once it has the name: no swap then.
            if leftovers(database)[1..3]
                .iter()
                .any(|path| node::exists(path))
                || leftovers(&copy)[1..].iter().any(|path| node::exists(path))
            {
                return Err(spindle_store::Error::Locked);
            }
            Ok(page * pages)
        });
        let after = match checked {
            Ok(after) => after,
            Err(error) => {
                for path in leftovers(&copy) {
                    let _ = node::remove_file(&path);
                }
                return Err(error);
            }
        };
        if let Err(error) = node::rename(&copy, database) {
            // Not renamed: the store is as it was, and the copy goes. (A rename the pool
            // cut part way left the copy under the store's name: the same session either
            // way, and no copy is left to remove.)
            for path in leftovers(&copy) {
                let _ = node::remove_file(&path);
            }
            return Err(error);
        }
        Ok((before, after))
    }

    /// A Node store's lock file shared by its store and whoever holds it past its close.
    struct Held {
        _owner: std::rc::Rc<node::NodeOwner>,
    }
    impl spindle_store::OwnerExclusion for Held {}

    fn fence_of(dir: &str) -> Option<String> {
        node::read_file(&format!("{dir}/store/{}", spindle_store::FENCE))
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
    }

    impl Places for NodePlaces {
        fn storage(&self, dir: &str) -> Result<Box<dyn Storage>, String> {
            if !dir.starts_with('/') && !dir.get(1..3).is_some_and(|s| s == ":\\" || s == ":/") {
                return Err("`dir` must be an absolute path".into());
            }
            if dir.contains('#') {
                return Err("`dir` may not contain `#`".into());
            }
            Ok(Box::new(NodeStorage {
                dir: dir.trim_end_matches(['/', '\\']).to_string(),
                journal: self.journal,
                note: self.note.clone(),
                seal: node::Seal::default(),
            }))
        }
        fn exists(&self, dir: &str) -> bool {
            node::exists(&format!(
                "{}/store/authority.sqlite",
                dir.trim_end_matches(['/', '\\'])
            ))
        }
    }

    impl Storage for NodeStorage {
        fn label(&self) -> String {
            self.dir.clone()
        }
        fn open(&self, create: bool) -> spindle_store::Result<Store> {
            let store = format!("{}/store", self.dir);
            let initialization = if create {
                node::make_dirs(&store)?;
                Initialization::Create
            } else {
                Initialization::Open
            };
            // The file a native host flocks: it opens no store directory without it.
            if node::exists(&store) {
                node::ensure_file(&format!("{store}/owner.lock"))?;
            }
            node::open_sealable(
                &self.database(),
                self.journal,
                initialization,
                &self.note,
                Some(self.seal.clone()),
            )
        }
        fn set_aside(&self, store: Store) -> spindle_store::Result<(Store, String)> {
            drop(store);
            let mut index = 1;
            let aside = loop {
                let aside = format!("{}/store.abandoned-{index}", self.dir);
                if !node::exists(&aside) {
                    break aside;
                }
                index += 1;
            };
            node::rename(&format!("{}/store", self.dir), &aside)?;
            let store = self.open(true)?;
            Ok((store, aside))
        }
        fn child(&self, key: &str) -> Box<dyn Storage> {
            Box::new(NodeStorage {
                dir: format!("{}/tasks/{key}", self.dir),
                journal: self.journal,
                note: self.note.clone(),
                seal: node::Seal::default(),
            })
        }
        fn sidecar(&self, name: &str) -> Option<Vec<u8>> {
            node::read_file(&format!("{}/{name}", self.dir))
        }
        fn keep_sidecar(&self, name: &str, bytes: &[u8]) -> spindle_store::Result<()> {
            // An import writes a task's sidecars before its store: its directory first.
            node::make_dirs(&self.dir)?;
            node::write_file(&format!("{}/{name}", self.dir), bytes)
        }
        fn is_empty(&self) -> bool {
            node::read_dir(&self.dir).is_empty()
        }
        fn tasks(&self) -> Vec<String> {
            task_keys(&self.dir)
        }
        fn import_store(&self, image: &[u8]) -> spindle_store::Result<()> {
            write_store(&self.dir, image, true)
        }
        fn seal(&self, note: &str) -> spindle_store::Result<()> {
            *self.seal.borrow_mut() = Some(format!("{}{note}\n", spindle_store::MOVED_FENCE));
            Ok(())
        }
        fn holder(&self) -> Option<String> {
            fence_of(&self.dir)
        }
        fn compact(&self) -> spindle_store::Result<Option<(u64, u64)>> {
            let database = self.database();
            compact_store(
                &database,
                // Its lock file, held past the store's close until the copy has the name:
                // no other owner opens the store meanwhile (and a sealed one is refused).
                || {
                    let owner = std::rc::Rc::new(node::NodeOwner::acquire(&database, &self.note)?);
                    let store = node::open_owned(
                        &database,
                        self.journal,
                        Initialization::Open,
                        Held {
                            _owner: owner.clone(),
                        },
                    )?;
                    Ok((store, owner))
                },
                |copy| node::open(copy, self.journal, Initialization::Open, &self.note),
            )
            .map(Some)
        }
    }

    /// A session's Web Lock, held by the app: released (its callback called) once the
    /// last store opened under it has closed.
    struct Lock(js_sys::Function);
    impl Drop for Lock {
        fn drop(&mut self) {
            let _ = self.0.call0(&wasm_bindgen::JsValue::NULL);
        }
    }
    struct Shared {
        _lock: std::rc::Rc<Lock>,
    }
    impl spindle_store::OwnerExclusion for Shared {}

    /// Sessions in a browser worker's OPFS (through the VFS over the OPFS adapter),
    /// each owned by a Web Lock the app takes before the session opens (`own`).
    pub struct OpfsPlaces {
        pub journal: JournalMode,
        owners: std::cell::RefCell<std::collections::BTreeMap<String, std::rc::Weak<Lock>>>,
        held: std::cell::RefCell<std::collections::BTreeMap<String, std::rc::Rc<Lock>>>,
    }

    impl OpfsPlaces {
        pub fn new(journal: JournalMode) -> Self {
            Self {
                journal,
                owners: Default::default(),
                held: Default::default(),
            }
        }
        /// The app holds `dir`'s Web Lock; `release` gives it back. Refused (and not
        /// kept: the app gives the lock back) for a path no session can have.
        pub fn own(&self, dir: &str, release: js_sys::Function) -> Result<(), String> {
            if !opfs_path(dir) {
                return Err("`dir` is an absolute OPFS path such as /sessions/a".into());
            }
            let lock = std::rc::Rc::new(Lock(release));
            self.owners
                .borrow_mut()
                .insert(dir.to_string(), std::rc::Rc::downgrade(&lock));
            self.held.borrow_mut().insert(dir.to_string(), lock);
            Ok(())
        }
        fn lock(&self, dir: &str) -> Result<std::rc::Rc<Lock>, String> {
            let top = dir.split('#').next().unwrap_or(dir);
            // The first store opened takes the app's hold over; from then on the lock
            // lives as long as some store of the session is open.
            if let Some(lock) = self.held.borrow_mut().remove(top) {
                return Ok(lock);
            }
            self.owners
                .borrow()
                .get(top)
                .and_then(std::rc::Weak::upgrade)
                .ok_or_else(|| format!("the Web Lock of `{top}` is not held; open it again"))
        }
    }

    /// `OpfsPlaces` shared between the session API and the call that owns sessions.
    pub struct SharedOpfs(pub std::rc::Rc<OpfsPlaces>);

    impl Places for SharedOpfs {
        fn storage(&self, dir: &str) -> Result<Box<dyn Storage>, String> {
            self.0.storage(dir)
        }
        fn exists(&self, dir: &str) -> bool {
            self.0.exists(dir)
        }
    }

    struct OpfsStorage {
        dir: String,
        journal: JournalMode,
        lock: std::rc::Rc<Lock>,
    }

    /// Whether `dir` is an OPFS session path in its one spelling (`/a/b`: no empty,
    /// `.` or `..` part, no trailing `/`, no `#`). OPFS files have no locks of their own
    /// (the Web Lock is the exclusion, named by `dir`), so two spellings of one place
    /// must not both open it.
    fn opfs_path(dir: &str) -> bool {
        dir.strip_prefix('/').is_some_and(|rest| {
            !dir.contains('#')
                && rest
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != "..")
        })
    }

    impl Places for OpfsPlaces {
        fn storage(&self, dir: &str) -> Result<Box<dyn Storage>, String> {
            if !opfs_path(dir) {
                return Err(
                    "`dir` is an absolute OPFS path such as /sessions/a (no empty, `.` or \
                     `..` part, no trailing `/`, no `#`)"
                        .into(),
                );
            }
            Ok(Box::new(OpfsStorage {
                dir: dir.to_string(),
                journal: self.journal,
                lock: self.lock(dir)?,
            }))
        }
        fn exists(&self, dir: &str) -> bool {
            opfs_path(dir) && node::exists(&format!("{dir}/store/authority.sqlite"))
        }
    }

    impl Storage for OpfsStorage {
        fn label(&self) -> String {
            self.dir.clone()
        }
        fn open(&self, create: bool) -> spindle_store::Result<Store> {
            let initialization = if create {
                Initialization::Create
            } else {
                Initialization::Open
            };
            // OPFS stores have no lock file of their own (the Web Lock excludes): a fence
            // here is a moved session's seal, or another host's, and refuses the open.
            if node::exists(&format!("{}/store/{}", self.dir, spindle_store::FENCE)) {
                return Err(spindle_store::Error::Locked);
            }
            node::open_owned(
                &format!("{}/store/authority.sqlite", self.dir),
                self.journal,
                initialization,
                Shared {
                    _lock: self.lock.clone(),
                },
            )
        }
        fn set_aside(&self, store: Store) -> spindle_store::Result<(Store, String)> {
            drop(store);
            let mut index = 1;
            let aside = loop {
                let aside = format!("{}/store.abandoned-{index}", self.dir);
                // Any file there (not only a database) refuses the rename.
                if !node::exists(&aside) {
                    break aside;
                }
                index += 1;
            };
            node::rename(&format!("{}/store", self.dir), &aside)?;
            Ok((self.open(true)?, aside))
        }
        fn child(&self, key: &str) -> Box<dyn Storage> {
            Box::new(OpfsStorage {
                dir: format!("{}/tasks/{key}", self.dir),
                journal: self.journal,
                lock: self.lock.clone(),
            })
        }
        /// Only a task's identity is kept: no native host opens an OPFS directory in place
        /// (a session leaves by bundle, which rebuilds the settings mirror from its store
        /// and gives a native store directory its `owner.lock`), and every file takes one
        /// of the pool's few slots.
        fn sidecar(&self, name: &str) -> Option<Vec<u8>> {
            (name == "task.json")
                .then(|| node::read_file(&format!("{}/{name}", self.dir)))
                .flatten()
        }
        fn keep_sidecar(&self, name: &str, bytes: &[u8]) -> spindle_store::Result<()> {
            if name != "task.json" {
                return Ok(());
            }
            node::write_file(&format!("{}/{name}", self.dir), bytes)
        }
        fn is_empty(&self) -> bool {
            node::read_dir(&self.dir).is_empty()
        }
        fn tasks(&self) -> Vec<String> {
            task_keys(&self.dir)
        }
        fn import_store(&self, image: &[u8]) -> spindle_store::Result<()> {
            write_store(&self.dir, image, false)
        }
        /// Written now: this place holds the session's Web Lock, so no other worker
        /// opens the store before it closes here.
        fn seal(&self, note: &str) -> spindle_store::Result<()> {
            node::write_file(
                &format!("{}/store/{}", self.dir, spindle_store::FENCE),
                format!("{}{note}\n", spindle_store::MOVED_FENCE).as_bytes(),
            )
        }
        fn holder(&self) -> Option<String> {
            fence_of(&self.dir)
        }
        fn compact(&self) -> spindle_store::Result<Option<(u64, u64)>> {
            let database = format!("{}/store/authority.sqlite", self.dir);
            compact_store(
                &database,
                // The session's Web Lock is this place's while it lives.
                || Ok((self.open(false)?, ())),
                |copy| {
                    node::open_owned(
                        copy,
                        self.journal,
                        Initialization::Open,
                        Shared {
                            _lock: self.lock.clone(),
                        },
                    )
                },
            )
            .map(Some)
        }
    }
}
