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
	return null;
}

const CAP_KEYS = ['examination', 'ingest', 'alt_source', 'state_walk', 'search'];
