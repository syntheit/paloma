//! [`ChatView`]: the message-history pane for one open chat.
//!
//! # Lifecycle (mirrors tgt's `tg_backend.rs` open-chat flow)
//!
//! On [`ChatView::open`] we, in order:
//! 1. `functions::open_chat(chat_id)` — REQUIRED. TDLib only streams message
//!    updates (`updateNewMessage`, read state, `updateMessageContent`, …) and
//!    keeps a chat's message cache warm while it is *open*. Without this the view
//!    would never see live messages.
//! 2. `functions::get_chat_history(chat_id, from_message_id=0, offset=0, limit=50,
//!    only_local=false)` to page in the most recent messages. TDLib's first call
//!    with `from_message_id=0` frequently primes the cache and returns few/no
//!    rows, so we loop a couple of batches (following tgt's
//!    `prepare_to_get_chat_history` + `get_chat_history` idea) using the oldest
//!    returned id as the next `from_message_id`.
//!
//! On [`ChatView::close`] we `functions::close_chat(chat_id)`.
//!
//! # Recycling model
//!
//! A `gtk::ListView` over a `gio::ListStore` of [`MessageObject`]s, driven by a
//! `SignalListItemFactory`. GTK reuses a small pool of bubble widgets — one per
//! visible slot, not one per message. Mutating a live [`MessageObject`] and then
//! firing `items_changed` for its row re-binds only that slot (same trick as the
//! chat list).
//!
//! # Live updates
//!
//! [`ChatView::handle_update`] is fed every raw TDLib update by [`ChatView::open`]'s
//! subscription and acts only on updates for the currently-open chat:
//! * `NewMessage`   → append (dedup: skip ids already present, e.g. our own optimistic echo)
//! * `MessageSendSucceeded { message, old_message_id }` → reconcile the optimistic
//!   temp row (matched by `old_message_id`) to the real message + id
//! * `MessageContent` → replace the body of one row
//! * `DeleteMessages` → remove rows (ignoring `from_cache` evictions, per tgt)

use adw::prelude::*;
use gtk::gio;
use gtk::glib;
use gtk::glib::clone;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use tdlib_rs::enums::{
    InputMessageContent, Message as MessageEnum, Messages, MessageSender, Update, User as UserEnum,
};
use tdlib_rs::functions;
use tdlib_rs::types::{FormattedText, InputMessageText};

use crate::models::MessageObject;
use crate::tdlib::TdClient;

/// The message-history component for a single chat. Cheap to `.clone()` — the
/// widget tree and all state live behind the shared `Rc<Inner>`.
#[derive(Clone)]
pub struct ChatView {
    root: gtk::Widget,
    inner: Rc<Inner>,
}

struct Inner {
    client: TdClient,
    chat_id: i64,
    /// Our own user id, resolved lazily via `get_me`, to distinguish grouping.
    me_id: Cell<i64>,
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
    list_view: gtk::ListView,
    scroller: gtk::ScrolledWindow,
    entry: gtk::TextView,
}

impl ChatView {
    /// Build (but do not yet open) a chat view for `chat_id` titled `title`.
    /// Call [`ChatView::open`] once it is on screen to start streaming.
    pub fn new(client: TdClient, chat_id: i64) -> Self {
        let store = gio::ListStore::new::<MessageObject>();

        // --- Row factory: one recycled bubble per visible slot. --------------
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, list_item| {
            let list_item = list_item
                .downcast_ref::<gtk::ListItem>()
                .expect("list item is a ListItem");

            // Outer row fills the list width; the bubble aligns left/right inside.
            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .build();

            let bubble = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(2)
                .build();
            bubble.add_css_class("msg-bubble");
            bubble.set_widget_name("bubble");

            let sender = gtk::Label::builder()
                .css_classes(["msg-sender"])
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .single_line_mode(true)
                .build();
            sender.set_widget_name("sender");
            bubble.append(&sender);

            let body = gtk::Label::builder()
                .css_classes(["msg-body"])
                .xalign(0.0)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .selectable(true)
                .build();
            body.set_widget_name("body");
            bubble.append(&body);

            let time = gtk::Label::builder()
                .css_classes(["msg-time", "dim-label"])
                .xalign(1.0)
                .build();
            time.set_widget_name("time");
            bubble.append(&time);

            row.append(&bubble);
            list_item.set_child(Some(&row));
        });

