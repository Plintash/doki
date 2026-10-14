import { describe, expect, test } from "bun:test";

import {
  WakuClient,
  WakuConnectionError,
  WakuRpcError,
  daemonUrl,
  type WebSocketLike,
} from "./client";
import { MAX_AUTOMATION_EVENTS, PROTOCOL_VERSION, type AgentSession, type Task } from "./generated";

class FakeSocket implements WebSocketLike {
  readyState = 0;
  sent: string[] = [];
  private listeners = new Map<string, Array<(...args: any[]) => void>>();

  addEventListener(type: "open", listener: () => void): void;
  addEventListener(type: "message", listener: (event: MessageEvent) => void): void;
  addEventListener(type: "error", listener: () => void): void;
  addEventListener(type: "close", listener: (event: CloseEvent) => void): void;
  addEventListener(type: string, listener: (...args: any[]) => void): void {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  send(data: string): void {
    this.sent.push(data);
  }

  close(): void {
    this.readyState = 3;
  }

  emitClose(): void {
    this.readyState = 3;
    this.emit("close", { reason: "" });
  }

  fail(reason: string): void {
    this.readyState = 3;
    this.emit("error", {});
    this.emit("close", { reason });
  }

  open(): void {
    this.readyState = 1;
    this.emit("open");
  }

  receive(message: unknown): void {
    this.emit("message", { data: JSON.stringify(message) });
  }

  private emit(type: string, event?: unknown): void {
    for (const listener of this.listeners.get(type) ?? []) listener(event);
  }
}

function fixture() {
  const sockets: FakeSocket[] = [];
  let nextId = 0;
  const client = new WakuClient({
    address: "127.0.0.1:4312",
    token: "secret",
    randomUUID: () => `00000000-0000-4000-8000-${String(++nextId).padStart(12, "0")}`,
    webSocketFactory: () => {
      const socket = new FakeSocket();
      sockets.push(socket);
      return socket;
    },
  });
  return { client, sockets };
}

async function connect(client: WakuClient, sockets: FakeSocket[]): Promise<FakeSocket> {
  const connected = client.connect();
  const socket = sockets.at(-1)!;
  socket.open();
  socket.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
  await connected;
  return socket;
}

describe("WakuClient", () => {
  test("reports connection state changes, including remote closure", async () => {
    const { client, sockets } = fixture();
    const states: string[] = [];
    const unsubscribe = client.subscribeConnectionState((state) => states.push(state));

    const connected = client.connect();
    const socket = sockets[0]!;
    socket.open();
    socket.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await connected;
    socket.close();
    socket.emitClose();
    unsubscribe();

    expect(states).toEqual(["disconnected", "connecting", "connected", "disconnected"]);
  });

  test("authenticates and correlates typed responses", async () => {
    const { client, sockets } = fixture();
    const connected = client.connect();
    const socket = sockets[0]!;
    socket.open();
    expect(JSON.parse(socket.sent[0]!)).toEqual({
      type: "hello",
      protocolVersion: PROTOCOL_VERSION,
      token: "secret",
      clientId: "00000000-0000-4000-8000-000000000001",
      resumeFrom: [],
    });
    socket.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await connected;

    const response = client.request({ type: "getSettings" });
    const request = JSON.parse(socket.sent[1]!);
    socket.receive({
      type: "response",
      requestId: request.requestId,
      outcome: { status: "ok", payload: { type: "ack" } },
    });
    await expect(response).resolves.toEqual({ type: "ack" });
  });

  test("surfaces daemon errors", async () => {
    const { client, sockets } = fixture();
    const socket = sockets[0] ?? new FakeSocket();
    const connected = client.connect();
    const active = sockets[0] ?? socket;
    active.open();
    active.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await connected;

    const response = client.request({ type: "getSettings" });
    const request = JSON.parse(active.sent[1]!);
    active.receive({
      type: "response",
      requestId: request.requestId,
      outcome: { status: "error", error: { message: "nope" } },
    });
    await expect(response).rejects.toBeInstanceOf(WakuRpcError);
  });

  test("round-trips issue descriptions and optional session draft metadata", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const task: Task = {
      id: "issue", title: "Support issue drafts", description: "Keep the brief\nseparate from execution.",
      state: "backlog", position: 0, created_at: 100, updated_at: 100,
    };
    // Older/non-board constructors can omit the new marker; both explicit
    // values must also survive a load/save round trip without normalization.
    const sessions: AgentSession[] = [undefined, false, true].map((isDraft, index) => ({
      id: `session-${index}`, title: "New task", project_id: "project", task_id: task.id,
      provider: "pi", runtime_mode: "fullAccess", status: "idle",
      created_at: 100, updated_at: 100, provider_cursor: null,
      messages: [], transcript_blocks: [], turns: [],
      ...(isDraft === undefined ? {} : { is_draft: isDraft }),
    }));
    const payload = {
      type: "taskState" as const, projects: [], tasks: [task], sessions,
      defaultCwd: "/workspace", projectlessRoot: null,
    };
    const loading = client.request({ type: "loadTaskState" });
    const loadRequest = JSON.parse(socket.sent.at(-1)!);
    socket.receive({
      type: "response", requestId: loadRequest.requestId,
      outcome: { status: "ok", payload },
    });
    const loaded = await loading;
    expect(loaded).toEqual(payload);
    if (loaded.type !== "taskState") throw new Error("Expected task state");
    expect(loaded.sessions[0]).not.toHaveProperty("is_draft");

    const command = {
      type: "saveTaskState" as const, projects: loaded.projects, tasks: loaded.tasks,
      sessions: loaded.sessions, liveSessionIds: loaded.sessions.map((session) => session.id),
    };
    const saving = client.request(command);
    const saveRequest = JSON.parse(socket.sent.at(-1)!);
    expect(saveRequest.command).toEqual(command);
    socket.receive({
      type: "response", requestId: saveRequest.requestId,
      outcome: { status: "ok", payload: { type: "taskStateSaved", sessions } },
    });
    await expect(saving).resolves.toEqual({ type: "taskStateSaved", sessions });
    client.disconnect();
  });

  test("deduplicates events and resumes from the last sequence", async () => {
    const { client, sockets } = fixture();
    const firstConnection = client.connect();
    const first = sockets[0]!;
    first.open();
    first.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await firstConnection;

    const received: number[] = [];
    client.subscribe("session", "runtime", (event) => received.push(event.sequence));
    const event = {
      type: "event",
      sessionId: "session",
      runtimeId: "runtime",
      epoch: "epoch-one",
      sequence: 4,
      event: { kind: "textDelta", payload: { text: "hi" } },
    };
    first.receive(event);
    first.receive(event);
    expect(received).toEqual([4]);

    client.disconnect();
    const secondConnection = client.connect();
    const second = sockets[1]!;
    second.open();
    expect(JSON.parse(second.sent[0]!).resumeFrom).toEqual([
      { sessionId: "session", runtimeId: "runtime", epoch: "epoch-one", sequence: 4 },
    ]);
    second.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await secondConnection;
  });

  test("reconnect resumes the diagnostic stream without replaying what was seen", async () => {
    const { client, sockets } = fixture();
    const received: Array<[number, string]> = [];
    const origins: string[] = [];
    let expired: { requestedSequence: number; oldestSequence: number } | undefined;
    client.subscribeDiagnostics((message) => {
      if (message.type === "diagnostic") {
        received.push([message.sequence, message.message]);
        origins.push(message.origin.process);
      } else {
        expired = {
          requestedSequence: message.requestedSequence,
          oldestSequence: message.oldestSequence,
        };
      }
    });

    const firstConnection = client.connect();
    const first = sockets[0]!;
    first.open();
    first.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await firstConnection;

    const record = (sequence: number, message: string) => ({
      type: "diagnostic",
      epoch: "epoch-one",
      sequence,
      timestampMs: sequence,
      level: "info",
      origin: { process: "app", pid: 1, version: "test" },
      source: "turn",
      message,
    });
    first.receive(record(1, "one"));
    first.receive(record(2, "two"));
    first.receive(record(2, "two"));
    expect(received).toEqual([
      [1, "one"],
      [2, "two"],
    ]);
    expect(client.lastDiagnosticCursor()).toEqual({ epoch: "epoch-one", sequence: 2 });

    client.disconnect();
    const secondConnection = client.connect();
    const second = sockets[1]!;
    second.open();
    expect(JSON.parse(second.sent[0]!).diagnosticFrom).toEqual({
      epoch: "epoch-one",
      sequence: 2,
    });
    second.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await secondConnection;

    // Only the records produced while the client was away arrive, in order.
    second.receive(record(3, "three"));
    second.receive({
      type: "diagnosticCursorExpired",
      epoch: "epoch-one",
      requestedSequence: 1,
      oldestSequence: 5,
    });
    expect(received).toEqual([
      [1, "one"],
      [2, "two"],
      [3, "three"],
    ]);
    expect(origins).toEqual(["app", "app", "app"]);
    expect(expired).toEqual({ requestedSequence: 1, oldestSequence: 5 });
  });

  test("buffers diagnostic records that arrive before the first subscriber", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    socket.receive({
      type: "diagnostic",
      epoch: "epoch-one",
      sequence: 1,
      timestampMs: 1,
      level: "info",
      origin: { process: "daemon", pid: 2 },
      source: "request",
      message: "buffered",
    });

    const messages: string[] = [];
    client.subscribeDiagnostics((message) => {
      if (message.type === "diagnostic") messages.push(message.message);
    });
    expect(messages).toEqual(["buffered"]);
  });

