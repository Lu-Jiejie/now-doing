//! Minimal file logger.
//!
//! No fern/env_logger: we want ~40 lines we control, a hard size cap, and zero
//! background threads. Writes go through a `Mutex<File>` opened in append mode;
//! rotation happens lazily on write, keeping at most two files (`log` and `log.1`).

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// Rotate once the active file exceeds this. Two files of 512 KiB = ~1 MiB total.
const MAX_BYTES: u64 = 512 * 1024;

struct FileLogger {
    path: PathBuf,
    file: Mutex<Option<File>>,
    level: LevelFilter,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let mut guard = match self.file.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        // Lazily open (and reopen after rotation).
        if guard.is_none() {
            *guard = open_append(&self.path);
        }

        let line = format!(
            "{} {:<5} {}\n",
            timestamp(),
            short_level(record.level()),
            record.args()
        );

        if let Some(file) = guard.as_mut() {
            let _ = file.write_all(line.as_bytes());
            // Cheap size check; flush immediately so a crash still leaves a trail.
            if let Ok(meta) = file.metadata() {
                if meta.len() > MAX_BYTES {
                    let _ = file.flush();
                    *guard = None;
                    rotate(&self.path);
                    *guard = open_append(&self.path);
                }
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut g) = self.file.lock() {
            if let Some(f) = g.as_mut() {
                let _ = f.flush();
            }
        }
    }
}

fn open_append(path: &PathBuf) -> Option<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

fn rotate(path: &PathBuf) {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("log");
    let backup = path.with_extension(format!("{ext}.1"));
    let _ = fs::rename(path, backup);
}

fn short_level(level: Level) -> &'static str {
    match level {
        Level::Error => "ERROR",
        Level::Warn => "WARN",
        Level::Info => "INFO",
        Level::Debug => "DEBUG",
        Level::Trace => "TRACE",
    }
}

/// `YYYY-MM-DD HH:MM:SS` in UTC. We deliberately avoid a chrono dependency: the
/// log is a debugging aid, not a user-facing timestamp.
fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's `civil_from_days`, valid for the whole proleptic Gregorian
/// calendar. Avoids pulling in a date library for one format string.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Install the logger. Safe to call once at startup; later calls are no-ops
/// because `log::set_logger` rejects a second registration.
pub fn init(path: PathBuf, level: LevelFilter) {
    // `set_boxed_logger` needs the `std` feature; we use a `'static` static box
    // instead so the logger also works if that feature gets turned off later.
    static LOGGER: std::sync::OnceLock<FileLogger> = std::sync::OnceLock::new();
    let logger = LOGGER.get_or_init(|| FileLogger {
        path,
        file: Mutex::new(None),
        level,
    });
    let _ = log::set_logger(logger);
    log::set_max_level(level);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_1970_01_01() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn known_date_matches() {
        // 2024-02-29 is day 19782 since the epoch.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }
}
