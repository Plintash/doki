// Real native-app regression coverage. Never starts an app or alters a task.
// Opt in after the selected checkout's debug app is ready:
// WAKU_RPA_TEST_CONFIG="$PWD/temp/rpa.json" bun test scripts/rpa-live.test.ts
import { beforeAll, test } from "bun:test";
import { readFileSync } from "node:fs";
import { call, LEGACY_PROTOCOL_VERSION } from "./rpa";

type Discovery = Parameters<typeof call>[0];
type Reply = Awaited<ReturnType<typeof call>>;
type Event = {
  epoch: string;
  sequence: number;
  source: string;
  message: string;
  origin: { process: string };
  fields?: Record<string, unknown>;
  sessionId?: string;
  turnId?: string;
};
type Evidence = {
  observed: {
    state: unknown;
    tree: { nodes: unknown[] };
    health: {
      workspace: string;
      daemon: { state: string; connected: boolean };
      lastError: unknown;
      sessions: unknown;
      providers: unknown[];
      pendingPermissions: unknown[];
      counters: { values: Record<string, number> };
    };
  };
  treeDiff: {
    baselineAvailable: boolean;
    observedAvailable: boolean;
    added: unknown[];
    removed: unknown[];
    changed: unknown[];
  };
  recentEvents: { events: Event[]; cursor: { epoch: string; sequence: number } };
  counters: {
    processes: { origin: { process: string; pid?: number }; values: Record<string, number> }[];
  };
  diagnosticErrors: string[];
};

const config = process.env.WAKU_RPA_TEST_CONFIG;
const liveTest = config ? test : test.skip;
const marker = `rpa-live-${crypto.randomUUID()}`;
let discovery: Discovery;

// Assertion failures print the actual snapshot and diagnostic events, with
// the connection token removed, rather than a bare boolean comparison.
function check(condition: unknown, message: string, evidence: unknown): asserts condition {
  if (!condition) {
    const dump = JSON.stringify(evidence, (key, value) => key === "token" ? "[redacted]" : value, 2);
    throw new Error(`${message}\n${dump}`);
  }
}

function assertNoAuthToken(reply: Reply) {
  const encoded = JSON.stringify(reply);
  check(!encoded.includes(discovery.token) && !encoded.includes('"token":'),
    "snapshot and failure evidence must not expose the instance authentication token", reply);
}

beforeAll(async () => {
  if (!config) return;
  discovery = JSON.parse(readFileSync(config, "utf8"));
  check(discovery.protocolVersion === LEGACY_PROTOCOL_VERSION, "live tests require the debug bridge protocol v3", {
    config, protocolVersion: discovery.protocolVersion,
  });
  await readySnapshot();
}, 20_000);

async function readySnapshot() {
  const reply = await call(discovery, { type: "snapshot" });
  assertNoAuthToken(reply);
  check(reply.ok, "the selected debug app must have a ready workspace", reply);
  const snapshot = reply.result;
  check(snapshot.health?.workspace === "ready", "snapshot must include ready workspace health", reply);
  check(snapshot.health?.daemon?.connected === true, "the selected app must be connected to its daemon", reply);
  check(snapshot.tree?.nodes?.length > 0, "a fresh native frame must have a semantic tree", reply);
  return snapshot;
}

liveTest("on-demand native snapshots advance frame counters even when the app is covered", async () => {
  const before = await readySnapshot();
  const after = await readySnapshot();
  for (const name of ["framesDrawn", "renderCommits"]) {
    check(after.health.counters.values[name] > before.health.counters.values[name],
      `fresh snapshots must advance ${name} without relying on a display-link tick`, { before, after });
  }
}, 20_000);