  test("disconnect rejects an in-flight handshake and permits reconnecting", async () => {
    const { client, sockets } = fixture();
    const firstConnection = client.connect();
    client.disconnect();
    await expect(firstConnection).rejects.toThrow("Waku client disconnected");

    const secondConnection = client.connect();
    const second = sockets[1]!;
    second.open();
    second.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await expect(secondConnection).resolves.toBeUndefined();
  });

  test("surfaces the native socket failure reason", async () => {
    const { client, sockets } = fixture();
    const connection = client.connect();

    sockets[0]!.fail("The network connection was lost");

    await expect(connection).rejects.toThrow("The network connection was lost");
  });

  test("accepts sequence one again when the daemon epoch changes", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const received: Array<[string, number]> = [];
    client.subscribe("session", "runtime", (event) => {
      received.push([event.epoch, event.sequence]);
    });

    socket.receive({
      type: "event",
      sessionId: "session",
      runtimeId: "runtime",
      epoch: "old",
      sequence: 9,
      event: { kind: "textDelta", payload: null },
    });
    socket.receive({
      type: "event",
      sessionId: "session",
      runtimeId: "runtime",
      epoch: "new",
      sequence: 1,
      event: { kind: "textDelta", payload: null },
    });

    expect(received).toEqual([
      ["old", 9],
      ["new", 1],
    ]);
  });

  test("buffers replayed events until a refreshed app attaches to the runtime", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);

    socket.receive({
      type: "event",
      sessionId: "session",
      runtimeId: "runtime",
      epoch: "epoch",
      sequence: 1,
      event: { kind: "textDelta", payload: "before attach" },
    });
    socket.receive({
      type: "event",
      sessionId: "session",
      runtimeId: "runtime",
      epoch: "epoch",
      sequence: 2,
      event: { kind: "textDelta", payload: "still before attach" },
    });

    const received: number[] = [];
    client.subscribe("session", "runtime", (event) => received.push(event.sequence));
    expect(received).toEqual([1, 2]);
  });

  test("notifies connected apps when another client changes task state", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const revisions: number[] = [];
    client.subscribeTaskState((revision) => revisions.push(revision));

    socket.receive({ type: "taskStateChanged", revision: 7 });
    expect(revisions).toEqual([7]);
  });

  test("disconnected requests reject instead of throwing synchronously", async () => {
    const { client } = fixture();
    const request = client.request({ type: "getSettings" });
    await expect(request).rejects.toThrow("Waku daemon is disconnected");
  });

  test("disconnected notifications reject instead of throwing synchronously", async () => {
    const { client } = fixture();
    const notification = client.notify({ type: "refreshBackgroundWork" });
    await expect(notification).rejects.toThrow("Waku daemon is disconnected");
  });

  test("notifications use the response-free nil request id", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);

    await client.notify(
      { type: "writeTerminal", data: "bHM=" },
      "terminal",
      "terminal",
    );

    expect(JSON.parse(socket.sent[1]!)).toEqual({
      type: "request",
      requestId: "00000000-0000-0000-0000-000000000000",
      sessionId: "terminal",
      runtimeId: "terminal",
      command: { type: "writeTerminal", data: "bHM=" },
    });
  });
});

