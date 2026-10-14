#!/usr/bin/env bun
// The debug automation CLI uses the existing authenticated daemon connection.
// --config/WAKU_RPA_CONFIG explicitly opts into the transitional P0 endpoint.
import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import { parseArgs } from "node:util";
import { WakuClient, WakuConnectionError } from "../packages/waku-client/src/client";
import type {
  AutomationCall, AutomationCommand, AutomationCondition, AutomationCursor,
  AutomationInstance, AutomationResult, AutomationScenario, AutomationSession, AutomationReplay, AutomationJob, AutomationJobRun, AutomationScheduleDefinition, TranscriptQuery,
} from "../packages/waku-client/src/generated";

type Discovery = {
  protocolVersion: number;
  address: string;
  token: string;
  pid: number;
  checkout: string;
  database: string;
  app: string;
  build: string;
};
type Failure = { code?: string; message: string; target?: string | null; nearest?: string[] | null; bundle?: unknown; [key: string]: unknown };
type Reply = { id: number; ok: boolean; result?: any; error?: Failure };

export const LEGACY_PROTOCOL_VERSION = 3;

const HELP = `Usage: bun scripts/rpa.ts [--json] [--instance ID | --checkout PATH] <command> [args]

Commands:
  instances                      list registered instance identities and executor health
  describe                       instance, capabilities and argument contracts
  tree [pattern]                 controls on screen (id, role, label)
  state [pointer]                compact context, or one JSON pointer
  inspect                        compact agent context (no transcript history)
  snap [--sections a,b]           the full snapshot, or selected sections
  transcript --conversation ID QUERY_JSON
                                 bounded history index/text without changing selection
  logs [--filter JSON]            merged daemon/app/provider events
  timeline --conversation ID [--turn ID]
                                 conversation lifecycle (diagnostic sessionId)
  counters                       live samples attributed to each process
  verbosity [JSON]               read or replace the runtime level filter
  click <id>                     click a control by semantic id
  focus <id>                     focus a control by semantic id
  scroll <id> <direction>         scroll up, down, left, or right
  type <text> [--into ID]         type text, optionally focusing a control first
  key <chord>                    send a chord (e.g. secondary-l)
  invoke <verb> [args-json]       run a workspace verb
  wait [--id ID [--gone]] [--text ID=SUBSTR] [--state PTR=VALUE]
       [--state-contains PTR=SUBSTR] [--frame N] [--diagnostic JSON] [--timeout MS]
  wait-build [--after-build ID] [--timeout MS]
                                 follow the checkout across relaunches; --instance pins one process
  attach                         attach a persistent session and print its id
  resume --session ID [--after JSON]
                                 resume a persistent session from a saved cursor
  detach --session ID            release a persistent session
  diff --session ID --after JSON [--streams ui,model,diagnostic]
  scratch create [--seed seed.json] [--name NAME] [--headless]
                                 start an isolated disposable app (empty by default)
  scratch remove <instanceId>     stop and remove only that scratch instance
  record start <name>             record supported calls on the returned --session
  record status|stop|cancel --session ID
                                 stop prints a portable replay JSON document
  replay <file.json> [--parameters inputs.json]
                                 start replay with explicit required input values
  scenario start <file.json>      start a daemon-owned run; prints sessionId and runId
  scenario status <runId> --session ID
  scenario abort <runId> --session ID
  job start|run <job.json>         daemon-owned ordered suite on fresh scratch targets
  job status|wait|abort <runId>    inspect, await, or cancel an unattended suite
  schedule create <schedule.json> persist one-shot or interval schedule
  schedule list|status <id>       inspect durable next/last/active execution state
  schedule enable|disable|cancel|remove <id>
                                 disable affects future runs; cancel also stops active work
  raw '<json>'                   send a shared AutomationCommand (legacy: P0 request)

Issue verbs: openTask {id}, newTask {title?}; no title opens the full New issue draft editor.
Conversation verbs: openSession {id}, newSession {taskId?}; omit taskId for standalone.
Sections: task/tasks are Board issues; session/sessions are conversations; board is UI/save state.

Daemon: WAKU_DAEMON_ADDRESS and WAKU_DAEMON_TOKEN (the existing daemon credentials).
Legacy fallback only: --config <path> or WAKU_RPA_CONFIG. No discovery file is read by default.
A --session ID resumes an automation attachment instead of choosing a new instance.
Diagnostic --conversation and query sessionId identify a conversation, never that attachment.
logs/timeline/counters/verbosity query the connected daemon directly and reject
--instance, --checkout and --session; targeted action/wait failures retain instance-specific evidence.
Scenarios continue after the CLI exits; release their session with detach when finished.`;

