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
cargo tauri build               # release bundle (icons included)
```

Scaffolded with SvelteKit 3 / Svelte 5 / Vite 8 (Kit ≥3 config lives in
`vite.config.js` under the `sveltekit()` plugin — `svelte.config.js` is no
longer read).

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
- **Status view** (rudimentary — the full dashboard is card t_e4479772):
  phase pill, coordinator online/down (20 s health poll + manual "Test"),
  session counters (completed / verified / failed / bytes staged) from the
  lib `StatsHandle` polled every 2 s, and per-slot lease rows: activity +
  chunk type + target key.
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
- No elapsed-time-per-lease, multi-session totals, or community totals —
  that's the dashboard card (t_e4479772).
- No auto-start on launch (settings/register first), no OS autostart,
  no updater.

## Credentials & data location

All app state lands in Tauri's app data dir (`identifier`
`app.landhunter.contributor-gui`):

- Linux: `~/.local/share/app.landhunter.contributor-gui/`
  - `credentials.json` — `client_id` + `auth_token`, **0600** (written by
    the lib's `save_credentials`)
  - `settings.json` — GUI settings, 0600
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
- `cargo build` green against `lh-contributor-client` c3729ac; tray/window
  runtime behavior not exercised in a GUI session on this box yet (binary
  links; headless CI box).
- House rules: local repo only (no remote), `commit.gpgsign false`
  repo-local, unsigned commits, `lh-contributor-client` is read-only here.