test("daemonUrl pins the versioned endpoint", () => {
  expect(daemonUrl("localhost:3030/anything?old=1")).toBe("ws://localhost:3030/v1");
  expect(daemonUrl("wss://waku.example.test")).toBe("wss://waku.example.test/v1");
});

describe("WakuClient connection failures", () => {
  async function failure(setup: (socket: FakeSocket) => void): Promise<WakuConnectionError> {
    const { client, sockets } = fixture();
    const connected = client.connect();
    setup(sockets[0]!);
    const error = await connected.catch((cause: unknown) => cause);
    expect(error).toBeInstanceOf(WakuConnectionError);
    return error as WakuConnectionError;
  }

  test("types handshake failures so callers can tell what needs a person", async () => {
    const rejected = await failure((socket) => {
      socket.open();
      socket.receive({ type: "rejected", message: "authentication failed" });
    });
    expect(rejected.kind).toBe("rejected");
    expect(rejected.retryable).toBe(false);
    expect(rejected.message).toBe("daemon rejected connection: authentication failed");

    const unsupported = await failure((socket) => {
      socket.open();
      socket.receive({ type: "rejected", message: "protocol 3 is unsupported; expected 7" });
    });
    expect(unsupported.kind).toBe("protocol");

    const mismatch = await failure((socket) => {
      socket.open();
      socket.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION + 1, daemonVersion: "x" });
    });
    expect(mismatch.kind).toBe("protocol");
    expect(mismatch.retryable).toBe(false);

    const garbage = await failure((socket) => {
      socket.open();
      socket.receive({ type: "event", sessionId: "x" });
    });
    expect(garbage.kind).toBe("handshake");

    const refused = await failure((socket) => socket.fail("Connection refused"));
    expect(refused.kind).toBe("unreachable");
    expect(refused.retryable).toBe(true);
    expect(refused.message).toBe("Connection refused");
  });

  test("times out a handshake the daemon never answers", async () => {
    const sockets: FakeSocket[] = [];
    const client = new WakuClient({
      address: "127.0.0.1:4312",
      token: "secret",
      connectTimeoutMs: 1,
      randomUUID: () => "00000000-0000-4000-8000-000000000001",
      webSocketFactory: () => {
        const socket = new FakeSocket();
        sockets.push(socket);
        return socket;
      },
    });
    const error = (await client.connect().catch((cause: unknown) => cause)) as WakuConnectionError;
    expect(error.kind).toBe("timeout");
    expect(error.retryable).toBe(true);
    expect(client.connectionState).toBe("disconnected");
    expect(sockets[0]!.readyState).toBe(3);

    // The abandoned socket's late events are ignored and a fresh attempt works.
    sockets[0]!.open();
    const again = client.connect();
    expect(sockets).toHaveLength(2);
    sockets[1]!.open();
    sockets[1]!.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await again;
    expect(client.connected).toBe(true);
  });

  test("lets a request shorten its own timeout", async () => {
    const { client, sockets } = fixture();
    await connect(client, sockets);
    await expect(
      client.request({ type: "getSettings" }, undefined, undefined, { timeoutMs: 1 }),
    ).rejects.toThrow("timed out waiting for Waku daemon");
  });

  test("settles requests and clears the socket before listeners hear a remote close", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const request = client.request({ type: "getSettings" });
    let reconnected: Promise<void> | null = null;
    client.subscribeConnectionState((state) => {
      if (state === "disconnected" && !reconnected) reconnected = client.connect();
    });

    socket.fail("Software caused connection abort");
    const error = (await request.catch((cause: unknown) => cause)) as WakuConnectionError;
    expect(error.kind).toBe("closed");
    expect(client.lastDisconnectReason).toBe("Software caused connection abort");
    expect(sockets).toHaveLength(2);

    sockets[1]!.open();
    sockets[1]!.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await reconnected!;
    expect(client.connected).toBe(true);
    const next = client.request({ type: "getSettings" });
    expect(sockets[1]!.sent).toHaveLength(2);
    const sent = JSON.parse(sockets[1]!.sent[1]!) as { requestId: string };
    sockets[1]!.receive({
      type: "response",
      requestId: sent.requestId,
      outcome: { status: "ok", payload: { type: "ack" } },
    });
    await expect(next).resolves.toEqual({ type: "ack" });
  });

  test("records when the daemon last spoke", async () => {
    const sockets: FakeSocket[] = [];
    let now = 41;
    const client = new WakuClient({
      address: "127.0.0.1:4312",
      token: "secret",
      now: () => now,
      randomUUID: () => "00000000-0000-4000-8000-000000000001",
      webSocketFactory: () => {
        const socket = new FakeSocket();
        sockets.push(socket);
        return socket;
      },
    });
    expect(client.lastMessageAt).toBe(0);
    const connected = client.connect();
    now = 42;
    sockets[0]!.open();
    sockets[0]!.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await connected;
    expect(client.lastMessageAt).toBe(42);
    now = 43;
    sockets[0]!.receive({ type: "taskStateChanged", revision: 1 });
    expect(client.lastMessageAt).toBe(43);
  });
});

