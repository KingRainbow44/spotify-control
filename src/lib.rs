//! Global media hotkeys for Spotify, driven through the Spotify Web API so the
//! bindings control whatever device is currently active — not just this machine.

pub mod auth;
pub mod config;
pub mod controller;
pub mod hotkeys;
pub mod launcher;
pub mod logging;
pub mod osd;
pub mod service;
pub mod spotify;
#[cfg(all(unix, not(target_os = "macos")))]
pub mod wayland;
