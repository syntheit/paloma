//! [`ChatView`]: the message-history pane for one open chat.
//!
//! # Lifecycle (mirrors tgt's `tg_backend.rs` open-chat flow)
//!
//! On [`ChatView::open`] we, in order:
//! 1. `functions::open_chat(chat_id)` — REQUIRED. TDLib only streams message
//!    updates (`updateNewMessage`, read state, `updateMessageContent`, …) and
//!    keeps a chat's message cache warm while it is *open*.
//! 2. `functions::get_chat_history(...)` to page in the most recent messages.
//!
//! On [`ChatView::close`] we `functions::close_chat(chat_id)`.
//!
//! # Recycling model
//!
//! A `gtk::ListView` over a `gio::ListStore` of [`MessageObject`]s, driven by a
//! `SignalListItemFactory`. GTK reuses a small pool of bubble widgets — one per
//! visible slot, not one per message. Mutating a live [`MessageObject`] and then
//! firing `items_changed` for its row re-binds only that slot.
//!
//! # Wave 3 additions
//!
//! * **Avatars**: incoming rows carry an [`adw::Avatar`]; the group's last row
//!   shows the sender's photo/initials, the rest reserve the space.
//! * **Photos**: `MessagePhoto` rows render a rounded [`gtk::Picture`] downloaded
//!   via the shared [`crate::tdlib::FileStore`]; tap opens a full viewer dialog.
//! * **Replies**: a reply target renders a quoted header inside the bubble; a
//!   "Reply" action arms a compose reply-strip and threads
//!   `InputMessageReplyTo::Message` into `send_message`.
//! * **Context menu + delete**: right-click / long-press → Reply / Copy / Delete
//!   (with an `AlertDialog` confirmation and revoke-for-everyone when allowed).
//! * **Grouping**: consecutive messages from the same sender within a short
//!   window hide the repeated name and share one trailing avatar.

use adw::prelude::*;
use gtk::gio;
use gtk::glib;
use gtk::glib::clone;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use tdlib_rs::enums::{
    InputFile, InputMessageContent, InputMessageReplyTo, Message as MessageEnum,
    MessageProperties as MessagePropsEnum, Messages, MessageSender, Update, User as UserEnum,
};
use tdlib_rs::functions;
use tdlib_rs::types::{
    FormattedText, InputFileLocal, InputMessageReplyToMessage, InputMessageText,
    InputMessageVoiceNote,
};

use crate::models::message_object::{decode_reactions, kind, send_status, send_status_glyph};
use crate::models::MessageObject;
use crate::tdlib::{FileStore, TdClient};
use crate::audio::{file_uri, VoiceEvent, VoicePlayer, VoiceRecorder};

/// Messages sent within this many seconds of each other by the same sender are
/// visually grouped (name hidden, one trailing avatar).
const GROUP_WINDOW_SECS: i64 = 300;

/// Pixel size of the per-message sender avatar shown in group chats.
const AVATAR_SIZE: i32 = 30;

/// The message-history component for a single chat. Cheap to `.clone()` — the
/// widget tree and all state live behind the shared `Rc<Inner>`.
#[derive(Clone)]
pub struct ChatView {
    root: gtk::Widget,
    inner: Rc<Inner>,
}

struct Inner {
    client: TdClient,
    files: FileStore,
    /// Shared single-stream voice-note player (one playback at a time).
    voice: VoicePlayer,
    /// Single-slot voice-note recorder for the compose bar.
    recorder: VoiceRecorder,
    /// True while a voice note is actively being recorded.
    recording: Cell<bool>,
    /// Source id of the 1s recording-timer tick, cleared when recording ends.
    rec_timer_id: RefCell<Option<glib::SourceId>>,
    /// The normal compose row (entry + send/mic), hidden while recording.
    compose_row: gtk::Box,
    /// The recording row (dot + timer + discard/send), shown while recording.
    recording_row: gtk::Box,
    /// Mic button in the compose row's trailing slot (shown when entry empty).
    mic_button: gtk::Button,
    /// Send button in the compose row's trailing slot (shown when entry has text).
    send_button: gtk::Button,
    /// Elapsed M:SS label shown in the recording row.
    rec_timer_label: gtk::Label,
    chat_id: i64,
    /// Our own user id, resolved lazily via `get_me`, to distinguish grouping.
    me_id: Cell<i64>,
    /// True once we've learned this chat is a basic group / supergroup; sender
    /// names are only shown for groups.
    is_group: Cell<bool>,
    /// The model backing the `ListView`; oldest message first, newest last.
    store: gio::ListStore,
    /// `message_id` → live [`MessageObject`] for O(1) updates.
    index: RefCell<HashMap<i64, MessageObject>>,
    /// Resolved sender display names, `user_id` → name, to label group messages.
    names: RefCell<HashMap<i64, String>>,
    /// Oldest loaded message id, used as `from_message_id` when paging older
    /// history at the top. `0` before the first batch.
    oldest_id: Cell<i64>,
    /// True once history paging has hit the top (no more older messages).
    reached_top: Cell<bool>,
    /// Guards against overlapping "load older" requests while one is in flight.
    loading_older: Cell<bool>,
    /// The message id we're composing a reply to, or 0 for a normal message.
    reply_to: Cell<i64>,
    /// The message id we're currently editing, or 0 when not editing.
    editing: Cell<i64>,
    /// Generation counter bumped on EVERY compose-state transition (enter/exit
    /// edit, start/clear reply). An async permission check captures the current
    /// value before awaiting and bails on resolve if it changed — so a stale
    /// `get_message_properties` result can't arm edit mode over a newer state.
    compose_gen: Cell<u64>,
    /// The overlay wrapping the message `ScrolledWindow`. Same size as the chat
    /// viewport and does NOT scroll — the host for the anchored context menu
    /// (and the scroll-to-bottom button).
    overlay: gtk::Overlay,
    /// The currently-open anchored context menu, `(catcher, menu_box)`, or
    /// `None`. Tracked so a new menu dismisses any prior one, and so dismissal
    /// removes exactly the widgets that were added to the overlay.
    open_menu: RefCell<Option<(gtk::Widget, gtk::Widget)>>,
    list_view: gtk::ListView,
    scroller: gtk::ScrolledWindow,
    entry: gtk::TextView,
    /// The chat's header title/subtitle. Title is the chat name (set by app.rs
    /// via `set_title`); subtitle carries the live "typing…" status.
    header_title: adw::WindowTitle,
    /// Reply-preview strip shown above the compose entry while replying.
    reply_bar: gtk::Revealer,
    reply_bar_name: gtk::Label,
    reply_bar_text: gtk::Label,
    /// Edit-preview strip shown above the compose entry while editing a message.
    edit_bar: gtk::Revealer,
    edit_bar_text: gtk::Label,
    /// Toast host over the history, for transient confirmations (e.g. forwards).
    toasts: adw::ToastOverlay,
    /// The subscription loop task; aborted in `close()` so it drops its
    /// receiver and the strong `ChatView` it captures (else the view leaks).
    sub_task: RefCell<Option<glib::JoinHandle<()>>>,
    /// Temporary (pending) message ids awaiting `MessageSendSucceeded`/`Failed`,
    /// used to dedup the `NewMessage` TDLib echoes for our own outgoing sends.
    temp_ids: RefCell<HashSet<i64>>,
    /// The chat's `last_read_outbox_message_id` (from `get_chat` / kept fresh by
    /// `updateChatReadOutbox`): every OUTGOING message with `id <=` this has been
    /// read by the recipient. Drives the double-check indicator. `0` until the
    /// first `get_chat` resolves.
    last_read_outbox: Cell<i64>,
    /// Unix seconds of the last `send_chat_action(Typing)` we sent, to throttle
    /// the outgoing typing cadence to ~one signal per 5s. `0` before any send.
    last_typing_sent: Cell<i64>,
    /// Generation counter for the typing-clear timeout: each incoming Typing
    /// action bumps it and arms a fresh 6s clear; only the latest-armed timeout
    /// actually clears, so a refreshing peer never prematurely blanks the status.
    typing_gen: Cell<u64>,
    /// In-flight reaction toggles keyed by `(message_id, emoji)`. A second
    /// toggle for the same key while the first request is outstanding is
    /// dropped, so a rapid double-tap can't fire add+add (or race add/remove).
    reaction_inflight: RefCell<HashSet<(i64, String)>>,
}

impl ChatView {
    /// Build (but do not yet open) a chat view for `chat_id`.
    /// Call [`ChatView::open`] once it is on screen to start streaming.
    pub fn new(client: TdClient, chat_id: i64) -> Self {
        let files = client.files();
        let store = gio::ListStore::new::<MessageObject>();
        let voice = VoicePlayer::new();

        // --- Row factory: one recycled row per visible slot. -----------------
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(build_row);

        let this_store = store.clone();
        let bind_files = files.clone();
        let bind_voice = voice.clone();
        factory.connect_bind(move |_, list_item| {
            bind_row(list_item, &this_store, &bind_files, &bind_voice);
        });

        // Tear down the per-row reactive notify handlers when a row leaves the
        // viewport into the recycle pool. Without this, the stashed
        // (MessageObject, SignalHandlerId) keeps the handler live between unbind
        // and the next bind, holding the widget + ListItem alive and letting
        // stale notifications fire into a pooled row. Runs on EVERY unbind.
        factory.connect_unbind(|_, list_item| {
            let list_item = list_item
                .downcast_ref::<gtk::ListItem>()
                .expect("list item is a ListItem");
            let root = list_item
                .child()
                .and_downcast::<gtk::Box>()
                .expect("row child is a Box")
                .upcast::<gtk::Widget>();
            if let Some(avatar) = find::<adw::Avatar>(&root, "avatar") {
                clear_avatar_notify(&avatar);
            }
            if let Some(chips) = find::<gtk::Box>(&root, "reactions") {
                clear_reactions_notify(&chips);
            }
            if let Some(body) = find::<gtk::Label>(&root, "body") {
                clear_content_notify(&body);
            }
        });

        let selection = gtk::NoSelection::new(Some(store.clone()));
        let list_view = gtk::ListView::new(Some(selection), Some(factory));
        list_view.add_css_class("msg-list");

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list_view)
            .build();

        // --- Reply-preview strip (revealed while composing a reply). ---------
        let reply_bar_name = gtk::Label::builder()
            .css_classes(["reply-bar-name"])
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .single_line_mode(true)
            .build();
        let reply_bar_text = gtk::Label::builder()
            .css_classes(["reply-bar-text", "dim-label"])
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .single_line_mode(true)
            .build();
        let reply_bar_labels = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        reply_bar_labels.append(&reply_bar_name);
        reply_bar_labels.append(&reply_bar_text);

        let reply_cancel = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .tooltip_text("Cancel reply")
            .build();

        let reply_inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_top(4)
            .margin_bottom(4)
            .margin_start(8)
            .margin_end(8)
            .build();
        reply_inner.add_css_class("reply-bar");
        reply_inner.append(&reply_bar_labels);
        reply_inner.append(&reply_cancel);

        let reply_bar = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .reveal_child(false)
            .child(&reply_inner)
            .build();

        // --- Edit-preview strip (revealed while editing a message). ----------
        let edit_bar_name = gtk::Label::builder()
            .css_classes(["reply-bar-name"])
            .label("Editing")
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .single_line_mode(true)
            .build();
        let edit_bar_text = gtk::Label::builder()
            .css_classes(["reply-bar-text", "dim-label"])
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .single_line_mode(true)
            .build();
        let edit_bar_labels = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        edit_bar_labels.append(&edit_bar_name);
        edit_bar_labels.append(&edit_bar_text);

        let edit_cancel = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .tooltip_text("Cancel edit")
            .build();

        let edit_inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_top(4)
            .margin_bottom(4)
            .margin_start(8)
            .margin_end(8)
            .build();
        edit_inner.add_css_class("reply-bar");
        edit_inner.append(&edit_bar_labels);
        edit_inner.append(&edit_cancel);

        let edit_bar = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .reveal_child(false)
            .child(&edit_inner)
            .build();

        // --- Compose bar: a growing TextView + a send button. ----------------
        let entry = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .top_margin(6)
            .bottom_margin(6)
            .left_margin(8)
            .right_margin(8)
            .build();
        entry.add_css_class("msg-entry");

        let entry_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .max_content_height(120)
            .propagate_natural_height(true)
            .hexpand(true)
            .child(&entry)
            .build();
        entry_scroll.add_css_class("msg-entry-scroll");

        let send_button = gtk::Button::builder()
            .icon_name("document-send-symbolic")
            .valign(gtk::Align::End)
            .css_classes(["circular", "suggested-action", "msg-send"])
            .tooltip_text("Send")
            .build();

        let mic_button = gtk::Button::builder()
            .icon_name("audio-input-microphone-symbolic")
            .valign(gtk::Align::End)
            .css_classes(["circular", "flat", "msg-mic"])
            .tooltip_text("Record voice message")
            .build();

        let compose_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(8)
            .margin_end(8)
            .build();
        compose_row.append(&entry_scroll);
        compose_row.append(&send_button);
        compose_row.append(&mic_button);
        send_button.set_visible(false);
        mic_button.set_visible(true);

        // Recording row (hidden until the mic button starts a recording).
        let rec_dot = gtk::Box::builder()
            .css_classes(["rec-dot"])
            .valign(gtk::Align::Center)
            .build();
        let rec_timer_label = gtk::Label::builder()
            .css_classes(["rec-timer"])
            .label("0:00")
            .xalign(0.0)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        let rec_cancel = gtk::Button::builder()
            .icon_name("edit-delete-symbolic")
            .valign(gtk::Align::Center)
            .css_classes(["circular", "flat"])
            .tooltip_text("Discard")
            .build();
        let rec_send = gtk::Button::builder()
            .icon_name("document-send-symbolic")
            .valign(gtk::Align::Center)
            .css_classes(["circular", "suggested-action"])
            .tooltip_text("Send voice message")
            .build();
        let recording_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(8)
            .margin_end(8)
            .build();
        recording_row.add_css_class("msg-recording");
        recording_row.append(&rec_dot);
        recording_row.append(&rec_timer_label);
        recording_row.append(&rec_cancel);
        recording_row.append(&rec_send);
        recording_row.set_visible(false);

        // Reply strip stacked above the compose row.
        let compose = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        compose.add_css_class("msg-compose");
        compose.append(&reply_bar);
        compose.append(&edit_bar);
        compose.append(&compose_row);
        compose.append(&recording_row);

