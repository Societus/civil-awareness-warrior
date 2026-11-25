// Regression harness for the operator issuance section: mount the REAL
// SettingsDialog with operator_visible=true (what a volunteer NEVER sees,
// but the operator sees the moment a lone .pem lands in the data dir) and
// prove the section renders without throwing.
//
// History: the Test-token button's disabled expression read
// draft.operator.cf_token — a field that does not exist (the token draft
// lives at the TOP level, draft.operator_cf_token), so undefined.trim()
// threw a TypeError at render time and the whole dialog failed to mount:
// gear click → softlock, operator mode unreachable. Demo-mode harnesses
// never rendered this section, so the class slipped through. This harness
// exists to catch any render-time throw in the operator section.
// Run with: node --conditions=browser .harness/render-operator.mjs
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
globalThis.MutationObserver = dom.window.MutationObserver;

const { mount } = await import('svelte');
const { default: SettingsDialog } = await import('./dialog.client.js');

// The view a Rust settings_view() hands over once a lone .pem was adopted:
// operator_visible=true, configured key path, empty CF ids.
const view = {
	coordinator_url: 'https://caw.landhunter.app',
	concurrency: 2,
	dry_run: true,
	capabilities: {},
	schedule: {},
	autostart: false,
	staging_mode: 'legacy',
	public_key_configured: true,
	operator_visible: true,
	public_key_tail: '3596d',
	data_dir: '/tmp/x',
	credentials_path: '/tmp/x/credentials.json',
	credentials_present: false,
	client_id: null,
	local_model: { mode: 'none', cap: 'cpu_third', model_id: '', endpoint: '', llama_server_bin: '', model_name: '', ctx_tokens: 4096 },
	operator: { key_path: '/tmp/x/operator-ed25519-private.pem', cf_account_id: '', cf_database_id: '' },
	operator_cf_token_present: false,
	bundled_models: [],
	llm: { status: 'off' }
};

const failures = [];
try {
	mount(SettingsDialog, {
		target: document.getElementById('app'),
		props: { view, status: { phase: 'idle' }, onClose: () => {}, onChanged: () => {} }
	});
} catch (e) {
	failures.push(`mount threw: ${e?.message ?? e}`);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
await sleep(1500);

const text = document.getElementById('app').textContent.replace(/\s+/g, ' ').trim();
for (const need of [
	'Operator — job issuance',
	'Private key path',
	'Derive public key',
	'Test token',
	'Issue chunks'
]) {
	if (!text.includes(need)) failures.push(`operator section missing: "${need}"`);
}

if (failures.length) {
	console.log('OPERATOR-RENDER-FAIL:', failures.join(' | '));
	process.exit(1);
}
console.log('OPERATOR-RENDER-OK: dialog mounts with operator_visible=true; section complete');
process.exit(0);
