//! The TDLib backend layer: a thin, clean wrapper over `tdlib-rs`.
//!
//! * [`client`] owns the client_id, runs the blocking `receive()` pump on a
//!   worker thread, and fans every [`Update`] out to subscribers on the GTK
//!   main thread.
//! * [`auth`] drives the TDLib authorization state machine and exposes the
//!   current [`auth::Phase`] as a stream the login UI observes.

pub mod auth;
pub mod client;

pub use client::TdClient;