        // Floating "scroll to newest" button overlaid on the history, shown when
        // the user has scrolled up away from the bottom.
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&scroller));
        let scroll_down = gtk::Button::builder()
            .icon_name("go-down-symbolic")
            .css_classes(["osd", "circular"])
            .halign(gtk::Align::End)
            .valign(gtk::Align::End)
            .margin_end(12)
            .margin_bottom(12)
            .tooltip_text("Scroll to latest")
            .build();
        scroll_down.set_visible(false);
        overlay.add_overlay(&scroll_down);

        // Toast host wrapping the message history (transient confirmations).
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&overlay));

        // History fills the space; compose bar pinned at the bottom.
        let toolbar = adw::ToolbarView::new();

        // Own header: a WindowTitle whose title is the chat name (set by app.rs)
        // and whose subtitle carries the live "typing…" status.
        let header_title = adw::WindowTitle::new("", "");
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&header_title));
        toolbar.add_top_bar(&header);

        toolbar.set_content(Some(&toasts));
        toolbar.add_bottom_bar(&compose);

        let inner = Rc::new(Inner {
            client,
            files,
            voice,
            recorder: VoiceRecorder::new(),
            recording: Cell::new(false),
            rec_timer_id: RefCell::new(None),
            compose_row: compose_row.clone(),
            recording_row: recording_row.clone(),
            mic_button: mic_button.clone(),
            send_button: send_button.clone(),
            rec_timer_label: rec_timer_label.clone(),
            chat_id,
            me_id: Cell::new(0),
            is_group: Cell::new(false),
            store,
            index: RefCell::new(HashMap::new()),
            names: RefCell::new(HashMap::new()),
            oldest_id: Cell::new(0),
            reached_top: Cell::new(false),
            loading_older: Cell::new(false),
            reply_to: Cell::new(0),
            editing: Cell::new(0),
            compose_gen: Cell::new(0),
            overlay: overlay.clone(),
            open_menu: RefCell::new(None),
            list_view,
            scroller,
            entry,
            header_title,
            reply_bar,
            reply_bar_name,
            reply_bar_text,
            edit_bar,
            edit_bar_text,
            toasts,
            sub_task: RefCell::new(None),
            temp_ids: RefCell::new(HashSet::new()),
            last_read_outbox: Cell::new(0),
            last_typing_sent: Cell::new(0),
            typing_gen: Cell::new(0),
            reaction_inflight: RefCell::new(HashSet::new()),
        });

        let this = ChatView {
            root: toolbar.upcast(),
            inner,
        };

        this.wire_send(&send_button);
        this.wire_typing_send();
        this.wire_compose_toggle();
        this.wire_voice_record(&mic_button, &rec_send, &rec_cancel);
        this.wire_scroll_paging();
        this.wire_scroll_button(&scroll_down);
        this.wire_row_menu();
        {
            let this2 = this.clone();
            reply_cancel.connect_clicked(move |_| this2.clear_reply());
        }
        {
            let this2 = this.clone();
            edit_cancel.connect_clicked(move |_| this2.cancel_edit());
        }
        this
    }

    /// The root widget to embed as the split-view content / a nav page child.
    pub fn widget(&self) -> &gtk::Widget {
        &self.root
    }

    /// The id of the chat this view renders.
    pub fn chat_id(&self) -> i64 {
        self.inner.chat_id
    }

    /// Set the chat title shown in this view's header (subtitle is managed
    /// internally by the typing-indicator logic).
    pub fn set_title(&self, title: &str) {
        self.inner.header_title.set_title(title);
    }

    /// Begin streaming: `open_chat`, resolve our own id, subscribe to updates,
    /// and page in the most recent history.
    pub fn open(&self) {
        let cid = self.inner.client.client_id();
        let chat_id = self.inner.chat_id;

        crate::runtime::spawn(
            async move { functions::open_chat(chat_id, cid).await },
            |res| {
                if let Err(e) = res {
                    tracing::warn!(code = e.code, msg = %e.message, "open_chat failed");
                }
            },
        );

        let this = self.clone();
        crate::runtime::spawn(
            async move { functions::get_me(cid).await },
            move |res| {
                if let Ok(UserEnum::User(me)) = res {
                    this.inner.me_id.set(me.id);
                }
            },
        );

        let this = self.clone();
        crate::runtime::spawn(
            async move { functions::get_chat(chat_id, cid).await },
            move |res| {
                if let Ok(tdlib_rs::enums::Chat::Chat(chat)) = res {
                    let is_group = matches!(
                        chat.r#type,
                        tdlib_rs::enums::ChatType::BasicGroup(_)
                            | tdlib_rs::enums::ChatType::Supergroup(_)
                    );
                    this.inner.is_group.set(is_group);
                    // Capture the recipient's read cursor so already-read
                    // outgoing messages show a double-check on first paint.
                    this.inner
                        .last_read_outbox
                        .set(chat.last_read_outbox_message_id);
                    this.refresh_read_state(chat.last_read_outbox_message_id);
                    if is_group {
                        // Names weren't resolved while we thought this was a 1:1
                        // chat; resolve them now that we know it's a group.
                        this.resolve_all_sender_names();
                    }
                    // Re-bind so sender names appear/disappear per group-ness.
                    this.rebind_all();
                }
            },
        );

        let updates = self.inner.client.subscribe();
        let this = self.clone();
        let handle = glib::spawn_future_local(async move {
            while let Ok(update) = updates.recv().await {
                this.handle_update(update);
            }
        });
        // Keep the handle so `close()` can abort the loop; aborting drops the
        // future, its captured strong `ChatView`, and the `updates` receiver.
        *self.inner.sub_task.borrow_mut() = Some(handle);

        self.load_initial_history();
    }

    /// Stop streaming for this chat. Call when the view is popped/replaced.
    pub fn close(&self) {
        // Halt any in-progress voice playback so switching chats stops audio.
        self.inner.voice.stop();
        // Abort the update loop so it drops its receiver and the strong
        // `ChatView` it holds — otherwise this view leaks on every chat switch.
        if let Some(handle) = self.inner.sub_task.borrow_mut().take() {
            handle.abort();
        }

        let cid = self.inner.client.client_id();
        let chat_id = self.inner.chat_id;
        crate::runtime::spawn(
            async move { functions::close_chat(chat_id, cid).await },
            |res| {
                if let Err(e) = res {
                    tracing::warn!(code = e.code, msg = %e.message, "close_chat failed");
                }
            },
        );
    }

    /// Load the most recent history. TDLib returns only ~1 message for a
    /// cold-cache `from_message_id=0` request, so — like tgt — we prime once
    /// then loop, paging from the running oldest id, until we've accumulated a
    /// real backlog (or a batch comes back empty). The whole accumulation is
    /// then ingested as the single initial batch (newest at the bottom).
    fn load_initial_history(&self) {
        // Target size of the initial backlog to show on open.
        const TARGET: usize = 40;
        // Per-request batch size.
        const BATCH: i32 = 50;

        let cid = self.inner.client.client_id();
        let chat_id = self.inner.chat_id;
        let this = self.clone();
        crate::runtime::spawn(
            async move {
                // Prime TDLib's message cache; the result is intentionally
                // discarded (cold chats answer this with ~1 message).
                let _ = functions::get_chat_history(chat_id, 0, 0, 100, false, cid).await;

                // Page from the oldest id we hold, oldest-anchored, until we
                // have TARGET messages or a batch returns empty.
                let mut acc: Vec<tdlib_rs::types::Message> = Vec::new();
                let mut from: i64 = 0;
                loop {
                    match functions::get_chat_history(chat_id, from, 0, BATCH, false, cid).await {
                        Ok(Messages::Messages(m)) => {
                            let batch: Vec<tdlib_rs::types::Message> =
                                m.messages.into_iter().flatten().collect();
                            if batch.is_empty() {
                                break;
                            }
                            // Anchor the next page at the oldest id in this batch
                            // (TDLib returns newest-first).
                            from = batch.iter().map(|msg| msg.id).min().unwrap_or(0);
                            acc.extend(batch);
                            if acc.len() >= TARGET || from == 0 {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(code = e.code, msg = %e.message, "get_chat_history (initial) failed");
                            break;
                        }
                    }
                }
                acc
            },
            move |msgs| {
                // `ingest_history` takes Vec<Option<Message>>; wrap them.
                let wrapped: Vec<Option<tdlib_rs::types::Message>> =
                    msgs.into_iter().map(Some).collect();
                this.ingest_history(wrapped, true);
            },
        );
    }

    /// Page in an older batch, anchored at the oldest loaded id.
    fn load_older_history(&self) {
        if self.inner.reached_top.get() || self.inner.loading_older.get() {
            return;
        }
        let from = self.inner.oldest_id.get();
        if from == 0 {
            return;
        }
        self.inner.loading_older.set(true);

        let cid = self.inner.client.client_id();
        let chat_id = self.inner.chat_id;
        let this = self.clone();
        crate::runtime::spawn(
            async move { functions::get_chat_history(chat_id, from, 0, 50, false, cid).await },
            move |res| match res {
                Ok(Messages::Messages(msgs)) if !msgs.messages.is_empty() => {
                    // Non-empty: ingest; the scroll-restore idle in
                    // `ingest_history` clears `loading_older` once it runs, so
                    // the programmatic set_value can't re-trigger this loop.
                    this.ingest_history(msgs.messages, false);
                }
                Ok(Messages::Messages(_)) => {
                    // Empty older batch: reached top; clear the guard here since
                    // no restore idle runs on the empty path.
                    this.ingest_history(Vec::new(), false); // sets reached_top
                    this.inner.loading_older.set(false);
                }
                Err(e) => {
                    tracing::warn!(code = e.code, msg = %e.message, "get_chat_history (older) failed");
                    this.inner.loading_older.set(false);
                }
            },
        );
    }

    /// Insert a history batch (TDLib returns newest-first; our store is
    /// oldest-first, so we reverse).
    fn ingest_history(&self, messages: Vec<Option<tdlib_rs::types::Message>>, is_initial: bool) {
        let batch: Vec<tdlib_rs::types::Message> = messages.into_iter().flatten().collect();
        if batch.is_empty() {
            // Only an empty *older* batch means we've hit the top. An empty
            // initial batch (e.g. a cold priming call) must not disable paging.
            if !is_initial {
                self.inner.reached_top.set(true);
            }
            return;
        }

        let mut ordered = batch;
        ordered.sort_by_key(|m| m.id);

        if let Some(min) = ordered.first().map(|m| m.id) {
            let cur = self.inner.oldest_id.get();
            if cur == 0 || min < cur {
                self.inner.oldest_id.set(min);
            }
        }

        let vadj = self.inner.scroller.vadjustment();
        let old_upper = vadj.upper();
        let old_value = vadj.value();

        let mut to_resolve: Vec<i64> = Vec::new();
        let mut inserted: Vec<i64> = Vec::new();
        for msg in &ordered {
            if self.inner.index.borrow().contains_key(&msg.id) {
                continue;
            }
            let obj = MessageObject::from_message(msg);
            self.apply_sender_name(&obj, &msg.sender_id, &mut to_resolve);
            self.insert_sorted(&obj);
            self.inner.index.borrow_mut().insert(msg.id, obj);
            inserted.push(msg.id);
        }

        self.resolve_names(to_resolve);
        self.resolve_replies(&inserted);

        if is_initial {
            // The initial `get_chat` may have resolved BEFORE any messages were
            // ingested, so its `refresh_read_state` ran over an empty index and
            // promoted nothing. Now that the initial batch is in `self.index`,
            // re-run it against the stored `last_read_outbox` cursor so already-
            // read outgoing messages show a double-check on first paint. (The
            // cursor is set by the get_chat callback independently of its own
            // refresh, so it's populated even when that refresh found nothing.)
            self.refresh_read_state(self.inner.last_read_outbox.get());
            // Grouping flags depend on neighbours; re-bind the whole loaded run.
            self.rebind_all();
            self.scroll_to_bottom();
            self.mark_visible_read(&ordered);
        } else {
            // Older pages are almost always already read by the recipient;
            // promote them now that they're in the index (the property binding
            // repaints their indicator live, no extra rebind needed).
            self.refresh_read_state(self.inner.last_read_outbox.get());
            // A prepend inserts a contiguous run at the top (positions
            // 0..inserted.len()). Re-bind only that run plus the one boundary
            // row below it — a full invalidate here fights scroll-restore.
            let count = (inserted.len() as u32)
                .saturating_add(1)
                .min(self.inner.store.n_items());
            if count > 0 {
                self.inner.store.items_changed(0, count, count);
            }
            let scroller = self.inner.scroller.clone();
            let inner = self.inner.clone();
            glib::idle_add_local_once(move || {
                let vadj = scroller.vadjustment();
                let new_upper = vadj.upper();
                let delta = new_upper - old_upper;
                vadj.set_value(old_value + delta);
                // Clear the load-older guard only after the anchor is restored,
                // so the programmatic set_value above can't re-trigger paging.
                inner.loading_older.set(false);
            });
        }
    }

    /// Insert `obj` into the store keeping ascending message-id order.
    fn insert_sorted(&self, obj: &MessageObject) -> u32 {
        let store = &self.inner.store;
        let id = obj.id();
        let n = store.n_items();
        let mut lo = 0u32;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let mid_id = store
                .item(mid)
                .and_downcast::<MessageObject>()
                .map(|m| m.id())
                .unwrap_or(i64::MAX);
            if mid_id < id {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        store.insert(lo, obj);
        lo
    }

    /// Route one raw update, acting only on our open chat.
    fn handle_update(&self, update: Update) {
        match update {
            Update::NewMessage(u) if u.message.chat_id == self.inner.chat_id => {
                let id = u.message.id;
                // TDLib also echoes our own outgoing sends as NewMessage; skip
                // ids we already hold or are tracking as optimistic temps.
                if self.inner.index.borrow().contains_key(&id)
                    || self.inner.temp_ids.borrow().contains(&id)
                {
                    if let Some(obj) = self.inner.index.borrow().get(&id).cloned() {
                        obj.update_from_message(&u.message);
                        self.notify_changed(id);
                    }
                    return;
                }
                let obj = MessageObject::from_message(&u.message);
                let mut to_resolve = Vec::new();
                self.apply_sender_name(&obj, &u.message.sender_id, &mut to_resolve);
                let pos = self.insert_sorted(&obj);
                self.inner.index.borrow_mut().insert(id, obj);
                self.resolve_names(to_resolve);
                self.resolve_replies(&[id]);
                // The insert may end the previous run's grouping; rebind the
                // affected window around the insertion point.
                self.rebind_around(pos);
                let aid = u.message.media_album_id;
                if aid != 0 {
                    self.rebind_album_run(aid);
                }
                self.scroll_to_bottom();
                if !u.message.is_outgoing {
                    self.view_message(id);
                }
            }
            Update::MessageSendSucceeded(u) if u.message.chat_id == self.inner.chat_id => {
                let old = u.old_message_id;
                let new_id = u.message.id;
                self.inner.temp_ids.borrow_mut().remove(&old);
                let existing = self.inner.index.borrow_mut().remove(&old);
                if let Some(obj) = existing {
                    // Drop the stale store slot (keyed at the temp id) BEFORE we
                    // mutate the object's id, then re-insert to keep the store's
                    // binary-search order intact under the real id.
                    self.remove_from_store_by_id(old);
                    obj.update_from_message(&u.message);
                    let pos = self.insert_sorted(&obj);
                    self.inner.index.borrow_mut().insert(new_id, obj);
                    self.rebind_around(pos);
                    let aid = u.message.media_album_id;
                    if aid != 0 {
                        self.rebind_album_run(aid);
                    }
                } else if !self.inner.index.borrow().contains_key(&new_id) {
                    let obj = MessageObject::from_message(&u.message);
                    let pos = self.insert_sorted(&obj);
                    self.inner.index.borrow_mut().insert(new_id, obj);
                    self.rebind_around(pos);
                    let aid = u.message.media_album_id;
                    if aid != 0 {
                        self.rebind_album_run(aid);
                    }
                }
            }
            Update::MessageSendFailed(u) if u.message.chat_id == self.inner.chat_id => {
                let old = u.old_message_id;
                self.inner.temp_ids.borrow_mut().remove(&old);
                if let Some(obj) = self.inner.index.borrow().get(&old).cloned() {
                    // Keep the row so the user sees it stuck-failed rather than
                    // vanishing; mark it pending/failed for styling.
                    obj.update_from_message(&u.message);
                    obj.set_is_pending(true);
                    self.notify_changed(old);
                }
                tracing::warn!(code = u.error.code, msg = %u.error.message, "message send failed");
            }
            Update::MessageContent(u) if u.chat_id == self.inner.chat_id => {
                if let Some(obj) = self.inner.index.borrow().get(&u.message_id).cloned() {
                    // A plain-text edit repaints reactively via the body label's
                    // `notify::content-text` handler (set_content → set_content_text
                    // emits it), so for that common case we deliberately avoid
                    // notify_changed: items_changed is unreliable for rebinds and
                    // disrupts the scroll anchor (would jump the chat).
                    //
                    // But the reactive handler only touches the text label, so a
                    // content update that changes the message KIND or its media
                    // (photo/caption/voice/document/file ids) would leave the row
                    // stale. Detect that by comparing the kind across set_content
                    // and force a full rebind for anything that isn't a plain-text
                    // → plain-text edit. The rarer media-change rebind may nudge
                    // the scroll anchor, an acceptable trade for correct repaint.
                    let prev_kind = obj.kind();
                    obj.set_content(&u.new_content);
                    if !(prev_kind == kind::TEXT && obj.kind() == kind::TEXT) {
                        self.notify_changed(u.message_id);
                    }
                }
            }
            Update::MessageInteractionInfo(u) if u.chat_id == self.inner.chat_id => {
                // Live reaction/view changes. Update the object's `reactions`
                // property; the row's bound `notify::reactions` handler rebuilds
                // its chips reactively (no items_changed — it doesn't reliably
                // repaint during update storms).
                if let Some(obj) = self.inner.index.borrow().get(&u.message_id).cloned() {
                    obj.set_reactions_from(u.interaction_info.as_ref());
                }
            }
            Update::DeleteMessages(u) if u.chat_id == self.inner.chat_id => {
                if u.from_cache {
                    return;
                }
                for id in u.message_ids {
                    self.remove_message(id);
                }
            }
            Update::ChatReadOutbox(u) if u.chat_id == self.inner.chat_id => {
                // The recipient advanced their read cursor: promote every
                // outgoing message at/under the new id to "read" (double check).
                self.inner
                    .last_read_outbox
                    .set(u.last_read_outbox_message_id);
                self.refresh_read_state(u.last_read_outbox_message_id);
            }
            Update::ChatAction(u) if u.chat_id == self.inner.chat_id => {
                self.handle_chat_action(u);
            }
            _ => {}
        }
    }

    /// Update the header subtitle from a peer's typing action in the open chat.
    /// Ignores our own actions; clears on Cancel or after a ~6s idle timeout.
    fn handle_chat_action(&self, u: tdlib_rs::types::UpdateChatAction) {
        // Ignore our own typing echo.
        let sender_id = match &u.sender_id {
            MessageSender::User(s) => s.user_id,
            MessageSender::Chat(_) => return,
        };
        if sender_id == self.inner.me_id.get() {
            return;
        }
        match u.action {
            tdlib_rs::enums::ChatAction::Typing => {
                let subtitle = if self.inner.is_group.get() {
                    let name = self
                        .inner
                        .names
                        .borrow()
                        .get(&sender_id)
                        .cloned()
                        .unwrap_or_default();
                    // Resolve the name if unknown so a later refresh can label it.
                    if name.is_empty() {
                        self.resolve_names(vec![sender_id]);
                        "typing…".to_string()
                    } else {
                        format!("{name} is typing…")
                    }
                } else {
                    "typing…".to_string()
                };
                self.set_typing_subtitle(&subtitle);
                // Arm a self-cancelling 6s clear: bump the generation, and only the
                // latest armed timeout actually clears (a refresh bumps it again).
                let gen = self.inner.typing_gen.get().wrapping_add(1);
                self.inner.typing_gen.set(gen);
                let this = self.clone();
                glib::timeout_add_seconds_local_once(6, move || {
                    if this.inner.typing_gen.get() == gen {
                        this.set_typing_subtitle("");
                    }
                });
            }
            tdlib_rs::enums::ChatAction::Cancel => {
                // Invalidate any pending clear timeout and clear now.
                self.inner
                    .typing_gen
                    .set(self.inner.typing_gen.get().wrapping_add(1));
                self.set_typing_subtitle("");
            }
            _ => {}
        }
    }

    /// Set the header's live subtitle (empty string hides the typing status).
    fn set_typing_subtitle(&self, subtitle: &str) {
        self.inner.header_title.set_subtitle(subtitle);
    }

    /// Remove a deleted message's row + index entry, disarming compose if it was
    /// the edit/reply target.
    fn remove_message(&self, id: i64) {
        let removed = self.inner.index.borrow_mut().remove(&id);
        if removed.is_none() {
            return;
        }
        // If the deleted message was the current compose target, disarm the
        // compose state so we don't edit/reply-to a now-invalid message.
        if self.inner.editing.get() == id {
            self.exit_edit_ui();
        }
        if self.inner.reply_to.get() == id {
            self.clear_reply();
        }
        let store = &self.inner.store;
        let n = store.n_items();
        for pos in 0..n {
            if let Some(obj) = store.item(pos).and_downcast::<MessageObject>() {
                if obj.id() == id {
                    store.remove(pos);
                    return;
                }
            }
        }
    }

    /// Re-bind the row for `message_id` after mutating its live object.
    fn notify_changed(&self, message_id: i64) {
        let store = &self.inner.store;
        let n = store.n_items();
        for pos in 0..n {
            if let Some(obj) = store.item(pos).and_downcast::<MessageObject>() {
                if obj.id() == message_id {
                    store.items_changed(pos, 1, 1);
                    return;
                }
            }
        }
    }

    /// Re-bind every row (used after a batch insert changes grouping).
    fn rebind_all(&self) {
        let n = self.inner.store.n_items();
        if n > 0 {
            self.inner.store.items_changed(0, n, n);
        }
    }

    /// Promote outgoing messages whose id is `<= last_read` to the "read"
    /// (double-check) status. Only touches SENT rows — pending/failed rows keep
    /// their clock, and messages already "read" are left alone. Mutating the
    /// live `MessageObject`'s `send-status` fires `notify` and the per-row
    /// property binding set up in `bind_row` updates the visible indicator, so
    /// no `items_changed`/rebind is needed here.
    fn refresh_read_state(&self, last_read: i64) {
        if last_read == 0 {
            return;
        }
        for obj in self.inner.index.borrow().values() {
            if obj.is_outgoing()
                && obj.id() <= last_read
                && obj.send_status() == send_status::SENT
            {
                obj.set_send_status(send_status::READ);
            }
        }
    }

    /// Re-bind the inserted row and its immediate neighbours so grouping/sender/
    /// avatar flags recompute for the affected window `[pos-1, pos+1]`.
    fn rebind_around(&self, pos: u32) {
        let n = self.inner.store.n_items();
        if n == 0 {
            return;
        }
        let start = pos.saturating_sub(1);
        let end = (pos + 1).min(n.saturating_sub(1));
        let count = end - start + 1;
        self.inner.store.items_changed(start, count, count);
    }

    /// Rebind every row of the album run with `album_id` so the album's first
    /// row re-renders its grid to include a newly-arrived member. No-op for 0.
    fn rebind_album_run(&self, album_id: i64) {
        if album_id == 0 {
            return;
        }
        let store = &self.inner.store;
        let n = store.n_items();
        let mut first: Option<u32> = None;
        let mut last: u32 = 0;
        for pos in 0..n {
            if let Some(obj) = store.item(pos).and_downcast::<MessageObject>() {
                if obj.media_album_id() == album_id {
                    if first.is_none() {
                        first = Some(pos);
                    }
                    last = pos;
                }
            }
        }
        if let Some(f) = first {
            let count = last - f + 1;
            store.items_changed(f, count, count);
        }
    }

    /// Remove the first store row whose id matches `id` (index-map untouched).
    /// Callers that already removed the object from `index` use this to drop the
    /// stale store slot before re-inserting under a new id.
    fn remove_from_store_by_id(&self, id: i64) {
        let store = &self.inner.store;
        let n = store.n_items();
        for pos in 0..n {
            if let Some(obj) = store.item(pos).and_downcast::<MessageObject>() {
                if obj.id() == id {
                    store.remove(pos);
                    return;
                }
            }
        }
    }

    /// Fill in `sender_name` for an incoming message; queue unknown user ids.
    ///
    /// Names are only shown in groups, but the AVATAR is shown for every incoming
    /// message (group OR 1:1), so we queue the sender's user id for resolution in
    /// both cases — `resolve_names` stamps the avatar regardless of group-ness and
    /// only stamps the name in groups.
    fn apply_sender_name(
        &self,
        obj: &MessageObject,
        sender: &MessageSender,
        to_resolve: &mut Vec<i64>,
    ) {
        if obj.is_outgoing() {
            return;
        }
        if let MessageSender::User(u) = sender {
            // In groups, stamp an already-known name synchronously.
            if self.inner.is_group.get() {
                if let Some(name) = self.inner.names.borrow().get(&u.user_id) {
                    obj.set_sender_name(name.clone());
                }
            }
            // Always queue for resolution so the avatar resolves in 1:1 too.
            to_resolve.push(u.user_id);
        }
    }

    /// Resolve sender names for all currently-loaded incoming messages (used
    /// once we learn the chat is a group, so names weren't resolved earlier).
    fn resolve_all_sender_names(&self) {
        let mut ids: Vec<i64> = Vec::new();
        let store = &self.inner.store;
        let n = store.n_items();
        for pos in 0..n {
            if let Some(obj) = store.item(pos).and_downcast::<MessageObject>() {
                if !obj.is_outgoing() && obj.sender_name().is_empty() {
                    let sid = obj.sender_id();
                    if sid != 0 && !ids.contains(&sid) {
                        ids.push(sid);
                    }
                }
            }
        }
        self.resolve_names(ids);
    }

    /// Resolve display + avatar for the given user ids, then re-apply.
    ///
    /// The AVATAR is resolved for every incoming sender (group OR 1:1); the NAME
    /// is only stamped in group chats (`apply_name_to_rows` is itself group-gated).
    /// We no longer early-return on non-group here, otherwise 1:1 senders would
    /// never resolve their avatar.
    fn resolve_names(&self, user_ids: Vec<i64>) {
        for uid in user_ids {
            if uid == 0 || self.inner.names.borrow().contains_key(&uid) {
                continue;
            }
            let cid = self.inner.client.client_id();
            let this = self.clone();
            crate::runtime::spawn(
                async move { functions::get_user(uid, cid).await },
                move |res| {
                    if let Ok(UserEnum::User(user)) = res {
                        let name = display_name(&user.first_name, &user.last_name);
                        this.inner.names.borrow_mut().insert(uid, name.clone());
                        // `profile_photo` is optional; `small` is the 160px avatar.
                        let avatar_id = user
                            .profile_photo
                            .as_ref()
                            .map(|p| p.small.id)
                            .unwrap_or(0);
                        // Name only shows in groups (apply_name_to_rows gates it);
                        // avatar shows for all incoming rows.
                        this.apply_name_to_rows(uid, &name);
                        this.apply_avatar_to_rows(uid, avatar_id);
                    }
                },
            );
        }
    }

    /// After a name resolves, stamp it onto every already-inserted row by `uid`
    /// and re-bind those rows.
    fn apply_name_to_rows(&self, uid: i64, name: &str) {
        // Sender names are only shown in group chats.
        if !self.inner.is_group.get() {
            return;
        }
        let store = &self.inner.store;
        let n = store.n_items();
        for pos in 0..n {
            if let Some(obj) = store.item(pos).and_downcast::<MessageObject>() {
                if !obj.is_outgoing() && obj.sender_id() == uid {
                    if obj.sender_name().is_empty() {
                        obj.set_sender_name(name.to_string());
                    }
                    store.items_changed(pos, 1, 1);
                }
            }
        }
    }

    /// After a user's avatar file id resolves, stamp it onto every already-
    /// inserted incoming row by `uid` and re-bind those rows so the trailing
    /// avatar of each same-sender run downloads and appears live.
    fn apply_avatar_to_rows(&self, uid: i64, avatar_file_id: i32) {
        // Avatars are shown for every incoming sender (group OR 1:1); keep the
        // guard against a missing avatar (file id 0 → initials fallback).
        if avatar_file_id == 0 {
            return;
        }
        let store = &self.inner.store;
        let n = store.n_items();
        for pos in 0..n {
            if let Some(obj) = store.item(pos).and_downcast::<MessageObject>() {
                if !obj.is_outgoing() && obj.sender_id() == uid {
                    if obj.avatar_file_id() == 0 {
                        obj.set_avatar_file_id(avatar_file_id);
                    }
                    store.items_changed(pos, 1, 1);
                }
            }
        }
    }

    /// For each of `ids`, if the message replies to another message, resolve the
    /// replied-to sender + snippet (from the store or via `get_message`) and
    /// stamp them onto the object, then re-bind.
    fn resolve_replies(&self, ids: &[i64]) {
        for &id in ids {
            let obj = match self.inner.index.borrow().get(&id).cloned() {
                Some(o) => o,
                None => continue,
            };
            let target = obj.reply_to_id();
            if target == 0 || !obj.reply_sender().is_empty() {
                continue;
            }
            // Fast path: the replied-to message is already loaded.
            if let Some(t) = self.inner.index.borrow().get(&target).cloned() {
                self.stamp_reply(&obj, &t);
                self.notify_changed(id);
                continue;
            }
            // Slow path: fetch it.
            let cid = self.inner.client.client_id();
            let chat_id = self.inner.chat_id;
            let this = self.clone();
            crate::runtime::spawn(
                async move { functions::get_message(chat_id, target, cid).await },
                move |res| {
                    if let Ok(MessageEnum::Message(m)) = res {
                        this.stamp_reply_from_message(id, &m);
                    }
                },
            );
        }
    }

    /// Stamp reply sender+snippet onto `obj` from an already-loaded target row.
    fn stamp_reply(&self, obj: &MessageObject, target: &MessageObject) {
        let sender = if target.is_outgoing() {
            "You".to_string()
        } else {
            let n = target.sender_name();
            if n.is_empty() {
                self.inner
                    .names
                    .borrow()
                    .get(&target.sender_id())
                    .cloned()
                    .unwrap_or_default()
            } else {
                n
            }
        };
        obj.set_reply_sender(sender);
        obj.set_reply_snippet(snippet(&target.content_text()));
    }

    /// Stamp reply sender+snippet onto message `id` from a fetched target msg.
    fn stamp_reply_from_message(&self, id: i64, target: &tdlib_rs::types::Message) {
        let obj = match self.inner.index.borrow().get(&id).cloned() {
            Some(o) => o,
            None => return,
        };
        let sender = if target.is_outgoing {
            "You".to_string()
        } else if let MessageSender::User(u) = &target.sender_id {
            self.inner
                .names
                .borrow()
                .get(&u.user_id)
                .cloned()
                .unwrap_or_default()
        } else {
            String::new()
        };
        obj.set_reply_sender(sender);
        obj.set_reply_snippet(snippet(&crate::models::message_object::content_text(
            &target.content,
        )));
        self.notify_changed(id);
    }

    /// Mark a batch of loaded incoming messages as read.
    fn mark_visible_read(&self, messages: &[tdlib_rs::types::Message]) {
        let ids: Vec<i64> = messages
            .iter()
            .filter(|m| !m.is_outgoing)
            .map(|m| m.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        let cid = self.inner.client.client_id();
        let chat_id = self.inner.chat_id;
        crate::runtime::spawn(
            async move { functions::view_messages(chat_id, ids, None, true, cid).await },
            |res| {
                if let Err(e) = res {
                    tracing::warn!(code = e.code, msg = %e.message, "view_messages failed");
                }
            },
        );
    }

    /// Mark a single message read.
    fn view_message(&self, id: i64) {
        let cid = self.inner.client.client_id();
        let chat_id = self.inner.chat_id;
        crate::runtime::spawn(
            async move { functions::view_messages(chat_id, vec![id], None, true, cid).await },
            |_res| {},
        );
    }

    /// Send the composed text (optimistic append; reconciled on success).
    fn wire_send(&self, send_button: &gtk::Button) {
        let this = self.clone();
        send_button.connect_clicked(clone!(
            #[strong]
            this,
            move |_| this.do_send()
        ));

        let this = self.clone();
        let key = gtk::EventControllerKey::new();
        key.connect_key_pressed(clone!(
            #[strong]
            this,
            move |_, keyval, _keycode, state| {
                // Escape cancels an in-progress edit before anything else.
                if keyval == gtk::gdk::Key::Escape && this.inner.editing.get() != 0 {
                    this.cancel_edit();
                    return glib::Propagation::Stop;
                }
                let is_enter =
                    keyval == gtk::gdk::Key::Return || keyval == gtk::gdk::Key::KP_Enter;
                let shift = state.contains(gtk::gdk::ModifierType::SHIFT_MASK);
                if is_enter && !shift {
                    this.do_send();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        ));
        self.inner.entry.add_controller(key);
    }

    /// Read the compose buffer, send it, optimistically append a pending row.
    fn do_send(&self) {
        // If we're editing, redirect the send to an edit of that message.
        let editing = self.inner.editing.get();
        if editing != 0 {
            self.do_edit(editing);
            return;
        }
        let buffer = self.inner.entry.buffer();
        let (start, end) = buffer.bounds();
        let text = buffer.text(&start, &end, false).to_string();
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        buffer.set_text("");

        let cid = self.inner.client.client_id();
        let chat_id = self.inner.chat_id;
        let content = InputMessageContent::InputMessageText(InputMessageText {
            text: FormattedText {
                text,
                entities: vec![],
            },
            link_preview_options: None,
            clear_draft: true,
        });

        // Thread the armed reply (if any), then disarm the reply strip.
        let reply = self.inner.reply_to.get();
        let reply_to = if reply != 0 {
            Some(InputMessageReplyTo::Message(InputMessageReplyToMessage {
                message_id: reply,
                quote: None,
                checklist_task_id: 0,
            }))
        } else {
            None
        };
        self.clear_reply();

        let this = self.clone();
        crate::runtime::spawn(
            async move {
                functions::send_message(chat_id, None, reply_to, None, content, cid).await
            },
            move |res| match res {
                Ok(MessageEnum::Message(msg)) => {
                    // The returned message already carries a temporary id and a
                    // Pending sending_state, so this IS the single optimistic
                    // insert; `from_message` sets is_pending() from that state.
                    this.append_sent(&msg);
                }
                Err(e) => {
                    tracing::warn!(code = e.code, msg = %e.message, "send_message failed");
                }
            },
        );
    }

    /// Optimistically append a just-sent message row (dedup-guarded by id).
    fn append_sent(&self, msg: &tdlib_rs::types::Message) {
        let id = msg.id;
        if !self.inner.index.borrow().contains_key(&id) {
            let obj = MessageObject::from_message(msg);
            self.inner.temp_ids.borrow_mut().insert(id);
            let pos = self.insert_sorted(&obj);
            self.inner.index.borrow_mut().insert(id, obj);
            self.resolve_replies(&[id]);
            self.rebind_around(pos);
            self.scroll_to_bottom();
        }
    }

    /// Toggle the trailing compose slot between the send and mic buttons based on
    /// whether the entry holds any non-whitespace text. Separate from the typing
    /// signal so it also fires on clears.
    fn wire_compose_toggle(&self) {
        let this = self.clone();
        let buffer = self.inner.entry.buffer();
        buffer.connect_changed(move |buf| {
            // Don't flip the trailing slot while recording (recording_row owns it).
            if this.inner.recording.get() {
                return;
            }
            let (s, e) = buf.bounds();
            let has_text = !buf.text(&s, &e, false).trim().is_empty();
            this.inner.send_button.set_visible(has_text);
            this.inner.mic_button.set_visible(!has_text);
        });
    }

    /// Wire the mic button (start) and the recording-row send/discard buttons.
    fn wire_voice_record(&self, mic_button: &gtk::Button, rec_send: &gtk::Button, rec_cancel: &gtk::Button) {
        let this = self.clone();
        mic_button.connect_clicked(move |_| this.start_recording());
        let this = self.clone();
        rec_send.connect_clicked(move |_| this.send_recording());
        let this = self.clone();
        rec_cancel.connect_clicked(move |_| this.cancel_recording());
    }

    /// Begin capturing a voice note: swap the compose row for the recording row
    /// and tick an elapsed timer once a second. No-op if the mic is unavailable.
    fn start_recording(&self) {
        if self.inner.recording.get() {
            return;
        }
        if let Err(e) = self.inner.recorder.start() {
            tracing::warn!(error = %e, "voice recording unavailable (mic?)");
            return;
        }
        self.inner.recording.set(true);
        self.inner.compose_row.set_visible(false);
        self.inner.recording_row.set_visible(true);
        self.inner.rec_timer_label.set_text("0:00");

        // Tick the M:SS elapsed label once a second while recording.
        let this = self.clone();
        let id = glib::timeout_add_seconds_local(1, move || {
            if !this.inner.recording.get() {
                return glib::ControlFlow::Break;
            }
            let secs = this.inner.recorder.duration_secs();
            this.inner.rec_timer_label.set_text(&format!("{}:{:02}", secs / 60, secs % 60));
            glib::ControlFlow::Continue
        });
        *self.inner.rec_timer_id.borrow_mut() = Some(id);
    }

    /// Reset the compose UI after a recording ends (shared by send + cancel).
    fn stop_rec_ui(&self) {
        if let Some(id) = self.inner.rec_timer_id.borrow_mut().take() {
            id.remove();
        }
        self.inner.recording.set(false);
        self.inner.recording_row.set_visible(false);
        self.inner.compose_row.set_visible(true);
        self.inner.rec_timer_label.set_text("0:00");
        // Restore the trailing-slot state per the (now-idle) entry contents.
        let buffer = self.inner.entry.buffer();
        let (s, e) = buffer.bounds();
        let has_text = !buffer.text(&s, &e, false).trim().is_empty();
        self.inner.send_button.set_visible(has_text);
        self.inner.mic_button.set_visible(!has_text);
    }

    /// Discard the in-progress recording (deletes the temp file) and reset the UI.
    fn cancel_recording(&self) {
        self.inner.recorder.cancel();
        self.stop_rec_ui();
        // Keep any armed reply intact — canceling a recording must not drop it.
    }

    /// Finalize the recording and send it as a voice note, threading any armed
    /// reply. Optimistically appends the returned pending message.
    fn send_recording(&self) {
        let recorded = self.inner.recorder.stop();
        self.stop_rec_ui();
        let (path, duration) = match recorded {
            Some(v) => v,
            None => return,
        };
        let path_string = path.to_string_lossy().into_owned();

        let cid = self.inner.client.client_id();
        let chat_id = self.inner.chat_id;
        let content = InputMessageContent::InputMessageVoiceNote(InputMessageVoiceNote {
            voice_note: InputFile::Local(InputFileLocal { path: path_string }),
            duration,
            waveform: String::new(),
            caption: None,
            self_destruct_type: None,
        });

        // Thread the armed reply (if any), then disarm the reply strip.
        let reply = self.inner.reply_to.get();
        let reply_to = if reply != 0 {
            Some(InputMessageReplyTo::Message(InputMessageReplyToMessage {
                message_id: reply,
                quote: None,
                checklist_task_id: 0,
            }))
        } else {
            None
        };
        self.clear_reply();

        let this = self.clone();
        crate::runtime::spawn(
            async move {
                functions::send_message(chat_id, None, reply_to, None, content, cid).await
            },
            move |res| match res {
                Ok(MessageEnum::Message(msg)) => this.append_sent(&msg),
                Err(e) => {
                    tracing::warn!(code = e.code, msg = %e.message, "send_message (voice) failed");
                }
            },
        );
    }

    /// Throttle-send a Typing chat action while the user edits the compose entry.
    /// Telegram's own cadence is ~every 5s; we gate on wall-clock seconds.
    fn wire_typing_send(&self) {
        let this = self.clone();
        let buffer = self.inner.entry.buffer();
        buffer.connect_changed(move |buf| {
            // Only signal typing when there's actual text (ignore clears on send).
            let (s, e) = buf.bounds();
            if buf.text(&s, &e, false).trim().is_empty() {
                return;
            }
            let now = glib::real_time() / 1_000_000; // micros -> seconds
            let last = this.inner.last_typing_sent.get();
            if now - last < 5 {
                return;
            }
            this.inner.last_typing_sent.set(now);
            let cid = this.inner.client.client_id();
            let chat_id = this.inner.chat_id;
            crate::runtime::spawn(
                async move {
                    functions::send_chat_action(
                        chat_id,
                        None,
                        Some(tdlib_rs::enums::ChatAction::Typing),
                        cid,
                    )
                    .await
                },
                |res| {
                    if let Err(e) = res {
                        tracing::warn!(code = e.code, msg = %e.message, "send_chat_action failed");
                    }
                },
            );
        });
    }

    /// Arm the compose reply-strip to reply to `message_id`.
    fn start_reply(&self, message_id: i64) {
        // Compose state changes; invalidate any in-flight async compose callback.
        self.inner.compose_gen.set(self.inner.compose_gen.get().wrapping_add(1));
        let obj = match self.inner.index.borrow().get(&message_id).cloned() {
            Some(o) => o,
            None => return,
        };
        // Replying and editing are mutually exclusive compose states; if an edit
        // was in progress, cancel it (clears the prefilled edit text) before
        // arming the reply strip.
        if self.inner.editing.get() != 0 {
            self.exit_edit_ui();
        }
        self.inner.reply_to.set(message_id);
        let name = if obj.is_outgoing() {
            "You".to_string()
        } else {
            let n = obj.sender_name();
            if n.is_empty() { "Reply".to_string() } else { n }
        };
        self.inner.reply_bar_name.set_text(&name);
        self.inner.reply_bar_text.set_text(&snippet(&obj.content_text()));
        self.inner.reply_bar.set_reveal_child(true);
        self.inner.entry.grab_focus();
    }

    /// Toggle the current user's reaction `emoji` on `message_id`: remove it if
    /// the message's current reactions show the user already chose it, otherwise
    /// add it. The resulting `Update::MessageInteractionInfo` re-renders the
    /// chips reactively.
    fn toggle_reaction(&self, message_id: i64, emoji: String) {
        // Is this emoji currently chosen by us on this message? Read the live
        // object's decoded reactions.
        let already_chosen = self
            .inner
            .index
            .borrow()
            .get(&message_id)
            .map(|obj| {
                decode_reactions(&obj.reactions())
                    .into_iter()
                    .any(|c| c.emoji == emoji && c.is_chosen)
            })
            .unwrap_or(false);

        // Drop a duplicate toggle for the same (message, emoji) while the first
        // request is still outstanding; otherwise a fast double-tap fires the
        // same add/remove twice instead of cancelling.
        let key = (message_id, emoji.clone());
        if !self.inner.reaction_inflight.borrow_mut().insert(key.clone()) {
            return;
        }

        let cid = self.inner.client.client_id();
        let chat_id = self.inner.chat_id;
        let reaction_type = tdlib_rs::enums::ReactionType::Emoji(tdlib_rs::types::ReactionTypeEmoji {
            emoji,
        });
        if already_chosen {
            let this = self.clone();
            let key = key.clone();
            crate::runtime::spawn(
                async move {
                    functions::remove_message_reaction(chat_id, message_id, reaction_type, cid).await
                },
                move |res| {
                    this.inner.reaction_inflight.borrow_mut().remove(&key);
                    if let Err(e) = res {
                        tracing::warn!(code = e.code, msg = %e.message, "remove_message_reaction failed");
                        crate::ui::toast(&this.inner.toasts, "Couldn't remove reaction");
                    }
                },
            );
        } else {
            let this = self.clone();
            crate::runtime::spawn(
                async move {
                    functions::add_message_reaction(chat_id, message_id, reaction_type, false, true, cid)
                        .await
                },
                move |res| {
                    this.inner.reaction_inflight.borrow_mut().remove(&key);
                    if let Err(e) = res {
                        tracing::warn!(code = e.code, msg = %e.message, "add_message_reaction failed");
                        crate::ui::toast(&this.inner.toasts, "Couldn't add reaction");
                    }
                },
            );
        }
    }

    /// Disarm the compose reply-strip.
    fn clear_reply(&self) {
        // Compose state changes; invalidate any in-flight async compose callback.
        self.inner.compose_gen.set(self.inner.compose_gen.get().wrapping_add(1));
        self.inner.reply_to.set(0);
        self.inner.reply_bar.set_reveal_child(false);
    }

    /// Begin editing `message_id` if TDLib says it's editable (gate mirrors
    /// `delete_message`'s `get_message_properties` check) AND the message is a
    /// plain-text message. Versioned against `compose_gen` so a slow permission
    /// check can't arm edit mode over a newer reply/edit/draft state.
    fn begin_edit(&self, message_id: i64) {
        let cid = self.inner.client.client_id();
        let chat_id = self.inner.chat_id;
        let gen = self.inner.compose_gen.get();
        let this = self.clone();
        crate::runtime::spawn(
            async move { functions::get_message_properties(chat_id, message_id, cid).await },
            move |res| match res {
                Ok(MessagePropsEnum::MessageProperties(p)) => {
                    // Bail if the compose state moved on while we awaited.
                    if this.inner.compose_gen.get() != gen {
                        return;
                    }
                    // Only plain-text messages take the text-edit path; editable
                    // media / captions / polls would fail an `edit_message_text`.
                    let is_text = this
                        .inner
                        .index
                        .borrow()
                        .get(&message_id)
                        .map(|obj| obj.kind() == kind::TEXT)
                        .unwrap_or(false);
                    if p.can_be_edited && is_text {
                        this.enter_edit_mode(message_id);
                    }
                }
                Err(_) => {}
            },
        );
    }

    /// Arm edit mode: prefill the compose entry with the message body and reveal
    /// the edit strip. Sending now edits `message_id` instead of composing anew.
    fn enter_edit_mode(&self, message_id: i64) {
        // Compose state changes; invalidate any in-flight async compose callback.
        self.inner.compose_gen.set(self.inner.compose_gen.get().wrapping_add(1));
        let obj = match self.inner.index.borrow().get(&message_id).cloned() {
            Some(o) => o,
            None => return,
        };
        // Editing and replying are mutually exclusive compose states.
        self.clear_reply();
        self.inner.editing.set(message_id);
        let text = obj.content_text();
        self.inner.entry.buffer().set_text(&text);
        self.inner.edit_bar_text.set_text(&snippet(&text));
        self.inner.edit_bar.set_reveal_child(true);
        // Focus the entry and drop the cursor at the end of the prefilled text.
        let buffer = self.inner.entry.buffer();
        let end = buffer.end_iter();
        buffer.place_cursor(&end);
        self.inner.entry.grab_focus();
    }

    /// Tear down the edit strip and clear the compose entry.
    fn exit_edit_ui(&self) {
        // Compose state changes; invalidate any in-flight async compose callback.
        self.inner.compose_gen.set(self.inner.compose_gen.get().wrapping_add(1));
        self.inner.editing.set(0);
        self.inner.edit_bar.set_reveal_child(false);
        self.inner.entry.buffer().set_text("");
    }

    /// Cancel an in-progress edit, discarding the compose entry contents.
    fn cancel_edit(&self) {
        self.exit_edit_ui();
    }

    /// Commit the compose entry as an edit of `message_id` via `edit_message_text`.
    /// Builds the `InputMessageText` identically to a normal outgoing send
    /// ([`do_send`]) so an edit never strips formatting or alters link previews
    /// beyond what a fresh send would. The edit-mode UI is torn down only once
    /// TDLib accepts the edit; on failure the compose text stays put so the
    /// user's edit isn't lost.
    fn do_edit(&self, message_id: i64) {
        let buffer = self.inner.entry.buffer();
        let (start, end) = buffer.bounds();
        let text = buffer.text(&start, &end, false).to_string();
        let text = text.trim().to_string();
        if text.is_empty() {
            // Keep edit mode armed rather than deleting the message.
            return;
        }
        let cid = self.inner.client.client_id();
        let chat_id = self.inner.chat_id;
        // Mirror `do_send`'s InputMessageText exactly (same empty entities, same
        // no link-preview override). `clear_draft` is false because an edit does
        // not consume the compose draft the way a fresh send does.
        let content = InputMessageContent::InputMessageText(InputMessageText {
            text: FormattedText {
                text,
                entities: vec![],
            },
            link_preview_options: None,
            clear_draft: false,
        });
        let this = self.clone();
        crate::runtime::spawn(
            async move { functions::edit_message_text(chat_id, message_id, content, cid).await },
            move |res| match res {
                Ok(_) => {
                    // Only now tear down the strip + clear the entry; the
                    // resulting `updateMessageContent` repaints the row in place.
                    this.exit_edit_ui();
                }
                Err(e) => {
                    // Keep edit mode armed and the user's text intact so the edit
                    // can be retried; surface the failure.
                    tracing::warn!(code = e.code, msg = %e.message, "edit_message_text failed");
                    crate::ui::toast(&this.inner.toasts, "Couldn't edit message");
                }
            },
        );
    }

    /// Forward `message_id` if TDLib says it may be forwarded, then open a
    /// destination picker (gate mirrors `begin_edit` / `delete_message`).
    fn forward_message(&self, message_id: i64) {
        let cid = self.inner.client.client_id();
        let chat_id = self.inner.chat_id;
        let this = self.clone();
        crate::runtime::spawn(
            async move { functions::get_message_properties(chat_id, message_id, cid).await },
            move |res| match res {
                Ok(MessagePropsEnum::MessageProperties(p)) => {
                    if p.can_be_forwarded {
                        this.open_forward_picker(message_id);
                    }
                }
                Err(_) => {}
            },
        );
    }

    /// Present a dialog listing chats; activating a row forwards `message_id`
    /// there. Titles resolve asynchronously per row. (No search filter yet.)
    fn open_forward_picker(&self, message_id: i64) {
        let list_box = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        list_box.add_css_class("boxed-list");
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list_box)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));
        let dialog = adw::Dialog::builder()
            .title("Forward to…")
            .content_width(360)
            .content_height(520)
            .child(&toolbar)
            .build();

        // Fetch the Main chat list, then resolve each chat's title on its own row.
        // The async blocks capture only the `Copy` client id (i32) — never the
        // `!Send` `TdClient` — so the spawned futures stay `Send`; we call the
        // underlying `functions::*` directly (mirroring `TdClient::get_chats` /
        // `get_chat`) instead of the client convenience helpers.
        let cid = self.inner.client.client_id();
        let this = self.clone();
        let list_box = list_box.clone();
        let dialog_rows = dialog.clone();
        crate::runtime::spawn(
            async move {
                use tdlib_rs::enums::{ChatList, Chats};
                let Chats::Chats(c) =
                    functions::get_chats(Some(ChatList::Main), 200, cid).await?;
                Ok::<Vec<i64>, tdlib_rs::types::Error>(c.chat_ids)
            },
            move |res| {
                let ids = match res {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(code = e.code, msg = %e.message, "get_chats failed");
                        return;
                    }
                };
                for id in ids {
                    let list_box = list_box.clone();
                    let this = this.clone();
                    let dialog = dialog_rows.clone();
                    crate::runtime::spawn(
                        async move {
                            use tdlib_rs::enums::Chat;
                            let Chat::Chat(c) = functions::get_chat(id, cid).await?;
                            Ok::<tdlib_rs::types::Chat, tdlib_rs::types::Error>(c)
                        },
                        move |res| {
                            let chat = match res {
                                Ok(c) => c,
                                Err(_) => return,
                            };
                            let dest = chat.id;
                            let row = adw::ActionRow::builder()
                                .title(chat.title)
                                .activatable(true)
                                .build();
                            let this = this.clone();
                            let dialog = dialog.clone();
                            row.connect_activated(move |_| {
                                this.do_forward(message_id, dest);
                                dialog.close();
                            });
                            list_box.append(&row);
                        },
                    );
                }
            },
        );

        if let Some(root) = self.root.root() {
            dialog.present(Some(&root));
        } else {
            dialog.present(Some(&self.root));
        }
    }

    /// Issue the actual `forward_messages` request to `dest_chat_id`.
    fn do_forward(&self, message_id: i64, dest_chat_id: i64) {
        let cid = self.inner.client.client_id();
        let from_chat_id = self.inner.chat_id;
        let this = self.clone();
        crate::runtime::spawn(
            async move {
                functions::forward_messages(
                    dest_chat_id,
                    None,
                    from_chat_id,
                    vec![message_id],
                    None,
                    false,
                    false,
                    cid,
                )
                .await
            },
            move |res| match res {
                Ok(Messages::Messages(m)) => {
                    // `messages` elements are nullable: a `None` means TDLib
                    // couldn't forward that message (e.g. content restricted).
                    if m.messages.first().map(Option::is_some).unwrap_or(false) {
                        crate::ui::toast(&this.inner.toasts, "Forwarded");
                    } else {
                        crate::ui::toast(&this.inner.toasts, "Couldn't forward");
                    }
                }
                Err(e) => {
                    tracing::warn!(code = e.code, msg = %e.message, "forward_messages failed");
                    crate::ui::toast(&this.inner.toasts, "Couldn't forward");
                }
            },
        );
    }

    /// Copy a message's text to the clipboard.
    fn copy_message(&self, message_id: i64) {
        if let Some(obj) = self.inner.index.borrow().get(&message_id).cloned() {
            let text = obj.content_text();
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&text);
            }
        }
    }

    /// Confirm + delete a message. Queries `get_message_properties` to decide
    /// whether revoke-for-everyone is offered.
    fn delete_message(&self, message_id: i64) {
        let cid = self.inner.client.client_id();
        let chat_id = self.inner.chat_id;
        let this = self.clone();
        crate::runtime::spawn(
            async move { functions::get_message_properties(chat_id, message_id, cid).await },
            move |res| {
                let (can_all, can_self) = match res {
                    Ok(MessagePropsEnum::MessageProperties(p)) => {
                        (p.can_be_deleted_for_all_users, p.can_be_deleted_only_for_self)
                    }
                    Err(_) => (false, true),
                };
                this.confirm_delete(message_id, can_all, can_self);
            },
        );
    }

    /// Show the delete confirmation dialog and issue `delete_messages`.
    fn confirm_delete(&self, message_id: i64, can_all: bool, can_self: bool) {
        let dialog = adw::AlertDialog::new(
            Some("Delete message?"),
            Some("This message will be permanently removed."),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.set_close_response("cancel");

        // Prefer revoke-for-everyone when allowed; otherwise delete-for-self.
        if can_all {
            dialog.add_response("all", "Delete for Everyone");
            dialog.set_response_appearance("all", adw::ResponseAppearance::Destructive);
        }
        if can_self || !can_all {
            dialog.add_response("self", "Delete");
            dialog.set_response_appearance("self", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("self"));
        }
        if can_all {
            dialog.set_default_response(Some("all"));
        }

        let this = self.clone();
        dialog.connect_response(None, move |_, response| {
            let revoke = match response {
                "all" => true,
                "self" => false,
                _ => return,
            };
            this.do_delete(message_id, revoke);
        });

        if let Some(root) = self.root.root() {
            dialog.present(Some(&root));
        } else {
            dialog.present(Some(&self.root));
        }
    }

    /// Issue the actual `delete_messages` request (rows drop on the resulting
    /// `Update::DeleteMessages`).
    fn do_delete(&self, message_id: i64, revoke: bool) {
        let cid = self.inner.client.client_id();
        let chat_id = self.inner.chat_id;
        crate::runtime::spawn(
            async move { functions::delete_messages(chat_id, vec![message_id], revoke, cid).await },
            |res| {
                if let Err(e) = res {
                    tracing::warn!(code = e.code, msg = %e.message, "delete_messages failed");
                }
            },
        );
    }

    /// Wire right-click + long-press on the list to open a per-message menu.
    fn wire_row_menu(&self) {
        // Right-click (secondary button) → context menu at the pointer. The
        // message body `gtk::Label` is non-selectable, so it no longer shows its
        // own built-in secondary-click menu; a plain bubble-phase gesture reaches
        // the list uncontested and opens our popover, matching the long-press.
        let right = gtk::GestureClick::new();
        right.set_button(gtk::gdk::BUTTON_SECONDARY);
        let this = self.clone();
        right.connect_pressed(move |gesture, _n, x, y| {
            if let Some(widget) = gesture.widget() {
                this.popup_menu_at(&widget, x, y);
            }
        });
        self.inner.list_view.add_controller(right);

        // Long-press (touch) → context menu at the press point.
        let long = gtk::GestureLongPress::new();
        long.set_touch_only(false);
        let this = self.clone();
        long.connect_pressed(move |gesture, x, y| {
            if let Some(widget) = gesture.widget() {
                this.popup_menu_at(&widget, x, y);
            }
        });
        self.inner.list_view.add_controller(long);

        // Primary click on a photo → open the full-image viewer. Uses the
        // capture phase so it fires before the list's own row activation.
        let tap = gtk::GestureClick::new();
        tap.set_button(gtk::gdk::BUTTON_PRIMARY);
        tap.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = self.clone();
        tap.connect_released(move |gesture, _n, x, y| {
            if let Some(widget) = gesture.widget() {
                this.maybe_open_photo(&widget, x, y);
            }
        });
        self.inner.list_view.add_controller(tap);

        // Reaction chips (in the list rows) toggle the user's reaction via this
        // durable action, parameterized by (message_id, emoji). Registered once
        // on the list view so it resolves for every row's chip buttons.
        let toggle = gio::SimpleAction::new(
            "toggle-reaction",
            Some(glib::VariantTy::new("(xs)").expect("valid variant type")),
        );
        {
            let this = self.clone();
            toggle.connect_activate(move |_, param| {
                if let Some((message_id, emoji)) =
                    param.and_then(|v| v.get::<(i64, String)>())
                {
                    this.toggle_reaction(message_id, emoji);
                }
            });
        }
        let group = gio::SimpleActionGroup::new();
        group.add_action(&toggle);
        self.inner.list_view.insert_action_group("react", Some(&group));
    }

    /// If (`x`,`y`) is over a photo `gtk::Picture`, open it in a viewer dialog.
    /// The picture stashes its `file_id` in its widget name (`photo-<id>`).
    fn maybe_open_photo(&self, list_view: &gtk::Widget, x: f64, y: f64) {
        let mut widget = match list_view.pick(x, y, gtk::PickFlags::DEFAULT) {
            Some(w) => w,
            None => return,
        };
        loop {
            let name = widget.widget_name();
            if let Some(stripped) = name.strip_prefix("photo-") {
                if let Ok(file_id) = stripped.parse::<i32>() {
                    self.open_photo_viewer(file_id);
                }
                return;
            }
            widget = match widget.parent() {
                Some(p) => p,
                None => return,
            };
            if widget.eq(list_view) {
                return;
            }
        }
    }

    /// Show a `file_id`'s image full-size in an `adw::Dialog`. If the file is not
    /// yet on disk it is downloaded first.
    fn open_photo_viewer(&self, file_id: i32) {
        if file_id == 0 {
            return;
        }
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .vexpand(true)
            .hexpand(true)
            .build();

        let toolbar = adw::ToolbarView::new();
        toolbar.add_css_class("image-viewer");
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&picture));

        let dialog = adw::Dialog::builder()
            .title("Photo")
            .content_width(720)
            .content_height(720)
            .child(&toolbar)
            .build();

        let apply = move |path: std::path::PathBuf| {
            if let Ok(texture) = gtk::gdk::Texture::from_filename(&path) {
                picture.set_paintable(Some(&texture));
            }
        };
        if let Some(path) = self.inner.files.cached(file_id) {
            apply(path);
        } else {
            self.inner.files.download(file_id, 20, apply);
        }

        if let Some(root) = self.root.root() {
            dialog.present(Some(&root));
        } else {
            dialog.present(Some(&self.root));
        }
    }

    /// Identify the row under (`x`,`y`) inside the list view and present a
    /// per-message context menu ANCHORED at that point, with no dim.
    ///
    /// The menu is a plain floating `gtk::Box` (`.msg-menu-card`) added to the
    /// chat's `gtk::Overlay` — not a popover (whose surface unmaps under the
    /// non-popover-host `ToolbarView` on touch) and not a dialog (which dims the
    /// whole screen behind a modal backdrop). A transparent full-area click
    /// catcher sits behind the card so any tap outside it dismisses, without
    /// darkening anything. Overlay margins place the card's top-left corner at
    /// the tapped point, re-clamped once the card is mapped so it never spills
    /// off the right/bottom edge. Only one menu is open at a time.
    fn popup_menu_at(&self, list_view: &gtk::Widget, x: f64, y: f64) {
        let message_id = match self.message_id_at(list_view, x, y) {
            Some(id) => id,
            None => return,
        };

        let overlay = self.inner.overlay.clone();

        // Only one menu at a time: tear down any menu already open before we
        // build a new one (e.g. a second long-press without dismissing).
        if let Some((catcher, menu_box)) = self.inner.open_menu.borrow_mut().take() {
            overlay.remove_overlay(&catcher);
            overlay.remove_overlay(&menu_box);
        }

        // Common fast-react set, mirrored from the old picker. Tapping one
        // toggles the current user's reaction and dismisses the menu.
        const PICKER_EMOJI: [&str; 7] = ["👍", "❤️", "🔥", "🎉", "😁", "😢", "🙏"];

        // The floating menu card: reaction bar on top, action rows below. Its
        // top-left corner is positioned via overlay margins (halign/valign Start).
        let menu_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["msg-menu-card"])
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .build();

        // Transparent full-area catcher behind the card: a tap anywhere off the
        // card dismisses the menu. Left fully transparent — no dim.
        let catcher = gtk::Box::builder().hexpand(true).vexpand(true).build();

        // Shared dismissal: remove BOTH overlay children and clear the tracked
        // state. Guarded so the several dismissal paths (catcher tap, an action,
        // an emoji, Escape) can't double-remove. Holds only local clones, which
        // drop once the widgets leave the overlay — no permanent refs on Inner,
        // so no reference cycle keeps the menu alive.
        let dismiss: Rc<dyn Fn()> = {
            let overlay = overlay.clone();
            let catcher = catcher.clone();
            let menu_box = menu_box.clone();
            let inner = self.inner.clone();
            Rc::new(move || {
                if inner.open_menu.borrow_mut().take().is_some() {
                    overlay.remove_overlay(&catcher);
                    overlay.remove_overlay(&menu_box);
                }
            })
        };

        // Catcher tap (any button) dismisses.
        let catch_click = gtk::GestureClick::new();
        catch_click.set_button(0);
        {
            let dismiss = dismiss.clone();
            catch_click.connect_pressed(move |_, _, _, _| dismiss());
        }
        catcher.add_controller(catch_click);

        // Escape on the card dismisses.
        let key = gtk::EventControllerKey::new();
        {
            let dismiss = dismiss.clone();
            key.connect_key_pressed(move |_, keyval, _keycode, _state| {
                if keyval == gtk::gdk::Key::Escape {
                    dismiss();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
        }
        menu_box.add_controller(key);

        // --- Quick-reaction emoji bar (pill row on top) ---
        // Seeded synchronously with `PICKER_EMOJI`; the async fetch below swaps
        // in the chat's actually-available reactions once it resolves.
        let bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(2)
            .css_classes(["msg-reaction-picker"])
            .build();
        menu_box.append(&bar);

        // Append one flat emoji button to `bar` that toggles the reaction and
        // dismisses the menu. Shared by the sync fallback and the async swap.
        let add_pick = {
            let this = self.clone();
            let dismiss = dismiss.clone();
            let bar = bar.clone();
            move |emoji: String| {
                let button = gtk::Button::builder()
                    .label(&emoji)
                    .css_classes(["flat", "msg-reaction-pick"])
                    .build();
                let this = this.clone();
                let dismiss = dismiss.clone();
                button.connect_clicked(move |_| {
                    this.toggle_reaction(message_id, emoji.clone());
                    dismiss();
                });
                bar.append(&button);
            }
        };

        // Seed the fallback set synchronously so the bar is full at present.
        for e in PICKER_EMOJI {
            add_pick(e.to_string());
        }

        // Fetch the chat's actually-available reactions (TDLib rejects emoji that
        // aren't allowed here, which is what produced the "Couldn't add reaction"
        // toasts). Prefer top, then popular, then recent; keep only plain emoji
        // reactions up to a small cap. On success, swap the fallback out in place;
        // on empty/failure, leave the synchronous fallback as-is.
        let cid = self.inner.client.client_id();
        let chat_id = self.inner.chat_id;
        let bar_async = bar.clone();
        crate::runtime::spawn(
            async move {
                functions::get_message_available_reactions(chat_id, message_id, 8, cid).await
            },
            move |res| {
                // Distinguish a successful fetch (even one yielding no usable
                // reactions) from an error. On success-but-empty we HIDE the
                // bar so the menu never offers reactions TDLib will reject
                // ("The reaction isn't available for the message" — the source
                // of the old "Couldn't add reaction" confusion in restricted
                // chats like Saved Messages or channels). On an Err we leave the
                // synchronous fallback in place as a best effort.
                let fetched_ok = matches!(
                    &res,
                    Ok(tdlib_rs::enums::AvailableReactions::AvailableReactions(_))
                );
                let mut emoji: Vec<String> = Vec::new();
                if let Ok(tdlib_rs::enums::AvailableReactions::AvailableReactions(a)) = res {
                    for group in [&a.top_reactions, &a.popular_reactions, &a.recent_reactions] {
                        for r in group {
                            if emoji.len() >= 8 {
                                break;
                            }
                            // Skip reactions gated behind Telegram Premium the
                            // user may not have; adding those fails at TDLib.
                            if r.needs_premium {
                                continue;
                            }
                            if let tdlib_rs::enums::ReactionType::Emoji(e) = &r.r#type {
                                if !emoji.contains(&e.emoji) {
                                    emoji.push(e.emoji.clone());
                                }
                            }
                        }
                    }
                }
                if emoji.is_empty() {
                    // Fetch succeeded but nothing is addable here → hide the bar
                    // so only the action rows show. On an errored fetch, keep the
                    // synchronous fallback as best effort.
                    if fetched_ok {
                        bar_async.set_visible(false);
                    }
                    return;
                }
                // Skip the swap entirely if the fetched set already matches what's
                // shown (common: the fetched set equals the synchronous seed).
                let current: Vec<String> = {
                    let mut v = Vec::new();
                    let mut child = bar_async.first_child();
                    while let Some(c) = child {
                        if let Some(btn) = c.downcast_ref::<gtk::Button>() {
                            v.push(btn.label().map(|s| s.to_string()).unwrap_or_default());
                        }
                        child = c.next_sibling();
                    }
                    v
                };
                if current == emoji {
                    return;
                }
                // Swap the fallback buttons for the fetched set in place.
                while let Some(child) = bar_async.first_child() {
                    bar_async.remove(&child);
                }
                for e in emoji {
                    add_pick(e);
                }
            },
        );

        // --- Action rows below the bar ---
        // Each row is an icon + label inside a flat button; clicking invokes the
        // target method directly then dismisses the menu.
        let make_item = |icon: &str, label: &str, destructive: bool| -> gtk::Button {
            let content = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(10)
                .build();
            content.append(&gtk::Image::from_icon_name(icon));
            content.append(
                &gtk::Label::builder()
                    .label(label)
                    .halign(gtk::Align::Start)
                    .hexpand(true)
                    .build(),
            );
            let mut classes = vec!["flat", "msg-menu-item"];
            if destructive {
                classes.push("destructive");
            }
            gtk::Button::builder().css_classes(classes).child(&content).build()
        };

        let reply = make_item("mail-reply-sender-symbolic", "Reply", false);
        {
            let this = self.clone();
            let dismiss = dismiss.clone();
            reply.connect_clicked(move |_| {
                this.start_reply(message_id);
                dismiss();
            });
        }
        menu_box.append(&reply);

        // Edit only applies to plain-text messages; `begin_edit` still confirms
        // `can_be_edited` asynchronously before arming the compose strip.
        let is_text = self
            .inner
            .index
            .borrow()
            .get(&message_id)
            .map(|obj| obj.kind() == kind::TEXT)
            .unwrap_or(false);
        if is_text {
            let edit = make_item("document-edit-symbolic", "Edit", false);
            let this = self.clone();
            let dismiss = dismiss.clone();
            edit.connect_clicked(move |_| {
                this.begin_edit(message_id);
                dismiss();
            });
            menu_box.append(&edit);
        }

        let copy = make_item("edit-copy-symbolic", "Copy", false);
        {
            let this = self.clone();
            let dismiss = dismiss.clone();
            copy.connect_clicked(move |_| {
                this.copy_message(message_id);
                dismiss();
            });
        }
        menu_box.append(&copy);

        let forward = make_item("mail-forward-symbolic", "Forward", false);
        {
            let this = self.clone();
            let dismiss = dismiss.clone();
            forward.connect_clicked(move |_| {
                this.forward_message(message_id);
                dismiss();
            });
        }
        menu_box.append(&forward);

        let delete = make_item("user-trash-symbolic", "Delete", true);
        {
            let this = self.clone();
            let dismiss = dismiss.clone();
            delete.connect_clicked(move |_| {
                this.delete_message(message_id);
                dismiss();
            });
        }
        menu_box.append(&delete);

        // Add to the overlay: catcher first (behind), then the card (above it).
        overlay.add_overlay(&catcher);
        overlay.add_overlay(&menu_box);
        *self.inner.open_menu.borrow_mut() = Some((
            catcher.clone().upcast::<gtk::Widget>(),
            menu_box.clone().upcast::<gtk::Widget>(),
        ));

        // Position the card's top-left at the tapped point, in overlay space.
        let (tx, ty) = list_view
            .translate_coordinates(&overlay, x, y)
            .unwrap_or((x, y));
        let tx = tx.max(0.0) as i32;
        let ty = ty.max(0.0) as i32;
        menu_box.set_margin_start(tx);
        menu_box.set_margin_top(ty);

        // Once mapped the card has a real allocation: re-clamp so it never
        // spills off the right/bottom edge of the overlay. Runs on every map;
        // cheap and idempotent.
        {
            let overlay = overlay.clone();
            menu_box.connect_map(move |menu_box| {
                let (menu_w, menu_h) = (menu_box.width(), menu_box.height());
                let (ov_w, ov_h) = (overlay.width(), overlay.height());
                let mut start = menu_box.margin_start();
                let mut top = menu_box.margin_top();
                if ov_w > 0 && start + menu_w > ov_w {
                    start = ov_w - menu_w;
                }
                if ov_h > 0 && top + menu_h > ov_h {
                    top = ov_h - menu_h;
                }
                let start = start.max(0);
                let top = top.max(0);
                if start != menu_box.margin_start() {
                    menu_box.set_margin_start(start);
                }
                if top != menu_box.margin_top() {
                    menu_box.set_margin_top(top);
                }
            });
        }
    }

    /// The message id of the row under (`x`,`y`) within the list view, if any.
    fn message_id_at(&self, list_view: &gtk::Widget, x: f64, y: f64) -> Option<i64> {
        let mut widget = list_view.pick(x, y, gtk::PickFlags::DEFAULT)?;
        // Walk up until we hit a row carrying a stashed message id.
        loop {
            let name = widget.widget_name();
            if let Some(stripped) = name.strip_prefix("msg-row-") {
                if let Ok(id) = stripped.parse::<i64>() {
                    return Some(id);
                }
            }
            widget = widget.parent()?;
            if widget.eq(list_view) {
                return None;
            }
        }
    }

    /// Load older history when the user scrolls near the top.
    fn wire_scroll_paging(&self) {
        let this = self.clone();
        let vadj = self.inner.scroller.vadjustment();
        vadj.connect_value_changed(move |adj| {
            if adj.value() <= adj.page_size() * 0.5 {
                this.load_older_history();
            }
        });
    }

    /// Wire the floating "scroll to newest" button: it clicks to the bottom and
    /// hides itself whenever the history is already near the bottom.
    fn wire_scroll_button(&self, button: &gtk::Button) {
        // Click → jump to the newest message and hide the button.
        let this = self.clone();
        let btn = button.clone();
        button.connect_clicked(move |_| {
            this.scroll_to_bottom();
            btn.set_visible(false);
        });

        // Toggle visibility from the scroll position: hidden within 200px of the
        // bottom, shown otherwise. This is a second handler on the same
        // vadjustment as `wire_scroll_paging` (multiple handlers are fine).
        let btn = button.clone();
        let vadj = self.inner.scroller.vadjustment();
        vadj.connect_value_changed(move |adj| {
            let distance = adj.upper() - (adj.value() + adj.page_size());
            btn.set_visible(distance > 200.0);
        });
    }

    /// Scroll the history to the newest message.
    fn scroll_to_bottom(&self) {
        let store = self.inner.store.clone();
        let list_view = self.inner.list_view.clone();
        glib::idle_add_local_once(move || {
            let n = store.n_items();
            if n > 0 {
                list_view.scroll_to(n - 1, gtk::ListScrollFlags::NONE, None);
            }
        });
    }
}

