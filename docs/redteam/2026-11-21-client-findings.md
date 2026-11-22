# RT2 — Client + supply-chain red-team findings

**Scope:** `lh-contributor` @ `bfb1982` (Lane C landed; local main, 2026-11-21) — GUI (Tauri), SDK, release pipeline, web surfaces. Landing `/download` resolver reviewed as externally-observable behavior (implementation lives in the private tracker repo; not quoted here).
**Verdict up front:** the trust core is sound — signature verification fails closed on key rotation, credentials are 0600/atomic, the LLM key never leaves its endpoint, archive extraction is hardened. The serious issues are lifecycle ones: a release can still ship the placeholder key, a stored key override can never be cleared from the UI, and switching the coordinator URL hands the old tracker's auth token to the new host.

Severity scale: **HIGH** = exploitable before or at next release · **MED** = exploitable with user action or owner-token compromise · **LOW** = hardening.

---

## F1 — Release can ship the placeholder contract key (Start bricked + built-in phishing prompt) · HIGH

**Narrative.**
1. `PUBLIC_KEY_HEX_DEFAULT` is `"REPLACE_ME_BEFORE_RELEASE_64_HEX"` in source; release prep swaps it by hand (settings.rs:30, doc comment lines 20-29).
2. `build_config` (contributor.rs:755-764) checks only `len == 64 && ascii_hexdigit`. The placeholder is neither, so the app fails closed — good — but the failure message is *"contract public key missing — set LH_PUBLIC_KEY_HEX (64-char hex ed25519) or enter it under Settings"*.
3. If the swap is forgotten, every volunteer's Start fails with a message that **solicits pasting a key**. That is a ready-made social-engineering channel: "the tracker is having issues, paste this key under Settings to fix it" — and a pasted attacker key beats the pin forever (see F2's precedence chain). The version-guard job (release.yml:42-62) checks tag-vs-conf only; nothing checks the pin.
4. No test asserts the const's shape, so the normal gates (cargo test 69/69, clippy, svelte-check) all stay green with the placeholder in place — this exact state is what main is in right now.

**Evidence.** gui/src-tauri/src/settings.rs:30; gui/src-tauri/src/contributor.rs:755-764; .github/workflows/release.yml:42-62 (no pin check).

**Fix.** Add a `pin-guard` step to `version-guard` in release.yml: fail the tag build unless `PUBLIC_KEY_HEX_DEFAULT` matches `^[0-9a-fA-F]{64}$`. One grep, closes both the availability failure and the phishing template. (Optionally a `#[cfg(test)]` assert that fails only on `debug_assertions`-off release builds is not possible for a source placeholder — CI is the right place.)

---

## F2 — Stored key override cannot be cleared from the UI; a stale override silently beats the pinned key forever · HIGH

**Narrative.**
1. Resolution order is stored Settings → `LH_PUBLIC_KEY_HEX` env → compile-time pin (settings.rs:472-485). The stored value wins unconditionally.
2. Every GUI user of v0.1.0–v0.1.3 who got Start working pasted a key into Settings (there was no pin). Those stored keys persist into the pinned-key release.
3. On key rotation (new release, new pin), those clients keep verifying against the **old** key: every lease fails `verify_lease_contract` → after 3 consecutive failures the run aborts (runner.rs:440-457) with *"check LH_PUBLIC_KEY_HEX or suspect tampering"* — a message that never mentions the stored override, which the app itself cannot know is stale.
4. There is **no way to clear the override from the UI**: the dialog seeds `public_key_hex: ''` and sends `undefined` when blank ("leave blank to keep") — `Some("")`, the only value that would clear it (merge_settings, contributor.rs:1586-1588), is never sent (SettingsDialog.svelte:118-121). The placeholder even reassures: *"unchanged (…tail)"* (SettingsDialog.svelte:549). The only escape is hand-editing settings.json.
5. Net effect after any rotation: the most engaged users (the ones who pasted a key manually) are exactly the ones permanently bricked, with an error message pointing them at the wrong fix — or at an attacker-supplied "fix" (F1 narrative).

**Evidence.** settings.rs:472-485 (precedence), settings.rs:499-505 (stored key validated only for shape, never compared to pin); contributor.rs:1586-1588 (merge only on `Some`); SettingsDialog.svelte:36, 118-121, 549; runner.rs:440-457 (fail-closed abort + message).

