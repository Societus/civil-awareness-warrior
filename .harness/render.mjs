// Mount the REAL dashboard component in jsdom: no Tauri, so the page takes
// its documented demo stand-in for the lib stats handle, and its real
// 2 s poll + 1 s clock intervals drive live updates. Snapshots at two times
// prove the view renders with ticking values.
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
globalThis.requestAnimationFrame = dom.window.requestAnimationFrame.bind(dom.window);
globalThis.cancelAnimationFrame = dom.window.cancelAnimationFrame.bind(dom.window);
globalThis.addEventListener = dom.window.addEventListener.bind(dom.window);
globalThis.removeEventListener = dom.window.removeEventListener.bind(dom.window);

const { mount } = await import('svelte');
const { default: Page } = await import('./page.client.js');
mount(Page, { target: document.getElementById('app') });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const grab = () => document.getElementById('app').textContent.replace(/\s+/g, ' ').trim();

await sleep(6000);
const t1 = grab();
await sleep(9000);
const t2 = grab();

console.log('SNAPSHOT@6s:', t1);
console.log('SNAPSHOT@15s:', t2);
console.log('LIVE-UPDATES:', t1 !== t2 ? 'yes — values changed between polls' : 'NO');

const need = [
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
	'Dry run',
	'Register',
	'Quit'
];
const missing = need.filter((s) => !t2.includes(s));
const leaseTimer = /\b\d{1,2}:\d{2}\b/.test(t2);
console.log('MISSING-STRINGS:', missing.length ? missing.join(' | ') : 'none');
console.log('LEASE-ELAPSED-TIMER-RENDERED:', leaseTimer ? 'yes' : 'NO');
process.exit(missing.length || t1 === t2 || !leaseTimer ? 1 : 0);
