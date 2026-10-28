const fs = require("node:fs");
const path = require("node:path");
const readline = require("node:readline");
let session = process.argv.includes("--fork") ? "forked" : "source";
let model = "recorded/model-b", effort = "low", leg = "", corrected = false;
readline.createInterface({ input: process.stdin }).on("line", line => {
  const request = JSON.parse(line);
  fs.appendFileSync(config.log, JSON.stringify({ ...request, activeModel: model, activeEffort: effort, activeSession: session }) + "\n");
  const reply = (data = {}) => console.log(JSON.stringify({ type: "response", id: request.id, command: request.type, success: true, data }));
  if (leg === config.failLeg && (request.type === config.failure || (corrected && request.type === "get_state" && config.failure === "state_readback"))) {
    console.log(JSON.stringify({ type: "response", id: request.id, success: false, error: "fixture rejected " + request.type }));
    return;
  }
  switch (request.type) {
    case "get_state": {
      const slash = model.indexOf("/");
      const provider = model.slice(0, slash), id = model.slice(slash + 1);
      reply({ sessionId: session, sessionFile: path.join(path.dirname(config.log), session + ".jsonl"), model: { provider, id }, thinkingLevel: effort });
      break;
    }
    case "get_fork_messages": case "get_branch_messages": reply({ messages: [{ entryId: "turn-1" }, { entryId: "turn-2" }] }); break;
    case "get_commands": reply({ commands: [] }); break;
    case "fork": case "branch": case "clone": case "switch_session":
      if (config.failure === "cancel_fork" && request.type !== "switch_session") { reply({ cancelled: true }); break; }
      session = request.type === "switch_session" ? "source" : "forked";
      leg = request.type === "switch_session" ? "switch" : "fork";
      corrected = false;
      if (config.resetSelection) { model = "recorded/model-b"; effort = "low"; }
      reply(); break;
    case "set_model":
      if (!(leg === config.failLeg && config.failure === "model_readback")) model = request.provider + "/" + request.modelId;
      corrected = true;
      reply(); break;
    case "set_thinking_level":
      if (!(leg === config.failLeg && config.failure === "effort_readback")) effort = request.level;
      corrected = true;
      reply(); break;
    case "prompt": reply(); break;
    default: reply();
  }
});
