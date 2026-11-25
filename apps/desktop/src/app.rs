//! Shared runtime state between the tray, the collector and the publisher.

use std::sync::{Arc, Mutex};

use crate::config;

#[derive(Default)]
pub struct Runtime {
    pub config: config::Config,
    /// Public id of the app currently being published, used for the tooltip.
    pub app: Option<String>,
    /// Title of the track currently being published, used for the tooltip.
    pub track: Option<String>,
    /// Set by the tray's "sync now" item; the publisher clears it.
    pub force_upload: bool,
    /// Unix seconds of the last successful upload.
    pub last_upload: Option<i64>,
    /// Message of the last failed upload, cleared on the next success.
    pub last_error: Option<String>,
    pub quit: bool,
}

pub type Shared = Arc<Mutex<Runtime>>;

pub fn shared(cfg: config::Config) -> Shared {
    Arc::new(Mutex::new(Runtime {
        config: cfg,
        ..Runtime::default()
    }))
}

impl Runtime {
    pub fn tooltip(&self) -> String {
        if self.config.paused {
            return "now-doing · 已暂停".into();
        }
        if self.last_error.is_some() {
            return "now-doing · 上报失败，后台重试中".into();
        }
        let app = self.app.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let track = self
            .track
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match (app, track) {
            // 45 characters is about what a tray tooltip shows before Windows cuts it,
            // and the track is the part worth keeping.
            (Some(_), Some(track)) => format!("now-doing · ♪ {}", truncate(track, 32)),
            (Some(app), None) => format!("now-doing · {app}"),
            (None, Some(track)) => format!("now-doing · ♪ {}", truncate(track, 32)),
            (None, None) => "now-doing · 待命".into(),
        }
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub fn with<R>(state: &Shared, f: impl FnOnce(&mut Runtime) -> R) -> R {
    match state.lock() {
        Ok(mut guard) => f(&mut guard),
        Err(poisoned) => f(&mut poisoned.into_inner()),
    }
}

pub fn config_of(state: &Shared) -> config::Config {
    with(state, |rt| rt.config.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> Runtime {
        Runtime::default()
    }

    #[test]
    fn the_tooltip_prefers_the_track() {
        let mut rt = runtime();
        rt.app = Some("cloudmusic".into());
        rt.track = Some("麦恩莉".into());
        assert_eq!(rt.tooltip(), "now-doing · ♪ 麦恩莉");
    }

    #[test]
    fn pausing_wins_over_everything() {
        let mut rt = runtime();
        rt.app = Some("code".into());
        rt.config.paused = true;
        rt.last_error = Some("timeout".into());
        assert_eq!(rt.tooltip(), "now-doing · 已暂停");
    }
}
