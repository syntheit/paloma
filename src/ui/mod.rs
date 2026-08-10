//! UI layer. Widgets are built in pure Rust (no `.ui` templates, no Blueprint) —
//! the app stays legible and fully type-checked in one language.

pub mod chat_list;
pub mod chat_view;
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

/* --- Message history bubbles --- */
.msg-list {
    background: transparent;
}
.msg-list > row {
    padding: 1px 8px;
}
.msg-bubble {
    padding: 6px 10px;
    border-radius: 16px;
    margin: 1px 4px;
}
.msg-bubble .msg-body {
    /* let bubbles size to content up to a comfortable reading width */
}
.msg-out {
    background-color: @accent_bg_color;
    color: @accent_fg_color;
    margin-left: 48px;
}
.msg-in {
    background-color: alpha(@card_fg_color, 0.08);
    margin-right: 48px;
}
.msg-out .msg-time,
.msg-out .msg-sender {
    color: alpha(@accent_fg_color, 0.8);
}
.msg-pending {
    opacity: 0.6;
}
.msg-sender {
    font-size: 0.82em;
    font-weight: bold;
}
.msg-time {
    font-size: 0.72em;
}
.msg-status {
    font-size: 0.72em;
    opacity: 0.7;
}
.msg-out .msg-status {
    color: alpha(@accent_fg_color, 0.8);
}
.msg-compose {
    background-color: @view_bg_color;
}
.msg-entry {
    background: transparent;
    font-size: 1em;
}
.msg-entry-scroll {
    border-radius: 18px;
    background-color: alpha(@card_fg_color, 0.08);
    padding: 2px 4px;
}

/* --- Photo messages --- */
.msg-photo {
    border-radius: 12px;
    margin: 1px 0;
}

/* --- Reply quoted header inside a bubble --- */
.msg-reply {
    border-left: 3px solid @accent_bg_color;
    padding: 1px 6px;
    margin-bottom: 2px;
    border-radius: 4px;
    background-color: alpha(@accent_bg_color, 0.12);
}
.msg-out .msg-reply {
    border-left-color: @accent_fg_color;
    background-color: alpha(@accent_fg_color, 0.15);
}
.msg-reply-name {
    font-size: 0.78em;
    font-weight: bold;
}
.msg-reply-text {
    font-size: 0.78em;
    opacity: 0.85;
}

/* --- Compose reply strip (above the entry) --- */
.reply-bar {
    border-left: 3px solid @accent_bg_color;
    padding-left: 8px;
}
.reply-bar-name {
    font-size: 0.85em;
    font-weight: bold;
    color: @accent_bg_color;
}
.reply-bar-text {
    font-size: 0.85em;
}

/* --- Full-image viewer dialog --- */
.image-viewer {
    background-color: @window_bg_color;
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
