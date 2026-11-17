# lh-contributor-gui

Tauri 2 graphical wrapper for the LandHunter contributor client
(`lh-contributor-client`, embedded as a path dependency at
`../lh-contributor-client`). Background-posture tray app: the window is a
status dashboard; the warrior run loop lives in a background task and keeps
running after the window is closed.

## Dev

```sh
npm install                     # frontend deps
npm run tauri dev               # dev: kit dev server @1420 + hot-reload app
npm run build                   # frontend → build/ (SPA via adapter-static)
cargo build                     # backend (run from src-tauri/)
cargo test                      # unit tests (runtime accumulation, lease mapping)
cargo tauri build               # release bundle (icons included)

# Verification harness (no-GUI render proof; jsdom is deliberately NOT a
# repo dependency — install ad-hoc with --no-save):
npm install --no-save --no-package-lock jsdom
node .harness/compile-page.mjs             # compile +page.svelte → client JS
node --conditions=browser .harness/render.mjs  # mount in jsdom; asserts live
                                             # updates, lease timers, all lines
# Visual variant: after npm run build,
#   npm run preview -- --host 0.0.0.0 --port 1423   # restart preview after EVERY build
#   chromium --headless=new --no-sandbox --disable-dev-shm-usage --window-size=360,480 \
#     --virtual-time-budget=16000 --screenshot=out.png http://127.0.0.1:1423/
```

Scaffolded with SvelteKit 3 / Svelte 5 / Vite 8 (Kit ≥3 config lives in
`vite.config.js` under the `sveltekit()` plugin — `svelte.config.js` is no
longer read). Kit 3 **removed the built-in `$lib` alias**: internal imports
use package.json subpath imports instead (`"imports": { "#lib/*":
"./src/lib/*" }` → `import … from '#lib/demo.js'`).

## What's wired (vs stubbed)

Wired:

- **Tray** (`src-tauri/src/main.rs`): Show / Pause⇄Resume / Quit menu; the
  Pause item's label flips at runtime with the run-loop phase.
- **Minimize-to-tray**: the close button hides the window
  (`prevent_close` + `hide`); the app and an active loop keep running. Quit
  is only via tray or the Quit button.
- **Register flow**: `register_client` → lib's `register_request()` →
  `POST /v1/register` → `save_credentials()` writes
  `credentials.json` (0600 on unix) into the app data dir. A `Runner` with
  no saved credentials also auto-registers (lib behavior), so the explicit
  button is convenience, not requirement.
- **Run / pause / resume**: `start_contributor` builds a lib `Config` from
  settings + env and spawns `Runner::run` on Tauri's tokio runtime with the
  lib's external `shutdown_rx` channel. The lib has no pause primitive, so
  **pause == graceful stop**: the watch channel fires, slots stop leasing,
  heartbeats stop, in-flight chunks settle and report, `run()` returns.
  **Resume builds a fresh `Runner` reusing the persisted identity** — no
  re-register. Quit does the same graceful stop plus a 2.5 s grace window
  before exit.
- **Dry-run parity**: the checkbox persists `dry_run` in `settings.json`
  and flows into `RunOptions.dry_run` at the next start. Dry runs stage
  blobs to `<data_dir>/staging/` and write result manifests to
  `<data_dir>/manifests/` without submitting anything (lib behavior).
- **Dashboard** (`src/routes/+page.svelte`, card t_e4479772): phase pill
  (`idle`/`running`/`paused`/…) plus a `Leased — <chunk type> · <target key>`
  summary and per-slot lease rows carrying an elapsed time per lease (mm:ss,
  measured client-side keyed on first-seen `chunk_id`, ± poll interval;
  verifying→executing→submitting share one timer), headline tiles for
  completed / verified / failed / bytes-staged, session uptime, total runtime
  across sessions, coordinator pill with the last-successful-poll timestamp,
  a community-total line, dry-run toggle, Start/Pause/Resume, Register, Quit.
  Stats are polled every 2 s via `get_status` (async invoke — the UI thread
  never blocks); lease timers tick on a 1 s wall-clock anchor.
- **Community totals** (`community_stats` command): the Rust side fetches the
  tracker's `/public/stats.json` (the webview can't — the tracker sends no
  CORS headers), caches it for 60 s (15 s backoff after failure), serves
  stale on failure, and returns null otherwise — the UI line simply hides
  ("degrade silently").