export function parseArguments(args: string[]) {
  const parsed = parseArgs({
    args, allowPositionals: true,
    options: {
      config: { type: "string" }, json: { type: "boolean" }, help: { type: "boolean" },
      instance: { type: "string" }, checkout: { type: "string" }, session: { type: "string" },
      into: { type: "string" }, id: { type: "string" }, gone: { type: "boolean" },
      text: { type: "string" }, state: { type: "string" }, "state-contains": { type: "string" },
      timeout: { type: "string" }, filter: { type: "string" }, conversation: { type: "string" }, turn: { type: "string" },
      frame: { type: "string" }, diagnostic: { type: "string" }, "after-build": { type: "string" },
      headless: { type: "boolean" }, parameters: { type: "string" },
      name: { type: "string" }, seed: { type: "string" }, sections: { type: "string" }, after: { type: "string" }, streams: { type: "string" },
    },
  });
  if (parsed.values.instance && parsed.values.checkout) throw new Error("choose --instance or --checkout, not both");
  if (parsed.values.session && (parsed.values.instance || parsed.values.checkout)) {
    throw new Error("--session already identifies an instance; do not combine it with --instance or --checkout");
  }
  return { command: parsed.positionals[0], args: parsed.positionals.slice(1), options: parsed.values };
}
type Arguments = ReturnType<typeof parseArguments>;

/** Kept for opt-in P0 regression tests and the explicitly selected fallback. */
export async function call(discovery: Discovery, request: Record<string, unknown>): Promise<Reply> {
  if (discovery.protocolVersion !== LEGACY_PROTOCOL_VERSION) {
    throw new Error(`legacy endpoint protocol ${discovery.protocolVersion} does not match this CLI's ${LEGACY_PROTOCOL_VERSION}; rebuild the selected debug app and use its current discovery file`);
  }
  const [host, port] = discovery.address.split(":");
  if (!host || !port) throw new Error("invalid legacy endpoint address");
  return await new Promise<Reply>((resolveReply, rejectReply) => {
    let buffer = "";
    const decoder = new TextDecoder();
    Bun.connect({
      hostname: host, port: Number(port),
      socket: {
        open(socket) {
          socket.write(JSON.stringify({ ...request, id: 1, version: LEGACY_PROTOCOL_VERSION, token: discovery.token }) + "\n");
        },
        data(socket, data) {
          buffer += decoder.decode(data, { stream: true });
          const newline = buffer.indexOf("\n");
          if (newline >= 0) {
            try { resolveReply(JSON.parse(buffer.slice(0, newline)) as Reply); }
            catch (error) { rejectReply(error); }
            socket.end();
          }
        },
        error(_socket, error) { rejectReply(error); },
        close() { rejectReply(new Error("the endpoint closed before answering")); },
      },
    }).catch(rejectReply);
  });
}

class CliFailure extends Error {
  constructor(readonly error: Failure) { super(error.message); }
}

/** Never print the transport secret, even if a malformed peer echoes it. */
export function redact(value: unknown, secrets: string[]): string {
  let text = typeof value === "string" ? value : JSON.stringify(value, (key, item) =>
    /^(token|authorization|password)$/i.test(key) ? "[redacted]" : item, 2) ?? "null";
  for (const secret of secrets) if (secret) {
    text = text.split(secret).join("[redacted]");
    text = text.split(JSON.stringify(secret).slice(1, -1)).join("[redacted]");
  }
  return text;
}

function failureOf(error: unknown): Failure {
  if (error instanceof CliFailure) return error.error;
  if (error && typeof error === "object" && "error" in error) {
    const detail = error.error;
    if (detail && typeof detail === "object" && "message" in detail) return detail as Failure;
  }
  return { code: "error", message: error instanceof Error ? error.message : String(error) };
}

function numberOption(value: string | undefined, fallback: number, name = "--timeout"): number {
  const number = value === undefined ? fallback : Number(value);
  if (!Number.isSafeInteger(number) || number < 0) throw new Error(`${name} needs a non-negative integer`);
  return number;
}

