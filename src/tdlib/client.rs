//! [`TdClient`]: the process's single handle to a running TDLib client.
//!
//! # Threading model
//!
//! TDLib's `receive()` is a *blocking* call, so it runs on a dedicated OS thread
//! (never on the GLib executor, which has no reactor, nor on the tokio runtime,
//! whose worker threads should stay free). That thread pushes every raw
//! [`Update`] down an `async_channel`; a `glib::spawn_future_local` task on the
//! GTK main thread drains it and fans each update out to subscribers.
//!
//! Because [`TdClient`] is `Rc`-backed it is **main-thread-only**: it is cloned
//! freely across UI code but never crosses a thread boundary. The only things
//! that cross into the pump thread are the `async_channel::Sender<Update>` and
//! the `Update`s themselves, both of which are `Send`.
//!
//! *Requests* (`tdlib_rs::functions::*`) are async and need a tokio reactor, so
//! they are dispatched through [`crate::runtime::spawn`]; see [`TdClient::request`].

use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gtk::glib;

use tdlib_rs::enums::Update;

/// Shared, main-thread-only state behind a [`TdClient`] handle.
struct Inner {
    /// Live update subscribers. Closed senders are pruned lazily on dispatch.
    subscribers: RefCell<Vec<async_channel::Sender<Update>>>,
    /// Shared file-download cache (avatars + media), created lazily on first use.
    files: RefCell<Option<crate::tdlib::files::FileStore>>,
    /// Cleared on shutdown to stop the blocking receive pump.
    running: Arc<AtomicBool>,
}

/// A cheap, cloneable handle to the running TDLib client.
///
/// Every clone refers to the same underlying client (same `client_id`, same
/// subscriber set). Use it on the GTK main thread only.
#[derive(Clone)]
pub struct TdClient {
    client_id: i32,
    inner: Rc<Inner>,
}

