//! Paloma — a native GTK4/libadwaita Telegram client.
//!
//! Entry point: sets up logging, initialises libadwaita, installs the app CSS,
//! and hands off to [`app::build_ui`].

mod app;
mod audio;
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

/// Uniform, device-scale-respecting font bump applied at startup. Multiplies the
/// resolved `gtk-xft-dpi` so all `em`-relative sizes in the app CSS scale with it.
/// Bump this single constant to make all app text larger/smaller.
const FONT_SCALE: f64 = 1.08;

/// Nudge every app font up by [`FONT_SCALE`] while honouring the compositor's
/// hi-dpi scale factor. Reads the live `gtk-xft-dpi` (1024ths of a point) and
/// scales it in place; falls back to the GTK default of 96 dpi when unset.
fn apply_font_scale() {
    if let Some(settings) = gtk::Settings::default() {
        let base = settings.gtk_xft_dpi();
        let base = if base > 0 { base as f64 } else { 96.0 * 1024.0 };
        settings.set_gtk_xft_dpi((base * FONT_SCALE).round() as i32);
    }
}

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
        apply_font_scale();
    });
    application.connect_activate(app::build_ui);

    application.run_with_args::<&str>(&[])
}
