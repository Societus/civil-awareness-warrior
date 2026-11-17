/**
 * The five chunk types a volunteer can opt into, with the plain-English
 * descriptions taken from the coordinator landing page
 * (lh-tracker/src/routes/landing.ts → "What Workers Do"), so the GUI and
 * the public page say the same thing to volunteers.
 */
export const CAPABILITIES = [
	{
		id: 'examination',
		label: 'Examination',
		description:
			'Re-run a parser over a staged data blob to verify measurements match exactly. Deterministic — two independent workers must agree.'
	},
	{
		id: 'ingest',
		label: 'Ingest',
		description:
			"Download all pages of a county's GIS service (zoning layers, parcel data) following a frozen fetch plan. Byte-hash verified against a 5% sample re-fetch."
	},
	{
		id: 'alt_source',
		label: 'Alt-Source Probe',
		description:
			'Mechanical probes of candidate GIS endpoints: enumerate services, count features, test pagination, check for null codes. No judgment — just structured reconnaissance.'
	},
	{
		id: 'state_walk',
		label: 'State Walk',
		description:
			'Walk a slice of a municipal code library (Municode, American Legal, etc.), extracting district titles and slug keys per a fixed recipe.'
	},
	{
		id: 'search',
		label: 'Search',
		description:
			'Run fixed query plans against search APIs (Brave, Bing) to locate election results, zoning portals, or certified PDFs. Results graded by source class.'
	}
];

/** @param {Record<string, boolean>} caps @returns {string[]} enabled ids */
export function enabledCapIds(caps) {
	return CAPABILITIES.filter((c) => caps?.[c.id]).map((c) => c.id);
}
