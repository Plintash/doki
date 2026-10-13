/// A fixture Pi extension that answers Waku's digest trigger.
///
/// The live transport tests assert the shape of the exchange — a command Pi
/// dispatches while no run is streaming, and a result that comes back as a
/// session entry rather than as conversation — against the provider itself
/// rather than against a canned frame. Waku's own extension adds a model call
/// on top of that; this one publishes a fixed sentence so the test needs no
/// credentials and no network.
///
/// The command name, the entry type, and the payload shape are the ones the
/// bundled extension uses: `docs/titles.md` documents them.
export default function wakuDigestProbe(pi) {
	pi.registerCommand("waku:digest", {
		description: "Waku live test: answer the digest trigger",
		handler: async (args) => {
			pi.appendEntry("waku:digest", {
				v: 1,
				dispatch: String(args ?? "").trim(),
				objective: "The live probe answered the digest trigger",
			});
		},
	});
}
