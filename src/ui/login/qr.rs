//! QR-code sign-in page.
//!
//! Telegram lets you sign in by scanning a `tg://` QR code with an already
//! authorized device. The QR link is delivered asynchronously via the auth
//! `Phase` stream (owned in `app.rs`); this page renders it into a `gtk::Picture`
//! when it arrives.

use adw::prelude::*;
use tdlib_rs::functions;

/// Build the QR sign-in page.
///
/// Asks TDLib to begin QR authentication and shows a placeholder frame until the
/// `tg://` link arrives via the auth `Phase` stream. `app.rs` locates the
/// picture (widget name `"qr-picture"`) and calls [`render_qr`].
pub fn page(client: &crate::tdlib::TdClient) -> adw::NavigationPage {
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

    let frame = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .halign(gtk::Align::Center)
        .css_classes(["qr-frame"])
        .build();

    let picture = gtk::Picture::builder()
        .width_request(240)
        .height_request(240)
        .build();
    // `app.rs` finds this picture by widget name to render the QR when the
    // `tg://` link arrives on the auth Phase stream.
    picture.set_widget_name("qr-picture");
    frame.append(&picture);
    content.append(&frame);

    let placeholder = gtk::Label::builder()
        .label("Generating QR code…")
        .build();
    content.append(&placeholder);

    let instructions = gtk::Label::builder()
        .label(
            "Open Telegram on your phone → Settings → Devices → Link Desktop \
             Device, then scan this code.",
        )
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    content.append(&instructions);

    let status = adw::StatusPage::builder()
        .title("QR sign-in")
        .child(&content)
        .build();

    let toolbar = adw::ToolbarView::new();
    // The NavigationView provides the back button automatically.
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&status));

    // Send-safety: capture only the `i32` client_id inside the async block,
    // never `&TdClient` (it is `Rc`-based / `!Send`). Call the raw
    // `tdlib_rs::functions::*` directly. See phone.rs for the full rationale.
    let cid = client.client_id();
    crate::runtime::spawn(
        async move { functions::request_qr_code_authentication(Vec::new(), cid).await },
        |_res: Result<(), tdlib_rs::types::Error>| {},
    );

    adw::NavigationPage::builder()
        .tag("login-qr")
        .title("QR sign-in")
        .child(&toolbar)
        .build()
}

/// Render `link` (a `tg://` URL) as a QR code SVG into `picture`.
///
/// The SVG is written to a temp file and loaded via `gtk::Picture::set_filename`
/// (GdkPixbuf's librsvg loader decodes it). `gdk::Texture` cannot decode SVG, so
/// the file route is the reliable dependency-light path for Wave 1.
pub fn render_qr(picture: &gtk::Picture, link: &str) {
    use qrcode::render::svg;
    use qrcode::QrCode;

    let Ok(code) = QrCode::new(link.as_bytes()) else {
        return;
    };
    let svg_string = code
        .render::<svg::Color>()
        .min_dimensions(240, 240)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build();
    let mut path = std::env::temp_dir();
    path.push(format!("paloma-qr-{}.svg", std::process::id()));
    if std::fs::write(&path, svg_string.as_bytes()).is_ok() {
        picture.set_filename(Some(&path));
    }
}