describe("diagnostic query helpers", () => {
  test("typed logs, timeline, counters and verbosity use the existing request transport", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const answer = (type: string, body: object) => {
      const request = JSON.parse(socket.sent.at(-1)!);
      expect(request.command.type).toBe(type);
      expect(request.sessionId).toBe("00000000-0000-0000-0000-000000000000");
      socket.receive({type:"response",requestId:request.requestId,outcome:{status:"ok",payload:{type,...body}}});
      return request.command;
    };
    const logs = {events:[],cursor:{epoch:"epoch",sequence:4},oldestSequence:1,expiry:null,truncated:false};
    const logPromise = client.diagnosticLogs({sessionId:"conversation",source:"turn",limit:20});
    expect(answer("diagnosticLogs",{result:logs}).query).toEqual({sessionId:"conversation",source:"turn",limit:20});
    expect(await logPromise).toEqual(logs);
    const timeline = {observedAtMs:100,entries:[],lastPhase:null,lastChangedAtMs:null,complete:false,truncated:false};
    const timelinePromise = client.diagnosticTimeline({sessionId:"conversation"});
    expect(answer("diagnosticTimeline",{result:timeline}).query).toEqual({sessionId:"conversation"});
    expect(await timelinePromise).toEqual(timeline);
    const counters = {observedAtMs:100,processes:[]};
    const countersPromise = client.diagnosticCounters();
    answer("diagnosticCounters",{result:counters});
    expect(await countersPromise).toEqual(counters);
    const verbosity = {defaultLevel:"info" as const,overrides:[]};
    const verbosityPromise = client.diagnosticVerbosity();
    expect(answer("diagnosticVerbosity",{verbosity}).update).toBeNull();
    expect(await verbosityPromise).toEqual(verbosity);
    client.disconnect();
  });

  test("runtime policy updates affect only the selected process and source and reset on reconnect", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const app = {process:"app" as const,pid:1,version:"test"};
    expect(client.diagnosticEnabled("debug",app,"pump")).toBe(false);
    socket.receive({type:"diagnosticVerbosity",verbosity:{defaultLevel:"info",overrides:[{process:"app",source:"pump",level:"trace"}]}});
    expect(client.diagnosticEnabled("trace",app,"pump")).toBe(true);
    expect(client.diagnosticEnabled("debug",app,"turn")).toBe(false);
    expect(client.diagnosticEnabled("debug",{process:"daemon",pid:2},"pump")).toBe(false);
    client.disconnect();
    const reconnected = await connect(client,sockets);
    reconnected.receive({type:"diagnosticVerbosity",verbosity:{defaultLevel:"info",overrides:[]}});
    expect(client.diagnosticEnabled("debug",app,"pump")).toBe(false);
    client.disconnect();
  });

  test("counter samples preserve process sample time and records retain correlation", async () => {
    const {client,sockets} = fixture();
    const socket = await connect(client,sockets);
    const origin = {process:"app" as const,pid:1,version:"test"};
    client.reportDiagnosticCounters({timestampMs:123,origin,values:{framesDrawn:42,pumpCommits:7}});
    expect(JSON.parse(socket.sent.at(-1)!)).toEqual({type:"diagnosticCounters",timestampMs:123,origin,values:{framesDrawn:42,pumpCommits:7}});
    client.recordDiagnostic({timestampMs:124,origin,source:"turn",message:"submitted",level:"info",sessionId:"conversation",turnId:"turn"});
    expect(JSON.parse(socket.sent.at(-1)!).sessionId).toBe("conversation");
    expect(JSON.parse(socket.sent.at(-1)!).taskId).toBeUndefined();
    client.disconnect();
  });

  test("request round-trip includes response delay and unexpected payloads are rejected", async () => {
    let now = 100;
    const socket = new FakeSocket();
    let id = 0;
    const client = new WakuClient({address:"localhost:1",token:"test",now:()=>now,randomUUID:()=>String(++id),webSocketFactory:()=>socket});
    const pendingConnection = client.connect();
    socket.open();
    socket.receive({type:"hello",protocolVersion:PROTOCOL_VERSION,daemonVersion:"test"});
    await pendingConnection;
    const result = client.diagnosticCounters();
    now = 145;
    socket.receive({type:"response",requestId:JSON.parse(socket.sent.at(-1)!).requestId,outcome:{status:"ok",payload:{type:"ack"}}});
    await expect(result).rejects.toThrow("unexpected diagnostic counters response");
    expect(client.lastRequestRoundTripMs()).toBe(45);
    client.disconnect();
  });
});

