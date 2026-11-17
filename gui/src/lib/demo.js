/**
 * Synthetic settings / status source for browser dev/preview (`npm run dev`
 * outside Tauri, `npm run build && npm run preview`): there is no Rust side
 * or lib stats handle in a plain browser tab, so the dashboard and the
 * settings dialog run against these instead — clearly labelled as demo data
 * in the page banner.
 *
 * Shapes mirror the UiStatus / SettingsView / FieldError structs in
 * src-tauri/src/contributor.rs + settings.rs EXACTLY (field names and
 * nullability), and the demo persists to localStorage with the same
 * validation rules, so "change → restart → values persist" is provable in
 * the browser preview too.
 *
 * The demo simulates an ACTIVE run loop (phase 'running') unless paused:
 * leases cycle verifying → executing → submitting → polling on a 14 s cycle
 * per slot, roughly 1 in 4 leases fails, and the counters tick with
 * wall-clock seconds — i.e. the same "live values behind a 2 s poll"
 * behavior the lib's StatsHandle produces. The run-window schedule gates
 * this synthetic loop the same way the Rust watcher gates the real one.
 */

import { scheduleInWindow, validateSettings, parseHHMM, LOCAL_MODEL_DEFAULT, normalizeLocalModel, demoLlmView } from '#lib/settings-util.js';

const CYCLE_SECS = 14; // 0–1 verifying, 2–9 executing, 10–12 submitting, 13 gap
const SETTINGS_STORAGE_KEY = 'lh-gui-de…s-v1';
const CREDS_STORAGE_KEY = 'lh-gui-demo-…s-v1';

const CHUNK_TYPES = ['state_walk', 'ingest', 'examination', 'alt_source', 'search'];
const TARGET_KEYS = [
	'Maricopa AZ/parcel-000142',
	'Cook IL/census-17031-004200',
	'Harris TX/parcel-0588-2211',
	'Santa Clara CA/parcel-0671-004311',
	'Wayne MI/census-26163-001100',
	'Pima AZ/state_walk-2026-AZ-07',
	'Kings NY/parcel-0302-007112',
	'Denver CO/census-08059-002300'
];

/** Seed so the cross-session total is visibly larger than the session uptime. */
const PAST_SESSIONS_SECS = 3721;

/**
 * Module-level "run session" mirror: the demo starts as if a loop were
 * already running (matching the parent dashboard card's verified state),
 * Start/Pause buttons drive it, and the schedule watcher gates it.
 * Not persisted — like a real app restart, a fresh page load starts fresh.
 * @type {{ running: boolean, pauseReason: string | null, frozenUptimeSecs: number | null }}
 */
const session = {
	running: true,
	pauseReason: null, // null | 'user' | 'schedule'
	frozenUptimeSecs: null
};
/** Wall seconds of the most recent makeDemoStatus call (for pause freezing). */
let lastTickSecs = 0;

const CAP_KEYS = ['examination', 'ingest', 'alt_source', 'state_walk', 'search'];

function defaultSettings() {
	return {
		coordinator_url: 'https://cap.landhunter.app',
		public_key_hex: '',
		concurrency: 2,
		dry_run: true,
		capabilities: { examination: true, ingest: true, alt_source: true, state_walk: true, search: true },
		schedule: { enabled: false, active_from: '09:00', active_until: '17:00' },
		// Launch-at-login is an OS registration — the browser demo has no OS
		// side, so it only mirrors the persisted intent shape.
		autostart: false,
		local_model: { ...LOCAL_MODEL_DEFAULT }
	};
}

function loadSettings() {
	const base = defaultSettings();
	try {
		const raw = localStorage.getItem(SETTINGS_STORAGE_KEY);
		if (!raw) return base;
		const stored = JSON.parse(raw);
		return {
			...base,
			...stored,
			capabilities: { ...base.capabilities, ...(stored.capabilities ?? {}) },
			schedule: { ...base.schedule, ...(stored.schedule ?? {}) },
			local_model: { ...base.local_model, ...(stored.local_model ?? {}) }
		};
	} catch {
		return base;
	}
}

/** @param {any} s */
function saveSettingsToStorage(s) {
	localStorage.setItem(SETTINGS_STORAGE_KEY, JSON.stringify(s));
}

function loadCreds() {
	try {
		const raw = localStorage.getItem(CREDS_STORAGE_KEY);
		if (!raw) return null;
		const c = JSON.parse(raw);
		return c?.client_id ? c : null;
	} catch {
		return null;
	}
}

/**
 * Mirror of `save_settings`: merge the partial input over the stored
 * settings, validate the merged result (throwing the same {field, message}
 * shape the Rust FieldError rejection carries), persist, and return nothing.
 * @param {any} input partial SettingsInput-shaped update
 */
