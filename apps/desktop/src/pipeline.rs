//! Privacy pipeline: raw sample in, publishable snapshot out.
//!
//! Default privacy means the pipeline is fail-closed. An app with no matching
//! rule is not published at all unless the user explicitly chose a generic
//! fallback. The raw executable name is the only thing ever matched, and it never
//! leaves this module: what goes out is the rule's public id and category, and the
//! reader supplies every human-readable string.
//!
//! The one exception to "publish nothing but ids" is a track title, and that
//! needs an explicit `category: "media"` on the app's rule — a browser playing a
//! video is not allowed to leak what it plays just because it has a rule.

use crate::collector::RawState;
use crate::config::{PrivacyConfig, Rule};
use crate::protocol::{Focus, Media, Status};

/// Characters that would let a title smuggle a URL or a filesystem path onto the
/// blog. Long titles are cut at a word-ish boundary rather than mid-word.
const MAX_TEXT: usize = 80;

/// Marker used when an unlisted app is published under the generic fallback.
pub const GENERIC_ID: &str = "other";

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Snapshot {
    pub status: Status,
    pub focus: Option<Focus>,
    pub media: Option<Media>,
}

impl Snapshot {
    /// Identity of a state change, with the clock fields left out.
    ///
    /// `since` and `seconds` move on their own even when nothing happened;
    /// including them would make every sample look like a change and turn the
    /// debounce into a one-hertz upload loop.
    pub fn fingerprint(&self) -> String {
        let focus = match &self.focus {
            Some(f) => format!(
                "{}|{}|{}",
                f.app,
                f.category,
                crate::protocol::names_key(&f.names)
            ),
            None => "-".to_string(),
        };
        let media = match &self.media {
            Some(m) => format!("{}|{}|{}", m.title, m.artist, m.app),
            None => "-".to_string(),
        };
        format!("{}|{}|{}", self.status.as_str(), focus, media)
    }
}

/// Everything the pipeline needs to look at one sample.
pub struct Pipeline<'a> {
    pub privacy: &'a PrivacyConfig,
    pub paused: bool,
    pub idle_threshold_s: u64,
}

impl<'a> Pipeline<'a> {
    pub fn new(privacy: &'a PrivacyConfig, paused: bool) -> Self {
        Self {
            privacy,
            paused,
            // Clamped so a typo in the config cannot make the machine report
            // "idle" while the user is typing.
            idle_threshold_s: privacy.idle_after_secs.clamp(30, 86_400),
        }
    }

    pub fn evaluate(&self, raw: &RawState) -> Snapshot {
        if self.paused {
            return Snapshot {
                status: Status::Paused,
                focus: None,
                media: None,
            };
        }

        let status = if raw.idle_secs >= self.idle_threshold_s {
            Status::Idle
        } else {
            Status::Active
        };

        let media = self.media(raw);

        let mut focus = self.focus(raw);
        // Being away should not also mean "and here is what I left open": the JSON
        // is public even when the blog card only prints a single word.
        if status == Status::Idle && self.privacy.hide_focus_when_idle {
            focus = None;
        }
        // Focusing the player you are listening to says nothing the now-playing
        // block does not already say, and the reader draws them as separate cards.
        if media.is_some()
            && focus
                .as_ref()
                .is_some_and(|f| f.category.eq_ignore_ascii_case("media"))
        {
            focus = None;
        }

        Snapshot {
            status,
            focus,
            media,
        }
    }

    fn focus(&self, raw: &RawState) -> Option<Focus> {
        let process = raw.process.as_deref()?;

        let (app, category, names, icon) = match self.match_rule(process) {
            Some(rule) => (
                rule.id(),
                rule.category.trim().to_string(),
                clean_names(&rule.names),
                clean_icon(rule.icon.as_deref()),
            ),
            None => match self.privacy.unknown_app.as_str() {
                "generic" => (
                    GENERIC_ID.to_string(),
                    GENERIC_ID.to_string(),
                    Default::default(),
                    None,
                ),
                // `hide` and any unknown value both fall through to hiding.
                _ => return None,
            },
        };

        let now = crate::util::unix_now();
        Some(Focus {
            app: sanitize_id(&app),
            category: category.chars().take(32).collect(),
            names,
            icon,
            since: raw.focus_since.min(now),
            seconds: (now - raw.focus_since).max(0) as u64,
        })
    }

    fn match_rule(&self, process: &str) -> Option<&Rule> {
        self.privacy
            .rules
            .iter()
            .find(|rule| rule_matches(&rule.match_process, process))
    }

