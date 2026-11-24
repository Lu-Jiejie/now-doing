# now-doing

A tiny Windows tray agent that reports what you are doing right now — the focused app and the song you are playing — to a JSON file your blog can `fetch`. No backend server, no database, no account.

```
your PC  ──PUT──▶  Cloudflare R2  ◀──CF edge cache (10 s)──  blog visitor
 (tray)              presence.json                              (fetch)
```

- **Resident cost:** ~15 MB working set, ~2.7 MB private bytes, 0 % CPU idle, **no child processes**.
- **Freshness:** 0–10 s end to end (measured 11.0 s worst case), no polling from the browser beyond one `fetch` per visit.
- **Read cost:** visitors hit the Cloudflare edge cache; R2 is only contacted on a cache miss.
- **Privacy:** default-deny. A process shows up on your blog only after you write a rule for it.

---

## Why this design

### The measurement that decided the stack

Tauri is often sold as "Electron but lighter". On Windows that is only true for disk. An [independent measurement of an empty window](https://lleg.dev/posts/electron-vs-tauri-empty-window-memory/) found Tauri 2.12 (WebView2) at **108.0 MB** private working set versus Electron 45 at **103.3 MB** — Tauri was slightly *worse*, because WebView2 runs the same Chromium process tree (browser, GPU, renderer, utility, crash handler). What Tauri actually saves is the ~357 MB of bundled Chromium on disk.

So the architectural rule of this project is not "Rust is cheap". It is:

> **The resident process never creates a webview window.**

`tauri.conf.json` declares `"windows": []`. At startup nothing but the Rust host, a tray icon and a kernel mutex exist. The settings window is built on demand with `WebviewWindowBuilder`, and closing it calls `destroy()` — not `hide()`, which would leave a ~100 MB renderer resident forever.

Measured on the reference machine (release build, 12-core):

| State | Working set | Private bytes | Threads | Child processes |
| --- | --- | --- | --- | --- |
| Resident, tray only | **14.8 MB** | **2.7 MB** | ~6 | **0** |
| Settings window open | 26–40 MB | 4.7–11.7 MB | 22–36 | 1 (`msedgewebview2.exe`, ~120 MB WS) |
| After closing settings | 26.4 MB → settles, private back to **4.6 MB** | | 22 | **0** |

Idle CPU over a 20 s sample: `delta = 0 ms` — below the timer resolution, i.e. statistically indistinguishable from zero.

### Why R2 instead of a GitHub Gist

The original plan was a secret Gist. It was tested and rejected:

- Gist raw URLs (`gist.githubusercontent.com/.../raw/presence.json`) are served through Fastly with `cache-control: max-age=300`. In 8 rounds of write-then-poll, **0/8** returned the new value within 25 s.
- Appending `?v=<bucket>` made it a coin flip (4/8 within 5 s, 4/8 still stale after 25 s) because Fastly is tiered: a POP miss can be answered by an upper-tier shield holding the old object.
- The only reliably fresh address is the commit-SHA-pinned raw URL (0.25–0.43 s, content-addressed and therefore cacheable forever) — but only the publisher knows the latest SHA. Bridging that requires a relay, i.e. exactly the backend we wanted to avoid.
- The same test against a normal Git repo gave identical numbers: this is a property of `raw.githubusercontent.com`, not of Gists.
- `api.github.com/gists/{id}` answers in ~0.6 s but is limited to **60 requests/hour/IP unauthenticated**, which a public blog would blow through immediately.

R2 has none of these problems:

- **Strongly consistent.** A `PUT` is immediately visible to a following `GET` (verified: write `seq=999`, read back `999` 288 ms later). No replica lag to design around.
- **S3-compatible**, so signing is straightforward and no Cloudflare-specific SDK is needed.
- **Free tier is per-account and generous:** 10 GB storage, 1 M Class A (write) ops, 10 M Class B (read) ops, free egress, monthly. Our write volume is ~1,210 ops/day ≈ 3.6 % of the monthly Class A allowance.
- **Over-quota is a slope, not a cliff.** Beyond the free tier you pay $0.36 per million reads. Unlike Netlify's credit cap — which suspends *the entire site* when exhausted — R2 keeps serving.

### Why the edge cache, and why the browser does not fight it

`.json` is not in Cloudflare's [default cacheable extension list](https://developers.cloudflare.com/cache/concepts/default-cache-behavior/), so out of the box every request hits R2. A Cache Rule makes the object edge-cacheable with a short TTL. Measured with a 10 s TTL:

- Cache `MISS` → origin: ~1,209 ms; `HIT` → 258–284 ms.
- Write at `t`, then poll every second: the new value appeared after **10,998 ms**, served from an `EXPIRED` entry.
- Concurrent visitors within one TTL window collapse into a single origin read (`Request collapsing`), so Class B usage tracks *cache windows*, not page views.

A 10 s TTL is the honest default. Pushing to 0–1 s means calling the Cloudflare purge API after every write; that is a later optional optimisation, not a requirement.

---

## Repository layout

```
apps/desktop/
  src-tauri/
    Cargo.toml
    tauri.conf.json          # "windows": [] — no window at startup
    src/
      main.rs                # startup order, ExitRequested guard
      config.rs              # JSON config, atomic save, writable-dir probe
      logger.rs              # rotating file logger, 512 KB cap
      tray.rs                # tray icon, menu, lazy settings window
      autostart.rs           # HKCU\...\Run reconcile
      single_instance.rs     # named mutex, silently exits on second launch
    icons/
  dist/settings.html         # placeholder UI for M0
```

Later milestones add `collector/` (focus, media, idle), `pipeline/` (privacy + fingerprint), `uploader/` (SigV4 R2 PUT) and the real Vue settings UI.

---

## Data protocol (v1)

`presence.json` is a single current-state snapshot, not a timeline. Every write replaces the previous one.

```json
{
  "schema": 1,
  "device": { "id": "…", "name": "…", "os": "windows" },
  "seq": 1234,
  "status": "active",
  "ts": 1794967200,
  "heartbeatS": 90,
  "focus": { "app": "vscode", "label": "写代码", "since": 1794966800, "seconds": 400 },
  "media": { "state": "playing", "title": "…", "artist": "…", "app": "网易云音乐" }
}
```

Design notes:

- **There is no `offline` state.** A client that is offline cannot write. The blog infers staleness from `ts`.
- **`seq` is monotonic** and lets the reader discard an older snapshot that a stale cache handed it.
- **The reader must correct for clock skew.** The client's clock is not your server's clock; compare `now - skew - ts` where `skew` is derived from `Date` on the first successful response.
- **`focus.app` is never a raw process name**, only the public id a rule resolved to. Raw titles and executable paths never leave the machine.
- All nullable fields must be written explicitly as `null`; serde's `Option` skipping would break consumers.

---

## Cloudflare setup

One-time, all on the free plan.

1. **R2 bucket** — create `now-doing`. Note the **Account ID** (`dash.cloudflare.com/<account-id>/r2/...`).
2. **S3 credentials** — R2 → *Manage API Tokens* → create an **Object Read & Write** token scoped to this bucket. Save:
   - Access Key ID (this is the token's `id`)
   - Secret Access Key (the SHA-256 of the token `value`, shown once)
   - endpoint: `https://<account-id>.r2.cloudflarestorage.com`
3. **Custom domain** — R2 → bucket → Settings → Public access → *Custom Domains* → connect e.g. `presence.example.com`. Do **not** use the `r2.dev` subdomain in production: it is rate-limited, cannot be cached, and has no WAF.
4. **Cache Rule** — zone → Caching → Cache Rules → *Eligible for cache* for `http.request.uri.path eq "/presence.json"`.
   - Edge TTL: override, 10 s.
   - ⚠️ **Never add `http.request.method eq "GET"` to the expression.** Cloudflare's single-file purge issues its request with a different method; a GET-only rule makes purge silently do nothing. Match on host and path only.
5. **CORS** — bucket → Settings → CORS Policy:

   ```json
   [{ "AllowedOrigins": ["https://yourblog.com"], "AllowedMethods": ["GET", "HEAD"], "MaxAgeSeconds": 86400 }]
   ```

   `AllowedOrigins` entries must be `scheme://host[:port]` with no path and no trailing slash. A wildcard `https://*.yourblog.com` does **not** match the bare domain. Purge the hostname after changing CORS, otherwise already-cached responses keep the old (missing) headers.
6. *(optional, for 0–1 s freshness)* a second API token with **Zone → Cache Purge** at `My Profile → API Tokens`. Note this is the **user-level** page; the account-level token page has no `Cache Purge` permission at all, which is a common dead end.

### Verifying it works

```bash
curl -sD- -H 'Origin: https://yourblog.com' https://presence.example.com/presence.json | head -20
```

Expect `200`, an `access-control-allow-origin` echoing your origin, and `cf-cache-status: MISS` on the first call then `HIT`.

---

## Build

Requires Rust ≥ 1.77 and — on first build — network access for crates.

```powershell
cd apps/desktop/src-tauri
cargo build --release      # → target/release/now-doing.exe, ~3.4 MB
cargo test                 # 11 unit tests
```

The release profile is tuned for size: `opt-level = "s"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`, `strip = true`.

> **Note for this machine:** `cargo` must be run from PowerShell, not from Git Bash — under msys the schannel backend fails with `SEC_E_NO_CREDENTIALS (0x8009030e)` on every HTTPS crate download.

### Development helper

`NOW_DOING_DEV_OPEN_SETTINGS=1` opens the settings window at startup so its memory cost can be measured without clicking the tray. Unset in normal use, so a release launch still creates zero windows.

---

## Configuration file

`%APPDATA%\now-doing\config.json` (falls back to `%LOCALAPPDATA%`, then `%TEMP%`, if the directory is not writable — the app never refuses to start because of a folder permission).

```json
{
  "version": 1,
  "paused": false,
  "autostart": false,
  "heartbeatSecs": 90,
  "sink": {
    "kind": "r2",
    "endpoint": "https://<account-id>.r2.cloudflarestorage.com",
    "bucket": "now-doing",
    "objectKey": "presence.json",
    "publicUrl": "https://presence.example.com/presence.json"
  },
  "privacy": {
    "unknownApp": "hide",
    "rules": []
  }
}
```

Saves are atomic: write `config.json.tmp`, `sync_all`, then rename. A corrupt file is renamed to `config.json.bad` and defaults take over.

Logs live beside it in `now-doing.log`, rotating to `.log.1` at 512 KB.

---

## Roadmap

| Milestone | Scope |
| --- | --- |
| **M0** ✅ | Tray, single instance, autostart, atomic config, logger, lazy settings window |
| **M1** | Focus + media collection (`GetForegroundWindow`, GSMTC events), idle detection, tray tooltip |
| **M2** | Privacy pipeline: rules, aliases, title sanitising, fingerprint debounce |
| **M3** | R2 uploader: SigV4 PUT, heartbeat, exponential backoff, silent failure |
| **M4** | Settings UI: credentials, rules table, live raw-process list, connectivity test |
| **M5** | Blog side: `usePresence()` + `<PresenceCard>` with four states |
| **M6** | Packaging, memory/CPU verification, fullscreen downclocking |

### Collection notes

- **Focus** — `GetForegroundWindow` → `GetWindowThreadProcessId` → `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` → `QueryFullProcessImageNameW`. Microseconds, no enumeration of all processes.
- **Media** — `GlobalSystemMediaTransportControlsSessionManager`, event-driven via `current_session_changed` / `media_properties_changed` / `playback_info_changed`. Zero polling.
- **Fallback** — if no SMTC session exists but the focused window belongs to a known player, parse `title - artist` from the window caption. If that fails, publish `media: null`.
- **Never** install a global hook (`SetWindowsHookEx`), enumerate all processes, or query WMI. Those are the three things that make "low-resource" tray apps show up in Task Manager and trip antivirus heuristics.

---

## Licence

MIT.