// --- Row factory helpers (free fns; the recycling factory captures no `self`) -

// The GObject data key under which a row's live `send-status` → label binding is
// stashed, so a recycled row can unbind the previous item's binding before it
// binds the next one (avoiding the re-bind bug where a stale binding keeps
// driving the label).
const STATUS_BINDING_KEY: &str = "paloma-status-binding";

/// Stash `binding` on `label`, unbinding+dropping any previously stashed one.
fn store_status_binding(label: &gtk::Label, binding: glib::Binding) {
    clear_status_binding(label);
    unsafe {
        label.set_data(STATUS_BINDING_KEY, binding);
    }
}

/// Unbind and drop any binding previously stashed on `label`.
fn clear_status_binding(label: &gtk::Label) {
    unsafe {
        if let Some(prev) = label.steal_data::<glib::Binding>(STATUS_BINDING_KEY) {
            prev.unbind();
        }
    }
}

/// The GObject data key under which a row's live `sender-name` → label binding is
/// stashed, so a recycled row can unbind the previous item's binding before
/// binding the next one (the same re-bind-safety pattern as the status binding).
const SENDER_BINDING_KEY: &str = "paloma-sender-binding";

/// Stash `binding` on `label`, unbinding+dropping any previously stashed one.
fn store_sender_binding(label: &gtk::Label, binding: glib::Binding) {
    clear_sender_binding(label);
    unsafe {
        label.set_data(SENDER_BINDING_KEY, binding);
    }
}

