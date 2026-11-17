<script>
	import { invoke } from '@tauri-apps/api/core';
	import { CAPABILITIES } from '#lib/capabilities.js';
	import { scheduleInWindow } from '#lib/settings-util.js';
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
			}
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
			}
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
				schedule: { ...next.schedule }
			};
			note = 'Settings saved. Concurrency, capabilities and dry-run apply on the next start; the run window is enforced live.';
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

		<section class="settings-section" aria-labelledby="adv-title">
			<h3 class="section-label" id="adv-title">Advanced</h3>
			<div class="advanced-placeholder">
				Future options — local model opt-in, resource caps (GPU / ⅓-cores), model status — will appear here when
				LLM chunks ship.
			</div>
		</section>

		<div class="settings-actions">
			<button class="primary" onclick={save} disabled={busy}>Save settings</button>
			<button onclick={() => onClose?.()} disabled={busy}>Close</button>
		</div>
	</div>
</div>