export function conditionFromArguments(options: Arguments["options"]): AutomationCondition {
  const choices = [options.id, options.text, options.state, options["state-contains"], options.frame, options.diagnostic];
  if (choices.filter(value => value !== undefined).length !== 1) throw new Error("wait needs exactly one condition");
  if (options.gone && !options.id) throw new Error("--gone needs --id");
  if (options.id !== undefined) return { on: "node", id: options.id, present: options.gone !== true };
  if (options.frame !== undefined) return { on: "frame", after: numberOption(options.frame, 0, "--frame") };
  if (options.diagnostic !== undefined) return { on: "diagnostic", query: JSON.parse(options.diagnostic) };
  const split = (value: string, name: string) => {
    const separator = value.indexOf("=");
    if (separator < 0) throw new Error(`${name} wants a target followed by = and a value`);
    return [value.slice(0, separator), value.slice(separator + 1)] as const;
  };
  if (options.text !== undefined) {
    const [id, contains] = split(options.text, "--text");
    return { on: "text", id, contains };
  }
  if (options.state !== undefined) {
    const [pointer, value] = split(options.state, "--state");
    return { on: "state", pointer, equals: JSON.parse(value) };
  }
  const [pointer, contains] = split(options["state-contains"]!, "--state-contains");
  return { on: "stateContains", pointer, contains };
}

export function selectInstance(instances: AutomationInstance[], options: Arguments["options"], cwd = process.cwd()): AutomationInstance {
  const matches = options.instance
    ? instances.filter(instance => instance.instanceId === options.instance)
    : options.checkout
      ? instances.filter(instance => instance.checkout === options.checkout || instance.checkout === resolve(cwd, options.checkout!))
      : instances.filter(instance => instance.checkout === cwd);
  const matching = matches.length || options.instance || options.checkout ? matches : instances;
  const connected = matching.filter(instance => instance.executorConnected);
  // UUIDs identify one process, including a known offline process. A checkout
  // follows its current app; retained history must not hide a live replacement.
  // Multiple live apps (including scratch instances) still require an exact ID.
  const candidates = options.instance ? matching
    : connected.length ? connected
      : matching.length === 1 ? matching : [];
  if (candidates.length !== 1) {
    throw new CliFailure({
      code: candidates.length ? "ambiguous_instance" : "instance_not_found",
      message: candidates.length ? "several instances match; use --instance ID or --checkout PATH" : "no matching instance is registered on this daemon",
      instances,
    });
  }
  return candidates[0]!;
}

class Daemon {
  readonly client: WakuClient;
  session?: AutomationSession;
  private ephemeral = false;

  constructor(private options: Arguments["options"]) {
    const address = process.env.WAKU_DAEMON_ADDRESS;
    const token = process.env.WAKU_DAEMON_TOKEN;
    if (!address || !token) throw new Error("set WAKU_DAEMON_ADDRESS and WAKU_DAEMON_TOKEN to the existing daemon connection (or explicitly select the P0 fallback with --config)");
    this.client = new WakuClient({ address, token, connectTimeoutMs: 2_000 });
  }
  async execute(command: AutomationCommand, timeoutMs?: number): Promise<AutomationResult> {
    const deadline = timeoutMs === undefined ? undefined : Date.now() + timeoutMs;
    const remaining = () => deadline === undefined ? undefined : Math.max(0, deadline - Date.now());
    if (remaining() === 0) throw new CliFailure({ code: "timeout", message: "automation request deadline expired" });
    if (!this.client.connected) await this.client.connect({ timeoutMs: timeoutMs === undefined ? undefined : Math.min(2_000, remaining()!) });
    if (remaining() === 0) throw new CliFailure({ code: "timeout", message: "automation request deadline expired" });
    return this.client.automation(command, { timeoutMs: remaining() });
  }
  async instances(timeoutMs?: number): Promise<AutomationInstance[]> {
    const result = await this.execute({ type: "instances" }, timeoutMs);
    if (result.type !== "instances") throw new Error("unexpected instance-list response");
    return result.instances;
  }
  async attach(persistent = false): Promise<AutomationSession> {
    if (this.session) return this.session;
    let command: AutomationCommand;
    if (this.options.session) command = {
      type: "resume", sessionId: this.options.session,
      after: this.options.after ? JSON.parse(this.options.after) : null,
    };
    else {
      const instance = selectInstance(await this.instances(), this.options);
      command = { type: "attach", instanceId: instance.instanceId };
    }
    const result = await this.execute(command);
    if (result.type !== "attached") throw new Error("unexpected attach response");
    this.session = result.session;
    this.ephemeral = !persistent && !this.options.session;
    return result.session;
  }
  async reconnect(): Promise<void> {
    await this.client.connect();
    if (!this.session) return;
    const result = await this.execute({ type: "resume", sessionId: this.session.sessionId, after: this.session.cursor });
    if (result.type !== "attached") throw new Error("unexpected resume response");
    this.session = result.session;
  }
  async perform(call: AutomationCall): Promise<{ value: any; result: unknown }> {
    const session = await this.attach();
    const result = await this.execute({ type: "call", sessionId: session.sessionId, call });
    if (result.type !== "called") throw new Error("unexpected automation-call response");
    session.cursor = result.result.cursor;
    return { value: result.result.value, result: result.result };
  }
  async close(): Promise<void> {
    // Do not recreate a lost connection just to clean up. Persistent sessions
    // and daemon-owned runs intentionally outlive this short CLI process.
    try {
      if (this.ephemeral && this.session && this.client.connected) {
        await this.client.automation({ type: "detach", sessionId: this.session.sessionId }).catch(() => undefined);
      }
    } finally { this.client.disconnect(); }
  }
}

