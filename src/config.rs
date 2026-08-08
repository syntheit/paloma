//! Configuration: Telegram API credentials and non-secret window preferences.
//!
//! Secrets (the `api_id` / `api_hash` pair) live in a small TOML file under
//! `~/.config/paloma/credentials.toml` and can be overridden per-run with the
//! `PALOMA_API_ID` / `PALOMA_API_HASH` environment variables. Non-secret UI
//! state (window geometry) is stored in GSettings, keyed by [`crate::APP_ID`].

use std::path::PathBuf;

use gtk::gio;
use gtk::prelude::*;

/// Telegram application credentials required to talk to TDLib.
#[derive(Clone, Debug)]
pub struct Credentials {
    /// The numeric `api_id` issued by <https://my.telegram.org>.
    pub api_id: i32,
    /// The `api_hash` string paired with `api_id`.
    pub api_hash: String,
}

/// Everything that can go wrong while resolving [`Credentials`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// No credentials were found in the environment or on disk.
    #[error("no Telegram API credentials found; set PALOMA_API_ID/PALOMA_API_HASH or create the credentials file")]
    Missing,
    /// Reading the credentials file failed.
    #[error("failed to read credentials file: {0}")]
    Io(#[from] std::io::Error),
    /// The credentials file exists but could not be parsed / validated.
    #[error("invalid credentials: {0}")]
    Parse(String),
}

/// The TOML shape of `credentials.toml`.
#[derive(serde::Deserialize)]
struct RawCreds {
    api_id: i32,
    api_hash: String,
}

/// Path to the credentials TOML file: `~/.config/paloma/credentials.toml`.
///
/// Falls back to a relative `paloma/credentials.toml` if the platform config
/// directory cannot be determined (should not happen on Linux).
pub fn credentials_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("paloma/credentials.toml")
}

/// Resolve the Telegram API credentials.
///
/// Resolution order:
/// 1. If **both** `PALOMA_API_ID` and `PALOMA_API_HASH` are set (and the id
///    parses as an `i32`), use them.
/// 2. Otherwise read [`credentials_path`]. A missing file yields
///    [`ConfigError::Missing`]; a malformed one yields [`ConfigError::Parse`].
pub fn load_credentials() -> Result<Credentials, ConfigError> {
    // 1. Environment overrides take priority.
    if let (Ok(id_str), Ok(hash)) = (
        std::env::var("PALOMA_API_ID"),
        std::env::var("PALOMA_API_HASH"),
    ) {
        if let Ok(api_id) = id_str.trim().parse::<i32>() {
            if api_id != 0 && !hash.is_empty() {
                return Ok(Credentials {
                    api_id,
                    api_hash: hash,
                });
            }
        }
    }

    // 2. Fall back to the on-disk credentials file.
    let path = credentials_path();
    if !path.exists() {
        return Err(ConfigError::Missing);
    }

    let contents = std::fs::read_to_string(&path)?;
    let raw: RawCreds = toml::from_str(&contents).map_err(|e| {
        let mut msg = e.to_string();
        // A quoted api_id (`api_id = "123"`) is the most common mistake and TOML's
        // own error is cryptic, so prepend a targeted hint.
        if contents.contains("api_id = \"") || contents.contains("api_id=\"") {
            msg = format!("api_id must be a bare integer, not quoted. {msg}");
        }
        ConfigError::Parse(msg)
    })?;

    // Validate the parsed values.
    if raw.api_id == 0 {
        return Err(ConfigError::Missing);
    }
    if raw.api_hash.is_empty() {
        return Err(ConfigError::Parse("api_hash is empty".to_string()));
    }

    Ok(Credentials {
        api_id: raw.api_id,
        api_hash: raw.api_hash,
    })
}

/// Root data directory for Paloma: `~/.local/share/paloma`.
///
/// This is the parent of the TDLib database directory.
pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from(".local/share"))
        .join("paloma")
}

/// The TDLib database directory: `<data_dir>/td`.
pub fn td_database_dir() -> PathBuf {
    data_dir().join("td")
}

// --- Non-secret window preferences (GSettings) ---------------------------

/// Open the GSettings backing store for this application.
fn settings() -> gio::Settings {
    gio::Settings::new(crate::APP_ID)
}

/// The last saved window size as `(width, height)`.
pub fn window_size() -> (i32, i32) {
    let s = settings();
    (s.int("window-width"), s.int("window-height"))
}

/// Persist the window size (best-effort; logs a warning on failure).
pub fn set_window_size(w: i32, h: i32) {
    let s = settings();
    if let Err(e) = s.set_int("window-width", w) {
        tracing::warn!(error = %e, "failed to persist window-width");
    }
    if let Err(e) = s.set_int("window-height", h) {
        tracing::warn!(error = %e, "failed to persist window-height");
    }
}

/// Whether the window was maximized when last closed.
pub fn window_maximized() -> bool {
    settings().boolean("window-maximized")
}

/// Persist the window maximized state (best-effort; logs a warning on failure).
pub fn set_window_maximized(maximized: bool) {
    if let Err(e) = settings().set_boolean("window-maximized", maximized) {
        tracing::warn!(error = %e, "failed to persist window-maximized");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_path_ends_correctly() {
        let path = credentials_path();
        assert!(
            path.ends_with("paloma/credentials.toml"),
            "unexpected credentials path: {}",
            path.display()
        );
    }
}
