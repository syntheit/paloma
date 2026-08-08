//! Async bridge between the shared tokio runtime and the GLib main loop.
//!
//! reqwest (and therefore `linkding-rs` and `oo7`) needs a tokio reactor in the
//! polling thread. The GLib executor has none, so we run network/keyring futures
//! on a dedicated multi-thread tokio runtime and marshal only the *result* back
//! to the GTK thread, where `!Send` widget updates are legal.
//!
//! Usage:
//! ```ignore
//! spawn(async { client.list_bookmarks(args).await }, move |res| {
//!     // runs on the GTK main thread
//!     match res { Ok(_) => (), Err(_) => () }
//! });
//! ```

use std::future::Future;
use std::sync::OnceLock;

use gtk::glib;
use tokio::runtime::Runtime;

/// The process-wide tokio runtime. Created lazily on first use.
fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        Runtime::new().expect("failed to build the tokio runtime")
    })
}

/// Run `fut` on the tokio runtime and invoke `on_done` with its output on the
/// GTK main thread once it completes.
///
/// `fut` must be `Send` (it runs on a worker thread); `on_done` runs locally and
/// may freely touch widgets.
pub fn spawn<F, T, G>(fut: F, on_done: G)
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
    G: FnOnce(T) + 'static,
{
    let (sender, receiver) = async_channel::bounded::<T>(1);

    runtime().spawn(async move {
        let result = fut.await;
        // If the receiver was dropped (window closed mid-flight) this is a no-op.
        let _ = sender.send(result).await;
    });

    glib::spawn_future_local(async move {
        if let Ok(value) = receiver.recv().await {
            on_done(value);
        }
    });
}