        factory.connect_bind(|_, list_item| {
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
            if let Some(sender) = find::<gtk::Label>(&root, "sender") {
                let name = item.sender_name();
                // Sender name only for incoming messages that have a resolved name.
                if !outgoing && !name.is_empty() {
                    sender.set_text(&name);
                    sender.set_visible(true);
                } else {
                    sender.set_visible(false);
                }
            }
            if let Some(body) = find::<gtk::Label>(&root, "body") {
                body.set_text(&item.content_text());
            }
            if let Some(time) = find::<gtk::Label>(&root, "time") {
                time.set_text(&format_time(item.date()));
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

        let compose = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(8)
            .margin_end(8)
            .build();
        compose.add_css_class("msg-compose");
        compose.append(&entry_scroll);
        compose.append(&send_button);

        // History fills the space; compose bar pinned at the bottom via a
        // ToolbarView bottom bar.
        let toolbar = adw::ToolbarView::new();
        toolbar.set_content(Some(&scroller));
        toolbar.add_bottom_bar(&compose);

        let inner = Rc::new(Inner {
            client,
            chat_id,
            me_id: Cell::new(0),
            store,
            index: RefCell::new(HashMap::new()),
            names: RefCell::new(HashMap::new()),
            oldest_id: Cell::new(0),
            reached_top: Cell::new(false),
            loading_older: Cell::new(false),
            list_view,
            scroller,
            entry,
        });

        let this = ChatView {
            root: toolbar.upcast(),
            inner,
        };

        this.wire_send(&send_button);
        this.wire_scroll_paging();
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
    /// and page in the most recent history. Idempotent enough for a single call
    /// right after the view is shown.
    pub fn open(&self) {
        let cid = self.inner.client.client_id();
        let chat_id = self.inner.chat_id;

        // 1. Tell TDLib the chat is open (starts streaming + read tracking).
        crate::runtime::spawn(
            async move { functions::open_chat(chat_id, cid).await },
            |res| {
                if let Err(e) = res {
                    tracing::warn!(code = e.code, msg = %e.message, "open_chat failed");
                }
            },
        );

        // Resolve our own user id for outgoing/grouping decisions.
        let this = self.clone();
        crate::runtime::spawn(
            async move { functions::get_me(cid).await },
            move |res| {
                if let Ok(UserEnum::User(me)) = res {
                    this.inner.me_id.set(me.id);
                }
            },
        );

        // 2. Subscribe to the raw update stream; route open-chat updates.
        let updates = self.inner.client.subscribe();
        let this = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(update) = updates.recv().await {
                this.handle_update(update);
            }
        });

        // 3. Page in the most recent history (two priming batches from id 0).
        self.load_initial_history();
    }

    /// Stop streaming for this chat. Call when the view is popped/replaced.
    pub fn close(&self) {
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

    /// Load the most recent ~50 messages. TDLib's first `get_chat_history` from
    /// id 0 often just primes the cache, so we do a priming call then a real
    /// batch (mirrors tgt's `prepare_to_get_chat_history` + `get_chat_history`).
    fn load_initial_history(&self) {
        let cid = self.inner.client.client_id();
        let chat_id = self.inner.chat_id;
        let this = self.clone();
        crate::runtime::spawn(
            async move {
                // Priming call — result deliberately ignored (may be near-empty).
                let _ = functions::get_chat_history(chat_id, 0, 0, 50, false, cid).await;
                // Real fetch of the most recent window.
                functions::get_chat_history(chat_id, 0, 0, 50, false, cid).await
            },
            move |res| {
                if let Ok(Messages::Messages(msgs)) = res {
                    this.ingest_history(msgs.messages, true);
                }
            },
        );
    }

    /// Page in an older batch, anchored at the oldest loaded id. No-op if we're
    /// already at the top or a request is in flight.
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
            // offset 0 = messages strictly older than `from` (per tgt).
            async move { functions::get_chat_history(chat_id, from, 0, 50, false, cid).await },
            move |res| {
                this.inner.loading_older.set(false);
                if let Ok(Messages::Messages(msgs)) = res {
                    this.ingest_history(msgs.messages, false);
                }
            },
        );
    }

