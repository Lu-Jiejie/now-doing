//! The wire format published to the object store, plus the fingerprint used to
//! decide whether a write is needed at all.
//!
//! Rules that shaped this type:
//! * Nullable blocks are always emitted as JSON `null`, never omitted. A reader
//!   must be able to tell "nothing is playing" from "the field was forgotten".
//! * No raw process names or paths ever reach this struct; the privacy pipeline
//!   maps them to public ids first.
//! * There is deliberately no `offline` state. When the machine is offline the
//!   write never lands, so staleness is inferred by the reader from `ts`.

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Presence {
    pub schema: u32,
    pub device: Device,
    pub seq: u64,
    pub status: Status,
    /// Unix seconds, measured on the reporting machine.
    pub ts: i64,
    pub updated_at: String,
    pub heartbeat_s: u64,
    pub focus: Option<Focus>,
    pub media: Option<Media>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    pub name: String,
    pub os: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Idle,
    Paused,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Idle => "idle",
            Status::Paused => "paused",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Focus {
    /// Public, rule-mapped identifier. Never a raw executable name.
    pub app: String,
    /// Free-form tag the reader can switch on. Empty when the rule set none.
    #[serde(default)]
    pub category: String,
    /// Display names keyed by language tag, e.g. `{"zh": "绝区零"}`. Omitted when
    /// the rule set none, so a reader that only wants an id pays nothing.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub names: std::collections::BTreeMap<String, String>,
    /// Icon URL from the rule that matched. Omitted when the rule sets none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Unix seconds when this app first became focused.
    pub since: i64,
    /// Seconds elapsed in this focus session.
    pub seconds: u64,
}

/// Order-independent rendering of the display names, so that renaming a window
/// counts as a change worth uploading while unchanged names stay silent.
pub fn names_key(names: &std::collections::BTreeMap<String, String>) -> String {
    names
        .iter()
        .map(|(lang, value)| format!("{lang}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Media {
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub app: String,
    /// Icon URL of the player, from the rule that opted into publishing tracks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaState {
    Playing,
    Paused,
    Stopped,
}

impl Presence {
    /// Everything that should trigger a fresh upload. Playback position and the
    /// elapsed-seconds counters are excluded on purpose: they change every
    /// second without any user-visible meaning.
    pub fn fingerprint(&self) -> String {
        let focus = match &self.focus {
            Some(f) => format!(
                "{}|{}|{}|{}",
                f.app,
                f.category,
                names_key(&f.names),
                f.icon.as_deref().unwrap_or("")
            ),
            None => "-".into(),
        };
        let media = match &self.media {
            Some(m) => format!(
                "{}|{}|{}|{}",
                m.title,
                m.artist,
                m.app,
                m.icon.as_deref().unwrap_or("")
            ),
            None => "-".into(),
        };
        format!("{}|{}|{}", self.status.as_str(), focus, media)
    }
}

impl MediaState {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaState::Playing => "playing",
            MediaState::Paused => "paused",
            MediaState::Stopped => "stopped",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Presence {
        Presence {
            schema: SCHEMA_VERSION,
            device: Device {
                id: "dev-1".into(),
                name: "desk".into(),
                os: "windows".into(),
            },
            seq: 7,
            status: Status::Active,
            ts: 1_700_000_000,
            updated_at: "2023-11-14T22:13:20Z".into(),
            heartbeat_s: 90,
            focus: Some(Focus {
                app: "coding".into(),
                category: "work".into(),
                names: Default::default(),
                icon: None,
                since: 1_699_999_000,
                seconds: 1000,
            }),
            media: None,
        }
    }

    #[test]
    fn nullable_blocks_serialize_as_null() {
        let json = serde_json::to_string(&sample()).unwrap();
        assert!(json.contains(r#""media":null"#), "{json}");
        assert!(!json.contains("positionS"));
    }

    #[test]
    fn roundtrip() {
        let p = sample();
        let back: Presence = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn status_uses_lowercase() {
        let json = serde_json::to_string(&sample()).unwrap();
        assert!(json.contains(r#""status":"active""#));
        assert!(json.contains(r#""schema":1"#));
    }

    #[test]
    fn fingerprint_ignores_clock_fields() {
        let a = sample();
        let mut b = sample();
        b.ts += 500;
        b.seq = 99;
        b.focus.as_mut().unwrap().seconds = 9_999;
        b.updated_at = "2030-01-01T00:00:00Z".into();
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn fingerprint_tracks_media_changes() {
        let mut a = sample();
        a.media = Some(Media {
            title: "Song A".into(),
            artist: "X".into(),
            app: "music".into(),
            icon: None,
        });
        let mut b = a.clone();
        assert_eq!(a.fingerprint(), b.fingerprint());
        b.media.as_mut().unwrap().title = "Song B".into();
        assert_ne!(a.fingerprint(), b.fingerprint());
        let mut c = a.clone();
        c.media.as_mut().unwrap().artist = "Y".into();
        assert_ne!(a.fingerprint(), c.fingerprint());
    }
}
