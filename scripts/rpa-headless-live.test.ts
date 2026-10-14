// Actual daemon/CLI acceptance. No display, GUI process, provider profile or
// credentials are inherited. Every process/data directory belongs to this test.
import { expect, test } from "bun:test";
import { mkdtempSync, mkdirSync, readFileSync, readdirSync, readlinkSync, realpathSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, resolve } from "node:path";
import { WakuClient } from "../packages/waku-client/src/client";
import type { AutomationCommand, AutomationResult, AutomationRun, AutomationReplay } from "../packages/waku-client/src/generated";

const live = process.env.WAKU_RPA_HEADLESS_TEST === "1" ? test : test.skip;

async function waitFor<T>(read: () => Promise<T>, ready: (value: T) => boolean, ms = 15000): Promise<T> {
  const deadline = Date.now() + ms;
  while (true) {
    const value = await read();
    if (ready(value)) return value;
    if (Date.now() > deadline) throw new Error("headless acceptance deadline");
    await Bun.sleep(25);
  }
}

async function fixture(existingDirectory?: string) {
  const directory = existingDirectory ?? mkdtempSync(join(tmpdir(), "doki-headless-"));
  const home = join(directory, "home");
  mkdirSync(home, { recursive: true });
  const token = crypto.randomUUID();
  const executable = process.env.WAKU_RPA_DAEMON_BIN;
  if (!executable) throw new Error("set WAKU_RPA_DAEMON_BIN to the freshly built debug daemon");
  const env = {
    PATH: process.env.PATH ?? "", LD_LIBRARY_PATH: process.env.LD_LIBRARY_PATH ?? "",
    HOME: home, USERPROFILE: home, XDG_CONFIG_HOME: join(directory, "config"),
    XDG_DATA_HOME: join(directory, "data"), WAKU_RPA_DATA_DIR: directory,
    WAKU_DAEMON_TOKEN: token,
  };
  const child = Bun.spawn([executable, "--automation-headless", "--parent-pid", String(process.pid)], {
    env, cwd: directory, stdout: "pipe", stderr: "pipe",
  });
  const stderr = new Response(child.stderr).text();
  const reader = child.stdout.getReader();
  const initial = await Promise.race([
    reader.read(),
    Bun.sleep(10000).then(() => { throw new Error("headless daemon readiness timeout"); }),
  ]);
  const ready = JSON.parse(new TextDecoder().decode(initial.value).trim());
  const client = new WakuClient({ address: ready.address, token, requestTimeoutMs: 40000 });
  await client.connect();
  await waitFor(() => client.automationInstances(), values => values.some(value => value.headless && value.executorConnected));
  async function cli(args: string[]) {
    const cli = Bun.spawn([process.execPath, join(import.meta.dir, "rpa.ts"), ...args, "--json"], {
      env: { ...env, WAKU_DAEMON_ADDRESS: ready.address }, stdout: "pipe", stderr: "pipe",
    });
    const [code, out, err] = await Promise.all([cli.exited, new Response(cli.stdout).text(), new Response(cli.stderr).text()]);
    expect(out + err).not.toContain(token);
    return { code, out, err, value: out.trim() ? JSON.parse(out) : undefined };
  }
  return {
    directory, client, cli, pid: child.pid,
    async close(remove = true) {
      client.disconnect();
      if (child.exitCode === null) child.kill();
      await child.exited;
      expect(await stderr).not.toContain(token);
      reader.releaseLock();
      if (remove) rmSync(directory, { recursive: true, force: true });
    },
  };
}

async function command(client: WakuClient, command: AutomationCommand): Promise<AutomationResult> {
  return client.automation(command, { timeoutMs: 40000 });
}

async function finish(client: WakuClient, sessionId: string, runId: string): Promise<AutomationRun> {
  await command(client, { type: "resume", sessionId, after: null });
  const result = await waitFor(() => command(client, { type: "scenarioResult", sessionId, runId }),
    result => result.type === "scenario" && result.run.status !== "pending");
  if (result.type !== "scenario") throw new Error("missing scenario result");
  return result.run;
}