/// Unbind and drop any sender binding previously stashed on `label`.
fn clear_sender_binding(label: &gtk::Label) {
    unsafe {
        if let Some(prev) = label.steal_data::<glib::Binding>(SENDER_BINDING_KEY) {
            prev.unbind();
        }
    }
}

/// Data key under which a voice row stashes its current `VoicePlayer` playback
/// token, so a recycled row can stop the stale stream it previously started.
const VOICE_TOKEN_KEY: &str = "paloma-voice-token";
/// Data key under which the voice play/pause button stashes its current click
/// `SignalHandlerId`, disconnected before a rebind installs a fresh handler.
const VOICE_CLICK_KEY: &str = "paloma-voice-click";
/// Data key under which the voice scale stashes its current `change-value`
/// `SignalHandlerId`, disconnected before a rebind installs a fresh handler.
const VOICE_SEEK_KEY: &str = "paloma-voice-seek";

/// Stash the current playback token on the voice box (replacing any prior one).
fn set_voice_token(vbox: &gtk::Box, token: u64) {
    unsafe {
        vbox.set_data(VOICE_TOKEN_KEY, token);
    }
}
/// Take (remove) the stashed playback token off the voice box, if any.
fn take_voice_token(vbox: &gtk::Box) -> Option<u64> {
    unsafe { vbox.steal_data::<u64>(VOICE_TOKEN_KEY) }
}

