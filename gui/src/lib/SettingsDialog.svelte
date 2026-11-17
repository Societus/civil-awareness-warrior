<script>
	import { invoke } from '@tauri-apps/api/core';
	import { CAPABILITIES } from '#lib/capabilities.js';
	import { scheduleInWindow, LOCAL_MODEL_MODES, LOCAL_MODEL_CAPS, LOCAL_MODEL_BUNDLED, LOCAL_MODEL_BUNDLED_DEFAULT_ID, bundledModelCaption, fmtBytes, fmtGiB } from '#lib/settings-util.js';
	import { demoSaveSettings, demoRegister, demoUnregister } from '#lib/demo.js';

	const inTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

	// Defaults are typed `any` (not a $props() cast): a recognized $props()
	// pattern keeps svelte2tsx's component-props extraction working, so the
	// parent's <SettingsDialog {view} {status} .../> attributes type-check.
	let {
		view = /** @type {any} */ (null),
		status = /** @type {any} */ (null),
		onClose = /** @type {any} */ (null),
		onChanged = /** @type {any} */ (null)
	} = $props();

	const phase = $derived(status?.phase ?? 'idle');
	const loopActive = $derived(phase === 'running' || phase === 'starting' || phase === 'stopping');

	/**
	 * Snapshot the persisted settings into the draft ONCE at dialog mount.
	 * Deliberately not a $derived: poll updates to `view` while the dialog
	 * is open must NOT clobber unsaved edits — capturing the initial value
	 * is the requirement, so the reactive read happens in a plain one-shot
	 * init outside any rune initializer.
	 * @param {any} v
	 * @returns {any}
	 */
	function seedDraft(v) {
		const c = v?.capabilities ?? {};
		const sc = v?.schedule ?? {};
		return {
			coordinator_url: v?.coordinator_url ?? '',
			public_key_hex: '',
			concurrency: v?.concurrency ?? 2,
			dry_run: v?.dry_run ?? true,
			capabilities: {
				examination: c.examination ?? true,
				ingest: c.ingest ?? true,
				alt_source: c.alt_source ?? true,
				state_walk: c.state_walk ?? true,
				search: c.search ?? true
			},
			schedule: {
				enabled: sc.enabled ?? false,
				active_from: sc.active_from ?? '09:00',
				active_until: sc.active_until ?? '17:00'
			},
			// Launch-at-login intent (the Rust side applies the OS entry on
			// save and re-points it every startup — see README autostart).
			autostart: v?.autostart ?? false,
			local_model: {
				mode: v?.local_model?.mode ?? 'none',
				cap: v?.local_model?.cap ?? 'cpu_third',
				model_id: v?.local_model?.model_id ?? LOCAL_MODEL_BUNDLED_DEFAULT_ID,
				endpoint: v?.local_model?.endpoint ?? '',
				llama_server_bin: v?.local_model?.llama_server_bin ?? '',
				model_name: v?.local_model?.model_name ?? '',
				ctx_tokens: v?.local_model?.ctx_tokens ?? 4096
			},
			// API keys never round-trip through any settings view; this field is
			// for ENTERING a new key only (the backend stores it out-of-band in
			// llm_api_key.json and never echoes it back).
			llm_api_key: ''
		};
	}

	// Intentional one-shot capture of the persisted view (see seedDraft doc):
	// a $derived would re-seed on every 2 s poll and clobber unsaved edits, so
	// the compiler hint on the next line is the honest opt-out.
	// svelte-ignore state_referenced_locally
	const initialDraft = seedDraft(view);
	/** @type {any} */
	let draft = $state(initialDraft);

	/** field → message, filled from FieldError-shaped rejections. @type {Record<string, string>} */
	let errors = $state({});
	let note = $state('');
	let busy = $state(false);
	let armedUnregister = $state(false);
	/** @type {ReturnType<typeof setTimeout> | null} */
	let disarmTimer = null; // plain let: a timer, not reactive state
	/** Wall clock, refreshed every 30 s so the "inside/outside window" hint is honest. */
	let nowMs = $state(Date.now());

	/** @type {HTMLDivElement | null} */
	let panel = $state(null);

	$effect(() => {
		panel?.focus();
		const tick = setInterval(() => (nowMs = Date.now()), 30000);
		const onKey = (/** @type {KeyboardEvent} */ e) => {
			if (e.key === 'Escape') onClose?.();
		};
		window.addEventListener('keydown', onKey);
		return () => {
			clearInterval(tick);
			window.removeEventListener('keydown', onKey);
			if (disarmTimer) clearTimeout(disarmTimer);
		};
	});

	/** FieldError wire shape ({field, message}) or a plain string error. */
	/** @param {any} e @returns {Record<string, string>} */
	function toFieldErrors(e) {
		if (e && typeof e === 'object' && typeof e.field === 'string') {
			return { [e.field]: String(e.message ?? e) };
		}
		return { form: String(e) };
	}

	async function save() {
		busy = true;
		note = '';
		errors = {};
		const key = draft.public_key_hex.trim();
		const input = {
			coordinator_url: draft.coordinator_url,
			public_key_hex: /** @type {string | undefined} */ (key === '' ? undefined : key),
			concurrency: Number(draft.concurrency),
			dry_run: draft.dry_run,
			capabilities: {
				examination: draft.capabilities.examination,
				ingest: draft.capabilities.ingest,
				alt_source: draft.capabilities.alt_source,
				state_walk: draft.capabilities.state_walk,
				search: draft.capabilities.search
			},
			schedule: {
				enabled: draft.schedule.enabled,
				active_from: draft.schedule.active_from,
				active_until: draft.schedule.active_until
			},
			autostart: draft.autostart,
			local_model: { ...draft.local_model },
			// Key handling: sent ONLY when a new key was typed (replace) or
			// Remove key was pressed ('' → backend deletes the stored file).
			// Untouched saves omit it entirely.
			llm_api_key: removeLlmKey ? '' : draft.llm_api_key.trim() === '' ? undefined : draft.llm_api_key.trim()
		};
		try {
			const next = inTauri ? await invoke('save_settings', { input }) : demoSaveSettings(input);
			// Re-seed from the persisted view (canonical clamped values).
			draft = {
				coordinator_url: next.coordinator_url,
				public_key_hex: '',
				concurrency: next.concurrency,
				dry_run: next.dry_run,
				capabilities: { ...next.capabilities },
				schedule: { ...next.schedule },
				autostart: next.autostart,
				local_model: { ...next.local_model },
				llm_api_key: ''
			};
			removeLlmKey = false;
			note = 'Settings saved. Concurrency, capabilities and dry-run apply on the next start; the run window, the launch-at-login entry and the local-model lane are reconciled immediately (a live sidecar restarts with the new caps when needed).';
			await onChanged?.();
		} catch (e) {
			errors = toFieldErrors(e);
		} finally {
			busy = false;
		}
	}

	async function register() {
		busy = true;
		note = '';
		errors = {};
		try {
			note = inTauri ? String(await invoke('register_client')) : demoRegister();
			await onChanged?.();
		} catch (e) {
			errors = toFieldErrors(e);
		} finally {
			busy = false;
		}
	}

	/** Two-step destructive action: first click arms, second click executes. */
	async function unregister() {
		if (!armedUnregister) {
			armedUnregister = true;
			if (disarmTimer) clearTimeout(disarmTimer);
			disarmTimer = setTimeout(() => (armedUnregister = false), 5000);
			return;
		}
		if (disarmTimer) clearTimeout(disarmTimer);
		armedUnregister = false;
		busy = true;
		note = '';
		errors = {};
		try {
			note = inTauri ? String(await invoke('unregister_identity')) : demoUnregister();
			await onChanged?.();
		} catch (e) {
			errors = toFieldErrors(e);
		} finally {
			busy = false;
		}
	}

	async function testConnection() {
		if (!inTauri) {
			note = 'Connection test is only available in the real app — demo mode has no network side.';
			return;
		}
		busy = true;
		note = '';
		errors = {};
		try {
			note = String(await invoke('check_coordinator'));
			await onChanged?.();
		} catch (e) {
			errors = toFieldErrors(e);
		} finally {
			busy = false;
		}
	}

	const windowNow = $derived(scheduleInWindow(draft.schedule, new Date(nowMs)));

	// Local model (t_3067589d). `llm` is the LIVE lane view: the parent
	// polls get_status/get_settings_view every 2 s, so model status and
	// download progress refresh here while the dialog stays open. The draft
	// (what the user is about to save) drives which controls show.
	const llm = $derived(status?.llm ?? view?.llm ?? null);
	const lmMode = $derived(draft.local_model.mode);
	let removeLlmKey = $state(false);

	const CAP_HINTS = /** @type {Record<string, string>} */ ({
		gpu: 'Offload every layer the GPU can take (--n-gpu-layers 999). No GPU? llama-server falls back to CPU — this cap never hard-fails.',
		cpu_third: 'CPU-only (--n-gpu-layers 0), threads capped to a third of your logical cores (--threads max(1, floor(cores/3))).',
		cpu_half: 'CPU-only (--n-gpu-layers 0), threads capped to half your logical cores (--threads max(1, floor(cores/2))).'
	});
	const lmCapHint = $derived(CAP_HINTS[draft.local_model.cap] ?? '');

	// Bundled model menu: SettingsView.bundled_models is the ground truth
	// (sidecar::bundled_models in Rust); the settings-util mirror only
	// covers the browser demo, which has no Rust behind it.
	const bundledModels = $derived(view?.bundled_models ?? LOCAL_MODEL_BUNDLED);

	// "Can I AI?" hardware advisor (t_10daf382): click-only RAM/VRAM probe +
	// suggestion matrix. Results render here and nowhere else — the check
	// never runs at startup or in the background, and it NEVER changes
	// settings by itself (the tier-2 deep link only preselects the UNSAVED
	// form draft; saving stays a human act).
	/** @type {any} */
	let hwAdvice = $state(null);
	let hwBusy = $state(false);

	async function runHwCheck() {
		if (!inTauri) {
			note = 'Can I AI? reads real hardware — only available in the real app; the browser demo mirrors the UI shape only.';
			return;
		}
		hwBusy = true;
		hwAdvice = null;
		note = '';
		errors = {};
		try {
			hwAdvice = await invoke('check_hardware');
		} catch (e) {
			hwAdvice = null;
			errors = toFieldErrors(e);
		} finally {
			hwBusy = false;
		}
	}

	/** AdviceLine → render parts: AdviceLink splices inline at the
	 *  {{link}} marker (mirrors hardware.rs INLINE_LINK_MARKER); otherwise
	 *  the anchor is appended after the sentence.
	 *  @param {any} line @returns {Array<{ t?: string, a?: any }>} */
	function lineParts(line) {
		const marker = '{{link}}';
		const text = String(line.text ?? '');
		if (line.link && text.includes(marker)) {
			/** @type {Array<{ t?: string, a?: any }>} */
			const out = [];
			const chunks = text.split(marker);
			chunks.forEach((chunk, i) => {
				if (chunk) out.push({ t: chunk });
				if (i < chunks.length - 1) out.push({ a: line.link });
			});
			return out;
		}
		/** @type {Array<{ t?: string, a?: any }>} */
		const out = [{ t: text }];
		if (line.link) out.push({ a: line.link });
		return out;
	}

	/** Open an advisor link in the SYSTEM browser via the opener plugin
	 *  (capability-scoped to huggingface.co only). The anchor href stays
	 *  real for the demo and as a copyable fallback; if the plugin rejects,
	 *  the URL is surfaced verbatim for opening by hand.
	 *  @param {MouseEvent} e @param {string} url */
	async function openLink(e, url) {
		if (!inTauri) return; // demo/browser: default anchor behaviour
		e.preventDefault();
		try {
			await invoke('plugin:opener|open_url', { url });
		} catch {
			note = `Could not open the link in-app — copy it by hand: ${url}`;
		}
	}

	/** Tier-2 deep link: preselect Custom endpoint in the FORM only —
	 *  nothing saves until Save settings is pressed (advisor doctrine:
	 *  purely advisory, never changes settings). */
	function openCustomEndpoint() {
		draft.local_model.mode = 'custom';
		note = 'Custom endpoint preselected in the form above — nothing is saved until you press Save settings.';
		// One tick so the custom form renders, then focus its endpoint input.
		setTimeout(() => document.getElementById('lm-endpoint-c')?.focus(), 0);
	}

	/** Result of check_binary_update (backend shape), null until asked. */
	/** @type {{installed: boolean, current_tag: string, latest_tag: string, latest_asset: string, latest_bytes: number, update_available: boolean, backend: string, override_set: boolean} | null} */
	let binaryUpdate = $state(null);

	const lmStatusText = $derived.by(() => {
		switch (llm?.status) {
			case 'model_missing':
				return 'not downloaded';
			case 'downloading':
				return `downloading… ${llm?.progress?.percent ?? 0}%`;
			case 'model_error':
				return 'download failed';
			case 'model_ready':
				return 'ready on disk — Start boots the sidecar';
			case 'binary_downloading':
				return `fetching the pinned llama.cpp release… ${llm?.progress?.percent ?? 0}%`;
			case 'binary_missing':
				return llm?.binary_fetch_supported === false
					? 'model ready — no llama-server found, and this OS/arch is outside the auto-install matrix: install llama.cpp yourself, set its path below, or switch to Custom endpoint'
					: 'model ready — no llama-server yet; Start fetches the pinned llama.cpp build automatically (sha256-verified before use)';
			case 'starting':
				return 'starting sidecar…';
			case 'ready':
				return `ready — ${llm?.model_line ?? 'local model'} · ctx ${draft.local_model.ctx_tokens}${llm?.reused_external_server ? ' — reusing your own server at that endpoint (the app never kills it)' : ''}`;
			case 'unhealthy':
				return 'not answering — Start will refuse until fixed';
			case 'off':
				return 'off';
			default:
				return '—';
		}
	});

	/** download / check / free-disk backend commands (real app only). */
	async function lmAction(/** @type {string} */ cmd, /** @type {string} */ label) {
		if (!inTauri) {
			note = `${label} is only available in the real app — the browser demo mirrors the UI shape only.`;
			return;
		}
		busy = true;
		note = '';
		errors = {};
		try {
			note = String(await invoke(cmd));
			await onChanged?.();
		} catch (e) {
			errors = toFieldErrors(e);
		} finally {
			busy = false;
		}
	}

	/** Ask the backend to re-resolve upstream llama.cpp releases and compare
	 *  with what is installed. Reports only — installs nothing. */
	async function checkBinaryUpdates() {
		if (!inTauri) {
			note = 'binary update check is only available in the real app.';
			return;
		}
		busy = true;
		note = '';
		errors = {};
		try {
			/** @type {{installed: boolean, current_tag: string, latest_tag: string, latest_asset: string, latest_bytes: number, update_available: boolean, backend: string, override_set: boolean}} */
			const res = await invoke('check_binary_update');
			binaryUpdate = res;
			note = res.update_available
				? `llama.cpp ${res.latest_tag} is available (${res.backend} build, ${fmtBytes(res.latest_bytes)}). You ${res.installed ? `run ${res.current_tag}` : `have nothing installed yet — the pinned default is ${res.current_tag}`}.${res.override_set ? ' Note: a llama-server path override is set, so what actually runs stays yours until you clear it.' : ''}`
				: `no newer release than ${res.installed ? `installed ${res.current_tag}` : `the pinned default ${res.current_tag}`}.`;
		} catch (e) {
			binaryUpdate = null;
			errors = toFieldErrors(e);
		} finally {
			busy = false;
		}
	}

	async function installBinaryUpdate() {
		await lmAction('install_binary_update', 'Binary update');
		binaryUpdate = null;
	}

	async function removeInstalledBinary() {
		await lmAction('remove_installed_binary', 'Remove llama-server');
		binaryUpdate = null;
	}