const pause = (ms: number) => new Promise<void>(resolve => setTimeout(resolve, ms));
const retryable = (error: unknown) => error instanceof WakuConnectionError && error.retryable;

async function waitForBuild(daemon: Daemon, options: Arguments["options"]): Promise<AutomationInstance> {
  const deadline = Date.now() + numberOption(options.timeout, 120_000);
  let baseline = options["after-build"];
  const selectedId = options.instance;
  let selectedCheckout = options.checkout;
  const remaining = () => Math.max(0, deadline - Date.now());
  let lastInstances: AutomationInstance[] = [];
  let lastError: Failure | undefined;
  do {
    try {
      lastInstances = await daemon.instances(remaining());
      // Ordinary app UUIDs change on relaunch. Only an explicitly requested
      // UUID stays pinned; automatic selection follows the chosen checkout.
      const instance = selectInstance(lastInstances, { ...options, instance: selectedId, checkout: selectedId ? undefined : selectedCheckout });
      if (!selectedId) selectedCheckout = instance.checkout;
      baseline ??= instance.build;
      if (instance.executorConnected && instance.build !== baseline) {
        const attached = await daemon.execute({ type: "attach", instanceId: instance.instanceId }, remaining());
        if (attached.type !== "attached") throw new Error("unexpected fresh-build attach response");
        try {
          const verified = await daemon.execute({ type: "call", sessionId: attached.session.sessionId, call: { type: "describe" } }, remaining());
          if (verified.type !== "called") throw new Error("unexpected fresh-build description response");
          const value = verified.result.value;
          const observed = value && typeof value === "object" && !Array.isArray(value) ? value.instance : undefined;
          if (!observed || typeof observed !== "object" || Array.isArray(observed)
            || observed.instanceId !== instance.instanceId || observed.build !== attached.session.instance.build
            || observed.build === baseline) {
            throw new CliFailure({ code: "executor_changed", message: "the executor identity changed while verifying the fresh build" });
          }
          return attached.session.instance;
        } finally {
          // Cleanup cannot replace a verified success (or the original error)
          // and must not acquire a fresh request budget after the deadline.
          if (daemon.client.connected && remaining() > 0) {
            await daemon.execute({ type: "detach", sessionId: attached.session.sessionId }, remaining()).catch(() => undefined);
          }
        }
      }
    } catch (error) {
      const failure = failureOf(error);
      if (remaining() === 0) { lastError = failure; break; }
      if (!retryable(error) && !["instance_not_found", "executor_unavailable", "executor_disconnected", "executor_gone", "no_executor", "executor_changed"].includes(failure.code ?? "")) throw error;
      lastError = failure;
    }
    if (Date.now() >= deadline) break;
    await pause(Math.min(100, deadline - Date.now()));
  } while (Date.now() <= deadline);
  throw new CliFailure({ code: "timeout", message: "no fresh connected build arrived before the deadline", afterBuild: baseline ?? null, instances: lastInstances, lastError });
}