- **Total runtime across sessions**: the lib's uptime is per-`Runner`, so the
  GUI accumulates wall time while a `Runner` is unfinished (1 s task,
  monotonic-uptime deltas with sanity caps) and persists it to `runtime.json`
  (0600) every 30 ticks, on pause, at run completion, and on quit. A finished
  Runner's snapshot has its final uptime frozen so a paused session's
  "session uptime" display stops instead of ticking forever.
- **Lease targets**: `StatsHandle` exposes `{chunk_type, chunk_id}` per slot
  but not the contract's `target_key`, and this app must not modify the
  client lib. So a 2 s enrichment task resolves `chunk_id → target_key`
  through the public `GET /v1/chunk/:id/contract` endpoint (`ApiClient::
  fetch_contract`), cached with bounded retries; on lookup failure the row
  degrades to the chunk id.
- **Settings**: coordinator URL, contract public key, concurrency (1–8),
  dry-run — persisted to `settings.json` (0600). Public key may alternatively
  come from `LH_PUBLIC_KEY_HEX`.

Stubbed / not yet here:

- R2 / presigned-upload staging config is **env-only**, not settings UI
  (see env table). Real (non-dry) runs without it refuse to start with the
  lib's own error surfaced in the banner.
- No auto-start on launch (settings/register first), no OS autostart,
  no updater.

## Credentials & data location

All app state lands in Tauri's app data dir (`identifier`
`app.landhunter.contributor-gui`):

- Linux: `~/.local/share/app.landhunter.contributor-gui/`
  - `credentials.json` — `client_id` + `auth_token`, **0600** (written by
    the lib's `save_credentials`)
  - `settings.json` — GUI settings, 0600
  - `runtime.json` — 0600, cross-session runtime total seconds
    (`{ "total_runtime_secs": … }`, accumulated by the dashboard card)
  - `staging/` — dry-run blob staging
  - `manifests/` — dry-run result manifests

The actual data dir path is displayed in the app under
*Settings & identity → credentials*.

## Environment variables

| Var | Meaning | Default |
| --- | --- | --- |
| `LH_PUBLIC_KEY_HEX` | ed25519 contract key, 64 hex chars (fallback when Settings key empty) | — |
| `LH_COORDINATOR_URL` | coordinator base URL (first-run seed only) | `https://cap.landhunter.app` |
| `LH_CONCURRENCY` | worker slots 1–8 (first-run seed) | `2` |
| `LH_GUI_DRY_RUN` | first-run dry-run default (`0`/`false` disables) | `true` |
| `LH_BRAVE_API_KEY` | search-chunk backend key, passed through to the lib | — |
| `LH_UPLOAD_PRESIGN_TEMPLATE` | presigned upload URL template containing `{key}` | — |
| `LH_R2_BUCKET` / `LH_R2_ACCESS_KEY_ID` / `LH_R2_SECRET_ACCESS_KEY` + `LH_R2_ENDPOINT` or `LH_R2_ACCOUNT_ID` | direct R2 staging for real runs | local (dry-run only) |

Notes:

- `LH_COORDINATOR_URL` / `LH_CONCURRENCY` / `LH_GUI_DRY_RUN` are
  **first-launch seeds**: after `settings.json` exists, the Settings UI is
  the source of truth. `LH_PUBLIC_KEY_HEX` remains a live fallback while the
  stored key is empty. Staging vars (`LH_R2_*`, presign) are read live at
  each start.
- Settings changes (dry-run, concurrency, coordinator URL) take effect on
  the next start — pause/resume applies them.

## Verification state

- `npm run build` green (Kit 3 + adapter-static SPA in `build/`).
- `cargo build` green against `lh-contributor-client` c3729ac (binary links;
  248 MB debug at `src-tauri/target/debug/lh-contributor-gui`).
- `npx svelte-check --threshold warning`: 0 errors / 0 warnings (a11y
  included). `cargo test`: 3/3 (runtime-delta policy, lease target mapping,
  runtime.json round-trip). `cargo clippy --all-targets`: 0 warnings.
- Dashboard rendering verified headlessly: headless-chromium at 360×480 shows
  header, activity + lease rows (elapsed timers), the four tiles and the
  runtime rows with values ticking between polls (bytes staged 820 KB →
  1.1 MB across dumps); `.harness/render.mjs` (jsdom, real component)
  asserts live updates + every required line against the mock run loop
  ("demo mode"). The real lib `StatsHandle` path (via `get_status`) shares
  those same rendering paths, but an actual Tauri window/tray session has
  still not been exercised on this headless box.
- House rules: local repo only (no remote), `commit.gpgsign false`
  repo-local, unsigned commits, `lh-contributor-client` is read-only here.
