//! Persistent configuration, stored as JSON next to the executable.
//!
//! Everything is written atomically so a crash mid-write can never leave a
//! truncated file behind.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Bumped whenever the on-disk shape changes in a way that needs migration.
pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// Schema version of this file, used for forward migration.
    #[serde(default = "default_version")]
    pub version: u32,

    /// When true the collector keeps running but nothing is published.
    #[serde(default)]
    pub paused: bool,

    /// Mirror of the HKCU Run entry, kept here so the UI can render the toggle
    /// without touching the registry on every paint.
    #[serde(default)]
    pub autostart: bool,

    /// Heartbeat interval in seconds. A write happens at most this often even
    /// when nothing changed, so the blog can tell "idle" from "offline".
    #[serde(default = "default_heartbeat")]
    pub heartbeat_secs: u64,

    /// Shortest gap between two writes when the state keeps changing. Raising it
    /// merges rapid app-switching into fewer snapshots.
    #[serde(default = "default_min_gap")]
    pub min_gap_secs: u64,

    /// How long a new state must hold before it counts as a change. A quick switch
    /// away and back never becomes news at all.
    #[serde(default = "default_change_debounce")]
    pub change_debounce_secs: u64,

    /// Optional HTTP proxy for the upload path, e.g. `http://127.0.0.1:7890`.
    /// Empty means a direct connection.
    #[serde(default)]
    pub proxy: Option<String>,

    /// Name published in `device.name`. Empty publishes the bare word `desktop`.
    /// The machine name is not used by default: the payload is world-readable and
    /// a hostname is a stronger identifier than it looks.
    #[serde(default)]
    pub device_name: Option<String>,

    /// Sample and filter as usual but write the payload to `data/presence.json`
    /// instead of uploading it, so a rule set can be tried without publishing.
    #[serde(default)]
    pub dry_run: bool,

    /// Target endpoint block. Filled in during M3, present now so the config file
    /// is stable and the settings pane has something to bind to.
    #[serde(default)]
    pub sink: SinkConfig,

    /// Privacy pipeline settings, wired up in M2.
    #[serde(default)]
    pub privacy: PrivacyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SinkConfig {
    /// `r2` is the only implemented backend for now.
    #[serde(default = "default_sink_kind")]
    pub kind: String,
    /// S3 endpoint, e.g. `https://<account>.r2.cloudflarestorage.com`.
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub bucket: String,
    /// Object key inside the bucket.
    #[serde(default = "default_object_key")]
    pub object_key: String,
    /// Public read URL served through the custom domain, e.g.
    /// `https://presence.example.com/presence.json`.
    #[serde(default)]
    pub public_url: String,

    /// Credentials. Hand-written config is the only input this program has, so
    /// they live here rather than behind a wizard.
    #[serde(default)]
    pub access_key_id: String,
    #[serde(default)]
    pub secret_access_key: String,
    /// Optional Cloudflare API token with the Cache Purge permission. When both
    /// this and `zone_id` are set the file is purged right after each write.
    #[serde(default)]
    pub purge_token: String,
    #[serde(default)]
    pub zone_id: String,
}