export function demoSaveSettings(input) {
	const merged = loadSettings();
	for (const k of ['coordinator_url', 'public_key_hex']) {
		if (input?.[k] != null) merged[k] = String(input[k]).trim();
	}
	if (input?.concurrency != null) merged.concurrency = Number(input.concurrency);
	if (input?.dry_run != null) merged.dry_run = Boolean(input.dry_run);
	if (input?.autostart != null) merged.autostart = Boolean(input.autostart);
	if (input?.capabilities) {
		for (const k of CAP_KEYS) {
			if (input.capabilities[k] != null) merged.capabilities[k] = Boolean(input.capabilities[k]);
		}
	}
	if (input?.schedule) {
		if (input.schedule.enabled != null) merged.schedule.enabled = Boolean(input.schedule.enabled);
		if (input.schedule.active_from != null) merged.schedule.active_from = String(input.schedule.active_from).trim();
		if (input.schedule.active_until != null)
			merged.schedule.active_until = String(input.schedule.active_until).trim();
	}
	if (input?.local_model) {
		const lm = merged.local_model;
		const i = input.local_model;
		if (i.mode != null && ['none', 'auto', 'custom'].includes(i.mode)) lm.mode = i.mode;
		if (i.cap != null && ['gpu', 'cpu_third', 'cpu_half'].includes(i.cap)) lm.cap = i.cap;
		if (i.endpoint != null) lm.endpoint = String(i.endpoint).trim();
		if (i.llama_server_bin != null) lm.llama_server_bin = String(i.llama_server_bin).trim();
		if (i.model_name != null) lm.model_name = String(i.model_name).trim();
		if (i.model_id != null) lm.model_id = String(i.model_id).trim();
		if (i.ctx_tokens != null) lm.ctx_tokens = Number(i.ctx_tokens);
	}
	// Same normalize the Rust save path runs (classify follows the mode).
	normalizeLocalModel(merged);
	const bad = validateSettings(merged);
	if (bad) throw bad;
	saveSettingsToStorage(merged);
	return settingsView(merged);
}

/** @param {any} s */
function settingsView(s) {
	const creds = loadCreds();
	return {
		coordinator_url: s.coordinator_url,
		concurrency: s.concurrency,
		dry_run: s.dry_run,
		capabilities: s.capabilities,
		schedule: s.schedule,
		autostart: s.autostart ?? false,
		staging_mode: s.dry_run
			? 'staging: local (dry run)'
			: 'staging: not configured (real runs will refuse to start)',
		public_key_configured: true, // the demo seed pretends a contract key is configured
		public_key_tail: 'e8a1c4',
		data_dir: '/demo/app-data',
		credentials_path: '/demo/app-data/credentials.json',
		credentials_present: creds != null,
		client_id: creds?.client_id ?? null,
		local_model: s.local_model,
		llm: demoLlmView(s)
	};
}

/** SettingsView-shaped stand-in for `get_settings_view` (reads persisted demo settings). */
export function makeDemoSettingsView() {
	return settingsView(loadSettings());
}

/** Mirror of `register_client` — mints a synthetic warrior identity. */
export function demoRegister() {
	const id = `demo-warrior-${Math.random().toString(16).slice(2, 10)}`;
	localStorage.setItem(CREDS_STORAGE_KEY, JSON.stringify({ client_id: id, auth_token: 'demo-token' }));
	return `registered as ${id} (credentials saved to /demo/app-data/credentials.json)`;
}

/** Mirror of `unregister_identity`, guard included. */
export function demoUnregister() {
	if (session.running) {
		throw 'pause the contributor loop before unregistering';
	}
	if (!loadCreds()) {
		return 'no local credentials file to remove — this client was never registered here';
	}
	localStorage.removeItem(CREDS_STORAGE_KEY);
	return 'identity removed locally (deleted /demo/app-data/credentials.json) — the coordinator has no revoke endpoint, so this client simply stops acting: any open leases lapse and requeue for other volunteers.';
}

/** Mirror of `start_contributor` / `resume_contributor` guards. */
export function demoStartContributor() {
	if (session.running) throw 'contributor loop already running';
	const caps = loadSettings().capabilities;
	if (!CAP_KEYS.some((k) => caps[k])) throw 'capabilities set is empty';
	session.running = true;
	session.pauseReason = null;
	session.frozenUptimeSecs = null;
}

/** Mirror of `pause_contributor` (graceful stop semantics). */
export function demoPauseContributor() {
	if (!session.running) throw 'contributor loop is not running';
	session.running = false;
	session.pauseReason = 'user';
	session.frozenUptimeSecs = lastTickSecs; // freeze like the real final_uptime
}

/** @param {number} slot @param {number} cycle */
function demoChunkId(slot, cycle) {
	/** @param {number} n */
	const hex = (n) => n.toString(16).padStart(4, '0');
	return `d${hex(slot)}${hex(cycle)}-demo-9f2c-01e7-contrib`;
}

/**
 * UiStatus-shaped snapshot at wall-clock `tickSecs` after page load.
 * Reads the persisted settings (concurrency / dry_run / capabilities /
 * schedule) and the module run-session state — the demo loop is gated by
 * the schedule exactly like the Rust watcher gates the real Runner.
 * @param {number} tickSecs
 */
