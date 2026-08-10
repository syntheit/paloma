//! [`ChatObject`]: the list-item GObject backing a row in the chat list.
//!
//! One instance per chat. Its properties are bound by the sidebar's
//! `SignalListItemFactory`, so mutating a setter (e.g. [`ChatObject::set_last_message`])
//! automatically refreshes the visible row without rebuilding it.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use tdlib_rs::enums::ChatList;
use tdlib_rs::types;

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(glib::Properties, Default)]
    #[properties(wrapper_type = super::ChatObject)]
    pub struct ChatObject {
        #[property(get, construct_only)]
        pub id: Cell<i64>,
        #[property(get, set)]
        pub title: RefCell<String>,
        #[property(get, set, name = "last-message")]
        pub last_message: RefCell<String>,
        #[property(get, set, name = "last-message-date")]
        pub last_message_date: Cell<i64>,
        #[property(get, set, name = "unread-count")]
        pub unread_count: Cell<i32>,
        #[property(get, set)]
        pub order: Cell<i64>,
        /// Small chat-photo `file_id` for the avatar, or 0 if the chat has no
        /// photo (the row then shows initials).
        #[property(get, set, name = "photo-file-id")]
        pub photo_file_id: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ChatObject {
        const NAME: &'static str = "PalomaChatObject";
        type Type = super::ChatObject;
    }

    #[glib::derived_properties]
    impl ObjectImpl for ChatObject {}
}

glib::wrapper! {
    pub struct ChatObject(ObjectSubclass<imp::ChatObject>);
}

impl ChatObject {
    /// Construct an empty [`ChatObject`] carrying only its immutable `id`.
    pub fn new(id: i64) -> Self {
        glib::Object::builder().property("id", id).build()
    }

    /// Build a fully-populated [`ChatObject`] from a TDLib [`types::Chat`].
    ///
    /// The sort `order` is taken from the chat's position in the **Main** chat
    /// list (falling back to `0` if the chat has no Main-list position).
    pub fn from_chat(chat: &types::Chat) -> Self {
        let obj = Self::new(chat.id);
        obj.set_title(chat.title.clone());
        obj.set_unread_count(chat.unread_count);
        obj.set_order(main_list_order(&chat.positions));
        obj.set_last_message(
            chat.last_message
                .as_ref()
                .map(message_preview)
                .unwrap_or_default(),
        );
        obj.set_last_message_date(
            chat.last_message.as_ref().map(|m| m.date as i64).unwrap_or(0),
        );
        obj.set_photo_file_id(photo_file_id_of(chat));
        obj
    }
}

/// Extract the Main-list sort order from a chat's positions, or `0` if none.
fn main_list_order(positions: &[types::ChatPosition]) -> i64 {
    positions
        .iter()
        .find(|p| matches!(p.list, ChatList::Main))
        .map(|p| p.order)
        .unwrap_or(0)
}

/// The small chat-photo `file_id` for a chat's avatar, or 0 if it has none.
fn photo_file_id_of(chat: &types::Chat) -> i32 {
    chat.photo.as_ref().map(|p| p.small.id).unwrap_or(0)
}

/// Render a short, single-line preview of a message for the chat list.
///
/// Text messages show their body; non-text content is summarised with a bracketed
/// placeholder (e.g. `[Photo]`). Kept public so the update pump can reuse it when
/// a `ChatLastMessage` update arrives.
pub fn message_preview(msg: &types::Message) -> String {
    crate::models::message_object::content_text(&msg.content)
}