    /// The rule that owns whatever is playing, matched on the player's public id.
    ///
    /// Two spellings have to be tried: the media watcher reports a sanitised
    /// executable stem (`cloudmusic`), while the rule is usually written as
    /// `cloudmusic.exe`.
    fn media_rule(&self, app: &str) -> Option<&Rule> {
        self.privacy.rules.iter().find(|rule| {
            rule_matches(&rule.match_process, app)
                || public_id(&rule.match_process).eq_ignore_ascii_case(app)
        })
    }

    fn media(&self, raw: &RawState) -> Option<Media> {
        let media = raw.media.as_ref()?;
        if media.title.trim().is_empty() {
            return None;
        }
        // Publishing a track is a separate promise from publishing "this app is
        // focused": only a rule that says `category: "media"` may do it. Without
        // this gate a browser with a rule would leak whatever video it plays.
        let rule = self.media_rule(&media.app)?;
        if !rule.allows_media() {
            return None;
        }
        // A player can still open a file whose name the user would not want on a
        // public page. One keyword hit drops the whole block rather than editing it.
        let haystack = format!("{} {}", media.title, media.artist).to_ascii_lowercase();
        if self
            .privacy
            .media_block_keywords
            .iter()
            .map(|word| word.trim().to_ascii_lowercase())
            .any(|word| !word.is_empty() && haystack.contains(&word))
        {
            return None;
        }

        Some(Media {
            title: clean_text(&media.title, MAX_TEXT),
            artist: clean_text(&media.artist, MAX_TEXT),
            app: clean_text(&media.app, 32),
            icon: clean_icon(rule.icon.as_deref()),
        })
    }
}

/// `*` acts as a prefix wildcard so one rule can cover a family of binaries
/// (`Code*`); matching is otherwise case-insensitive and exact.
pub fn rule_matches(pattern: &str, process: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        process
            .to_ascii_lowercase()
            .starts_with(&prefix.to_ascii_lowercase())
    } else {
        pattern.eq_ignore_ascii_case(process)
    }
}

/// A stable, non-identifying handle for an app: derived from its name but
/// stripped of anything that could reveal a build or a user path.
pub(crate) fn public_id(process: &str) -> String {
    let stem = process
        .trim_end_matches(".exe")
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(process);
    let id: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let id = id.trim_matches('-').to_string();
    if id.is_empty() {
        GENERIC_ID.into()
    } else {
        id
    }
}

/// Display names are free text from the config, so they get the same care as a
/// track title: no control characters, trimmed, blanks dropped, length capped,
/// and only the first few languages kept. A name is a label, not a paragraph.
pub fn clean_names(
    names: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    const MAX_LANGS: usize = 4;
    const MAX_NAME: usize = 64;
    names
        .iter()
        .filter_map(|(lang, value)| {
            let lang: String = lang
                .trim()
                .to_ascii_lowercase()
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            let value: String = value.chars().filter(|c| !c.is_control()).collect();
            let value = value.trim();
            if lang.is_empty() || value.is_empty() {
                return None;
            }
            Some((lang, value.chars().take(MAX_NAME).collect()))
        })
        .take(MAX_LANGS)
        .collect()
}

/// Only a plain http(s) link is published: anything else is either a local path
/// or a scheme the browser would refuse to load from a public page.
pub fn clean_icon(icon: Option<&str>) -> Option<String> {
    const MAX_ICON: usize = 512;
    let icon = icon?.trim();
    let lower = icon.to_ascii_lowercase();
    if icon.is_empty()
        || icon.len() > MAX_ICON
        || icon.chars().any(char::is_control)
        || !(lower.starts_with("https://") || lower.starts_with("http://"))
    {
        return None;
    }
    Some(icon.to_string())
}

