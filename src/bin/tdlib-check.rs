//! Headless smoke check for Paloma's TDLib backend.
//!
//! Boots a TDLib client, sends `set_tdlib_parameters` with a dummy api_id/hash,
//! pumps updates, and asserts it reaches `AuthorizationState::WaitPhoneNumber`
//! within ~15s — proving the backend links libtdjson and the auth pipeline flows
//! on a headless host (the GUI can't run here). Exits 0 on success, 1 on timeout.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use tdlib_rs::enums::{AuthorizationState, Update};
use tdlib_rs::functions;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    // Dummy credentials: TDLib accepts params and advances to WaitPhoneNumber
    // without validating them (validation happens later, at phone submission).
    let api_id: i32 = std::env::var("PALOMA_API_ID").ok().and_then(|s| s.parse().ok()).unwrap_or(123456);
    let api_hash: String = std::env::var("PALOMA_API_HASH").unwrap_or_else(|_| "0123456789abcdef0123456789abcdef".to_string());

    let client_id = tdlib_rs::create_client();
    eprintln!("[tdlib-check] created client_id = {client_id}");

    // Channel the blocking receive() thread uses to hand updates to main.
    let (tx, rx) = mpsc::channel::<Update>();

    // Blocking receive pump on a dedicated OS thread.
    std::thread::spawn(move || {
        loop {
            match tdlib_rs::receive() {
                Some((update, _cid)) => { if tx.send(update).is_err() { break; } }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    });

    // TDLib needs at least one request before it delivers updates.
    functions::set_log_verbosity_level(1, client_id).await.expect("set_log_verbosity_level failed");
    eprintln!("[tdlib-check] set_log_verbosity_level ok");

    let db_dir = std::env::temp_dir().join(format!("paloma-check-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&db_dir);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut params_sent = false;

    loop {
        if Instant::now() >= deadline {
            eprintln!("[tdlib-check] TIMEOUT: did not reach WaitPhoneNumber in 15s");
            std::process::exit(1);
        }
        // Poll the update channel with a short timeout so we can re-check the deadline.
        let update = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(u) => u,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => { eprintln!("[tdlib-check] receive thread died"); std::process::exit(1); }
        };
        if let Update::AuthorizationState(state) = update {
            eprintln!("[tdlib-check] auth state: {:?}", state.authorization_state);
            match state.authorization_state {
                AuthorizationState::WaitTdlibParameters if !params_sent => {
                    params_sent = true;
                    let res = functions::set_tdlib_parameters(
                        false,
                        db_dir.to_string_lossy().into_owned(),
                        String::new(),
                        String::new(),
                        true,  // use_file_database
                        true,  // use_chat_info_database
                        true,  // use_message_database
                        true,  // use_secret_chats
                        api_id,
                        api_hash.clone(),
                        "en".to_string(),
                        "Paloma".to_string(),
                        String::new(),
                        env!("CARGO_PKG_VERSION").to_string(),
                        client_id,
                    ).await;
                    if let Err(e) = res { eprintln!("[tdlib-check] set_tdlib_parameters error: code={} msg={}", e.code, e.message); }
                    else { eprintln!("[tdlib-check] set_tdlib_parameters ok"); }
                }
                AuthorizationState::WaitPhoneNumber => {
                    println!("[tdlib-check] SUCCESS: reached WaitPhoneNumber");
                    // Close cleanly then exit 0.
                    let _ = functions::close(client_id).await;
                    std::process::exit(0);
                }
                other => {
                    eprintln!("[tdlib-check] (ignoring auth state {other:?})");
                }
            }
        }
    }
}
