//! Command line entry points that do not start the resident process.
//!
//! With the GUI gone these replace the two things the settings window used to be
//! needed for: discovering raw process names for the rule list, and previewing the
//! sanitised payload before it reaches the network.

use std::io::{IsTerminal, Write};
use std::time::Duration;

use windows_sys::Win32::System::Console::{AllocConsole, AttachConsole, ATTACH_PARENT_PROCESS};

use crate::app;
use crate::collector::{self, RawState};
use crate::config;
use crate::pipeline::Pipeline;
use crate::secrets::Secrets;

/// Give a GUI-subsystem binary somewhere to print.
///
/// Borrowing the parent console covers `now-doing --probe` typed into a terminal;
/// when there is no parent (a double-clicked shortcut) one is created instead, so
/// the output is visible rather than silently lost.
///
/// Returns true when this process owns the console and should wait before exiting.
pub fn open_console() -> bool {
    let borrowed = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0;
    if borrowed {
        return false;
    }
    unsafe { AllocConsole() != 0 }
}

/// Keep a self-made console on screen long enough to read.
///
/// Skipped when stdout is redirected: a pipe already gives the caller the output
/// and has no one to press Enter.
pub fn hold_console(owned: bool) {
    if !owned || !std::io::stdout().is_terminal() {
        return;
    }
    println!();
    print!("Press Enter to close this window...");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

/// `--watch`: live view of what the rules produce, without touching the network.
///
/// This is the replacement for the settings window's preview pane: every time the
/// published payload would change, the raw process name and the resulting JSON are
/// printed, so an unrecognised app can be copied straight into `matchProcess`.
pub fn watch() -> i32 {
    if config::ensure_data_dir().is_err() {
        eprintln!("no writable data directory");
        return 1;
    }
    let cfg = config::load();
    let settings = app::shared(cfg.clone());
    let state = collector::shared();
    collector::spawn_for_preview(state.clone(), settings);

    println!("now-doing {} — live preview", env!("CARGO_PKG_VERSION"));
    println!("config      : {}", config::config_path().display());
    println!("data dir    : {}", config::data_dir().display());
    println!(
        "rules       : {}   unknownApp: {}   (follow the focused window, not the mouse)",
        cfg.privacy.rules.len(),
        cfg.privacy.unknown_app
    );
    println!("nothing is uploaded; press Ctrl+C to stop");
    println!();

    // Let the collector fill the shared state once, otherwise the first block
    // describes an empty sample.
    std::thread::sleep(Duration::from_millis(1200));

    let mut last = String::new();
    loop {
        let raw: RawState = state.lock().map(|g| g.clone()).unwrap_or_default();
        let snapshot = Pipeline::new(&cfg.privacy, cfg.paused).evaluate(&raw);

        // The raw process is part of what decides a redraw, though it is never
        // published: an app without a rule produces the same "hidden" snapshot as
        // the one before it, and reading its name is the whole point of this mode.
        let key = watch_key(&raw, &snapshot);

        if key != last {
            last = key;
            let stamp = crate::util::rfc3339(crate::util::unix_now());
            let head = match &snapshot.focus {
                Some(f) if f.category.is_empty() => {
                    format!("{} · {}", snapshot.status.as_str(), f.app)
                }
                Some(f) => format!("{} · {} ({})", snapshot.status.as_str(), f.app, f.category),
                None => format!("{} · (hidden)", snapshot.status.as_str()),
            };
            let track = match &snapshot.media {
                Some(m) => format!("  ♪ {} — {}", m.title, m.artist),
                None => String::new(),
            };
            println!("[{stamp}] {head}{track}");
            println!(
                "  raw process: {}",
                raw.process.as_deref().unwrap_or("(no foreground window)")
            );
            if let Some(title) = &raw.title {
                println!("  raw title  : {title}");
            }
            // What a reader would actually receive, seq and device block included.
            println!("  --- presence.json ---");
            let payload = crate::publisher::preview_payload(&snapshot, &cfg);
            for line in payload.lines() {
                println!("  {line}");
            }
            println!();
            let _ = std::io::stdout().flush();
        }

        // Only changes are worth showing, so a slow poll is enough and keeps the
        // viewer as cheap as the resident loop.
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// What `--watch` treats as "something worth printing".
///
/// Not just the sanitised snapshot: an app with no rule produces the same hidden
/// snapshot as every other unlisted app, so the raw process name has to take part
/// or the view freezes on the first unlisted window and never updates again.
fn watch_key(raw: &RawState, snapshot: &crate::pipeline::Snapshot) -> String {
    let media = raw
        .media
        .as_ref()
        .map(|m| format!("{}|{}|{}", m.app, m.title, m.artist))
        .unwrap_or_default();
    format!(
        "{}|{}|{}|{}",
        snapshot.fingerprint(),
        raw.process.as_deref().unwrap_or_default(),
        raw.title.as_deref().unwrap_or_default(),
        media
    )
}

/// `--probe`: sample once and print what would be published.
///
/// Prints the raw process name as well, which is exactly the string a rule's
/// `matchProcess` needs — the collector's own output is never published.
pub fn probe() -> i32 {
    if config::ensure_data_dir().is_err() {
        eprintln!("no writable data directory");
        return 1;
    }
    let cfg = config::load();
    let settings = app::shared(cfg.clone());
    let state = collector::shared();
    collector::spawn_for_preview(state.clone(), settings);

    // One collection pass takes about a second; give the media watcher (3 s) a
    // little longer so a currently playing track shows up too.
    // Wait for the media watcher to finish a round instead of guessing: a slow
    // COM call used to make this print "nothing playing" for a playing track.
    let deadline = std::time::Instant::now() + Duration::from_millis(3200);
    while std::time::Instant::now() < deadline && !collector::media::polled_once() {
        std::thread::sleep(Duration::from_millis(100));
    }

    let raw: RawState = state.lock().map(|g| g.clone()).unwrap_or_default();
    println!(
        "raw foreground process : {}",
        raw.process.as_deref().unwrap_or("(none)")
    );
    println!(
        "raw window title      : {}",
        raw.title.as_deref().unwrap_or("(none)")
    );
    println!("idle seconds          : {}", raw.idle_secs);
    match &raw.media {
        Some(m) => {
            println!("raw media player      : {}", m.app);
            println!("raw media track       : {} — {}", m.title, m.artist);
            println!("raw media state       : {:?}", m.state);
        }
        None => {
            println!("raw media player      : (nothing playing)");
            println!("                        run --media to see every session the system knows");
        }
    }

    let snapshot = Pipeline::new(&cfg.privacy, cfg.paused).evaluate(&raw);
    println!();
    println!("--- what would be published ---");
    match serde_json::to_string_pretty(&snapshot) {
        Ok(json) => println!("{json}"),
        Err(err) => {
            eprintln!("cannot serialise the snapshot: {err}");
            return 1;
        }
    }

    println!();
    println!("--- setup ---");
    println!(
        "config file           : {}",
        config::config_path().display()
    );
    println!("data directory        : {}", config::data_dir().display());
    println!("rules                 : {}", cfg.privacy.rules.len());
    for warning in config::warnings(&cfg) {
        println!("  ! {warning}");
    }
    let sealed = Secrets::load().is_configured();
    println!(
        "credentials           : {}",
        if sealed {
            "sealed in secrets.bin"
        } else if cfg.sink.access_key_id.trim().is_empty() {
            "missing (config.json has no accessKeyId)"
        } else {
            "in config.json"
        }
    );
    println!(
        "target                : {}",
        if cfg.sink.public_url.trim().is_empty() {
            "(publicUrl is empty)"
        } else {
            cfg.sink.public_url.trim()
        }
    );
    println!(
        "mode                  : {}",
        if cfg.dry_run {
            "dry run — writes data/presence.json, never uploads"
        } else {
            "publishing"
        }
    );
    if cfg.dry_run {
        println!(
            "dry run file          : {}",
            config::data_dir().join("presence.json").display()
        );
    }
    0
}

/// `--media`: what the system media broker actually reports, session by session.
///
/// The collector can only publish what a player hands to Windows. When a track is
/// audible and nothing appears, this shows whether the player registered at all —
/// a player missing from this list cannot be read through the media API, no matter
/// what the rule says.
pub fn media() -> i32 {
    if config::ensure_data_dir().is_err() {
        eprintln!("no writable data directory");
        return 1;
    }
    let cfg = config::load();

    println!("now-doing {} — media sessions", env!("CARGO_PKG_VERSION"));
    println!();
    for line in collector::media::diagnostics(&cfg.privacy.rules) {
        println!("{line}");
    }
    println!();
    println!("A player that is absent here publishes nothing to Windows, so only the");
    println!("window-title fallback can describe it: give its rule \"category\": \"media\".");
    0
}

/// Show what the uploader's resolver would do for a host. The system resolver is
/// the one that fails on `api.cloudflare.com` here, so both paths are printed.
pub fn resolve(host: &str) -> i32 {
    if host.trim().is_empty() {
        eprintln!("usage: now-doing --resolve api.cloudflare.com");
        return 2;
    }
    println!("now-doing {} — resolver", env!("CARGO_PKG_VERSION"));
    println!();
    match crate::dnsfallback::system_lookup(host) {
        Ok(addrs) => println!("system   : {addrs:?}"),
        Err(e) => println!("system   : failed — {e}"),
    }
    match crate::dnsfallback::query_a(host) {
        Some(ips) => {
            println!("fallback : {ips:?}");
            0
        }
        None => {
            println!(
                "fallback : no A record from any of {:?}",
                crate::dnsfallback::SERVERS
            );
            1
        }
    }
}

/// Exercise the purge credentials once, so a wrong token is found here instead of
/// silently costing every reader the full edge TTL.
pub fn purge() -> i32 {
    if config::ensure_data_dir().is_err() {
        eprintln!("no writable data directory");
        return 1;
    }
    let cfg = config::load();
    if cfg.sink.purge_token.trim().is_empty() || cfg.sink.zone_id.trim().is_empty() {
        eprintln!("purgeToken and zoneId are empty in config.json");
        return 2;
    }
    if cfg.sink.public_url.trim().is_empty() {
        eprintln!("publicUrl is empty in config.json");
        return 2;
    }

    println!("now-doing {} — purge", env!("CARGO_PKG_VERSION"));
    println!();
    println!("zone      : {}", cfg.sink.zone_id.trim());
    println!("file      : {}", cfg.sink.public_url.trim());
    let agent = crate::sink::Agent::new(cfg.proxy.as_deref());
    match agent.purge(&cfg.sink) {
        Ok(()) => {
            println!("purge     : ok");
            0
        }
        Err(err) => {
            println!("purge     : failed — {err}");
            1
        }
    }
}

pub fn help() {
    println!("now-doing {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("Run without arguments to stay resident in the system tray.");
    println!();
    println!("  --watch   live preview: prints every change with the exact presence.json body");
    println!("  --probe   sample once and print the raw process name plus the payload");
    println!("  --media   list the media sessions Windows reports, and why one is missing");
    println!("  --resolve resolve a host exactly the way the uploader does");
    println!("  --purge   test the Cloudflare purge credentials once");
    println!("  --seal    move credentials from config.json into a DPAPI-sealed file");
    println!("  --help    show this message");
    println!();
    println!("config.json lives next to the executable (in data/ when that is writable).");
    println!("Add \"dryRun\": true to a resident run to keep a local presence.json updated");
    println!("instead of uploading, and watch that file change while you work.");
}

/// `--seal`: move credentials from `config.json` into a DPAPI-sealed file.
pub fn seal() -> i32 {
    if config::ensure_data_dir().is_err() {
        eprintln!("no writable data directory");
        return 1;
    }

    let mut cfg = config::load();
    if cfg.sink.access_key_id.trim().is_empty() {
        eprintln!("config.json has no accessKeyId to seal");
        return 1;
    }

    let secrets = Secrets {
        access_key_id: cfg.sink.access_key_id.clone(),
        secret_access_key: cfg.sink.secret_access_key.clone(),
        purge_token: cfg.sink.purge_token.clone(),
        zone_id: cfg.sink.zone_id.clone(),
    };

    if let Err(err) = secrets.save() {
        eprintln!("cannot write secrets.bin: {err}");
        return 1;
    }

    cfg.sink.access_key_id.clear();
    cfg.sink.secret_access_key.clear();
    cfg.sink.purge_token.clear();
    cfg.sink.zone_id.clear();

    if let Err(err) = config::save(&cfg) {
        eprintln!("cannot rewrite config.json: {err}");
        return 1;
    }

    println!("credentials sealed into secrets.bin; config.json no longer contains them");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PrivacyConfig;

    #[test]
    fn watch_redraws_when_only_the_raw_process_changes() {
        // Two unlisted apps both come out hidden, so the snapshot cannot tell them
        // apart; without the raw name the live view would freeze on the first one.
        let snapshot = Pipeline::new(&PrivacyConfig::default(), false).evaluate(&RawState {
            process: Some("Code.exe".into()),
            ..RawState::default()
        });
        let a = watch_key(
            &RawState {
                process: Some("Code.exe".into()),
                ..RawState::default()
            },
            &snapshot,
        );
        let b = watch_key(
            &RawState {
                process: Some("notepad.exe".into()),
                ..RawState::default()
            },
            &snapshot,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn watch_holds_still_when_nothing_changes() {
        let raw = RawState {
            process: Some("Code.exe".into()),
            ..RawState::default()
        };
        let snapshot = Pipeline::new(&PrivacyConfig::default(), false).evaluate(&raw);
        assert_eq!(watch_key(&raw, &snapshot), watch_key(&raw, &snapshot));
    }
}