impl Default for SinkConfig {
    fn default() -> Self {
        Self {
            kind: default_sink_kind(),
            endpoint: String::new(),
            bucket: String::new(),
            object_key: default_object_key(),
            public_url: String::new(),
            access_key_id: String::new(),
            secret_access_key: String::new(),
            purge_token: String::new(),
            zone_id: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrivacyConfig {
    /// Default stance for apps that no rule matches.
    #[serde(default = "default_unknown_policy")]
    pub unknown_app: String,
    /// Silence for this long turns the published status into `idle`. Two minutes
    /// is the usual "away from keyboard" line.
    #[serde(default = "default_idle_after")]
    pub idle_after_secs: u64,
    /// Track titles containing any of these words are dropped entirely. A player
    /// can open anything; this is the escape hatch for private files.
    #[serde(default)]
    pub media_block_keywords: Vec<String>,
    /// Drop the `focus` block once the status turns idle, so "away" does not also
    /// publish what was left open on screen.
    #[serde(default = "default_hide_focus_when_idle")]
    pub hide_focus_when_idle: bool,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            unknown_app: default_unknown_policy(),
            idle_after_secs: default_idle_after(),
            media_block_keywords: Vec::new(),
            hide_focus_when_idle: default_hide_focus_when_idle(),
            rules: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    /// Raw executable name, e.g. `Code.exe`. The only thing ever matched, and it
    /// never leaves the machine.
    pub match_process: String,
    /// Icon URL the blog can show next to the name, e.g.
    /// `https://cdn.simpleicons.org/neteasecloudmusic`. Only the link travels; the
    /// image is served by whoever you point at.
    #[serde(default)]
    pub icon: Option<String>,
    /// Free-form tag the blog can switch on for its own text and icon. The value
    /// `media` is special — it is the only way to let an app publish a track.
    #[serde(default)]
    pub category: String,
    /// Display names keyed by language tag, e.g. `{"zh": "绝区零", "en": "Zenless
    /// Zone Zero"}`. A name is a property of the window rather than a sentence, so
    /// it travels with the sample; the wording ("playing a game") stays in the
    /// reader. Without this every new game would mean a blog rebuild.
    #[serde(default)]
    pub names: std::collections::BTreeMap<String, String>,
}

impl Rule {
    /// The id this rule publishes, derived from the executable: renaming a window
    /// stays one config edit instead of a second identifier to keep in sync.
    pub fn id(&self) -> String {
        crate::pipeline::public_id(&self.match_process)
    }

    /// True when this rule may publish what the app is playing.
    pub fn allows_media(&self) -> bool {
        self.category.eq_ignore_ascii_case("media")
    }
}

fn default_version() -> u32 {
    CONFIG_VERSION
}
fn default_heartbeat() -> u64 {
    90
}
fn default_min_gap() -> u64 {
    5
}
fn default_change_debounce() -> u64 {
    3
}
fn default_sink_kind() -> String {
    "r2".into()
}
fn default_object_key() -> String {
    "presence.json".into()
}
fn default_unknown_policy() -> String {
    "hide".into()
}
fn default_idle_after() -> u64 {
    120
}

fn default_hide_focus_when_idle() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            paused: false,
            autostart: false,
            heartbeat_secs: default_heartbeat(),
            min_gap_secs: default_min_gap(),
            change_debounce_secs: default_change_debounce(),
            proxy: None,
            device_name: None,
            dry_run: false,
            sink: SinkConfig::default(),
            privacy: PrivacyConfig::default(),
        }
    }
}

impl Config {
    /// Parse, applying defaults for anything missing. Unknown keys are ignored so
    /// a newer build's config can still be read by an older one.
    pub fn from_json(text: &str) -> Result<Self, String> {
        serde_json::from_str::<Config>(text).map_err(|e| format!("config parse error: {e}"))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }
}

/// Resolved once at startup by [`ensure_data_dir`].
///
/// Portable by design: settings and logs live next to the executable so the
/// whole install can be moved or deleted in one go, and nothing is written to a
/// system profile directory unless the application folder turns out to be
/// read-only.
static DATA_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Directory of the running executable, if we can determine it.
fn exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.parent().map(Path::to_path_buf)
}

/// Candidates in preference order. Portable locations first; the temp directory
/// is the last resort and only reached when the application folder and the
/// working directory are both read-only.
fn data_dir_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = exe_dir() {
        out.push(dir.join("data"));
        out.push(dir);
    }
    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd.join("now-doing-data"));
    }
    out.push(std::env::temp_dir().join("now-doing"));
    out
}

/// The data directory proved writable at startup, or the preferred candidate
/// before [`ensure_data_dir`] has run.
pub fn data_dir() -> PathBuf {
    match DATA_DIR.get() {
        Some(dir) => dir.clone(),
        // Before `ensure_data_dir` runs there is no answer yet; report the
        // preferred location so logging paths in early messages still look right.
        None => data_dir_candidates()
            .into_iter()
            .next()
            .unwrap_or_else(|| PathBuf::from(".")),
    }
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

pub fn log_path() -> PathBuf {
    data_dir().join("now-doing.log")
}

/// Read the config, falling back to defaults when the file is missing or corrupt.
/// A corrupt file is renamed to `config.json.bad` so the user can inspect it.
pub fn load() -> Config {
    let path = config_path();
    match fs::read_to_string(&path) {
        Ok(text) => match Config::from_json(&text) {
            Ok(cfg) => cfg,
            Err(err) => {
                log::warn!("{err}; falling back to defaults");
                let _ = fs::rename(&path, path.with_extension("json.bad"));
                Config::default()
            }
        },
        Err(_) => Config::default(),
    }
}

/// Write atomically: full write to a sibling temp file, fsync, then rename over
/// the target. On Windows `rename` over an existing file is atomic enough for our
/// purposes because the destination is replaced, not appended to.
pub fn save(cfg: &Config) -> Result<(), String> {
    let dir = data_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create {dir:?}: {e}"))?;
    let target = config_path();
    let tmp = target.with_extension("json.tmp");

    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("create {tmp:?}: {e}"))?;
        f.write_all(cfg.to_json().as_bytes())
            .map_err(|e| format!("write {tmp:?}: {e}"))?;
        f.sync_all().map_err(|e| format!("sync {tmp:?}: {e}"))?;
    }

    fs::rename(&tmp, &target).map_err(|e| format!("rename into {target:?}: {e}"))?;
    Ok(())
}

