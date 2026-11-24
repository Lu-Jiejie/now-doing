//! now-doing — a silent tray agent that reports what you are currently doing.
//!
//! Architectural rule number one: **the resident process never creates a webview
//! window.** Tauri does not make WebView2 cheaper than Electron; it only makes the
//! *disk footprint* smaller. The savings come from not spawning the renderer at
//! all until the user asks for the settings UI, and destroying it again on close.
//!
//! `windows_subsystem = "windows"` is set in `.cargo/config.toml` via the
//! `windows` rustc flag so a release build never flashes a console window.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod config;
mod logger;
mod single_instance;
mod tray;

use log::LevelFilter;

fn main() {
    // The single-instance mutex must be taken before anything else touches the
    // config file, otherwise two copies could race on the atomic rename.
    let Some(_mutex) = single_instance::acquire() else {
        // A second launch is a no-op, not an error dialog. The brief is explicit:
        // never pop up anything the user did not ask for.
        return;
    };

    // Resolve a writable data directory. A failure here is the one case we
    // cannot paper over: with no place to keep config or logs there is nothing
    // to do but leave quietly. Every candidate has already been probed.
    let data_dir = match config::ensure_data_dir() {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("now-doing: no writable data directory: {err}");
            return;
        }
    };

    logger::init(config::log_path(), LevelFilter::Info);
    log::info!("now-doing starting (pid {})", std::process::id());
    log::info!("data dir: {}", data_dir.display());

    let cfg = config::load();
    log::info!(
        "config loaded: paused={} autostart={} heartbeat={}s",
        cfg.paused,
        cfg.autostart,
        cfg.heartbeat_secs
    );

    // Reconcile the registry with the config so a manual registry edit or a
    // moved executable cannot leave the two out of sync.
    let autostart_actual = autostart::is_enabled();
    if autostart_actual != cfg.autostart {
        log::info!(
            "autostart drift detected: config={} registry={}",
            cfg.autostart,
            autostart_actual
        );
    }

    tauri::Builder::default()
        .manage(tray::AppState::new(cfg.clone()))
        .setup(move |app| {
            tray::install(app.handle(), &cfg)?;
            log::info!("setup complete, no window created");

            // Development escape hatch for measuring the settings window's memory
            // cost without clicking the tray by hand. Off unless the variable is
            // set explicitly, so a normal launch still creates no window at all.
            if std::env::var_os("NOW_DOING_DEV_OPEN_SETTINGS").is_some_and(|v| v == "1") {
                log::warn!("NOW_DOING_DEV_OPEN_SETTINGS=1: opening settings at startup");
                tray::open_settings(app.handle());
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build tauri app")
        .run(|_app, event| {
            // Keep the process alive when every window is gone: the tray is the
            // real lifetime owner, so destroying the settings window must not end
            // the process. An explicit `app.exit(code)` carries a code and still
            // goes through, which is how the tray's Quit item works.
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
}
