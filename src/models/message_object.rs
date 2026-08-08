//! [`MessageObject`]: the list-item GObject backing a row in the message history.
//!
//! One instance per message. Its properties are read by the history view's
//! `SignalListItemFactory` on bind. Live updates mutate the setters in place and
//! the view re-binds the affected row (same pattern as [`crate::models::ChatObject`]).
//!
//! # Content kind
//!
//! A GObject property can't carry a Rust enum directly, so the parsed
//! [`MessageKind`] is flattened onto scalar properties: a `kind` discriminant
//! (see [`MessageKind::as_i32`]) plus the media fields it needs
//! (`photo-file-id`, dimensions, `media-caption`, document `doc-file-id` /
//! `doc-name` / `doc-size`). The bind fn reads `kind()` and switches on it.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use tdlib_rs::enums::{MessageContent, MessageReplyTo, MessageSender, MessageSendingState};
use tdlib_rs::types;

/// A message's parsed content kind. The heavy media data (bytes) is never held
/// here — only the `file_id`s + metadata needed to render a bubble and, on
/// demand, download the blob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageKind {
    /// Plain (or formatted, flattened) text.
    Text,
    /// A photo: the chosen size's `file_id` and pixel dimensions.
    Photo { file_id: i32, width: i32, height: i32 },
    /// A document/file: `file_id`, original name, size in bytes.
    Document { file_id: i32, name: String, size: i64 },
    /// A voice note of `duration` seconds.
    Voice { duration: i32 },
    /// A sticker (rendered as a placeholder this wave).
    Sticker,
    /// A video (placeholder this wave).
    Video,
    /// Anything else we don't specially render yet.
    Other,
}

impl MessageKind {
    /// Stable discriminant stored in the `kind` GObject property.
    pub fn as_i32(&self) -> i32 {
        match self {
            MessageKind::Text => 0,
            MessageKind::Photo { .. } => 1,
            MessageKind::Document { .. } => 2,
            MessageKind::Voice { .. } => 3,
            MessageKind::Sticker => 4,
            MessageKind::Video => 5,
            MessageKind::Other => 6,
        }
    }
}

/// Discriminant constants mirroring [`MessageKind::as_i32`], for use in the
/// view's bind fn without reconstructing the enum. The full set is kept for
/// completeness even though some kinds are rendered generically for now.
pub mod kind {
    #![allow(dead_code)]
    pub const TEXT: i32 = 0;
    pub const PHOTO: i32 = 1;
    pub const DOCUMENT: i32 = 2;
    pub const VOICE: i32 = 3;
    pub const STICKER: i32 = 4;
    pub const VIDEO: i32 = 5;
    pub const OTHER: i32 = 6;
}

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

        // --- Content kind (flattened MessageKind) ----------------------------
        /// Discriminant from [`super::MessageKind::as_i32`].
        #[property(get, set)]
        pub kind: Cell<i32>,
        /// Photo size `file_id` (kind == Photo), else 0.
        #[property(get, set, name = "photo-file-id")]
        pub photo_file_id: Cell<i32>,
        /// Photo pixel width (kind == Photo).
        #[property(get, set, name = "photo-width")]
        pub photo_width: Cell<i32>,
        /// Photo pixel height (kind == Photo).
        #[property(get, set, name = "photo-height")]
        pub photo_height: Cell<i32>,
        /// Caption under a media message (may be empty).
        #[property(get, set, name = "media-caption")]
        pub media_caption: RefCell<String>,
        /// Document `file_id` (kind == Document), else 0.
        #[property(get, set, name = "doc-file-id")]
        pub doc_file_id: Cell<i32>,
        /// Document original name (kind == Document).
        #[property(get, set, name = "doc-name")]
        pub doc_name: RefCell<String>,
        /// Document size in bytes (kind == Document).
        #[property(get, set, name = "doc-size")]
        pub doc_size: Cell<i64>,

        // --- Reply-to (a message in the same chat) ---------------------------
        /// Replied-to message id, or 0 if this message is not a reply (or the
        /// reply target is a story / another chat we don't render).
        #[property(get, set, name = "reply-to-id")]
        pub reply_to_id: Cell<i64>,
        /// Resolved sender name of the replied-to message (filled by the view).
        #[property(get, set, name = "reply-sender")]
        pub reply_sender: RefCell<String>,
        /// Resolved snippet of the replied-to message (filled by the view).
        #[property(get, set, name = "reply-snippet")]
        pub reply_snippet: RefCell<String>,

        /// Sender's small profile-photo `file_id` for the row avatar (incoming
        /// only), or 0 for initials. Filled by the view once the user resolves.
        #[property(get, set, name = "avatar-file-id")]
        pub avatar_file_id: Cell<i32>,
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
    /// `sender_name` / reply sender+snippet are left empty here — the view fills
    /// them in from its user/message caches, since a bare `Message` carries only
    /// numeric ids.
    pub fn from_message(msg: &types::Message) -> Self {
        let obj = Self::new(msg.id);
        obj.apply(msg);
        obj
    }

    /// Refresh mutable fields from a fresh [`types::Message`] (e.g. after
    /// `MessageSendSucceeded` reconciles a temp message to its real form).
    pub fn update_from_message(&self, msg: &types::Message) {
        self.set_id(msg.id);
        self.apply(msg);
    }

    /// Copy every derived field off `msg` (shared by build + update).
    fn apply(&self, msg: &types::Message) {
        self.set_sender_id(sender_id_of(&msg.sender_id));
        self.set_is_outgoing(msg.is_outgoing);
        self.set_date(i64::from(msg.date));
        self.set_is_pending(matches!(
            msg.sending_state,
            Some(MessageSendingState::Pending(_)) | Some(MessageSendingState::Failed(_))
        ));
        self.set_reply_to_id(reply_to_id_of(&msg.reply_to));
        self.apply_content(&msg.content);
    }

    /// Replace just the content-derived fields (used for `updateMessageContent`).
    pub fn set_content(&self, content: &MessageContent) {
        self.apply_content(content);
    }

    /// Parse a [`MessageContent`] onto the flattened kind + media properties and
    /// the display text.
    fn apply_content(&self, content: &MessageContent) {
        let kind = parse_kind(content);
        self.set_content_text(content_text(content));
        self.set_kind(kind.as_i32());
        // Reset media fields, then fill for the matched kind.
        self.set_photo_file_id(0);
        self.set_photo_width(0);
        self.set_photo_height(0);
        self.set_doc_file_id(0);
        self.set_doc_name(String::new());
        self.set_doc_size(0);
        self.set_media_caption(String::new());
        match &kind {
            MessageKind::Photo { file_id, width, height } => {
                self.set_photo_file_id(*file_id);
                self.set_photo_width(*width);
                self.set_photo_height(*height);
                self.set_media_caption(caption_of(content));
            }
            MessageKind::Document { file_id, name, size } => {
                self.set_doc_file_id(*file_id);
                self.set_doc_name(name.clone());
                self.set_doc_size(*size);
                self.set_media_caption(caption_of(content));
            }
            _ => {}
        }
    }
}