/// Persist only the two fields the tray owns, layered onto whatever is on disk
/// right now. Writing the whole in-memory copy instead would wipe a token or a
/// rule the user typed into config.json while the process was running.
pub fn save_tray_flags(paused: bool, autostart: bool) -> Result<(), String> {
    // Deliberately not `load()`: that renames an unparseable file to `.bad` and
    // hands back defaults, so one typo plus a tray click would erase the tokens.
    let text = match fs::read_to_string(config_path()) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::from("{}"),
        Err(e) => return Err(format!("read config: {e}")),
    };
    let cfg = merged_tray_flags(&text, paused, autostart)?;
    save(&cfg)
}

/// Force the two tray-owned flags onto whatever is on disk, leaving every other
/// field exactly as the user wrote it.
fn merged_tray_flags(text: &str, paused: bool, autostart: bool) -> Result<Config, String> {
    let mut cfg = Config::from_json(text).map_err(|e| format!("config: {e}"))?;
    cfg.paused = paused;
    cfg.autostart = autostart;
    Ok(cfg)
}

/// Everything in this config that will silently misbehave, phrased so one log
/// line is enough to act on. Checked once at startup and by `--probe`.
pub fn warnings(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    let heartbeat = cfg.heartbeat_secs.clamp(30, 3600);

    if cfg.heartbeat_secs != heartbeat {
        out.push(format!(
            "heartbeatSecs {} is out of range and is published as {heartbeat}",
            cfg.heartbeat_secs
        ));
    }
    if cfg.min_gap_secs >= heartbeat {
        out.push(format!(
            "minGapSecs {} >= heartbeatSecs {heartbeat}: the heartbeat can never fire",
            cfg.min_gap_secs
        ));
    }
    if cfg.change_debounce_secs > heartbeat {
        out.push(format!(
            "changeDebounceSecs {} exceeds heartbeatSecs {heartbeat}: changes only show up on the next heartbeat",
            cfg.change_debounce_secs
        ));
    }

    let policy = cfg.privacy.unknown_app.trim();
    if !policy.eq_ignore_ascii_case("hide") && !policy.eq_ignore_ascii_case("generic") {
        out.push(format!(
            "unknownApp {policy:?} is neither \"hide\" nor \"generic\"; apps are hidden"
        ));
    }
    if cfg.privacy.rules.is_empty() {
        out.push("privacy.rules is empty, so every app is hidden".into());
    }
    for (i, rule) in cfg.privacy.rules.iter().enumerate() {
        if rule.match_process.trim().is_empty() {
            out.push(format!("rules[{i}].matchProcess is empty and cannot match"));
        }
        if rule.category.trim().is_empty() {
            out.push(format!(
                "rules[{i}].category is empty, so this app can never publish a track"
            ));
        }
    }

    let has_config_keys =
        !cfg.sink.access_key_id.trim().is_empty() && !cfg.sink.secret_access_key.trim().is_empty();
    if !has_config_keys && !crate::secrets::Secrets::load().is_configured() {
        out.push("no accessKeyId/secretAccessKey in config.json and no sealed secrets.bin: nothing can be uploaded".into());
    }
    if cfg.sink.public_url.trim().is_empty() {
        out.push("publicUrl is empty: the tray link has no target and purge is skipped".into());
    }
    out
}
/// Pick the first writable data directory and remember it for the process
/// lifetime. Returns the directory that won, so `main` can name it in the log.
///
/// The last candidate is probed by an actual write, not just by `mkdir`: an
/// existing directory can still reject writes, and a directory we cannot write
/// is worse than useless for storing config.
pub fn ensure_data_dir() -> Result<PathBuf, String> {
    let mut last_err = String::from("no candidate data directory");
    for dir in data_dir_candidates() {
        match fs::create_dir_all(&dir) {
            Ok(()) => match probe_writable(&dir) {
                Ok(()) => {
                    let _ = DATA_DIR.set(dir.clone());
                    return Ok(dir);
                }
                Err(e) => last_err = format!("{dir:?} exists but is not writable: {e}"),
            },
            Err(e) => last_err = format!("{dir:?}: {e}"),
        }
    }
    Err(last_err)
}

