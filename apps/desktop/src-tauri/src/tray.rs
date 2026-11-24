//! System-tray icon and its context menu.
//!
//! The tray is the entire UI of the resident process. Everything here runs on the
//! main thread's event loop; the menu callbacks never block, so a hung network
//! stack cannot freeze the tray.

use crate::{autostart, config};
use std::sync::Mutex;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};

/// Shared runtime state. M0 only tracks the paused flag and the loaded config;
/// later milestones add the collector snapshot here.
pub struct AppState {
    pub config: Mutex<config::Config>,
}

impl AppState {
    pub fn new(cfg: config::Config) -> Self {
        Self {
            config: Mutex::new(cfg),
        }
    }
}

const MENU_SHOW: &str = "show";
const MENU_PAUSE: &str = "pause";
const MENU_AUTOSTART: &str = "autostart";
const MENU_QUIT: &str = "quit";

/// Build the tray icon, its menu, and wire every callback.
pub fn install(app: &AppHandle, cfg: &config::Config) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, MENU_SHOW, "打开设置…", true, None::<&str>)?;
    let pause_item = CheckMenuItem::with_id(
        app,
        MENU_PAUSE,
        "暂停上报（隐私模式）",
        true,
        cfg.paused,
        None::<&str>,
    )?;
    let autostart_item = CheckMenuItem::with_id(
        app,
        MENU_AUTOSTART,
        "开机自启动",
        true,
        cfg.autostart,
        None::<&str>,
    )?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit_item = MenuItem::with_id(app, MENU_QUIT, "退出", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[&show_item, &sep, &pause_item, &autostart_item, &sep, &quit_item],
    )?;

    let icon = load_icon(app);

    TrayIconBuilder::with_id("main-tray")
        .icon(icon)
        .tooltip(tooltip(cfg))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            MENU_SHOW => open_settings(app),
            MENU_PAUSE => toggle_pause(app),
            MENU_AUTOSTART => toggle_autostart(app),
            MENU_QUIT => {
                log::info!("quit requested from tray");
                app.exit(0);
            }
            other => log::debug!("unhandled menu id {other}"),
        })
        .on_tray_icon_event(|tray, event| {
            // Left click on the icon opens settings, matching the Windows
            // convention for tray utilities that have no primary window.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                open_settings(tray.app_handle());
            }
        })
        .build(app)?;

    log::info!("tray installed");
    Ok(())
}

/// Tooltip text doubles as the at-a-glance status readout the brief asked for.
fn tooltip(cfg: &config::Config) -> String {
    if cfg.paused {
        "now-doing · 已暂停".into()
    } else {
        "now-doing · 正在上报".into()
    }
}

fn load_icon(_app: &AppHandle) -> tauri::image::Image<'static> {
    // The PNG is compiled into the binary so the tray icon works even when the
    // process runs straight out of `target/` with no bundle resources around.
    // `default_window_icon()` cannot be used here: its borrow is tied to the
    // AppHandle and does not satisfy the `'static` the tray builder wants.
    const ICON_PNG: &[u8] = include_bytes!("../icons/icon.png");
    match tauri::image::Image::from_bytes(ICON_PNG) {
        Ok(img) => img,
        Err(err) => {
            log::error!("failed to decode embedded tray icon: {err}");
            // A 16x16 transparent square keeps the tray alive even if the asset
            // is broken; a missing icon would otherwise abort startup.
            tauri::image::Image::new_owned(vec![0u8; 4 * 16 * 16], 16, 16)
        }
    }
}

/// Create the settings window on demand. If it already exists, focus it instead
/// of building a second one.
///
/// Also used by `main` behind `NOW_DOING_DEV_OPEN_SETTINGS=1` so the window's
/// memory cost can be measured without clicking the tray.
pub fn open_settings(app: &AppHandle) {
    if let Some(existing) = app.get_webview_window("settings") {
        let _ = existing.show();
        let _ = existing.set_focus();
        return;
    }

    log::info!("creating settings window");

    let built = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("settings.html".into()))
        .title("now-doing 设置")
        .inner_size(760.0, 620.0)
        .min_inner_size(560.0, 480.0)
        .resizable(true)
        .center()
        .build();

    match built {
        Ok(window) => {
            // Closing the window must *destroy* it, not hide it: only `destroy`
            // tears down the WebView2 renderer process group and gives the
            // memory back. Hiding would keep ~70-110 MB resident forever.
            let handle = window.clone();
            window.on_window_event(move |event| {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    if let Err(err) = handle.destroy() {
                        log::warn!("destroy settings window failed: {err}");
                    } else {
                        log::info!("settings window destroyed, memory released");
                    }
                }
            });
        }
        Err(err) => log::error!("failed to create settings window: {err}"),
    }
}

fn toggle_pause(app: &AppHandle) {
    let state = app.state::<AppState>();
    let new_value = {
        let Ok(mut cfg) = state.config.lock() else {
            log::error!("config mutex poisoned");
            return;
        };
        cfg.paused = !cfg.paused;
        cfg.paused
    };
    persist(app, "pause toggled");
    refresh_tray(app, new_value);
    log::info!("paused = {new_value}");
}

fn toggle_autostart(app: &AppHandle) {
    let state = app.state::<AppState>();
    let desired = {
        let Ok(cfg) = state.config.lock() else {
            log::error!("config mutex poisoned");
            return;
        };
        !cfg.autostart
    };

    match autostart::set_enabled(desired) {
        Ok(actual) => {
            if let Ok(mut cfg) = state.config.lock() {
                cfg.autostart = actual;
            }
            persist(app, "autostart toggled");
            log::info!("autostart = {actual}");
        }
        // A registry failure is logged, never surfaced as a dialog: the brief
        // requires silent handling of every background failure.
        Err(err) => log::error!("autostart toggle failed: {err}"),
    }
}

fn persist(app: &AppHandle, why: &str) {
    let state = app.state::<AppState>();
    let snapshot = {
        let Ok(cfg) = state.config.lock() else { return };
        cfg.clone()
    };
    if let Err(err) = config::save(&snapshot) {
        log::error!("config save failed ({why}): {err}");
    }
}

fn refresh_tray(app: &AppHandle, paused: bool) {
    if let Some(tray) = app.tray_by_id("main-tray") {
        let cfg = config::Config {
            paused,
            ..Default::default()
        };
        let _ = tray.set_tooltip(Some(tooltip(&cfg)));
    }
}