**Fix.** Two small changes: (a) add a "clear override" affordance (like the LLM key's Remove button) that sends `Some("")`; (b) when a lease verify-fails and a stored override differs from `PUBLIC_KEY_HEX_DEFAULT`, surface "your stored advanced key differs from the app's pinned key — clear it under Settings" instead of the generic env-var message. Optional stronger form once the pin has shipped a rotation or two: invert precedence to pin-first for stock installs and demote the stored key to a dev override that must be re-typed after each app update.

---

## F3 — Switching the coordinator URL sends the existing auth token to the new host (cross-tracker token replay); `http://` accepted with no warning · HIGH

**Narrative.**
1. Credentials are a single `credentials.json` in the app data dir; the run loop uses it regardless of which coordinator URL is configured (contributor.rs:768, runner.rs:172-184).
2. All authenticated routes carry `client_id` + `auth_token` in the request **body** (api.rs:1-7; lease 671-684, heartbeat 686-699, result 701-703, fail 705-718, search-proxy 534-557).
3. Attack: convince a volunteer to change the coordinator URL ("use our regional mirror / faster endpoint" — a plain text field, settings.rs:492-498 accepts any `http(s)://` with no confirmation). The very next lease poll sends the **real tracker's token** to the attacker's host. Over `http://` it's cleartext on the wire; over `https://` it's delivered to the attacker anyway.
4. With the stolen token the attacker acts as the victim against the **real** tracker: leases chunks, submits crafted results under the victim's identity. Lane B's quarantine blunts public-stats poisoning, but canary misses land on the *victim's* reputation (shadow-quarantine), and one stolen identity + one sybil satisfies the k=2 consensus default for promotion — this is the RT1 consensus model defeated by one phished URL.
5. A hostile endpoint additionally gets: the register declaration surface (platform, version, capabilities, LLM capability — no secrets), verify-failure details (aids crafting the F1/F2 phish), and can replay **genuinely signed** public contracts (`GET /v1/chunk/:id/contract` is public, api.rs:720-734) under forged lease envelopes — signature-valid, so the client executes real work and submits results + token to the attacker. It can never mint a *forged* contract (no operator key) — that path stays closed.
6. The 401 emergency re-register (runner.rs:395-412) also fires against whatever URL is configured, minting attacker-issued identities without user visibility.

**Evidence.** settings.rs:492-498 (scheme check is `starts_with` only); contributor.rs:254-257 (ApiClient built from stored URL verbatim); api.rs:677-683 (token in body); runner.rs:172-184 (saved creds reused for any host); contributor.rs:1880-1894 (re-register only on explicit button).

**Fix.** (a) Key credentials per coordinator host (`credentials-<host>.json` or a map), so a URL change never replays an old token — this kills the exfil class outright; (b) confirm-and-warn dialog when the coordinator URL changes away from the default: "the tracker at this URL will receive your identity token and your work results — Register again unless you trust it"; (c) allow `http://` only for loopback hosts, or gate non-HTTPS behind the same confirmation (the default is already HTTPS, settings.rs:18 — keep that).

---

## F4 — Unsigned release artifacts + mutable release assets + suffix-matching download resolver = GitHub-token compromise is mass RCE · MED

**Narrative.**
1. No Tauri updater is configured (tauri.conf.json has no updater block; no `tauri-plugin-updater` dep) — updates are "download the new installer", delivered by the landing `/download/:slug` routes which 302 to the latest GitHub release assets (behavior of the public landing page; resolver implementation in the private tracker repo).
2. Release artifacts are unsigned by documented decision (release.yml:7-8). `SHA256SUMS` is generated by the same CI run from the same bytes (release.yml:218-227) — it is integrity-vs-transport only. An attacker who can add or replace an asset on a release (GitHub token compromise, malicious PAT, or a compromised `contents: write` workflow) republishes `SHA256SUMS` alongside the trojaned binary and everything stays "self-consistent".
3. The resolver matches assets by **name suffix** and picks the **first** match (`_x64-setup.exe`, `_universal.dmg`, …) and caches resolution for 10 minutes. Adding a second asset that also ends in `_x64-setup.exe` to an existing release is enough to be served for that slug — no retag, no CI run, no version-guard exposure.
4. Client-side there is no signature check anywhere on this path: volunteers get whatever bytes the 302 lands on. With launch-at-login installed, a trojaned update is persistent code execution on every contributor machine.

**Evidence.** gui/src-tauri/tauri.conf.json (no updater); .github/workflows/release.yml:7-8, 218-236; landing `/download` slugs observed on the public site (`/download/windows-installer` → 302 to `objects.githubusercontent.com`).

**Fix.** Minimum: sign `SHA256SUMS` (minisign/cosign) with an operator key whose public half is printed on the landing page and inside the app's About/Settings, and document `minisign -V` in the download section. Better: exact-asset-name equality in the resolver (drop suffix matching) and refuse to resolve when a release carries >1 candidate for a slug. Longer term: Tauri updater with a signed manifest so updates stop being a browser download at all. The signing key, like the contract key, never travels through chat — the operator generates it.

---

## F5 — `workflow_dispatch` bypasses the version guard and can publish arbitrary-tag releases · MED

**Narrative.**
1. `version-guard` runs only on tag pushes (`if: startsWith(github.ref, 'refs/tags/')`, release.yml:46). `build` proceeds when the guard was skipped (release.yml:68) — so a dispatch build never checks tag-vs-version.
2. The publish step releases against `inputs.tag_name || github.ref_name` (release.yml:232-233): on dispatch from a branch this creates/moves a release (and softprops will create the tag at the dispatch SHA) named after a **branch** or any typed string. A rushed or compromised dispatch can publish `v9.9.9` built from arbitrary, unguarded sources — exactly the mislabel class the guard was built to stop (v0.1.1/v0.1.3 incidents).
3. Minor adjacent gap: the build's "verify-version" step runs *after* `sync-version` has already **mutated** the checkout (release.yml:131-136) — it verifies that sync worked, not that the repo was consistent. The tag-vs-conf guard covers the real case, but only on the tag-push path (see 1).
4. Mitigation present: publishing requires `contents: write` (a credentials boundary), and there is no in-app auto-update to amplify a bad release (F4). Severity stays MED for that reason.

**Evidence.** .github/workflows/release.yml:20-28 (dispatch inputs, prerelease default true), 42-68 (guard gating), 229-236 (publish tag_name).

**Fix.** Run the version check on dispatch too (derive expected version from `inputs.tag_name` when present, else fall back to `github.ref_name` only if it is a `v*` tag); reject dispatch when `tag_name` is not `v*`-shaped. In CI, call `sync-version.mjs --verify` **without** the apply step first (or add a `--check-only` invocation before stamping) so drift fails instead of being silently fixed in-flight.

---

## F6 — `demo.js` (and its localStorage identity) ships inside the production bundle · LOW

**Narrative.**
1. Browser-preview demo data is statically imported by `+page.svelte` and `SettingsDialog.svelte` (+page.svelte:5-12; SettingsDialog.svelte:5) and gated at runtime by `'__TAURI_INTERNALS__' in window` (+page.svelte:14). In Tauri the demo paths never execute — dead code in the shipped JS.
2. The demo can only ever store a **hardcoded fake** (`auth_token: 'demo-token'`, demo.js:183-187) under demo-scoped localStorage keys (demo.js:25-26) and makes no network calls (makeDemoCommunity is synthetic precisely because CORS blocks the browser). A real token cannot reach it: the Rust side never sends the token to the webview (`settings_view`/`get_status` carry `client_id` only), and CSP `default-src 'self'` (tauri.conf.json:26) blocks any injected-page scenario from loading remote content.
3. Residual cost is bundle hygiene + the `?settings` dev URL param also shipping in prod (+page.svelte:28-30).

**Evidence.** gui/src/lib/demo.js:25-26, 183-187; gui/src/routes/+page.svelte:14, 28-30; tauri.conf.json:26; contributor.rs:1809, 1716 (client_id only in views).

**Fix.** Move `#lib/demo.js` behind a dynamic `import()` in the `!inTauri` branch so it is code-split out of the production bundle (Vite splits dynamic imports automatically). Optional: strip the `?settings` param in production builds.

---

## F7 — Supply-chain spot-check: clean overall; two reqwest major versions compiled in, tar path less hardened than zip, no CI test lane · LOW

**Narrative / evidence.**
1. **Deps:** both `Cargo.lock` files and `package-lock.json` are committed (`git ls-files`: `gui/src-tauri/Cargo.lock`, `sdk/Cargo.lock`, `gui/package-lock.json`); 571 locked crates; direct deps are small and mainstream (tauri 2.12.7, reqwest 0.12 w/ rustls, serde, tokio, chrono, url, sha2, zip 9.8.1, tar 0.4.46, flate2, ed25519-dalek 2.2.0). GitHub Actions are SHA-pinned (release.yml:48, 105, 108, 115, 191, 212, 230). No anomalies like git deps or build-script curl-bombs spotted at direct-dep level.
2. **Anomaly worth fixing:** the GUI lock contains **two reqwest majors** (0.12.28 app-pinned + 0.13.5 pulled by the tauri ecosystem) — two HTTP/TLS stacks compiled into the binary; audit surface and size for no benefit. Fix: bump the app's reqwest to the major the ecosystem already uses (or wait for the ecosystem), one-line Cargo.toml change at a quiet moment.
3. **Extraction asymmetry:** the zip path is exemplary (`enclosed_name()`, symlink skip, leaf-name filter, sidecar.rs:1283-1305); the tar path relies on `tar::Archive::unpack`'s built-in traversal rejection and does **not** skip symlink entries or apply the leaf filter at extract time (sidecar.rs:1270-1276) — `collect_promotables` filters by *name* later, but a symlinked `llama-server` name-matches and `fs::copy` follows it. Practical risk is low (archive bytes are sha256-verified against GitHub's API digest before extraction, sidecar.rs:1018-1029, 1088-1094, so this needs a malicious *upstream* llama.cpp release — at which point running llama-server is already game over), but the zip path shows the intended standard. Fix: iterate tar entries and apply the same enclosed-name/symlink/leaf policy.
4. **No CI test lane:** `.github/workflows/` contains only `release.yml` — nothing runs `cargo test`/`clippy`/`svelte-check` on push/PR, and the release pipeline itself runs zero tests before publishing. Gates currently live in kanban lane discipline only. Fix: a minimal `ci.yml` (test + clippy + svelte-check on the three OSes, or at least linux) so a tag cannot ship an untested tree.

