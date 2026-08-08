//! UI layer. Widgets are built in pure Rust (no `.ui` templates, no Blueprint) —
//! the app stays legible and fully type-checked in one language.

pub mod chat_list;
pub mod login;

use gtk::gdk;

/// App stylesheet: unread badges, chat-row typography, QR frame.
const APP_CSS: &str = "
.chat-title { font-weight: bold; }
.chat-preview { font-size: 0.9em; }
.unread-badge {
    min-width: 20px;
    padding: 1px 7px;
    border-radius: 9999px;
    font-size: 0.8em;
    background-color: @accent_bg_color;
    color: @accent_fg_color;
}
.qr-frame {
    background-color: #ffffff;
    border-radius: 12px;
    padding: 12px;
}
";

/// Install the app stylesheet. Call once at application startup.
pub fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(APP_CSS);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Show a transient toast on the given overlay. `.use_markup(false)` so chat
/// titles / error messages containing markup-like characters render literally.
pub fn toast(overlay: &adw::ToastOverlay, message: &str) {
    overlay.add_toast(
        adw::Toast::builder()
            .title(message)
            .use_markup(false)
            .timeout(3)
            .build(),
    );
}