    /// Insert a history batch. TDLib returns newest-first; our store is
    /// oldest-first, so we reverse. `is_initial` controls the empty→top
    /// bookkeeping and read marking.
    fn ingest_history(&self, messages: Vec<Option<tdlib_rs::types::Message>>, is_initial: bool) {
        let batch: Vec<tdlib_rs::types::Message> = messages.into_iter().flatten().collect();
        if batch.is_empty() {
            self.inner.reached_top.set(true);
            return;
        }

        // Newest-first → oldest-first for display.
        let mut ordered = batch;
        ordered.sort_by_key(|m| m.id);

        // Track the new oldest id (batch min).
        if let Some(min) = ordered.first().map(|m| m.id) {
            let cur = self.inner.oldest_id.get();
            if cur == 0 || min < cur {
                self.inner.oldest_id.set(min);
            }
        }

        // Preserve scroll position when prepending older history: remember the
        // current top offset so the viewport doesn't jump.
        let vadj = self.inner.scroller.vadjustment();
        let old_upper = vadj.upper();
        let old_value = vadj.value();

        let mut to_resolve: Vec<i64> = Vec::new();
        for msg in &ordered {
            if self.inner.index.borrow().contains_key(&msg.id) {
                continue;
            }
            let obj = MessageObject::from_message(msg);
            self.apply_sender_name(&obj, &msg.sender_id, &mut to_resolve);
            // Insert keeping the store sorted ascending by id.
            let pos = self.insert_sorted(&obj);
            let _ = pos;
            self.inner.index.borrow_mut().insert(msg.id, obj);
        }

        self.resolve_names(to_resolve);

        if is_initial {
            // Jump to the newest message.
            self.scroll_to_bottom();
            self.mark_visible_read(&ordered);
        } else {
            // Restore the viewport after prepending (keep the same message under
            // the user's eyes). Defer to after layout settles.
            let scroller = self.inner.scroller.clone();
            glib::idle_add_local_once(move || {
                let vadj = scroller.vadjustment();
                let new_upper = vadj.upper();
                let delta = new_upper - old_upper;
                vadj.set_value(old_value + delta);
            });
        }
    }

