//! GObject list-item models. This is the ONLY place in Paloma that uses GObject
//! subclassing — the two big lists (chats, messages) must recycle their rows, so
//! their items are lightweight GObjects with properties bound by a factory.

pub mod chat_object;
pub mod message_object;

pub use chat_object::ChatObject;
pub use message_object::MessageObject;
