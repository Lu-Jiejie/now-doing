//! State collection: foreground app, idle time, and the current media session.
//!
//! The collector runs at a low fixed rate (1 Hz by default, slower while the
//! screen is locked or an exclusive fullscreen app is in front). Every read is a
//! user32/COM lookup with no enumeration and no hooks, so the whole tick stays in
//! the tens-of-microseconds range.

pub mod focus;
pub mod idle;
pub mod media;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::app;

pub use media::RawMedia;

/// One sampled moment, before any privacy mapping.
#[derive(Debug, Clone, Default)]
pub struct RawState {
    /// Bare executable name, e.g. `Code.exe`.
    pub process: Option<String>,
    /// Window title. Only the diagnostics commands fill this in: nothing that is
    /// published is derived from the foreground title any more, so the resident
    /// loop never reads one.
    pub title: Option<String>,
    pub media: Option<RawMedia>,
    pub idle_secs: u64,
    /// Unix seconds when the current `process` first became focused.
    pub focus_since: i64,
}

pub type SharedState = Arc<Mutex<RawState>>;

pub fn shared() -> SharedState {
    Arc::new(Mutex::new(RawState::default()))
}

/// Spawn the sampling thread. It never blocks the caller and never returns an
/// error upward: a failed sample simply leaves the previous state in place.
pub fn spawn(shared: SharedState, state: app::Shared) {
    start(shared, state, false);
}

/// Same thread, but reading the foreground window title on every tick. Used by
/// `--watch` / `--probe` so the user can see what is in front of them while
/// writing a rule; the title is printed and then forgotten.
pub fn spawn_for_preview(shared: SharedState, state: app::Shared) {
    start(shared, state, true);
}

fn start(shared: SharedState, state: app::Shared, read_titles: bool) {
    std::thread::Builder::new()
        .name("collector".into())
        .stack_size(128 * 1024)
        .spawn(move || run(shared, state, read_titles))
        .ok();
}

fn run(shared: SharedState, state: app::Shared, read_titles: bool) {
    let media = media::MediaWatcher::spawn(Duration::from_secs(3), state.clone());

    let mut last_process: Option<String> = None;
    let mut since = crate::util::unix_now();

    loop {
        // Nothing is published while paused, so the only thing worth doing is
        // noticing the unpause — sampling a window nobody will see is pure waste.
        if app::with(&state, |rt| rt.config.paused) {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }

        let fg = focus::foreground(read_titles);
        let idle_secs = idle::seconds_since_input();
        let fullscreen = fg.as_ref().is_some_and(|f| f.fullscreen);

        let process = fg.as_ref().map(|f| f.process.clone());
        if process != last_process {
            last_process = process.clone();
            since = crate::util::unix_now();
        }

        let next = RawState {
            process,
            title: fg.and_then(|f| f.title),
            media: media.latest(),
            idle_secs,
            focus_since: since,
        };

        if let Ok(mut slot) = shared.lock() {
            *slot = next;
        }
        std::thread::sleep(sample_interval(idle_secs, fullscreen));
    }
}

/// Nobody is watching the screen, so there is nothing to notice quickly. Slowing
/// down while idle keeps a sleeping machine at a true 0 % CPU. A fullscreen
/// foreground window means a game or a film is running, so the slowest tier is
/// used regardless of input activity — a 5 s staleness is invisible there, an
/// extra timer tick during a frame is not.
fn sample_interval(idle_secs: u64, fullscreen: bool) -> Duration {
    if fullscreen {
        return Duration::from_secs(5);
    }
    match idle_secs {
        0..=120 => Duration::from_secs(1),
        121..=600 => Duration::from_secs(2),
        _ => Duration::from_secs(5),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_slows_down_as_the_user_goes_away() {
        assert_eq!(sample_interval(0, false), Duration::from_secs(1));
        assert_eq!(sample_interval(120, false), Duration::from_secs(1));
        assert_eq!(sample_interval(121, false), Duration::from_secs(2));
        assert_eq!(sample_interval(601, false), Duration::from_secs(5));
    }

    #[test]
    fn fullscreen_takes_the_slowest_tier_even_when_active() {
        assert_eq!(sample_interval(0, true), Duration::from_secs(5));
    }
}
