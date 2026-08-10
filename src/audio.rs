//! Shared single-slot voice-note audio player.
//!
//! A single shared [`gstplay::Play`] pipeline plays at most one voice note at a
//! time across the whole app; voice bubbles call [`VoicePlayer::start`] to take
//! ownership and receive [`VoiceEvent`]s (duration/position/playing/ended) via a
//! callback. A monotonic "token" identifies the current owner, so a recycled
//! list row can detach a stale playback with [`VoicePlayer::stop_if_owner`] once
//! it no longer represents the note it started. Main-thread only.

use gtk::glib;
use gstreamer as gst;
use gstreamer_play as gstplay;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Initialize GStreamer exactly once for the process.
///
/// Failure is non-fatal: we log and continue, so the app still runs (voice
/// playback simply won't work) rather than panicking on a missing/broken
/// GStreamer install.
fn ensure_init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Err(e) = gst::init() {
            tracing::warn!(error = %e, "GStreamer init failed; voice playback disabled");
        }
    });
}

/// Progress/state delivered to a voice bubble's UI as playback proceeds.
#[derive(Clone, Copy, Debug)]
pub enum VoiceEvent {
    /// Duration became known (nanoseconds).
    Duration(u64),
    /// Position advanced (nanoseconds).
    Position(u64),
    /// Playing (true) or paused (false).
    Playing(bool),
    /// Playback finished (reached end) or was stopped for this owner.
    Ended,
}

/// A voice bubble's per-event callback (runs on the GTK main thread).
type EventCallback = Box<dyn Fn(VoiceEvent)>;

/// A cheaply-cloneable handle to the shared voice-note player. Store one on the
/// chat view and clone it into each voice bubble's controls.
#[derive(Clone)]
pub struct VoicePlayer {
    inner: Rc<Inner>,
}

struct Inner {
    play: gstplay::Play,
    /// Token of the bubble that currently owns the pipeline (0 == none).
    owner: Cell<u64>,
    /// Monotonic token allocator; each `start()` mints the next token.
    next_token: Cell<u64>,
    /// The current owner's event callback (replaced on each `start()`).
    cb: RefCell<Option<EventCallback>>,
    /// Last-seen playing state from the bus (drives `toggle`).
    playing: Cell<bool>,
    /// Keeps the bus watch alive; dropping it removes the watch.
    _bus_watch: RefCell<Option<gst::bus::BusWatchGuard>>,
}

impl VoicePlayer {
    /// Build the shared player: create the audio-only pipeline, configure a
    /// 200ms position-update cadence, and attach a main-thread bus watch that
    /// forwards playback messages to the current owner's callback.
    pub fn new() -> Self {
        ensure_init();

        // Audio-only playback: pass `None` typed as a `PlayVideoRenderer` so
        // the pipeline is built without any video sink.
        let play = gstplay::Play::new(None::<gstplay::PlayVideoRenderer>);

        // Emit `PositionUpdated` roughly every 200ms so the UI seek bar moves
        // smoothly without flooding the main thread.
        let mut config = play.config();
        config.set_position_update_interval(200);
        let _ = play.set_config(config);

        let inner = Rc::new(Inner {
            play,
            owner: Cell::new(0),
            next_token: Cell::new(0),
            cb: RefCell::new(None),
            playing: Cell::new(false),
            _bus_watch: RefCell::new(None),
        });

        let bus = inner.play.message_bus();
        bus.set_flushing(false);

        // The watch closure captures the pipeline's `Inner`, but the returned
        // guard is stored *inside* that same `Inner`. Capturing a strong `Rc`
        // would form a self-sustaining reference cycle (guard -> closure ->
        // Inner -> guard) that never drops. Capture a `Weak` instead and
        // upgrade per message; when the last strong handle goes away the watch
        // simply becomes a no-op.
        let weak = Rc::downgrade(&inner);
        let guard = bus
            .add_watch_local(move |_, msg| {
                let Some(inner) = weak.upgrade() else {
                    return glib::ControlFlow::Continue;
                };

                if !gstplay::Play::is_play_message(msg) {
                    return glib::ControlFlow::Continue;
                }

                match gstplay::PlayMessage::parse(msg) {
                    Ok(gstplay::PlayMessage::DurationChanged(dc)) => {
                        let ns = dc.duration().map(|d| d.nseconds()).unwrap_or(0);
                        dispatch(&inner, VoiceEvent::Duration(ns));
                    }
                    Ok(gstplay::PlayMessage::PositionUpdated(pu)) => {
                        let ns = pu.position().map(|p| p.nseconds()).unwrap_or(0);
                        dispatch(&inner, VoiceEvent::Position(ns));
                    }
                    Ok(gstplay::PlayMessage::StateChanged(sc)) => {
                        let playing = sc.state() == gstplay::PlayState::Playing;
                        inner.playing.set(playing);
                        dispatch(&inner, VoiceEvent::Playing(playing));
                    }
                    Ok(gstplay::PlayMessage::EndOfStream(_)) => {
                        inner.play.stop();
                        inner.playing.set(false);
                        // Notify the owner *before* clearing ownership so its
                        // `Ended` handler still runs against the live callback.
                        dispatch(&inner, VoiceEvent::Ended);
                        clear_owner(&inner);
                    }
                    Ok(gstplay::PlayMessage::Error(err)) => {
                        tracing::warn!(error = %err.error(), "voice playback error");
                        inner.play.stop();
                        inner.playing.set(false);
                        dispatch(&inner, VoiceEvent::Ended);
                        clear_owner(&inner);
                    }
                    _ => {}
                }

                glib::ControlFlow::Continue
            })
            .ok();
        *inner._bus_watch.borrow_mut() = guard;

        Self { inner }
    }