/// Build one recycled row: `[bubble{ reply, sender, body, photo, caption,
/// time }]`. Widgets are named so `bind_row` can retrieve them.
fn build_row(_: &gtk::SignalListItemFactory, list_item: &glib::Object) {
    let list_item = list_item
        .downcast_ref::<gtk::ListItem>()
        .expect("list item is a ListItem");

    // Outer vertical row: a full-width date separator (hidden by default) above
    // the horizontal avatar+bubble line. `bind_row` stamps the message id onto
    // THIS outer box (`msg-row-<id>`) for the context-menu gesture walk.
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    row.set_widget_name("msg-row");

    // Date separator pill, centered, shown only on the first row of a new day.
    let date_sep = gtk::Label::builder()
        .css_classes(["msg-date-sep"])
        .halign(gtk::Align::Center)
        .build();
    date_sep.set_widget_name("date-sep");
    date_sep.set_visible(false);
    row.append(&date_sep);

    // Horizontal line holding the avatar slot and the bubble; the bubble's own
    // halign pushes it to one side.
    let hbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .hexpand(true)
        .halign(gtk::Align::Fill)
        .build();
    hbox.set_widget_name("msg-hbox");

    // Avatar slot (left of the bubble). Only populated/visible for the last
    // message of an incoming same-sender run in a group chat; otherwise it
    // reserves its width (invisible) so bubbles stay left-aligned, or is fully
    // hidden (width 0) for outgoing / 1:1 rows. `valign End` sits it at the
    // bottom of the run, matching the official client.
    let avatar = adw::Avatar::new(AVATAR_SIZE, None, true);
    avatar.set_widget_name("avatar");
    avatar.set_valign(gtk::Align::End);
    hbox.append(&avatar);

    let bubble = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .build();
    bubble.add_css_class("msg-bubble");
    bubble.set_widget_name("bubble");

    // Reply quoted header (accent-bordered box; hidden unless replying).
    let reply_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    reply_box.add_css_class("msg-reply");
    reply_box.set_widget_name("reply");
    let reply_name = gtk::Label::builder()
        .css_classes(["msg-reply-name"])
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .single_line_mode(true)
        .build();
    reply_name.set_widget_name("reply-name");
    let reply_text = gtk::Label::builder()
        .css_classes(["msg-reply-text"])
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .single_line_mode(true)
        .build();
    reply_text.set_widget_name("reply-text");
    reply_box.append(&reply_name);
    reply_box.append(&reply_text);
    bubble.append(&reply_box);

    let sender = gtk::Label::builder()
        .css_classes(["msg-sender"])
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .single_line_mode(true)
        .build();
    sender.set_widget_name("sender");
    bubble.append(&sender);

    // Album (media-group) grid: renders 2+ grouped photos as a tiled grid on
    // the first row of the album run. Hidden for single photos / non-photos.
    let album_grid = gtk::Grid::builder()
        .row_spacing(2)
        .column_spacing(2)
        .build();
    album_grid.add_css_class("msg-album");
    album_grid.set_widget_name("album-grid");
    album_grid.set_visible(false);
    bubble.append(&album_grid);

    // Photo (hidden unless the message is a photo).
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .can_shrink(true)
        .build();
    picture.add_css_class("msg-photo");
    picture.add_css_class("msg-photo-single");
    picture.set_widget_name("photo");
    picture.set_visible(false);
    bubble.append(&picture);

    // Voice-note player row (hidden unless the message is a voice note): a
    // play/pause toggle, a seekable progress scale, and an M:SS time label.
    let voice_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    voice_box.add_css_class("msg-voice");
    voice_box.set_widget_name("voice");
    voice_box.set_visible(false);

    let voice_play = gtk::Button::builder()
        .icon_name("media-playback-start-symbolic")
        .css_classes(["circular", "flat", "msg-voice-btn"])
        .valign(gtk::Align::Center)
        .build();
    voice_play.set_widget_name("voice-play");
    voice_box.append(&voice_play);

    let voice_scale = gtk::Scale::builder()
        .orientation(gtk::Orientation::Horizontal)
        .hexpand(true)
        .draw_value(false)
        .valign(gtk::Align::Center)
        .width_request(120)
        .build();
    voice_scale.add_css_class("msg-voice-scale");
    voice_scale.set_widget_name("voice-scale");
    voice_scale.set_range(0.0, 1.0);
    voice_box.append(&voice_scale);

    let voice_time = gtk::Label::builder()
        .css_classes(["msg-voice-time", "dim-label"])
        .valign(gtk::Align::Center)
        .build();
    voice_time.set_widget_name("voice-time");
    voice_box.append(&voice_time);

    bubble.append(&voice_box);

    let body = gtk::Label::builder()
        .css_classes(["msg-body"])
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .selectable(false)
        .max_width_chars(36)
        .build();
    body.set_widget_name("body");
    // Open http/https links (rendered as markup in `bind_row`) in the default
    // handler. Wired once here at setup, not per-bind, so recycling doesn't
    // stack duplicate handlers.
    body.connect_activate_link(|_label, uri| {
        if let Err(e) =
            gio::AppInfo::launch_default_for_uri(uri, None::<&gio::AppLaunchContext>)
        {
            tracing::warn!(uri = %uri, error = %e, "failed to open link");
        }
        glib::Propagation::Stop
    });
    bubble.append(&body);

    // Footer: timestamp + (outgoing only) the sent/read indicator, right-aligned.
    let footer = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(3)
        .halign(gtk::Align::End)
        .build();
    footer.set_widget_name("footer");

    let time = gtk::Label::builder()
        .css_classes(["msg-time", "dim-label"])
        .xalign(1.0)
        .build();
    time.set_widget_name("time");
    footer.append(&time);

    // Sent/read checkmark (hidden for incoming; text driven by a property
    // binding to the item's `send-status` set up in `bind_row`).
    let status = gtk::Label::builder()
        .css_classes(["msg-status"])
        .xalign(1.0)
        .build();
    status.set_widget_name("status");
    status.set_visible(false);
    footer.append(&status);

    bubble.append(&footer);

    // Reaction chips row (emoji + count), rendered BELOW the footer inside the
    // bubble; hidden unless the message carries reactions. `bind_row` fills it
    // from the item's `reactions` property and rebuilds it reactively on
    // `notify::reactions`. Horizontal so multiple chips sit in a line.
    let reactions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .build();
    reactions.add_css_class("msg-reactions");
    reactions.set_widget_name("reactions");
    reactions.set_visible(false);
    bubble.append(&reactions);

    hbox.append(&bubble);
    row.append(&hbox);
    list_item.set_child(Some(&row));
}