/// Prove we can create and remove a file, which is what the config writer needs.
fn probe_writable(dir: &Path) -> std::io::Result<()> {
    let probe = dir.join(".write-probe");
    {
        let mut f = fs::File::create(&probe)?;
        f.write_all(b"ok")?;
    }
    let _ = fs::remove_file(&probe);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_fields() {
        let mut cfg = Config::default();
        cfg.heartbeat_secs = 120;
        cfg.sink.bucket = "now-doing".into();
        cfg.privacy.rules.push(Rule {
            match_process: "Code.exe".into(),
            category: "dev".into(),
            names: Default::default(),
            icon: Some("https://example.test/code.svg".into()),
        });
        let back = Config::from_json(&cfg.to_json()).unwrap();
        assert_eq!(back.heartbeat_secs, 120);
        assert_eq!(back.sink.bucket, "now-doing");
        assert_eq!(
            back.privacy.rules[0].icon.as_deref(),
            Some("https://example.test/code.svg")
        );
        assert_eq!(back.privacy.rules[0].id(), "code");
    }

    #[test]
    fn a_rule_id_is_the_executable_stem() {
        let rule = Rule {
            match_process: "Code.exe".into(),
            category: "dev".into(),
            names: Default::default(),
            icon: None,
        };
        assert_eq!(rule.id(), "code");
        assert!(!rule.id().contains(".exe"));
    }

    #[test]
    fn empty_object_yields_defaults() {
        let cfg = Config::from_json("{}").unwrap();
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert_eq!(cfg.heartbeat_secs, 90);
        assert_eq!(cfg.sink.kind, "r2");
        assert_eq!(cfg.sink.object_key, "presence.json");
        assert_eq!(cfg.privacy.unknown_app, "hide");
        assert!(!cfg.paused);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let cfg = Config::from_json(r#"{"heartbeatSecs":45,"futureField":true}"#).unwrap();
        assert_eq!(cfg.heartbeat_secs, 45);
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(Config::from_json("{not json").is_err());
    }

    #[test]
    fn a_broken_config_is_never_treated_as_an_empty_one() {
        let err = merged_tray_flags("{ not json", true, false).unwrap_err();
        assert!(err.contains("config"), "unexpected error: {err}");

        let text = r#"{"heartbeatSecs": 45, "privacy": {"unknownApp": "generic"}}"#;
        let merged = merged_tray_flags(text, true, true).unwrap();
        assert!(merged.paused && merged.autostart);
        assert_eq!(merged.heartbeat_secs, 45, "unrelated fields must survive");
        assert_eq!(merged.privacy.unknown_app, "generic");
    }

    #[test]
    fn warnings_name_the_mistakes_that_silently_break_publishing() {
        let mut cfg = Config::default();
        cfg.heartbeat_secs = 60;
        cfg.min_gap_secs = 120;
        cfg.privacy.rules = vec![Rule {
            match_process: String::new(),
            category: String::new(),
            names: Default::default(),
            icon: None,
        }];
        let found = warnings(&cfg);
        assert!(found.iter().any(|w| w.contains("minGapSecs")));
        assert!(found.iter().any(|w| w.contains("matchProcess")));
        assert!(found.iter().any(|w| w.contains("category")));

        let mut quiet = Config::default();
        quiet.heartbeat_secs = 60;
        quiet.min_gap_secs = 10;
        quiet.privacy.rules = vec![Rule {
            match_process: "Code.exe".into(),
            category: "dev".into(),
            names: Default::default(),
            icon: None,
        }];
        assert!(!warnings(&quiet).iter().any(|w| w.contains("minGapSecs")));
    }
    #[test]
    fn probe_accepts_a_writable_dir() {
        // Use a directory under the crate so the test does not depend on the
        // machine letting us write to %TEMP% (locked-down setups can deny it).
        let dir = Path::new("target").join("probe-test-dir");
        fs::create_dir_all(&dir).unwrap();
        assert!(probe_writable(&dir).is_ok());
        // The probe must not leave its scratch file behind.
        assert!(!dir.join(".write-probe").exists());
        let _ = fs::remove_dir(&dir);
    }
}