export function makeDemoStatus(tickSecs) {
	lastTickSecs = tickSecs;
	const settings = loadSettings();
	const { schedule } = settings;
	const inWindow = scheduleInWindow(schedule, new Date());

	// Schedule gating mirrors schedule_tick(): polite stop outside the
	// window, auto-resume on re-entry only for schedule-initiated stops.
	if (schedule.enabled && session.running && !inWindow) {
		session.running = false;
		session.pauseReason = 'schedule';
		session.frozenUptimeSecs = tickSecs;
	} else if (schedule.enabled && !session.running && session.pauseReason === 'schedule' && inWindow) {
		session.running = true;
		session.pauseReason = null;
		session.frozenUptimeSecs = null;
	}

	const running = session.running;
	const concurrency = Math.min(8, Math.max(1, Number(settings.concurrency) || 1));

	/** @type {any[]} */
	const slots = [];
	let completed = 0;
	let verified = 0;
	let failed = 0;
	let bytes = 0;

	if (running) {
		for (let s = 0; s < concurrency; s++) {
			const local = tickSecs + s * 7; // stagger slots so leases differ
			const doneCycles = Math.floor(local / CYCLE_SECS);
			const frac = local % CYCLE_SECS;
			const inFlight = doneCycles; // lease currently held
			// s*3 offset: coprime with both list lengths, so slots show DIFFERENT
			// chunk types/targets (an s*1000 offset modded identically to 0).
			const typeIdx = (s * 3 + inFlight) % CHUNK_TYPES.length;
			const keyIdx = (s * 3 + inFlight) % TARGET_KEYS.length;
			// Only offered chunk types appear on a lease row — mirrors the
			// capability filter the real lease polls carry.
			const capOrder = CHUNK_TYPES.filter((t) => settings.capabilities[t] !== false);
			const capIdx = capOrder.length ? typeIdx % capOrder.length : 0;

			/** @type {string} */ let activity;
			/** @type {string | null} */ let chunkId = null;
			/** @type {string | null} */ let chunkType = null;
			/** @type {string | null} */ let targetKey = null;

			if (frac < 2) {
				activity = 'verifying';
				chunkId = demoChunkId(s, inFlight);
				chunkType = capOrder[capIdx];
				targetKey = TARGET_KEYS[keyIdx];
			} else if (frac < 10) {
				activity = 'executing';
				chunkId = demoChunkId(s, inFlight);
				chunkType = capOrder[capIdx];
				targetKey = TARGET_KEYS[keyIdx];
			} else if (frac < 13) {
				activity = 'submitting';
				chunkId = demoChunkId(s, inFlight);
				targetKey = TARGET_KEYS[keyIdx]; // contract was cached while executing
			} else {
				activity = 'polling';
			}
			slots.push({ slot: s, activity, chunk_id: chunkId, chunk_type: chunkType, target_key: targetKey });

			// A lease counts as verified once its contract checks out (at 2 s).
			verified += doneCycles + (frac >= 2 ? 1 : 0);
			for (let n = 0; n < doneCycles; n++) {
				if (n % 4 === 3) failed += 1;
				else completed += 1;
			}
			const executingFraction = frac >= 2 && frac < 10 ? (frac - 2) / 8 : 0;
			bytes += Math.round(480_000 * (doneCycles + executingFraction));
		}
	}

	const uptime_secs = running ? tickSecs : (session.frozenUptimeSecs ?? 0);

	return {
		phase: running ? 'running' : 'paused',
		error: null,
		coordinator_ok: true,
		// 20 s health-poll cadence, cycled so the "last poll" age varies.
		last_health_ok_ms: Date.now() - (tickSecs % 20) * 1000,
		dry_run: settings.dry_run,
		concurrency,
		pause_reason: running ? null : session.pauseReason ?? 'user',
		schedule_enabled: schedule.enabled,
		schedule_in_window: inWindow,
		total_runtime_secs: PAST_SESSIONS_SECS + uptime_secs,
		staging: settings.dry_run ? 'staging: local (dry run)' : 'staging: not configured (real runs will refuse to start)',
		credentials_present: loadCreds() != null,
		client_id: loadCreds()?.client_id ?? null,
		stats: {
			chunks_completed: completed,
			chunks_failed: failed,
			chunks_verified: verified,
			bytes_staged: bytes,
			uptime_secs
		},
		slots,
		client_version: '0.1.0 (demo)',
		gui_version: '0.1.0',
		// LlmView-shaped live lane mirror (the browser demo never downloads
		// or spawns anything — statuses stay honest per settings-util).
		llm: demoLlmView(settings)
	};
}

/**
 * CommunityStats-shaped stand-in for what `community_stats` returns in the
 * real app (the browser can't fetch /public/stats.json directly — CORS).
 * @param {number} [nowMs]
 */
export function makeDemoCommunity(nowMs = Date.now()) {
	return {
		chunks_verified: 154,
		chunks_pending: 613,
		chunks_leased: 5,
		warriors_online: 3,
		warriors_total: 12,
		updated_at: nowMs
	};
}

/** Exported so tests/harnesses can inspect what the demo persisted. */
export function demoReadPersistedSettings() {
	try {
		return JSON.parse(localStorage.getItem(SETTINGS_STORAGE_KEY) ?? 'null');
	} catch {
		return null;
	}
}

/** Sanity re-export so the dialog can use the same parser as the demo. */
export { parseHHMM };
