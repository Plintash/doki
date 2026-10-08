/// A fixture Pi extension for the live transport tests.
///
/// Every record those tests assert on is produced by Pi in response to
/// something an extension did, so a canned frame would prove the decoding
/// against the decoder's own assumption rather than against the provider. The
/// commands below are that something, and the fixture is loaded through the
/// launch's `--extension` flag — the same argument path Waku's own Pi
/// extension takes.
///
/// Plain JavaScript on purpose: the fixture loads standalone, without
/// resolving Pi's extension SDK from a package it does not own.
export default function wakuLiveProbe(pi) {
	/// Appends a message and lets it start a run — the shape a background
	/// workflow's completion wake takes.
	pi.registerCommand("waku-live-wake", {
		description: "Waku live test: a run the extension starts on its own",
		handler: async () => {
			await pi.sendMessage(
				{
					customType: "waku-live-wake",
					content: "Reply with exactly: WAKU-LIVE-WAKE. Do not use any tools.",
					display: true,
				},
				{ triggerTurn: true },
			);
		},
	});

	/// Appends extension messages that must not start a run — a subagent's own
	/// child notification, a plain notice, and one marked not for display —
	/// then asks a question and reports the answer as another message.
	pi.registerCommand("waku-live-probe", {
		description: "Waku live test: extension messages and one question",
		handler: async (_args, ctx) => {
			await pi.sendMessage(
				{ customType: "waku-live-notice", content: "probe notice", display: true },
				{ triggerTurn: false },
			);
			await pi.sendMessage(
				{
					customType: "subagent-incremental-child-notify",
					content: "Workflow child completed: **probe-child** (1/1).\nprobe child output",
					display: true,
				},
				{ triggerTurn: false },
			);
			await pi.sendMessage(
				{ customType: "waku-live-hidden", content: "hidden probe", display: false },
				{ triggerTurn: false },
			);
			const answer = await ctx.ui.confirm("Waku live probe", "Answer the probe?");
			await pi.sendMessage(
				{ customType: "waku-live-answer", content: `answer=${answer}`, display: true },
				{ triggerTurn: false },
			);
		},
	});
}