    /// Take ownership of the pipeline to play `uri`, delivering progress/state
    /// to `on_event`. Returns a token identifying this playback; pass it to the
    /// other methods so a recycled row can tell whether it still owns the
    /// pipeline. Any previous owner is notified with [`VoiceEvent::Ended`] so it
    /// can reset its UI.
    pub fn start<F>(&self, uri: &str, on_event: F) -> u64
    where
        F: Fn(VoiceEvent) + 'static,
    {
        // Evict the previous owner: take its callback out and tell it that it
        // lost the pipeline so the old bubble resets to a stopped state. Take
        // before invoking so the callback can't observe a half-updated player.
        let prev = self.inner.cb.borrow_mut().take();
        if let Some(prev) = prev {
            prev(VoiceEvent::Ended);
        }

        // Mint a fresh token for this playback.
        let token = self.inner.next_token.get() + 1;
        self.inner.next_token.set(token);

        self.inner.owner.set(token);
        *self.inner.cb.borrow_mut() = Some(Box::new(on_event));
        self.inner.playing.set(false);

        self.inner.play.set_uri(Some(uri));
        self.inner.play.play();

        token
    }

    /// Toggle play/pause for `token`, but only if it still owns the pipeline.
    /// A stale token (a recycled row) is ignored.
    pub fn toggle(&self, token: u64) {
        if self.inner.owner.get() == token {
            if self.inner.playing.get() {
                self.inner.play.pause();
            } else {
                self.inner.play.play();
            }
        }
    }

    /// Seek `token`'s playback to `ns` nanoseconds, if it still owns the
    /// pipeline.
    pub fn seek(&self, token: u64, ns: u64) {
        if self.inner.owner.get() == token {
            self.inner.play.seek(gst::ClockTime::from_nseconds(ns));
        }
    }

    /// Stop and detach playback iff `token` currently owns the pipeline. Used
    /// when a list row is being recycled/torn down. Emits no event: the caller
    /// is discarding the bubble, so there is nothing to update.
    pub fn stop_if_owner(&self, token: u64) {
        if self.inner.owner.get() == token {
            self.inner.play.stop();
            self.inner.play.set_uri(None);
            self.inner.playing.set(false);
            clear_owner(&self.inner);
        }
    }

    /// Unconditionally stop and detach any playback (e.g. when the chat is
    /// closed). Emits no event.
    pub fn stop(&self) {
        self.inner.play.stop();
        self.inner.play.set_uri(None);
        self.inner.playing.set(false);
        clear_owner(&self.inner);
    }

    /// Whether `token` currently owns the pipeline.
    pub fn is_owner(&self, token: u64) -> bool {
        self.inner.owner.get() == token
    }
}

impl Default for VoicePlayer {
    fn default() -> Self {
        Self::new()
    }
}

/// Fire the current owner's callback, if one is registered. The borrow is
/// dropped before the callback runs so the callback may re-enter the player.
fn dispatch(inner: &Rc<Inner>, event: VoiceEvent) {
    // Hold a shared borrow only for the call; it is released when this fn
    // returns, so callers (e.g. the EndOfStream path) can `borrow_mut` the
    // callback in `clear_owner` immediately afterwards without conflict.
    let cb = inner.cb.borrow();
    if let Some(cb) = cb.as_ref() {
        cb(event);
    }
}

