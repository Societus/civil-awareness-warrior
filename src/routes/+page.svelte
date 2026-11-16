<script>
	import { invoke } from '@tauri-apps/api/core';

	const inTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

	/** @type {any} */
	let status = $state(null);
	/** @type {any} */
	let view = $state(null);
	let error = $state('');
	let note = $state('');
	let busy = $state(false);
	let settingsLoaded = $state(false);

	let form = $state({ coordinator_url: '', public_key_hex: '', concurrency: 2 });

	async function refresh() {
		if (!inTauri) return;
		try {
			status = await invoke('get_status');
			view = await invoke('get_settings_view');
			if (!settingsLoaded && view) {
				form.coordinator_url = view.coordinator_url;
				form.concurrency = view.concurrency;
				settingsLoaded = true;
			}
		} catch (e) {
			error = String(e);
		}
	}

	$effect(() => {
		if (!inTauri) return;
		refresh();
		const t = setInterval(refresh, 2000);
		return () => clearInterval(t);
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

	const primaryLabel = $derived(
		phase === 'running'
			? 'Pause'
			: phase === 'stopping'
				? 'Pausing…'
				: phase === 'paused'
					? 'Resume'
					: phase === 'error'
						? 'Retry'
						: 'Start'
	);

	function primaryAction() {
		if (phase === 'running' || phase === 'paused' || phase === 'error') {
			call(phase === 'running' ? 'pause_contributor' : 'start_contributor');
		} else if (phase === 'starting') {
			error = 'Startup already in progress.';
		} else {
			call('start_contributor');
		}
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
		const h = Math.floor(s / 3600);
		const m = Math.floor((s % 3600) / 60);
		const sec = s % 60;
		if (h) return `${h}h ${m}m`;
		if (m) return `${m}m ${sec}s`;
		return `${sec}s`;
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
		<span
			class="pill {status?.coordinator_ok === true ? 'ok' : status?.coordinator_ok === false ? 'error' : ''}"
			role="status"
		>
			{status?.coordinator_ok === true ? 'coordinator online' : status?.coordinator_ok === false ? 'coordinator down' : 'checking…'}
		</span>
	</header>

	{#if !inTauri}
		<div class="banner" role="alert">Not running inside Tauri — use <code>npm run tauri dev</code>.</div>
	{/if}

	{#if error || status?.error}
		<div class="banner" role="alert">{error || status.error}</div>
	{:else if note}
		<div class="note" role="status">{note}</div>
	{/if}

	<section class="card" aria-label="Run status">
		<div class="status-row">
			<span class="pill {phase}">{phase}</span>
			<span class="fine">
				{status?.dry_run ? 'dry run — no results submitted' : 'live run'} ·
				{status?.staging ?? '—'} ·
				{status?.concurrency ?? '—'} slots
			</span>
		</div>
	</section>

	<section class="card" aria-label="Session totals">
		<h2>Session</h2>
		<div class="grid4">
			<div class="tile">
				<div class="section-label">Completed</div>
				<div class="num">{status?.stats?.chunks_completed ?? 0}</div>
			</div>
			<div class="tile">
				<div class="section-label">Verified</div>
				<div class="num">{status?.stats?.chunks_verified ?? 0}</div>
			</div>
			<div class="tile">
				<div class="section-label">Failed</div>
				<div class="num">{status?.stats?.chunks_failed ?? 0}</div>
			</div>
			<div class="tile">
				<div class="section-label">Staged</div>
				<div class="num">{fmtBytes(status?.stats?.bytes_staged)}</div>
			</div>
		</div>
	</section>

	<section class="card" aria-label="Current leases">
		<h2>Leases</h2>
		{#if status?.slots?.length}
			<div class="leases">
				{#each status.slots as slot (slot.slot)}
					<div class="slot">
						<span class="pill {slot.activity}">{slot.activity} #{slot.slot}</span>
						<span class="ids">
							{#if slot.target_key}
								<b>{slot.target_key}</b> · {slot.chunk_type ?? '—'}
							{:else if slot.chunk_id}
								<b>{shortId(slot.chunk_id)}</b> · {slot.chunk_type ?? '—'}
							{:else}
								{slot.chunk_type ?? 'no lease held'}
							{/if}
						</span>
					</div>
				{/each}
			</div>
		{:else}
			<p class="empty">No active lease.</p>
		{/if}
	</section>

	<div class="controls">
		<button class="primary" onclick={primaryAction} disabled={busy || phase === 'starting' || phase === 'stopping'}>
			{primaryLabel}
		</button>
		<button onclick={register} disabled={busy || phase === 'running' || phase === 'starting' || !view?.credentials_present && false}>
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
		<span>uptime {fmtUptime(status?.stats?.uptime_secs)}</span>
	</footer>
</main>
