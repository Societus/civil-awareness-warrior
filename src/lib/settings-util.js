/**
 * Shared settings helpers between the settings dialog (real app) and the
 * browser demo mirror. Validation rules mirror src-tauri/src/settings.rs
 * exactly (same field ids, same messages in spirit) so the demo exercises
 * the same failure shapes the Rust side produces.
 */

/** @param {string} s @returns {number | null} minutes since local midnight */
export function parseHHMM(s) {
	const m = /^(\d{2}):(\d{2})$/.exec(String(s ?? ''));
	if (!m) return null;
	const h = Number(m[1]);
	const min = Number(m[2]);
	if (h > 23 || min > 59) return null;
	return h * 60 + min;
}

/** @param {{enabled:boolean, active_from:string, active_until:string}} schedule @param {Date} date */
export function scheduleInWindow(schedule, date) {
	if (!schedule?.enabled) return true;
	const from = parseHHMM(schedule.active_from);
	const to = parseHHMM(schedule.active_until);
	if (from == null || to == null || from === to) return true; // corrupt → always
	const mins = date.getHours() * 60 + date.getMinutes();
	return from < to ? mins >= from && mins < to : mins >= from || mins < to;
}

/** @param {Date} d @returns {string} zero-padded HH:MM */
export function fmtHHMM(d) {
	return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
}

/**
 * Validate a merged settings draft. Returns null when valid, else a
 * `{field, message}` shaped error — same wire shape the Rust save_settings
 * rejection carries, so the dialog's inline-error code is shared.
 * @param {any} merged
 */
export function validateSettings(merged) {
	const err = (
		/** @type {string} */ field,
		/** @type {string} */ message
	) => ({ field, message });
	const url = String(merged.coordinator_url ?? '').trim();
	if (!/^https?:\/\//.test(url)) {
		return err('coordinator_url', 'coordinator URL must start with http:// or https://');
	}
	const key = String(merged.public_key_hex ?? '').trim();
	if (key && !/^[0-9a-fA-F]{64}$/.test(key)) {
		return err('public_key_hex', 'public key must be 64 hex characters (32-byte ed25519)');
	}
	const conc = Number(merged.concurrency);
	if (!Number.isInteger(conc) || conc < 1 || conc > 8) {
		return err('concurrency', 'concurrency must be between 1 and 8');
	}
	const caps = merged.capabilities ?? {};
	if (!CAP_KEYS.some((k) => caps[k] === true)) {
		return err(
			'capabilities',
			'enable at least one chunk type — the run loop refuses to start with an empty capability set'
		);
	}
	if (merged.schedule?.enabled) {
		const from = parseHHMM(merged.schedule.active_from);
		const to = parseHHMM(merged.schedule.active_until);
		if (from != null && to != null && from === to) {
			return err('schedule', 'active-from and active-until must differ — or switch back to Always active');
		}
		if (from == null) {
			return err(
				'schedule',
				`active-from '${merged.schedule.active_from}' is not a valid time — use HH:MM 24-hour (use Always active for no window)`
			);
		}
		if (to == null) {
			return err(
				'schedule',
				`active-until '${merged.schedule.active_until}' is not a valid time — use HH:MM 24-hour (use Always active for no window)`
			);
		}
	}
	const lmErr = validateLocalModel(merged);
	if (lmErr) return lmErr;
	return null;
}

const CAP_KEYS = ['examination', 'ingest', 'alt_source', 'state_walk', 'search'];

// ---------------------------------------------------------------------------
// Local Model (t_3067589d) — mirrors LocalModelCfg / LlmMode / ResourceCap /
// validate() / normalize_local_model() in src-tauri/src/settings.rs, and the
// LlmView shape from contributor.rs, so the browser demo exercises the same
// shapes and failures as the Rust side.
// ---------------------------------------------------------------------------

export const LOCAL_MODEL_MODES = [
	{ value: 'none', label: 'None — no local model (default)' },
	{ value: 'auto', label: 'Auto-configure — bundled model + llama-server sidecar' },
	{ value: 'custom', label: 'Custom endpoint — OpenAI-compatible server you run' }
];

export const LOCAL_MODEL_CAPS = [
	{ value: 'gpu', label: 'GPU (if available)' },
	{ value: 'cpu_third', label: 'CPU ≤⅓ cores' },
	{ value: 'cpu_half', label: 'CPU ≤½ cores' }
];

export const LOCAL_MODEL_DEFAULT = {
	mode: 'none',
	cap: 'cpu_third',
	endpoint: '',
	llama_server_bin: '',
	model_name: '',
	ctx_tokens: 4096
};

export const LLM_DEFAULT_ENDPOINT = 'http://127.0.0.1:8081/v1';

