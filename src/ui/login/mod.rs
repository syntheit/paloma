//! The login flow pages: phone entry, code entry, 2FA password, and QR sign-in.
//! Each is a plain `adw::NavigationPage` wired to `crate::tdlib::auth`.

pub mod code;
pub mod password;
pub mod phone;
pub mod qr;