function assertEvidence(reply: Reply, code: string, snapshot: any, matchesAction: (event: Event) => boolean) {
  assertNoAuthToken(reply);
  check(reply.ok === false && reply.error?.code === code, `expected a real ${code} failure`, reply);
  const bundle = reply.error.bundle as Evidence | undefined;
  check(bundle, "failed call must carry its evidence bundle", reply);
  check(bundle.observed?.state != null, "bundle must carry the observed live state", reply);
  const health = bundle.observed.health;
  check(health?.workspace === "ready", "failure capture must observe the ready workspace", reply);
  check(health.daemon?.state === "connected" && health.daemon.connected, "health must report the live link", reply);
  check("lastError" in health && "sessions" in health, "health must name last error and session state", reply);
  check(Array.isArray(health.providers) && Array.isArray(health.pendingPermissions), "health must name provider and permission state", reply);
  check(Number.isFinite(health.counters?.values?.framesDrawn), "health must include current frame counters", reply);
  check(bundle.observed.tree?.nodes?.length > 0, "failure capture must include a fresh native semantic tree", reply);
  check(bundle.treeDiff?.baselineAvailable && bundle.treeDiff.observedAvailable, "failure diff needs two real frames", reply);
  check([bundle.treeDiff.added, bundle.treeDiff.removed, bundle.treeDiff.changed].every(Array.isArray), "tree diff must describe additions, removals and changes", reply);
  check(bundle.diagnosticErrors?.length === 0, "a healthy daemon must supply complete failure evidence", reply);

  const reports = bundle.counters?.processes ?? [];
  const app = reports.find(report => report.origin.process === "app" && report.origin.pid === discovery.pid);
  check(app && ["framesDrawn", "renderCommits", "pumpCommits", "eventBacklog", "requestRoundTripMs"]
    .every(name => Number.isFinite(app.values[name])), "bundle must include this app's frame, commit, backlog and latency counters", reply);
  check(reports.some(report => report.origin.process === "daemon"), "bundle must include the daemon's counters", reply);

  const events = bundle.recentEvents?.events;
  check(Array.isArray(events) && events.length > 0, "bundle must include recent events from the real daemon", reply);
  check(events.every((event, index) => index === 0 || event.sequence > events[index - 1].sequence), "events must retain stream ordering", reply);
  const action = events.find(event => event.origin.process === "app" && event.source === "action" && matchesAction(event));
  check(action, "bundle must include the matching action; enable info recording for app/action before this test", reply);
  check((action.sessionId ?? null) === (snapshot.state.session?.id ?? null), "action must retain selected-session correlation", reply);
  const turnId = snapshot.health.sessions?.selected?.turnId ?? null;
  check((action.turnId ?? null) === turnId, "action must retain selected-turn correlation", reply);
  check(events.every(event => event.epoch === bundle.recentEvents.cursor.epoch), "bundle cursor must identify its retained events", reply);
}

const failures: {
  name: string;
  code: string;
  request: Record<string, unknown>;
  matches: (event: Event) => boolean;
}[] = [
  {
    name: "timed-out wait",
    code: "timeout",
    request: { type: "wait", condition: { on: "node", id: `${marker}.wait`, present: true }, timeout_ms: 30 },
    matches: event => event.message === "wait" && String(event.fields?.condition).includes(`${marker}.wait`),
  },
  {
    name: "missing action target",
    code: "not_found",
    request: { type: "act", action: "click", target: { id: `${marker}.click` } },
    matches: event => event.message === "act" && event.fields?.target === `${marker}.click`,
  },
  {
    name: "missing typing target",
    code: "not_found",
    request: { type: "type", text: "never typed", target: { id: `${marker}.type` } },
    matches: event => event.message === "type" && event.fields?.target === `${marker}.type`,
  },
  {
    name: "malformed chord",
    code: "invalid_chord",
    request: { type: "key", chord: `${marker}-x` },
    matches: event => event.message === "key" && event.fields?.chord === `${marker}-x`,
  },
  {
    name: "unknown invocation",
    code: "unknown_verb",
    request: { type: "invoke", verb: `${marker}.unknown`, args: {} },
    matches: event => event.message === "invoke" && event.fields?.verb === `${marker}.unknown`,
  },
  {
    name: "invalid invocation arguments",
    code: "invalid_args",
    request: { type: "invoke", verb: "openSession", args: { id: `${marker}.not-a-uuid` } },
    matches: event => event.message === "invoke" && event.fields?.verb === "openSession"
      && (event.fields?.args as any)?.id === `${marker}.not-a-uuid`,
  },
];

for (const failure of failures) {
  liveTest(`${failure.name} carries ready-workspace state and cross-process evidence`, async () => {
    const snapshot = await readySnapshot();
    const reply = await call(discovery, failure.request);
    assertEvidence(reply, failure.code, snapshot, failure.matches);
    check(JSON.stringify(reply.error?.bundle && (reply.error.bundle as Evidence).observed.state) === JSON.stringify(snapshot.state),
      "invalid action must not mutate the observed workspace state (run against an idle checkout)", reply);
  }, 20_000);
}

liveTest("authenticated malformed calls reach the same live failure boundary", async () => {
  const snapshot = await readySnapshot();
  // An invalid wire request has no action to record. Retain the most recent
  // attempted action as context without introducing a test-only event API.
  const verb = `${marker}.before-invalid-call`;
  const attempted = await call(discovery, { type: "invoke", verb });
  assertEvidence(attempted, "unknown_verb", snapshot, event => event.fields?.verb === verb);
  const reply = await call(discovery, { type: "act", action: "not-an-action", target: { id: marker } });
  assertEvidence(reply, "invalid_call", snapshot, event => event.fields?.verb === verb);
}, 20_000);

liveTest("unauthenticated live callers receive no workspace evidence", async () => {
  const reply = await call(discovery, { type: "snapshot", token: "invalid-test-token" });
  check(reply.ok === false && reply.error?.code === "unauthorized", "wrong token must be rejected", reply);
  check(!reply.error.bundle && !(reply.error as any).state, "unauthenticated caller must not receive state or diagnostics", reply);
}, 20_000);
