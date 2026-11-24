//! Persistent configuration, stored as JSON under `%APPDATA%\now-doing\config.json`.
//!
//! M0 only needs the skeleton: theme of the app, autostart toggle, pause flag and
//! the (not yet used) R2 credentials block. Everything is written atomically so a
//! crash mid-write can never leave a truncated file behind.

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
}

impl Default for SinkConfig {
    fn default() -> Self {
        Self {
            kind: default_sink_kind(),
            endpoint: String::new(),
            bucket: String::new(),
            object_key: default_object_key(),
            public_url: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrivacyConfig {
    /// Default stance for apps that no rule matches.
    #[serde(default = "default_unknown_policy")]
    pub unknown_app: String,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            unknown_app: default_unknown_policy(),
            rules: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    /// Raw executable name, e.g. `Code.exe`.
    pub match_process: String,
    /// Public label shown on the blog. When empty the app is hidden.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub category: String,
    /// When true the label is only used if a window title is also allowed.
    #[serde(default)]
    pub include_title: bool,
}

fn default_version() -> u32 {
    CONFIG_VERSION
}
fn default_heartbeat() -> u64 {
    90
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

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            paused: false,
            autostart: false,
            heartbeat_secs: default_heartbeat(),
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
/// We cannot hardcode `%APPDATA%`: on locked-down machines, roaming profiles on
/// network shares, or when an antivirus holds the folder, `%APPDATA%` writes can
/// be denied. The brief says the app must never fail loudly, so instead of
/// quitting we fall through a list of candidates and remember whichever worked.
static DATA_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Candidates in preference order. `%APPDATA%` is the right home for settings
/// because it roams with the profile; `%LOCALAPPDATA%` and the temp directory
/// are progressively less ideal but always writable.
fn data_dir_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for var in ["APPDATA", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            out.push(PathBuf::from(base).join("now-doing"));
        }
    }
    out.push(std::env::temp_dir().join("now-doing"));
    out
}

/// `%APPDATA%\now-doing`, or the fallback that [`ensure_data_dir`] proved writable.
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
            label: "写代码".into(),
            category: "coding".into(),
            include_title: false,
        });
        let back = Config::from_json(&cfg.to_json()).unwrap();
        assert_eq!(back.heartbeat_secs, 120);
        assert_eq!(back.sink.bucket, "now-doing");
        assert_eq!(back.privacy.rules[0].label, "写代码");
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