---

## F8 — Verified-clean items (for the record)

- **Key rotation fails closed:** old binary + new operator key → every lease fails verification, each failure is fail-reported, 3 consecutive failures abort the run (runner.rs:440-457). No fail-open path exists in `verify_contract`/`verify_lease_contract` (crypto.rs:149-212: malformed key/sig/prefix all return false/Err). There is **no MITM window during rotation** because the key never transits the network — it is compiled in (settings.rs:20-29) — and a network attacker cannot force distrust of a locally pinned key (the DoS-a-key idea has no network surface). Rotation is strictly a new-release event.
- **Credentials hygiene:** `save_credentials` writes temp → chmod 0600 → atomic rename, no lax window, chmod failure is fatal (config.rs:310-324, tested at 393-454); GUI register persists through the same function (contributor.rs:1884). `Credentials` has no `Debug` (api.rs:355-364); no log/echo path formats the token (tracing calls log client_id/lease_id/chunk_id/target only, runner.rs:181, 410, 422-429); no crash reporter exists; `LlmConfig::api_key` and `StagingConfig::R2::secret_access_key` likewise Debug-less (config.rs:33-34, 132-133).
- **LLM key isolation:** the custom-endpoint key lives in its own 0600 `llm_api_key.json`, is sent **only** as a bearer to `local_model.endpoint` (llm.rs:282-283), never appears in any settings view (`api_key_present` boolean only, contributor.rs:1449-1450), and never travels to the coordinator (register declares `LlmCapability` fields only, api.rs:292-299).
- **Brave key routing:** backend `custom` sends no auth headers at all; backend `brave` targets the fixed Brave URL only (chunk/search.rs:63-115) — a contract cannot redirect the Brave key to an attacker endpoint. Contracts are operator-signed, so this whole lane is unreachable without the operator key or an override (F2).
- **Source fetching:** redirect policy `none` — a contracted source cannot bounce the client off-host (runner.rs:223-231).
- **Sidecar exec:** the llama-server path override must be absolute, regular, executable, correctly named, symlink-free at every component, and is re-validated on load (settings.rs:608-660, 679-685). Opener plugin scoped to `huggingface.co`/`github.com` only (capabilities/default.json).
- **Version propagation:** `sync-version.mjs` is idempotent, refuses unparsable layouts, and tauri.conf.json is the single source of truth (gui/scripts/sync-version.mjs:20-47).

---

## Triage table

| ID | Severity | Verdict |
|----|----------|---------|
| F1 placeholder key can ship | HIGH | **Must fix before release** — CI pin-guard step (one grep in release.yml) |
| F2 stored key override unclearable / silently wins | HIGH | **Must fix before release** — clear-override affordance + stale-override error hint (F2a); precedence flip can defer |
| F3 coordinator-URL change exfiltrates auth token; http allowed | HIGH | **Must fix before release** — per-host credentials (F3a) or change-confirmation + TLS gate; F3a preferred |
| F4 unsigned artifacts, suffix-match resolver | MED | Fix before release if cheap (exact asset match + signed SHA256SUMS); full signing can be the fast-follow |
| F5 dispatch bypasses version guard | MED | Fix before release — run guard on dispatch, reject non-`v*` tag_name |
| F6 demo.js in prod bundle | LOW | Defer — dynamic import at next frontend touch |
| F7 reqwest dupe / tar asymmetry / no CI tests | LOW | Defer — reqwest dedupe + ci.yml at a quiet moment; tar hardening with F4's signing pass |
| F8 clean confirmations | — | No action |