async function waitForCondition(daemon: Daemon, options: Arguments["options"]) {
  const condition = conditionFromArguments(options);
  const timeoutMs = numberOption(options.timeout, 10_000);
  const session = await daemon.attach();
  let response = await daemon.execute({ type: "wait", sessionId: session.sessionId, condition, timeoutMs });
  if (response.type !== "wait") throw new Error("unexpected wait response");
  let wait = response.wait;
  // Only inspect the runtime's registered wait. The daemon observes UI/model
  // events; the CLI never polls snapshots to evaluate a condition.
  const deadline = Date.now() + timeoutMs + 5_000;
  while (wait.status === "pending") {
    if (Date.now() >= deadline) throw new CliFailure({ code: "timeout", message: "daemon did not return the registered wait's result", wait });
    await pause(100);
    try {
      if (!daemon.client.connected) await daemon.reconnect();
      response = await daemon.execute({ type: "waitResult", sessionId: session.sessionId, waitId: wait.waitId });
      if (response.type !== "wait") throw new Error("unexpected wait-result response");
      wait = response.wait;
    } catch (error) { if (!retryable(error)) throw error; }
  }
  if (wait.status !== "succeeded") throw new CliFailure({ ...(wait.error ?? { code: wait.status, message: `wait ${wait.status}` }), wait });
  return wait;
}

export const CONTEXT_SECTIONS = ["route", "selection", "task", "composer", "providers", "runtime", "workspace", "board", "panels", "ui"];

export function snapshotSections(command: string, pointer = "", explicit?: string): string[] {
  if (explicit !== undefined) return explicit.split(",");
  if (command === "tree") return ["tree"];
  if (command === "inspect" || (command === "state" && !pointer)) return [...CONTEXT_SECTIONS];
  if (command !== "state" || !pointer) return [];
  if (!pointer.startsWith("/") || /~(?![01])/.test(pointer)) throw new Error("state pointer must be a valid JSON pointer starting with /");
  return [pointer.slice(1).split("/")[0]!.replace(/~1/g, "/").replace(/~0/g, "~")];
}

function pointerValue(value: any, pointer: string): any {
  if (!pointer) return value;
  if (!pointer.startsWith("/")) throw new Error("state pointer must start with /");
  return pointer.slice(1).split("/").reduce((current: any, part: string) => current?.[part.replace(/~1/g, "/").replace(/~0/g, "~")], value);
}

