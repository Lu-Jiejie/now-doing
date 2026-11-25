//! The publishing loop: sample → privacy → debounce → upload.
//!
//! Debounce is the whole point of this module. The snapshot's fingerprint is
//! compared against the last one uploaded; if nothing meaningful changed the loop
//! sleeps until the heartbeat deadline, so a machine sitting idle produces one
//! write per heartbeat rather than one per second.

use crate::app::{self, Shared};
use crate::collector::SharedState;
use crate::config;
use crate::pipeline::{Pipeline, Snapshot};
use crate::protocol::{Device, Presence, SCHEMA_VERSION};
use crate::secrets::Secrets;
use crate::sink::Agent;
use std::time::{Duration, Instant};

/// Longest single sleep, so the loop notices a pause request promptly.
const MAX_TICK: Duration = Duration::from_secs(1);

pub fn spawn(state: Shared, shared: SharedState) {
    let spawned = std::thread::Builder::new()
        .name("publisher".into())
        .stack_size(192 * 1024)
        .spawn(move || run(state, shared));

    if let Err(err) = spawned {
        log::error!("publisher thread failed to start: {err}");
    }
}

fn run(state: Shared, shared: SharedState) {
    let mut cfg = app::config_of(&state);
    let mut agent = Agent::new(cfg.proxy.as_deref());
    let mut device_id = crate::device::load_or_create_id();
    let mut last_fingerprint = String::new();
    let mut pending: Option<(String, Instant)> = None;
    // Wall clock, not Instant: QPC stops advancing while the machine sleeps, so an
    // Instant-based heartbeat waited a whole extra period after every resume.
    let mut last_sent: Option<i64> = None;
    let mut last_tick_wall = crate::util::unix_now();
    let mut seq: u64 = load_seq();
    let mut backoff = Duration::ZERO;
    let mut failures: u32 = 0;

    // Give the collector a tick and the media watcher its first poll, otherwise a
    // restart publishes an empty snapshot for the length of one heartbeat.
    std::thread::sleep(Duration::from_millis(3000));

    loop {
        if app::with(&state, |rt| rt.quit) {
            log::info!("publisher stopping");
            return;
        }

        // Re-read every tick for one reason: the tray mutates the config in memory
        // (pause, autostart) and persists it. Edits to config.json on disk are only
        // picked up at startup.
        let fresh = app::config_of(&state);
        if fresh.proxy != cfg.proxy {
            agent = Agent::new(fresh.proxy.as_deref());
            log::info!("proxy changed; http agent rebuilt");
        }
        if fresh.paused != cfg.paused && !fresh.paused {
            backoff = Duration::ZERO;
            failures = 0;
        }
        cfg = fresh;

        // A long gap between two ticks means the machine was asleep or the clock was
        // stepped. Publish immediately and drop the backoff: nothing about the network
        // is known after a resume. The bar is 30 s because a slow upload can hold one
        // tick for the whole request timeout without anything having slept.
        let now_wall = crate::util::unix_now();
        let gap = now_wall.saturating_sub(last_tick_wall);
        last_tick_wall = now_wall;
        let resumed = gap > 30;
        if resumed {
            log::info!("clock jumped {gap}s; publishing immediately");
            backoff = Duration::ZERO;
            failures = 0;
        }
        let forced = app::with(&state, |rt| std::mem::take(&mut rt.force_upload));

        let snapshot = sample_tick(&shared, &cfg);
        let fingerprint = snapshot.fingerprint();
        app::with(&state, |rt| {
            rt.app = snapshot.focus.as_ref().map(|f| f.app.clone());
            rt.track = snapshot.media.as_ref().map(|m| m.title.clone());
        });

        // A state that only exists for a second — an alt-tab and straight back — is
        // not news. It counts as a change only once it has held still for the
        // debounce interval.
        let debounce = Duration::from_secs(cfg.change_debounce_secs.min(300));
        let due_by_change = debounced_change(
            &fingerprint,
            &last_fingerprint,
            &mut pending,
            debounce,
            Instant::now(),
        );
        let heartbeat = cfg.heartbeat_secs.clamp(30, 3600);
        let due_by_heartbeat = last_sent
            .map(|sent| now_wall - sent >= heartbeat as i64)
            .unwrap_or(true);
        // Shortest gap between two uploads even when the state keeps changing, so a
        // window-switching spree cannot turn into a flood of writes.
        let min_gap = cfg.min_gap_secs.clamp(1, 3600) as i64;
        let ready = last_sent
            .map(|sent| now_wall - sent >= min_gap)
            .unwrap_or(true);

        if (due_by_change || due_by_heartbeat || forced || resumed) && ready && backoff.is_zero() {
            // Only commit the counter once the write is acknowledged. A failed
            // attempt then reuses the same number instead of burning one, so the
            // published sequence has no holes.
            let next_seq = seq + 1;
            let presence = build_presence(&snapshot, &cfg, next_seq, &device_id);
            let body = match serde_json::to_string(&presence) {
                Ok(body) => body,
                Err(err) => {
                    log::error!("serialise presence: {err}");
                    std::thread::sleep(MAX_TICK);
                    continue;
                }
            };

            if cfg.dry_run {
                // Dry run keeps every step that decides what would be published —
                // sampling, privacy rules, debounce — and only swaps the upload for
                // a local file, so a rule set can be checked without publishing.
                match write_preview(&body) {
                    Ok(path) => log::info!("dry run: wrote {} (seq {next_seq})", path.display()),
                    Err(err) => log::warn!("dry run: cannot write the preview: {err}"),
                }
                seq = next_seq;
                save_seq(seq);
                last_fingerprint = fingerprint;
                last_sent = Some(crate::util::unix_now());
                std::thread::sleep(MAX_TICK);
                continue;
            }

            let secrets = Secrets::load();
            // Credentials may live in config.json (plaintext, easy to hand-edit)
            // or in the DPAPI-sealed file written by `--seal`; the sealed copy wins.
            let effective = if secrets.is_configured() {
                config::SinkConfig {
                    access_key_id: secrets.access_key_id.clone(),
                    secret_access_key: secrets.secret_access_key.clone(),
                    purge_token: if secrets.purge_token.is_empty() {
                        cfg.sink.purge_token.clone()
                    } else {
                        secrets.purge_token.clone()
                    },
                    zone_id: if secrets.zone_id.is_empty() {
                        cfg.sink.zone_id.clone()
                    } else {
                        secrets.zone_id.clone()
                    },
                    ..cfg.sink.clone()
                }
            } else {
                cfg.sink.clone()
            };

            match agent.put_json(&effective, &body) {
                Ok(status) => {
                    seq = next_seq;
                    save_seq(seq);
                    last_fingerprint = fingerprint;
                    last_sent = Some(crate::util::unix_now());
                    failures = 0;
                    backoff = Duration::ZERO;
                    app::with(&state, |rt| {
                        rt.last_upload = Some(crate::util::unix_now());
                        rt.last_error = None;
                    });
                    log::debug!("published seq {seq} (http {status})");
                    if let Err(err) = agent.purge(&effective) {
                        log::warn!("edge purge failed: {err}");
                    }
                }
                Err(err) => {
                    failures = failures.saturating_add(1);
                    // A low ceiling on purpose: the old 300 s cap was longer than a
                    // heartbeat, so one blip cost three missed writes. A DNS failure
                    // says nothing about the endpoint, so it gets a quick retry
                    // before the ladder starts climbing.
                    let secs = if err.dns {
                        5
                    } else {
                        2u64.saturating_pow(failures.min(7)).min(120)
                    };
                    backoff = Duration::from_secs(if err.fatal { secs.max(60) } else { secs });
                    log::warn!(
                        "publish failed ({}): retry in {}s",
                        err.message,
                        backoff.as_secs()
                    );
                    app::with(&state, |rt| rt.last_error = Some(err.message.clone()));
                }
            }
        }

        // Re-create the device id if the data directory moved under us.
        if device_id.is_empty() {
            device_id = crate::device::load_or_create_id();
        }

        let nap = if backoff.is_zero() {
            MAX_TICK
        } else {
            let step = backoff.min(MAX_TICK);
            backoff = backoff.saturating_sub(step);
            step
        };
        std::thread::sleep(nap);
    }
}