live("actual CLI records semantic operations and replays fresh identities without a window", async () => {
  const f = await fixture();
  try {
    const create = await f.cli(["scratch", "create", "--headless"]);
    expect(create.code).toBe(0);
    const original = create.value.instance;
    expect(original.headless).toBe(true);
    expect(original.scratch).toBe(true);
    expect(original.build).toMatch(/^daemon-[0-9a-f]{16}$/);
    console.info(`Verified daemon-only executor ${original.build} (${original.version})`);
    expect(original.pid).not.toBe(f.pid);
    if (process.platform === "linux") {
      const environment = readFileSync(`/proc/${original.pid}/environ`, "utf8").split("\0");
      expect(environment.some(entry => entry.startsWith("DISPLAY=") || entry.startsWith("WAYLAND_DISPLAY="))).toBe(false);
      expect(readlinkSync(`/proc/${original.pid}/exe`)).toBe(realpathSync(process.env.WAKU_RPA_DAEMON_BIN!));
      const processes = Bun.spawnSync(["ps", "--ppid", String(original.pid), "-o", "comm="]);
      expect(processes.stdout.toString().trim()).toBe("");
    }
    const inspection = await f.cli(["--instance", original.instanceId, "inspect"]);
    expect(inspection.code).toBe(0);
    expect(inspection.value.headless).toBe(true);
    const start = await f.cli(["--instance", original.instanceId, "record", "start", "portable draft"]);
    expect(start.code).toBe(0);
    const sessionId: string = start.value.sessionId;
    const call = async (verb: string, args: object) => {
      const result = await f.cli(["--session", sessionId, "invoke", verb, JSON.stringify(args)]);
      expect(result.code, result.err).toBe(0);
      return result.value.value;
    };
    const created = await call("newSession", {});
    const originalId = created.opened;
    await call("renameSession", { id: originalId, title: "Recorded session title" });
    await call("setDraft", { id: originalId, text: "Safe portable draft" });
    const waited = await f.cli(["--session", sessionId, "wait", "--state", '/draft/text="Safe portable draft"', "--timeout", "1000"]);
    expect(waited.code).toBe(0);
    const observed = await f.cli(["--session", sessionId, "snap", "--sections", "sessions,draft"]);
    expect(observed.code).toBe(0);
    const stopped = await f.cli(["--session", sessionId, "record", "stop"]);
    expect(stopped.code).toBe(0);
    const replay: AutomationReplay = stopped.value;
    expect(replay.formatVersion).toBe(1);
    expect(JSON.stringify(replay)).not.toContain(originalId);
    expect(JSON.stringify(replay)).not.toContain("Safe portable draft");
    expect(JSON.stringify(replay)).not.toContain("Recorded session title");
    expect(replay.steps[1]!.bindings.some(binding => binding.source.type === "stepResult")).toBe(true);
    const replayFile = join(f.directory, "recording.json");
    const inputsFile = join(f.directory, "inputs.json");
    writeFileSync(replayFile, JSON.stringify(replay));
    writeFileSync(inputsFile, JSON.stringify({ text1: "Recorded session title", text2: "Safe portable draft", text3: "Safe portable draft" }));
    const fresh = await f.cli(["scratch", "create", "--headless"]);
    expect(fresh.code).toBe(0);
    const freshDirectory = dirname(fresh.value.instance.database);
    const launched = await f.cli(["--instance", fresh.value.instance.instanceId, "replay", replayFile, "--parameters", inputsFile]);
    expect(launched.code).toBe(0); // CLI has exited; daemon still owns replay.
    const run = await finish(f.client, launched.value.sessionId, launched.value.runId);
    expect(run.status).toBe("succeeded");
    const replayId = (run.steps[0]!.result as any).value.opened;
    expect(replayId).not.toBe(originalId);
    const final = (run.steps.at(-1)!.result as any).value;
    expect(final.state.sessions.find((session: any) => session.id === replayId).title).toBe(observed.value.state.sessions.find((session: any) => session.id === originalId).title);
    expect(final.state.draft.text).toBe(observed.value.state.draft.text);
    await waitFor(async () => existsSync(freshDirectory), exists => !exists);
    const unsupported = await f.cli(["--instance", original.instanceId, "click", "composer-field"]);
    expect(unsupported.code).toBe(1);
    expect(unsupported.err).toContain("unsupported_headless");
    const uiWait = await f.cli(["--instance", original.instanceId, "wait", "--id", "composer-field", "--timeout", "100"]);
    expect(uiWait.code).toBe(1);
    expect(uiWait.err).toContain("unsupported_headless");
    const removed = await f.cli(["scratch", "remove", original.instanceId]);
    expect(removed.code).toBe(0);
    expect(existsSync(dirname(original.database))).toBe(false);
  } finally { await f.close(); }
}, 90000);


const describeProgram = (name = "describe") => ({ type: "scenario", scenario: {
  name, steps: [{ name: "describe", type: "call", call: { type: "describe" } }],
} });
const waitingProgram = (timeoutMs = 10000) => ({ type: "scenario", scenario: {
  name: "wait for absent state", steps: [
    { name: "create draft", type: "call", call: { type: "invoke", verb: "newSession", args: {} } },
    { name: "wait", type: "wait", condition: { on: "state", pointer: "/selection/selectedSession", equals: "never" }, timeoutMs },
  ],
} });

async function waitJob(f: Awaited<ReturnType<typeof fixture>>, runId: string) {
  return waitFor(async () => {
    const result = await command(f.client, { type: "jobResult", runId });
    if (result.type !== "job") throw new Error("missing job result");
    return result.run;
  }, run => run.status !== "pending");
}