/// Flatten a [`MessageSender`] to its numeric id.
fn sender_id_of(sender: &MessageSender) -> i64 {
    match sender {
        MessageSender::User(u) => u.user_id,
        MessageSender::Chat(c) => c.chat_id,
    }
}

/// Extract the replied-to message id if this message replies to another message
/// in the same chat; 0 for story replies / no reply.
fn reply_to_id_of(reply_to: &Option<MessageReplyTo>) -> i64 {
    match reply_to {
        Some(MessageReplyTo::Message(m)) => m.message_id,
        _ => 0,
    }
}

/// Pick the best photo size to display: the largest whose longest edge is
/// ≤ 1280 px, falling back to the overall largest by pixel area. TDLib orders
/// `sizes` small→large but we don't rely on that.
fn best_photo_size(photo: &types::Photo) -> Option<&types::PhotoSize> {
    let capped = photo
        .sizes
        .iter()
        .filter(|s| s.width.max(s.height) <= 1280)
        .max_by_key(|s| (s.width as i64) * (s.height as i64));
    capped.or_else(|| {
        photo
            .sizes
            .iter()
            .max_by_key(|s| (s.width as i64) * (s.height as i64))
    })
}

/// Parse a [`MessageContent`] into a [`MessageKind`].
pub fn parse_kind(content: &MessageContent) -> MessageKind {
    match content {
        MessageContent::MessageText(_) => MessageKind::Text,
        MessageContent::MessagePhoto(p) => match best_photo_size(&p.photo) {
            Some(size) => MessageKind::Photo {
                file_id: size.photo.id,
                width: size.width,
                height: size.height,
            },
            None => MessageKind::Other,
        },
        MessageContent::MessageDocument(d) => MessageKind::Document {
            file_id: d.document.document.id,
            name: d.document.file_name.clone(),
            size: d.document.document.size,
        },
        MessageContent::MessageVoiceNote(v) => MessageKind::Voice {
            duration: v.voice_note.duration,
        },
        MessageContent::MessageSticker(_) => MessageKind::Sticker,
        MessageContent::MessageVideo(_) | MessageContent::MessageVideoNote(_) => MessageKind::Video,
        _ => MessageKind::Other,
    }
}

/// The caption text of a photo/document message (empty if none).
fn caption_of(content: &MessageContent) -> String {
    match content {
        MessageContent::MessagePhoto(p) => p.caption.text.clone(),
        MessageContent::MessageDocument(d) => d.caption.text.clone(),
        _ => String::new(),
    }
}

/// Render the display text for a message's content: real text for text messages,
/// a short human placeholder for media (documents show name + size).
pub fn content_text(content: &MessageContent) -> String {
    match content {
        MessageContent::MessageText(t) => t.text.text.clone(),
        MessageContent::MessagePhoto(p) => {
            if p.caption.text.is_empty() {
                "📷 Photo".to_string()
            } else {
                p.caption.text.clone()
            }
        }
        MessageContent::MessageVideo(_) => "🎬 Video".to_string(),
        MessageContent::MessageVideoNote(_) => "🎬 Video message".to_string(),
        MessageContent::MessageVoiceNote(v) => {
            format!("🎤 Voice message ({})", format_duration(v.voice_note.duration))
        }
        MessageContent::MessageAudio(_) => "🎵 Audio".to_string(),
        MessageContent::MessageDocument(d) => {
            let name = if d.document.file_name.is_empty() {
                "Document".to_string()
            } else {
                d.document.file_name.clone()
            };
            format!("📎 {} ({})", name, format_size(d.document.document.size))
        }
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

/// Format a byte count as a short human string (e.g. `2.3 MB`).
pub fn format_size(bytes: i64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Format a seconds duration as `M:SS`.
fn format_duration(secs: i32) -> String {
    let s = secs.max(0);
    format!("{}:{:02}", s / 60, s % 60)
}
