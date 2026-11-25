//! "What is playing right now", read through the system media transport
//! controls (GSMTC).
//!
//! GSMTC is the Windows 10+ media session broker: any player that integrates
//! with the system media overlay (Spotify, Groove, foobar2000, a browser tab, and
//! NetEase Cloud Music when it registers) is visible here for free. No polling of
//! the player itself, no memory reading, no window-title guessing.
//!
//! The read happens on its own thread so a player stuck inside a COM call can
//! never stall the collector: the collector only ever takes a mutex and copies
//! the last value.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession, GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PbStatus,
};
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, IsWindowVisible};

use crate::app;
use crate::config::Rule;
use crate::pipeline::{public_id, rule_matches};
use crate::protocol::MediaState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawMedia {
    pub state: MediaState,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Sanitised player name, e.g. `spotify`.
    pub app: String,
    pub position_s: u64,
    pub duration_s: u64,
}

/// Polls GSMTC on a worker thread and hands the latest result to the collector.
pub struct MediaWatcher {
    latest: Arc<Mutex<Option<RawMedia>>>,
}

impl MediaWatcher {
    pub fn spawn(interval: Duration, settings: app::Shared) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&latest);
        std::thread::Builder::new()
            .name("media-watch".into())
            .stack_size(256 * 1024)
            .spawn(move || watch_loop(sink, interval, settings))
            .ok();
        Self { latest }
    }

    pub fn latest(&self) -> Option<RawMedia> {
        self.latest.lock().ok().and_then(|g| g.clone())
    }
}

fn init_com() {
    use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    unsafe {
        CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32);
    }
}

type Manager = GlobalSystemMediaTransportControlsSessionManager;

fn request_manager() -> Result<Manager, String> {
    GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
        .map_err(|e| e.message())?
        .join()
        .map_err(|e| e.message())
}

/// Set once the watcher has finished its first round, so `--probe` can wait
/// for a real answer instead of reading an empty slot.
static POLLED_ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn polled_once() -> bool {
    POLLED_ONCE.load(std::sync::atomic::Ordering::Relaxed)
}

fn watch_loop(sink: Arc<Mutex<Option<RawMedia>>>, interval: Duration, settings: app::Shared) {
    init_com();

    // The manager is expensive to create and cheap to keep: one per thread. A
    // failure here used to end the thread for good, which looked exactly like
    // "nothing is ever playing"; now it is retried and logged instead.
    let mut manager: Option<Manager> = None;
    let mut guard = ZombieGuard::default();

    loop {
        if manager.is_none() {
            match request_manager() {
                Ok(m) => manager = Some(m),
                Err(err) => {
                    log::warn!("media: cannot reach the system media manager: {err}");
                    std::thread::sleep(Duration::from_secs(30));
                    continue;
                }
            }
        }

        let rules = app::with(&settings, |rt| rt.config.privacy.rules.clone());
        let players: Vec<&Rule> = rules.iter().filter(|r| r.allows_media()).collect();

        // Nothing is published while paused, so asking the broker and walking every
        // window for a caption is pure waste. Drop the cached value too, otherwise a
        // stale track would reappear the moment the user unpauses.
        if app::with(&settings, |rt| rt.config.paused) {
            if let Ok(mut slot) = sink.lock() {
                *slot = None;
            }
            guard = ZombieGuard::default();
            std::thread::sleep(interval);
            continue;
        }

        // A session belonging to a browser must not shadow the actual player: pick
        // only among players the rules allow, and fall back to a caption when the
        // broker has nothing of ours.
        let value = manager
            .as_ref()
            .and_then(|m| pick_session(m, &players))
            .and_then(|s| read_session(&s))
            .or_else(|| caption_probe(&rules))
            .filter(|m| {
                let key = format!("{}|{}", m.app, m.title);
                !guard.is_stale(
                    m.state == MediaState::Playing,
                    &key,
                    m.position_s,
                    m.duration_s,
                )
            });
        if let Ok(mut slot) = sink.lock() {
            *slot = value;
            POLLED_ONCE.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        std::thread::sleep(interval);
    }
}

/// Read the caption of a window belonging to a rule-declared media player.
///
/// The NetEase client, among others, never registers with the system media
/// broker, so its window caption (`track - artist`) is the only thing it
/// publishes. That caption is worth reading even while another window has the
/// focus, otherwise a track would vanish from the page the moment the reader
/// switches to their editor.
fn caption_probe(rules: &[Rule]) -> Option<RawMedia> {
    let players: Vec<&Rule> = rules.iter().filter(|r| r.allows_media()).collect();
    if players.is_empty() {
        return None;
    }

    struct Ctx<'a> {
        players: &'a [&'a Rule],
        found: Option<RawMedia>,
    }

    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam as *mut Ctx);
        if ctx.found.is_some() || IsWindowVisible(hwnd) == 0 {
            return 1;
        }
        let Some(caption) = super::focus::window_title(hwnd) else {
            return 1;
        };
        // A caption without a dash cannot be `track - artist`, and asking for the
        // process name costs an OpenProcess, so the cheap test comes first.
        if !caption.contains(" - ") {
            return 1;
        }
        let Some(process) = super::focus::process_name(hwnd) else {
            return 1;
        };
        let Some(rule) = ctx
            .players
            .iter()
            .find(|r| rule_matches(&r.match_process, &process))
        else {
            return 1;
        };
        if let Some(media) = parse_caption(&caption, &process, &rule.id()) {
            ctx.found = Some(media);
            return 0;
        }
        1
    }

    let mut ctx = Ctx {
        players: &players,
        found: None,
    };
    unsafe {
        EnumWindows(Some(each), &mut ctx as *mut Ctx as LPARAM);
    }
    ctx.found
}