/// The published form of a public id: readable, stable, and safe to put in a URL.
///
/// Unlike `public_id` this keeps non-ASCII letters: a display name may be written
/// in any script, and folding it to ASCII used to collapse names into `other`.
pub(crate) fn sanitize_id(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.trim().chars() {
        if c.is_alphanumeric() || c == '_' || c == '.' {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let id: String = out.trim_matches('-').chars().take(64).collect();
    let id = id.trim_matches('-').to_string();
    if id.is_empty() {
        GENERIC_ID.into()
    } else {
        id
    }
}

/// Drop URLs and path fragments, collapse whitespace, and truncate on a character
/// boundary. Titles come from arbitrary applications and must be treated as
/// hostile input.
pub fn clean_text(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max));
    for token in text.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        let is_link = lower.starts_with("http://")
            || lower.starts_with("https://")
            || lower.starts_with("www.")
            || token.contains("://");
        if is_link {
            continue;
        }
        // A bare Windows path or a POSIX path is a filename leak, not a title.
        let looks_like_path = (token.contains('\\') && token.len() > 3)
            || (token.matches('/').count() >= 2 && token.contains('.'));
        let token = if looks_like_path {
            token
                .rsplit(['\\', '/'])
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("")
        } else {
            token
        };
        if token.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(token);
        if out.chars().count() >= max {
            break;
        }
    }
    truncate_chars(&out, max)
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Rule;

    fn privacy(rules: Vec<Rule>, unknown: &str) -> PrivacyConfig {
        PrivacyConfig {
            unknown_app: unknown.into(),
            idle_after_secs: 120,
            media_block_keywords: Vec::new(),
            hide_focus_when_idle: true,
            rules,
        }
    }

    fn rule(process: &str, category: &str) -> Rule {
        Rule {
            match_process: process.into(),
            category: category.into(),
            names: Default::default(),
            icon: None,
        }
    }

    fn raw(process: &str) -> RawState {
        RawState {
            process: Some(process.into()),
            focus_since: crate::util::unix_now() - 10,
            ..RawState::default()
        }
    }

    fn playing(app: &str) -> RawState {
        let mut state = raw("Code.exe");
        state.media = Some(crate::collector::RawMedia {
            state: crate::protocol::MediaState::Playing,
            title: "Song".into(),
            artist: "Someone".into(),
            album: String::new(),
            app: app.into(),
            position_s: 1,
            duration_s: 200,
        });
        state
    }

    #[test]
    fn unlisted_app_is_hidden_by_default() {
        let p = privacy(vec![], "hide");
        let pipe = Pipeline::new(&p, false);
        assert!(pipe.evaluate(&raw("Secret.exe")).focus.is_none());
    }

    #[test]
    fn unlisted_app_uses_generic_when_asked() {
        let p = privacy(vec![], "generic");
        let pipe = Pipeline::new(&p, false);
        let focus = pipe.evaluate(&raw("Secret.exe")).focus.unwrap();
        assert_eq!(focus.app, GENERIC_ID);
        assert_eq!(focus.category, GENERIC_ID);
        assert!(!focus.app.contains("secret"));
    }

    #[test]
    fn a_matched_app_never_leaks_the_exe_name() {
        let p = privacy(vec![rule("Code.exe", "dev")], "hide");
        let pipe = Pipeline::new(&p, false);
        let focus = pipe.evaluate(&raw("Code.exe")).focus.unwrap();
        assert_eq!(focus.app, "code");
        assert_eq!(focus.category, "dev");
    }

    #[test]
    fn a_rule_may_carry_an_icon_link() {
        let mut r = rule("Code.exe", "dev");
        r.icon = Some("https://cdn.simpleicons.org/visualstudiocode".into());
        let p = privacy(vec![r], "hide");
        let pipe = Pipeline::new(&p, false);
        assert_eq!(
            pipe.evaluate(&raw("Code.exe"))
                .focus
                .unwrap()
                .icon
                .as_deref(),
            Some("https://cdn.simpleicons.org/visualstudiocode")
        );
    }

    #[test]
    fn an_icon_that_is_not_a_link_is_dropped() {
        assert!(clean_icon(Some("C:\\Windows\\code.ico")).is_none());
        assert!(clean_icon(Some("javascript:alert(1)")).is_none());
        assert!(clean_icon(Some("   ")).is_none());
        assert!(clean_icon(Some(&format!("https://x.test/{}", "a".repeat(600)))).is_none());
        assert_eq!(
            clean_icon(Some(" https://x.test/a.png ")).as_deref(),
            Some("https://x.test/a.png")
        );
    }

    #[test]
    fn a_playing_player_is_not_a_focus() {
        // The reader draws "now playing" as its own card, so a focus line naming
        // the player itself would only repeat it.
        let p = privacy(vec![rule("cloudmusic.exe", "media")], "hide");
        let out = Pipeline::new(&p, false).evaluate(&playing("cloudmusic"));
        assert!(out.media.is_some());
        assert!(out.focus.is_none());

        // Nothing playing: focusing the player is real information again.
        let quiet = Pipeline::new(&p, false).evaluate(&raw("cloudmusic.exe"));
        assert_eq!(quiet.focus.map(|f| f.app), Some("cloudmusic".to_string()));
    }

    #[test]
    fn a_rule_may_carry_a_display_name_per_language() {
        let mut names = std::collections::BTreeMap::new();
        names.insert("zh".to_string(), "绝区零".to_string());
        names.insert("en".to_string(), "Zenless Zone Zero".to_string());
        let privacy = privacy(
            vec![Rule {
                match_process: "ZenlessZoneZero.exe".into(),
                category: "game".into(),
                names,
                icon: None,
            }],
            "hide",
        );
        let out = Pipeline::new(&privacy, false).evaluate(&raw("ZenlessZoneZero.exe"));
        let focus = out.focus.expect("the rule matches");
        assert_eq!(focus.app, "zenlesszonezero");
        assert_eq!(focus.category, "game");
        assert_eq!(focus.names.get("zh").map(String::as_str), Some("绝区零"));
        assert_eq!(
            focus.names.get("en").map(String::as_str),
            Some("Zenless Zone Zero")
        );
    }

    #[test]
    fn a_renamed_window_is_a_change_worth_uploading() {
        let mut names = std::collections::BTreeMap::new();
        names.insert("zh".to_string(), "绝区零".to_string());
        let one = privacy(
            vec![Rule {
                match_process: "ZenlessZoneZero.exe".into(),
                category: "game".into(),
                names: names.clone(),
                icon: None,
            }],
            "hide",
        );
        names.insert("zh".to_string(), "绝区零 2".to_string());
        let two = privacy(
            vec![Rule {
                match_process: "ZenlessZoneZero.exe".into(),
                category: "game".into(),
                names,
                icon: None,
            }],
            "hide",
        );
        let a = Pipeline::new(&one, false).evaluate(&raw("ZenlessZoneZero.exe"));
        let b = Pipeline::new(&two, false).evaluate(&raw("ZenlessZoneZero.exe"));
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn names_are_cleaned_and_capped() {
        let mut names = std::collections::BTreeMap::new();
        names.insert(" ZH ".to_string(), "  绝区零\u{0}\n".to_string());
        names.insert(String::new(), "ignored".to_string());
        names.insert("en".to_string(), "   ".to_string());
        let cleaned = clean_names(&names);
        assert_eq!(cleaned.len(), 1);
        assert_eq!(cleaned.get("zh").map(String::as_str), Some("绝区零"));
    }

    #[test]
    fn sanitize_id_keeps_what_a_reader_can_type() {
        assert_eq!(sanitize_id("Code"), "code");
        assert_eq!(sanitize_id("  Hello   World  "), "hello-world");
        assert_eq!(sanitize_id("a/b?c"), "a-b-c");
        assert_eq!(sanitize_id("---"), GENERIC_ID);
        assert_eq!(sanitize_id(""), GENERIC_ID);
        assert_eq!(sanitize_id(&"x".repeat(200)).chars().count(), 64);
    }
    #[test]
    fn wildcard_rules_match_a_family() {
        assert!(rule_matches("Code*", "Code - Insiders.exe"));
        assert!(rule_matches("code.exe", "CODE.EXE"));
        assert!(!rule_matches("Code.exe", "CodeC.exe"));
        assert!(!rule_matches("", "Code.exe"));
    }

    #[test]
    fn paused_wins_over_everything() {
        let p = privacy(vec![rule("Code.exe", "dev")], "generic");
        let pipe = Pipeline::new(&p, true);
        let snap = pipe.evaluate(&raw("Code.exe"));
        assert_eq!(snap.status, Status::Paused);
        assert!(snap.focus.is_none() && snap.media.is_none());
    }

    #[test]
    fn idle_is_inferred_from_input_age() {
        let p = privacy(vec![rule("Code.exe", "dev")], "generic");
        let pipe = Pipeline::new(&p, false);
        let mut state = raw("Code.exe");
        state.idle_secs = 10;
        assert_eq!(pipe.evaluate(&state).status, Status::Active);
        state.idle_secs = 600;
        assert_eq!(pipe.evaluate(&state).status, Status::Idle);
    }

    #[test]
    fn the_idle_line_is_configurable() {
        let mut p = privacy(vec![rule("Code.exe", "dev")], "hide");
        p.idle_after_secs = 30;
        let pipe = Pipeline::new(&p, false);
        let mut state = raw("Code.exe");
        state.idle_secs = 45;
        assert_eq!(pipe.evaluate(&state).status, Status::Idle);
    }

    #[test]
    fn media_is_dropped_when_the_player_is_not_listed() {
        let p = privacy(vec![rule("Code.exe", "dev")], "hide");
        let pipe = Pipeline::new(&p, false);
        assert!(pipe.evaluate(&playing("spotify")).media.is_none());
    }

    #[test]
    fn a_browser_with_a_rule_still_cannot_publish_what_it_plays() {
        // The whole point of `category: "media"`: having a rule is not consent to
        // reveal the video playing in that window.
        let p = privacy(vec![rule("msedge.exe", "browse")], "hide");
        let pipe = Pipeline::new(&p, false);
        assert!(pipe.evaluate(&playing("msedge")).media.is_none());
    }

    #[test]
    fn media_is_kept_for_a_rule_that_opted_in() {
        let p = privacy(
            vec![rule("Code.exe", "dev"), rule("spotify.exe", "media")],
            "hide",
        );
        let pipe = Pipeline::new(&p, false);
        let media = pipe.evaluate(&playing("spotify")).media.unwrap();
        assert_eq!(media.title, "Song");
        assert_eq!(media.app, "spotify");
    }

    #[test]
    fn media_needs_a_session_even_when_the_rule_allows_it() {
        let p = privacy(vec![rule("spotify.exe", "media")], "hide");
        let pipe = Pipeline::new(&p, false);
        assert!(pipe.evaluate(&raw("spotify.exe")).media.is_none());
    }

    #[test]
    fn a_blocked_keyword_drops_the_whole_track() {
        let mut p = privacy(vec![rule("vlc.exe", "media")], "hide");
        p.media_block_keywords = vec!["  Secret  ".into(), String::new()];
        let pipe = Pipeline::new(&p, false);

        let mut state = playing("vlc");
        if let Some(m) = state.media.as_mut() {
            m.title = "secret holiday video".into();
        }
        assert!(pipe.evaluate(&state).media.is_none());

        let mut clean = playing("vlc");
        if let Some(m) = clean.media.as_mut() {
            m.title = "A Normal Album Track".into();
        }
        assert!(pipe.evaluate(&clean).media.is_some());
    }

    #[test]
    fn clean_text_strips_links_and_paths() {
        assert_eq!(clean_text("hello  https://a.b/c  world", 80), "hello world");
        assert_eq!(
            clean_text(r"file C:\Users\me\a.txt here", 80),
            "file a.txt here"
        );
    }

    #[test]
    fn clean_text_truncates_with_an_ellipsis() {
        let long = "字".repeat(200);
        let out = clean_text(&long, 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn a_track_is_matched_by_the_executable_stem() {
        // Regression: the media watcher reports the player by its executable stem
        // (`cloudmusic`), while the rule is written as `cloudmusic.exe`.
        let p = PrivacyConfig {
            unknown_app: "hide".into(),
            idle_after_secs: 120,
            media_block_keywords: Vec::new(),
            hide_focus_when_idle: true,
            rules: vec![Rule {
                match_process: "cloudmusic.exe".into(),
                category: "media".into(),
                names: Default::default(),
                icon: None,
            }],
        };
        let out = Pipeline::new(&p, false).evaluate(&playing("cloudmusic"));
        assert_eq!(out.media.map(|m| m.title), Some("Song".to_string()));
    }

    #[test]
    fn the_media_block_carries_the_players_icon() {
        let mut r = rule("cloudmusic.exe", "media");
        r.icon = Some("https://cdn.simpleicons.org/neteasecloudmusic/D43C3C".into());
        let p = privacy(vec![r], "hide");
        let out = Pipeline::new(&p, false).evaluate(&playing("cloudmusic"));
        assert_eq!(
            out.media.unwrap().icon.as_deref(),
            Some("https://cdn.simpleicons.org/neteasecloudmusic/D43C3C")
        );
    }

    #[test]
    fn a_browser_rule_still_cannot_publish_a_track() {
        // Only `category: "media"` opts in; an icon does not change that.
        let mut r = rule("chrome.exe", "browse");
        r.icon = Some("https://cdn.simpleicons.org/googlechrome".into());
        let p = privacy(vec![r], "hide");
        let out = Pipeline::new(&p, false).evaluate(&playing("chrome"));
        assert!(out.media.is_none());
    }

    #[test]
    fn being_away_hides_what_was_left_open() {
        let p = privacy(vec![rule("code.exe", "dev")], "hide");
        let mut state = raw("code.exe");
        state.idle_secs = 600;
        let out = Pipeline::new(&p, false).evaluate(&state);
        assert_eq!(out.status, crate::protocol::Status::Idle);
        assert!(out.focus.is_none());
    }

    #[test]
    fn focus_can_survive_being_away_if_asked_to() {
        let mut p = privacy(vec![rule("code.exe", "dev")], "hide");
        p.hide_focus_when_idle = false;
        let mut state = raw("code.exe");
        state.idle_secs = 600;
        assert!(Pipeline::new(&p, false).evaluate(&state).focus.is_some());
    }
}
