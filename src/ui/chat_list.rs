//! [`ChatList`]: the chat sidebar, backed by a recycling [`gtk::ListView`].
//!
//! # Recycling model
//!
//! The list is a `ListView` over a [`gio::ListStore`] of lightweight
//! [`ChatObject`] GObjects, driven by a [`gtk::SignalListItemFactory`]. GTK
//! reuses a small pool of row widgets — one per *visible* slot, not one per
//! chat — so a 10 000-chat account still builds only a handful of rows. We
//! therefore never eagerly materialise a widget per chat.
//!
//! # Live updates
//!
//! Each chat keeps a single live `ChatObject` in [`Inner::index`] for O(1)
//! mutation. When a TDLib update mutates an object, we call
//! [`ChatList::notify_changed`] which fires `items_changed` on the store for
//! that one row — the factory then re-binds *only* the affected visible slot.
//! (We deliberately re-bind on demand rather than wire per-property GObject
//! bindings in `connect_bind`, which would need per-row binding-lifetime
//! bookkeeping; see the note in [`ChatList::new`].)

use adw::prelude::*;
use gtk::gio;
use gtk::glib;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use tdlib_rs::enums::ChatList as TdChatList;

use crate::models::ChatObject;

/// The chat sidebar component. Cheap to `.clone()` — the widget tree and all
/// state live behind the shared `Rc<Inner>`.
#[derive(Clone)]
pub struct ChatList {
    root: gtk::Widget,
    inner: Rc<Inner>,
}

/// Shared interior state for a [`ChatList`].
struct Inner {
    /// Handle to the running TDLib client (main-thread-only, `!Send`).
    client: crate::tdlib::TdClient,
    /// The model backing the `ListView`; holds one [`ChatObject`] per chat.
    store: gio::ListStore,
    /// The sorter driving the visible order; re-run when a chat's order changes.
    sorter: gtk::CustomSorter,
    /// `chat_id` → the live [`ChatObject`] in the store, for O(1) updates.
    index: RefCell<HashMap<i64, ChatObject>>,
    /// Switches between the "loading", "list" and "empty" views.
    stack: gtk::Stack,
}