/// `track - artist`, with the player's own trailing name dropped.
///
/// `player` is the rule's public id, used only to recognise the player's own name
/// when it appears as a caption segment.
pub fn parse_caption(caption: &str, process: &str, player: &str) -> Option<RawMedia> {
    let parts: Vec<&str> = caption
        .split(" - ")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() < 2 {
        return None;
    }
    // Most players append their own name ("夜曲 - 周杰伦 - 网易云音乐"); that tail is
    // not part of the track and would otherwise be read as the artist.
    let last = parts[parts.len() - 1];
    let end = if parts.len() > 2 && looks_like_player_name(last, player) {
        parts.len() - 1
    } else {
        parts.len()
    };
    let track = parts[0];
    let artist = parts[1..end].join(" / ");
    if track.is_empty() || artist.is_empty() {
        return None;
    }

    Some(RawMedia {
        state: MediaState::Playing,
        title: track.to_string(),
        artist,
        album: String::new(),
        app: public_id(process),
        position_s: 0,
        duration_s: 0,
    })
}

/// True for the trailing segment a player puts in its own caption: its own id, its
/// executable stem, or a word that only a player would use.
fn looks_like_player_name(segment: &str, player: &str) -> bool {
    let lower = segment.to_ascii_lowercase();
    if !player.is_empty()
        && (lower == player.to_ascii_lowercase() || lower.contains(&player.to_ascii_lowercase()))
    {
        return true;
    }
    ["音乐", "播放器"].iter().any(|word| segment.contains(word))
        || ["music", "player", "media"]
            .iter()
            .any(|word| lower.contains(word))
}

/// Which session to report.
///
/// `GetCurrentSession` is "whatever Windows considers current", which is often a
/// browser tab that is not playing anything. Asking for the list and preferring a
/// session that is actually playing gives the track the user hears, and a paused
/// one is still worth showing.
fn pick_session(
    manager: &Manager,
    players: &[&Rule],
) -> Option<GlobalSystemMediaTransportControlsSession> {
    let mut paused: Option<GlobalSystemMediaTransportControlsSession> = None;
    let mut fallback: Option<GlobalSystemMediaTransportControlsSession> = None;

    if let Ok(list) = manager.GetSessions() {
        for i in 0..list.Size().unwrap_or(0) {
            let Ok(session) = list.GetAt(i) else { continue };
            if !session_allowed(&session, players) {
                continue;
            }
            match playback_status(&session) {
                PbStatus::Playing => return Some(session),
                PbStatus::Paused => paused = paused.or(Some(session)),
                _ => fallback = fallback.or(Some(session)),
            }
        }
    }

    paused.or(fallback)
}

/// Is this session owned by an app the rules let publish a track?
fn session_allowed(session: &GlobalSystemMediaTransportControlsSession, players: &[&Rule]) -> bool {
    if session.SourceAppUserModelId().is_err() {
        return false;
    }
    // The broker reports a sanitised stem (`cloudmusic`) while the rule is normally
    // written as an executable (`cloudmusic.exe`), so both spellings have to be
    // accepted or the session is silently dropped.
    let player = player_name(session);
    players.iter().any(|rule| {
        rule_matches(&rule.match_process, &player)
            || public_id(&rule.match_process).eq_ignore_ascii_case(&player)
    })
}

fn playback_status(session: &GlobalSystemMediaTransportControlsSession) -> PbStatus {
    session
        .GetPlaybackInfo()
        .and_then(|i| i.PlaybackStatus())
        .unwrap_or(PbStatus::Closed)
}

