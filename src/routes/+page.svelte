<script>
	import { invoke } from '@tauri-apps/api/core';
	import { makeDemoStatus, makeDemoCommunity, makeDemoSettingsView } from '#lib/demo.js';

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
	/** Plain flag (NOT $state): seeding must not feed the polling effect's
	 *  dependency graph. */
	let settingsSeeded = false;
	/** Wall-clock anchor so the per-lease timers tick between polls. */
	let now = $state(Date.now());

	let form = $state({ coordinator_url: '', public_key_hex: '', concurrency: 2 });

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
	 * takes the same code path so the dashboard is previewable in a browser.
	 *
	 * IMPORTANT: results land in LOCAL variables before being written to
	 * state, and all reads of them use the locals. Calling this synchronously
	 * from the polling $effect would otherwise make the effect READ the same
	 * state it WRITES (trackLeases(status), seed guard) → self-triggering →
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
			if (!settingsSeeded && vw) {
				form.coordinator_url = vw.coordinator_url;
				form.concurrency = vw.concurrency;
				settingsSeeded = true;
			}
		} catch (e) {
			error = String(e);
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
			error = String(e);
			await refresh();
		} finally {
			busy = false;
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
		if (phase === 'paused') return 'Paused — leases released, nothing being worked';
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

	function primaryAction() {
		if (phase === 'running' || phase === 'paused' || phase === 'error') {
			call(phase === 'running' ? 'pause_contributor' : 'start_contributor');
		} else if (phase === 'starting') {
			error = 'Startup already in progress.';
		} else {
			call('start_contributor');
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
			await invoke('save_settings', { input: { dry_run: e.currentTarget.checked } });
			note = 'Dry-run setting applies on the next start.';
			await refresh();
		} catch (err) {
			error = String(err);
		}
	}

	async function saveSettings() {
		const input = {
			coordinator_url: form.coordinator_url.trim() || undefined,
			concurrency: Number(form.concurrency) || undefined,
			public_key_hex: form.public_key_hex.trim() || undefined
		};
		await call('save_settings', { input });
		note = 'Settings saved.';
		form.public_key_hex = '';
	}

	async function register() {
		await call('register_client');
	}

	async function testConnection() {
		await call('check_coordinator');
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
			<h1>LandHunter Contributor</h1>
		</div>
		<span class="conn" role="status">
			<span class="pill {coordinatorPill}">
				{status?.coordinator_ok === true ? 'online' : status?.coordinator_ok === false ? 'down' : 'checking…'}
			</span>
			<span class="conn-when">
				{status?.last_health_ok_ms ? `last poll ${fmtClock(status.last_health_ok_ms)}` : 'contacting coordinator'}
			</span>
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
		<p class="fine">
			{status?.dry_run ? 'dry run — staged locally, nothing submitted' : 'live run'} ·
			{status?.staging ?? '—'} ·
			{status?.concurrency ?? '—'} slots
		</p>
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
		<button onclick={register} disabled={busy || phase === 'running' || phase === 'starting'}>
			Register
		</button>
		<button class="danger" onclick={quit} disabled={busy}>Quit</button>
	</div>

	<div class="toggle-row">
		<label for="dryrun">Dry run (no results submitted)</label>
		<input id="dryrun" type="checkbox" checked={status?.dry_run ?? true} onchange={toggleDryRun} />
		<button onclick={testConnection} disabled={busy} style="margin-left:auto;padding:4px 8px;font-size:11.5px;">Test</button>
	</div>

	<details>
		<summary>Settings &amp; identity</summary>
		<div class="form">
			<div>
				<label for="coord">Coordinator URL</label>
				<input id="coord" type="text" bind:value={form.coordinator_url} />
			</div>
			<div>
				<label for="pubkey">Contract public key (ed25519, 64 hex)</label>
				<input
					id="pubkey"
					type="password"
					bind:value={form.public_key_hex}
					placeholder={view?.public_key_configured ? `unchanged (…${view.public_key_tail})` : '64-char hex, or set LH_PUBLIC_KEY_HEX'}
					autocomplete="off"
				/>
			</div>
			<div>
				<label for="conc">Concurrency (1–8 slots)</label>
				<input id="conc" type="number" min="1" max="8" bind:value={form.concurrency} />
			</div>
			<button onclick={saveSettings} disabled={busy}>Save settings</button>
			<p class="fine">
				identity: {view?.credentials_present ? `registered as ${shortId(view.client_id)}` : 'not registered yet'}<br />
				credentials: {view?.credentials_path ?? '—'}
			</p>
		</div>
	</details>

	<footer>
		<span>app v{status?.gui_version ?? '0.1.0'} · lib v{status?.client_version ?? '—'}</span>
		{#if inTauri}
			<span>closing minimizes to tray</span>
		{:else}
			<span class="demo-tag" role="status">demo mode — synthetic data (run npm run tauri dev for live values)</span>
		{/if}
	</footer>
</main>