test("a delayed verbosity query response cannot overwrite a newer broadcast", async () => {
  const {client,sockets} = fixture();
  const socket = await connect(client,sockets);
  const response = client.diagnosticVerbosity();
  const request = JSON.parse(socket.sent.at(-1)!);
  socket.receive({type:"response",requestId:request.requestId,outcome:{status:"ok",payload:{type:"diagnosticVerbosity",verbosity:{defaultLevel:"info",overrides:[]}}}});
  socket.receive({type:"diagnosticVerbosity",verbosity:{defaultLevel:"debug",overrides:[]}});
  await response;
  expect(client.diagnosticEnabled("debug",{process:"app",pid:1,version:"test"},"pump")).toBe(true);
  client.disconnect();
});

const automationInstance = {
  instanceId: "instance", checkout: "/test/checkout", database: "/test/checkout/temp/app.db",
  pid: 42, build: "build-one", version: "test", executorConnected: true, scratch: false,
};
const automationCursor = (sequence: number) => ({ epoch: "automation-epoch", sequence });
const automationEvent = (sequence: number) => ({
  sessionId: "automation-session", cursor: automationCursor(sequence), stream: "ui", payload: { sequence },
});
function automationReply(socket: FakeSocket, result: unknown): void {
  const request = JSON.parse(socket.sent.at(-1)!);
  socket.receive({ type: "response", requestId: request.requestId,
    outcome: { status: "ok", payload: { type: "automation", result } } });
}
function automationAttached(sequence = 0, events: unknown[] = [], expiry: unknown = null) {
  return { type: "attached", session: {
    sessionId: "automation-session", instance: automationInstance, cursor: automationCursor(sequence), streams: ["ui"],
  }, replay: { events, cursor: automationCursor(sequence), expiry } };
}

test("automation calls, subscriptions and ordered resume use the authenticated client", async () => {
  const { client, sockets } = fixture();
  const first = await connect(client, sockets);
  const attached = client.attachAutomation("instance");
  expect(JSON.parse(first.sent.at(-1)!).command).toEqual({ type: "automation", command: { type: "attach", instanceId: "instance" } });
  automationReply(first, automationAttached());
  const session = await attached;
  const received: number[] = [];
  const notices: string[] = [];
  client.subscribeAutomation(session.sessionId, message => {
    notices.push(message.type);
    if (message.type === "event") received.push(message.event.cursor.sequence);
  });
  const subscribed = client.automation({ type: "subscribe", sessionId: session.sessionId, streams: ["ui"] });
  automationReply(first, { type: "subscribed", session });
  await subscribed;
  const called = client.automationCall(session.sessionId, { type: "snapshot", sections: ["state"] });
  automationReply(first, { type: "called", result: { cursor: automationCursor(1), value: { ready: true }, observed: { state: {} }, treeDiff: {}, stateDiff: [] } });
  expect((await called).value).toEqual({ ready: true });
  for (const sequence of [1, 2, 2]) first.receive({ type: "automationEvent", event: automationEvent(sequence) });
  expect(received).toEqual([1, 2]);
  client.disconnect();
  const second = await connect(client, sockets);
  const resumed = client.resumeAutomation(session.sessionId);
  expect(JSON.parse(second.sent.at(-1)!).command.command.after).toEqual(automationCursor(2));
  // This live event races the resume answer. Replay must still arrive first.
  second.receive({ type: "automationEvent", event: automationEvent(4) });
  automationReply(second, automationAttached(4, [automationEvent(3), automationEvent(4)]));
  await resumed;
  second.receive({ type: "automationEvent", event: automationEvent(5) });
  expect(received).toEqual([1, 2, 3, 4, 5]);
  expect(notices).toEqual(["event", "event", "event", "event", "event"]);
  expect(client.lastAutomationCursor(session.sessionId)).toEqual(automationCursor(5));
  client.disconnect();
});

