// Compile the real dashboard component to client JS for the Node/jsdom
// render harness (camofox is down on this box; this proves the component
// renders + ticks without a GUI browser).
import { compile } from 'svelte/compiler';
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';

/**
 * Compile a component and print any compiler warnings. Node can't import
 * raw .svelte files, so every component the harness mounts gets compiled to
 * a .client.js in .harness/ and cross-component imports are rewritten to
 * those compiled siblings.
 * @param {string} file
 * @param {string} outName
 * @param {Record<string, string>} importMap import-specifier rewrites
 */
function compileTo(file, outName, importMap = {}) {
	const src = readFileSync(file, 'utf8');
	const { js, warnings } = compile(src, {
		filename: file,
		generate: 'client',
		dev: false
	});
	if (warnings.length) {
		console.log(`compile warnings (${file}):`, JSON.stringify(warnings.map((w) => w.code ?? w.message)));
	}
	let code = js.code;
	for (const [spec, target] of Object.entries(importMap)) {
		code = code.replace(new RegExp(`from\\s+(['"])${spec}\\1`, 'g'), `from '${target}'`);
	}
	mkdirSync('.harness', { recursive: true });
	writeFileSync(`.harness/${outName}`, code);
	console.log('compiled', file, '→', outName, code.length, 'bytes');
}

compileTo('src/lib/SettingsDialog.svelte', 'dialog.client.js');
compileTo('src/routes/+page.svelte', 'page.client.js', {
	'#lib/SettingsDialog\\.svelte': './dialog.client.js'
});
