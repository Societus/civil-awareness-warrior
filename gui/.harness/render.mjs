// Mount the REAL dashboard component in jsdom: no Tauri, so the page takes
// its documented demo stand-in for the lib stats handle, and its real
// 2 s poll + 1 s clock intervals drive live updates. Snapshots at two times
// prove the view renders with ticking values. Then the harness opens the
// settings dialog through the gear control and drives the settings
// round-trip, capability gating of the demo lease loop, validation
// rejection, pause/resume, register and unregister through the same demo
// paths a browser preview uses.
// Run with: node --conditions=browser .harness/render.mjs
import { JSDOM } from 'jsdom';

const dom = new JSDOM('<!doctype html><html><body><div id="app"></div></body></html>', {
	url: 'http://127.0.0.1:1423/',
	pretendToBeVisual: true
});

// Browser globals must exist before the svelte client runtime is imported.
globalThis.window = dom.window;
globalThis.document = dom.window.document;
Object.defineProperty(globalThis, 'navigator', { value: dom.window.navigator, configurable: true });
globalThis.HTMLElement = dom.window.HTMLElement;
globalThis.Element = dom.window.Element;
globalThis.Node = dom.window.Node;
globalThis.Text = dom.window.Text;
globalThis.Comment = dom.window.Comment;
globalThis.DocumentFragment = dom.window.DocumentFragment;
globalThis.MouseEvent = dom.window.MouseEvent;
globalThis.Event = dom.window.Event;
globalThis.localStorage = dom.window.localStorage;
globalThis.requestAnimationFrame = dom.window.requestAnimationFrame.bind(dom.window);
globalThis.cancelAnimationFrame = dom.window.cancelAnimationFrame.bind(dom.window);
globalThis.addEventListener = dom.window.addEventListener.bind(dom.window);
globalThis.removeEventListener = dom.window.removeEventListener.bind(dom.window);

const { mount } = await import('svelte');
const { default: Page } = await import('./page.client.js');
const demo = await import('#lib/demo.js');

mount(Page, { target: document.getElementById('app') });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const grab = () => document.getElementById('app').textContent.replace(/\s+/g, ' ').trim();

/** @type {string[]} */
const failures = [];
/** @param {string[]} need @param {string} where @param {string} hay */
function checkStrings(need, where, hay) {
	const missing = need.filter((s) => !hay.includes(s));
	for (const s of missing) failures.push(`missing in ${where}: "${s}"`);
	console.log(`${where}-MISSING-STRINGS:`, missing.length ? missing.join(' | ') : 'none');
	return missing.length === 0;
}

await sleep(6000);
const t1 = grab();
await sleep(9000);
const t2 = grab();

console.log('SNAPSHOT@6s:', t1.slice(0, 400));
console.log('SNAPSHOT@15s:', t2.slice(0, 400));
const live = t1 !== t2;
console.log('LIVE-UPDATES:', live ? 'yes — values changed between polls' : 'NO');
if (!live) failures.push('no live updates between polls');

checkStrings(
	[
		'LandHunter Contributor',
		'demo mode — synthetic data',
		'online',
		'last poll',
		'Activity',
		'running',
		'Leased —',
		'Bytes staged',
		'Session uptime',
		'Total runtime, all sessions',
		'Community total',
		'Dry run (no results submitted)',
		'Quit'
	],
	'dashboard',
	t2
);
const leaseTimer = /\b\d{1,2}:\d{2}\b/.test(t2);
console.log('LEASE-ELAPSED-TIMER-RENDERED:', leaseTimer ? 'yes' : 'NO');
if (!leaseTimer) failures.push('per-lease elapsed timer not rendered');

