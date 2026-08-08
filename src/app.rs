//! The application shell: window, navigation, and the auth → main routing loop.
//!
//! [`build_ui`] builds the [`adw::ApplicationWindow`], restores its geometry, and
//! boots the auth pipeline. As [`auth::Phase`] updates arrive on the GTK main
//! thread, [`route_phase`] drives the [`adw::NavigationView`] so the correct
//! login step — or the connected main view — is on screen.

use adw::prelude::*;
use gtk::glib;

use std::cell::RefCell;
use std::rc::Rc;

use crate::config;
use crate::tdlib::{auth, TdClient};
use crate::ui;
use crate::ui::chat_view::ChatView;

/// Build the main window and kick off the auth pipeline.
///
/// If credentials are missing we show a static instructions page and never
/// create a [`TdClient`]; otherwise we connect, stream [`auth::Phase`] updates,
/// and route each to the UI.
pub fn build_ui(app: &adw::Application) {
    let (width, height) = config::window_size();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Paloma")
        .default_width(width)
        .default_height(height)
        .width_request(300)
        .height_request(400)
        .build();

    if config::window_maximized() {
        window.maximize();
    }

    // Persist geometry on close so the next launch restores it.
    window.connect_close_request(|window| {
        let (w, h) = window.default_size();
        config::set_window_size(w, h);
        config::set_window_maximized(window.is_maximized());
        glib::Propagation::Proceed
    });

    let nav = adw::NavigationView::new();
    window.set_content(Some(&nav));
    window.present();

    // Boot sequence: credentials decide whether we even attempt to connect.
    match config::load_credentials() {
        Err(err) => {
            tracing::warn!("no credentials: {err}");
            nav.replace(&[credentials_page(app)]);
        }
        Ok(creds) => {
            let client = TdClient::new();
            nav.replace(&[loading_page()]);

            let phases = auth::start(&client, creds);
            glib::spawn_future_local(clone_loop(nav.clone(), client.clone(), phases));
        }
    }
}

/// Consume the [`auth::Phase`] stream, routing each update to the UI until the
/// channel closes.
async fn clone_loop(
    nav: adw::NavigationView,
    client: TdClient,
    phases: async_channel::Receiver<auth::Phase>,
) {
    while let Ok(phase) = phases.recv().await {
        route_phase(&nav, &client, phase);
    }
    tracing::debug!("auth phase stream closed");
}

/// Static page shown when no API credentials are configured. Needs no client.
fn credentials_page(app: &adw::Application) -> adw::NavigationPage {
    let path = config::credentials_path();
    let description = format!(
        "Paloma needs a Telegram API ID and hash.\n\n\
         1. Visit my.telegram.org and log in.\n\
         2. Open “API development tools” and create an app.\n\
         3. Copy the api_id and api_hash into:\n\
         {}\n\n\
         as:\n\
         api_id = 123456\n\
         api_hash = \"your-hash-here\"\n\n\
         Then restart Paloma.",
        path.display()
    );

    let quit = gtk::Button::builder()
        .label("Quit")
        .halign(gtk::Align::Center)
        .build();
    quit.add_css_class("pill");
    quit.connect_clicked(glib::clone!(
        #[weak]
        app,
        move |_| app.quit()
    ));

    let status = adw::StatusPage::builder()
        .icon_name("dialog-password-symbolic")
        .title("Add your Telegram API credentials")
        .description(&description)
        .child(&quit)
        .build();

    let header = adw::HeaderBar::new();
    let toolbar = adw::ToolbarView::builder().content(&status).build();
    toolbar.add_top_bar(&header);

    adw::NavigationPage::builder()
        .title("Paloma")
        .tag("credentials")
        .child(&toolbar)
        .build()
}

/// Centered spinner shown while TDLib connects.
fn loading_page() -> adw::NavigationPage {
    let spinner = gtk::Spinner::builder()
        .width_request(32)
        .height_request(32)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    spinner.start();

    let status = adw::StatusPage::builder()
        .title("Connecting…")
        .child(&spinner)
        .build();

    let header = adw::HeaderBar::new();
    let toolbar = adw::ToolbarView::builder().content(&status).build();
    toolbar.add_top_bar(&header);

    adw::NavigationPage::builder()
        .title("Paloma")
        .tag("loading")
        .child(&toolbar)
        .build()
}