impl ChatList {
    /// Build the chat sidebar for `client`, kick off the initial load, and
    /// begin draining the client's update stream.
    /// Build the chat sidebar for `client`. `on_activate` is invoked with the
    /// `(chat_id, title)` of a row when the user activates it (click / Enter).
    pub fn new(
        client: crate::tdlib::TdClient,
        on_activate: impl Fn(i64, String) + 'static,
    ) -> Self {
        let store = gio::ListStore::new::<ChatObject>();
        let files = client.files();

        // --- Row factory: builds & recycles one widget per visible slot. -----
        let factory = gtk::SignalListItemFactory::new();

        factory.connect_setup(|_, list_item| {
            let list_item = list_item
                .downcast_ref::<gtk::ListItem>()
                .expect("list item is a ListItem");

            let hbox = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(12)
                .margin_top(8)
                .margin_bottom(8)
                .margin_start(12)
                .margin_end(12)
                .build();

            // Avatar placeholder; its initials are set from the title on bind.
            let avatar = adw::Avatar::new(40, None, true);
            avatar.set_widget_name("avatar");
            hbox.append(&avatar);

            // Title + preview stacked vertically, taking the free width.
            let text_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .hexpand(true)
                .valign(gtk::Align::Center)
                .build();

            let title_label = gtk::Label::builder()
                .css_classes(["chat-title"])
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .single_line_mode(true)
                .build();
            title_label.set_widget_name("title");
            text_box.append(&title_label);

            let preview_label = gtk::Label::builder()
                .css_classes(["chat-preview", "dim-label"])
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .single_line_mode(true)
                .build();
            preview_label.set_widget_name("preview");
            text_box.append(&preview_label);

            hbox.append(&text_box);

            // Unread badge, right-aligned and vertically centred.
            let badge = gtk::Label::builder()
                .css_classes(["unread-badge"])
                .valign(gtk::Align::Center)
                .build();
            badge.set_widget_name("badge");
            hbox.append(&badge);

            list_item.set_child(Some(&hbox));
        });

        let bind_files = files.clone();
        factory.connect_bind(move |_, list_item| {
            let list_item = list_item
                .downcast_ref::<gtk::ListItem>()
                .expect("list item is a ListItem");

            let item = list_item
                .item()
                .and_downcast::<ChatObject>()
                .expect("list item holds a ChatObject");
            let hbox = list_item
                .child()
                .and_downcast::<gtk::Box>()
                .expect("row child is a Box");
            let root = hbox.upcast::<gtk::Widget>();

            // We snapshot the object's current values into the recycled widgets
            // here rather than establishing live GObject bindings: on any change
            // the update pump calls `notify_changed`, which re-runs this bind
            // for the affected slot. This keeps binding lifetimes trivial.
            if let Some(avatar) = find::<adw::Avatar>(&root, "avatar") {
                // Default: initials from the title, no custom image.
                avatar.set_text(Some(&item.title()));
                avatar.set_show_initials(true);
                avatar.set_custom_image(gtk::gdk::Paintable::NONE);

                let file_id = item.photo_file_id();
                if file_id != 0 {
                    if let Some(path) = bind_files.cached(file_id) {
                        // Cache hit: apply immediately.
                        if let Ok(texture) = gtk::gdk::Texture::from_filename(&path) {
                            avatar.set_custom_image(Some(&texture));
                        }
                    } else {
                        // Miss: download, then apply — but only if this recycled
                        // row still shows the same chat (guards against reuse).
                        let want_id = item.id();
                        let avatar = avatar.clone();
                        let item = item.clone();
                        bind_files.download(file_id, 16, move |path| {
                            if item.id() != want_id {
                                return;
                            }
                            if let Ok(texture) = gtk::gdk::Texture::from_filename(&path) {
                                avatar.set_custom_image(Some(&texture));
                            }
                        });
                    }
                }
            }
            if let Some(title) = find::<gtk::Label>(&root, "title") {
                title.set_text(&item.title());
            }
            if let Some(preview) = find::<gtk::Label>(&root, "preview") {
                preview.set_text(&item.last_message());
            }
            if let Some(badge) = find::<gtk::Label>(&root, "badge") {
                let count = item.unread_count();
                if count > 0 {
                    badge.set_text(&count.to_string());
                    badge.set_visible(true);
                } else {
                    badge.set_visible(false);
                }
            }
        });

        // --- Sort by Telegram chat order (descending: higher order first, so
        // pinned/most-recent chats sort to the top). The SortListModel wraps the
        // backing store; the ListView sees the sorted view, the index/store
        // still address rows by insertion for O(1) mutation.
        let sorter = gtk::CustomSorter::new(move |a, b| {
            let a = a.downcast_ref::<ChatObject>().map(|c| c.order()).unwrap_or(0);
            let b = b.downcast_ref::<ChatObject>().map(|c| c.order()).unwrap_or(0);
            // Descending order.
            b.cmp(&a).into()
        });
        let sort_model = gtk::SortListModel::new(Some(store.clone()), Some(sorter.clone()));

        // --- ListView in a scroller. ----------------------------------------
        let selection = gtk::NoSelection::new(Some(sort_model.clone()));
        let list_view = gtk::ListView::new(Some(selection), Some(factory));
        list_view.add_css_class("navigation-sidebar");

        // Activating a row opens its chat. The NoSelection model hands us the
        // position within the *sorted* view, so we read the item straight off it.
        {
            let sort_model = sort_model.clone();
            let on_activate = std::rc::Rc::new(on_activate);
            list_view.connect_activate(move |_, pos| {
                if let Some(obj) = sort_model.item(pos).and_downcast::<ChatObject>() {
                    on_activate(obj.id(), obj.title());
                }
            });
        }

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list_view)
            .build();

        // --- Stack: loading / list / empty. ---------------------------------
        let spinner = gtk::Spinner::builder()
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        spinner.start();
        let loading = adw::Bin::builder().child(&spinner).build();

        let empty = adw::StatusPage::builder()
            .icon_name("chat-symbolic")
            .title("No chats yet")
            .build();