/// Everything the media broker knows, for `--media`.
///
/// Answers the only question that matters when a player is running and nothing
/// appears: does that player register a session at all, and what does it publish?
pub fn diagnostics(rules: &[Rule]) -> Vec<String> {
    init_com();

    let mut out = Vec::new();
    let manager = match request_manager() {
        Ok(m) => m,
        Err(err) => {
            out.push(format!("cannot reach the system media manager: {err}"));
            return out;
        }
    };

    match manager.GetCurrentSession() {
        Ok(session) => out.push(format!("current session : {}", describe(&session))),
        Err(_) => out.push("current session : none".to_string()),
    }

    let players: Vec<&Rule> = rules.iter().filter(|r| r.allows_media()).collect();

    match manager.GetSessions() {
        Ok(list) => {
            let count = list.Size().unwrap_or(0);
            out.push(format!("sessions        : {count}"));
            for i in 0..count {
                if let Ok(session) = list.GetAt(i) {
                    let verdict = if session_allowed(&session, &players) {
                        "[allowed]"
                    } else {
                        "[not allowed: no rule with category \"media\"]"
                    };
                    out.push(format!("  [{i}] {verdict} {}", describe(&session)));
                }
            }
        }
        Err(err) => out.push(format!("cannot list sessions: {}", err.message())),
    }

    out.push(
        match pick_session(&manager, &players).and_then(|s| read_session(&s)) {
            Some(m) => format!(
                "from the broker : \"{}\" — {}  (player {})",
                m.title, m.artist, m.app
            ),
            None => "from the broker : nothing the rules allow".to_string(),
        },
    );

    out.push(match caption_probe(rules) {
        Some(m) => format!(
            "caption fallback: \"{}\" — {}  (player {})",
            m.title, m.artist, m.app
        ),
        None => "caption fallback: no opted-in player window with a `track - artist` caption"
            .to_string(),
    });

    out
}

fn describe(session: &GlobalSystemMediaTransportControlsSession) -> String {
    let aumid = session
        .SourceAppUserModelId()
        .map(|h| h.to_string())
        .unwrap_or_default();
    let status = playback_status(session);
    let detail = match read_session(session) {
        Some(m) => format!(
            "\"{}\" — {}  (player {}, {}s/{}s)",
            m.title, m.artist, m.app, m.position_s, m.duration_s
        ),
        None => "(a session, but it publishes no title)".to_string(),
    };
    format!("{aumid}\n        status {status:?}, {detail}")
}

fn read_session(session: &GlobalSystemMediaTransportControlsSession) -> Option<RawMedia> {
    let props = session.TryGetMediaPropertiesAsync().ok()?.join().ok()?;

    let title = props.Title().map(|h| h.to_string()).unwrap_or_default();
    if title.trim().is_empty() {
        return None;
    }
    let artist = props.Artist().map(|h| h.to_string()).unwrap_or_default();
    let album = props
        .AlbumTitle()
        .map(|h| h.to_string())
        .unwrap_or_default();

    let state = match session.GetPlaybackInfo().and_then(|i| i.PlaybackStatus()) {
        Ok(PbStatus::Playing) => MediaState::Playing,
        Ok(PbStatus::Paused) => MediaState::Paused,
        Ok(_) => MediaState::Stopped,
        Err(_) => MediaState::Playing,
    };

    let (position_s, duration_s) = match session.GetTimelineProperties() {
        Ok(t) => (
            ticks_to_secs(t.Position().map(|v| v.Duration).unwrap_or(0)),
            ticks_to_secs(t.EndTime().map(|v| v.Duration).unwrap_or(0)),
        ),
        Err(_) => (0, 0),
    };

    Some(RawMedia {
        state,
        title,
        artist,
        album,
        app: player_name(session),
        position_s,
        duration_s,
    })
}

/// How many identical polls in a row mean the playhead is not moving.
const FROZEN_POLLS: u32 = 3;

/// Windows keeps a session alive after the page or app that owned it is gone: the
/// session still claims to be playing while its position never advances. Only a
/// player that publishes a timeline can be judged this way, so a track without one
/// is always kept.
#[derive(Default)]
struct ZombieGuard {
    seen: Option<(String, u64)>,
    frozen: u32,
}

impl ZombieGuard {
    fn is_stale(&mut self, playing: bool, key: &str, position_s: u64, duration_s: u64) -> bool {
        if !playing || duration_s == 0 {
            self.seen = None;
            self.frozen = 0;
            return false;
        }
        let current = (key.to_string(), position_s);
        if self.seen.as_ref() == Some(&current) {
            self.frozen += 1;
        } else {
            self.frozen = 0;
            self.seen = Some(current);
        }
        self.frozen >= FROZEN_POLLS
    }
}