/// Bind a [`MessageObject`] onto a recycled row, computing grouping from the
/// neighbouring rows in `store` and resolving media via `files`.
fn bind_row(list_item: &glib::Object, store: &gio::ListStore, files: &FileStore, voice: &VoicePlayer) {
    let list_item = list_item
        .downcast_ref::<gtk::ListItem>()
        .expect("list item is a ListItem");
    let item = list_item
        .item()
        .and_downcast::<MessageObject>()
        .expect("item is a MessageObject");
    let row = list_item
        .child()
        .and_downcast::<gtk::Box>()
        .expect("row child is a Box");
    let root = row.clone().upcast::<gtk::Widget>();

    // Tear down any stale per-row reactive handlers from the item this recycled
    // row PREVIOUSLY showed, before any early return below (e.g. album
    // continuation members return early and would otherwise leave the previous
    // item's avatar/reactions notify handlers stashed on the pooled widgets).
    if let Some(avatar) = find::<adw::Avatar>(&root, "avatar") {
        clear_avatar_notify(&avatar);
    }
    if let Some(chips) = find::<gtk::Box>(&root, "reactions") {
        clear_reactions_notify(&chips);
    }
    if let Some(body) = find::<gtk::Label>(&root, "body") {
        clear_content_notify(&body);
    }

    let outgoing = item.is_outgoing();
    // Stash the message id on the row so the context-menu gesture can find it.
    row.set_widget_name(&format!("msg-row-{}", item.id()));
    // Message id captured by reaction chip taps (each chip toggles the user's
    // reaction on its emoji). Resolved to the ChatView action group via the
    // list view's `react.toggle-reaction` action (see `wire_row_menu`).
    let tap_target = item.id();

    // Grouping: look at neighbours in the model (list positions, not ids, since
    // the store is already ascending-sorted by id === chronological).
    let pos = list_item.position();
    let prev = neighbour(store, pos, -1);
    let next = neighbour(store, pos, 1);
    let album_id = item.media_album_id();
    // A later member of an album run: the previous row shares this album id.
    let is_album_first = prev.as_ref().map(|p| p.media_album_id()) != Some(album_id);
    // Find the last store position sharing this album id (contiguous run).
    let run_end_pos = if album_id != 0 {
        let n = store.n_items();
        let mut end = pos;
        let mut p = pos + 1;
        while p < n {
            match store.item(p).and_downcast::<MessageObject>() {
                Some(m) if m.media_album_id() == album_id => {
                    end = p;
                    p += 1;
                }
                _ => break,
            }
        }
        end
    } else {
        pos
    };
    // For grouping (sender/avatar) treat the whole album as one unit: the
    // "next" row for grouping is the first row AFTER the album run.
    let group_next = if album_id != 0 {
        neighbour(store, run_end_pos, 1)
    } else {
        next.clone()
    };
    let show_sender = !grouped_with(Some(&item), prev.as_ref());
    let is_last_of_run = !grouped_with(group_next.as_ref(), Some(&item));

    // --- Recycle reset for album-toggled widgets (run on EVERY bind) --------
    // hbox visible by default (later album members hide it below).
    if let Some(hbox) = find::<gtk::Box>(&root, "msg-hbox") {
        hbox.set_visible(true);
    }
    // Album grid cleared + hidden by default (album-first branch re-shows it).
    if let Some(grid) = find::<gtk::Grid>(&root, "album-grid") {
        while let Some(child) = grid.first_child() {
            grid.remove(&child);
        }
        grid.set_visible(false);
    }

    // Voice recycle reset: if this recycled row still holds a live playback
    // token from the message it PREVIOUSLY showed, stop that stream (the row is
    // about to show a different message) and clear the stashed token. Runs on
    // every bind — including before the album early-return below — so scrolling
    // a playing voice note off-screen halts audio.
    if let Some(vbox) = find::<gtk::Box>(&root, "voice") {
        if let Some(old) = take_voice_token(&vbox) {
            voice.stop_if_owner(old);
        }
        // Default hidden; the Voice branch below re-shows + configures it.
        vbox.set_visible(false);
    }

    // A later member of an album run renders nothing: its photo is drawn in the
    // album-first row's grid. Collapse this row to zero height. (The row's
    // widget-name stash above keeps the context menu working.)
    if album_id != 0 && !is_album_first {
        // Tear down any live property bindings from a previous item so a hidden
        // label isn't driven by a stale binding.
        if let Some(status) = find::<gtk::Label>(&root, "status") {
            clear_status_binding(&status);
        }
        if let Some(sender) = find::<gtk::Label>(&root, "sender") {
            clear_sender_binding(&sender);
        }
        if let Some(reactions) = find::<gtk::Box>(&root, "reactions") {
            reactions.set_visible(false);
        }
        // Hide the horizontal content line; also hide the date separator (the
        // album's first member owns the date).
        if let Some(hbox) = find::<gtk::Box>(&root, "msg-hbox") {
            hbox.set_visible(false);
        }
        if let Some(sep) = find::<gtk::Label>(&root, "date-sep") {
            sep.set_visible(false);
            sep.set_text("");
        }
        return;
    }

    row.set_halign(gtk::Align::Fill);

    // Date separator: shown on the first loaded row and whenever this message
    // falls on a different local calendar day than the row above it. Recycled
    // rows must reset to hidden, so we always set visibility explicitly here.
    if let Some(sep) = find::<gtk::Label>(&root, "date-sep") {
        let first_of_day = match prev.as_ref() {
            None => true,
            Some(p) => !same_local_day(p.date(), item.date()),
        };
        if first_of_day {
            sep.set_text(&crate::format::date_separator(item.date()));
            sep.set_visible(true);
        } else {
            sep.set_text("");
            sep.set_visible(false);
        }
    }

    if let Some(bubble) = find::<gtk::Box>(&root, "bubble") {
        bubble.set_halign(if outgoing {
            gtk::Align::End
        } else {
            gtk::Align::Start
        });
        bubble.remove_css_class("msg-out");
        bubble.remove_css_class("msg-in");
        bubble.remove_css_class("msg-pending");
        bubble.add_css_class(if outgoing { "msg-out" } else { "msg-in" });
        if item.is_pending() {
            bubble.add_css_class("msg-pending");
        }
    }

    // Reply header.
    if let Some(reply_box) = find::<gtk::Box>(&root, "reply") {
        let target = item.reply_to_id();
        if target != 0 && !item.reply_snippet().is_empty() {
            reply_box.set_visible(true);
            if let Some(rn) = find::<gtk::Label>(&root, "reply-name") {
                rn.set_text(&item.reply_sender());
            }
            if let Some(rt) = find::<gtk::Label>(&root, "reply-text") {
                rt.set_text(&item.reply_snippet());
            }
        } else {
            reply_box.set_visible(false);
        }
    }

    // Sender name (incoming, groups only, first of a consecutive same-sender
    // run). Driven by a property binding — like the status checkmark — so a
    // name that resolves ASYNC (get_user after the row bound) repaints live
    // without needing an items_changed. Recycled rows tear the old binding
    // down first so they never cross-wire to a stale MessageObject.
    if let Some(sender) = find::<gtk::Label>(&root, "sender") {
        clear_sender_binding(&sender);
        let name = item.sender_name();
        // `sender-name` is only ever populated for INCOMING messages in GROUP
        // chats (apply_sender_name/resolve_names early-return otherwise), so a
        // non-empty name here already implies group+incoming. We still gate on
        // !outgoing and the first-of-run flag so stacked messages hide it.
        if !outgoing && show_sender && !name.is_empty() {
            sender.set_text(&name);
            sender.set_visible(true);
            let binding = item
                .bind_property("sender-name", &sender, "label")
                .sync_create()
                .build();
            store_sender_binding(&sender, binding);
        } else if !outgoing && show_sender {
            // Group incoming, first-of-run, but the name hasn't resolved yet:
            // keep the slot bound (and hidden) so the label repaints and shows
            // the moment the async get_user lands, no scroll/rebind required.
            sender.set_text("");
            sender.set_visible(false);
            let binding = item
                .bind_property("sender-name", &sender, "label")
                .sync_create()
                .build();
            store_sender_binding(&sender, binding);
        } else {
            sender.set_visible(false);
            sender.set_text("");
        }
    }

    // Sender avatar (every incoming message, group OR 1:1). Shown only on the
    // LAST row of a same-sender run; earlier rows of the run keep the slot but
    // hide the image so bubbles stay left-aligned. Outgoing rows collapse the
    // slot to 0.
    if let Some(avatar) = find::<adw::Avatar>(&root, "avatar") {
        // The sender avatar is shown for EVERY incoming message (group OR 1:1),
        // never for outgoing. In groups the initials fallback uses the sender
        // name; in 1:1 the name is empty and adw::Avatar falls back to a generic
        // person icon, which is fine.
        // (The stale `notify::avatar-file-id` handler from a prior occupant is
        // already torn down at the top of bind_row, before any early return.)
        if !outgoing {
            // Placeholder color: rely on AdwAvatar's OWN built-in text-derived
            // auto-color (the `.colorN` class it adds from `text`). Our `text` is
            // STABLE per user — empty in 1:1 (constant) and the sender name in
            // groups (constant per sender) — so the color never flips. A manual
            // `.colorN` was pointless: AdwAvatar's auto class always coexists with
            // and overrides it. Because we `set_text` on every bind, AdwAvatar
            // re-derives the color for the new occupant, so no clearing is needed
            // on recycle.
            // Reserve the slot on every incoming row. `text` is a STABLE per-user
            // value (the sender name) so nothing about the avatar varies per
            // message; a resolved custom image (below) overrides the initials.
            avatar.set_size(AVATAR_SIZE);
            avatar.set_show_initials(true);
            avatar.set_text(Some(&item.sender_name()));
            avatar.set_custom_image(gtk::gdk::Paintable::NONE);
            if is_last_of_run {
                avatar.set_visible(true);
                // The avatar file id resolves ASYNC (resolve_names → set on the
                // MessageObject). items_changed does NOT reliably repaint list
                // rows during TDLib update storms, so drive the image off a
                // reactive `notify::avatar-file-id` handler instead: it fires the
                // moment the id lands and re-runs the load. We also run it once
                // now for the already-resolved case. The handler id is stashed on
                // the avatar and torn down on the next bind (clear_avatar_notify).
                // The message id this row is bound to; used as the recycle
                // identity everywhere below (the ListItem's current item must
                // still match it before we paint).
                let row_msg_id = item.id();
                let refresh = {
                    let avatar = avatar.clone();
                    let li = list_item.clone();
                    let files = files.clone();
                    move |item: &MessageObject| {
                        // Recycle guard: only paint if this row still shows the
                        // message we bound. `item` here is the emitting object,
                        // so compare the ListItem's CURRENT item against the id
                        // captured at bind time.
                        if li.item().and_downcast::<MessageObject>().map(|m| m.id())
                            != Some(row_msg_id)
                        {
                            return;
                        }
                        let file_id = item.avatar_file_id();
                        if file_id == 0 {
                            return;
                        }
                        if let Some(path) = files.cached(file_id) {
                            if let Ok(texture) = gtk::gdk::Texture::from_filename(&path) {
                                avatar.set_custom_image(Some(&texture));
                            }
                        } else {
                            // Recycle-safe: only paint if this row still shows the
                            // message we started the download for.
                            let avatar2 = avatar.clone();
                            let li = li.clone();
                            files.download(file_id, 12, move |path| {
                                if li.item().and_downcast::<MessageObject>().map(|m| m.id())
                                    != Some(row_msg_id)
                                {
                                    return;
                                }
                                if let Ok(texture) = gtk::gdk::Texture::from_filename(&path) {
                                    avatar2.set_custom_image(Some(&texture));
                                }
                            });
                        }
                    }
                };
                // Run once for the value already present at bind time.
                refresh(&item);
                // React to the async resolution; `refresh` itself re-checks that
                // this row still shows the bound message before painting.
                let handler = item.connect_notify_local(Some("avatar-file-id"), move |item, _| {
                    let item = item.downcast_ref::<MessageObject>().expect("MessageObject");
                    refresh(item);
                });
                store_avatar_notify(&avatar, &item, handler);
            } else {
                // Earlier row of a run: keep the width, hide the drawing.
                avatar.set_visible(false);
            }
        } else {
            // Outgoing or 1:1: collapse the slot entirely.
            avatar.set_visible(false);
            avatar.set_size(0);
            avatar.set_custom_image(gtk::gdk::Paintable::NONE);
        }
    }

    // Photo / album grid vs. text body.
    let kind = item.kind();
    let is_voice = kind == kind::VOICE;
    // Locate the single picture by its stable css class (album cells carry a
    // different class, so this never returns an album cell).
    let single_pic = find_picture_with_css(&root, "msg-photo-single");

    if album_id != 0 && is_album_first {
        // Album-first row: render the whole run as a tiled grid.
        // Hide the single picture.
        if let Some(picture) = single_pic.as_ref() {
            picture.set_visible(false);
            picture.set_paintable(gtk::gdk::Paintable::NONE);
            picture.set_size_request(-1, -1);
        }
        // Collect (file_id) for each photo member of the run, in order.
        let mut cells: Vec<i32> = Vec::new();
        let n = store.n_items();
        let mut p = pos;
        while p < n {
            match store.item(p).and_downcast::<MessageObject>() {
                Some(m) if m.media_album_id() == album_id => {
                    if m.kind() == kind::PHOTO {
                        cells.push(m.photo_file_id());
                    }
                    p += 1;
                }
                _ => break,
            }
        }
        if let Some(grid) = find::<gtk::Grid>(&root, "album-grid") {
            // (Grid was already cleared in the recycle reset above.)
            let n_cells = cells.len();
            let cols: usize = match n_cells {
                0 | 1 => 1,
                2 => 2,
                3 => 3,
                4 => 2,
                _ => 3,
            };
            // Cell size so cols * cell + gaps <= ~320 px.
            let cell = ((320 - (cols as i32 - 1) * 2) / cols as i32).max(60);
            for (idx, &file_id) in cells.iter().enumerate() {
                let pic = gtk::Picture::builder()
                    .content_fit(gtk::ContentFit::Cover)
                    .can_shrink(true)
                    .build();
                pic.add_css_class("msg-album-cell");
                pic.set_widget_name(&format!("photo-{file_id}"));
                pic.set_size_request(cell, cell);
                let col = (idx % cols) as i32;
                let rowi = (idx / cols) as i32;
                grid.attach(&pic, col, rowi, 1, 1);
                // Recycle-safe download/paint.
                if let Some(path) = files.cached(file_id) {
                    if let Ok(texture) = gtk::gdk::Texture::from_filename(&path) {
                        pic.set_paintable(Some(&texture));
                    }
                } else {
                    let want = item.id();
                    let pic2 = pic.clone();
                    let li = list_item.clone();
                    files.download(file_id, 8, move |path| {
                        if li.item().and_downcast::<MessageObject>().map(|m| m.id())
                            != Some(want)
                        {
                            return;
                        }
                        if let Ok(texture) = gtk::gdk::Texture::from_filename(&path) {
                            pic2.set_paintable(Some(&texture));
                        }
                    });
                }
            }
            grid.set_visible(true);
        }
    } else if kind == kind::PHOTO {
        // Single (non-album) photo — unchanged behaviour, only the lookup helper
        // differs (find_picture_with_css instead of find_first).
        if let Some(picture) = single_pic.as_ref() {
            picture.set_visible(true);
            size_picture(picture, item.photo_width(), item.photo_height());
            let file_id = item.photo_file_id();
            picture.set_widget_name(&format!("photo-{file_id}"));
            picture.set_paintable(gtk::gdk::Paintable::NONE);
            if let Some(path) = files.cached(file_id) {
                if let Ok(texture) = gtk::gdk::Texture::from_filename(&path) {
                    picture.set_paintable(Some(&texture));
                }
            } else {
                let want = item.id();
                let picture2 = picture.clone();
                let li = list_item.clone();
                files.download(file_id, 8, move |path| {
                    if li.item().and_downcast::<MessageObject>().map(|m| m.id()) != Some(want) {
                        return;
                    }
                    if let Ok(texture) = gtk::gdk::Texture::from_filename(&path) {
                        picture2.set_paintable(Some(&texture));
                    }
                });
            }
        }
    } else {
        // Not a photo: hide + reset the single picture.
        if let Some(picture) = single_pic.as_ref() {
            picture.set_visible(false);
            picture.set_paintable(gtk::gdk::Paintable::NONE);
            picture.set_size_request(-1, -1);
            picture.set_widget_name("photo");
        }
    }

    // Voice note: render the player controls; the generic body text is hidden.
    if is_voice {
        if let Some(vbox) = find::<gtk::Box>(&root, "voice") {
            vbox.set_visible(true);
            let duration = item.voice_duration().max(0);
            let file_id = item.voice_file_id();

            // Reset controls to a stopped state on (re)bind. The scale range is
            // kept in SECONDS; a min of 1 avoids a zero-length range.
            if let Some(scale) = find::<gtk::Scale>(&root, "voice-scale") {
                scale.set_range(0.0, duration.max(1) as f64);
                scale.set_value(0.0);
            }
            if let Some(tl) = find::<gtk::Label>(&root, "voice-time") {
                tl.set_text(&crate::models::message_object::format_duration(duration));
            }
            if let Some(btn) = find::<gtk::Button>(&root, "voice-play") {
                btn.set_icon_name("media-playback-start-symbolic");
            }

            // (Re)install fresh, recycle-safe click + seek handlers. Any handler
            // left over from the previous item shown in this row is disconnected
            // first (see wire_voice_controls).
            wire_voice_controls(&root, list_item, voice, files, file_id, duration);
        }
    }

    // Body text: for photos, show only a non-empty caption; else the text.
    // Non-empty bodies render as markup with http/https URLs linkified; all
    // non-URL text is markup-escaped so message content can't inject markup.
    if let Some(body) = find::<gtk::Label>(&root, "body") {
        let mut is_text_body = false;
        let text = if is_voice {
            String::new()
        } else if album_id != 0 && is_album_first {
            // Album caption: first non-empty media_caption in the run.
            let mut cap = String::new();
            let n = store.n_items();
            let mut p = pos;
            while p < n {
                match store.item(p).and_downcast::<MessageObject>() {
                    Some(m) if m.media_album_id() == album_id => {
                        let c = m.media_caption();
                        if !c.is_empty() {
                            cap = c;
                            break;
                        }
                        p += 1;
                    }
                    _ => break,
                }
            }
            cap
        } else if kind == kind::PHOTO {
            item.media_caption()
        } else {
            is_text_body = true;
            item.content_text()
        };
        if text.is_empty() {
            body.set_visible(false);
        } else {
            body.set_visible(true);
            body.set_use_markup(true);
            body.set_markup(&linkify(&text));
        }
        // Reactive body repaint on edit: when a text message is edited, TDLib
        // fires Update::MessageContent → set_content → set_content_text, which
        // emits `notify::content-text`. Re-render the body markup in place (no
        // items_changed, so no scroll jump). Only for the plain text-body case;
        // photo/voice/album bodies don't track content-text. The handler is
        // stashed on the body label and torn down on the next bind / unbind
        // (clear_content_notify), so a recycled row never reacts to a stale item.
        if is_text_body {
            let row_msg_id = item.id();
            let body_ref = body.clone();
            let li = list_item.clone();
            let handler = item.connect_notify_local(Some("content-text"), move |item, _| {
                let item = item.downcast_ref::<MessageObject>().expect("MessageObject");
                // Recycle guard: skip if this row no longer shows the bound message.
                if li.item().and_downcast::<MessageObject>().map(|m| m.id()) != Some(row_msg_id) {
                    return;
                }
                let text = item.content_text();
                if text.is_empty() {
                    body_ref.set_visible(false);
                } else {
                    body_ref.set_visible(true);
                    body_ref.set_use_markup(true);
                    body_ref.set_markup(&linkify(&text));
                }
            });
            store_content_notify(&body, &item, handler);
        }
    }

    if let Some(time) = find::<gtk::Label>(&root, "time") {
        time.set_text(&crate::format::message_time(item.date()));
    }

    // Sent/read indicator: outgoing only. Drive its text via a property binding
    // to `send-status` so it updates live when the chat view promotes the
    // message to "read" (e.g. on updateChatReadOutbox) without an items_changed.
    if let Some(status) = find::<gtk::Label>(&root, "status") {
        // Tear down any binding left over from a previous item in this recycled
        // row before (re)binding, so we never leave two bindings driving one
        // label or a binding pointing at a stale MessageObject.
        clear_status_binding(&status);
        if outgoing {
            status.set_visible(true);
            status.set_text(send_status_glyph(item.send_status()));
            let binding = item
                .bind_property("send-status", &status, "label")
                .transform_to(|_, status: i32| Some(send_status_glyph(status)))
                .sync_create()
                .build();
            store_status_binding(&status, binding);
        } else {
            status.set_visible(false);
            status.set_text("");
        }
    }

    // Reaction chips: rebuild the chip row from the item's `reactions` property
    // now, and install a `notify::reactions` handler so live changes (arriving
    // via `Update::MessageInteractionInfo`) re-render the chips reactively —
    // items_changed does NOT reliably repaint rows during update storms. The
    // handler is stashed on the chips box and torn down on the next bind
    // (clear_reactions_notify), so a recycled row never reacts to a stale item.
    if let Some(chips) = find::<gtk::Box>(&root, "reactions") {
        clear_reactions_notify(&chips);
        // Render whatever is already present at bind time.
        render_reaction_chips(&chips, &item.reactions(), &tap_target);
        // Then react to async / live updates on this exact item.
        let chips_ref = chips.clone();
        let target = tap_target;
        let li = list_item.clone();
        let handler = item.connect_notify_local(Some("reactions"), move |item, _| {
            let item = item.downcast_ref::<MessageObject>().expect("MessageObject");
            // Recycle guard: skip if this row no longer shows the bound message
            // (`target` == tap_target == the message id captured at bind time).
            if li.item().and_downcast::<MessageObject>().map(|m| m.id()) != Some(target) {
                return;
            }
            render_reaction_chips(&chips_ref, &item.reactions(), &target);
        });
        store_reactions_notify(&chips, &item, handler);
    }
}

