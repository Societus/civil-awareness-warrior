# Civil Awareness Warrior

Tauri 2 graphical wrapper for the Civil Awareness Warrior volunteer
client (the `lh-contributor-client` SDK crate, embedded as a path
dependency at `../../sdk` — this directory is the `gui/` subtree of the
monorepo). Background-posture tray app: the window is a
status dashboard; the warrior run loop lives in a background task and keeps
running after the window is closed.

## Installing (volunteers)

Built installers live on this repository's **Releases** page (the release
pipeline is described under [Releasing (operator)](#releasing-operator)).
Pick your platform:

- **Windows 10/11 (x64):** `civil-awareness-warrior_<version>_x64-setup.exe`
  — an NSIS installer that installs *for your user only* (no admin
  rights, no system-wide footprint). Portable alternative:
  `civil-awareness-warrior-portable.zip` is the bare exe — unpack anywhere and
  run; Windows 10+ already ships the WebView2 runtime the app needs.
- **macOS 13+ (Apple Silicon or Intel):** the universal `.dmg` — open it
  and drag **Civil Awareness Warrior.app** into Applications.
- **Linux (x86_64):** the `.deb` (Debian/Ubuntu-family:
  `sudo apt install ./civil-awareness-warrior_<version>_amd64.deb`) or the
  `.AppImage` (any modern distro: `chmod +x` and run; the first run adds
  an applications-menu entry).

Whichever you pick: this is a **tray app**. Closing the window hides it —
the contributor stays reachable from the system tray, and quitting is via
the tray menu (or the dashboard's Quit button). Nothing runs until you
press Start (dry-run mode is the default).

Releases are currently **unsigned** (signing/notarization is a documented
operator follow-up): expect Gatekeeper's right-click → Open prompt the
first time on macOS, and SmartScreen's "More info → Run anyway" on
Windows, until release artifacts carry signatures.

## Requirements (volunteers)

Same numbers as the coordinator landing page's Requirements section
(cap.landhunter.app — the two pages are deliberately kept mirrored), plus
GUI-specific extras:

- **Minimum:** 64-bit CPU (x86_64 or ARM64) · 512 MB available RAM ·
  1 GB free disk for staging · outbound HTTPS (443) · Linux /
  macOS 13+ (the GUI floor; the coordinator's CLI floor is lower) /
  Windows 10+.
- **Recommended:** 2+ CPU cores · 2 GB+ RAM · 5 GB+ free disk · unmetered
  or high-capacity internet · always-on or scheduled uptime.
- **Network:** no inbound ports · outbound only, to the coordinator and
  chunk target hosts · politeness cap ≤30 req/min per target host · no
  VPN/proxy needed (residential IPs preferred for some GIS lookups).
- **GUI extras — only if you opt into Local Model grading:** a
  llama-server binary (from llama.cpp) on PATH or set explicitly in
  Settings ▸ Local Model; the default pinned model downloads 1.1 GB, the
  opt-in 9B model 5.8 GB. Rough 9B floor: ≥10 GiB dedicated GPU VRAM or
  ≥24 GB unified memory. Settings ▸ Local Model ▸ **"Can I AI?"** reads
  your RAM/VRAM when you click it (read-only, nothing stored or sent)
  and tells you which model tier your machine can host.

## Autostart (launch at login)

**Default: off.** The only switch is Settings ▸ System ▸ "Launch at
login" — deliberately not env-seedable, because registering a login entry
is a human action. When on, the app registers a **per-user** entry:

| Platform | Where the entry lives |
| --- | --- |
| Windows | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` value `civil-awareness-warrior` |
| Linux | `~/.config/autostart/civil-awareness-warrior.desktop` (XDG autostart; the AppImage registers the AppImage itself, the deb package registers the installed binary) |
| macOS | LaunchAgent plist `~/Library/LaunchAgents/civil-awareness-warrior.plist` (label `civil-awareness-warrior`) |

How it stays honest:

- Flipping the checkbox applies the OS entry **before** settings.json is
  written; if the OS write fails the save aborts, so settings.json never
  claims an autostart state the OS does not have.
- Every launch re-applies the persisted intent and **re-points the entry
  at the binary actually running**: a hand-deleted entry re-registers on
  the next launch, and a stale entry from an old install path (or a dev
  build) is corrected rather than left to rot.
- Autostart starts the **tray app, idle**. Starting an actual contributor
  run remains a human Start press; the schedule watcher only pauses /
  resumes a loop that is already live in the current session.

### macOS notes (LaunchAgent / LaunchDaemon patterns)

The in-app switch writes a LaunchAgent: per-user, starting at *GUI
login*. Known caveats:

- A LaunchAgent lives in the user's Aqua (GUI) session. Recent macOS
  (26+) refuses `launchctl bootstrap gui/<uid>` from a plain ssh session
  — load from a Terminal **inside a logged-in console session**, or just
  use the in-app checkbox (the app self-registering from inside its own
  GUI session is exactly the supported context).
- A root LaunchDaemon (`/Library/LaunchDaemons`, starts at boot with no
  login) can start the binary on a headless Mac, but Tauri windows and
  tray icons need a window-server connection: expect the GUI layer to
  fail or run invisibly without a logged-in session. Test before trusting
  it — the reliable headless-Mac patterns are console auto-login +
  LaunchAgent, or running the contributor **CLI** instead of the GUI.

Manual patterns (from a console Terminal; the in-app switch effectively
does the same):

```sh
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/civil-awareness-warrior.plist
launchctl print   gui/$(id -u)/civil-awareness-warrior  # status
launchctl bootout gui/$(id -u)/civil-awareness-warrior  # unload
```

LaunchDaemon variant: copy the plist the app wrote into
`/Library/LaunchDaemons/`, add a `UserName` key pinned to the intended
account, then `launchctl bootstrap system /Library/LaunchDaemons/civil-awareness-warrior.plist`
— with the GUI caveat above.

## Upgrading

There is **no in-app updater** on purpose (no update feed yet). Upgrade =
install the newest release over the old:

- Windows: run the new `-setup.exe` — it installs in place.
- macOS: drag the new app onto the old one in Applications.
- Linux: `sudo apt install ./<new>.deb`, or just replace the AppImage file.

**One-time note for the Civil Awareness Warrior rename (0.1.0):** the
Windows install directory, the Linux deb package name and the macOS `.app`
name all follow the (new) product name, so the FIRST upgrade after the
rebrand installs alongside the old one instead of replacing it. App state
is keyed by the unchanged identifier — credentials, settings and
downloaded models carry over untouched. Afterwards the old install can be
removed once (uninstall the old Windows entry / `apt remove` the old
package / drag the old app to Trash) and a stale old launch-at-login file
(`lh-contributor-gui.desktop` / `.plist` / HKCU value) deleted; the app
re-registers its own entry under the new name on the next launch.

App state (see *Credentials & data location*) is never touched by
install or uninstall: registration identity, settings, and **downloaded
model files stay verified across upgrades** — no re-download, no
re-verify. The per-user autostart entry survives too; the next launch
re-points it at the new binary.

## Versioning

`src-tauri/tauri.conf.json → version` is the single source of truth.
`npm run sync-version` stamps `package.json` and `src-tauri/Cargo.toml`
(also runs automatically inside every `tauri build` via
`beforeBuildCommand`); `npm run verify-version` fails loudly if any of
the three disagree. Releasing a new version means editing **one** file.

## Releasing (operator)

**One-time setup** — monorepo edition: `sdk/` and `gui/` live in ONE repo,
so the release workflow needs a single checkout and no sibling-repo token:

1. Create the GitHub repo (`lh-contributor`, since renamed on GitHub to
   `civil-awareness-warrior`) in the org and push `main`
   once. `.github/workflows/release.yml` (repo root) builds all three
   platforms from that single checkout — the old sibling-repo checkout
   step and `LH_CONTRIBUTOR_CLIENT_TOKEN` secret are gone.
2. Tag `v0.1.0` and push the tag to trigger the release matrix; see
   CHANGELOG.md for the full first-tag ritual.
3. Push `main`, then follow the **release checklist in CHANGELOG.md**.

What the pipeline (`.github/workflows/release.yml`) does on a `v*` tag
push: builds **Linux** (deb + AppImage), **Windows** (NSIS installer +
portable zip) and **macOS** (universal dmg, arm64+x86_64, on the
macos-14 runner) in parallel, renames bundle artifacts to the kebab
`civil-awareness-warrior_*` naming (Tauri derives bundle file names from
the display productName, and GitHub asset upload would mangle the spaces),
then publishes one GitHub Release named
`Civil Awareness Warrior <tag>` carrying every artifact — as a full
(non-prerelease) release.

Artifacts are **not code-signed/notarized yet** — the next operator
milestone once signing credentials exist: `bundle.windows.nsis`
`signCommand`, and macOS `signingIdentity` + notarytool credentials in
the bundle config (both documented in Tauri's publishing guide). Until
then the Gatekeeper/SmartScreen caveats from *Installing* apply.

`workflow_dispatch` (the "Run workflow" button) executes the same
pipeline and publishes it as a **prerelease named after the ref it was
dispatched from** — a pipeline dry-run; delete the throwaway tag/release
afterward.

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

- **Tray** (`src-tauri/src/main.rs`): Show / Settings… / Pause⇄Resume / Quit
  menu; the Pause item's label flips at runtime with the run-loop phase.
  Settings… shows the window and emits `lh-settings`, which opens the webview
  settings dialog.
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
  a community-total line, dry-run toggle, Start/Pause/Resume, Quit, and a gear
  button that opens the settings dialog (Register lives in the dialog's
  Identity section).
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
- **Settings** (`src-tauri/src/settings.rs` + `src/lib/SettingsDialog.svelte`,
  card t_2b8170a8): persisted to `settings.json` (0600). A `SettingsInput` is
  a partial update — unset fields keep their stored values; `public_key_hex`
  empty means "unchanged" (the dialog shows the stored tail `…xxxxxx` as the
  placeholder). Validation returns a `FieldError { field, message }` so the
  dialog renders the message **under the exact control that is wrong**
  (capabilities-empty, concurrency out of 1–8, unparseable times, bad URL…).
  `capabilities` and `schedule` carry `serde(default)`: a settings.json
  written by an older build loads and simply gains today's defaults.
- **Capabilities**: five per-chunk-type opt-ins (examination / ingest /
  alt_source / state_walk / search). Descriptions are verbatim from the
  coordinator landing page (`lh-tracker/src/routes/landing.ts` → "What
  Workers Do") so the GUI and the public page tell volunteers the same thing.
  The enabled set flows into the lib's lease polls as
  `RunOptions.config.capabilities` (Vec<ChunkType>) at the next start;
  enabling none is rejected on both sides.
- **Run-window schedule**: `active_from`/`active_until` local-clock window,
  default disabled ("always"); overnight windows wrap. When enabled, a 15 s
  watcher politely stops a live loop outside the window (reason `schedule`)
  and auto-resumes when the window reopens — a user Pause is never
  auto-resumed. Saving a schedule edit applies immediately (the save calls
  the watcher); status carries `schedule_enabled` / `schedule_in_window` and
  the pause label says "outside your active window".
- **Unregister**: two-step button (arm → confirm). Refused while the loop is
  live (pause first). Deletes the local credentials file, clears the client
  id and restarts the registration state; the coordinator exposes no revoke
  endpoint, so open leases simply lapse and requeue for other volunteers —
  stated verbatim in the UI.
- **Local Model lane** (Settings ▸ Local Model; cards t_3067589d +
  t_10daf382): three modes (None / Auto bundled sidecar / Custom
  OpenAI-compatible endpoint), sha256-pinned resumable GGUF download
  (default 2B, opt-in 9B, per-model download state), resource-capped
  llama-server spawn with orphan-proof lifecycle, API key in its own 0600
  side file, health-gated classify capability, and a click-only
  "Can I AI?" hardware advisor.
- **Launch at login** (card t_f9279e25): the Settings ▸ System checkbox
  drives the OS entry via tauri-plugin-autostart — HKCU Run value
  (Windows), XDG autostart .desktop (Linux, AppImage-aware), LaunchAgent
  plist (macOS). Applied on save *before* settings.json is written, and
  re-pointed at the running binary on every startup. See the *Autostart*
  section above for semantics and the macOS console-login caveats.

Stubbed / not yet here:

- R2 / presigned-upload staging config is **env-only**, not settings UI
  (see env table). Real (non-dry) runs without it refuse to start with the
  lib's own error surfaced in the banner.
- No autostart of the *run loop* itself: launch-at-login starts the tray
  app idle, and Start stays a human press. No in-app updater (upgrade =
  install the newest release; data and models survive in place). OS
  autostart landed with card t_f9279e25.

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
| `LH_CAPABILITIES` | first-run capability seed — comma list of chunk types (unknown name ⇒ warn + all types) | all five |
| `LH_BRAVE_API_KEY` | search-chunk backend key, passed through to the lib | — |
| `LH_UPLOAD_PRESIGN_TEMPLATE` | presigned upload URL template containing `{key}` | — |
| `LH_R2_BUCKET` / `LH_R2_ACCESS_KEY_ID` / `LH_R2_SECRET_ACCESS_KEY` + `LH_R2_ENDPOINT` or `LH_R2_ACCOUNT_ID` | direct R2 staging for real runs | local (dry-run only) |

Notes:

- `LH_COORDINATOR_URL` / `LH_CONCURRENCY` / `LH_GUI_DRY_RUN` are
  **first-launch seeds**: after `settings.json` exists, the Settings UI is
  the source of truth. `LH_PUBLIC_KEY_HEX` remains a live fallback while the
  stored key is empty. Staging vars (`LH_R2_*`, presign) are read live at
  each start.
- Settings changes take effect as follows: **schedule** immediately (the save
  invokes the window watcher — an out-of-window save politely stops a live
  loop); dry-run, concurrency, capabilities and coordinator URL apply on the
  next start — pause/resume applies them.

## Verification state

- `npm run build` green (Kit 3 + adapter-static SPA in `build/`).
- `cargo build` green against `lh-contributor-client` c3729ac (binary links;
  248 MB debug at `src-tauri/target/debug/lh-contributor-gui`).
- `npx svelte-check --threshold warning`: 0 errors / 0 warnings (a11y
  included). `cargo test`: 12/12 — the original runtime-delta / lease-target /
  runtime.json tests plus: settings round-trip with capabilities+schedule,
  legacy settings.json back-compat, validation pinpoints the bad field,
  window-membership boundaries + overnight wrap, `LH_CAPABILITIES` csv seed,
  schedule-gate/manual-pause semantics, unregister-while-active refusal,
  **settings actually drive the RunOptions the lib consumes** (capabilities
  reach the lease polls, concurrency reaches the lib's stats slots), and a
  full **fake-coordinator lifecycle test** (register → leases with the
  configured capabilities → schedule-pause observed as stopped polling →
  window reopens → auto-resume → re-pause → resume → unregister → re-register
  as a new identity, with a wire-capture log). `cargo clippy --all-targets`:
  0 warnings.
- Dashboard + settings-dialog rendering verified headlessly: headless-chromium
  at 360×480 shows the wrapped header (gear never crushes the title),
  activity + lease rows (elapsed timers) and tiles with values ticking between
  polls (bytes staged 820 KB → 1.1 MB across dumps); the dialog at 360×900
  (`/?settings`) shows all five capability rows with landing-page copy, the
  slider, schedule and placeholders. `.harness/render.mjs` (jsdom, real
  components incl. the dialog opened through the real gear click) asserts the
  dashboard lines, ~30 dialog strings, the public-key tail placeholder,
  scrim-close, the persistence round-trip (stored caps/schedule/concurrency
  reflected in status), capability gating of the demo lease loop, FieldError
  validation (rejected saves leave the store byte-identical — module path and
  real-click UI path showing the inline error), pause→resume, register and
  unregister (refused while live). The real lib `StatsHandle` path (via
  `get_status`) shares those rendering paths, but an actual Tauri window/tray
  session has still not been exercised on this headless box.
- House rules: local repo only (no remote), `commit.gpgsign false`
  repo-local, unsigned commits, the SDK subtree (`sdk/`) is read-only from the GUI side.