/** Mirror of sidecar::is_loopback_url — loopback + private LAN ranges.
 * @param {string} url @returns {boolean} */
export function isLocalEndpoint(url) {
	let host;
	try {
		host = new URL(url).hostname.toLowerCase();
	} catch {
		return false;
	}
	if (host === 'localhost' || host.endsWith('.local')) return true;
	if (host === '::1' || host.startsWith('fe80') || host.startsWith('fc') || host.startsWith('fd')) return true;
	const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.\d{1,3}$/.exec(host);
	if (!m) return false;
	const a = Number(m[1]);
	const b = Number(m[2]);
	return a === 127 || a === 10 || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168);
}

/** Mirror of sidecar::ensure_v1. @param {string} url @returns {string} */
export function ensureV1(url) {
	const trimmed = String(url).trim().replace(/\/+$/, '');
	try {
		const path = new URL(trimmed).pathname;
		if (path === '' || path === '/') return `${trimmed}/v1`;
	} catch {
		/* leave unparsable strings alone; validation flags them */
	}
	return trimmed;
}

/** Mirror of GuiSettings::normalize_local_model (mutates the merged draft).
 * @param {any} merged @returns {any} */
export function normalizeLocalModel(merged) {
	merged.capabilities.classify = (merged.local_model?.mode ?? 'none') !== 'none';
	const e = String(merged.local_model?.endpoint ?? '').trim();
	merged.local_model.endpoint = e === '' ? '' : ensureV1(e);
	return merged;
}

/** Mirror of validate()'s local_model arm + the classify invariant.
 * @param {any} merged @returns {{field:string,message:string}|null} */
export function validateLocalModel(merged) {
	const err = (/** @type {string} */ field, /** @type {string} */ message) => ({ field, message });
	const lm = merged.local_model ?? LOCAL_MODEL_DEFAULT;
	const ctx = Number(lm.ctx_tokens);
	if (!Number.isInteger(ctx) || ctx < 1024 || ctx > 32768) {
		return err('local_model', 'context size must be between 1024 and 32768 tokens (default 4096)');
	}
	const endpoint = String(lm.endpoint ?? '').trim();
	if (lm.mode === 'auto') {
		if (endpoint && !isLocalEndpoint(endpoint)) {
			return err(
				'local_model',
				'auto-configure runs a sidecar on THIS machine — the endpoint override must be a 127.0.0.1/localhost http URL. Pick Custom endpoint to use another server.'
			);
		}
	} else if (lm.mode === 'custom') {
		if (!endpoint) {
			return err(
				'local_model',
				'custom endpoint mode needs a base URL (e.g. http://127.0.0.1:1234/v1 for LM Studio, or your provider\'s https URL)'
			);
		}
		if (!endpoint.startsWith('http://') && !endpoint.startsWith('https://')) {
			return err('local_model', 'endpoint must start with http:// or https://');
		}
		try {
			new URL(endpoint);
		} catch {
			return err('local_model', 'endpoint is not a valid URL');
		}
	}
	if (Boolean(merged.capabilities?.classify) !== (lm.mode !== 'none')) {
		return err(
			'capabilities',
			'the search-grading (classify) capability follows the Local Model setting — pick a model mode or switch the model off'
		);
	}
	return null;
}

/** LlmView-shaped demo lane derived from persisted demo settings only (the
 * browser demo has no sidecar and no downloads — statuses stay honest:
 * auto never leaves model_missing, custom is never probed).
 * @param {any} s @returns {any} */
export function demoLlmView(s) {
	const lm = { ...LOCAL_MODEL_DEFAULT, ...(s.local_model ?? {}) };
	return {
		mode: lm.mode,
		status: lm.mode === 'none' ? 'off' : lm.mode === 'auto' ? 'model_missing' : 'off',
		detail: null,
		progress: null,
		model_line: null,
		model_bytes_on_disk: lm.mode === 'auto' ? 0 : null,
		cap: lm.cap,
		ctx_tokens: lm.ctx_tokens,
		endpoint: lm.mode === 'none' ? null : lm.endpoint === '' ? LLM_DEFAULT_ENDPOINT : lm.endpoint,
		endpoint_is_cloud:
			lm.mode === 'custom' && lm.endpoint !== '' && !isLocalEndpoint(lm.endpoint),
		binary_found: lm.mode !== 'auto',
		api_key_present: false,
		reused_external_server: false,
		sidecar_running: false
	};
}

/** Byte counts as human sizes (model row / freed-disk messages).
 * @param {number|null|undefined} n @returns {string} */
export function fmtBytes(n) {
	if (n == null) return '';
	if (n >= 1073741824) return `${(n / 1073741824).toFixed(2)} GB`;
	if (n >= 1048576) return `${Math.round(n / 1048576)} MB`;
	return `${n} B`;
}