test("automation resume surfaces cursor expiry before replay and keeps sessions independent", async () => {
  const { client, sockets } = fixture();
  const socket = await connect(client, sockets);
  const received: string[] = [];
  client.subscribeAutomation("automation-session", message => received.push(message.type));
  const other: string[] = [];
  client.subscribeAutomation("other-session", message => other.push(message.type));
  const resumed = client.resumeAutomation("automation-session", automationCursor(1));
  automationReply(socket, automationAttached(5, [automationEvent(5)], {
    requested: automationCursor(1), oldestSequence: 5, cursor: automationCursor(5),
  }));
  await resumed;
  socket.receive({ type: "automationWaitResolved", sessionId: "other-session", wait: {
    waitId: "wait", status: "succeeded", cursor: automationCursor(0), result: true, error: null,
  } });
  socket.receive({ type: "automationRunChanged", sessionId: "other-session", run: {
    runId: "run", name: "smoke", status: "succeeded", cursor: automationCursor(0), steps: [], error: null,
  } });
  expect(received).toEqual(["expired", "event"]);
  expect(other).toEqual(["waitResolved", "runChanged"]);
  client.disconnect();
});

test("automation error preserves target, nearest controls and complete failure evidence", async () => {
  const { WakuAutomationError } = await import("./client");
  const { client, sockets } = fixture();
  const socket = await connect(client, sockets);
  const error = { code: "not_found", message: "missing", target: "missing", nearest: ["composer.send"],
    bundle: { observed: { state: { ready: true } }, treeDiff: { removed: ["missing"] },
      recentEvents: null, counters: { observedAtMs: 4, processes: [] }, diagnosticErrors: ["evidence unavailable"] } };
  const request = client.automationCall("automation-session", { type: "describe" });
  automationReply(socket, { type: "error", error });
  try { await request; throw new Error("expected automation failure"); }
  catch (failure) {
    expect(failure).toBeInstanceOf(WakuAutomationError);
    expect((failure as InstanceType<typeof WakuAutomationError>).error).toEqual(error);
  }
  client.disconnect();
});

test("async executor work never holds up ordinary RPC or interest delivery", async () => {
  const { client, sockets } = fixture();
  const socket = await connect(client, sockets);
  let finish!: (value: any) => void;
  const interests: boolean[] = [];
  client.setAutomationExecutorHandler(() => new Promise(resolve => { finish = resolve; }),
    interest => interests.push(interest.attached));
  const registered = client.registerAutomationExecutor(automationInstance);
  automationReply(socket, { type: "registered", instance: automationInstance });
  await registered;
  socket.receive({ type: "automationExecutorRequest", request: { requestId: "executor-request", sessionId: "automation-session",
    instanceId: "instance", call: { type: "describe" } } });
  socket.receive({ type: "automationInterest", instanceId: "instance", attached: true, streams: ["ui"] });
  const ordinary = client.request({ type: "getSettings" });
  const request = JSON.parse(socket.sent.at(-1)!);
  socket.receive({ type: "response", requestId: request.requestId, outcome: { status: "ok", payload: { type: "ack" } } });
  await expect(ordinary).resolves.toEqual({ type: "ack" });
  expect(interests).toEqual([true]);
  expect(socket.sent.map(value => JSON.parse(value).type)).not.toContain("automationExecutorReply");
  finish({ status: "ok", execution: { value: "done", observed: {}, treeDiff: {}, stateDiff: [] } });
  await new Promise(resolve => setTimeout(resolve, 0));
  expect(JSON.parse(socket.sent.at(-1)!)).toMatchObject({ type: "automationExecutorReply", reply: { requestId: "executor-request", outcome: { status: "ok" } } });
  client.publishAutomationEvent({ instanceId: "instance", stream: "ui", observed: {}, treeDiff: {}, stateDiff: [] });
  expect(JSON.parse(socket.sent.at(-1)!).type).toBe("automationExecutorEvent");
  client.disconnect();
});

test("late executor answers cannot leak onto a reconnected socket", async () => {
  const { client, sockets } = fixture();
  const first = await connect(client, sockets);
  let finish!: (value: any) => void;
  client.setAutomationExecutorHandler(() => new Promise(resolve => { finish = resolve; }));
  first.receive({ type: "automationExecutorRequest", request: { requestId: "old-request", sessionId: "automation-session",
    instanceId: "instance", call: { type: "describe" } } });
  await Promise.resolve();
  client.disconnect();
  const second = await connect(client, sockets);
  finish({ status: "error", error: { code: "test", message: "done" } });
  await new Promise(resolve => setTimeout(resolve, 0));
  expect(second.sent.map(value => JSON.parse(value).type)).toEqual(["hello"]);
  client.disconnect();
});

test("automation buffers signal a local gap instead of silently dropping changes", async () => {
  const { client, sockets } = fixture();
  const socket = await connect(client, sockets);
  for (let sequence = 1; sequence <= MAX_AUTOMATION_EVENTS + 10; sequence++) {
    socket.receive({ type: "automationEvent", event: automationEvent(sequence) });
  }
  const received: any[] = [];
  client.subscribeAutomation("automation-session", message => received.push(message));
  expect(received.length).toBeLessThanOrEqual(MAX_AUTOMATION_EVENTS + 1);
  expect(received[0]).toMatchObject({ type: "expired", expiry: { requested: automationCursor(0), oldestSequence: MAX_AUTOMATION_EVENTS + 2 } });
  expect(received.at(-1).event.cursor.sequence).toBe(MAX_AUTOMATION_EVENTS + 10);
  client.disconnect();
});

