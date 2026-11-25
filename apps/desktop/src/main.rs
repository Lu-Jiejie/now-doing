//! Entry point: single instance → data directory → logging → config → threads → message loop.
//!
//! The message loop is a plain Win32 pump because `tray-icon` creates its own
//! hidden window on this thread and delivers clicks through it. Waiting is done
//! with `MsgWaitForMultipleObjectsEx`, which parks the thread until input arrives
//! or the timeout expires, so an idle process burns no CPU at all.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use log::LevelFilter;
use now_doing::{app, autostart, cli, collector, config, logger, publisher, single_instance, tray};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjectsEx, PeekMessageW, TranslateMessage, MSG,
    MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT,
};

/// How long to wait before re-checking the shared state for a tooltip update.
const WAIT_MS: u32 = 1000;

fn main() {
    // The binary is built as a GUI subsystem app so a resident start never flashes
    // a console. The diagnostic subcommands still need one: typed into a terminal
    // they borrow it, double-clicked they get a window of their own.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(flag) = args.first() {
        match flag.as_str() {
            "--watch" => {
                let owned = cli::open_console();
                let code = cli::watch();
                cli::hold_console(owned);
                std::process::exit(code);
            }
            "--probe" => {
                let owned = cli::open_console();
                let code = cli::probe();
                cli::hold_console(owned);
                std::process::exit(code);
            }
            "--history" => {
                let owned = cli::open_console();
                let code =
                    cli::history(history_limit(&args[1..]), args.iter().any(|a| a == "--raw"));
                cli::hold_console(owned);
                std::process::exit(code);
            }
            "--media" => {
                let owned = cli::open_console();
                let code = cli::media();
                cli::hold_console(owned);
                std::process::exit(code);
            }
            "--resolve" => {
                let owned = cli::open_console();
                let code = cli::resolve(args.get(1).map(String::as_str).unwrap_or(""));
                cli::hold_console(owned);
                std::process::exit(code);
            }
            "--purge" => {
                let owned = cli::open_console();
                let code = cli::purge();
                cli::hold_console(owned);
                std::process::exit(code);
            }
            "--seal" => {
                cli::open_console();
                std::process::exit(cli::seal());
            }
            "--help" | "-h" => {
                cli::open_console();
                cli::help();
                return;
            }
            other => {
                cli::open_console();
                eprintln!("unknown argument {other:?}; try --help");
                std::process::exit(2);
            }
        }
    }

    let Some(_mutex) = single_instance::acquire() else {
        // Another copy is already resident; refuse quietly, never with a dialog.
        return;
    };

    let data_dir = match config::ensure_data_dir() {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("now-doing: no writable data directory: {err}");
            return;
        }
    };

    logger::init(config::log_path(), LevelFilter::Info);
    log::info!(
        "now-doing {} starting (data dir: {})",
        env!("CARGO_PKG_VERSION"),
        data_dir.display()
    );

    let mut cfg = config::load();
    if !config::config_path().exists() {
        if let Err(err) = config::save(&cfg) {
            log::warn!("could not write the initial config: {err}");
        } else {
            log::info!("wrote a starter config to {:?}", config::config_path());
        }
    }

    // The registry is the source of truth for autostart; the config only mirrors it.
    let actual = autostart::is_enabled();
    if actual != cfg.autostart {
        cfg.autostart = actual;
        let _ = config::save(&cfg);
    }

    for warning in config::warnings(&cfg) {
        log::warn!("config: {warning}");
    }

    let state = app::shared(cfg.clone());
    let raw = collector::shared();
    collector::spawn(raw.clone(), state.clone());
    publisher::spawn(state.clone(), raw);

    let mut tray = match tray::Tray::install(&state) {
        Ok(tray) => tray,
        Err(err) => {
            log::error!("cannot install the tray icon: {err}");
            return;
        }
    };

    message_loop(&state, &mut tray);
    log::info!("now-doing stopped");
}

/// How many lines `--history` should show: the first non-flag argument, capped so
/// a stray number cannot dump the whole file.
fn history_limit(rest: &[String]) -> usize {
    rest.iter()
        .filter(|arg| !arg.starts_with('-'))
        .find_map(|arg| arg.parse::<usize>().ok())
        .unwrap_or(20)
        .clamp(1, 2000)
}

fn message_loop(state: &app::Shared, tray: &mut tray::Tray) {
    let mut msg = MSG {
        hwnd: std::ptr::null_mut(),
        message: 0,
        wParam: 0,
        lParam: 0,
        time: 0,
        pt: windows_sys::Win32::Foundation::POINT { x: 0, y: 0 },
    };

    loop {
        if !tray.pump(state) {
            break;
        }
        if app::with(state, |rt| rt.quit) {
            break;
        }

        // Wait for input, then drain whatever queued up. nCount = 0 means "no
        // handles to wait on, just the message queue".
        unsafe {
            MsgWaitForMultipleObjectsEx(
                0,
                std::ptr::null(),
                WAIT_MS,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
        }
        while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}
