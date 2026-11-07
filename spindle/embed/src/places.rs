//! Where the package keeps sessions: in memory; (Node, Electron) in directories laid
//! out as a native host lays them out (`<dir>/store/authority.sqlite` with its
//! `owner.lock`, `<dir>/session.json`, a task's in `<dir>/tasks/<task>/` with its
//! `task.json`), each store behind its own lock file, so a native host opens them in
//! place; or (browser) in OPFS under the same paths, keeping only stores, task
//! identities and a native host's pack session id (sessions leave OPFS by bundle), owned
//! by the session's Web Lock.
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
        importing: std::cell::RefCell<Option<std::rc::Rc<node::NodeOwner>>>,
        /// The lock file of the store last opened here, kept past that store's close
        /// while a retry's swap moves it (`replace_store`).
        owner: std::cell::RefCell<std::rc::Weak<node::NodeOwner>>,
    }

    impl NodeStorage {
        fn new(dir: String, journal: JournalMode, note: String) -> Self {
            Self {
                dir,
                journal,
                note,
                seal: node::Seal::default(),
                importing: Default::default(),
                owner: Default::default(),
            }
        }

        fn database(&self) -> String {
            format!("{}/store/authority.sqlite", self.dir)
        }

        /// Opens the session's store behind `owner`, its lock file, which the place
        /// keeps sight of (`replace_store`).
        fn open_held(
            &self,
            owner: std::rc::Rc<node::NodeOwner>,
            initialization: Initialization,
        ) -> spindle_store::Result<Store> {
            *self.owner.borrow_mut() = std::rc::Rc::downgrade(&owner);
            node::open_owned(
                &self.database(),
                self.journal,
                initialization,
                Held { _owner: owner },
            )
        }
    }

    /// The task keys under `dir/tasks` whose store is there.
    fn task_keys(dir: &str) -> Vec<String> {
        node::read_dir(&format!("{dir}/tasks"))
            .into_iter()
            .filter(|key| {
                key.len() == 20
                    && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                    // One that cannot be told is listed: opening it says why.
                    && node::exists(&format!("{dir}/tasks/{key}/store/authority.sqlite"))
                        .unwrap_or(true)
            })
            .collect()
    }

    /// Whether `dir` holds a session's store: `store`, or a fresh store a retry's swap
    /// was cut short before it took that name (`finish_swap`).
    fn holds_store(dir: &str) -> spindle_store::Result<bool> {
        Ok(node::exists(&format!("{dir}/store/authority.sqlite"))?
            || node::exists(&format!("{dir}/{FRESH}/authority.sqlite"))?
                && !node::exists(&format!("{dir}/store"))?)
    }

    /// Where a retry builds the store that replaces a session's (`replace_store`).
    const FRESH: &str = "store.fresh";

    /// Finishes a swap a retry's `replace_store` began: the old store was kept aside and
    /// the fresh one (whole: made before the old moved) had not taken its name yet.
    fn finish_swap(dir: &str) -> spindle_store::Result<()> {
        let fresh = format!("{dir}/{FRESH}");
        if !node::exists(&format!("{dir}/store"))?
            && node::exists(&format!("{fresh}/authority.sqlite"))?
            && let Err(error) = node::rename(&fresh, &format!("{dir}/store"))
            // Another owner finishing it at once may have renamed it first.
            && !node::exists(&format!("{dir}/store/authority.sqlite"))?
        {
            return Err(error);
        }
        Ok(())
    }

    /// A free `store.abandoned-N` name in `dir` (any file there refuses the rename).
    fn aside_name(dir: &str) -> spindle_store::Result<String> {
        let mut index = 1;
        loop {
            let aside = format!("{dir}/store.abandoned-{index}");
            if !node::exists(&aside)? {
                return Ok(aside);
            }
            index += 1;
        }
    }

    /// What keeps every other owner out of a store through a swap (`swap_store`), past
    /// the store's close: a Node store's lock file, which goes with its directory when
    /// that is renamed; nothing for OPFS (the session's Web Lock is held throughout).
    trait SwapHold {
        /// The store's directory was renamed: its lock file is `database`'s now.
        fn moved(&self, _database: &str) {}
        /// The store was set aside: no longer the session's (its seal stays behind).
        fn detach(&self) {}
    }
    impl SwapHold for () {}
    impl SwapHold for std::rc::Rc<node::NodeOwner> {
        fn moved(&self, database: &str) {
            node::NodeOwner::moved(self, database);
        }
        fn detach(&self) {
            node::NodeOwner::detach(self);
        }
    }

    /// Replaces the store of the session in `dir` (`Storage::replace_store`): the new
    /// store is made whole beside it first (`store.fresh`, by `make`, then `prepare`d
    /// with the name the old one will be kept under, and closed), the old one closed and
    /// renamed aside, then the fresh one renamed into its place and opened (`open`). A
    /// crash before the old store moves leaves the session as it was (and a fresh store
    /// the next retry removes); one between the two renames, a whole fresh store that
    /// `finish_swap` names when the session opens. Each store's hold (`old`'s and the
    /// one `make` gives) is kept from before its store closes until its directory has its
    /// new name, so no other owner opens either store meanwhile: an opener finds the old
    /// store's lock file, or the fresh one's, which goes with it into `store` (an opener
    /// that names the fresh store itself, `finish_swap`, then finds it held). The old
    /// store's lock file is let go where it went, never where it was.
    fn swap_store<H: SwapHold>(
        dir: &str,
        old: (Store, H),
        prepare: &mut dyn FnMut(&mut Store, &str) -> spindle_store::Result<()>,
        make: impl FnOnce(&str) -> spindle_store::Result<(Store, H)>,
        open: impl FnOnce(H) -> spindle_store::Result<Store>,
    ) -> spindle_store::Result<(Store, String)> {
        let database = |at: &str| format!("{at}/authority.sqlite");
        let fresh = format!("{dir}/{FRESH}");
        let store = format!("{dir}/store");
        let aside = aside_name(dir)?;
        node::remove_tree(&fresh)?;
        node::make_dirs(&fresh)?;
        let made = make(&database(&fresh)).and_then(|(mut made, hold)| {
            let prepared = prepare(&mut made, &aside);
            // Closed before its hold can go.
            drop(made);
            prepared.map(|()| hold)
        });
        let fresh_hold = match made {
            Ok(hold) => hold,
            Err(error) => {
                let _ = node::remove_tree(&fresh);
                return Err(error);
            }
        };
        let (old, old_hold) = old;
        drop(old);
        if let Err(error) = node::rename(&store, &aside) {
            // Nothing moved: the old store's lock file goes where it is.
            drop(fresh_hold);
            let _ = node::remove_tree(&fresh);
            return Err(error);
        }
        old_hold.detach();
        old_hold.moved(&database(&aside));
        drop(old_hold);
        if let Err(error) = node::rename(&fresh, &store)
            // An opener may have named it first (`finish_swap`): held by this lock file.
            && (node::exists(&fresh)? || !node::exists(&database(&store))?)
        {
            // Its lock file goes, so the next open names it (`finish_swap`).
            return Err(error);
        }
        fresh_hold.moved(&database(&store));
        Ok((open(fresh_hold)?, aside))
    }

    /// Where `remove_task` moves a task's directory before removing it: out of `tasks/`,
    /// so neither host lists it (or a part of it) meanwhile.
    const REMOVING: &str = "tasks.removing";

    /// Removes task `key` of the session in `dir` (`Storage::remove_task`): its directory
    /// renamed out of `tasks/` in one step (a crash leaves it whole there, or out of
    /// every listing), then removed with whatever an earlier removal cut short left.
    fn remove_task(dir: &str, key: &str) -> spindle_store::Result<()> {
        let task = format!("{dir}/tasks/{key}");
        let removing = format!("{dir}/{REMOVING}");
        if node::exists(&task)? {
            sweep(dir)?;
            node::make_dirs(&removing)?;
            node::rename(&task, &format!("{removing}/{key}"))?;
        }
        sweep(dir)
    }

    /// Removes what a `remove_task` in `dir` cut short left, if anything.
    fn sweep(dir: &str) -> spindle_store::Result<()> {
        let removing = format!("{dir}/{REMOVING}");
        if node::exists(&removing)? {
            node::remove_tree(&removing)?;
        }
        Ok(())
    }

    /// Removes everything in `dir` (an import's files, written into an empty place).
    fn clear_dir(dir: &str) -> spindle_store::Result<()> {
        for name in node::read_dir(dir) {
            node::remove_tree(&format!("{dir}/{name}"))?;
        }
        Ok(())
    }

    /// Writes `image` as the store in `dir/store` (refused where one is), with the
    /// `owner.lock` a native store directory has when `native` (a directory a native
    /// host may open in place).
    fn write_store(dir: &str, image: &[u8], native: bool) -> spindle_store::Result<()> {
        let store = format!("{dir}/store");
        if node::exists(&format!("{store}/authority.sqlite"))? {
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
            for path in leftovers(database)[1..3]
                .iter()
                .chain(&leftovers(&copy)[1..])
            {
                if node::exists(path)? {
                    return Err(spindle_store::Error::Locked);
                }
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
            Ok(Box::new(NodeStorage::new(
                dir.trim_end_matches(['/', '\\']).to_string(),
                self.journal,
                self.note.clone(),
            )))
        }
        fn exists(&self, dir: &str) -> bool {
            self.holds(dir).unwrap_or(false)
        }
        fn holds(&self, dir: &str) -> Result<bool, String> {
            holds_store(dir.trim_end_matches(['/', '\\'])).map_err(|error| error.to_string())
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
            finish_swap(&self.dir)?;
            // The file a native host flocks: it opens no store directory without it.
            if node::exists(&store)? {
                node::ensure_file(&format!("{store}/owner.lock"))?;
            }
            let importing = self.importing.borrow_mut().take();
            let owner = match importing {
                Some(owner) => owner,
                None => std::rc::Rc::new(node::acquire_sealable(
                    &self.database(),
                    &self.note,
                    Some(self.seal.clone()),
                )?),
            };
            self.open_held(owner, initialization)
        }
        fn set_aside(&self, store: Store) -> spindle_store::Result<(Store, String)> {
            self.replace_store(store, &mut |_, _| Ok(()))
        }
        fn replace_store(
            &self,
            store: Store,
            prepare: &mut dyn FnMut(&mut Store, &str) -> spindle_store::Result<()>,
        ) -> spindle_store::Result<(Store, String)> {
            // The lock file `store` was opened behind (`open`), held past its close.
            let held = self
                .owner
                .borrow()
                .upgrade()
                .filter(|owner| owner.holds(&self.database()))
                .ok_or(spindle_store::Error::Invalid)?;
            swap_store(
                &self.dir,
                (store, held),
                prepare,
                |database| {
                    node::ensure_file(&format!("{}/{FRESH}/owner.lock", self.dir))?;
                    let owner =
                        std::rc::Rc::new(node::acquire_sealable(database, &self.note, None)?);
                    let made = node::open_owned(
                        database,
                        self.journal,
                        Initialization::Create,
                        Held {
                            _owner: owner.clone(),
                        },
                    )?;
                    Ok((made, owner))
                },
                |owner| {
                    owner.attach(Some(self.seal.clone()));
                    self.open_held(owner, Initialization::Open)
                },
            )
        }
        fn child(&self, key: &str) -> Box<dyn Storage> {
            Box::new(NodeStorage::new(
                format!("{}/tasks/{key}", self.dir),
                self.journal,
                self.note.clone(),
            ))
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
            let names = node::read_dir(&self.dir);
            names.is_empty()
                || names == ["store"] && node::read_dir(&format!("{}/store", self.dir)).is_empty()
        }
        fn tasks(&self) -> Vec<String> {
            task_keys(&self.dir)
        }
        fn begin_import(
            &self,
        ) -> spindle_store::Result<Option<Box<dyn spindle_store::OwnerExclusion>>> {
            node::make_dirs(&format!("{}/store", self.dir))?;
            let owner = node::NodeOwner::acquire(&self.database(), &self.note)?;
            if node::exists(&self.database())? {
                return Err(spindle_store::Error::Conflict);
            }
            owner.attach(Some(self.seal.clone()));
            let owner = std::rc::Rc::new(owner);
            *self.importing.borrow_mut() = Some(owner.clone());
            Ok(Some(Box::new(Held { _owner: owner })))
        }
        fn import_store(&self, image: &[u8]) -> spindle_store::Result<()> {
            write_store(&self.dir, image, true)
        }
        /// Written now, into the lock file its open store holds (refused while none is
        /// open here): a seal that cannot be written fails the move, and the store stays
        /// this session's.
        fn seal(&self, note: &str) -> spindle_store::Result<()> {
            node::seal_held(
                &self.seal,
                &format!("{}{note}\n", spindle_store::MOVED_FENCE),
            )
        }
        fn discard(&self) -> spindle_store::Result<()> {
            if self.importing.borrow().is_some() {
                for name in node::read_dir(&self.dir) {
                    if name == "store" {
                        for file in node::read_dir(&format!("{}/store", self.dir)) {
                            if file != "authority.sqlite.lock" {
                                node::remove_tree(&format!("{}/store/{file}", self.dir))?;
                            }
                        }
                    } else {
                        node::remove_tree(&format!("{}/{name}", self.dir))?;
                    }
                }
                return Ok(());
            }
            clear_dir(&self.dir)
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
        fn size(&self) -> u64 {
            node::tree_size(&self.dir)
        }
        fn remove_task(&self, key: &str) -> spindle_store::Result<()> {
            remove_task(&self.dir, key)
        }
        fn sweep(&self) -> spindle_store::Result<()> {
            sweep(&self.dir)
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
        fn holds(&self, dir: &str) -> Result<bool, String> {
            self.0.holds(dir)
        }
        fn release(&self, dir: &str) {
            self.0.release(dir)
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
            self.holds(dir).unwrap_or(false)
        }
        fn holds(&self, dir: &str) -> Result<bool, String> {
            if !opfs_path(dir) {
                return Ok(false);
            }
            holds_store(dir).map_err(|error| error.to_string())
        }
        /// The app's hold on `dir`'s Web Lock, if no store took it over: given back now
        /// (a session closed, or never opened by the calls that took it).
        fn release(&self, dir: &str) {
            let held = self.held.borrow_mut().remove(dir);
            drop(held);
        }
    }

    /// The sidecars an OPFS place keeps (see `OpfsStorage::sidecar`).
    fn kept_in_opfs(name: &str) -> bool {
        name == "task.json" || name == spindle_store::bundle::PACK_SESSION
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
            finish_swap(&self.dir)?;
            // OPFS stores have no lock file of their own (the Web Lock excludes): a fence
            // here is a moved session's seal, or another host's, and refuses the open.
            if node::exists(&format!("{}/store/{}", self.dir, spindle_store::FENCE))? {
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
            self.replace_store(store, &mut |_, _| Ok(()))
        }
        fn replace_store(
            &self,
            store: Store,
            prepare: &mut dyn FnMut(&mut Store, &str) -> spindle_store::Result<()>,
        ) -> spindle_store::Result<(Store, String)> {
            swap_store(
                &self.dir,
                (store, ()),
                prepare,
                |database| {
                    let made = node::open_owned(
                        database,
                        self.journal,
                        Initialization::Create,
                        Shared {
                            _lock: self.lock.clone(),
                        },
                    )?;
                    Ok((made, ()))
                },
                |()| self.open(false),
            )
        }
        fn child(&self, key: &str) -> Box<dyn Storage> {
            Box::new(OpfsStorage {
                dir: format!("{}/tasks/{key}", self.dir),
                journal: self.journal,
                lock: self.lock.clone(),
            })
        }
        /// Only a task's identity and a native host's MCP pack session id are kept: no
        /// native host opens an OPFS directory in place (a session leaves by bundle, which
        /// rebuilds the settings mirror from its store and gives a native store directory
        /// its `owner.lock`), and every file takes one of the pool's few slots. The pack
        /// session id is the place's own (no store holds it): kept, it goes on with the
        /// session when it moves to a native host again.
        fn sidecar(&self, name: &str) -> Option<Vec<u8>> {
            kept_in_opfs(name)
                .then(|| node::read_file(&format!("{}/{name}", self.dir)))
                .flatten()
        }
        fn keep_sidecar(&self, name: &str, bytes: &[u8]) -> spindle_store::Result<()> {
            if !kept_in_opfs(name) {
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
        fn discard(&self) -> spindle_store::Result<()> {
            clear_dir(&self.dir)
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
        fn size(&self) -> u64 {
            node::tree_size(&self.dir)
        }
        fn remove_task(&self, key: &str) -> spindle_store::Result<()> {
            remove_task(&self.dir, key)
        }
        fn sweep(&self) -> spindle_store::Result<()> {
            sweep(&self.dir)
        }
    }
}
