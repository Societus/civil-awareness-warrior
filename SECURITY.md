# Security Policy

## Supported Versions

One supported line. We ship fast, and volunteers should always run the newest release.

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |
| < 0.1   | :x: (pre-release)  |

## Reporting a Vulnerability

Use **GitHub's private vulnerability reporting**: Security tab → *Report a vulnerability*.
Please do **not** open a public issue for anything exploit-shaped.

What to expect:

- **Acknowledgment** within 72 hours.
- **Status updates** at least every 5 days while a report is in flight.
- **Accepted reports** get a fix on the next release, credited in the release notes (or anonymously, your choice).
- **Declined reports** get a written reason — no silent closes.

## Scope

**In scope:** the desktop client in this repository — chunk contract handling, fetch-plan execution, staging, the llama-server sidecar (binary/model acquisition and spawn), credential storage, Tauri command surface, and the update path.

**Also in scope but built elsewhere:** the coordinator API at `cap.landhunter.app` (a Cloudflare Worker). Coordinator-side reports are welcome through the same channel.

**Out of scope:** social engineering of volunteers, volumetric DoS of free-tier Cloudflare, and reports from automated scanners without a described path — tell us *how*, not just *that*.

## What We Already Do

This codebase ships with a standing security posture rather than a promise to think about security later:

- Chunk contracts are **ed25519 signature-verified before any execution** — nothing unsigned runs.
- Fetch plans are pinned to contract-allowlisted hosts with size caps and redirects disabled.
- Downloaded binaries and models are **digest-pinned and verified before use**.
- Credentials are stored `0600`; coordinator tokens are hashed at rest.
- The coordinator and client have both been through a **two-pass static security audit** (hermes-security/method-1), and every finding from those audits is remediated in the shipping code.

## AI Disclosure

AI is used to assist in development and securing of this application — because what sane person can keep up with all the exploits lately? Every AI-assisted change is human-reviewed and signed before it lands here.
