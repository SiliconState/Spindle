//! Where an embedded session's stores live. The package supplies storage for Node
//! (files through its VFS, a lock file per store) and browsers (OPFS behind a Web
//! Lock); memory serves tests and scratch sessions. Every store is opened through
//! `Store::from_connection` with a guard released only after SQLite closes.
use crate::{ConnectionProfile, OwnerExclusion, Store};

/// One session's place for its store (and those of the tasks it starts).
pub trait Storage {
    /// How the place is named in messages and events (a path or a name).
    fn label(&self) -> String;
    /// Opens the session's store: a new one (`create`, refused where history exists)
    /// or the existing one (refused where there is none).
    fn open(&self, create: bool) -> crate::Result<Store>;
    /// Closes `store`, keeps its files aside (a first request retried in a fresh
    /// store: the old one keeps its unknown request on record) and opens a new empty
    /// store in its place. Returns the new store and where the old one was kept.
    fn set_aside(&self, store: Store) -> crate::Result<(Store, String)>;
    /// The storage of the task session `key` (a store of its own beside this one).
    fn child(&self, key: &str) -> Box<dyn Storage>;
    /// The sidecar file `name` beside the store (`session.json`, a task's `task.json`),
    /// where this place keeps a native host's layout and the file is there.
    fn sidecar(&self, _name: &str) -> Option<Vec<u8>> {
        None
    }
    /// Keeps `bytes` as the sidecar `name`, durably, where this place keeps a native
    /// host's layout (Node and OPFS directories), so a native host reads the session
    /// as one of its own; elsewhere nothing is kept.
    fn keep_sidecar(&self, _name: &str, _bytes: &[u8]) -> crate::Result<()> {
        Ok(())
    }
    /// Whether nothing is kept in this place yet (an import needs an empty one).
    fn is_empty(&self) -> bool {
        true
    }
    /// The keys of the task sessions kept beside this one (`tasks/<key>/` with a store).
    fn tasks(&self) -> Vec<String> {
        vec![]
    }
    /// Writes `image` (a checked store image) as this place's store, with the files a
    /// native store directory has; refused where a store is already.
    fn import_store(&self, image: &[u8]) -> crate::Result<()>;
    /// Seals this place's session as moved elsewhere (`spindle-moved: <note>`), in
    /// effect once its store closes: no host opens this copy as the session again.
    /// Places that keep nothing once closed (memory) need no seal.
    fn seal(&self, _note: &str) -> crate::Result<()> {
        Ok(())
    }
    /// What the fence of this place's store says (who holds it, or where its session
    /// moved), where the place has one.
    fn holder(&self) -> Option<String> {
        None
    }
    /// Gives the free pages of this place's store back (the store closed): a compacted
    /// copy is written beside it, made durable, checked whole and renamed over it.
    /// Returns the store's size before and after, in bytes; None where nothing is kept
    /// on disk (memory).
    fn compact(&self) -> crate::Result<Option<(u64, u64)>> {
        Ok(None)
    }
}

struct NoGuard;
impl OwnerExclusion for NoGuard {}

/// A fresh in-memory authority, owned by its connection alone.
pub fn memory_store() -> crate::Result<Store> {
    Store::from_connection(
        rusqlite::Connection::open_in_memory()?,
        ConnectionProfile::Memory,
        NoGuard,
    )
}

type Images = std::rc::Rc<std::cell::RefCell<std::collections::BTreeMap<String, Vec<u8>>>>;

/// Sessions that live as long as the app keeps them open (tests, scratch work).
/// Opening one again after it closed finds nothing, unless it was imported (a moved
/// session's store images wait here, the session's and its tasks', until opened).
pub struct MemoryStorage {
    label: String,
    images: Images,
}

impl MemoryStorage {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            images: Images::default(),
        }
    }
}

impl Storage for MemoryStorage {
    fn label(&self) -> String {
        self.label.clone()
    }
    fn open(&self, create: bool) -> crate::Result<Store> {
        if !create {
            let image = self
                .images
                .borrow_mut()
                .remove(&self.label)
                .ok_or(crate::Error::Incompatible)?;
            return Store::from_image(&image, NoGuard);
        }
        memory_store()
    }
    fn set_aside(&self, store: Store) -> crate::Result<(Store, String)> {
        drop(store);
        Ok((
            memory_store()?,
            format!("{} (discarded: memory)", self.label),
        ))
    }
    fn child(&self, key: &str) -> Box<dyn Storage> {
        Box::new(Self {
            label: format!("{}#{key}", self.label),
            images: self.images.clone(),
        })
    }
    fn is_empty(&self) -> bool {
        let prefix = format!("{}#", self.label);
        !self
            .images
            .borrow()
            .keys()
            .any(|label| *label == self.label || label.starts_with(&prefix))
    }
    fn tasks(&self) -> Vec<String> {
        let prefix = format!("{}#", self.label);
        self.images
            .borrow()
            .keys()
            .filter_map(|label| label.strip_prefix(&prefix))
            .filter(|key| !key.contains('#'))
            .map(String::from)
            .collect()
    }
    fn import_store(&self, image: &[u8]) -> crate::Result<()> {
        match self.images.borrow_mut().entry(self.label.clone()) {
            std::collections::btree_map::Entry::Occupied(_) => Err(crate::Error::Conflict),
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(image.to_vec());
                Ok(())
            }
        }
    }
}