async function main() {
  const parsed = parseArguments(process.argv.slice(2));
  const { command, args, options } = parsed;
  if (options.help) { console.log(HELP); return; }
  if (!command) throw new Error(HELP);
  const secrets = [process.env.WAKU_DAEMON_TOKEN ?? ""];
  let daemon: Daemon | undefined;
  let discovery: Discovery | undefined;
  const output = (value: unknown) => console.log(redact(value, secrets));
  try {
    const config = options.config ?? process.env.WAKU_RPA_CONFIG;
    if (config) {
      if (!existsSync(config)) throw new Error(`no discovery file at ${config}`);
      discovery = JSON.parse(readFileSync(config, "utf8")) as Discovery;
      secrets.push(discovery.token);
    } else daemon = new Daemon(options);
    const legacy = async (request: Record<string, unknown>) => {
      const reply = await call(discovery!, request);
      if (!reply.ok) throw new CliFailure(reply.error ?? { code: "error", message: "unknown failure" });
      return reply.result;
    };
    const perform = async (request: AutomationCall) => {
      if (daemon) return daemon.perform(request);
      const result = await legacy(request);
      return { value: result, result };
    };
    const required = (value: string | undefined, description: string): string => {
      if (value === undefined || value === "") throw new Error(`${command} needs ${description}`);
      return value;
    };
    const native = (): Daemon => {
      if (!daemon) throw new Error(`${command} requires the daemon transport; remove --config/WAKU_RPA_CONFIG`);
      return daemon;
    };
    switch (command) {
      case "scratch": {
        const active = native();
        const operation = required(args[0], "create or remove");
        if (options.session || options.instance || options.checkout) throw new Error("scratch commands take their own instance and do not use --session, --instance, or --checkout");
        if (operation === "create") {
          if (args[1]) throw new Error("scratch create takes its seed file with --seed FILE");
          const seed = options.seed ? JSON.parse(readFileSync(options.seed, "utf8")) : { projects: [], sessions: [] };
          output(await active.execute({ type: "scratchCreate", name: options.name ?? null, seed, headless: options.headless ?? false }));
        } else if (operation === "remove") {
          output(await active.execute({ type: "scratchRemove", instanceId: required(args[1], "a scratch instanceId") }));
        } else throw new Error("scratch needs create or remove");
        break;
      }
      case "instances": {
        const instances = await native().instances();
        if (options.json) output(instances);
        else for (const instance of instances) output(`${instance.instanceId}\t${instance.executorConnected ? "connected" : "disconnected"}\t${instance.build}\tpid ${instance.pid}\t${instance.checkout}\t${instance.database}`);
        break;
      }
      case "wait-build": output(await waitForBuild(native(), options)); break;
      case "attach": case "resume": {
        if (command === "resume") required(options.session, "--session ID");
        output(await native().attach(true));
        break;
      }
      case "detach": {
        output(await native().execute({ type: "detach", sessionId: required(options.session, "--session ID") }));
        break;
      }
      case "diff": {
        const active = native();
        required(options.session, "--session ID");
        const session = await active.attach(true);
        const after: AutomationCursor = JSON.parse(required(options.after, "--after JSON"));
        const streams = (options.streams ?? "ui,model,diagnostic").split(",");
        if (!streams.every(stream => ["ui", "model", "diagnostic"].includes(stream))) throw new Error("--streams accepts ui,model,diagnostic");
        output(await active.execute({ type: "diff", sessionId: session.sessionId, after, streams: streams as ("ui" | "model" | "diagnostic")[] }));
        break;
      }
      case "job": {
        const active = native();
        if (options.session || options.instance || options.checkout) throw new Error("job commands own fresh scratch targets; omit instance/session selectors");
        const operation = required(args[0], "start, run, status, wait, or abort");
        if (!["start", "run", "status", "wait", "abort"].includes(operation)) throw new Error("job needs start, run, status, wait, or abort");
        // Validate local options before a start can create background work.
        const waitTimeout = operation === "run" || operation === "wait"
          ? numberOption(options.timeout, 1_850_000) : undefined;
        let result: AutomationResult;
        if (operation === "start" || operation === "run") {
          const job: AutomationJob = JSON.parse(readFileSync(required(args[1], "a job JSON file"), "utf8"));
          result = await active.execute({ type: "jobStart", job });
        } else result = await active.execute({ type: operation === "abort" ? "jobAbort" : "jobResult", runId: required(args[1], "a job runId") });
        if (result.type !== "job") throw new Error("unexpected job response");
        let run: AutomationJobRun = result.run;
        if (operation === "run" || operation === "wait") {
          const deadline = Date.now() + waitTimeout!;
          while (run.status === "pending") {
            if (Date.now() >= deadline) throw new CliFailure({ code: "client_wait_timeout", message: "CLI wait ended; the daemon-owned job is still running", run });
            await Bun.sleep(Math.min(100, Math.max(0, deadline - Date.now())));
            if (Date.now() >= deadline) continue;
            const next = await active.execute({ type: "jobResult", runId: run.runId }, deadline - Date.now());
            if (next.type !== "job") throw new Error("unexpected job response");
            run = next.run;
          }
        }
        if (run.status === "failed" || run.status === "aborted") throw new CliFailure({ ...(run.error ?? { code: "job_failed", message: `job ${run.status}` }), run });
        output(run);
        break;
      }
      case "schedule": {
        const active = native();
        if (options.session || options.instance || options.checkout) throw new Error("schedule commands do not use instance/session selectors");
        const operation = required(args[0], "create, list, status, enable, disable, cancel, or remove");
        let request: AutomationCommand;
        if (operation === "create") {
          const definition: AutomationScheduleDefinition = JSON.parse(readFileSync(required(args[1], "a schedule JSON file"), "utf8"));
          request = { type: "scheduleCreate", definition };
        } else if (operation === "list") request = { type: "scheduleList" };
        else {
          const scheduleId = required(args[1], "a schedule id");
          if (operation === "status") request = { type: "scheduleGet", scheduleId };
          else if (operation === "enable" || operation === "disable") request = { type: "scheduleSetEnabled", scheduleId, enabled: operation === "enable" };
          else if (operation === "cancel") request = { type: "scheduleCancel", scheduleId };
          else if (operation === "remove") request = { type: "scheduleRemove", scheduleId };
          else throw new Error("schedule needs create, list, status, enable, disable, cancel, or remove");
        }
        const result = await active.execute(request);
        output(result.type === "schedule" ? result.schedule : result.type === "schedules" ? result.schedules : result);
        break;
      }
      case "record": {
        const active = native();
        const operation = required(args[0], "start, status, stop, or cancel");
        if (!["start", "status", "stop", "cancel"].includes(operation)) throw new Error("record needs start, status, stop, or cancel");
        if (operation !== "start") required(options.session, "--session ID");
        const session = await active.attach(true);
        const result = await active.execute(operation === "start"
          ? { type: "recordingStart", sessionId: session.sessionId, name: required(args[1], "a recording name") }
          : { type: operation === "status" ? "recordingStatus" : operation === "stop" ? "recordingStop" : "recordingCancel", sessionId: session.sessionId });
        if (result.type === "replay") output(result.replay);
        else if (result.type === "recording") output({ sessionId: session.sessionId, ...result.recording });
        else throw new Error("unexpected recording response");
        break;
      }
      case "replay": {
        const active = native();
        const replay: AutomationReplay = JSON.parse(readFileSync(required(args[0], "a replay JSON file"), "utf8"));
        const parameters = options.parameters ? JSON.parse(readFileSync(options.parameters, "utf8")) : {};
        const session = await active.attach(true);
        const result = await active.execute({ type: "replayStart", sessionId: session.sessionId, replay, parameters });
        if (result.type !== "scenario") throw new Error("unexpected replay response");
        output({ sessionId: session.sessionId, ...result.run });
        break;
      }
      case "scenario": {
        const active = native();
        const operation = required(args[0], "start, status, or abort");
        if (!["start", "status", "abort"].includes(operation)) throw new Error("scenario needs start, status, or abort");
        let scenario: AutomationScenario | undefined;
        if (operation === "start") scenario = JSON.parse(readFileSync(required(args[1], "a scenario JSON file"), "utf8"));
        else { required(options.session, "--session ID"); required(args[1], "a runId"); }
        const session = await active.attach(true);
        const result = await active.execute(operation === "start"
          ? { type: "scenarioStart", sessionId: session.sessionId, scenario: scenario! }
          : { type: operation === "status" ? "scenarioResult" : "scenarioAbort", sessionId: session.sessionId, runId: args[1]! });
        if (result.type !== "scenario") throw new Error("unexpected scenario response");
        const run = { sessionId: session.sessionId, ...result.run };
        if (result.run.status === "failed") throw new CliFailure({ ...(result.run.error ?? { code: "scenario_failed", message: "scenario failed" }), run });
        output(run);
        break;
      }
      case "transcript": {
        native();
        const conversationId = required(options.conversation, "--conversation ID");
        const query: TranscriptQuery = JSON.parse(required(args[0], "a transcript query JSON object"));
        const { value } = await perform({ type: "transcript", conversationId, query });
        output(value);
        break;
      }
      case "describe": {
        const { value } = await perform({ type: "describe" });
        output(value);
        break;
      }
      case "tree": case "state": case "snap": case "inspect": {
        let sections = snapshotSections(command, args[0], options.sections);
        if (daemon && !options.sections && (command === "inspect" || (command === "state" && !args[0])) && (await daemon.attach()).instance.headless) {
          sections = ["session", "sessions", "projects", "selection", "draft"];
        }
        const { value } = await perform({ type: "snapshot", sections });
        if (command === "snap" || command === "inspect") { output(value); break; }
        if (command === "state") { output(pointerValue(value.state, args[0] ?? "")); break; }
        const nodes: any[] = (value?.tree?.nodes ?? []).filter((node: any) => node.element_id);
        const filtered = args[0] ? nodes.filter(node => [node.element_id, node.role, node.label, node.value].some(field => String(field ?? "").toLowerCase().includes(args[0]!.toLowerCase()))) : nodes;
        if (options.json) output(filtered);
        else {
          for (const node of filtered) {
            const flags = [node.disabled ? "disabled" : "", (node.actions ?? []).includes("Click") ? "clickable" : ""].filter(Boolean).join(",");
            output(`${node.element_id}\t${node.role}\t${node.label ?? node.value ?? ""}${flags ? `\t${flags}` : ""}`);
          }
          output(`(${filtered.length} of ${nodes.length} controls)`);
        }
        break;
      }
      case "logs": case "timeline": case "counters": case "verbosity": {
        if (options.instance || options.checkout || options.session) {
          throw new Error(`${command} queries the connected daemon directly and does not accept --instance, --checkout, or --session; targeted action/wait failures retain instance-specific evidence`);
        }
        if (daemon && !daemon.client.connected) await daemon.client.connect();
        if (command === "logs") {
          const query = JSON.parse(options.filter ?? "{}");
          output(daemon ? await daemon.client.diagnosticLogs(query) : await legacy({ type: "logs", query }));
        } else if (command === "timeline") {
          const query = { sessionId: options.conversation, turnId: options.turn };
          if (!query.sessionId && !query.turnId) throw new Error("timeline needs --conversation ID or --turn ID");
          output(daemon ? await daemon.client.diagnosticTimeline(query) : await legacy({ type: "timeline", query }));
        } else if (command === "counters") output(daemon ? await daemon.client.diagnosticCounters() : await legacy({ type: "counters" }));
        else {
          const update = args[0] === undefined ? undefined : JSON.parse(args[0]);
          output(daemon ? await daemon.client.diagnosticVerbosity(update) : await legacy({ type: "verbosity", update }));
        }
        break;
      }
      case "click": case "focus": case "scroll": {
        const id = required(args[0], "a control id");
        const directions = { up: "scrollUp", down: "scrollDown", left: "scrollLeft", right: "scrollRight" } as const;
        const direction = args[1] as keyof typeof directions;
        if (command === "scroll" && !Object.hasOwn(directions, direction)) throw new Error("scroll needs up, down, left, or right");
        const action = command === "scroll" ? directions[direction] : command;
        const { result } = await perform({ type: "act", action, target: { id } });
        output(options.json ? result : `ok ${command} ${id}`);
        break;
      }
      case "type": {
        if (args[0] === undefined) throw new Error("type needs text");
        const { value, result } = await perform({ type: "type", text: args[0], target: options.into ? { id: options.into } : null });
        output(options.json ? result : `ok typed ${value.typed} chars`);
        break;
      }
      case "key": {
        const { value, result } = await perform({ type: "key", chord: required(args[0], "a chord") });
        output(options.json ? result : `ok keys ${value.keys}`);
        break;
      }
      case "invoke": {
        const verb = required(args[0], "a verb");
        const { result } = await perform({ type: "invoke", verb, args: JSON.parse(args[1] ?? "{}") });
        output(options.json ? result : `ok ${verb} ${JSON.stringify(result)}`);
        break;
      }
      case "wait": {
        if (daemon) output(await waitForCondition(daemon, options));
        else {
          const condition = conditionFromArguments(options);
          const legacyCondition = condition.on === "stateContains" ? { ...condition, on: "state_contains" } : condition;
          output(await legacy({ type: "wait", condition: legacyCondition, timeout_ms: numberOption(options.timeout, 10_000) }));
        }
        break;
      }
      case "raw": {
        const request = JSON.parse(required(args[0], "a JSON command"));
        output(daemon ? await daemon.execute(request) : await legacy(request));
        break;
      }
      default: throw new Error(`unknown command ${command}\n${HELP}`);
    }
  } catch (error) {
    const failure = failureOf(error);
    if (options.json) console.error(redact({ ok: false, error: failure }, secrets));
    else {
      console.error(redact(`rpa: ${failure.code ?? "error"}: ${failure.message}`, secrets));
      // Keep every evidence field, including run/step outcomes and unknown
      // future bundle fields, rather than narrowing errors to a message.
      const { message: _message, code: _code, ...detail } = failure;
      if (Object.keys(detail).length) console.error(redact(detail, secrets));
    }
    process.exitCode = 1;
  } finally { await daemon?.close(); }
}

if (import.meta.main) {
  try { await main(); }
  catch (error) {
    const failure = { ok: false, error: failureOf(error) };
    console.error(redact(process.argv.includes("--json") ? failure : `rpa: ${failure.error.message}`, [process.env.WAKU_DAEMON_TOKEN ?? ""]));
    process.exitCode = 1;
  }
}
