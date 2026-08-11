// Vibe Kanban ↔ Pi permission bridge.
//
// Loaded with an explicit `-e` flag in SUPERVISED mode. Read-only tools pass;
// every mutating or unknown tool is confirmed through Pi's extension UI
// protocol, which the Vibe Kanban RPC client answers with its approval system.
//
// No npm dependencies: Pi loads .mjs extensions natively.

const READ_ONLY_TOOLS = new Set(["read", "grep", "find", "ls"]);
const ENVELOPE_PREFIX = "vk-pi-approval:";
const MAX_SUMMARY_CHARS = 500;

function summarize(value) {
	let text;
	try {
		text = typeof value === "string" ? value : JSON.stringify(value);
	} catch {
		text = String(value);
	}
	if (text.length > MAX_SUMMARY_CHARS) {
		return `${text.slice(0, MAX_SUMMARY_CHARS)}…`;
	}
	return text;
}

export default function (pi) {
	if (process.env.VK_PI_PERMISSION_POLICY !== "SUPERVISED") {
		return;
	}

	pi.on("tool_call", async (event, ctx) => {
		if (READ_ONLY_TOOLS.has(event.toolName)) {
			return undefined;
		}

		if (!ctx.hasUI) {
			return { block: true, reason: "Supervised mode requires approval UI" };
		}

		const envelope = {
			v: 1,
			kind: "vk_tool_approval",
			toolCallId: event.toolCallId,
			toolName: event.toolName,
			summary: summarize(event.input),
		};

		const confirmed = await ctx.ui.confirm(
			`${ENVELOPE_PREFIX}${JSON.stringify(envelope)}`,
		);
		if (confirmed !== true) {
			return { block: true, reason: "Denied in Vibe Kanban" };
		}
		return undefined;
	});
}
