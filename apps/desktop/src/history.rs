//! Bounded, append-only record of what was actually published.
//!
//! One line per write, holding the payload exactly as a reader received it. The
//! object store only ever holds the newest snapshot, so without this file there is
//! no way to look at a session afterwards — only to sit in front of `--watch` and
//! hope the interesting transition happens again.
//!
//! The file is capped: once it passes `MAX_BYTES` the older half is dropped, so a
//! long debug run cannot slowly fill a disk in a tool that is meant to be
//! invisible.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::protocol::Presence;

/// Trim once the file passes this size. A line is a few hundred bytes, so this
/// holds a few hundred snapshots — hours, at a one-minute heartbeat.
const MAX_BYTES: u64 = 256 * 1024;

/// Lines kept when trimming happens.
const KEEP_LINES: usize = 120;

pub fn path() -> PathBuf {
    crate::config::data_dir().join("history.jsonl")
}

/// Append one published payload.
///
/// Callers log and ignore a failure: this is a debugging aid and must never
/// disturb the publishing loop.
pub fn record(body: &str) -> std::io::Result<()> {
    record_at(&path(), body)
}

fn record_at(path: &Path, body: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(body.trim().as_bytes())?;
    f.write_all(b"\n")?;
    f.flush()?;
    drop(f);
    trim(path);
    Ok(())
}

/// Drop the older half once the file is too big, instead of rewriting on every
/// append.
fn trim(path: &Path) {
    let too_big = std::fs::metadata(path)
        .map(|meta| meta.len() > MAX_BYTES)
        .unwrap_or(false);
    if !too_big {
        return;
    }

    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.len() <= 1 {
        return;
    }

    // Halve as well as cap: a handful of huge lines could otherwise stay above the
    // limit for good and trim on every single append.
    let keep_from = lines.len() - KEEP_LINES.min(lines.len() / 2).max(1);
    let keep = &lines[keep_from..];
    let mut out = String::new();
    for line in keep {
        out.push_str(line);
        out.push('\n');
    }

    let tmp = path.with_extension("jsonl.tmp");
    if std::fs::write(&tmp, out).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Raw lines, oldest first, at most `limit`.
pub fn tail(limit: usize) -> Vec<String> {
    tail_at(&path(), limit)
}

fn tail_at(path: &Path, limit: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<String> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect();
    let start = lines.len().saturating_sub(limit);
    lines[start..].to_vec()
}

/// How many lines the file holds.
pub fn count() -> usize {
    count_at(&path())
}

fn count_at(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0)
}

/// Parsed snapshots, oldest first. A line that no longer parses (an older schema,
/// a hand edit) is skipped rather than failing the whole view.
pub fn load(limit: usize) -> Vec<Presence> {
    tail(limit)
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Device, Status};
    use std::collections::BTreeMap;

    fn sample(seq: u64) -> String {
        let presence = Presence {
            schema: 1,
            device: Device {
                id: "d".into(),
                name: "desktop".into(),
                os: "windows".into(),
            },
            seq,
            status: Status::Active,
            ts: 1_700_000_000 + seq as i64,
            updated_at: crate::util::rfc3339(1_700_000_000 + seq as i64),
            heartbeat_s: 60,
            focus: Some(crate::protocol::Focus {
                app: "zzz".into(),
                category: "game".into(),
                names: BTreeMap::new(),
                icon: None,
                since: 1_700_000_000,
                seconds: seq,
            }),
            media: None,
        };
        serde_json::to_string(&presence).unwrap()
    }

    /// Beside the build output, not in `%TEMP%`: this checkout carries a Low
    /// integrity label, and a test binary that inherits it is refused by the
    /// Medium-level temp directory before the code under test even runs.
    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("history-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("history.jsonl")
    }

    #[test]
    fn a_recorded_payload_reads_back_verbatim() {
        let path = scratch("roundtrip");
        let body = sample(1);
        record_at(&path, &body).unwrap();
        assert_eq!(tail_at(&path, 10), vec![body.clone()]);
        let parsed: Presence = serde_json::from_str(&tail_at(&path, 1)[0]).unwrap();
        assert_eq!(parsed.seq, 1);
        assert_eq!(parsed.focus.unwrap().app, "zzz");
    }

    #[test]
    fn tail_keeps_only_the_newest_lines_in_order() {
        let path = scratch("tail");
        for seq in 1..=5 {
            record_at(&path, &sample(seq)).unwrap();
        }
        assert_eq!(count_at(&path), 5);
        let newest = tail_at(&path, 2);
        assert_eq!(newest.len(), 2);
        assert!(newest[1].contains("\"seq\":5"));
    }

    #[test]
    fn a_blank_line_is_not_an_entry() {
        let path = scratch("blank");
        record_at(&path, &sample(1)).unwrap();
        record_at(&path, "   ").unwrap();
        assert_eq!(count_at(&path), 1);
    }

    #[test]
    fn an_oversized_file_is_trimmed_to_the_newest_lines() {
        let path = scratch("trim");
        // 4 KB per line: ~80 lines crosses the 256 KB cap.
        let filler = "x".repeat(4096);
        for seq in 1..=80 {
            record_at(&path, &format!("{{\"seq\":{seq},\"pad\":\"{filler}\"}}")).unwrap();
        }
        let kept = count_at(&path);
        assert!(kept <= KEEP_LINES, "kept {kept} lines");
        // The newest line must survive the rewrite.
        assert!(tail_at(&path, 1)[0].contains("\"seq\":80"));
        assert!(std::fs::metadata(&path).unwrap().len() <= MAX_BYTES);
    }
}
