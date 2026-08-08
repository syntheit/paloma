//! The TDLib authorization state machine, surfaced as a clean stream of phases.
//!
//! [`start`] subscribes to the client's update stream, translates each
//! `AuthorizationState` into a user-facing [`Phase`], and — on the very first
//! `WaitTdlibParameters` — sends `set_tdlib_parameters` built from the caller's
//! [`Credentials`]. The login UI observes the returned [`Phase`] receiver and
//! answers prompts with the `submit_*` helpers below.

use gtk::glib;

use tdlib_rs::enums::{AuthorizationState, Update};

use crate::config::Credentials;
use crate::tdlib::TdClient;

/// The user-facing authentication phase, derived from TDLib's AuthorizationState.
#[derive(Clone, Debug)]
pub enum Phase {
    /// Booting: parameters sent, waiting for the first real auth prompt.
    Connecting,
    /// TDLib wants a phone number.
    WaitPhone,
    /// A login code was sent; collect it. Carries no data in Wave 1.
    WaitCode,
    /// Two-factor password required.
    WaitPassword,
    /// QR login: show this tg:// link as a QR code to scan.
    WaitQr(String),
    /// New account: collect first/last name.
    WaitRegistration,
    /// Logged in.
    Ready,
    /// Logging out / session closed.
    Closed,
    /// A TDLib error surfaced during auth (code, message) — UI shows it.
    Error(String),
}

/// Start driving authorization on `client`. Returns a receiver of [`Phase`]
/// updates delivered on the GTK main thread. Internally subscribes to the
/// client's update stream, translates `AuthorizationState` → `Phase`, and on
/// `WaitTdlibParameters` sends `set_tdlib_parameters` built from `creds`.
pub fn start(client: &TdClient, creds: Credentials) -> async_channel::Receiver<Phase> {
    let (phase_tx, phase_rx) = async_channel::unbounded::<Phase>();
    let updates = client.subscribe();

    let client = client.clone();
    glib::spawn_future_local(async move {
        while let Ok(update) = updates.recv().await {
            // We only care about authorization-state transitions.
            let Update::AuthorizationState(u) = update else {
                continue;
            };

            let phase = match u.authorization_state {
                AuthorizationState::WaitTdlibParameters => {
                    // TDLib is asking for its startup parameters; supply them and
                    // report that we're still connecting.
                    send_tdlib_parameters(&client, &creds, phase_tx.clone());
                    Phase::Connecting
                }
                AuthorizationState::WaitPhoneNumber => Phase::WaitPhone,
                AuthorizationState::WaitCode(_) => Phase::WaitCode,
                AuthorizationState::WaitPassword(_) => Phase::WaitPassword,
                AuthorizationState::WaitOtherDeviceConfirmation(x) => Phase::WaitQr(x.link),
                AuthorizationState::WaitRegistration(_) => Phase::WaitRegistration,
                AuthorizationState::Ready => Phase::Ready,
                AuthorizationState::LoggingOut
                | AuthorizationState::Closing
                | AuthorizationState::Closed => Phase::Closed,
                // Wave 1 doesn't implement email or premium-purchase auth steps.
                AuthorizationState::WaitEmailAddress(_)
                | AuthorizationState::WaitEmailCode(_)
                | AuthorizationState::WaitPremiumPurchase(_) => {
                    Phase::Error("unsupported auth step".into())
                }
            };

            // If the UI dropped the receiver there's nothing left to drive.
            if phase_tx.send(phase).await.is_err() {
                break;
            }
        }
    });

    phase_rx
}

/// Build and dispatch `set_tdlib_parameters` from the resolved credentials.
///
/// All owned strings are computed up front and moved into the `Send` future so
/// nothing `!Send` crosses into the tokio runtime.
fn send_tdlib_parameters(
    client: &TdClient,
    creds: &Credentials,
    phase_tx: async_channel::Sender<Phase>,
) {
    let db_dir = crate::config::td_database_dir()
        .to_string_lossy()
        .into_owned();
    let api_id = creds.api_id;
    let api_hash = creds.api_hash.clone();
    let lang = system_language_code();
    let cid = client.client_id();

    let fut = async move {
        tdlib_rs::functions::set_tdlib_parameters(
            false,                              // use_test_dc
            db_dir,                             // database_directory
            String::new(),                      // files_directory
            String::new(),                      // database_encryption_key
            true,                               // use_file_database
            true,                               // use_chat_info_database
            true,                               // use_message_database
            true,                               // use_secret_chats
            api_id,                             // api_id
            api_hash,                           // api_hash
            lang,                               // system_language_code
            "Paloma".into(),                    // device_model
            String::new(),                      // system_version
            env!("CARGO_PKG_VERSION").into(),   // application_version
            cid,                                // client_id
        )
        .await
    };

    let tx = phase_tx.clone();
    client.request(fut, move |res| {
        if let Err(e) = res {
            tracing::error!(code = e.code, msg = %e.message, "set_tdlib_parameters failed");
            let tx = tx.clone();
            // Deliver an error phase to the UI so the spinner doesn't hang forever.
            // `send` is async, so marshal it back through the GLib main loop.
            glib::spawn_future_local(async move {
                let _ = tx.send(Phase::Error(e.message)).await;
            });
        }
    });
}

/// Best-effort system language code (e.g. `en`) derived from `$LANG`.
///
/// Takes the portion of `$LANG` before the first `_` or `.`, defaulting to `en`.
fn system_language_code() -> String {
    std::env::var("LANG")
        .ok()
        .and_then(|lang| {
            lang.split(['_', '.'])
                .next()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "en".to_string())
}

// --- Login-page request helpers ------------------------------------------
//
// Each answers one auth prompt and returns the raw TDLib result so the UI can
// surface errors (e.g. a wrong code) inline.

/// Submit the phone number for phone-based login.
pub async fn submit_phone(client: &TdClient, phone: String) -> Result<(), tdlib_rs::types::Error> {
    tdlib_rs::functions::set_authentication_phone_number(phone, None, client.client_id()).await
}

/// Submit the login code sent to the user's other Telegram sessions / SMS.
pub async fn submit_code(client: &TdClient, code: String) -> Result<(), tdlib_rs::types::Error> {
    tdlib_rs::functions::check_authentication_code(code, client.client_id()).await
}

/// Submit the two-factor password.
pub async fn submit_password(
    client: &TdClient,
    password: String,
) -> Result<(), tdlib_rs::types::Error> {
    tdlib_rs::functions::check_authentication_password(password, client.client_id()).await
}

/// Request QR-code login, yielding a `WaitOtherDeviceConfirmation` phase.
pub async fn request_qr(client: &TdClient) -> Result<(), tdlib_rs::types::Error> {
    tdlib_rs::functions::request_qr_code_authentication(Vec::new(), client.client_id()).await
}

/// Register a brand-new account with the given name.
pub async fn submit_registration(
    client: &TdClient,
    first: String,
    last: String,
) -> Result<(), tdlib_rs::types::Error> {
    tdlib_rs::functions::register_user(first, last, false, client.client_id()).await
}
