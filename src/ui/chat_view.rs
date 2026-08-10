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
    InputMessageContent, InputMessageReplyTo, Message as MessageEnum, MessageProperties as MessagePropsEnum,
    Messages, MessageSender, Update, User as UserEnum,
};
use tdlib_rs::functions;
use tdlib_rs::types::{FormattedText, InputMessageReplyToMessage, InputMessageText};

use crate::models::message_object::{kind, send_status, send_status_glyph};
use crate::models::MessageObject;
use crate::tdlib::{FileStore, TdClient};

/// Messages sent within this many seconds of each other by the same sender are
/// visually grouped (name hidden, one trailing avatar).
const GROUP_WINDOW_SECS: i64 = 300;

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
    list_view: gtk::ListView,
    scroller: gtk::ScrolledWindow,
    entry: gtk::TextView,
    /// Reply-preview strip shown above the compose entry while replying.
    reply_bar: gtk::Revealer,
    reply_bar_name: gtk::Label,
    reply_bar_text: gtk::Label,
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
}

impl ChatView {
    /// Build (but do not yet open) a chat view for `chat_id`.
    /// Call [`ChatView::open`] once it is on screen to start streaming.
    pub fn new(client: TdClient, chat_id: i64) -> Self {
        let files = client.files();
        let store = gio::ListStore::new::<MessageObject>();

        // --- Row factory: one recycled row per visible slot. -----------------
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(build_row);

        let this_store = store.clone();
        let bind_files = files.clone();
        factory.connect_bind(move |_, list_item| {
            bind_row(list_item, &this_store, &bind_files);
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
            .icon_name("paper-plane-symbolic")
            .valign(gtk::Align::End)
            .css_classes(["circular", "suggested-action", "msg-send"])
            .tooltip_text("Send")
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

        // Reply strip stacked above the compose row.
        let compose = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        compose.add_css_class("msg-compose");
        compose.append(&reply_bar);
        compose.append(&compose_row);

        // History fills the space; compose bar pinned at the bottom.
        let toolbar = adw::ToolbarView::new();
        toolbar.set_content(Some(&scroller));
        toolbar.add_bottom_bar(&compose);

        let inner = Rc::new(Inner {
            client,
            files,
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
            list_view,
            scroller,
            entry,
            reply_bar,
            reply_bar_name,
            reply_bar_text,
            sub_task: RefCell::new(None),
            temp_ids: RefCell::new(HashSet::new()),
            last_read_outbox: Cell::new(0),
        });

        let this = ChatView {
            root: toolbar.upcast(),
            inner,
        };

        this.wire_send(&send_button);
        this.wire_scroll_paging();
        this.wire_row_menu();
        {
            let this2 = this.clone();
            reply_cancel.connect_clicked(move |_| this2.clear_reply());
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
            // Grouping flags depend on neighbours; re-bind the whole loaded run.
            self.rebind_all();
            self.scroll_to_bottom();
            self.mark_visible_read(&ordered);
        } else {
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
                } else if !self.inner.index.borrow().contains_key(&new_id) {
                    let obj = MessageObject::from_message(&u.message);
                    let pos = self.insert_sorted(&obj);
                    self.inner.index.borrow_mut().insert(new_id, obj);
                    self.rebind_around(pos);
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
                    obj.set_content(&u.new_content);
                    self.notify_changed(u.message_id);
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
            _ => {}
        }
    }

    /// Remove a message row by id, if present.
    fn remove_message(&self, id: i64) {
        let removed = self.inner.index.borrow_mut().remove(&id);
        if removed.is_none() {
            return;
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
    fn apply_sender_name(
        &self,
        obj: &MessageObject,
        sender: &MessageSender,
        to_resolve: &mut Vec<i64>,
    ) {
        if obj.is_outgoing() {
            return;
        }
        // Sender names are only shown in group chats.
        if !self.inner.is_group.get() {
            return;
        }
        if let MessageSender::User(u) = sender {
            if let Some(name) = self.inner.names.borrow().get(&u.user_id) {
                obj.set_sender_name(name.clone());
            } else {
                to_resolve.push(u.user_id);
            }
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

    /// Resolve display names for the given user ids, then re-apply.
    fn resolve_names(&self, user_ids: Vec<i64>) {
        // Sender names are only shown in group chats.
        if !self.inner.is_group.get() {
            return;
        }
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
                        this.apply_name_to_rows(uid, &name);
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
                    let id = msg.id;
                    if !this.inner.index.borrow().contains_key(&id) {
                        let obj = MessageObject::from_message(&msg);
                        this.inner.temp_ids.borrow_mut().insert(id);
                        let pos = this.insert_sorted(&obj);
                        this.inner.index.borrow_mut().insert(id, obj);
                        this.resolve_replies(&[id]);
                        this.rebind_around(pos);
                        this.scroll_to_bottom();
                    }
                }
                Err(e) => {
                    tracing::warn!(code = e.code, msg = %e.message, "send_message failed");
                }
            },
        );
    }

    /// Arm the compose reply-strip to reply to `message_id`.
    fn start_reply(&self, message_id: i64) {
        let obj = match self.inner.index.borrow().get(&message_id).cloned() {
            Some(o) => o,
            None => return,
        };
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

    /// Disarm the compose reply-strip.
    fn clear_reply(&self) {
        self.inner.reply_to.set(0);
        self.inner.reply_bar.set_reveal_child(false);
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
        // Right-click (secondary button) → context menu at the pointer.
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

    /// Identify the row under (`x`,`y`) inside the list view and pop a menu.
    fn popup_menu_at(&self, list_view: &gtk::Widget, x: f64, y: f64) {
        let message_id = match self.message_id_at(list_view, x, y) {
            Some(id) => id,
            None => return,
        };

        let menu = gio::Menu::new();
        menu.append(Some("Reply"), Some("msg.reply"));
        menu.append(Some("Copy"), Some("msg.copy"));
        menu.append(Some("Delete"), Some("msg.delete"));

        let group = gio::SimpleActionGroup::new();
        let reply = gio::SimpleAction::new("reply", None);
        let copy = gio::SimpleAction::new("copy", None);
        let delete = gio::SimpleAction::new("delete", None);
        {
            let this = self.clone();
            reply.connect_activate(move |_, _| this.start_reply(message_id));
        }
        {
            let this = self.clone();
            copy.connect_activate(move |_, _| this.copy_message(message_id));
        }
        {
            let this = self.clone();
            delete.connect_activate(move |_, _| this.delete_message(message_id));
        }
        group.add_action(&reply);
        group.add_action(&copy);
        group.add_action(&delete);

        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(list_view);
        popover.insert_action_group("msg", Some(&group));
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.set_has_arrow(false);
        // Detach the popover from the widget tree once dismissed.
        popover.connect_closed(|p| p.unparent());
        popover.popup();
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

/// Build one recycled row: `[bubble{ reply, sender, body, photo, caption,
/// time }]`. Widgets are named so `bind_row` can retrieve them.
fn build_row(_: &gtk::SignalListItemFactory, list_item: &glib::Object) {
    let list_item = list_item
        .downcast_ref::<gtk::ListItem>()
        .expect("list item is a ListItem");

    // Full-width row holding a single bubble; the bubble's own halign pushes it
    // to one side.
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    row.set_widget_name("msg-row");

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

    // Photo (hidden unless the message is a photo).
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Cover)
        .can_shrink(true)
        .build();
    picture.add_css_class("msg-photo");
    picture.set_widget_name("photo");
    picture.set_visible(false);
    bubble.append(&picture);

    let body = gtk::Label::builder()
        .css_classes(["msg-body"])
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .selectable(true)
        .max_width_chars(36)
        .build();
    body.set_widget_name("body");
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

    row.append(&bubble);
    list_item.set_child(Some(&row));
}

/// Bind a [`MessageObject`] onto a recycled row, computing grouping from the
/// neighbouring rows in `store` and resolving media via `files`.
fn bind_row(list_item: &glib::Object, store: &gio::ListStore, files: &FileStore) {
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

    let outgoing = item.is_outgoing();
    // Stash the message id on the row so the context-menu gesture can find it.
    row.set_widget_name(&format!("msg-row-{}", item.id()));

    // Grouping: look at neighbours in the model (list positions, not ids, since
    // the store is already ascending-sorted by id === chronological).
    let pos = list_item.position();
    let prev = neighbour(store, pos, -1);
    let show_sender = !grouped_with(Some(&item), prev.as_ref());

    row.set_halign(gtk::Align::Fill);

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

    // Photo vs. text body.
    let kind = item.kind();
    if let Some(picture) = find_first::<gtk::Picture>(&root) {
        if kind == kind::PHOTO {
            picture.set_visible(true);
            size_picture(&picture, item.photo_width(), item.photo_height());
            let file_id = item.photo_file_id();
            // Stash the file id in the name so the tap handler can read it.
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
                    // Guard against recycling: only paint if this row still
                    // holds the item we started the download for.
                    if li.item().and_downcast::<MessageObject>().map(|m| m.id()) != Some(want) {
                        return;
                    }
                    if let Ok(texture) = gtk::gdk::Texture::from_filename(&path) {
                        picture2.set_paintable(Some(&texture));
                    }
                });
            }
        } else {
            picture.set_visible(false);
            picture.set_paintable(gtk::gdk::Paintable::NONE);
            picture.set_size_request(-1, -1);
            picture.set_widget_name("photo");
        }
    }

    // Body text: for photos, show only a non-empty caption; else the text.
    if let Some(body) = find::<gtk::Label>(&root, "body") {
        let text = if kind == kind::PHOTO {
            item.media_caption()
        } else {
            item.content_text()
        };
        if text.is_empty() {
            body.set_visible(false);
        } else {
            body.set_visible(true);
            body.set_text(&text);
        }
    }

    if let Some(time) = find::<gtk::Label>(&root, "time") {
        time.set_text(&format_time(item.date()));
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
    const MAX_H: i32 = 320;
    if width <= 0 || height <= 0 {
        picture.set_size_request(MAX_W, 200);
        return;
    }
    let scale = (MAX_W as f64 / width as f64)
        .min(MAX_H as f64 / height as f64)
        .min(1.0);
    let w = (width as f64 * scale).round() as i32;
    let h = (height as f64 * scale).round() as i32;
    picture.set_size_request(w.max(1), h.max(1));
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

/// Format a Unix timestamp (seconds) as a short local `HH:MM` label.
fn format_time(unix: i64) -> String {
    let dt = glib::DateTime::from_unix_local(unix)
        .and_then(|d| d.format("%H:%M"))
        .map(|s| s.to_string());
    dt.unwrap_or_default()
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