    /// Insert `obj` into the store keeping ascending message-id order. Returns
    /// the insertion index.
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
                // Dedup: our own optimistic echo (same id) or a duplicate.
                if self.inner.index.borrow().contains_key(&id) {
                    if let Some(obj) = self.inner.index.borrow().get(&id).cloned() {
                        obj.update_from_message(&u.message);
                        self.notify_changed(id);
                    }
                    return;
                }
                let obj = MessageObject::from_message(&u.message);
                let mut to_resolve = Vec::new();
                self.apply_sender_name(&obj, &u.message.sender_id, &mut to_resolve);
                self.insert_sorted(&obj);
                self.inner.index.borrow_mut().insert(id, obj);
                self.resolve_names(to_resolve);
                self.scroll_to_bottom();
                // Mark the freshly-arrived incoming message read (chat is open).
                if !u.message.is_outgoing {
                    self.view_message(id);
                }
            }
            Update::MessageSendSucceeded(u) if u.message.chat_id == self.inner.chat_id => {
                // Reconcile the optimistic temp row (old_message_id) → real id.
                let old = u.old_message_id;
                let new_id = u.message.id;
                let existing = self.inner.index.borrow_mut().remove(&old);
                if let Some(obj) = existing {
                    obj.update_from_message(&u.message);
                    self.inner.index.borrow_mut().insert(new_id, obj);
                    self.notify_changed(new_id);
                } else if !self.inner.index.borrow().contains_key(&new_id) {
                    // We never saw the temp row (rare): just insert the final one.
                    let obj = MessageObject::from_message(&u.message);
                    self.insert_sorted(&obj);
                    self.inner.index.borrow_mut().insert(new_id, obj);
                }
            }
            Update::MessageContent(u) if u.chat_id == self.inner.chat_id => {
                if let Some(obj) = self.inner.index.borrow().get(&u.message_id).cloned() {
                    obj.set_content(&u.new_content);
                    self.notify_changed(u.message_id);
                }
            }
            Update::DeleteMessages(u) if u.chat_id == self.inner.chat_id => {
                // from_cache = TDLib cache eviction, NOT a server delete (per tgt):
                // don't drop the rows from the UI.
                if u.from_cache {
                    return;
                }
                for id in u.message_ids {
                    self.remove_message(id);
                }
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

    /// Fill in `sender_name` for an incoming, group-style message. If the sender
    /// is a user we haven't resolved yet, queue its id for `resolve_names`.
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
            if let Some(name) = self.inner.names.borrow().get(&u.user_id) {
                obj.set_sender_name(name.clone());
            } else {
                to_resolve.push(u.user_id);
            }
        }
    }

    /// Resolve display names for the given user ids, then re-apply them to any
    /// rows authored by those users and re-bind those rows.
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
                        this.apply_name_to_rows(uid, &name);
                    }
                },
            );
        }
    }

    /// After a name resolves, stamp it onto every already-inserted row by `uid`.
    fn apply_name_to_rows(&self, uid: i64, name: &str) {
        let store = &self.inner.store;
        let n = store.n_items();
        for pos in 0..n {
            if let Some(obj) = store.item(pos).and_downcast::<MessageObject>() {
                if !obj.is_outgoing() && obj.sender_id() == uid && obj.sender_name().is_empty() {
                    obj.set_sender_name(name.to_string());
                    store.items_changed(pos, 1, 1);
                }
            }
        }
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
        // Click to send.
        let this = self.clone();
        send_button.connect_clicked(clone!(
            #[strong]
            this,
            move |_| this.do_send()
        ));

        // Enter to send; Shift+Enter inserts a newline.
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

        let this = self.clone();
        crate::runtime::spawn(
            async move {
                functions::send_message(chat_id, None, None, None, content, cid).await
            },
            move |res| match res {
                Ok(MessageEnum::Message(msg)) => {
                    // Optimistic append: TDLib returns a temp Message (pending
                    // sending_state, temporary id). We show it now and reconcile
                    // it on MessageSendSucceeded (matched by old_message_id).
                    let id = msg.id;
                    if !this.inner.index.borrow().contains_key(&id) {
                        let obj = MessageObject::from_message(&msg);
                        this.insert_sorted(&obj);
                        this.inner.index.borrow_mut().insert(id, obj);
                        this.scroll_to_bottom();
                    }
                }
                Err(e) => {
                    tracing::warn!(code = e.code, msg = %e.message, "send_message failed");
                }
            },
        );
    }

    /// Load older history when the user scrolls near the top.
    fn wire_scroll_paging(&self) {
        let this = self.clone();
        let vadj = self.inner.scroller.vadjustment();
        vadj.connect_value_changed(move |adj| {
            // Near the top and there is more to load: page older history.
            if adj.value() <= adj.page_size() * 0.5 {
                this.load_older_history();
            }
        });
    }

    /// Scroll the history to the newest message. Deferred so the list has laid
    /// out the freshly-inserted rows first.
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