/// Wire the voice row's play/pause button and seekable scale, with recycle-safe
/// handler teardown. Called on every bind of a voice message; it disconnects the
/// previous item's click/seek handlers before installing fresh ones, and starts
/// playback (from cache or after a download) on click, driving the button/scale/
/// time-label from the [`VoicePlayer`]'s per-event callback.
fn wire_voice_controls(
    root: &gtk::Widget,
    list_item: &gtk::ListItem,
    voice: &VoicePlayer,
    files: &FileStore,
    file_id: i32,
    duration: i32,
) {
    let _ = duration;
    let btn = match find::<gtk::Button>(root, "voice-play") { Some(b) => b, None => return };
    let scale = match find::<gtk::Scale>(root, "voice-scale") { Some(s) => s, None => return };
    let time = match find::<gtk::Label>(root, "voice-time") { Some(t) => t, None => return };
    let vbox = match find::<gtk::Box>(root, "voice") { Some(v) => v, None => return };

    // Recycle guard: the item id this row currently shows. Async work (download)
    // only applies if the row still shows this same message when it completes.
    let want = list_item
        .item()
        .and_downcast::<MessageObject>()
        .map(|m| m.id())
        .unwrap_or(0);

    // Per-widget drag guard, reused across binds so the (once-installed) drag
    // gesture and the (re-installed each bind) change-value + event handlers all
    // share the same flag. Get-or-create it on the scale.
    let seeking: Rc<Cell<bool>> = unsafe {
        match scale.data::<Rc<Cell<bool>>>("paloma-voice-seeking") {
            Some(ptr) => ptr.as_ref().clone(),
            None => {
                let s = Rc::new(Cell::new(false));
                scale.set_data("paloma-voice-seeking", s.clone());
                s
            }
        }
    };

    // A single closure that, given the downloaded/cached local path, builds the
    // uri, starts playback, stashes the returned token on the vbox, and wires an
    // event callback that drives the button/scale/time widgets. Reused by the
    // cached (sync) and download (async) paths below. Wrapped in an `Rc` so it
    // can be cloned into both paths and called multiple times.
    let start_playback = {
        let voice = voice.clone();
        let btn = btn.clone();
        let scale = scale.clone();
        let time = time.clone();
        let vbox = vbox.clone();
        let seeking = seeking.clone();
        move |path: std::path::PathBuf| {
            let uri = match file_uri(&path) {
                Some(u) => u,
                None => return,
            };
            // Clones captured by the per-event callback (Fn + 'static, runs on
            // the main thread — the !Send widgets are fine here).
            let btn_ev = btn.clone();
            let scale_ev = scale.clone();
            let time_ev = time.clone();
            let vbox_ev = vbox.clone();
            let seeking_ev = seeking.clone();
            let token = voice.start(&uri, move |ev| match ev {
                VoiceEvent::Duration(ns) => {
                    let secs = (ns / 1_000_000_000) as i32;
                    scale_ev.set_range(0.0, secs.max(1) as f64);
                }
                VoiceEvent::Position(ns) => {
                    // Don't move the thumb while the user is dragging it.
                    if !seeking_ev.get() {
                        let secs_f = ns as f64 / 1_000_000_000.0;
                        scale_ev.set_value(secs_f);
                        time_ev.set_text(
                            &crate::models::message_object::format_duration(secs_f as i32),
                        );
                    }
                }
                VoiceEvent::Playing(playing) => {
                    btn_ev.set_icon_name(if playing {
                        "media-playback-pause-symbolic"
                    } else {
                        "media-playback-start-symbolic"
                    });
                }
                VoiceEvent::Ended => {
                    // Reset the controls and drop the stashed token so a later
                    // click starts a fresh playback rather than toggling a dead
                    // stream. Safe to run here: the Ended callback only touches
                    // widgets + the token stash, never voice.start/stop.
                    btn_ev.set_icon_name("media-playback-start-symbolic");
                    scale_ev.set_value(0.0);
                    take_voice_token(&vbox_ev);
                }
            });
            set_voice_token(&vbox, token);
        }
    };
    let start_playback = Rc::new(start_playback);

    // (Re)install the play/pause click handler, tearing down any handler left
    // over from the previous item shown in this recycled row first.
    unsafe {
        if let Some(old) = btn.steal_data::<glib::SignalHandlerId>(VOICE_CLICK_KEY) {
            btn.disconnect(old);
        }
    }
    {
        let voice = voice.clone();
        let files = files.clone();
        let vbox_click = vbox.clone();
        let li = list_item.clone();
        let start_playback = start_playback.clone();
        let id = btn.connect_clicked(move |_| {
            // If this row still owns a live stream, toggle it. Peek the stashed
            // token without consuming it (steal then re-stash).
            let stashed = take_voice_token(&vbox_click);
            if let Some(token) = stashed {
                if voice.is_owner(token) {
                    set_voice_token(&vbox_click, token); // put it back
                    voice.toggle(token);
                    return;
                }
                // else: stale token from a finished/evicted stream; drop it.
            }
            // Start fresh. Resolve the file (cache hit → immediate; miss →
            // download, recycle-guarded by the row's current item id).
            if let Some(path) = files.cached(file_id) {
                (start_playback)(path);
            } else {
                let li2 = li.clone();
                let start_playback = start_playback.clone();
                files.download(file_id, 16, move |path| {
                    // Recycle guard: only start if this row still shows the same
                    // message it did when the download was requested.
                    if li2.item().and_downcast::<MessageObject>().map(|m| m.id()) != Some(want) {
                        return;
                    }
                    (start_playback)(path);
                });
            }
        });
        unsafe {
            btn.set_data(VOICE_CLICK_KEY, id);
        }
    }

    // (Re)install the scale's change-value seek handler, tearing down the prior
    // one first so a recycled scale doesn't fire the previous item's closure.
    unsafe {
        if let Some(old) = scale.steal_data::<glib::SignalHandlerId>(VOICE_SEEK_KEY) {
            scale.disconnect(old);
        }
    }
    {
        let voice = voice.clone();
        let vbox_seek = vbox.clone();
        let seeking_change = seeking.clone();
        let id = scale.connect_change_value(move |_scale, _scroll, value| {
            // Only seek while the user is actively dragging (guard set by the
            // press gesture); otherwise this fires from our own PositionUpdated
            // `set_value` and would fight the pipeline.
            if seeking_change.get() {
                if let Some(token) = take_voice_token(&vbox_seek) {
                    // Peek: put the token back, then seek if we still own it.
                    set_voice_token(&vbox_seek, token);
                    if voice.is_owner(token) {
                        let ns = (value.max(0.0) * 1_000_000_000.0) as u64;
                        voice.seek(token, ns);
                    }
                }
            }
            glib::Propagation::Proceed
        });
        unsafe {
            scale.set_data(VOICE_SEEK_KEY, id);
        }
    }

    // Install the drag-tracking gesture exactly once per recycled scale widget.
    // It captures the per-widget `seeking` (stable across binds) so it keeps
    // working after rebinds without stacking duplicate gestures.
    let already_wired = unsafe { scale.data::<bool>("paloma-voice-drag").is_some() };
    if !already_wired {
        unsafe {
            scale.set_data("paloma-voice-drag", true);
        }
        let press = gtk::GestureClick::new();
        let seeking_press = seeking.clone();
        press.connect_pressed(move |_, _, _, _| seeking_press.set(true));
        let seeking_release = seeking.clone();
        let voice_release = voice.clone();
        let scale_release = scale.clone();
        let vbox_release = vbox.clone();
        press.connect_released(move |_, _, _, _| {
            // Final seek to the released position, then release the guard.
            if let Some(token) = take_voice_token(&vbox_release) {
                set_voice_token(&vbox_release, token);
                if voice_release.is_owner(token) {
                    let ns = (scale_release.value().max(0.0) * 1_000_000_000.0) as u64;
                    voice_release.seek(token, ns);
                }
            }
            seeking_release.set(false);
        });
        scale.add_controller(press);
    }
}

