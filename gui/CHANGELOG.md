# Changelog

Keep-a-Changelog style entries, SemVer versions. **The single source of
truth for the version is `src-tauri/tauri.conf.json`** — `npm run
sync-version` stamps `package.json` + `src-tauri/Cargo.toml` (every
`tauri build` runs it automatically; see README ▸ Versioning).

## [Unreleased]

- Release signing/notarization (NSIS `signCommand`, macOS
  `signingIdentity` + notarytool) — blocked on operator signing
  credentials, not on code.
- `STRATA_DOCS_URL` in `src-tauri/src/hardware.rs` is a `<OPERATOR_FILL>`
  sentinel: replace with the published Strata install-instructions URL
  when it exists. Until then the hardware-advice Strata line renders
  without a link (by design — no invented URLs).
- Known pre-existing gap (flagged in card t_10daf382, deliberately not
  inherited): the sidecar Unix lifecycle **test** helpers (`libc::kill`,
  `Permissions::from_mode`) don't compile under a Windows *test* profile;
  the main Windows build profile is clean.

## [0.1.0] — first tagged release (card t_f9279e25; tag ritual below)

The full contributor train, packaged for the first time:

### Added

- Contributed-work **dashboard** (headline tiles, per-lease rows with
  elapsed timers, community totals, cross-session runtime).
- **Settings**: capabilities / concurrency / run-window schedule /
  identity (register, unregister).
- **Local Model lane**: None / Auto (sha256-pinned resumable GGUF
  download + resource-capped llama-server sidecar, orphan-proof
  lifecycle) / Custom OpenAI-compatible endpoint; API key in its own
  0600 file; classify capability follows the mode.
- Bundled models: default 2B-class `qwen2.5-1.5b-instruct-q4_k_m`
  (1,117,320,736 B, Apache-2.0) + opt-in `ornith-1.5-9b-q4_k_m`
  (5,780,090,816 B, MIT), with per-model download state.
- **"Can I AI?" hardware advisor**: click-only read-only RAM/VRAM probe
  (DXGI / sysfs / nvidia-smi / sysctl) + suggestion matrix; inconclusive
  probes never guess.
- **Launch-at-login autostart** (Settings ▸ System): per-user OS entry —
  HKCU Run value / XDG autostart .desktop / LaunchAgent plist — applied
  on save before settings.json, re-pointed at the running binary every
  startup (README ▸ Autostart).
- **Packaging**: Windows NSIS installer (per-user, no admin) + CI-built
  portable zip; macOS **universal** dmg (arm64+x86_64, minimum macOS
  13); Linux `.deb` + `.AppImage`.
- **Version strategy**: `tauri.conf.json` single source of truth,
  propagated by `scripts/sync-version.mjs` via `beforeBuildCommand`.
- **Release CI** (`.github/workflows/release.yml`): on `v*` tag push, a
  3-OS build matrix plus a release job that publishes every artifact to
  one GitHub Release; `workflow_dispatch` as a prerelease pipeline
  dry-run.

## Release checklist (operator — run this per release)

1. **One-time before the first release** — README ▸ *Releasing
   (operator)*: create the GitHub repos (this one + same-org
   `lh-contributor-client`), add the `LH_CONTRIBUTOR_CLIENT_TOKEN` secret
   only if the SDK repo is private, push `main`.
2. **Pick the version** — edit `src-tauri/tauri.conf.json → version`
   ONLY (e.g. `0.1.0`).
3. `npm run sync-version && npm run verify-version` — all three manifests
   must agree. Commit the bump unsigned, locally:
   `chore(release): v0.1.0`.
4. **Gates green before tagging** (repo convention):
   `cargo test` in `src-tauri/` (51/51 as of this card),
   `cargo build`, `cargo check --target x86_64-pc-windows-gnu`, and at
   the repo root `npm run build` + `npm run check`. Optional but
   recommended locally: `npx tauri build --bundles deb,appimage`.
5. **Tag and push** (this is the operator's push — workers never push):
   `git tag -a v0.1.0 -m "v0.1.0"` then `git push origin main v0.1.0`.
   The tag push IS the release trigger.
6. **Watch the Actions tab** → `Release` workflow: linux / windows /
   macos-universal build legs, then the release job (runs only after all
   three legs pass). On a leg failure, fix or re-run just that leg.
7. **Verify the Release page carries all five artifact kinds**:
   `.deb`, `.AppImage` (+ `.zsync`), NSIS `*_x64-setup.exe`,
   `lh-contributor-gui-portable.zip`, and the universal `.dmg`. A tag
   push publishes a full (non-prerelease) release; a
   `workflow_dispatch` dry-run publishes a prerelease — delete the
   throwaway afterward.
8. **Smoke-test the volunteer path** on at least one machine per
   platform: install → first run → tray present → Settings dialog opens
   → a dry run starts → launch-at-login (if ticked) survives a logout/
   login cycle.
9. When signing credentials exist, add the signing hooks (README ▸
   Releasing) and retire the Gatekeeper/SmartScreen paragraph from
   *Installing*.