impl TdClient {
    /// Create a TDLib client, start its update pump, and send the warm-up request.
    ///
    /// TDLib delivers no updates until it receives its first request, so we
    /// immediately fire `set_log_verbosity_level` to kick the state machine
    /// (which triggers the initial `WaitTdlibParameters` authorization update).
    pub fn new() -> Self {
        let client_id = tdlib_rs::create_client();

        // Cleared on shutdown to stop the blocking receive pump.
        let running = Arc::new(AtomicBool::new(true));

        let this = TdClient {
            client_id,
            inner: Rc::new(Inner {
                subscribers: RefCell::new(Vec::new()),
                files: RefCell::new(None),
                running: running.clone(),
            }),
        };

        // Raw update channel: filled by the blocking pump thread, drained on the
        // GTK main thread. Unbounded so the pump never blocks on a slow UI.
        let (raw_tx, raw_rx) = async_channel::unbounded::<Update>();

        // The blocking receive loop. A plain OS thread keeps the blocking call
        // off both the GLib executor and the tokio runtime.
        let pump_running = running.clone();
        std::thread::spawn(move || loop {
            // Stop when the pump has been signalled to shut down.
            if !pump_running.load(Ordering::Relaxed) {
                break;
            }
            match tdlib_rs::receive() {
                Some((update, _client_id)) => {
                    // Err means every receiver was dropped (app shutting down).
                    if raw_tx.send_blocking(update).is_err() {
                        break;
                    }
                }
                // No update ready yet; back off briefly and poll again.
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        });

        // Drain raw updates on the GTK thread and fan them out to subscribers.
        let dispatcher = this.clone();
        glib::spawn_future_local(async move {
            while let Ok(update) = raw_rx.recv().await {
                let d = dispatcher.clone();
                if catch_unwind(AssertUnwindSafe(move || d.dispatch(update))).is_err() {
                    tracing::error!("panic in update dispatch; continuing");
                }
            }
        });

        // Warm-up request: nudges TDLib into emitting its first auth update.
        let cid = client_id;
        crate::runtime::spawn(
            async move { tdlib_rs::functions::set_log_verbosity_level(1, cid).await },
            |_res| {},
        );

        this
    }

    /// The TDLib client identifier this handle wraps.
    pub fn client_id(&self) -> i32 {
        self.client_id
    }

    /// Signal the blocking receive pump to stop (used on logout/close).
    #[allow(dead_code)]
    pub fn shutdown(&self) {
        self.inner.running.store(false, Ordering::Relaxed);
    }

    /// The shared file-download cache for this session (created on first use).
    ///
    /// Every caller gets the *same* [`crate::tdlib::files::FileStore`], so a
    /// path resolved for one widget is instantly visible to all the others.
    pub fn files(&self) -> crate::tdlib::files::FileStore {
        self.inner
            .files
            .borrow_mut()
            .get_or_insert_with(|| crate::tdlib::files::FileStore::new(self.client_id))
            .clone()
    }

    /// Forward one update to every live subscriber, pruning closed channels.
    fn dispatch(&self, update: Update) {
        self.inner.subscribers.borrow_mut().retain(|sender| {
            match sender.try_send(update.clone()) {
                // Delivered, or full (unbounded so this can't actually happen) —
                // keep the subscriber either way.
                Ok(()) | Err(async_channel::TrySendError::Full(_)) => true,
                // Receiver dropped: drop this subscriber.
                Err(async_channel::TrySendError::Closed(_)) => false,
            }
        });
    }

    /// Subscribe to the raw TDLib update stream.
    ///
    /// Returns a fresh receiver; consume it with `glib::spawn_future_local`.
    /// Dropping the receiver unsubscribes automatically (pruned on next dispatch).
    pub fn subscribe(&self) -> async_channel::Receiver<Update> {
        let (tx, rx) = async_channel::unbounded::<Update>();
        self.inner.subscribers.borrow_mut().push(tx);
        rx
    }

    /// Run a TDLib request on the tokio runtime, delivering the result on the
    /// GTK thread.
    ///
    /// `tdlib_rs::functions::*` futures are `Send` (the library is thread-safe by
    /// `client_id`), so this bridges cleanly through [`crate::runtime::spawn`].
    pub fn request<F, T>(
        &self,
        fut: F,
        on_done: impl FnOnce(Result<T, tdlib_rs::types::Error>) + 'static,
    ) where
        F: std::future::Future<Output = Result<T, tdlib_rs::types::Error>> + Send + 'static,
        T: Send + 'static,
    {
        crate::runtime::spawn(fut, on_done);
    }

    // --- Convenience async helpers -------------------------------------------
    //
    // These simply call `tdlib_rs::functions::*` with the stored `client_id` and
    // unwrap the response enum. They're meant to be awaited from async contexts
    // (e.g. inside `client.request(...)`), not called directly on the GTK thread.

    /// Fetch up to `limit` chat ids from the Main list.
    pub async fn get_chats(&self, limit: i32) -> Result<Vec<i64>, tdlib_rs::types::Error> {
        use tdlib_rs::enums::{ChatList, Chats};

        let chats =
            tdlib_rs::functions::get_chats(Some(ChatList::Main), limit, self.client_id).await?;
        let Chats::Chats(c) = chats;
        Ok(c.chat_ids)
    }

    /// Ask TDLib to load more Main-list chats into the local cache.
    ///
    /// Returns an error with code `404` when all chats are already loaded; that
    /// is expected and callers should treat it as "nothing more to load".
    pub async fn load_chats(&self, limit: i32) -> Result<(), tdlib_rs::types::Error> {
        use tdlib_rs::enums::ChatList;

        tdlib_rs::functions::load_chats(Some(ChatList::Main), limit, self.client_id).await
    }

    /// Fetch full chat info for a single chat id.
    pub async fn get_chat(
        &self,
        chat_id: i64,
    ) -> Result<tdlib_rs::types::Chat, tdlib_rs::types::Error> {
        use tdlib_rs::enums::Chat;

        let chat = tdlib_rs::functions::get_chat(chat_id, self.client_id).await?;
        let Chat::Chat(c) = chat;
        Ok(c)
    }
}