/// The store item `delta` positions from `pos` (bounds-checked).
fn neighbour(store: &gio::ListStore, pos: u32, delta: i32) -> Option<MessageObject> {
    let target = if delta < 0 {
        pos.checked_sub((-delta) as u32)?
    } else {
        pos.checked_add(delta as u32)?
    };
    store.item(target).and_downcast::<MessageObject>()
}

/// Whether `a` groups with its predecessor `b`: same sender, same direction,
/// within [`GROUP_WINDOW_SECS`]. Missing neighbours never group.
fn grouped_with(a: Option<&MessageObject>, b: Option<&MessageObject>) -> bool {
    let (a, b) = match (a, b) {
        (Some(a), Some(b)) => (a, b),
        _ => return false,
    };
    a.is_outgoing() == b.is_outgoing()
        && a.sender_id() == b.sender_id()
        && (a.date() - b.date()).abs() <= GROUP_WINDOW_SECS
}

/// Constrain the picture to a chat-friendly box while preserving aspect ratio.
fn size_picture(picture: &gtk::Picture, width: i32, height: i32) {
    const MAX_W: i32 = 320;
    const MAX_H: i32 = 400;
    if width <= 0 || height <= 0 {
        picture.set_size_request(MAX_W, 240);
        return;
    }
    let scale = (MAX_W as f64 / width as f64)
        .min(MAX_H as f64 / height as f64)
        .min(1.0);
    let w = (width as f64 * scale).round() as i32;
    let h = (height as f64 * scale).round() as i32;
    picture.set_size_request(w.max(1), h.max(1));
}

/// Data key under which the sender avatar stashes its current
/// `notify::avatar-file-id` `SignalHandlerId` (plus the object it is connected
/// to), disconnected before a rebind installs a fresh handler so recycled rows
/// never accumulate or cross-wire to a stale MessageObject.
const AVATAR_NOTIFY_KEY: &str = "paloma-avatar-notify";

/// Stash the avatar's live `avatar-file-id` notify handler (replacing any prior
/// one, which is disconnected first).
fn store_avatar_notify(avatar: &adw::Avatar, item: &MessageObject, handler: glib::SignalHandlerId) {
    clear_avatar_notify(avatar);
    unsafe {
        avatar.set_data(AVATAR_NOTIFY_KEY, (item.clone(), handler));
    }
}

/// Disconnect and drop any `avatar-file-id` notify handler previously stashed on
/// `avatar`, so a recycled row stops reacting to its old MessageObject.
fn clear_avatar_notify(avatar: &adw::Avatar) {
    unsafe {
        if let Some((item, handler)) =
            avatar.steal_data::<(MessageObject, glib::SignalHandlerId)>(AVATAR_NOTIFY_KEY)
        {
            item.disconnect(handler);
        }
    }
}

/// Data key under which a row's body [`gtk::Label`] stashes its live
/// `notify::content-text` `SignalHandlerId` (plus the object it is connected to),
/// disconnected before a rebind installs a fresh handler so recycled rows never
/// accumulate handlers or cross-wire to a stale MessageObject. Drives live
/// re-render of the body markup when a message is edited (Update::MessageContent).
const CONTENT_NOTIFY_KEY: &str = "paloma-content-notify";

/// Stash the body label's live `content-text` notify handler (replacing any prior
/// one, which is disconnected first).
fn store_content_notify(body: &gtk::Label, item: &MessageObject, handler: glib::SignalHandlerId) {
    clear_content_notify(body);
    unsafe {
        body.set_data(CONTENT_NOTIFY_KEY, (item.clone(), handler));
    }
}

/// Disconnect and drop any `content-text` notify handler previously stashed on
/// `body`, so a recycled row stops reacting to its old MessageObject.
fn clear_content_notify(body: &gtk::Label) {
    unsafe {
        if let Some((item, handler)) =
            body.steal_data::<(MessageObject, glib::SignalHandlerId)>(CONTENT_NOTIFY_KEY)
        {
            item.disconnect(handler);
        }
    }
}

/// Data key under which a row's reaction-chips box stashes its live
/// `notify::reactions` `SignalHandlerId` (plus the object it is connected to),
/// disconnected before a rebind installs a fresh handler so recycled rows never
/// accumulate handlers or cross-wire to a stale MessageObject.
const REACTIONS_NOTIFY_KEY: &str = "paloma-reactions-notify";

/// Stash the chips box's live `reactions` notify handler (replacing any prior
/// one, which is disconnected first).
fn store_reactions_notify(chips: &gtk::Box, item: &MessageObject, handler: glib::SignalHandlerId) {
    clear_reactions_notify(chips);
    unsafe {
        chips.set_data(REACTIONS_NOTIFY_KEY, (item.clone(), handler));
    }
}

/// Disconnect and drop any `reactions` notify handler previously stashed on
/// `chips`, so a recycled row stops reacting to its old MessageObject.
fn clear_reactions_notify(chips: &gtk::Box) {
    unsafe {
        if let Some((item, handler)) =
            chips.steal_data::<(MessageObject, glib::SignalHandlerId)>(REACTIONS_NOTIFY_KEY)
        {
            item.disconnect(handler);
        }
    }
}

/// Rebuild the reaction-chips row `chips` from the encoded `reactions` string.
/// Clears any existing chip buttons, then adds one button per emoji reaction
/// (emoji + count), highlighting chips the current user chose. Each chip
/// activates `react.toggle-reaction` with a `(message_id, emoji)` target, so a
/// tap toggles the user's reaction on that emoji. The whole row is hidden when
/// the message has no reactions.
fn render_reaction_chips(chips: &gtk::Box, encoded: &str, message_id: &i64) {
    // Clear previously-rendered chips (recycled row / live update).
    while let Some(child) = chips.first_child() {
        chips.remove(&child);
    }
    let decoded = decode_reactions(encoded);
    if decoded.is_empty() {
        chips.set_visible(false);
        return;
    }
    chips.set_visible(true);
    for chip in decoded {
        let label = format!("{} {}", chip.emoji, chip.count);
        let button = gtk::Button::builder()
            .label(&label)
            .css_classes(["msg-reaction-chip"])
            .build();
        if chip.is_chosen {
            button.add_css_class("chosen");
        }
        // Toggle the current user's reaction on this emoji via the list view's
        // `react.toggle-reaction` action, parameterized by (message_id, emoji).
        let target = (*message_id, chip.emoji.clone()).to_variant();
        button.set_action_name(Some("react.toggle-reaction"));
        button.set_action_target_value(Some(&target));
        chips.append(&button);
    }
}

/// Combine first/last name into a single display string.
fn display_name(first: &str, last: &str) -> String {
    let name = format!("{first} {last}");
    let trimmed = name.trim();
    if trimmed.is_empty() {
        "Unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

/// A short single-line snippet of a message body for reply headers/strips.
fn snippet(text: &str) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX: usize = 80;
    if one_line.chars().count() > MAX {
        let cut: String = one_line.chars().take(MAX).collect();
        format!("{cut}…")
    } else {
        one_line
    }
}

/// Whether two Unix timestamps (seconds) fall on the same local calendar day.
fn same_local_day(a: i64, b: i64) -> bool {
    match (
        glib::DateTime::from_unix_local(a),
        glib::DateTime::from_unix_local(b),
    ) {
        (Ok(a), Ok(b)) => {
            a.year() == b.year()
                && a.month() == b.month()
                && a.day_of_month() == b.day_of_month()
        }
        _ => false,
    }
}

/// Render `text` as Pango markup, turning `http://` / `https://` URLs into
/// clickable `<a>` links. Every non-URL run and every URL is passed through
/// `glib::markup_escape_text`, so arbitrary message text (including `<`, `&`,
/// `>`) cannot inject markup.
fn linkify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0usize;
    // Byte offset of the start of the current pending non-URL run.
    let mut run_start = 0usize;

    while i < text.len() {
        let rest = &text[i..];
        if rest.starts_with("http://") || rest.starts_with("https://") {
            // Flush the escaped non-URL run before this URL.
            if run_start < i {
                out.push_str(&glib::markup_escape_text(&text[run_start..i]));
            }
            // A URL runs until the next ASCII whitespace.
            let mut j = i;
            while j < text.len() && !bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let url = &text[i..j];
            let esc = glib::markup_escape_text(url);
            out.push_str("<a href=\"");
            out.push_str(&esc);
            out.push_str("\">");
            out.push_str(&esc);
            out.push_str("</a>");
            i = j;
            run_start = j;
        } else {
            // Advance one char (URLs only start at an ASCII 'h', so stepping by
            // one byte here is safe — we never split inside a multibyte char
            // because the next `starts_with` check re-anchors at a char start).
            let ch_len = rest.chars().next().map(char::len_utf8).unwrap_or(1);
            i += ch_len;
        }
    }
    // Flush the trailing non-URL run.
    if run_start < text.len() {
        out.push_str(&glib::markup_escape_text(&text[run_start..]));
    }
    out
}

/// Depth-first search for the first descendant of `root` whose widget name is
/// `name` and which downcasts to `T`.
fn find<T: IsA<gtk::Widget>>(root: &gtk::Widget, name: &str) -> Option<T> {
    let mut child = root.first_child();
    while let Some(c) = child {
        if c.widget_name() == name {
            if let Ok(w) = c.clone().downcast::<T>() {
                return Some(w);
            }
        }
        if let Some(found) = find::<T>(&c, name) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}

/// Depth-first search for the first descendant of `root` that downcasts to `T`,
/// ignoring widget names. Used for the row's single [`gtk::Picture`], whose name
/// carries a per-bind file id rather than a stable lookup key.
#[allow(dead_code)]
fn find_first<T: IsA<gtk::Widget>>(root: &gtk::Widget) -> Option<T> {
    let mut child = root.first_child();
    while let Some(c) = child {
        if let Ok(w) = c.clone().downcast::<T>() {
            return Some(w);
        }
        if let Some(found) = find_first::<T>(&c) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}

/// First descendant Picture carrying `css` as a style class.
fn find_picture_with_css(root: &gtk::Widget, css: &str) -> Option<gtk::Picture> {
    let mut child = root.first_child();
    while let Some(c) = child {
        if let Ok(p) = c.clone().downcast::<gtk::Picture>() {
            if p.has_css_class(css) {
                return Some(p);
            }
        }
        if let Some(found) = find_picture_with_css(&c, css) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}