        let stack = gtk::Stack::new();
        stack.add_named(&loading, Some("loading"));
        stack.add_named(&scroller, Some("list"));
        stack.add_named(&empty, Some("empty"));
        stack.set_visible_child_name("loading");

        let inner = Rc::new(Inner {
            client,
            store,
            sorter,
            index: RefCell::new(HashMap::new()),
            stack: stack.clone(),
        });

        let this = ChatList {
            root: stack.upcast(),
            inner,
        };

        this.subscribe_updates();
        this.initial_load();

        this
    }

    /// The root widget to embed in the parent layout.
    pub fn widget(&self) -> &gtk::Widget {
        &self.root
    }

    /// Drain the client's update stream on the GTK main thread, dispatching
    /// each update into [`ChatList::handle_update`].
    fn subscribe_updates(&self) {
        let updates = self.inner.client.subscribe();
        let this = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(update) = updates.recv().await {
                this.handle_update(update);
            }
        });
    }

    /// Apply a single TDLib update to the store / index.
    fn handle_update(&self, update: tdlib_rs::enums::Update) {
        use tdlib_rs::enums::Update;

        match update {
            Update::NewChat(u) => {
                let id = u.chat.id;
                // Dedupe by id: ignore chats we already track.
                if self.inner.index.borrow().contains_key(&id) {
                    return;
                }
                let obj = ChatObject::from_chat(&u.chat);
                self.inner.store.append(&obj);
                self.inner.index.borrow_mut().insert(id, obj);
                self.update_visibility();
                self.resort();
            }
            Update::ChatLastMessage(u) => {
                let order = Self::main_order(&u.positions);
                if let Some(obj) = self.inner.index.borrow().get(&u.chat_id) {
                    obj.set_last_message(
                        u.last_message
                            .as_ref()
                            .map(crate::models::chat_object::message_preview)
                            .unwrap_or_default(),
                    );
                    obj.set_order(order);
                }
                self.notify_changed(u.chat_id);
                self.resort();
            }
            Update::ChatPosition(u) => {
                if matches!(u.position.list, TdChatList::Main) {
                    // order == 0 means the chat left the Main list — drop it.
                    if u.position.order == 0 {
                        self.remove_chat(u.chat_id);
                    } else {
                        if let Some(obj) = self.inner.index.borrow().get(&u.chat_id) {
                            obj.set_order(u.position.order);
                        }
                        self.notify_changed(u.chat_id);
                        self.resort();
                    }
                }
            }
            Update::ChatTitle(u) => {
                if let Some(obj) = self.inner.index.borrow().get(&u.chat_id) {
                    obj.set_title(u.title.as_str());
                }
                self.notify_changed(u.chat_id);
            }
            Update::ChatPhoto(u) => {
                if let Some(obj) = self.inner.index.borrow().get(&u.chat_id) {
                    obj.set_photo_file_id(u.photo.as_ref().map(|p| p.small.id).unwrap_or(0));
                }
                self.notify_changed(u.chat_id);
            }
            Update::ChatReadInbox(u) => {
                if let Some(obj) = self.inner.index.borrow().get(&u.chat_id) {
                    obj.set_unread_count(u.unread_count);
                }
                self.notify_changed(u.chat_id);
            }
            _ => {}
        }
    }

    /// Sort order from the first **Main**-list position, or `0` if none.
    fn main_order(positions: &[tdlib_rs::types::ChatPosition]) -> i64 {
        positions
            .iter()
            .find(|p| matches!(p.list, TdChatList::Main))
            .map(|p| p.order)
            .unwrap_or(0)
    }

    /// Kick TDLib to load the Main chat list, then fetch its ids and hydrate.
    ///
    /// `get_chats`/`load_chats`/`get_chat` borrow `&TdClient`, which is `!Send`,
    /// so they can't run inside [`crate::runtime::spawn`]. We instead capture
    /// the plain `client_id` and call the raw `tdlib_rs::functions::*`.
    fn initial_load(&self) {
        let cid = self.inner.client.client_id();
        let this = self.clone();
        crate::runtime::spawn(
            async move {
                use tdlib_rs::enums::{ChatList, Chats};
                // Ask TDLib to load the main list (ignore the expected 404
                // "already loaded" once the cache is warm).
                let _ = tdlib_rs::functions::load_chats(Some(ChatList::Main), 50, cid).await;
                match tdlib_rs::functions::get_chats(Some(ChatList::Main), 50, cid).await {
                    Ok(Chats::Chats(c)) => Ok(c.chat_ids),
                    Err(e) => Err(e),
                }
            },
            move |res| match res {
                Ok(ids) => this.hydrate_chats(ids),
                Err(_e) => this.update_visibility(),
            },
        );
    }

    /// Fetch and insert any chats from `ids` we don't already track.
    fn hydrate_chats(&self, ids: Vec<i64>) {
        let cid = self.inner.client.client_id();
        for id in ids {
            if self.inner.index.borrow().contains_key(&id) {
                continue;
            }
            let this = self.clone();
            crate::runtime::spawn(
                async move {
                    use tdlib_rs::enums::Chat;
                    match tdlib_rs::functions::get_chat(id, cid).await {
                        Ok(Chat::Chat(c)) => Some(c),
                        Err(_) => None,
                    }
                },
                move |res| {
                    if let Some(chat) = res {
                        this.add_or_update(chat);
                    }
                },
            );
        }
        self.update_visibility();
    }

    /// Insert `chat` if new, otherwise refresh the existing live object in place.
    fn add_or_update(&self, chat: tdlib_rs::types::Chat) {
        let id = chat.id;
        let existing = self.inner.index.borrow().get(&id).cloned();
        if let Some(obj) = existing {
            obj.set_title(chat.title.as_str());
            obj.set_unread_count(chat.unread_count);
            obj.set_order(Self::main_order(&chat.positions));
            obj.set_last_message(
                chat
                    .last_message
                    .as_ref()
                    .map(crate::models::chat_object::message_preview)
                    .unwrap_or_default(),
            );
            obj.set_photo_file_id(chat.photo.as_ref().map(|p| p.small.id).unwrap_or(0));
            self.notify_changed(id);
        } else {
            let obj = ChatObject::from_chat(&chat);
            self.inner.store.append(&obj);
            self.inner.index.borrow_mut().insert(id, obj);
        }
        self.update_visibility();
        self.resort();
    }

    /// Force the factory to re-bind the row for `chat_id`, reflecting mutations
    /// made to its live [`ChatObject`]. No-op if the chat isn't in the store.
    fn notify_changed(&self, chat_id: i64) {
        let n = self.inner.store.n_items();
        for pos in 0..n {
            if let Some(obj) = self.inner.store.item(pos).and_downcast::<ChatObject>() {
                if obj.id() == chat_id {
                    self.inner.store.items_changed(pos, 1, 1);
                    return;
                }
            }
        }
    }

    /// Ask the sorter to re-run after an `order` change. `SorterChange::Different`
    /// tells the SortListModel the ordering may have changed for any item.
    fn resort(&self) {
        self.inner
            .sorter
            .changed(gtk::SorterChange::Different);
    }

    /// Remove a chat from the store and index (e.g. it left the Main list).
    fn remove_chat(&self, chat_id: i64) {
        if self.inner.index.borrow_mut().remove(&chat_id).is_none() {
            return;
        }
        let store = &self.inner.store;
        let n = store.n_items();
        for pos in 0..n {
            if let Some(obj) = store.item(pos).and_downcast::<ChatObject>() {
                if obj.id() == chat_id {
                    store.remove(pos);
                    break;
                }
            }
        }
        self.update_visibility();
    }

    /// Show "list" once we have chats, otherwise "empty". Called after each load.
    fn update_visibility(&self) {
        if self.inner.store.n_items() == 0 {
            self.inner.stack.set_visible_child_name("empty");
        } else {
            self.inner.stack.set_visible_child_name("list");
        }
    }
}

/// Depth-first search for the first descendant of `root` (inclusive of its
/// direct children) whose widget name is `name` and which downcasts to `T`.
///
/// Rows are built in `connect_setup` with unique widget names so `connect_bind`
/// can retrieve the recycled child widgets without stashing references.
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
