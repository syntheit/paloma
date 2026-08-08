//! Phone-number entry page — the first step of the interactive login flow.

use adw::prelude::*;
use gtk::glib::clone;
use tdlib_rs::functions;

/// Build the phone-number entry page.
///
/// Collects a phone number and hands it to TDLib. On success the authorization
/// `Phase` stream (driven in `app.rs`) advances the UI to the code page; on
/// failure we surface the TDLib error message as a toast.
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
    let phone_row = adw::EntryRow::builder().title("Phone number").build();
    phone_row.set_input_purpose(gtk::InputPurpose::Phone);
    group.add(&phone_row);
    content.append(&group);

    let button = gtk::Button::builder()
        .label("Next")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();
    content.append(&button);

    let qr_button = gtk::Button::builder()
        .label("Use QR code instead")
        .halign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    content.append(&qr_button);

    let status = adw::StatusPage::builder()
        .icon_name("user-info-symbolic")
        .title("Welcome to Paloma")
        .description("Enter your phone number to sign in.")
        .child(&content)
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&status));

    toasts.set_child(Some(&toolbar));

    // Send-safety: `TdClient` is `Rc`-based and thus `!Send`, but the future
    // handed to `runtime::spawn` must be `Send + 'static`. So we never capture
    // `&TdClient` (nor the `auth::submit_*` helpers that borrow it) inside the
    // async block — we capture only the plain `i32` client_id plus owned data,
    // and call the raw `tdlib_rs::functions::*` directly. This same pattern is
    // used by every login page.
    button.connect_clicked(clone!(
        #[strong]
        phone_row,
        #[strong]
        toasts,
        #[strong]
        client,
        move |button| {
            let phone = phone_row.text();
            let phone = phone.trim();
            if phone.is_empty() {
                super::super::toast(&toasts, "Enter your phone number");
                return;
            }
            button.set_sensitive(false);
            button.set_label("Sending…");
            let cid = client.client_id();
            let phone = phone.to_string();
            crate::runtime::spawn(
                async move {
                    functions::set_authentication_phone_number(phone, None, cid).await
                },
                clone!(
                    #[strong]
                    button,
                    #[strong]
                    toasts,
                    move |res: Result<(), tdlib_rs::types::Error>| {
                        button.set_sensitive(true);
                        button.set_label("Next");
                        if let Err(e) = res {
                            super::super::toast(&toasts, &e.message);
                        }
                        // On success, the auth Phase stream (driven in app.rs)
                        // advances the UI.
                    }
                ),
            );
        }
    ));

    // "Use QR code instead" — push the QR page onto the enclosing nav view.
    qr_button.connect_clicked(clone!(
        #[strong]
        client,
        move |btn| {
            if let Some(nav) = find_nav_view(btn) {
                nav.push(&super::qr::page(&client));
            }
        }
    ));

    adw::NavigationPage::builder()
        .tag("login-phone")
        .title("Sign in")
        .child(&toasts)
        .build()
}

/// Walk up the widget tree to the enclosing AdwNavigationView.
fn find_nav_view(w: &impl IsA<gtk::Widget>) -> Option<adw::NavigationView> {
    let mut cur = w.clone().upcast::<gtk::Widget>().parent();
    while let Some(p) = cur {
        if let Some(nav) = p.downcast_ref::<adw::NavigationView>() {
            return Some(nav.clone());
        }
        cur = p.parent();
    }
    None
}
