//! now-doing: a tray-only presence reporter.
//!
//! The resident footprint is the point of this crate's shape: no webview, no
//! window, one small Win32 message loop.

pub mod app;
pub mod autostart;
pub mod cli;
pub mod collector;
pub mod config;
pub mod device;
pub mod dnsfallback;
pub mod logger;
pub mod pipeline;
pub mod protocol;
pub mod publisher;
pub mod secrets;
pub mod shell;
pub mod sigv4;
pub mod single_instance;
pub mod sink;
pub mod tray;
pub mod util;
