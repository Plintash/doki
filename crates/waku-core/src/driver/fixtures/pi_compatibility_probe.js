// Provider-free fixture. The ordinary probe never starts a run. The startup
// gate checks Pi 1.2 preflight only with an isolated unknown model; its caller
// cancels before releasing it. Any ordinary input is consumed here.
export default function compatibilityProbe(pi) {
  let invalidSection = false;
  let fastFailure = false;
  pi.on("input", async () => ({ action: "handled" }));
  pi.on("before_agent_start", async (event, ctx) => {
    if (event.messageType === "custom") {
      if (!fastFailure) await ctx.ui.confirm("Doki startup gate", "Release only after cancellation");
      if (invalidSection) event.systemPromptOptions.sections["Invalid Section"] = "Fixture";
    }
  });
  pi.registerCommand("waku-compatibility-startup", {
    description: "Provider-free Pi 1.2 startup cancellation gate",
    handler: (args, ctx) => {
      invalidSection = args === "invalid" || args === "invalid-fast";
      fastFailure = args === "invalid-fast";
      if (ctx.model?.provider !== "unknown" || ctx.model?.id !== "unknown") {
        throw new Error("Startup fixture requires the isolated unknown model");
      }
      pi.sendMessage({
        customType: "waku-compatibility-startup",
        content: "No provider configured; cancel before releasing the gate",
        display: true,
      }, { triggerTurn: true });
    },
  });
  pi.registerCommand("waku-compatibility-probe", {
    description: "Provider-free Doki compatibility check",
    handler: async (_args, ctx) => {
      pi.setSessionName("Doki Pi compatibility");
      pi.sendMessage({
        customType: "waku-compatibility-notice",
        content: "Extension message without calling a model",
        display: true,
      });
      const confirmed = await ctx.ui.confirm("Doki compatibility", "Continue the check?");
      pi.sendMessage({
        customType: "waku-compatibility-answer",
        content: String(confirmed),
        display: false,
      });
    },
  });
}