test("same-client resume replays the retained tail after a pre-listener overflow", async () => {
  const { client, sockets } = fixture();
  const socket = await connect(client, sockets);
  const newest = MAX_AUTOMATION_EVENTS + 10;
  for (let sequence = 1; sequence <= newest; sequence++) {
    socket.receive({ type: "automationEvent", event: automationEvent(sequence) });
  }
  socket.receive({ type: "automationEvent", event: { ...automationEvent(1000), sessionId: "other-session" } });
  const received: any[] = [];
  const other: any[] = [];
  client.subscribeAutomation("automation-session", message => received.push(message));
  client.subscribeAutomation("other-session", message => other.push(message));
  expect(received[0].type).toBe("expired");
  expect(received.slice(1).map(message => message.event.cursor.sequence)).toEqual(
    Array.from({ length: 9 }, (_, index) => MAX_AUTOMATION_EVENTS + 2 + index),
  );
  expect(client.lastAutomationCursor("automation-session")).toEqual(automationCursor(newest));
  received.length = 0;

  const resumed = client.resumeAutomation("automation-session", automationCursor(0));
  expect(JSON.parse(socket.sent.at(-1)!).command.command.after).toEqual(automationCursor(0));
  // The receive cursor is already at newest, but it does not prove consumption.
  // Replay must obey the caller's cursor; overlapping live delivery stays deduped.
  for (const sequence of [newest, newest + 1, newest + 1]) {
    socket.receive({ type: "automationEvent", event: automationEvent(sequence) });
  }
  for (const sequence of [1000, 1001, 1001]) {
    socket.receive({ type: "automationEvent", event: { ...automationEvent(sequence), sessionId: "other-session" } });
  }
  const events = Array.from({ length: MAX_AUTOMATION_EVENTS }, (_, index) => automationEvent(index + 11));
  const expiry = { requested: automationCursor(0), oldestSequence: 11, cursor: automationCursor(newest) };
  automationReply(socket, automationAttached(newest, events, expiry));
  await resumed;
  expect(received).toEqual([
    { type: "expired", expiry }, ...events.map(event => ({ type: "event", event })),
    { type: "event", event: automationEvent(newest + 1) },
  ]);
  socket.receive({ type: "automationEvent", event: automationEvent(newest + 1) });
  expect(received.length).toBe(MAX_AUTOMATION_EVENTS + 2);
  expect(other.map(message => message.event.cursor.sequence)).toEqual([1000, 1001]);
  expect(client.lastAutomationCursor("automation-session")).toEqual(automationCursor(newest + 1));
  expect(client.lastAutomationCursor("other-session")).toEqual(automationCursor(1001));
  client.disconnect();
});

test("automation resume bounds racing live events and replay can repair an overflow", async () => {
  for (const repaired of [false, true]) {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const received: any[] = [];
    client.subscribeAutomation("automation-session", message => received.push(message));
    const resumed = client.resumeAutomation("automation-session", automationCursor(0));
    for (let sequence = 1; sequence <= MAX_AUTOMATION_EVENTS * 2; sequence++) {
      socket.receive({ type: "automationEvent", event: automationEvent(sequence) });
    }
    const replay = repaired ? Array.from({ length: MAX_AUTOMATION_EVENTS }, (_, index) => automationEvent(index + 1)) : [];
    automationReply(socket, automationAttached(repaired ? MAX_AUTOMATION_EVENTS : 0, replay));
    await resumed;
    expect(received.filter(message => message.type === "expired").length).toBe(repaired ? 0 : 1);
    const events = received.filter(message => message.type === "event");
    expect(events.length).toBe(MAX_AUTOMATION_EVENTS * (repaired ? 2 : 1));
    expect(events.at(-1).event.cursor.sequence).toBe(MAX_AUTOMATION_EVENTS * 2);
    client.disconnect();
  }
});

test("resume before subscribing replaces stale backlog and keeps full replay plus racing messages", async () => {
  const { client, sockets } = fixture();
  const socket = await connect(client, sockets);
  const newest = MAX_AUTOMATION_EVENTS + 10;
  for (let sequence = 1; sequence <= newest; sequence++) {
    socket.receive({ type: "automationEvent", event: automationEvent(sequence) });
  }
  socket.receive({ type: "automationEvent", event: { ...automationEvent(5), sessionId: "other-session" } });
  const resumed = client.resumeAutomation("automation-session", automationCursor(0));
  for (const sequence of [newest, newest + 1, newest + 1]) {
    socket.receive({ type: "automationEvent", event: automationEvent(sequence) });
  }
  const wait = { waitId: "wait", status: "succeeded", cursor: automationCursor(newest), result: true, error: null };
  socket.receive({ type: "automationWaitResolved", sessionId: "automation-session", wait });
  const events = Array.from({ length: MAX_AUTOMATION_EVENTS }, (_, index) => automationEvent(index + 11));
  const expiry = { requested: automationCursor(0), oldestSequence: 11, cursor: automationCursor(newest) };
  automationReply(socket, automationAttached(newest, events, expiry));
  await resumed;
  const received: any[] = [];
  client.subscribeAutomation("automation-session", message => received.push(message));
  expect(received).toEqual([
    { type: "expired", expiry }, ...events.map(event => ({ type: "event", event })),
    { type: "event", event: automationEvent(newest + 1) }, { type: "waitResolved", wait },
  ]);
  const other: any[] = [];
  client.subscribeAutomation("other-session", message => other.push(message));
  expect(other).toEqual([{ type: "event", event: { ...automationEvent(5), sessionId: "other-session" } }]);
  socket.receive({ type: "automationEvent", event: automationEvent(newest + 1) });
  expect(received.length).toBe(MAX_AUTOMATION_EVENTS + 3);
  client.disconnect();
});

