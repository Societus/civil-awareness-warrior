<script>
	import { invoke } from '@tauri-apps/api/core';
	import { listen } from '@tauri-apps/api/event';
	import SettingsDialog from '#lib/SettingsDialog.svelte';
	import {
		makeDemoStatus,
		makeDemoCommunity,
		makeDemoSettingsView,
		demoSaveSettings,
		demoStartContributor,
		demoPauseContributor
	} from '#lib/demo.js';

	const inTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
	const demoStartedAtMs = Date.now();

	/** @type {any} */
	let status = $state(null);
	/** @type {any} */
	let view = $state(null);
	/** @type {any} */
	let community = $state(null);
	let error = $state('');
	let note = $state('');
	let busy = $state(false);
	// ?settings in the URL opens the dialog on load — a dev affordance that
	// lets headless-browser verification reach the dialog without clicking.
	const openSettingsParam =
		typeof window !== 'undefined' && new URLSearchParams(window.location.search).has('settings');
	let settingsOpen = $state(openSettingsParam);
	/** Wall-clock anchor so the per-lease timers tick between polls. */
	let now = $state(Date.now());

	/**
	 * Per-lease elapsed tracking. The lib's stats handle carries no lease
	 * age, so each slot's current chunk_id gets a first-seen timestamp;
	 * elapsed is keyed on chunk_id, so verifying→executing→submitting all
	 * share one timer and a new lease resets it. Accuracy is ± the poll
	 * interval (2 s), plenty for a glance-at display.
	 * @type {Map<number, { chunkId: string | null, sinceMs: number }>}
	 */
	const leaseStarts = new Map();

	/** @param {any} st */
	function trackLeases(st) {
		const at = Date.now();
		/** @type {any[]} */
		const slots = st?.slots ?? [];
		for (const s of slots) {
			const id = s.chunk_id ?? null;
			const rec = leaseStarts.get(s.slot);
			if (!rec || rec.chunkId !== id) {
				leaseStarts.set(s.slot, { chunkId: id, sinceMs: at });
			}
		}
		for (const key of [...leaseStarts.keys()]) {
			if (!slots.some((s) => s.slot === key)) leaseStarts.delete(key);
		}
	}

	/**
	 * Pull the lib's stats handle via get_status (2 s tick, async invoke —
	 * the UI thread never blocks). Outside Tauri the synthetic demo status
	 * takes the same code path so the dashboard + settings are previewable
	 * in a browser.
	 *
	 * IMPORTANT: results land in LOCAL variables before being written to
	 * state, and all reads of them use the locals. Calling this synchronously
	 * from the polling $effect would otherwise make the effect READ the same
	 * state it WRITES (trackLeases(status)) → self-triggering →
	 * effect_update_depth_exceeded. (In the Tauri branch the awaits untrack
	 * the writes, but the demo branch has none.)
	 */
	async function refresh() {
		try {
			/** @type {any} */
			let st;
			/** @type {any} */
			let vw;
			/** @type {any} */
			let cm;
			if (inTauri) {
				st = await invoke('get_status');
				vw = await invoke('get_settings_view');
				cm = await invoke('community_stats');
			} else {
				const t = Math.max(1, Math.floor((Date.now() - demoStartedAtMs) / 1000));
				st = makeDemoStatus(t);
				vw = makeDemoSettingsView();
				cm = makeDemoCommunity();
			}
			status = st;
			view = vw;
			community = cm;
			trackLeases(st);
		} catch (e) {
			error = errText(e);
		}
	}

	$effect(() => {
		refresh();
		const poll = setInterval(refresh, 2000); // 1–2 s stats tick
		const clock = setInterval(() => (now = Date.now()), 1000); // lease timers
		return () => {
			clearInterval(poll);
			clearInterval(clock);
		};
	});

	// Tray "Settings…" opens the settings dialog (the Rust side shows the
	// window and emits this event; Tauri only). The write happens inside an
	// event callback, not the effect body, so it never re-triggers the effect.
	$effect(() => {
		if (!inTauri) return;
		/** @type {any} */
		let unlisten = null;
		let alive = true;
		listen('lh-settings', () => {
			error = '';
			note = '';
			settingsOpen = true;
		}).then((f) => {
			if (alive) unlisten = f;
			else f();
		});
		return () => {
			alive = false;
			unlisten?.();
		};
	});

	/** FieldError-shaped rejections ({field, message}) arrive as objects. */
	/** @param {any} e @returns {string} */
	function errText(e) {
		return e && typeof e === 'object' && typeof e.message === 'string' ? e.message : String(e);
	}

	/**
	 * @param {string} cmd
	 * @param {any} [args]
	 */
	async function call(cmd, args) {
		busy = true;
		error = '';
		note = '';
		try {
			const r = args === undefined ? await invoke(cmd) : await invoke(cmd, args);
			if (typeof r === 'string') note = r;
			await refresh();
		} catch (e) {
			error = errText(e);
			await refresh();
		} finally {
			busy = false;
		}
	}

	/** Demo-mode Start/Pause: drive the synthetic session, then re-render. */
	/** @param {() => void} fn */
	function demoRunAction(fn) {
		error = '';
		note = '';
		try {
			fn();
			refresh();
		} catch (e) {
			error = errText(e);
		}
	}

	const phase = $derived(status?.phase ?? 'idle');
	/** @type {any[]} */
	const slots = $derived(status?.slots ?? []);
	const leasedSlots = $derived(slots.filter((s) => s.chunk_id != null));
	const coordinatorPill = $derived(
		status?.coordinator_ok === true ? 'ok' : status?.coordinator_ok === false ? 'down' : ''
	);

	const activityText = $derived.by(() => {
		if (phase === 'paused') {
			if (status?.pause_reason === 'schedule') {
				return 'Paused — outside your active window; resumes automatically when the window opens';
			}
			return 'Paused — leases released, nothing being worked';
		}
		if (phase === 'stopping') return 'Stopping — in-flight leases settling';
		if (phase === 'starting') return 'Starting the run loop…';
		if (leasedSlots.length > 0) {
			const first = leasedSlots[0];
			const parts = [first.chunk_type ?? 'chunk', first.target_key ?? shortId(first.chunk_id)];
			const extra = leasedSlots.length > 1 ? ` (+${leasedSlots.length - 1} more)` : '';
			return `Leased — ${parts.filter(Boolean).join(' · ')}${extra}`;
		}
		if (phase === 'running') return 'Idle — polling the coordinator for work';
		return 'Idle — no run in progress';
	});

	// Separator-joined meta line computed in JS: Svelte whitespace collapse
	// eats the spaces around inline {#if} blocks and glues fragments.
	const fineLine = $derived.by(() => {
		const parts = [
			status ? (status.dry_run ? 'dry run — staged locally, nothing submitted' : 'live run') : null,
			status?.staging ?? null,
			status ? `${status.concurrency ?? '—'} slots` : null
		];
		if (status?.schedule_enabled) {
			const sched = view?.schedule;
			const windowText = sched ? `active window ${sched.active_from}–${sched.active_until}` : 'active window';
			parts.push(status.schedule_in_window ? `${windowText} · now inside` : `${windowText} · now outside`);
		}
		return parts.filter(Boolean).join(' · ') || '—';
	});

	function primaryAction() {
		if (phase === 'running' || phase === 'paused' || phase === 'error') {
			const isRun = phase === 'running';
			if (inTauri) {
				call(isRun ? 'pause_contributor' : 'start_contributor');
			} else {
				demoRunAction(isRun ? demoPauseContributor : demoStartContributor);
			}
		} else if (phase === 'starting') {
			error = 'Startup already in progress.';
		} else if (inTauri) {
			call('start_contributor');
		} else {
			demoRunAction(demoStartContributor);
		}
	}

	/** @returns {string} */
	function primaryLabel() {
		return phase === 'running'
			? 'Pause'
			: phase === 'stopping'
				? 'Pausing…'
				: phase === 'paused'
					? 'Resume'
					: phase === 'error'
						? 'Retry'
						: 'Start';
	}

	/** @param {Event & { currentTarget: HTMLInputElement }} e */
	async function toggleDryRun(e) {
		error = '';
		try {
			const input = { dry_run: e.currentTarget.checked };
			if (inTauri) {
				await invoke('save_settings', { input });
				note = 'Dry-run setting applies on the next start.';
			} else {
				demoSaveSettings(input);
				note = 'Dry-run setting applies on the next start (demo).';
			}
			await refresh();
		} catch (err) {
			error = errText(err);
		}
	}

	function quit() {
		call('quit_app');
	}

	/** @param {number} slotId */
	function leaseElapsed(slotId) {
		const rec = leaseStarts.get(slotId);
		if (!rec || rec.chunkId == null) return null;
		return Math.max(0, now - rec.sinceMs);
	}

	/** @param {any} s @returns {string} */
	function slotText(s) {
		const idPart = s.target_key ?? (s.chunk_id ? shortId(s.chunk_id) : null);
		return [idPart, s.chunk_type].filter(Boolean).join(' · ') || 'waiting for a lease';
	}

	/** @param {number | null | undefined} n */
	function fmtBytes(n) {
		if (n == null) return '—';
		const units = ['B', 'KB', 'MB', 'GB', 'TB'];
		let i = 0;
		let v = n;
		while (v >= 1024 && i < units.length - 1) {
			v /= 1024;
			i += 1;
		}
		return `${v.toFixed(i ? 1 : 0)} ${units[i]}`;
	}

	/** @param {number | null | undefined} s */
	function fmtUptime(s) {
		if (s == null) return '—';
		const d = Math.floor(s / 86400);
		const h = Math.floor((s % 86400) / 3600);
		const m = Math.floor((s % 3600) / 60);
		const sec = s % 60;
		if (d) return `${d}d ${h}h`;
		if (h) return `${h}h ${m}m`;
		if (m) return `${m}m ${sec}s`;
		return `${sec}s`;
	}

	/** mm:ss (h:mm:ss past an hour) — the live per-lease timer. @param {number} ms */
	function fmtElapsed(ms) {
		const sec = Math.floor(ms / 1000);
		const h = Math.floor(sec / 3600);
		const m = Math.floor((sec % 3600) / 60);
		const s = sec % 60;
		return h
			? `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`
			: `${m}:${String(s).padStart(2, '0')}`;
	}

	/** @param {number} ms */
	function fmtClock(ms) {
		return new Date(ms).toLocaleTimeString([], { hour12: false });
	}

	/** @param {number | null | undefined} n */
	function fmtNum(n) {
		return (n ?? 0).toLocaleString();
	}

	/** @param {string | null | undefined} id */
	function shortId(id) {
		if (!id) return '—';
		return id.length > 12 ? `${id.slice(0, 12)}…` : id;
	}
