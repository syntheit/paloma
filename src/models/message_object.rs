//! [`MessageObject`]: the list-item GObject backing a row in the message history.
//!
//! **Wave-1 stub.** Only carries the minimum needed to compile the models module;
//! Wave 2 will flesh this out (timestamps, sender, media, reply state) alongside
//! the message-history `ListView`.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(glib::Properties, Default)]
    #[properties(wrapper_type = super::MessageObject)]
    pub struct MessageObject {
        #[property(get, construct_only)]
        pub id: Cell<i64>,
        #[property(get, set)]
        pub text: RefCell<String>,
        #[property(get, set)]
        pub outgoing: Cell<bool>,
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
    /// Construct a [`MessageObject`] carrying only its immutable `id`.
    pub fn new(id: i64) -> Self {
        glib::Object::builder().property("id", id).build()
    }
}
