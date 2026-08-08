//! [`FileStore`]: a per-session TDLib file-download cache shared by avatars and
//! media.
//!
//! TDLib addresses every downloadable blob (avatars, photo sizes, documents…) by
//! a small integer `file_id`. [`FileStore::download`] fetches one via
//! `functions::download_file(file_id, priority, offset=0, limit=0,
//! synchronous=true, client_id)` on the tokio runtime; with `synchronous=true`
//! the future resolves only once the whole file is on disk, so the resulting
//! `File.local.path` is immediately usable. Fine for the small files this wave
//! needs (160×160 avatars, chat photos).
//!
//! Resolved paths are cached by `file_id` so a recycling list factory can look
//! one up synchronously on bind ([`FileStore::cached`]) and only spawn a
//! download on a miss. A per-`file_id` in-flight guard collapses the burst of
//! duplicate requests a freshly-scrolled list produces into a single download,
//! delivering the path to every queued caller when it lands.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

/// A cheap, cloneable, main-thread-only handle to the file-download cache.
///
/// Every clone shares the same cache + in-flight table (`Rc`-backed), so a path
/// resolved for one widget is instantly visible to all the others.
#[derive(Clone)]
pub struct FileStore {
    client_id: i32,
    inner: Rc<Inner>,
}

/// A queue of callbacks awaiting one file's download completion.
type Waiters = Vec<Box<dyn FnOnce(PathBuf)>>;

struct Inner {
    /// `file_id` → resolved on-disk path, for O(1) synchronous lookup on bind.
    cache: RefCell<HashMap<i32, PathBuf>>,
    /// `file_id` → callbacks awaiting the in-flight download of that file.
    /// Presence of a key also means "a download is already running for it".
    pending: RefCell<HashMap<i32, Waiters>>,
}

impl FileStore {
    /// Build an empty store bound to a TDLib `client_id`.
    pub fn new(client_id: i32) -> Self {
        FileStore {
            client_id,
            inner: Rc::new(Inner {
                cache: RefCell::new(HashMap::new()),
                pending: RefCell::new(HashMap::new()),
            }),
        }
    }

    /// The already-downloaded local path for `file_id`, if we have resolved it.
    pub fn cached(&self, file_id: i32) -> Option<PathBuf> {
        self.inner.cache.borrow().get(&file_id).cloned()
    }

    /// Resolve the local path for `file_id`, invoking `on_done` on the GTK main
    /// thread once it is available.
    ///
    /// * cache hit  → `on_done` fires (deferred, so callers never re-enter a
    ///   borrow synchronously) with the cached path;
    /// * in flight  → `on_done` is queued behind the running download;
    /// * cold       → a new `download_file` is spawned; every queued `on_done`
    ///   for the same `file_id` fires when it lands.
    ///
    /// `priority` is TDLib's 1–32 download priority (higher = sooner).
    pub fn download(&self, file_id: i32, priority: i32, on_done: impl FnOnce(PathBuf) + 'static) {
        if file_id == 0 {
            return;
        }
        // Fast path: already on disk.
        if let Some(path) = self.cached(file_id) {
            gtk::glib::idle_add_local_once(move || on_done(path));
            return;
        }

        // Queue behind (or start) the download for this file_id.
        {
            let mut pending = self.inner.pending.borrow_mut();
            let entry = pending.entry(file_id).or_default();
            entry.push(Box::new(on_done));
            // A download is already in flight for this file_id — piggyback on it.
            if entry.len() > 1 {
                return;
            }
        }

        let cid = self.client_id;
        let this = self.clone();
        crate::runtime::spawn(
            async move {
                use tdlib_rs::enums::File;
                match tdlib_rs::functions::download_file(file_id, priority, 0, 0, true, cid).await {
                    Ok(File::File(f)) => Some(f.local.path),
                    Err(e) => {
                        tracing::warn!(file_id, code = e.code, msg = %e.message, "download_file failed");
                        None
                    }
                }
            },
            move |res| this.resolve(file_id, res),
        );
    }

    /// A download completed (or failed): cache a good path, then drain every
    /// queued callback for this `file_id`.
    fn resolve(&self, file_id: i32, path: Option<String>) {
        let waiters = self.inner.pending.borrow_mut().remove(&file_id);
        let path = match path {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            // Empty/failed: drop the waiters without firing (nothing to show).
            _ => return,
        };
        self.inner.cache.borrow_mut().insert(file_id, path.clone());
        if let Some(waiters) = waiters {
            for cb in waiters {
                cb(path.clone());
            }
        }
    }
}
