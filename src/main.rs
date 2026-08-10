//! Paloma — a native GTK4/libadwaita Telegram client.
//!
//! Entry point: sets up logging, initialises libadwaita, installs the app CSS,
//! and hands off to [`app::build_ui`].

mod app;
mod config;
mod format;
mod models;
mod runtime;
mod tdlib;
mod ui;

use gtk::prelude::*;

/// Reverse-DNS application id. Also the GSettings schema id and the D-Bus name;
/// keep in sync with `data/*.gschema.xml` and the `.desktop` file.
pub const APP_ID: &str = "io.matv.Paloma";

fn main() -> gtk::glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "paloma=info,warn".into()),
        )
        .init();

    adw::init().expect("failed to initialise libadwaita");

    let application = adw::Application::builder()
        .application_id(APP_ID)
        .build();

    application.connect_startup(|_| {
        ui::load_css();
    });
    application.connect_activate(app::build_ui);

    application.run_with_args::<&str>(&[])
}
