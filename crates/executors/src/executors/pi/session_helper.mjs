// vibe-kanban PI session history helper.
//
// Usage: node session_helper.mjs <pi-package-index-path> <session-file>
//
// Prints the session's active-branch entries (compaction applied) as JSONL on
// stdout, one entry per line. Tree/compaction semantics belong to PI's own
// SessionManager — this helper only re-exports its result; never reimplement
// branch resolution here.

import { pathToFileURL } from "node:url";

const [pkgIndex, sessionFile] = process.argv.slice(2);
if (!pkgIndex || !sessionFile) {
	console.error(
		"usage: session_helper.mjs <pi-package-index-path> <session-file>",
	);
	process.exit(2);
}

const mod = await import(pathToFileURL(pkgIndex).href);
const { SessionManager } = mod;
if (
	typeof SessionManager !== "function" ||
	typeof SessionManager.open !== "function"
) {
	console.error(
		"unsupported @earendil-works/pi-coding-agent API: SessionManager.open missing",
	);
	process.exit(3);
}

const sm = SessionManager.open(sessionFile);
if (typeof sm.buildContextEntries !== "function") {
	console.error("unsupported SessionManager API: buildContextEntries missing");
	process.exit(3);
}

for (const entry of sm.buildContextEntries()) {
	process.stdout.write(JSON.stringify(entry) + "\n");
}
