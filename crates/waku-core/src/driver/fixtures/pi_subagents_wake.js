/// A fixture Pi extension for the `pi-subagents` acceptance run.
///
/// The acceptance path is the product one — `npm:pi-subagents` running a
/// background workflow whose child completion wakes the parent session — so the
/// workflow is launched through the extension's own documented in-process RPC
/// rather than by asking a model to reach for a tool. Everything after the
/// launch is the extension's own machinery: the detached runner, the child
/// session, and the `subagent-notify` completion that wakes this session.
///
/// Plain JavaScript on purpose: the fixture loads standalone, without resolving
/// Pi's extension SDK from a package it does not own.
export default function wakuSubagentsWake(pi) {
	pi.registerCommand("waku-subagents-wake", {
		description: "Waku acceptance: one background workflow, one child",
		handler: (_args, ctx) => {
			const requestId = "waku-acceptance";
			// A refused spawn would otherwise be indistinguishable from a slow
			// one, which is what the run has a timeout for.
			pi.events.on(`subagents:rpc:v1:reply:${requestId}`, (reply) => {
				if (!reply?.success) {
					ctx.ui.notify(
						`waku acceptance spawn failed: ${reply?.error?.message ?? "no reply"}`,
						"error",
					);
				}
			});
			pi.events.emit("subagents:rpc:v1:request", {
				version: 1,
				requestId,
				method: "spawn",
				params: {
					script:
						'return runs.run("probe", { agent: "scout", task: "Reply with exactly CHILD-OK and nothing else. Do not use any tools." })',
				},
			});
		},
	});
}