fn sample_tick(shared: &SharedState, cfg: &config::Config) -> Snapshot {
    let raw = shared.lock().map(|g| g.clone()).unwrap_or_default();
    Pipeline::new(&cfg.privacy, cfg.paused).evaluate(&raw)
}

/// Restore the sequence counter from the previous run.
///
/// `seq` only has to increase forever, so a restart that reset it to zero would
/// make readers discard the new snapshots as stale until it caught up. When the
/// file is missing, the clock is a safe seed: it sits far above any counter a lost
/// file could have left behind, so a reader never sees the number go backwards.
fn load_seq() -> u64 {
    let path = crate::config::data_dir().join("seq");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .filter(|seq| *seq > 0)
        .unwrap_or_else(|| crate::util::unix_now().max(0) as u64)
}

fn save_seq(seq: u64) {
    let path = crate::config::data_dir().join("seq");
    let tmp = path.with_extension("tmp");
    if let Err(err) = std::fs::write(&tmp, seq.to_string()) {
        log::debug!("could not persist seq: {err}");
        return;
    }
    if let Err(err) = std::fs::rename(&tmp, &path) {
        log::debug!("could not persist seq: {err}");
    }
}

/// Write the payload a dry run would have uploaded. Deliberately not atomic: the
/// file exists to be watched by an editor, and a truncate-then-write keeps that
/// file's identity stable so the watcher is not replaced by a rename.
fn write_preview(body: &str) -> Result<std::path::PathBuf, std::io::Error> {
    let path = crate::config::data_dir().join("presence.json");
    std::fs::write(&path, body)?;
    Ok(path)
}