/// A plain single-message status page (used for terminal/edge states).
fn simple_status_page(tag: &str, title: &str, body: &str) -> adw::NavigationPage {
    let status = adw::StatusPage::builder()
        .title(title)
        .description(body)
        .build();

    let header = adw::HeaderBar::new();
    let toolbar = adw::ToolbarView::builder().content(&status).build();
    toolbar.add_top_bar(&header);

    adw::NavigationPage::builder()
        .title("Paloma")
        .tag(tag)
        .child(&toolbar)
        .build()
}

/// Is `tag` the tag of the navigation view's currently visible page?
fn visible_tag_is(nav: &adw::NavigationView, tag: &str) -> bool {
    nav.visible_page()
        .and_then(|page| page.tag())
        .is_some_and(|t| t == tag)
}

/// Ensure a page with `tag` is on top of the stack, building it if necessary.
///
/// If it is already visible, do nothing. If it exists deeper in the stack, pop
/// back to it. Otherwise build and push it.
fn ensure_page(
    nav: &adw::NavigationView,
    tag: &str,
    build: impl FnOnce() -> adw::NavigationPage,
) {
    if visible_tag_is(nav, tag) {
        return;
    }
    if nav.find_page(tag).is_some() {
        nav.pop_to_tag(tag);
    } else {
        nav.push(&build());
    }
}

/// Route one [`auth::Phase`] to the navigation view.
fn route_phase(nav: &adw::NavigationView, client: &TdClient, phase: auth::Phase) {
    match phase {
        auth::Phase::Connecting => {
            ensure_page(nav, "loading", loading_page);
        }
        auth::Phase::WaitPhone => {
            // First real prompt: replace the stack so the loading page is gone
            // and back-navigation stays clean.
            if !visible_tag_is(nav, "login-phone") {
                nav.replace(&[ui::login::phone::page(client)]);
            }
        }
        auth::Phase::WaitCode => {
            ensure_page(nav, "login-code", || ui::login::code::page(client));
        }
        auth::Phase::WaitPassword => {
            ensure_page(nav, "login-password", || ui::login::password::page(client));
        }
        auth::Phase::WaitQr(link) => {
            ensure_page(nav, "login-qr", || ui::login::qr::page(client));
            if let Some(page) = nav.find_page("login-qr") {
                if let Some(child) = page.child() {
                    if let Some(picture) = find_named::<gtk::Picture>(&child, "qr-picture") {
                        ui::login::qr::render_qr(&picture, &link);
                    }
                }
            }
        }
        auth::Phase::WaitRegistration => {
            tracing::warn!("registration required — not supported in Wave 1");
            nav.replace(&[simple_status_page(
                "registration",
                "Registration required",
                "Creating a new Telegram account isn't supported yet.",
            )]);
        }
        auth::Phase::Ready => {
            // TDLib re-emits Ready after transient reconnects; only build the main
            // view the first time so we don't wipe the open chat / draft / scroll.
            if nav.find_page("main").is_none() {
                nav.replace(&[main_page(client)]);
            }
        }
        auth::Phase::Closed => {
            nav.replace(&[simple_status_page(
                "closed",
                "Signed out",
                "Your session has ended. Restart Paloma to sign in again.",
            )]);
        }
        auth::Phase::Error(msg) => {
            tracing::error!("{msg}");
            nav.replace(&[simple_status_page("error", "Something went wrong", &msg)]);
        }
    }
}