/// AUMIDs arrive in three shapes: `Package_Family!AppId`, `C:\path\app.exe`, and a
/// bare `app.exe`. Splitting on `.` without checking for the extension turns
/// `cloudmusic.exe` into `exe`, so the extension is stripped first.
fn player_name(session: &GlobalSystemMediaTransportControlsSession) -> String {
    let raw = session
        .SourceAppUserModelId()
        .map(|h| h.to_string())
        .unwrap_or_default();
    aumid_to_player(&raw)
}

fn aumid_to_player(raw: &str) -> String {
    let tail = raw.rsplit('!').next().unwrap_or(raw);
    let tail = tail.rsplit(['\\', '/']).next().unwrap_or(tail);
    let lower = tail.to_ascii_lowercase();
    let tail = if lower.ends_with(".exe") {
        &tail[..tail.len() - 4]
    } else if tail.contains('.') {
        tail.rsplit('.').next().unwrap_or(tail)
    } else {
        tail
    };
    let cleaned: String = tail
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .collect();
    cleaned.trim().to_ascii_lowercase()
}

/// TimeSpan counts 100 ns ticks.
fn ticks_to_secs(ticks: i64) -> u64 {
    if ticks <= 0 {
        0
    } else {
        (ticks / 10_000_000) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aumids_reduce_to_a_readable_player_name() {
        assert_eq!(aumid_to_player("cloudmusic.exe"), "cloudmusic");
        assert_eq!(
            aumid_to_player(r"C:\Program Files\Bar\Foobar2000.exe"),
            "foobar2000"
        );
        assert_eq!(
            aumid_to_player("SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify"),
            "spotify"
        );
        assert_eq!(
            aumid_to_player("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"),
            "zunemusic"
        );
    }

    #[test]
    fn a_frozen_playhead_gives_the_session_up() {
        let mut guard = ZombieGuard::default();
        assert!(!guard.is_stale(true, "edge|video", 10, 68));
        assert!(!guard.is_stale(true, "edge|video", 11, 68));
        assert!(!guard.is_stale(true, "edge|video", 12, 68));
        assert!(!guard.is_stale(true, "edge|video", 13, 68));
    }

    #[test]
    fn a_frozen_playhead_is_recognised_after_three_polls() {
        let mut guard = ZombieGuard::default();
        assert!(!guard.is_stale(true, "edge|video", 27, 68));
        assert!(!guard.is_stale(true, "edge|video", 27, 68));
        assert!(!guard.is_stale(true, "edge|video", 27, 68));
        assert!(guard.is_stale(true, "edge|video", 27, 68));
    }

    #[test]
    fn a_track_without_a_timeline_is_never_called_a_zombie() {
        let mut guard = ZombieGuard::default();
        for _ in 0..10 {
            assert!(!guard.is_stale(true, "cloudmusic|song", 0, 0));
        }
        for _ in 0..10 {
            assert!(!guard.is_stale(false, "edge|video", 27, 68));
        }
    }

    #[test]
    fn ticks_convert_to_seconds() {
        assert_eq!(ticks_to_secs(0), 0);
        assert_eq!(ticks_to_secs(-5), 0);
        assert_eq!(ticks_to_secs(10_000_000), 1);
        assert_eq!(ticks_to_secs(215_340_000_000_000 / 1000), 21_534);
    }

    #[test]
    fn parses_the_caption_a_player_puts_in_its_title_bar() {
        let media = parse_caption("怎么了 - 周兴哲", "cloudmusic.exe", "网易云音乐").unwrap();
        assert_eq!(media.title, "怎么了");
        assert_eq!(media.artist, "周兴哲");
        assert_eq!(media.app, "cloudmusic");
        assert_eq!(media.state, MediaState::Playing);
    }

    #[test]
    fn drops_the_player_name_a_caption_tail_carries() {
        let media = parse_caption("夜曲 - 周杰伦 - 网易云音乐", "cloudmusic.exe", "音乐").unwrap();
        assert_eq!(media.title, "夜曲");
        assert_eq!(media.artist, "周杰伦");

        // A feature credit that is not a player name belongs to the artist.
        let media = parse_caption("Song - A - B", "spotify.exe", "音乐").unwrap();
        assert_eq!(media.artist, "A / B");
    }

    #[test]
    fn refuses_a_caption_that_is_not_track_shaped() {
        assert!(parse_caption("网易云音乐", "cloudmusic.exe", "音乐").is_none());
        assert!(parse_caption("", "cloudmusic.exe", "音乐").is_none());
    }
}
