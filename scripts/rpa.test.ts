import { afterEach, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { LEGACY_PROTOCOL_VERSION, parseArguments, snapshotSections } from "./rpa";

const directories: string[] = [];
afterEach(() => {
  for (const path of directories.splice(0)) rmSync(path, { recursive: true, force: true });
});

test("global options can precede the command without consuming command arguments", () => {
  const parsed = parseArguments(["--config", "/tmp/other/rpa.json", "--json", "type", "run the tests", "--into", "composer.input"]);
  expect(parsed.command).toBe("type");
  expect(parsed.args).toEqual(["run the tests"]);
  expect(parsed.options).toMatchObject({ config: "/tmp/other/rpa.json", json: true, into: "composer.input" });
});

async function runCli(args: string[], response: (request: any) => object, protocolVersion = LEGACY_PROTOCOL_VERSION) {
  const directory = mkdtempSync(join(tmpdir(), "waku-rpa-cli-"));
  directories.push(directory);
  const server = Bun.listen<{ buffer: string }>({
    hostname: "127.0.0.1",
    port: 0,
    socket: {
      open(socket) { socket.data = { buffer: "" }; },
      data(socket, data) {
        socket.data.buffer += data.toString();
        if (!socket.data.buffer.includes("\n")) return;
        const request = JSON.parse(socket.data.buffer.trim());
        socket.write(JSON.stringify({ id: request.id, ...response(request) }) + "\n");
      },
    },
  });
  const config = join(directory, "rpa.json");
  writeFileSync(config, JSON.stringify({ protocolVersion, address: `127.0.0.1:${server.port}`, token: "cli-test" }));
  try {
    const child = Bun.spawn([process.execPath, join(import.meta.dir, "rpa.ts"), "--config", config, ...args], { stdout: "pipe", stderr: "pipe" });
    const [code, stdout, stderr] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
    return { code, stdout, stderr };
  } finally { server.stop(true); }
}

test("logs sends typed filters without losing correlation", async () => {
  const filter = { minLevel: "warn", sessionId: "conversation", source: "driver" };
  const result = await runCli(["logs", "--filter", JSON.stringify(filter), "--json"], request => ({ ok: true, result: request }));
  expect(result.code).toBe(0);
  expect(JSON.parse(result.stdout)).toMatchObject({ type: "logs", version: 3, query: filter });
});

test("legacy protocol mismatch fails before sending a diagnostic query", async () => {
  for (const version of [2, 4]) {
    let calls = 0;
    const result = await runCli(["logs", "--filter", '{"sessionId":"conversation"}', "--json"], () => {
      calls += 1;
      return { ok: true, result: {} };
    }, version);
    expect(result.code).toBe(1);
    expect(result.stderr).toContain(`legacy endpoint protocol ${version} does not match this CLI's 3`);
    expect(result.stderr).not.toContain("cli-test");
    expect(calls).toBe(0);
  }
});

test("timeline and verbosity preserve their shared protocol shape", async () => {
  const timeline = await runCli(["timeline", "--turn", "turn", "--json"], request => ({ ok: true, result: request }));
  expect(timeline.code).toBe(0);
  expect(JSON.parse(timeline.stdout)).toMatchObject({ type: "timeline", query: { turnId: "turn" } });
  const conversation = await runCli(["timeline", "--conversation", "conversation", "--turn", "turn", "--json"], request => ({ ok: true, result: request }));
  expect(conversation.code).toBe(0);
  expect(JSON.parse(conversation.stdout)).toMatchObject({ type: "timeline", query: { sessionId: "conversation", turnId: "turn" } });
  expect(() => parseArguments(["timeline", "--task", "issue"])).toThrow();
  const policy = { defaultLevel: "info", overrides: [{ process: "app", source: "pump", level: "debug" }] };
  const verbosity = await runCli(["verbosity", JSON.stringify(policy), "--json"], request => ({ ok: true, result: request }));
  expect(verbosity.code).toBe(0);
  expect(JSON.parse(verbosity.stdout)).toMatchObject({ type: "verbosity", update: policy });
});

test("failed waits print their full evidence and exit unsuccessfully", async () => {
  const bundle = { observed: { health: { daemon: "disconnected" } }, treeDiff: { removed: ["composer.send"] }, recentEvents: [], counters: {} };
  const result = await runCli(["wait", "--id", "composer.send", "--timeout", "1", "--json"], () => ({ ok: false, error: { code: "timeout", message: "not observed", bundle } }));
  expect(result.code).toBe(1);
  expect(result.stdout).toBe("");
  expect(JSON.parse(result.stderr)).toMatchObject({ ok: false, error: { code: "timeout", bundle } });
});

test("human-readable failures retain the evidence too", async () => {
  const result = await runCli(["click", "missing"], () => ({ ok: false, error: { code: "not_found", message: "no matching node", nearest: ["composer.send"], bundle: { counters: { framesDrawn: 12 } } } }));
  expect(result.code).toBe(1);
  expect(result.stderr).toContain("not_found");
  expect(result.stderr).toContain("composer.send");
  expect(result.stderr).toContain('"framesDrawn": 12');
});

// These tests speak the real shared daemon framing over a loopback WebSocket;
// they never need an app-local publication or a native UI executor.
import { PROTOCOL_VERSION } from "../packages/waku-client/src/generated";
import { conditionFromArguments, redact, selectInstance } from "./rpa";

const instance = {
  instanceId: "11111111-1111-4111-8111-111111111111", checkout: "/checkouts/rpa", database: "/checkouts/rpa/temp/app.db",
  pid: 123, build: "build-one", version: "test", executorConnected: true, scratch: false,
};
const cursor = { epoch: "22222222-2222-4222-8222-222222222222", sequence: 1 };
const sessionId = "33333333-3333-4333-8333-333333333333";
const runId = "44444444-4444-4444-8444-444444444444";
const session = { sessionId, instance, cursor, streams: [] };
const effect = { cursor, value: { typed: 4 }, observed: { state: { composer: { text: "test" } } }, treeDiff: { changed: ["composer.input"] }, stateDiff: [{ pointer: "/composer/text", value: "test" }] };

async function runDaemonCli(
  args: string[],
  response: (command: any, socket: any) => object | null | undefined = () => undefined,
  environment: Record<string, string> = {},
) {
  const requests: any[] = [];
  const hellos: any[] = [];
  const token = "test-daemon-secret-do-not-print";
  const server = Bun.serve({
    hostname: "127.0.0.1", port: 0,
    fetch(request, server) { return server.upgrade(request) ? undefined : new Response("WebSocket required", { status: 400 }); },
    websocket: {
      message(socket, message) {
        const request = JSON.parse(String(message));
        if (request.type === "hello") {
          hellos.push(request);
          socket.send(JSON.stringify({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" }));
          return;
        }
        if (request.type !== "request") return;
        requests.push(request);
        const command = request.command;
        let payload = response(command, socket);
        if (payload === null) return; // Deliberately unanswered RPC for deadline coverage.
        if (payload === undefined) {
          if (command.type === "automation") {
            const automation = command.command;
            let result: any;
            if (automation.type === "instances") result = { type: "instances", instances: [instance] };
            else if (automation.type === "attach" || automation.type === "resume") result = { type: "attached", session, replay: { events: [], cursor, expiry: null } };
            else if (automation.type === "detach") result = { type: "detached" };
            else if (automation.type === "call") result = { type: "called", result: effect };
            else throw new Error(`unhandled test command ${automation.type}`);
            payload = { type: "automation", result };
          } else throw new Error(`unhandled test command ${command.type}`);
        }
        socket.send(JSON.stringify({ type: "response", requestId: request.requestId, outcome: { status: "ok", payload } }));
      },
    },
  });
  try {
    const child = Bun.spawn([process.execPath, join(import.meta.dir, "rpa.ts"), ...args], {
      env: { ...process.env, WAKU_RPA_CONFIG: "", WAKU_DAEMON_ADDRESS: `127.0.0.1:${server.port}`, WAKU_DAEMON_TOKEN: token, ...environment },
      stdout: "pipe", stderr: "pipe",
    });
    const [code, stdout, stderr] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
    return { code, stdout, stderr, requests, hellos, token };
  } finally { server.stop(true); }
}

function automation(result: object) { return { type: "automation", result }; }

test("native targeting options reject ambiguous combinations", () => {
  expect(() => parseArguments(["snap", "--instance", "id", "--checkout", "/other"])).toThrow("choose --instance or --checkout");
  expect(() => parseArguments(["snap", "--session", sessionId, "--checkout", "/other"])).toThrow("--session already identifies");
  expect(selectInstance([instance], {}, "/elsewhere")).toEqual(instance);
  expect(selectInstance([instance, { ...instance, instanceId: "other", checkout: "/other" }], { checkout: instance.checkout })).toEqual(instance);
  expect(() => selectInstance([instance, { ...instance, instanceId: "other", checkout: "/other" }], {}, "/elsewhere")).toThrow("several instances");
  expect(() => selectInstance([instance], { instance: "missing" })).toThrow("no matching instance");
});

test("wait parser uses shared camelCase conditions and rejects conflicting conditions", () => {
  expect(conditionFromArguments({ "state-contains": "/composer/text=a=b" })).toEqual({ on: "stateContains", pointer: "/composer/text", contains: "a=b" });
  expect(conditionFromArguments({ frame: "12" })).toEqual({ on: "frame", after: 12 });
  expect(() => conditionFromArguments({ id: "composer", state: "/x=true" })).toThrow("exactly one");
  expect(() => conditionFromArguments({ frame: "-1" })).toThrow("non-negative integer");
});

test("default transport lists instances using the authenticated daemon without discovery", async () => {
  const result = await runDaemonCli(["instances", "--json"]);
  expect(result.code).toBe(0);
  expect(JSON.parse(result.stdout)).toEqual([instance]);
  expect(result.hellos).toHaveLength(1);
  expect(result.hellos[0]).toMatchObject({ type: "hello", protocolVersion: PROTOCOL_VERSION, token: result.token });
  expect(result.requests).toHaveLength(1);
  expect(result.requests[0].command).toEqual({ type: "automation", command: { type: "instances" } });
  expect(result.stdout + result.stderr).not.toContain(result.token);
});

test("one CLI call targets the named checkout and preserves its action cursor and effect", async () => {
  const result = await runDaemonCli(["--checkout", instance.checkout, "--json", "type", "test", "--into", "composer.input"]);
  expect(result.code).toBe(0);
  expect(JSON.parse(result.stdout)).toEqual(effect);
  expect(result.requests.map(request => request.command.command)).toEqual([
    { type: "instances" }, { type: "attach", instanceId: instance.instanceId },
    { type: "call", sessionId, call: { type: "type", text: "test", target: { id: "composer.input" } } },
    { type: "detach", sessionId },
  ]);
});

test("diagnostic commands use the daemon directly with no executor or automation session", async () => {
  const query = { minLevel: "warn", source: "driver" };
  const logs = await runDaemonCli(["logs", "--filter", JSON.stringify(query), "--json"], command => {
    expect(command).toEqual({ type: "diagnosticLogs", query });
    return { type: "diagnosticLogs", result: { events: [], cursor: null, truncated: false } };
  });
  expect(logs.code).toBe(0);
  expect(logs.requests).toHaveLength(1);
  const counters = await runDaemonCli(["counters", "--json"], command => {
    expect(command).toEqual({ type: "diagnosticCounters" });
    return { type: "diagnosticCounters", result: { processes: [] } };
  });
  expect(counters.code).toBe(0);
  expect(counters.requests).toHaveLength(1);
});

test("daemon failures preserve all bundle fields in human and JSON output and redact credentials", async () => {
  const bundle = { observed: { tree: { nodes: [] }, health: { daemon: "connected" } }, treeDiff: { removed: ["composer.send"] }, recentEvents: [], counters: {}, diagnosticErrors: [], futureField: "preserved" };
  for (const json of [false, true]) {
    const result = await runDaemonCli(["click", "missing", ...(json ? ["--json"] : [])], command => {
      if (command.type === "automation" && command.command.type === "call") {
        return automation({ type: "error", error: { code: "not_found", message: "cannot find missing", target: "missing", nearest: ["composer.send"], bundle } });
      }
      return undefined;
    });
    expect(result.code).toBe(1);
    expect(result.stdout).toBe("");
    expect(result.stderr).toContain("preserved");
    expect(result.stderr).toContain("composer.send");
    if (json) expect(JSON.parse(result.stderr)).toMatchObject({ ok: false, error: { code: "not_found", bundle } });
  }
  expect(redact({ token: "secret", nested: { message: "echo secret" } }, ["secret"])).not.toContain("secret");
});

test("async waits only poll their daemon-owned result, and retain timeout evidence", async () => {
  const waitId = "55555555-5555-4555-8555-555555555555";
  const bundle = { observed: { state: { ready: false } }, treeDiff: { removed: [] }, counters: {}, recentEvents: [] };
  const result = await runDaemonCli(["wait", "--state", "/ready=true", "--timeout", "5", "--json"], command => {
    if (command.type !== "automation") return;
    if (command.command.type === "wait") {
      expect(command.command).toEqual({ type: "wait", sessionId, condition: { on: "state", pointer: "/ready", equals: true }, timeoutMs: 5 });
      return automation({ type: "wait", wait: { waitId, status: "pending", cursor, result: null, error: null } });
    }
    if (command.command.type === "waitResult") return automation({ type: "wait", wait: { waitId, status: "failed", cursor, result: null, error: { code: "timeout", message: "not observed", bundle } } });
  });
  expect(result.code).toBe(1);
  expect(JSON.parse(result.stderr)).toMatchObject({ error: { code: "timeout", bundle, wait: { waitId, status: "failed" } } });
  expect(result.requests.some(request => request.command.command?.type === "call")).toBe(false);
});

test("fresh-build wait reconnects after transport loss and verifies the new executor", async () => {
  let lists = 0;
  const fresh = { ...instance, build: "build-two", pid: 456 };
  const result = await runDaemonCli(["wait-build", "--instance", instance.instanceId, "--after-build", "build-one", "--timeout", "3000", "--json"], (command, socket) => {
    if (command.type !== "automation") return;
    if (command.command.type === "instances") {
      lists++;
      if (lists === 1) { socket.close(); return automation({ type: "instances", instances: [] }); }
      return automation({ type: "instances", instances: [fresh] });
    }
    if (command.command.type === "attach") return automation({ type: "attached", session: { ...session, instance: fresh }, replay: { events: [], cursor, expiry: null } });
    if (command.command.type === "call") return automation({ type: "called", result: { ...effect, value: { instance: fresh } } });
  });
  expect(result.code).toBe(0);
  expect(JSON.parse(result.stdout)).toEqual(fresh);
  expect(result.hellos.length).toBeGreaterThanOrEqual(2);
  expect(result.requests.some(request => request.command.command?.call?.type === "describe")).toBe(true);
});

test("fresh-build waits do not accept the old build and expose the final observed identities", async () => {
  const result = await runDaemonCli(["wait-build", "--after-build", instance.build, "--timeout", "40", "--json"]);
  expect(result.code).toBe(1);
  expect(JSON.parse(result.stderr)).toMatchObject({ error: { code: "timeout", afterBuild: instance.build, instances: [instance] } });
});

test("scenario start is daemon-owned and leaves its resumable session alive", async () => {
  const directory = mkdtempSync(join(tmpdir(), "waku-rpa-scenario-"));
  directories.push(directory);
  const path = join(directory, "smoke.json");
  const scenario = { name: "read only", steps: [{ name: "snapshot", type: "call", call: { type: "snapshot", sections: [] } }] };
  writeFileSync(path, JSON.stringify(scenario));
  const run = { runId, name: scenario.name, status: "pending", steps: [], cursor, error: null };
  const result = await runDaemonCli(["scenario", "start", path, "--json"], command => {
    if (command.type === "automation" && command.command.type === "scenarioStart") {
      expect(command.command).toEqual({ type: "scenarioStart", sessionId, scenario });
      return automation({ type: "scenario", run });
    }
  });
  expect(result.code).toBe(0);
  expect(JSON.parse(result.stdout)).toEqual({ sessionId, ...run });
  expect(result.requests.some(request => request.command.command?.type === "detach")).toBe(false);
});

test("scenario status and abort resume the named session without guessing an instance", async () => {
  for (const operation of ["status", "abort"]) {
    const result = await runDaemonCli(["scenario", operation, runId, "--session", sessionId, "--json"], command => {
      if (command.type === "automation" && ["scenarioResult", "scenarioAbort"].includes(command.command.type)) {
        return automation({ type: "scenario", run: { runId, name: "smoke", status: operation === "abort" ? "aborted" : "succeeded", steps: [], cursor, error: null } });
      }
    });
    expect(result.code).toBe(0);
    expect(result.requests.map(request => request.command.command)).toEqual([
      { type: "resume", sessionId, after: null },
      { type: operation === "abort" ? "scenarioAbort" : "scenarioResult", sessionId, runId },
    ]);
  }
});

test("scenario failures keep failed steps and the complete failure bundle", async () => {
  const bundle = { observed: { state: {} }, treeDiff: { removed: [] }, counters: {}, recentEvents: [] };
  const result = await runDaemonCli(["scenario", "status", runId, "--session", sessionId, "--json"], command => {
    if (command.type === "automation" && command.command.type === "scenarioResult") return automation({ type: "scenario", run: {
      runId, name: "smoke", status: "failed", steps: [{ name: "missing core id", status: "failed", result: null, error: { code: "timeout", message: "missing", bundle } }],
      cursor, error: { code: "timeout", message: "missing", bundle },
    } });
  });
  expect(result.code).toBe(1);
  expect(JSON.parse(result.stderr)).toMatchObject({ error: { bundle, run: { sessionId, runId, steps: [{ name: "missing core id", error: { bundle } }] } } });
});

test("resume sends a caller's saved cursor over the shared transport", async () => {
  const after = { ...cursor, sequence: 0 };
  const result = await runDaemonCli(["resume", "--session", sessionId, "--after", JSON.stringify(after), "--json"]);
  expect(result.code).toBe(0);
  expect(result.requests[0].command.command).toEqual({ type: "resume", sessionId, after });
});

test("fresh-build verification rejects a stale executor behind a new registry entry", async () => {
  let descriptions = 0;
  const fresh = { ...instance, build: "build-two" };
  const result = await runDaemonCli(["wait-build", "--after-build", "build-one", "--timeout", "1000", "--json"], command => {
    if (command.type !== "automation") return;
    if (command.command.type === "instances") return automation({ type: "instances", instances: [fresh] });
    if (command.command.type === "attach") return automation({ type: "attached", session: { ...session, instance: fresh }, replay: { events: [], cursor, expiry: null } });
    if (command.command.type === "call") return automation({ type: "called", result: { ...effect, value: { instance: ++descriptions === 1 ? instance : fresh } } });
  });
  expect(result.code).toBe(0);
  expect(descriptions).toBe(2);
  expect(JSON.parse(result.stdout).build).toBe("build-two");
});

test("scratch lifecycle uses typed daemon commands without attaching a working instance", async () => {
  const created = await runDaemonCli(["scratch", "create", "--name", "cli-test", "--json"], command => {
    expect(command).toEqual({ type: "automation", command: { type: "scratchCreate", name: "cli-test", seed: { projects: [], sessions: [] } } });
    return automation({ type: "scratchCreated", instance: { ...instance, scratch: true } });
  });
  expect(created.code).toBe(0);
  expect(created.requests).toHaveLength(1);
  const removed = await runDaemonCli(["scratch", "remove", instance.instanceId, "--json"], command => {
    expect(command).toEqual({ type: "automation", command: { type: "scratchRemove", instanceId: instance.instanceId } });
    return automation({ type: "scratchRemoved", instanceId: instance.instanceId });
  });
  expect(removed.code).toBe(0);
  expect(removed.requests).toHaveLength(1);
});

test("redaction covers JSON-escaped credentials and named secret fields", () => {
  const secret = 'a"b\\c\nsecret';
  const redacted = redact({ message: `peer echoed ${secret}`, authorization: "anything" }, [secret]);
  expect(redacted).not.toContain(JSON.stringify(secret).slice(1, -1));
  expect(redacted).not.toContain("anything");
});

test("scratch creation forwards an explicit seed file without reading a working database", async () => {
  const directory = mkdtempSync(join(tmpdir(), "waku-rpa-scratch-seed-"));
  directories.push(directory);
  const path = join(directory, "seed.json");
  const seed = { projects: [], sessions: [] };
  writeFileSync(path, JSON.stringify(seed));
  const result = await runDaemonCli(["scratch", "create", "--seed", path, "--json"], command => {
    expect(command.command).toEqual({ type: "scratchCreate", name: null, seed });
    return automation({ type: "scratchCreated", instance: { ...instance, scratch: true } });
  });
  expect(result.code).toBe(0);
});

test("committed fan-out scenario observes real serialized child state without executing a provider", async () => {
  const scenario = await Bun.file(join(import.meta.dir, "../rpa/scenarios/fan-out-real-model.json")).json();
  expect(scenario.name).toContain("explicit opt-in");
  expect(scenario.steps.filter((step: any) => step.call?.verb === "submitPrompt")).toHaveLength(1);
  expect(scenario.steps.filter((step: any) => step.type === "wait").map((step: any) => step.condition)).toContainEqual({
    on: "state", pointer: "/session/subagent_runs/0/children/0/state", equals: "completed",
  });
  expect(scenario.steps.filter((step: any) => step.type === "wait").map((step: any) => step.condition)).toContainEqual({
    on: "state", pointer: "/session/subagent_runs/0/children/1/state", equals: "completed",
  });
  expect(scenario.steps.at(-1).call).toEqual({ type: "snapshot", sections: [] });
  for (const step of scenario.steps) {
    expect(step.name).toBeString();
    expect(["call", "wait"]).toContain(step.type);
    if (step.type === "wait") expect(step.timeoutMs).toBeGreaterThan(0);
  }
});

test("direct diagnostic commands reject target selectors instead of querying a different daemon", async () => {
  for (const command of ["logs", "timeline", "counters", "verbosity"]) {
    for (const selector of [["--instance", instance.instanceId], ["--checkout", instance.checkout], ["--session", sessionId]]) {
      const result = await runDaemonCli([command, ...selector, "--json"]);
      expect(result.code).toBe(1);
      expect(result.stdout).toBe("");
      expect(JSON.parse(result.stderr).error.message).toContain("queries the connected daemon directly");
      expect(result.hellos).toHaveLength(0);
      expect(result.requests).toHaveLength(0);
    }
  }
});

test("checkout targeting prefers the current live app over retained disconnected generations", () => {
  const offline = { ...instance, executorConnected: false };
  const fresh = { ...instance, instanceId: "new-process", build: "build-two" };
  const anotherOld = { ...offline, instanceId: "older-process" };
  expect(selectInstance([offline, anotherOld, fresh], { checkout: instance.checkout })).toEqual(fresh);
  expect(selectInstance([offline, fresh], {}, instance.checkout)).toEqual(fresh);
  expect(selectInstance([offline, fresh], { instance: offline.instanceId })).toEqual(offline);
  expect(() => selectInstance([fresh, { ...fresh, instanceId: "scratch", scratch: true }], { checkout: instance.checkout })).toThrow("several instances");
  expect(() => selectInstance([offline, anotherOld], { checkout: instance.checkout })).toThrow("no matching instance");
});

test("fresh-build wait follows a changed process UUID and pins the initially selected checkout across reconnect", async () => {
  for (const target of [[], ["--checkout", instance.checkout]]) {
    let lists = 0;
    const fresh = { ...instance, instanceId: "99999999-9999-4999-8999-999999999999", build: "build-two", pid: 456 };
    const unrelated = { ...fresh, instanceId: "88888888-8888-4888-8888-888888888888", checkout: "/another-checkout" };
    const result = await runDaemonCli(["wait-build", ...target, "--timeout", "2000", "--json"], (command, socket) => {
      if (command.type !== "automation") return;
      if (command.command.type === "instances") {
        lists++;
        if (lists === 1) return automation({ type: "instances", instances: [instance] });
        if (lists === 2) { socket.close(); return null; }
        return automation({ type: "instances", instances: [{ ...instance, executorConnected: false }, fresh, unrelated] });
      }
      if (command.command.type === "attach") {
        expect(command.command.instanceId).toBe(fresh.instanceId);
        return automation({ type: "attached", session: { ...session, instance: fresh }, replay: { events: [], cursor, expiry: null } });
      }
      if (command.command.type === "call") return automation({ type: "called", result: { ...effect, value: { instance: fresh } } });
    });
    expect(result.code).toBe(0);
    expect(JSON.parse(result.stdout)).toEqual(fresh);
    expect(result.hellos.length).toBeGreaterThanOrEqual(2);
  }
});

test("fresh-build wait keeps an explicit process UUID exact after another app replaces it", async () => {
  const fresh = { ...instance, instanceId: "99999999-9999-4999-8999-999999999999", build: "build-two" };
  const result = await runDaemonCli(["wait-build", "--instance", instance.instanceId, "--after-build", instance.build, "--timeout", "40", "--json"], command => {
    if (command.type === "automation" && command.command.type === "instances") return automation({ type: "instances", instances: [{ ...instance, executorConnected: false }, fresh] });
  });
  expect(result.code).toBe(1);
  expect(JSON.parse(result.stderr).error.code).toBe("timeout");
  expect(result.requests.some(request => request.command.command.type === "attach")).toBe(false);
});

test("fresh-build deadline bounds every RPC and cleanup cannot replace verified success", async () => {
  const fresh = { ...instance, build: "build-two" };
  for (const hung of ["instances", "attach", "call", "detach"]) {
    const started = Date.now();
    const result = await runDaemonCli(["wait-build", "--after-build", instance.build, "--timeout", "60", "--json"], command => {
      if (command.type !== "automation") return;
      if (command.command.type === hung) return null;
      if (command.command.type === "instances") return automation({ type: "instances", instances: [fresh] });
      if (command.command.type === "attach") return automation({ type: "attached", session: { ...session, instance: fresh }, replay: { events: [], cursor, expiry: null } });
      if (command.command.type === "call") return automation({ type: "called", result: { ...effect, value: { instance: fresh } } });
    });
    expect(Date.now() - started).toBeLessThan(750);
    expect(result.code).toBe(hung === "detach" ? 0 : 1);
    if (hung === "detach") expect(JSON.parse(result.stdout)).toEqual(fresh);
    else expect(JSON.parse(result.stderr).error.code).toBe("timeout");
  }
});


test("ordinary tree and pointer-state reads select only the requested section", () => {
  expect(snapshotSections("tree")).toEqual(["tree"]);
  expect(snapshotSections("state", "/composer/text")).toEqual(["composer"]);
  expect(snapshotSections("state", "/runtime/permission")).toEqual(["runtime"]);
  expect(snapshotSections("state", "/a~1b/~0")).toEqual(["a/b"]);
  expect(snapshotSections("state")).toEqual(snapshotSections("inspect"));
  expect(snapshotSections("snap")).toEqual([]);
  expect(snapshotSections("state", "/composer", "composer,route")).toEqual(["composer", "route"]);
  expect(snapshotSections("inspect")).not.toContain("session");
  expect(snapshotSections("inspect")).not.toContain("model");
  expect(() => snapshotSections("state", "composer")).toThrow("JSON pointer");
  expect(() => snapshotSections("state", "/composer~2")).toThrow("JSON pointer");
});

test("daemon tree and state requests avoid full transcript serialization", async () => {
  for (const [command, pointer, sections] of [["tree", "", ["tree"]], ["state", "/composer/text", ["composer"]], ["inspect", "", snapshotSections("inspect")]] as const) {
    const result = await runDaemonCli([command, ...(pointer ? [pointer] : []), "--json"], command => {
      if (command.type === "automation" && command.command.type === "call") {
        expect(command.command.call).toEqual({type:"snapshot", sections});
        return {type:"automation", result:{type:"called", result:{...effect, value:{state:{composer:{text:"safe"}},tree:{nodes:[]}}}}};
      }
    });
    expect(result.code).toBe(0);
  }
});


test("transcript queries preserve separate attachment and conversation identities", async () => {
  const conversationId = "55555555-5555-4555-8555-555555555555";
  const query = {type:"index",collection:"messages",offset:0,limit:2};
  const result = await runDaemonCli(["--session", sessionId, "transcript", "--conversation", conversationId, JSON.stringify(query), "--json"], command => {
    if (command.type === "automation" && command.command.type === "call") {
      expect(command.command.sessionId).toBe(sessionId);
      expect(command.command.call).toEqual({type:"transcript",conversationId,query});
      return {type:"automation",result:{type:"called",result:{...effect,value:{conversationId,source:"persisted",detailLoaded:false,page:{type:"index",items:[],total:0,nextOffset:null}}}}};
    }
  });
  expect(result.code).toBe(0);
  expect(JSON.parse(result.stdout)).toMatchObject({conversationId,source:"persisted",detailLoaded:false});
});


test("ordinary describe exposes the discoverable argument contracts", async () => {
  const manifest = {instance,capabilities:{readOnly:["describe","snapshot","transcript"],verbs:[{name:"openSession",args:{id:"conversation UUID"}}]}};
  const result = await runDaemonCli(["describe"], command => {
    if (command.type === "automation" && command.command.type === "call") return {type:"automation",result:{type:"called",result:{...effect,value:manifest}}};
  });
  expect(result.code).toBe(0);
  expect(JSON.parse(result.stdout)).toEqual(manifest);
});
