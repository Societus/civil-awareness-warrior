// Compile the real dashboard component to client JS for the Node/jsdom
// render harness (camofox is down on this box; this proves the component
// renders + ticks without a GUI browser).
import { compile } from 'svelte/compiler';
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';

const src = readFileSync('src/routes/+page.svelte', 'utf8');
const { js, warnings } = compile(src, {
	filename: '+page.svelte',
	generate: 'client',
	dev: false
});
if (warnings.length) {
	console.log('compile warnings:', JSON.stringify(warnings.map((w) => w.message)));
}
mkdirSync('.harness', { recursive: true });
writeFileSync('.harness/page.client.js', js.code);
console.log('compiled', js.code.length, 'bytes');