live("unattended suites survive CLI exit and bound failure, timeout, cancellation and owned cleanup", async () => {
  const f = await fixture();
  const jobFile = join(f.directory, "job.json");
  const scratchEmpty = () => !existsSync(join(f.directory, "rpa-scratch")) || readdirSync(join(f.directory, "rpa-scratch")).length === 0;
  const start = async (programs: unknown[], timeoutMs = 10000) => {
    writeFileSync(jobFile, JSON.stringify({ name: "headless suite", headless: true, seed: { projects: [], sessions: [] }, timeoutMs, programs }));
    const result = await f.cli(["job", "start", jobFile]);
    expect(result.code).toBe(0);
    return result.value.runId as string;
  };
  try {
    const success = await waitJob(f, await start([describeProgram("one"), describeProgram("two")]));
    expect(success.status).toBe("succeeded");
    expect(success.programs).toHaveLength(2);
    const identities = success.programs.map(run => (run.steps[0]!.result as any).value.instance.instanceId);
    expect(identities[0]).not.toBe(identities[1]);
    expect(scratchEmpty()).toBe(true);
    const failure = await waitJob(f, await start([waitingProgram(100), describeProgram("must not run")]));
    expect(failure.status).toBe("failed");
    expect(failure.programs).toHaveLength(1);
    expect(failure.programs[0]!.steps).toHaveLength(2);
    expect(failure.programs[0]!.error?.bundle?.observed).toBeObject();
    expect(scratchEmpty()).toBe(true);
    const failedStatus = await f.cli(["job", "status", failure.runId]);
    expect(failedStatus.code).toBe(1);
    const timeout = await waitJob(f, await start([waitingProgram()], 1200));
    expect(timeout.status).toBe("failed");
    expect(timeout.error?.code).toBe("job_timeout");
    expect(timeout.programs).toHaveLength(1);
    expect(timeout.programs[0]!.steps.length).toBeGreaterThanOrEqual(1);
    expect(scratchEmpty()).toBe(true);
    const runId = await start([waitingProgram()]);
    await waitFor(() => f.client.automationInstances(), instances => instances.some(instance => instance.scratch));
    const abort = await f.cli(["job", "abort", runId]);
    expect([0, 1]).toContain(abort.code);
    const cancelled = await waitJob(f, runId);
    expect(cancelled.status).toBe("aborted");
    expect(scratchEmpty()).toBe(true);
    // The controller's own database and home are never cleanup targets.
    expect(existsSync(f.directory)).toBe(true);
    expect(existsSync(join(f.directory, "home"))).toBe(true);
  } finally { await f.close(); }
}, 90000);

live("local schedules persist future jobs and interrupted one-shots across real daemon restarts", async () => {
  let f = await fixture();
  const directory = f.directory;
  const scheduleFile = join(directory, "schedule.json");
  const job = (programs: unknown[]) => ({ name: "scheduled suite", headless: true, timeoutMs: 15000, seed: { projects: [], sessions: [] }, programs });
  const create = async (name: string, firstRunAtMs: number, programs: unknown[], intervalMs: number | null = null) => {
    writeFileSync(scheduleFile, JSON.stringify({ name, firstRunAtMs, intervalMs, job: job(programs) }));
    const result = await f.cli(["schedule", "create", scheduleFile]);
    expect(result.code).toBe(0);
    return result.value;
  };
  const status = async (id: string) => {
    const result = await f.cli(["schedule", "status", id]);
    expect(result.code).toBe(0);
    return result.value;
  };
  try {
    const future = await create("future", Date.now() + 60000, [describeProgram()]);
    expect((await status(future.id)).lastResult).toBeNull();
    const due = await create("due once", Date.now() - 1, [describeProgram()]);
    const completed = await waitFor(() => status(due.id), value => value.lastResult !== null);
    expect(completed.lastResult.status).toBe("succeeded");
    expect(completed.enabled).toBe(false);
    expect(completed.nextRunAtMs).toBeNull();
    const active = await create("interrupted once", Date.now() - 1, [waitingProgram()]);
    const claimed = await waitFor(() => status(active.id), value => value.activeRunId !== null);
    const originalRun = claimed.activeRunId;
    await f.close(false); // Abrupt process stop: persisted claim must not rerun.
    await Bun.sleep(750); // Owned child daemon sees its dead parent and exits.
    f = await fixture(directory);
    const restoredFuture = await status(future.id);
    expect(restoredFuture.nextRunAtMs).toBe(future.nextRunAtMs);
    expect(restoredFuture.lastResult).toBeNull();
    const interrupted = await status(active.id);
    expect(interrupted.activeRunId).toBeNull();
    expect(interrupted.enabled).toBe(false);
    expect(interrupted.lastResult.runId).toBe(originalRun);
    expect(interrupted.lastResult.error.code).toBe("schedule_interrupted");
    expect((await status(due.id)).lastResult.runId).toBe(completed.lastResult.runId);
    const disabled = await f.cli(["schedule", "disable", future.id]);
    expect(disabled.code).toBe(0);
    expect(disabled.value.enabled).toBe(false);
    const removed = await f.cli(["schedule", "remove", future.id]);
    expect(removed.code).toBe(0);
  } finally { await f.close(); }
}, 90000);
