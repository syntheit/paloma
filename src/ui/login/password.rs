//! Two-step-verification (2FA) password page.

use adw::prelude::*;
use gtk::glib::clone;
use tdlib_rs::functions;

/// Build the 2FA password page.
///
/// When the account has two-step verification enabled, TDLib asks for the
/// cloud password after the login code. On success the authorization `Phase`
/// stream (driven in `app.rs`) advances the UI; on failure we surface the
/// TDLib error as a toast.
pub fn page(client: &crate::tdlib::TdClient) -> adw::NavigationPage {
    let toasts = adw::ToastOverlay::new();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .halign(gtk::Align::Center)
        .width_request(300)
        .build();

    let group = adw::PreferencesGroup::new();
    let pw_row = adw::PasswordEntryRow::builder().title("Password").build();
    group.add(&pw_row);
    content.append(&group);

    let button = gtk::Button::builder()
        .label("Sign in")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();
    content.append(&button);

    let status = adw::StatusPage::builder()
        .icon_name("dialog-password-symbolic")
        .title("Two-step verification")
        .description("Enter your Telegram password.")
        .child(&content)
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&status));

    toasts.set_child(Some(&toolbar));

    // Send-safety: capture only the `i32` client_id + owned password inside the
    // async block, never `&TdClient` (it is `Rc`-based / `!Send`). Call the raw
    // `tdlib_rs::functions::*` directly. See phone.rs for the full rationale.
    button.connect_clicked(clone!(
        #[strong]
        pw_row,
        #[strong]
        toasts,
        #[strong]
        client,
        move |button| {
            // Do not trim passwords — they may legitimately contain whitespace.
            let password = pw_row.text().to_string();
            if password.is_empty() {
                super::super::toast(&toasts, "Enter your password");
                return;
            }
            button.set_sensitive(false);
            button.set_label("Signing in…");
            let cid = client.client_id();
            crate::runtime::spawn(
                async move { functions::check_authentication_password(password, cid).await },
                clone!(
                    #[strong]
                    button,
                    #[strong]
                    toasts,
                    move |res: Result<(), tdlib_rs::types::Error>| {
                        button.set_sensitive(true);
                        button.set_label("Sign in");
                        if let Err(e) = res {
                            super::super::toast(&toasts, &e.message);
                        }
                    }
                ),
            );
        }
    ));

    adw::NavigationPage::builder()
        .tag("login-password")
        .title("Password")
        .child(&toasts)
        .build()
}