</script>

<main>
	<header>
		<div class="brand">
			<span class="logo" aria-hidden="true">⌖</span>
			<h1>Civil Awareness Warrior</h1>
		</div>
		<span class="conn">
			<span class="pill {coordinatorPill}" role="status">
				{status?.coordinator_ok === true ? 'online' : status?.coordinator_ok === false ? 'down' : 'checking…'}
			</span>
			<span class="conn-when">
				{status?.last_health_ok_ms ? `last poll ${fmtClock(status.last_health_ok_ms)}` : 'contacting coordinator'}
			</span>
			<button class="icon-btn" aria-label="Open settings" title="Settings" onclick={() => (settingsOpen = true)}>
				<span aria-hidden="true">⚙</span>
			</button>
		</span>
	</header>

	{#if error || status?.error}
		<div class="banner" role="alert">{error || status.error}</div>
	{:else if note}
		<div class="note" role="status">{note}</div>
	{/if}

	<section class="card" aria-label="Current activity">
		<h2>Activity</h2>
		<div class="status-row">
			<span class="pill {phase}">{phase}</span>
			<span class="activity">{activityText}</span>
		</div>
		<p class="fine">{fineLine}</p>
		{#if slots.length}
			<div class="leases">
				{#each slots as s (s.slot)}
					{@const elapsed = leaseElapsed(s.slot)}
					<div class="slot">
						<span class="pill {s.activity}">{s.activity} #{s.slot}</span>
						<span class="ids">{slotText(s)}</span>
						{#if elapsed != null}
							<span class="elapsed">{fmtElapsed(elapsed)}</span>
						{/if}
					</div>
				{/each}
			</div>
		{:else}
			<p class="empty">No slots yet — Start begins the run loop.</p>
		{/if}
	</section>

	<section class="card" aria-label="Contribution totals">
		<h2>My contribution</h2>
		<div class="grid4">
			<div class="tile">
				<div class="section-label">Completed</div>
				<div class="num">{fmtNum(status?.stats?.chunks_completed)}</div>
			</div>
			<div class="tile">
				<div class="section-label">Verified</div>
				<div class="num">{fmtNum(status?.stats?.chunks_verified)}</div>
			</div>
			<div class="tile">
				<div class="section-label">Failed</div>
				<div class="num">{fmtNum(status?.stats?.chunks_failed)}</div>
			</div>
			<div class="tile">
				<div class="section-label">Bytes staged</div>
				<div class="num">{fmtBytes(status?.stats?.bytes_staged)}</div>
			</div>
		</div>
		<div class="meta-rows">
			<div class="meta-row">
				<span>Session uptime</span>
				<b>{fmtUptime(status?.stats?.uptime_secs)}</b>
			</div>
			<div class="meta-row">
				<span>Total runtime, all sessions</span>
				<b>{fmtUptime(status?.total_runtime_secs)}</b>
			</div>
		</div>
	</section>

	{#if community}
		<p class="community" role="status">
			Community total —
			<b>{fmtNum(community.chunks_verified)}</b> chunks verified ·
			<b>{fmtNum(community.chunks_pending)}</b> pending ·
			<b>{fmtNum(community.warriors_online)}/{fmtNum(community.warriors_total)}</b> volunteers online
		</p>
	{/if}

	<div class="controls">
		<button class="primary" onclick={primaryAction} disabled={busy || phase === 'starting' || phase === 'stopping'}>
			{primaryLabel()}
		</button>
		<button class="danger" onclick={quit} disabled={busy}>Quit</button>
	</div>

	<div class="toggle-row">
		<label for="dryrun">Dry run (no results submitted)</label>
		<input id="dryrun" type="checkbox" checked={status?.dry_run ?? true} onchange={toggleDryRun} />
	</div>

	<footer>
		<span>app v{status?.gui_version ?? '0.1.0'} · lib v{status?.client_version ?? '—'}</span>
		{#if inTauri}
			<span>closing minimizes to tray</span>
		{:else}
			<span class="demo-tag" role="status">demo mode — synthetic data (run npm run tauri dev for live values)</span>
		{/if}
	</footer>
</main>

{#if settingsOpen}
	<SettingsDialog {view} {status} onClose={() => (settingsOpen = false)} onChanged={refresh} />
{/if}
