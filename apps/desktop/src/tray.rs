//! System tray: the only UI this program has.
//!
//! Everything the user can do lives in the context menu. There is no window and
//! no webview, which is what keeps the resident footprint at a few megabytes.

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::app::{self, Shared};
use crate::config;

const MENU_CONFIG: &str = "open-config";
const MENU_LOG: &str = "open-log";
const MENU_SYNC: &str = "sync-now";
const MENU_OPEN_URL: &str = "open-url";
const MENU_PAUSE: &str = "toggle-pause";
const MENU_AUTOSTART: &str = "toggle-autostart";
const MENU_QUIT: &str = "quit";

pub struct Tray {
    _icon: TrayIcon,
    pause: CheckMenuItem,
    autostart: CheckMenuItem,
    tooltip: String,
    /// Whether the shell acknowledged the icon. `tray-icon` swallows a failed
    /// `Shell_NotifyIcon`, so without this the app would look healthy while being
    /// completely invisible and unreachable.
    registered: bool,
    recheck: Option<std::time::Instant>,
    /// Failed rechecks so far. One or two may just mean explorer has not started
    /// yet, so the user is only told once the situation looks permanent.
    misses: u32,
}

impl Tray {
    pub fn install(state: &Shared) -> Result<Self, String> {
        let cfg = app::config_of(state);

        let open_config = MenuItem::with_id(MENU_CONFIG, "打开配置文件", true, None);
        let open_log = MenuItem::with_id(MENU_LOG, "打开日志", true, None);
        // The publisher only wakes once a second for a heartbeat every few minutes;
        // being able to say "now" is worth one menu item.
        let sync_now = MenuItem::with_id(MENU_SYNC, "立即同步一次", true, None);
        let open_url = MenuItem::with_id(MENU_OPEN_URL, "打开公开链接", true, None);
        let pause = CheckMenuItem::with_id(MENU_PAUSE, "暂停上报", true, cfg.paused, None);
        let autostart =
            CheckMenuItem::with_id(MENU_AUTOSTART, "开机自启", true, cfg.autostart, None);
        let quit = MenuItem::with_id(MENU_QUIT, "退出", true, None);

        let menu = Menu::new();
        menu.append_items(&[
            &open_config,
            &open_log,
            &sync_now,
            &open_url,
            &PredefinedMenuItem::separator(),
            &pause,
            &autostart,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .map_err(|e| format!("build menu: {e}"))?;

        let mut builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("now-doing")
            .with_menu_on_left_click(false);
        if let Some(icon) = load_icon() {
            builder = builder.with_icon(icon);
        } else {
            log::warn!("tray icon failed to decode; falling back to the default icon");
        }

        let icon = builder.build().map_err(|e| format!("tray: {e}"))?;
        let rect = icon.rect();
        let registered = rect.is_some();
        if let Some(rect) = rect {
            log::info!(
                "tray installed at ({}, {})",
                rect.position.x,
                rect.position.y
            );
        } else {
            // The usual cause is a low-integrity executable: Windows blocks it from
            // messaging the taskbar, which reports as ERROR_ACCESS_DENIED.
            log::error!(
                "the shell refused the tray icon, so this process is running invisibly; \
                 move the executable to a normal folder (not one marked low integrity)"
            );
        }

        Ok(Self {
            _icon: icon,
            pause,
            autostart,
            tooltip: String::new(),
            registered,
            recheck: None,
            misses: 0,
        })
    }

    /// Drain pending events. Returns false when the user asked to quit.
    pub fn pump(&mut self, state: &Shared) -> bool {
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button,
                button_state,
                ..
            } = event
            {
                use tray_icon::MouseButton::Left;
                use tray_icon::MouseButtonState::Up;
                if button == Left && button_state == Up {
                    open_config();
                }
            }
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            match event.id.as_ref() {
                MENU_CONFIG => open_config(),
                MENU_LOG => open_path(&config::log_path()),
                MENU_SYNC => {
                    app::with(state, |rt| rt.force_upload = true);
                    log::info!("manual sync requested");
                }
                MENU_OPEN_URL => open_public_url(state),
                MENU_PAUSE => self.toggle_pause(state),
                MENU_AUTOSTART => self.toggle_autostart(state),
                MENU_QUIT => {
                    log::info!("quit requested");
                    app::with(state, |rt| rt.quit = true);
                    return false;
                }
                other => log::debug!("unhandled menu item {other}"),
            }
        }

        self.sync(state);
        true
    }

