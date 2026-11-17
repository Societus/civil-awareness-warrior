#!/usr/bin/env node
// Version propagation (card t_f9279e25): tauri.conf.json is the SINGLE
// source of truth for the release version. This script stamps the same
// version into package.json and src-tauri/Cargo.toml, so a tag push builds
// every platform artifact with one identical version.
//
//   node scripts/sync-version.mjs           — apply (rewrites only on drift)
//   node scripts/sync-version.mjs --verify  — check only; exit 1 on drift
//
// `tauri build` runs the apply form via build.beforeBuildCommand; CI runs
// both forms explicitly (sync, then verify) so a mismatch fails loudly
// instead of shipping mislabeled artifacts.
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const read = (p) => readFileSync(path.join(root, p), "utf8");

const version = JSON.parse(read("src-tauri/tauri.conf.json")).version;
if (!/^\d+\.\d+\.\d+([-+][0-9A-Za-z.-]+)?$/.test(String(version))) {
	console.error(
		`tauri.conf.json version ${JSON.stringify(version)} is not plain semver (1.2.3 or 1.2.3-tag.1) — refusing to propagate`
	);
	process.exit(1);
}

const targets = [
	{
		// Top-level "version" key. package.json has no other "version":
		// keys (dependency ranges are values under named keys).
		// The replacement MUST splice the version back into the captured
		// shell (`$1`/`$2`): a bare replacement string would eat the
		// leading indentation the `^(\s*` anchor gobbled (learned the hard
		// way on the first real build run).
		file: 'package.json',
		pattern: /^(\s*"version":\s*")[^"]+(")/m,
		repl: `$1${version}$2`,
	},
	{
		// The [package] version — the first `version = "..."` line; nothing
		// before it in Cargo.toml can match. No surrounding shell to keep.
		file: 'src-tauri/Cargo.toml',
		pattern: /^version = "[^"]*"/m,
		repl: `version = "${version}"`,
	},
];

const verify = process.argv.includes('--verify');
let drift = 0;
for (const t of targets) {
	const raw = read(t.file);
	if (!t.pattern.test(raw)) {
		// A moved/renamed field is a refactor this script was not told about
		// — say so loudly instead of appending a duplicate key.
		console.error(`${t.file}: no version field matched — refusing to guess`);
		process.exit(1);
	}
	const updated = raw.replace(t.pattern, t.repl);
	if (updated !== raw) {
		drift++;
		if (verify) {
			console.error(`${t.file} does not carry ${version} — run: npm run sync-version`);
		} else {
			writeFileSync(path.join(root, t.file), updated);
			console.log(`${t.file} -> ${version}`);
		}
	}
}

if (verify) {
	if (drift) process.exit(1);
	console.log(`version ${version} consistent: tauri.conf.json = package.json = Cargo.toml`);
}
