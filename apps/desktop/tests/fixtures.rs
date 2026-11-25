//! Cross-check the Rust writer against the protocol fixtures.
//!
//! The fixtures are also consumed by the TypeScript client, so if either side
//! drifts away from the schema one of the two test suites fails.

use std::path::PathBuf;

use now_doing::protocol::Presence;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/protocol/fixtures")
}

fn load(name: &str) -> Presence {
    let path = fixtures_dir().join(name);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

#[test]
fn fixtures_round_trip() {
    for name in ["active-with-media.json", "idle-nothing.json", "paused.json"] {
        let presence = load(name);
        let text = serde_json::to_string(&presence).unwrap();
        let back: Presence = serde_json::from_str(&text).unwrap();
        assert_eq!(presence, back, "{name} did not survive a round trip");
    }
}

#[test]
fn absent_blocks_are_explicit_null() {
    for name in ["idle-nothing.json", "paused.json"] {
        let presence = load(name);
        let text = serde_json::to_string(&presence).unwrap();
        assert!(text.contains(r#""focus":null"#), "{name}: {text}");
        assert!(text.contains(r#""media":null"#), "{name}: {text}");
    }
}

#[test]
fn active_fixture_keeps_every_field() {
    let presence = load("active-with-media.json");
    assert_eq!(presence.status.as_str(), "active");
    let focus = presence.focus.expect("focus block");
    assert_eq!(focus.app, "code");
    let media = presence.media.expect("media block");
    assert_eq!(media.app, "cloudmusic");
    assert!(!media.title.is_empty());
}