</script>

<div class="settings-overlay">
	<button class="scrim" aria-label="Close settings" onclick={() => onClose?.()}></button>
	<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
	<div class="settings-panel" role="dialog" aria-modal="true" aria-labelledby="settings-title" tabindex="-1" bind:this={panel}>
		<div class="settings-head">
			<h2 id="settings-title">Settings</h2>
			<button class="icon-btn" aria-label="Close settings" onclick={() => onClose?.()}>✕</button>
		</div>

		{#if errors.form}
			<div class="banner" role="alert">{errors.form}</div>
		{:else if note}
			<div class="note" role="status">{note}</div>
		{/if}

		<section class="settings-section" aria-labelledby="caps-title">
			<h3 class="section-label" id="caps-title">Capabilities</h3>
			<p class="fine" id="caps-help">Which chunk types this machine offers. Descriptions match the coordinator's landing page.</p>
			{#if errors.capabilities}
				<p class="field-error" role="alert">{errors.capabilities}</p>
			{/if}
			<div class="cap-list" role="group" aria-describedby="caps-help">
				{#each CAPABILITIES as cap (cap.id)}
					<label class="cap-row" for={cap.id}>
						<input type="checkbox" id={cap.id} bind:checked={draft.capabilities[cap.id]} aria-describedby={`${cap.id}-desc`} />
						<span class="cap-text">
							<b>{cap.label}</b>
							<span class="cap-desc" id={`${cap.id}-desc`}>{cap.description}</span>
						</span>
					</label>
				{/each}
			</div>
			<p class="fine">
				Search-grading (classify) is not a manual toggle — it unlocks automatically when a local model below is
				configured and its endpoint is healthy.
			</p>
		</section>

		<section class="settings-section" aria-labelledby="conc-title">
			<h3 class="section-label" id="conc-title">Concurrency</h3>
			<div class="concurrency-row">
				<label for="conc-range">Parallel slots</label>
				<input id="conc-range" type="range" min="1" max="8" step="1" bind:value={draft.concurrency} />
				<output for="conc-range" class="conc-num">{draft.concurrency}</output>
			</div>
			<p class="fine">1–8 simultaneous leases. Applies on the next start.</p>
			{#if errors.concurrency}
				<p class="field-error" role="alert">{errors.concurrency}</p>
			{/if}
		</section>

		<section class="settings-section" aria-labelledby="sched-title">
			<h3 class="section-label" id="sched-title">Schedule</h3>
			<div class="toggle-row">
				<label for="sched-enabled">Limit to an active window</label>
				<input id="sched-enabled" type="checkbox" bind:checked={draft.schedule.enabled} />
			</div>
			{#if draft.schedule.enabled}
				<div class="window-row">
					<div class="window-field">
						<label for="sched-from">Active from</label>
						<input id="sched-from" type="time" bind:value={draft.schedule.active_from} />
					</div>
					<div class="window-field">
						<label for="sched-until">Active until</label>
						<input id="sched-until" type="time" bind:value={draft.schedule.active_until} />
					</div>
				</div>
				<p class="fine">
					Currently {windowNow ? 'inside' : 'outside'} your window. Outside the window the loop pauses politely —
					in-flight leases may finish — and resumes automatically when the window opens again.
				</p>
			{:else}
				<p class="fine">Always active (default).</p>
			{/if}
			{#if errors.schedule}
				<p class="field-error" role="alert">{errors.schedule}</p>
			{/if}
		</section>

		<section class="settings-section" aria-labelledby="autostart-title">
			<h3 class="section-label" id="autostart-title">System</h3>
			<div class="toggle-row">
				<label for="autostart-enabled">Launch at login</label>
				<input id="autostart-enabled" type="checkbox" bind:checked={draft.autostart} />
			</div>
			<p class="fine">
				Registers this app for your user only (Windows: Run key entry · Linux: autostart entry ·
				macOS: login LaunchAgent). Each launch re-points the entry at the app you last ran, so it
				never points at a stale or old-path build.
			</p>
			{#if errors.autostart}
				<p class="field-error" role="alert">{errors.autostart}</p>
			{/if}
		</section>

		<section class="settings-section" aria-labelledby="mode-title">
			<h3 class="section-label" id="mode-title">Mode</h3>
			<div class="toggle-row">
				<label for="dlg-dryrun">Dry run (stages locally, submits nothing)</label>
				<input id="dlg-dryrun" type="checkbox" bind:checked={draft.dry_run} />
			</div>
			<p class="fine">{status?.staging ?? view?.staging_mode ?? '—'}</p>
		</section>

		<section class="settings-section" aria-labelledby="coord-title">
			<h3 class="section-label" id="coord-title">Coordinator</h3>
			<div>
				<label for="coord-url">Coordinator URL</label>
				<input id="coord-url" type="text" bind:value={draft.coordinator_url} />
				{#if errors.coordinator_url}
					<p class="field-error" role="alert">{errors.coordinator_url}</p>
				{/if}
			</div>
			<div>
				<label for="dlg-pubkey">Contract public key (ed25519, 64 hex)</label>
				<!-- The contract key is PUBLIC (the coordinator publishes it);
				     masking it would only hide what a volunteer needs to
				     verify, and triggers the browser's password-in-form hint. -->
				<input
					id="dlg-pubkey"
					type="text"
					spellcheck="false"
					bind:value={draft.public_key_hex}
					placeholder={view?.public_key_configured ? `unchanged (…${view.public_key_tail})` : '64-char hex, or set LH_PUBLIC_KEY_HEX'}
					autocomplete="off"
				/>
				{#if errors.public_key_hex}
					<p class="field-error" role="alert">{errors.public_key_hex}</p>
				{/if}
			</div>
			<button onclick={testConnection} disabled={busy}>Test connection</button>
		</section>

		<section class="settings-section" aria-labelledby="ident-title">
			<h3 class="section-label" id="ident-title">Identity</h3>
			<div class="identity-box">
				<div class="meta-row">
					<span>client_id</span>
					<b>{view?.client_id ?? 'not registered yet'}</b>
				</div>
				<div class="meta-row">
					<span>credentials</span>
					<b>{view?.credentials_present ? view.credentials_path : 'none yet (created on register)'}</b>
				</div>
				<div class="meta-row">
					<span>app data dir</span>
					<b>{view?.data_dir ?? '—'}</b>
				</div>
			</div>
			<div class="settings-actions">
				<button onclick={register} disabled={busy || loopActive}>Register</button>
				<button class={armedUnregister ? 'danger armed' : 'danger'} onclick={unregister} disabled={busy || !view?.credentials_present}>
					{armedUnregister ? 'Confirm unregister?' : 'Unregister'}
				</button>
			</div>
			<p class="fine">
				Unregister deletes the local credentials, and this client stops acting: the coordinator has no revoke
				endpoint, so any open leases simply lapse and requeue for other volunteers.
			</p>
		</section>

		<section class="settings-section" aria-labelledby="lm-title">
			<h3 class="section-label" id="lm-title">Local model</h3>
			<div>
				<label for="lm-mode">Local model</label>
				<select id="lm-mode" bind:value={draft.local_model.mode}>
					{#each LOCAL_MODEL_MODES as m (m.value)}
						<option value={m.value}>{m.label}</option>
					{/each}
				</select>
			</div>
			{#if errors.local_model}
				<p class="field-error" role="alert">{errors.local_model}</p>
			{/if}

			{#if lmMode === 'none'}
				<p class="fine">
					Some search-grading jobs are unavailable without a local model. Every other job type works normally —
					this is not an error.
				</p>
			{:else}
				<p class="fine">
					{lmMode === 'auto'
						? 'Runs a small local model on this machine to grade search results — nothing is uploaded except the verdict JSON.'
						: 'Grades search results through an OpenAI-compatible endpoint you run or subscribe to. The verdict JSON schema is identical to Auto-configure.'}
				</p>

				{#if lmMode === 'auto'}
					<div>
						<label for="lm-model-pick">Bundled model</label>
						<select id="lm-model-pick" bind:value={draft.local_model.model_id}>
							{#each bundledModels as m (m.id)}
								<option value={m.id}>
									{bundledModelCaption(m.id)}{m.id === LOCAL_MODEL_BUNDLED_DEFAULT_ID ? ' — default' : ''}
								</option>
							{/each}
						</select>
					</div>
					<p class="fine">
						Each entry is its own sha256-pinned download: switching models re-kicks the download of the
						chosen file (the other model's verified file stays on disk), and a live sidecar restarts
						against the new pick. The 9B-class file is opt-in — only worth picking if your machine
						clears the "Can I AI?" check below.
					</p>
					<div>
						<label for="lm-cap">Resource cap</label>
						<select id="lm-cap" bind:value={draft.local_model.cap}>
							{#each LOCAL_MODEL_CAPS as c (c.value)}
								<option value={c.value}>{c.label}</option>
							{/each}
						</select>
					</div>
					<p class="fine">{lmCapHint}</p>
					<div class="identity-box">
						<div class="meta-row">
							<span>Model</span>
							<b>{llm?.model_line ?? bundledModelCaption(draft.local_model.model_id)}</b>
						</div>
						<div class="meta-row">
							<span>Status</span>
							<b>{lmStatusText}</b>
						</div>
						{#if llm?.binary_version}
							<div class="meta-row">
								<span>llama-server</span>
								<b>
									{llm.binary_version}
									{llm.binary_tag
										? `— installed ${llm.binary_tag} (${llm.binary_backend ?? 'local'} build under the app data dir)`
										: '— from your own install (override/PATH)'}
								</b>
							</div>
						{:else if !llm?.binary_found}
							<div class="meta-row">
								<span>llama-server</span>
								<b>{llm?.binary_fetch_supported === false
										? 'not found — this OS/arch needs a manual install (see path override below)'
										: 'not installed yet — Start fetches the pinned llama.cpp build for you'}</b>
							</div>
						{/if}
						{#if llm?.model_bytes_on_disk}
							<div class="meta-row">
								<span>on disk</span>
								<b>{fmtBytes(llm.model_bytes_on_disk)}</b>
							</div>
						{/if}
						</div>
						{#if llm?.progress}
						<div class="meta-row lm-progress">
							<span>{llm.status === 'binary_downloading' ? 'llama-server download' : 'download'}</span>
							<b>
								<progress max={llm.progress.total_bytes || 1} value={llm.progress.done_bytes}></progress>
								{llm.progress.percent}%
							</b>
						</div>
						{/if}
						{#if llm?.detail}
						<p class="fine lm-detail">{llm.detail}</p>
						{/if}
						<div class="settings-actions">
						<button onclick={() => lmAction('download_local_model', 'Model download')} disabled={busy}>Download model</button>
						<button onclick={() => lmAction('check_local_model', 'Endpoint check')} disabled={busy}>Check now</button>
						<button class="danger" onclick={() => lmAction('delete_local_model', 'Free disk')} disabled={busy}>Free disk</button>
						</div>
						<div class="settings-actions">
						<button onclick={checkBinaryUpdates} disabled={busy}>Check for binary updates</button>
						{#if binaryUpdate?.update_available}
							<button onclick={installBinaryUpdate} disabled={busy}>Install llama.cpp {binaryUpdate.latest_tag}</button>
						{/if}
						{#if llm?.binary_tag}
							<button class="danger" onclick={removeInstalledBinary} disabled={busy}>Remove llama-server</button>
						{/if}
						</div>
						<p class="fine">
						Auto-configure fetches the pinned llama.cpp release itself (sha256-verified against the release digest,
						self-checked before install; Vulkan build when the hardware check sees a GPU). Nothing auto-updates:
						"Check for binary updates" only tells you what upstream has, and installing it is a separate press.
						</p>
					<div>
						<label for="lm-endpoint">Endpoint override (optional)</label>
						<input
							id="lm-endpoint"
							type="text"
							bind:value={draft.local_model.endpoint}
							placeholder="default http://127.0.0.1:8081/v1 — set only if your own llama-server already listens there"
							autocomplete="off"
						/>
					</div>
				{:else}
					<div>
						<label for="lm-endpoint-c">Endpoint URL</label>
						<input
							id="lm-endpoint-c"
							type="text"
							bind:value={draft.local_model.endpoint}
							placeholder="http://127.0.0.1:1234/v1 (LM Studio) · http://192.168.1.20:8081/v1 (LAN) · or a provider https URL"
							autocomplete="off"
						/>
					</div>
					<div>
						<label for="lm-model">Model name</label>
						<input
							id="lm-model"
							type="text"
							bind:value={draft.local_model.model_name}
							placeholder="slug sent to the endpoint (llama-server ignores it; cloud providers need the exact name)"
							autocomplete="off"
						/>
					</div>
					<div>
						<label for="lm-key">API key (only if your endpoint requires one)</label>
						<input
							id="lm-key"
							type="password"
							bind:value={draft.llm_api_key}
							placeholder={llm?.api_key_present ? 'stored — type to replace, or press Remove key' : 'only ever sent to the endpoint above'}
							autocomplete="off"
						/>
						{#if removeLlmKey}
							<p class="fine lm-detail">The stored key will be deleted when you save.</p>
						{:else if llm?.api_key_present}
							<button class="danger" onclick={() => (removeLlmKey = true)} disabled={busy}>Remove key</button>
						{/if}
						{#if errors.llm_api_key}
							<p class="field-error" role="alert">{errors.llm_api_key}</p>
						{/if}
					</div>
					<p class="fine">
						The key lives in its own 0600 file (same protection class as credentials.json) — never displayed,
						never logged, and sent only to the endpoint above.
					</p>
					{#if llm?.endpoint_is_cloud}
						<p class="banner" role="note">
							Cloud endpoint selected: search result snippets transit this third-party provider for grading —
							only the chunk input and the verdict JSON are exchanged.
						</p>
					{/if}
					<div class="identity-box">
						<div class="meta-row">
							<span>Status</span>
							<b>{llm?.status === 'ready'
								? 'healthy — Start/Register will advertise search grading'
								: llm?.status === 'unhealthy'
									? 'not answering — Start will refuse until fixed'
									: 'not probed since the last save'}</b>
						</div>
					</div>
					{#if llm?.detail}
						<p class="fine lm-detail">{llm.detail}</p>
					{/if}
					<button onclick={() => lmAction('check_local_model', 'Endpoint check')} disabled={busy}>Check now</button>
				{/if}

				<div>
					<label for="lm-ctx">Context tokens (1024–32768)</label>
					<input id="lm-ctx" type="number" min="1024" max="32768" step="512" bind:value={draft.local_model.ctx_tokens} />
				</div>
			{/if}

			{#if lmMode === 'auto'}
				<details>
					<summary>Advanced (sidecar binary)</summary>
					<div>
						<label for="lm-bin">llama-server binary path</label>
						<input
							id="lm-bin"
							type="text"
							bind:value={draft.local_model.llama_server_bin}
							placeholder="empty = search PATH (llama.cpp install)"
							autocomplete="off"
						/>
					</div>
					<p class="fine">
						Bundled model selected above: every choice is a resumable download with its pinned sha256
						verified after landing, with per-model bookkeeping under {view?.data_dir ?? 'the app data dir'}/models/.
						"Free disk" removes the active model's file; re-enabling Auto downloads it again.
					</p>
				</details>
			{/if}

			<div class="hw-advisor">
				<div class="hw-advisor-row">
					<button onclick={runHwCheck} disabled={busy || hwBusy}>{hwBusy ? 'Checking…' : 'Can I AI?'}</button>
					<span class="fine">
						Click-only check: reads this machine's RAM and graphics memory locally — nothing is stored,
						uploaded, or transmitted, and the answer never changes any setting.
					</span>
				</div>
				{#if hwAdvice}
					<div class="identity-box hw-result">
						<div class="meta-row"><span>OS</span><b>{hwAdvice.os_label}</b></div>
						{#if hwAdvice.ram_bytes != null}
							<div class="meta-row">
								<span>System RAM</span>
								<b>{fmtGiB(hwAdvice.ram_bytes)}{hwAdvice.unified_memory ? ' · unified (the GPU shares it)' : ''}</b>
							</div>
						{/if}
						{#if hwAdvice.gpu_state === 'detected'}
							<div class="meta-row">
								<span>GPU</span>
								<b>{fmtGiB(hwAdvice.vram_bytes)} dedicated VRAM · raw ({hwAdvice.gpu_source})</b>
							</div>
						{:else if hwAdvice.gpu_state === 'absent'}
							<div class="meta-row">
								<span>GPU</span>
								<b>{hwAdvice.unified_memory ? 'integrated — unified memory, no separate VRAM' : 'no discrete GPU detected'}</b>
							</div>
						{:else}
							<div class="meta-row"><span>GPU</span><b>couldn't determine</b></div>
						{/if}
						{#if hwAdvice.usable_vram_estimate != null}
							<div class="meta-row">
								<span>VRAM usable est.</span>
								<b>≈{fmtGiB(hwAdvice.usable_vram_estimate)} — Windows + desktop composition reserve part of the raw {fmtGiB(hwAdvice.vram_bytes)}</b>
							</div>
						{/if}
						{#each hwAdvice.lines as line, li (li)}
							<p class="fine hw-line">
								{#each lineParts(line) as part, pi (pi)}
									{#if part.a}
										<a class="hw-link" href={part.a.url} title={part.a.url} target="_blank" rel="noopener noreferrer" onclick={(/** @type {MouseEvent} */ e) => openLink(e, part.a.url)}>{part.a.label}</a>
									{:else}
										{part.t}
									{/if}
								{/each}
							</p>
						{/each}
						{#if hwAdvice.offer_custom_endpoint}
							<div class="settings-actions">
								<button onclick={openCustomEndpoint} disabled={busy}>Open Custom endpoint settings</button>
							</div>
							<p class="fine">This button only preselects the Custom endpoint form above — nothing is saved until you press Save settings.</p>
						{/if}
					</div>
				{/if}
			</div>
		</section>

		<div class="settings-actions">
			<button class="primary" onclick={save} disabled={busy}>Save settings</button>
			<button onclick={() => onClose?.()} disabled={busy}>Close</button>
		</div>
	</div>
</div>
