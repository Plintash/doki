// Opt-in native acceptance checks through the existing daemon connection.
// Never starts an app, submits a prompt, or changes a working task.
// WAKU_RPA_DAEMON_TEST=1 WAKU_DAEMON_ADDRESS=... WAKU_DAEMON_TOKEN=... \
//   bun test scripts/rpa-daemon-live.test.ts
// Optionally target WAKU_RPA_TEST_INSTANCE or WAKU_RPA_TEST_CHECKOUT.
import { afterAll, beforeAll, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AutomationInstance } from "../packages/waku-client/src/generated";
import { selectInstance } from "./rpa";

const enabled = process.env.WAKU_RPA_DAEMON_TEST === "1";
const liveTest = enabled ? test : test.skip;
const target = process.env.WAKU_RPA_TEST_INSTANCE
  ? ["--instance", process.env.WAKU_RPA_TEST_INSTANCE]
  : process.env.WAKU_RPA_TEST_CHECKOUT ? ["--checkout", process.env.WAKU_RPA_TEST_CHECKOUT] : [];
let selected: AutomationInstance;
let directory: string | undefined;

async function cli(args: string[], useTarget = true) {
  const child = Bun.spawn([process.execPath, join(import.meta.dir, "rpa.ts"), "--json", ...(useTarget ? target : []), ...args], {
    env: { ...process.env, WAKU_RPA_CONFIG: "" }, stdout: "pipe", stderr: "pipe",
  });
  const [code, stdout, stderr] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
  const token = process.env.WAKU_DAEMON_TOKEN!;
  // Failure text is already redacted by the CLI. Never put credentials in an
  // assertion's expected/actual pair or a native test's diagnostic output.
  if (stdout.includes(token) || stderr.includes(token)) throw new Error("CLI output exposed its transport credential");
  return { code, value: stdout ? JSON.parse(stdout) : null, error: stderr ? JSON.parse(stderr).error : null };
}

beforeAll(async () => {
  if (!enabled) return;
  if (!process.env.WAKU_DAEMON_ADDRESS || !process.env.WAKU_DAEMON_TOKEN) throw new Error("native daemon tests require WAKU_DAEMON_ADDRESS and WAKU_DAEMON_TOKEN");
  const result = await cli(["instances"]);
  expect(result.code).toBe(0);
  selected = selectInstance(result.value, { instance: process.env.WAKU_RPA_TEST_INSTANCE, checkout: process.env.WAKU_RPA_TEST_CHECKOUT });
  expect(selected.executorConnected).toBe(true);
}, 20_000);

afterAll(() => { if (directory) rmSync(directory, { recursive: true, force: true }); });

liveTest("native CLI reaches the selected executor with no app-local discovery", async () => {
  const description = await cli(["describe"]);
  expect(description.code).toBe(0);
  expect(description.value.instance.checkout).toBe(selected.checkout);
  expect(description.value.instance.build).toBe(selected.build);
  const snapshot = await cli(["snap"]);
  expect(snapshot.code).toBe(0);
  expect(snapshot.value.tree.nodes.length).toBeGreaterThan(0);
  expect(snapshot.value.health.daemon.connected).toBe(true);
  expect(snapshot.value.state).toBeObject();
}, 20_000);

liveTest("native CLI exposes daemon diagnostics over the same authenticated transport", async () => {
  for (const command of [["logs", "--filter", '{"limit":5}'], ["counters"], ["verbosity"]]) {
    const result = await cli(command, false);
    expect(result.code).toBe(0);
    expect(result.value).toBeObject();
  }
}, 20_000);

liveTest("native on-demand snapshots advance paint counters without a legacy endpoint", async () => {
  const before = await cli(["snap", "--sections", "tree,route"]);
  const after = await cli(["snap", "--sections", "tree,route"]);
  expect(before.code).toBe(0);
  expect(after.code).toBe(0);
  for (const counter of ["framesDrawn", "renderCommits"]) {
    expect(after.value.health.counters.values[counter]).toBeGreaterThan(before.value.health.counters.values[counter]);
  }
}, 20_000);

liveTest("native failed wait includes its failure-time UI and daemon evidence", async () => {
  const result = await cli(["wait", "--id", `rpa-daemon-test.missing.${crypto.randomUUID()}`, "--timeout", "30"]);
  expect(result.code).toBe(1);
  expect(result.error.code).toBe("wait_timeout");
  expect(result.error.bundle.observed.tree.nodes.length).toBeGreaterThan(0);
  expect(result.error.bundle.observed.health.daemon.connected).toBe(true);
  expect(result.error.bundle.treeDiff).toBeObject();
  expect(result.error.bundle.recentEvents.events).toBeArray();
  expect(result.error.bundle.counters.processes).toBeArray();
}, 20_000);

liveTest("native standalone diagnostic timeout captures healthy UI evidence", async () => {
  const result = await cli(["wait", "--diagnostic", JSON.stringify({ source: `rpa-never.${crypto.randomUUID()}` }), "--timeout", "30"]);
  expect(result.code).toBe(1);
  expect(result.error.code).toBe("wait_timeout");
  expect(result.error.bundle.observed.tree.nodes.length).toBeGreaterThan(0);
  expect(result.error.bundle.observed.health.daemon.connected).toBe(true);
  expect(result.error.bundle.treeDiff).toBeObject();
  expect(result.error.bundle.recentEvents.events).toBeArray();
}, 20_000);

liveTest("native scenario survives CLI disconnect, stops at a failed step, and keeps its evidence", async () => {
  directory = mkdtempSync(join(tmpdir(), "waku-rpa-daemon-live-"));
  const path = join(directory, "read-only-failure.json");
  writeFileSync(path, JSON.stringify({ name: "native read-only CLI regression", steps: [
    { name: "capture native frame", type: "call", call: { type: "snapshot", sections: [] } },
    { name: "deliberately absent id", type: "wait", condition: { on: "node", id: `rpa-daemon-test.missing.${crypto.randomUUID()}`, present: true }, timeoutMs: 30 },
    { name: "must not run", type: "call", call: { type: "snapshot", sections: [] } },
  ] }));
  const started = await cli(["scenario", "start", path]);
  const initialRun = started.value ?? started.error?.run;
  expect(initialRun?.runId).toBeString();
  expect(initialRun?.sessionId).toBeString();
  const { runId, sessionId } = initialRun;
  try {
    const deadline = Date.now() + 10_000;
    let result = await cli(["scenario", "status", runId, "--session", sessionId], false);
    while (result.value?.status === "pending" && Date.now() < deadline) {
      await Bun.sleep(100);
      result = await cli(["scenario", "status", runId, "--session", sessionId], false);
    }
    expect(result.code).toBe(1);
    expect(result.error.run.status).toBe("failed");
    expect(result.error.run.steps[0].status).toBe("succeeded");
    expect(result.error.run.steps[1].status).toBe("failed");
    expect(result.error.run.steps[2]?.status).not.toBe("succeeded");
    expect(result.error.bundle.observed.tree.nodes.length).toBeGreaterThan(0);
  } finally {
    const detached = await cli(["detach", "--session", sessionId], false);
    expect(detached.code).toBe(0);
  }
}, 20_000);