// ── gear → settings dialog ─────────────────────────────────────────────────
const gear = document.querySelector('button[aria-label="Open settings"]');
if (!gear) {
	failures.push('gear button (aria-label "Open settings") not found');
} else {
	gear.dispatchEvent(new dom.window.MouseEvent('click', { bubbles: true, cancelable: true }));
	await sleep(400);
	const dlg = grab();
	checkStrings(
		[
			'Settings',
			'Capabilities',
			'Examination',
			'Ingest',
			'Alt-Source Probe',
			'State Walk',
			'Search',
			'two independent workers must agree', // examination description (landing wording)
			"county's GIS service", // ingest description
			'structured reconnaissance', // alt_source description
			'municipal code library', // state_walk description
			'Brave, Bing', // search description
			'Concurrency',
			'Parallel slots',
			'1–8 simultaneous leases',
			'Schedule',
			'Limit to an active window',
			'Always active (default)',
			'Dry run (stages locally, submits nothing)',
			'Coordinator',
			'Contract public key (ed25519, 64 hex)',
			'Identity',
			'client_id',
			'not registered yet',
			'none yet (created on register)',
			'/demo/app-data',
			'Register',
			'Unregister',
			'Test connection',
			'Advanced',
			'local model opt-in',
			'Save settings'
		],
		'dialog',
		dlg
	);
	if (!document.querySelector('[role="dialog"]')) failures.push('settings dialog role=dialog missing');
	const keyInput = document.querySelector('#dlg-pubkey');
	const placeholder = keyInput?.getAttribute('placeholder') ?? '';
	const tailOk = placeholder.includes('unchanged (…e8a1c4)');
	console.log('PUBKEY-PLACEHOLDER-TAIL:', tailOk ? `yes — "${placeholder}"` : `NO — "${placeholder}"`);
	if (!tailOk) failures.push('public key tail placeholder missing');
	// Close via the scrim and confirm the dialog unmounts.
	document.querySelector('.scrim')?.dispatchEvent(new dom.window.MouseEvent('click', { bubbles: true, cancelable: true }));
	await sleep(400);
	if (document.querySelector('[role="dialog"]')) failures.push('dialog did not close on scrim click');
	console.log('DIALOG-CLOSE-ON-SCRIM:', document.querySelector('[role="dialog"]') ? 'NO' : 'yes');
}

// ── settings round-trip: save → persisted store (what a restart reads) ─────
demo.demoSaveSettings({
	capabilities: { examination: true, ingest: false, alt_source: true, state_walk: true, search: false },
	schedule: { enabled: true, active_from: '00:00', active_until: '23:59' },
	concurrency: 3
});
const stored = demo.makeDemoSettingsView();
const st3 = demo.makeDemoStatus(10);
console.log('STORED-CAPS:', JSON.stringify(stored.capabilities));
console.log('STORED-SCHEDULE:', JSON.stringify(stored.schedule), 'CONCURRENCY:', stored.concurrency);
console.log(
	'STATUS-REFLECTS-STORE:',
	st3.concurrency === 3 && st3.slots.length === 3 && st3.schedule_enabled === true && st3.schedule_in_window === true
		? 'yes'
		: 'NO'
);
if (!(st3.concurrency === 3 && st3.slots.length === 3)) failures.push('status does not reflect stored concurrency');
if (!(st3.schedule_enabled && st3.schedule_in_window)) failures.push('status schedule flags wrong');

// Capability gating: with ALL caps except examination off, every demo lease
// must be an examination lease — the same filter the real lease polls carry.
demo.demoSaveSettings({ capabilities: { examination: true, ingest: false, alt_source: false, state_walk: false, search: false } });
/** @type {Set<string>} */
const seenTypes = new Set();
for (let tick = 10; tick < 40; tick += 3) {
	for (const slot of demo.makeDemoStatus(tick).slots) {
		if (slot.chunk_type) seenTypes.add(slot.chunk_type);
	}
}
console.log('CAPABILITY-GATED-LEASE-TYPES:', [...seenTypes].join(',') || '(none seen)');
if (seenTypes.size !== 1 || !seenTypes.has('examination')) failures.push('demo lease loop ignored capability filter');

// Validation surfaces the same FieldError shape the Rust rejection carries.
const storeBeforeBadSave = JSON.stringify(demo.demoReadPersistedSettings());
/** @type {any} */
let rejected = null;
try {
	demo.demoSaveSettings({ capabilities: { examination: false, ingest: false, alt_source: false, state_walk: false, search: false } });
} catch (e) {
	rejected = e;
}
const validationOk = rejected?.field === 'capabilities' && typeof rejected?.message === 'string';
console.log('VALIDATION-ALL-CAPS-OFF:', validationOk ? `rejected field=${rejected.field} msg="${rejected.message}"` : 'NO (BUG)');
if (!validationOk) failures.push('empty-capability validation missing');
const storeAfterBadSave = JSON.stringify(demo.demoReadPersistedSettings());
if (storeAfterBadSave !== storeBeforeBadSave) {
	failures.push('rejected save corrupted the persisted store');
} else {
	console.log('REJECTED-SAVE-LEFT-STORE-UNCHANGED: yes');
}

