/**
 * Synthetic status source for browser dev/preview (`npm run dev` outside
 * Tauri, `npm run build && npm run preview`): there is no Rust side or
 * lib stats handle in a plain browser tab, so the dashboard renders from
 * these instead — clearly labelled as demo data in the page banner.
 *
 * Shapes mirror the UiStatus / SettingsView structs in
 * src-tauri/src/contributor.rs EXACTLY (field names and nullability), so
 * the real polling, lease-elapsed, and formatting code paths run
 * unmodified against them.
 *
 * The demo simulates an ACTIVE run loop (phase 'running'): leases cycle
 * verifying → executing → submitting → polling on a 14 s cycle per slot,
 * roughly 1 in 4 leases fails, and the counters tick with wall-clock
 * seconds — i.e. the same "live values behind a 2 s poll" behavior the
 * lib's StatsHandle produces.
 */

const CYCLE_SECS = 14; // 0–1 verifying, 2–9 executing, 10–12 submitting, 13 gap
export const DEMO_CONCURRENCY = 2;

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

/** @param {number} slot @param {number} cycle */
function demoChunkId(slot, cycle) {
	/** @param {number} n */
	const hex = (n) => n.toString(16).padStart(4, '0');
	return `d${hex(slot)}${hex(cycle)}-demo-9f2c-01e7-contrib`;
}

/**
 * UiStatus-shaped snapshot at wall-clock `tickSecs` after demo start.
 * @param {number} tickSecs
 * @param {number} [concurrency]
 */
export function makeDemoStatus(tickSecs, concurrency = DEMO_CONCURRENCY) {
	/** @type {any[]} */
	const slots = [];
	let completed = 0;
	let verified = 0;
	let failed = 0;
	let bytes = 0;

	for (let s = 0; s < concurrency; s++) {
		const local = tickSecs + s * 7; // stagger slots so leases differ
		const doneCycles = Math.floor(local / CYCLE_SECS);
		const frac = local % CYCLE_SECS;
		const inFlight = doneCycles; // lease currently held
		// s*3 offset: coprime with both list lengths, so slots show DIFFERENT
		// chunk types/targets (an s*1000 offset modded identically to 0).
		const typeIdx = (s * 3 + inFlight) % CHUNK_TYPES.length;
		const keyIdx = (s * 3 + inFlight) % TARGET_KEYS.length;

		/** @type {string} */ let activity;
		/** @type {string | null} */ let chunkId = null;
		/** @type {string | null} */ let chunkType = null;
		/** @type {string | null} */ let targetKey = null;

		if (frac < 2) {
			activity = 'verifying';
			chunkId = demoChunkId(s, inFlight);
			chunkType = CHUNK_TYPES[typeIdx];
			targetKey = TARGET_KEYS[keyIdx];
		} else if (frac < 10) {
			activity = 'executing';
			chunkId = demoChunkId(s, inFlight);
			chunkType = CHUNK_TYPES[typeIdx];
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

	return {
		phase: 'running',
		error: null,
		coordinator_ok: true,
		// 20 s health-poll cadence, cycled so the "last poll" age varies.
		last_health_ok_ms: Date.now() - (tickSecs % 20) * 1000,
		dry_run: true,
		concurrency,
		total_runtime_secs: PAST_SESSIONS_SECS + tickSecs,
		staging: 'staging: local (dry run)',
		credentials_present: true,
		client_id: 'demo-warrior-9f2c41e7',
		stats: {
			chunks_completed: completed,
			chunks_failed: failed,
			chunks_verified: verified,
			bytes_staged: bytes,
			uptime_secs: tickSecs
		},
		slots,
		client_version: '0.1.0 (demo)',
		gui_version: '0.1.0'
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

/** SettingsView-shaped stand-in for the browser preview. */
export function makeDemoSettingsView() {
	return {
		coordinator_url: 'https://cap.landhunter.app',
		concurrency: DEMO_CONCURRENCY,
		dry_run: true,
		staging_mode: 'staging: local (dry run)',
		public_key_configured: true,
		public_key_tail: 'e8a1c4',
		data_dir: '/demo/app-data',
		credentials_path: '/demo/app-data/credentials.json',
		credentials_present: true,
		client_id: 'demo-warrior-9f2c41e7'
	};
}
