//! Login-code entry page — the second step of the interactive login flow.

use adw::prelude::*;
use gtk::glib::clone;
use tdlib_rs::functions;

/// Build the login-code entry page.
///
/// TDLib has sent a login code to the user's other Telegram sessions; we collect
/// it and hand it back. On success the authorization `Phase` stream (driven in
/// `app.rs`) advances the UI; on failure we surface the TDLib error as a toast.
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
    let code_row = adw::EntryRow::builder().title("Login code").build();
    code_row.set_input_purpose(gtk::InputPurpose::Digits);
    group.add(&code_row);
    content.append(&group);

    let button = gtk::Button::builder()
        .label("Verify")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();
    content.append(&button);

    let status = adw::StatusPage::builder()
        .icon_name("mail-unread-symbolic")
        .title("Enter code")
        .description("We sent a login code to your Telegram app.")
        .child(&content)
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&status));

    toasts.set_child(Some(&toolbar));

    // Send-safety: capture only the `i32` client_id + owned code inside the
    // async block, never `&TdClient` (it is `Rc`-based / `!Send`). Call the raw
    // `tdlib_rs::functions::*` directly. See phone.rs for the full rationale.
    button.connect_clicked(clone!(
        #[strong]
        code_row,
        #[strong]
        toasts,
        #[strong]
        client,
        move |button| {
            let code = code_row.text();
            let code = code.trim();
            if code.is_empty() {
                super::super::toast(&toasts, "Enter the login code");
                return;
            }
            button.set_sensitive(false);
            button.set_label("Verifying…");
            let cid = client.client_id();
            let code = code.to_string();
            crate::runtime::spawn(
                async move { functions::check_authentication_code(code, cid).await },
                clone!(
                    #[strong]
                    button,
                    #[strong]
                    toasts,
                    move |res: Result<(), tdlib_rs::types::Error>| {
                        button.set_sensitive(true);
                        button.set_label("Verify");
                        if let Err(e) = res {
                            super::super::toast(&toasts, &e.message);
                        }
                    }
                ),
            );
        }
    ));

    adw::NavigationPage::builder()
        .tag("login-code")
        .title("Code")
        .child(&toasts)
        .build()
}