// Restore sane values, prove persistence survived the rejected attempt.
demo.demoSaveSettings({ capabilities: { examination: true, ingest: true, alt_source: true, state_walk: true, search: true } });

// ── inline field errors in the REAL dialog UI (clicks, not module calls) ───
gear.dispatchEvent(new dom.window.MouseEvent('click', { bubbles: true, cancelable: true }));
await sleep(400);
const capsBeforeUiSave = JSON.stringify(demo.demoReadPersistedSettings()?.capabilities);
for (const id of ['examination', 'ingest', 'alt_source', 'state_walk', 'search']) {
	const cb = document.querySelector(`#${id}`);
	if (!cb?.checked) {
		failures.push(`capability checkbox #${id} not rendered checked on reopen`);
		continue;
	}
	cb.dispatchEvent(new dom.window.MouseEvent('click', { bubbles: true, cancelable: true }));
}
await sleep(200);
const saveBtn = [...document.querySelectorAll('button')].find((b) => b.textContent.trim() === 'Save settings');
if (!saveBtn) {
	failures.push('Save settings button not found');
} else {
	saveBtn.dispatchEvent(new dom.window.MouseEvent('click', { bubbles: true, cancelable: true }));
	await sleep(500);
	const afterSave = grab();
	const inlineOk =
		afterSave.includes('enable at least one chunk type') && !afterSave.includes('Settings saved');
	console.log('INLINE-FIELD-ERROR:', inlineOk ? 'shown under Capabilities, no save note' : 'NO');
	if (!inlineOk) failures.push('inline field error not rendered in dialog');
	const capsAfterUiSave = JSON.stringify(demo.demoReadPersistedSettings()?.capabilities);
	if (capsAfterUiSave !== capsBeforeUiSave) {
		failures.push('UI-rejected save leaked into the persisted store');
	} else {
		console.log('REJECTED-UI-SAVE-LEFT-STORE-UNCHANGED: yes');
	}
	document.querySelector('.scrim')?.dispatchEvent(new dom.window.MouseEvent('click', { bubbles: true, cancelable: true }));
	await sleep(300);
}

// ── pause / resume ─────────────────────────────────────────────────────────
demo.demoPauseContributor();
const paused = demo.makeDemoStatus(10);
const pauseOk = paused.phase === 'paused' && paused.pause_reason === 'user';
console.log('PAUSE:', pauseOk ? 'phase=paused reason=user' : `NO (${paused.phase}/${paused.pause_reason})`);
if (!pauseOk) failures.push('pause semantics wrong');
const frozen = paused.stats.uptime_secs;
demo.demoStartContributor();
const resumed = demo.makeDemoStatus(10);
const resumeOk = resumed.phase === 'running' && resumed.pause_reason === null;
console.log('RESUME:', resumeOk ? 'phase=running' : `NO (${resumed.phase}/${resumed.pause_reason})`);
if (!resumeOk) failures.push('resume semantics wrong');

// ── register → unregister ──────────────────────────────────────────────────
const regMsg = demo.demoRegister();
const withId = demo.makeDemoSettingsView();
const regOk = regMsg.includes('registered as') && withId.client_id?.startsWith('demo-warrior-') && withId.credentials_present;
console.log('REGISTER:', regOk ? `client_id=${withId.client_id}` : `NO (${regMsg})`);
if (!regOk) failures.push('demo register path broken');

// Unregister refuses while the loop is live (mirror of the Rust guard).
/** @type {any} */
let liveReject = null;
try {
	demo.demoUnregister();
} catch (e) {
	liveReject = e;
}
const refuseOk = typeof liveReject === 'string' && liveReject.includes('pause the contributor loop before unregistering');
console.log('UNREGISTER-REFUSED-WHILE-LIVE:', refuseOk ? `yes — "${liveReject}"` : 'NO (BUG)');
if (!refuseOk) failures.push('unregister guard while live missing');

demo.demoPauseContributor();
const unregMsg = demo.demoUnregister();
const goneView = demo.makeDemoSettingsView();
const unregOk = unregMsg.includes('identity removed locally') && goneView.client_id === null && goneView.credentials_present === false;
console.log('UNREGISTER:', unregOk ? 'credentials gone, client_id=null' : `NO (${unregMsg})`);
if (!unregOk) failures.push('unregister path broken');

console.log('FAILURES:', failures.length ? failures.join(' || ') : 'none');
process.exit(failures.length ? 1 : 0);