fn build_presence(
    snapshot: &Snapshot,
    cfg: &config::Config,
    seq: u64,
    device_id: &str,
) -> Presence {
    let now = crate::util::unix_now();
    Presence {
        schema: SCHEMA_VERSION,
        device: Device {
            id: device_id.to_string(),
            name: published_name(cfg),
            os: "windows".into(),
        },
        seq,
        status: snapshot.status,
        ts: now,
        updated_at: crate::util::rfc3339(now),
        heartbeat_s: cfg.heartbeat_secs.clamp(30, 3600),
        focus: snapshot.focus.clone(),
        media: snapshot.media.clone(),
    }
}

/// The name a reader sees. Config wins; otherwise a neutral word rather than the
/// machine name, which the user may not realise is being published.
fn published_name(cfg: &config::Config) -> String {
    match cfg.device_name.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => name.chars().take(32).collect(),
        _ => "desktop".into(),
    }
}

/// Would this state have been worth a write by now?
///
/// `pending` remembers the first time the state was seen; only a state that has
/// held still for `debounce` becomes a change, so a quick switch away and back
/// never reaches the network.
fn debounced_change(
    fingerprint: &str,
    last: &str,
    pending: &mut Option<(String, Instant)>,
    debounce: Duration,
    now: Instant,
) -> bool {
    if fingerprint == last {
        *pending = None;
        return false;
    }
    let since = match pending {
        Some((seen, since)) if seen == fingerprint => *since,
        _ => {
            *pending = Some((fingerprint.to_string(), now));
            now
        }
    };
    now.duration_since(since) >= debounce
}

/// The exact body `--watch` shows as "what would be uploaded".
pub fn preview_payload(snapshot: &Snapshot, cfg: &config::Config) -> String {
    let device_id = crate::device::load_or_create_id();
    let presence = build_presence(snapshot, cfg, load_seq() + 1, &device_id);
    serde_json::to_string_pretty(&presence).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::debounced_change;
    use std::time::{Duration, Instant};

    #[test]
    fn a_quick_switch_away_and_back_is_never_published() {
        let t0 = Instant::now();
        let mut pending = None;
        assert!(!debounced_change(
            "b",
            "a",
            &mut pending,
            Duration::from_secs(3),
            t0
        ));
        // Back to where we were before the debounce expired.
        assert!(!debounced_change(
            "a",
            "a",
            &mut pending,
            Duration::from_secs(3),
            t0 + Duration::from_secs(1)
        ));
        assert!(pending.is_none());
    }

    #[test]
    fn a_state_that_stays_becomes_a_change() {
        let t0 = Instant::now();
        let mut pending = None;
        assert!(!debounced_change(
            "b",
            "a",
            &mut pending,
            Duration::from_secs(3),
            t0
        ));
        assert!(!debounced_change(
            "b",
            "a",
            &mut pending,
            Duration::from_secs(3),
            t0 + Duration::from_secs(2)
        ));
        assert!(debounced_change(
            "b",
            "a",
            &mut pending,
            Duration::from_secs(3),
            t0 + Duration::from_secs(4)
        ));
    }

    #[test]
    fn zero_debounce_publishes_immediately() {
        let t0 = Instant::now();
        let mut pending = None;
        assert!(debounced_change("b", "a", &mut pending, Duration::ZERO, t0));
    }
}