/// Drop the current owner: clears the callback and resets ownership to none.
fn clear_owner(inner: &Rc<Inner>) {
    inner.owner.set(0);
    *inner.cb.borrow_mut() = None;
}

/// Build a `file://` URI for a local path, for [`VoicePlayer::start`]. Returns
/// `None` if the path can't be represented as a URI.
pub fn file_uri(path: &std::path::Path) -> Option<String> {
    glib::filename_to_uri(path, None).ok().map(|g| g.to_string())
}

/// A cheaply-cloneable single-slot voice-note recorder.
///
/// Captures the default mic to a temp OGG/Opus file via a GStreamer pipeline.
/// The OGG muxer MUST be flushed by pushing EOS and waiting for it to propagate
/// *before* setting the pipeline to Null; otherwise the OGG is truncated and
/// invalid. A missing or unavailable microphone makes [`VoiceRecorder::start`]
/// return `Err` rather than panicking, so the caller can fall back gracefully.
#[derive(Clone)]
pub struct VoiceRecorder {
    inner: Rc<RecInner>,
}

struct RecInner {
    pipeline: RefCell<Option<gst::Pipeline>>,
    path: RefCell<Option<std::path::PathBuf>>,
    started: Cell<Option<std::time::Instant>>,
}

impl VoiceRecorder {
    /// Build an idle recorder (initializes GStreamer once for the process).
    pub fn new() -> Self {
        ensure_init();
        Self {
            inner: Rc::new(RecInner {
                pipeline: RefCell::new(None),
                path: RefCell::new(None),
                started: Cell::new(None),
            }),
        }
    }

    /// Start capturing to a fresh temp OGG/Opus file. Returns `Err` (leaving the
    /// recorder idle) if the pipeline can't be built or started — e.g. no mic.
    pub fn start(&self) -> Result<(), String> {
        let dir = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let filename = format!("paloma-voice-{millis}.ogg");
        let path = dir.join(filename);
        let description = format!(
            "autoaudiosrc ! audioconvert ! audioresample ! opusenc ! oggmux ! filesink location=\"{}\"",
            path.display()
        );

        use gst::prelude::*;
        let element = gst::parse::launch(&description).map_err(|e| e.to_string())?;
        let pipeline = element
            .dynamic_cast::<gst::Pipeline>()
            .map_err(|_| "not a pipeline".to_string())?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| e.to_string())?;

        *self.inner.pipeline.borrow_mut() = Some(pipeline);
        *self.inner.path.borrow_mut() = Some(path);
        self.inner.started.set(Some(std::time::Instant::now()));
        Ok(())
    }

    /// Elapsed recording time in whole seconds (a minimum of 1 while recording,
    /// 0 when idle).
    pub fn duration_secs(&self) -> i32 {
        self.inner
            .started
            .get()
            .map(|t| {
                let s = t.elapsed().as_secs();
                if s == 0 { 1 } else { s as i32 }
            })
            .unwrap_or(0)
    }

    /// Whether a recording is currently in progress.
    #[allow(dead_code)]
    pub fn is_recording(&self) -> bool {
        self.inner.pipeline.borrow().is_some()
    }

    /// Stop and finalize the recording, returning the file path and its duration
    /// in seconds. Flushes the muxer via EOS-then-wait before tearing down so the
    /// OGG is valid. Returns `None` if not recording.
    pub fn stop(&self) -> Option<(std::path::PathBuf, i32)> {
        use gst::prelude::*;
        let pipeline = self.inner.pipeline.borrow_mut().take()?;
        let duration = self.duration_secs();
        let bus = pipeline.bus();
        let _ = pipeline.send_event(gst::event::Eos::new());
        if let Some(bus) = bus {
            let _ = bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(3),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            );
        }
        let _ = pipeline.set_state(gst::State::Null);
        let path = self.inner.path.borrow_mut().take();
        self.inner.started.set(None);
        path.map(|p| (p, duration))
    }

    /// Abort the recording and delete the temp file, leaving the recorder idle.
    pub fn cancel(&self) {
        use gst::prelude::*;
        if let Some(pipeline) = self.inner.pipeline.borrow_mut().take() {
            let _ = pipeline.set_state(gst::State::Null);
        }
        if let Some(path) = self.inner.path.borrow_mut().take() {
            let _ = std::fs::remove_file(&path);
        }
        self.inner.started.set(None);
    }
}

impl Default for VoiceRecorder {
    fn default() -> Self {
        Self::new()
    }
}
