//! Spindle inside an app's own process: sessions whose every effect
//! outside the store (model requests, app tools, commands, timers) the app performs,
//! driven by its event loop. The `spindle-embed` package builds this for wasm and
//! gives it storage (Node files, browser OPFS) and a JavaScript client.
//!
//! `Host` is one session: the native session's turn loop over the driver, parking
//! where the native one blocks and handing the app `Work` instead. `Embedded` serves
//! the public API's methods over such sessions through the shared protocol, so an
//! app can move between `spindle api` and the package.
mod backend;
mod code;
mod env;
mod host;
mod storage;

pub use backend::{Embedded, FEATURES, Places, answer_of};
pub use env::Catalog;
pub use host::{
    ACCOUNT_SLOT, Claim, Dropped, Finished, Host, KEY_SLOT, Progress, Work, WorkAnswer,
    check_settings, host_helper, task_key,
};
pub use storage::{MemoryStorage, Storage, memory_store};
