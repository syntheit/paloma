//! [`MessageObject`]: the list-item GObject backing a row in the message history.
//!
//! One instance per message. Its properties are read by the history view's
//! `SignalListItemFactory` on bind. Live updates mutate the setters in place and
//! the view re-binds the affected row (same pattern as [`crate::models::ChatObject`]).

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use tdlib_rs::enums::{MessageContent, MessageSender, MessageSendingState};
use tdlib_rs::types;

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(glib::Properties, Default)]
    #[properties(wrapper_type = super::MessageObject)]
    pub struct MessageObject {
        /// TDLib message id. Mutable because an optimistic (pending) message is
        /// created with a temporary id and reconciled to the real id on
        /// `MessageSendSucceeded`.
        #[property(get, set, construct)]
        pub id: Cell<i64>,
        /// Raw sender id: a user id (positive) or a chat id. `0` if unknown.
        #[property(get, set, name = "sender-id")]
        pub sender_id: Cell<i64>,
        /// True if the current user sent this message.
        #[property(get, set, name = "is-outgoing")]
        pub is_outgoing: Cell<bool>,
        /// The rendered body text (real text, or a placeholder for media).
        #[property(get, set, name = "content-text")]
        pub content_text: RefCell<String>,
        /// Unix timestamp (seconds) the message was sent.
        #[property(get, set)]
        pub date: Cell<i64>,
        /// Display name of the sender, shown above incoming group messages.
        #[property(get, set, name = "sender-name")]
        pub sender_name: RefCell<String>,
        /// True while the message is optimistically shown but not yet confirmed
        /// by the server (sending or failed).
        #[property(get, set, name = "is-pending")]
        pub is_pending: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MessageObject {
        const NAME: &'static str = "PalomaMessageObject";
        type Type = super::MessageObject;
    }

    #[glib::derived_properties]
    impl ObjectImpl for MessageObject {}
}

glib::wrapper! {
    pub struct MessageObject(ObjectSubclass<imp::MessageObject>);
}

impl MessageObject {
    /// Construct a [`MessageObject`] carrying only its immutable-ish `id`.
    pub fn new(id: i64) -> Self {
        glib::Object::builder().property("id", id).build()
    }

    /// Build a fully-populated [`MessageObject`] from a TDLib [`types::Message`].
    ///
    /// Text content is extracted verbatim; every other content kind becomes a
    /// short placeholder (Wave 2 renders text only). `sender_name` is left empty
    /// here — the view fills it in from its user cache, since a bare `Message`
    /// carries only the numeric sender id.
    pub fn from_message(msg: &types::Message) -> Self {
        let obj = Self::new(msg.id);
        obj.set_sender_id(sender_id_of(&msg.sender_id));
        obj.set_is_outgoing(msg.is_outgoing);
        obj.set_content_text(content_text(&msg.content));
        obj.set_date(i64::from(msg.date));
        obj.set_is_pending(matches!(
            msg.sending_state,
            Some(MessageSendingState::Pending(_)) | Some(MessageSendingState::Failed(_))
        ));
        obj
    }

    /// Refresh mutable fields from a fresh [`types::Message`] (e.g. after
    /// `MessageSendSucceeded` reconciles a temp message to its real form).
    pub fn update_from_message(&self, msg: &types::Message) {
        self.set_id(msg.id);
        self.set_sender_id(sender_id_of(&msg.sender_id));
        self.set_is_outgoing(msg.is_outgoing);
        self.set_content_text(content_text(&msg.content));
        self.set_date(i64::from(msg.date));
        self.set_is_pending(matches!(
            msg.sending_state,
            Some(MessageSendingState::Pending(_)) | Some(MessageSendingState::Failed(_))
        ));
    }

    /// Replace just the body text (used for `updateMessageContent`).
    pub fn set_content(&self, content: &MessageContent) {
        self.set_content_text(content_text(content));
    }
}

/// Flatten a [`MessageSender`] to its numeric id.
fn sender_id_of(sender: &MessageSender) -> i64 {
    match sender {
        MessageSender::User(u) => u.user_id,
        MessageSender::Chat(c) => c.chat_id,
    }
}

/// Render the display text for a message's content: real text for text messages,
/// a short emoji placeholder for everything else (Wave 2 is text-only).
pub fn content_text(content: &MessageContent) -> String {
    match content {
        MessageContent::MessageText(t) => t.text.text.clone(),
        MessageContent::MessagePhoto(_) => "📷 Photo".to_string(),
        MessageContent::MessageVideo(_) => "🎬 Video".to_string(),
        MessageContent::MessageVideoNote(_) => "🎬 Video message".to_string(),
        MessageContent::MessageVoiceNote(_) => "🎤 Voice message".to_string(),
        MessageContent::MessageAudio(_) => "🎵 Audio".to_string(),
        MessageContent::MessageDocument(_) => "📎 Document".to_string(),
        MessageContent::MessageSticker(_) => "Sticker".to_string(),
        MessageContent::MessageAnimation(_) => "GIF".to_string(),
        MessageContent::MessageAnimatedEmoji(_) => "Emoji".to_string(),
        MessageContent::MessageLocation(_) => "📍 Location".to_string(),
        MessageContent::MessageVenue(_) => "📍 Venue".to_string(),
        MessageContent::MessageContact(_) => "👤 Contact".to_string(),
        MessageContent::MessagePoll(_) => "📊 Poll".to_string(),
        MessageContent::MessageCall(_) => "📞 Call".to_string(),
        _ => "Unsupported message".to_string(),
    }
}