    fn toggle_pause(&mut self, state: &Shared) {
        let paused = app::with(state, |rt| {
            rt.config.paused = !rt.config.paused;
            rt.config.paused
        });
        persist(state);
        self.pause.set_checked(paused);
        self.tooltip.clear();
        log::info!("paused = {paused}");
    }

    fn toggle_autostart(&mut self, state: &Shared) {
        let wanted = app::with(state, |rt| {
            rt.config.autostart = !rt.config.autostart;
            rt.config.autostart
        });
        match crate::autostart::set_enabled(wanted) {
            Ok(actual) => {
                app::with(state, |rt| rt.config.autostart = actual);
                persist(state);
                self.autostart.set_checked(actual);
                log::info!("autostart = {actual}");
            }
            Err(err) => {
                // The registry is authoritative: undo the optimistic flip so the
                // menu never claims something that is not true.
                app::with(state, |rt| rt.config.autostart = !wanted);
                log::warn!("autostart toggle failed: {err}");
                self.autostart.set_checked(!wanted);
            }
        }
    }

    /// Refresh the tooltip and the check marks from the shared state.
    fn sync(&mut self, state: &Shared) {
        if !self.registered {
            // The icon can still appear later: the shell re-broadcasts
            // `TaskbarCreated` after an explorer restart, and `tray-icon` re-adds it.
            let now = std::time::Instant::now();
            if self.recheck.map(|at| now >= at).unwrap_or(true) {
                self.recheck = Some(now + std::time::Duration::from_secs(5));
                if self._icon.rect().is_some() {
                    self.registered = true;
                    log::info!("the tray icon is live now");
                } else {
                    self.misses += 1;
                    if self.misses == 3 {
                        warn_invisible();
                    }
                }
            }
        }

        let (tooltip, paused, autostart) = app::with(state, |rt| {
            (rt.tooltip(), rt.config.paused, rt.config.autostart)
        });

        if tooltip != self.tooltip {
            if let Err(err) = self._icon.set_tooltip(Some(&tooltip)) {
                log::debug!("set_tooltip failed: {err}");
            }
            self.tooltip = tooltip;
        }
        if self.pause.is_checked() != paused {
            self.pause.set_checked(paused);
        }
        if self.autostart.is_checked() != autostart {
            self.autostart.set_checked(autostart);
        }
    }
}

fn persist(state: &Shared) {
    let (paused, autostart) = app::with(state, |rt| (rt.config.paused, rt.config.autostart));
    if let Err(err) = config::save_tray_flags(paused, autostart) {
        log::warn!("save config: {err}");
    }
}

fn open_config() {
    open_path(&config::config_path());
}

/// Sends the reader-facing URL to the shell. `ShellExecuteW` treats a URL as one
/// more thing to hand to the default handler, so the same path works.
fn open_public_url(state: &Shared) {
    let url = app::with(state, |rt| rt.config.sink.public_url.clone());
    let url = url.trim();
    if url.is_empty() {
        log::warn!("no publicUrl is configured, so there is nothing to open");
        return;
    }
    if let Err(err) = crate::shell::open(std::path::Path::new(url)) {
        log::warn!("open {url}: {err}");
    }
}

fn open_path(path: &std::path::Path) {
    if let Err(err) = crate::shell::open(path) {
        log::warn!("open {path:?}: {err}");
    }
}

fn load_icon() -> Option<Icon> {
    let bytes: &[u8] = include_bytes!("../icons/icon.png");
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let used = &buf[..info.buffer_size()];

    let rgba = match info.color_type {
        png::ColorType::Rgba => used.to_vec(),
        png::ColorType::Rgb => used
            .chunks_exact(3)
            .flat_map(|px| [px[0], px[1], px[2], 255])
            .collect(),
        other => {
            log::warn!("unsupported icon colour type {other:?}");
            return None;
        }
    };

    Icon::from_rgba(rgba, info.width, info.height).ok()
}

/// A program with no window, no tray icon and no way to quit is worse than one
/// that says why it cannot work, so this is the single place we speak out loud.
fn warn_invisible() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONWARNING, MB_OK};

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let text = wide(
        "now-doing 无法创建托盘图标，现在正在后台隐形运行。\n\n\
         请把 now-doing.exe 放到一个普通文件夹（例如 D:\\Software\\now-doing）后再启动，\n\
         不要放在被系统标记为“低完整性”的目录里（这类目录下的程序不许和任务栏通信）。",
    );
    let caption = wide("now-doing");
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONWARNING,
        );
    }
}