test("failed resume preserves unread backlog and flushes racing live events", async () => {
  const { client, sockets } = fixture();
  const socket = await connect(client, sockets);
  for (const sequence of [1, 2]) socket.receive({ type: "automationEvent", event: automationEvent(sequence) });
  const resumed = client.resumeAutomation("automation-session", automationCursor(0));
  socket.receive({ type: "automationEvent", event: automationEvent(3) });
  automationReply(socket, { type: "error", error: { code: "unavailable", message: "try again" } });
  await expect(resumed).rejects.toThrow("try again");
  const received: any[] = [];
  client.subscribeAutomation("automation-session", message => received.push(message));
  expect(received.map(message => message.event.cursor.sequence)).toEqual([1, 2, 3]);
  client.disconnect();
});

test("replay cannot hide a notification lost from the bounded resume queue", async () => {
  const { client, sockets } = fixture();
  const socket = await connect(client, sockets);
  const received: any[] = [];
  client.subscribeAutomation("automation-session", message => received.push(message));
  const resumed = client.resumeAutomation("automation-session", automationCursor(0));
  socket.receive({ type: "automationWaitResolved", sessionId: "automation-session", wait: {
    waitId: "lost-wait", status: "succeeded", cursor: automationCursor(0), result: true, error: null,
  } });
  const events = Array.from({ length: MAX_AUTOMATION_EVENTS }, (_, index) => automationEvent(index + 1));
  for (const event of events) socket.receive({ type: "automationEvent", event });
  automationReply(socket, automationAttached(MAX_AUTOMATION_EVENTS, events));
  await resumed;
  expect(received.filter(message => message.type === "event").length).toBe(MAX_AUTOMATION_EVENTS);
  expect(received.at(-1).type).toBe("expired");
  client.disconnect();
});

test("a throwing automation listener cannot interrupt another listener or replay", async () => {
  const { client, sockets } = fixture();
  const socket = await connect(client, sockets);
  client.subscribeAutomation("automation-session", () => { throw new Error("consumer failed"); });
  const received: any[] = [];
  client.subscribeAutomation("automation-session", message => received.push(message));
  const resumed = client.resumeAutomation("automation-session", automationCursor(0));
  automationReply(socket, automationAttached(2, [automationEvent(1), automationEvent(2)]));
  await resumed;
  expect(received.map(message => message.event.cursor.sequence)).toEqual([1, 2]);
  client.disconnect();
});

test("executor overload is bounded, rejects excess work, and reconnect bypasses a hung old handler", async () => {
  const { client, sockets } = fixture();
  const first = await connect(client, sockets);
  let invocations = 0;
  client.setAutomationExecutorHandler(() => { invocations++; return new Promise(() => {}); });
  for (let index = 0; index <= MAX_AUTOMATION_EVENTS; index++) {
    first.receive({ type: "automationExecutorRequest", request: {
      requestId: `executor-${index}`, sessionId: "automation-session", instanceId: "instance", call: { type: "describe" },
    } });
  }
  await Promise.resolve();
  expect(invocations).toBe(1);
  expect(JSON.parse(first.sent.at(-1)!)).toMatchObject({ type: "automationExecutorReply", reply: {
    requestId: `executor-${MAX_AUTOMATION_EVENTS}`, outcome: { status: "error", error: { code: "executor_busy" } },
  } });
  client.disconnect();
  const second = await connect(client, sockets);
  client.setAutomationExecutorHandler(() => ({ status: "error", error: { code: "new-handler", message: "replacement answered" } }));
  second.receive({ type: "automationExecutorRequest", request: {
    requestId: "replacement", sessionId: "automation-session", instanceId: "instance", call: { type: "describe" },
  } });
  await new Promise(resolve => setTimeout(resolve, 0));
  expect(JSON.parse(second.sent.at(-1)!)).toMatchObject({ type: "automationExecutorReply", reply: {
    requestId: "replacement", outcome: { error: { code: "new-handler" } },
  } });
  client.disconnect();
});

test("a complete expired retained tail is buffered intact before the first automation listener", async () => {
  const { client, sockets } = fixture();
  const socket = await connect(client, sockets);
  const resumed = client.resumeAutomation("automation-session", automationCursor(0));
  const events = Array.from({ length: MAX_AUTOMATION_EVENTS }, (_, index) => automationEvent(index + 2));
  const expiry = { requested: automationCursor(0), oldestSequence: 2, cursor: automationCursor(MAX_AUTOMATION_EVENTS + 1) };
  automationReply(socket, automationAttached(MAX_AUTOMATION_EVENTS + 1, events, expiry));
  await resumed;
  const received: any[] = [];
  client.subscribeAutomation("automation-session", message => received.push(message));
  expect(received).toEqual([{ type: "expired", expiry }, ...events.map(event => ({ type: "event", event }))]);
  client.disconnect();
});

test("a connection attempt can use the caller's remaining deadline", async () => {
  const { client, sockets } = fixture();
  const started = Date.now();
  await expect(client.connect({ timeoutMs: 1 })).rejects.toMatchObject({ kind: "timeout" });
  expect(Date.now() - started).toBeLessThan(500);
  expect(sockets[0]!.readyState).toBe(3);
});