/// The connected main view: an adaptive sidebar/content split that collapses to
/// a single column on narrow (mobile) widths.
///
/// Activating a chat row builds a [`ChatView`], installs it as the split's
/// *content* page, and calls `set_show_content(true)`. On desktop the content
/// pane is always visible; when collapsed (mobile) `AdwNavigationSplitView`
/// treats sidebar/content as a navigation stack, so this pushes the chat as its
/// own page with an automatic back button that returns to the list.
fn main_page(client: &TdClient) -> adw::NavigationPage {
    let split = adw::NavigationSplitView::new();
    split.set_min_sidebar_width(280.0);
    split.set_max_sidebar_width(360.0);

    // The empty-state placeholder shown before any chat is selected.
    let empty_page = empty_content_page();
    split.set_content(Some(&empty_page));

    // The currently-open ChatView, so we can `close_chat` the previous one when
    // switching chats.
    let current: Rc<RefCell<Option<ChatView>>> = Rc::new(RefCell::new(None));

    // Row activation → open the chat in the content pane.
    let on_activate = {
        let split = split.clone();
        let client = client.clone();
        let current = current.clone();
        move |chat_id: i64, title: String| {
            // If this chat is already open, just reveal the content pane.
            if let Some(view) = current.borrow().as_ref() {
                if view.chat_id() == chat_id {
                    split.set_show_content(true);
                    return;
                }
            }
            // Close the previously-open chat (stops its update streaming).
            if let Some(prev) = current.borrow_mut().take() {
                prev.close();
            }

            let view = ChatView::new(client.clone(), chat_id);
            let header = adw::HeaderBar::new();

            // Chat avatar at the start of the header: initials now, photo once
            // downloaded. Fetched via get_chat since the row only handed us the
            // id + title.
            let avatar = adw::Avatar::new(28, Some(&title), true);
            header.pack_start(&avatar);
            {
                let files = client.files();
                let cid = client.client_id();
                let avatar = avatar.clone();
                let current = current.clone();
                crate::runtime::spawn(
                    async move {
                        use tdlib_rs::enums::Chat;
                        match tdlib_rs::functions::get_chat(chat_id, cid).await {
                            Ok(Chat::Chat(c)) => c.photo.as_ref().map(|p| p.small.id).unwrap_or(0),
                            Err(_) => 0,
                        }
                    },
                    move |file_id| {
                        if file_id == 0 {
                            return;
                        }
                        let apply = {
                            let avatar = avatar.clone();
                            let current = current.clone();
                            move |path: std::path::PathBuf| {
                                // Discard a late photo for a chat the user already switched away from.
                                let still_current =
                                    current.borrow().as_ref().map(|v| v.chat_id()) == Some(chat_id);
                                if !still_current {
                                    return;
                                }
                                if let Ok(texture) = gtk::gdk::Texture::from_filename(&path) {
                                    avatar.set_custom_image(Some(&texture));
                                }
                            }
                        };
                        if let Some(path) = files.cached(file_id) {
                            apply(path);
                        } else {
                            files.download(file_id, 16, apply);
                        }
                    },
                );
            }

            let toolbar = adw::ToolbarView::builder().content(view.widget()).build();
            toolbar.add_top_bar(&header);
            let page = adw::NavigationPage::builder()
                .title(if title.is_empty() { "Chat" } else { &title })
                .tag("content")
                .child(&toolbar)
                .build();
            split.set_content(Some(&page));
            split.set_show_content(true);

            view.open();
            *current.borrow_mut() = Some(view);
        }
    };

    let chat_list = ui::chat_list::ChatList::new(client.clone(), on_activate);

    // Sidebar: the chat list.
    let sidebar_header = adw::HeaderBar::new();
    let sidebar_toolbar = adw::ToolbarView::builder()
        .content(chat_list.widget())
        .build();
    sidebar_toolbar.add_top_bar(&sidebar_header);
    let sidebar_page = adw::NavigationPage::builder()
        .title("Chats")
        .tag("sidebar")
        .child(&sidebar_toolbar)
        .build();
    split.set_sidebar(Some(&sidebar_page));

    let bin = adw::BreakpointBin::builder()
        .child(&split)
        .width_request(300)
        .height_request(400)
        .build();

    let condition = adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        560.0,
        adw::LengthUnit::Sp,
    );
    let breakpoint = adw::Breakpoint::new(condition);
    breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
    bin.add_breakpoint(breakpoint);

    adw::NavigationPage::builder()
        .title("Paloma")
        .tag("main")
        .child(&bin)
        .build()
}

/// The placeholder content page shown before any chat is selected.
fn empty_content_page() -> adw::NavigationPage {
    let content_status = adw::StatusPage::builder()
        .icon_name("chat-symbolic")
        .title("Select a chat")
        .description("Choose a conversation from the list.")
        .build();
    let content_header = adw::HeaderBar::new();
    let content_toolbar = adw::ToolbarView::builder()
        .content(&content_status)
        .build();
    content_toolbar.add_top_bar(&content_header);
    adw::NavigationPage::builder()
        .title("Paloma")
        .tag("content")
        .child(&content_toolbar)
        .build()
}

/// Depth-first search for a descendant widget by `widget_name`, downcast to `T`.
fn find_named<T: IsA<gtk::Widget>>(root: &gtk::Widget, name: &str) -> Option<T> {
    if root.widget_name() == name {
        if let Ok(found) = root.clone().downcast::<T>() {
            return Some(found);
        }
    }
    let mut child = root.first_child();
    while let Some(widget) = child {
        if let Some(found) = find_named::<T>(&widget, name) {
            return Some(found);
        }
        child = widget.next_sibling();
    }
    None
}
