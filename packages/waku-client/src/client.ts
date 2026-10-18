import {
  MAX_DIAGNOSTIC_EVENTS,
  MAX_AUTOMATION_EVENTS,
  type AutomationCall,
  type AutomationCallResult,
  type AutomationCommand,
  type AutomationCursor,
  type AutomationError,
  type AutomationEvent,
  type AutomationExecutorEvent,
  type AutomationExecutorOutcome,
  type AutomationExecutorRequest,
  type AutomationExpiry,
  type AutomationInstance,
  type AutomationResult,
  type AutomationRun,
  type AutomationSession,
  type AutomationStream,
  type AutomationWait,
  PROTOCOL_VERSION,
  type ClientMessage,
  type Command,
  type DiagnosticCursor,
  type DiagnosticLevel,
  type DiagnosticOrigin,
  type DiagnosticRecord,
  type DiagnosticQuery,
  type DiagnosticLogs,
  type DiagnosticTimelineQuery,
  type DiagnosticTimeline,
  type DiagnosticCounterReport,
  type DiagnosticCounters,
  type DiagnosticVerbosity,
  type IssueRun,
  type IssueRunAction,
  type IssueWorkflow,
  type ReplayCursor,
  type ResponsePayload,
  type RpcError,
  type SequencedEvent,
  type ServerMessage,
} from "./generated";

const NIL_UUID = "00000000-0000-0000-0000-000000000000";
const OPEN = 1;
const MAX_BUFFERED_EVENTS_PER_RUNTIME = 4096;
// A complete retained tail plus its preceding server/local expiry marker.
const MAX_BUFFERED_AUTOMATION_MESSAGES = MAX_AUTOMATION_EVENTS + 1;
// A retained replay tail plus the bounded live tail that raced its response.
const MAX_BUFFERED_AUTOMATION_REPLAY = MAX_BUFFERED_AUTOMATION_MESSAGES * 2;

export type EventListener = (event: SequencedEvent) => void;

/** One message from the instance's diagnostic stream. A reconnect that
 * outran retention is named as a `diagnosticCursorExpired` before the
 * retained records, so a consumer knows a gap exists instead of inferring
 * one from a sequence jump. */
export type DiagnosticMessage = Extract<
  ServerMessage,
  { type: "diagnostic" } | { type: "diagnosticCursorExpired" }
>;
export type DiagnosticListener = (message: DiagnosticMessage) => void;
export type AutomationMessage =
  | { type: "event"; event: AutomationEvent }
  | { type: "expired"; expiry: AutomationExpiry }
  | { type: "waitResolved"; wait: AutomationWait }
  | { type: "runChanged"; run: AutomationRun };
export type AutomationListener = (message: AutomationMessage) => void;
export type AutomationInterest = Extract<ServerMessage, { type: "automationInterest" }>;
export type AutomationExecutorHandler =
  (request: AutomationExecutorRequest) => AutomationExecutorOutcome | Promise<AutomationExecutorOutcome>;
export type AutomationSuccess = Exclude<AutomationResult, { type: "error" }>;

/** The original automation failure, including its cross-process evidence. */
export class WakuAutomationError extends Error {
  constructor(readonly error: AutomationError) {
    super(`${error.code}: ${error.message}`);
    this.name = "WakuAutomationError";
  }
}

export type WakuConnectionState = "disconnected" | "connecting" | "connected";
export type ConnectionStateListener = (state: WakuConnectionState) => void;

export interface WebSocketLike {
  readonly readyState: number;
  send(data: string): void;
  close(code?: number, reason?: string): void;
  addEventListener(type: "open", listener: () => void): void;
  addEventListener(type: "message", listener: (event: MessageEvent) => void): void;
  addEventListener(type: "error", listener: () => void): void;
  addEventListener(type: "close", listener: (event: CloseEvent) => void): void;
}

export interface WakuClientOptions {
  /** A daemon address (`127.0.0.1:34123`) or complete ws(s) URL. */
  address: string;
  token: string;
  clientId?: string;
  requestTimeoutMs?: number;
  /** How long a handshake may take before the attempt fails as `timeout`. */
  connectTimeoutMs?: number;
  webSocketFactory?: (url: string) => WebSocketLike;
  randomUUID?: () => string;
  now?: () => number;
}

export interface RequestOptions {
  /** Overrides the corresponding client-wide timeout for this request or connection attempt. */
  timeoutMs?: number;
}

export class WakuRpcError extends Error {
  readonly error: RpcError;

  constructor(error: RpcError | string) {
    const detail = typeof error === "string" ? { message: error } : error;
    super(detail.message);
    this.name = "WakuRpcError";
    this.error = detail;
  }
}

/** Why a connection attempt failed or an established connection ended. */
export type WakuConnectionFailure =
  /** The daemon refused the token. */
  | "rejected"
  /** The daemon speaks another protocol version. */
  | "protocol"
  /** Whatever answered did not speak the Waku protocol. */
  | "handshake"
  /** The socket failed or closed before the handshake completed. */
  | "unreachable"
  /** The handshake did not settle within `connectTimeoutMs`. */
  | "timeout"
  /** An established connection closed. */
  | "closed"
  /** `disconnect()` ended the attempt. */
  | "aborted";

export class WakuConnectionError extends Error {
  readonly kind: WakuConnectionFailure;

  constructor(kind: WakuConnectionFailure, message: string) {
    super(message);
    this.name = "WakuConnectionError";
    this.kind = kind;
  }

  /** True when a later attempt could succeed without anyone changing anything. */
  get retryable(): boolean {
    return this.kind === "unreachable" || this.kind === "timeout" || this.kind === "closed";
  }
}

interface PendingRequest {
  resolve: (payload: ResponsePayload) => void;
  reject: (error: Error) => void;
  timeout: ReturnType<typeof setTimeout>;
  startedAt: number;
}

interface LastSequence {
  epoch: string;
  sequence: number;
}

interface AutomationBuffer {
  messages: AutomationMessage[];
  limit: number;
}

interface AutomationResumeGap {
  cursor: AutomationCursor;
  notification: boolean;
}

/** Browser-safe client for Waku's versioned JSON-over-WebSocket protocol. */
export class WakuClient {
  readonly clientId: string;

  private readonly address: string;
  private readonly token: string;
  private readonly requestTimeoutMs: number;
  private readonly connectTimeoutMs: number;
  private readonly socketFactory: (url: string) => WebSocketLike;
  private readonly randomUUID: () => string;
  private readonly now: () => number;
  private socket?: WebSocketLike;
  private state: WakuConnectionState = "disconnected";
  private pending = new Map<string, PendingRequest>();
  private subscriptions = new Map<string, Set<EventListener>>();
  private pendingEvents = new Map<string, SequencedEvent[]>();
  private taskStateListeners = new Set<(revision: number) => void>();
  private diagnosticListeners = new Set<DiagnosticListener>();
  private pendingDiagnostics: DiagnosticMessage[] = [];
  private lastDiagnostic?: DiagnosticCursor;
  private verbosity: DiagnosticVerbosity = { defaultLevel: "info", overrides: [] };
  private requestRoundTripMs = 0;
  private connectionStateListeners = new Set<ConnectionStateListener>();
  private sequences = new Map<string, LastSequence>();
  private connectionGeneration = 0;
  private rejectConnect?: (error: Error) => void;
  private receivedAt = 0;
  private disconnectReason: string | null = null;
  private automationListeners = new Map<string, Set<AutomationListener>>();
  private pendingAutomation = new Map<string, AutomationBuffer>();
  private automationCursors = new Map<string, AutomationCursor>();
  private resumingAutomation = new Map<string, AutomationMessage[]>();
  private droppedResumeAutomation = new Map<string, AutomationResumeGap>();
  private executorHandler?: AutomationExecutorHandler;
  private executorInterest?: (interest: AutomationInterest) => void;
  private executorQueue: Promise<void> = Promise.resolve();
  private executorQueueLength = 0;

  constructor(options: WakuClientOptions) {
    this.address = options.address;
    this.token = options.token;
    this.requestTimeoutMs = options.requestTimeoutMs ?? 120_000;
    this.connectTimeoutMs = options.connectTimeoutMs ?? 15_000;
    this.socketFactory =
      options.webSocketFactory ??
      ((url) => {
        if (typeof WebSocket === "undefined") {
          throw new Error("WebSocket is unavailable; provide webSocketFactory");
        }
        return new WebSocket(url);
      });
    this.randomUUID =
      options.randomUUID ??
      (() => {
        if (typeof crypto === "undefined" || !crypto.randomUUID) {
          throw new Error("crypto.randomUUID is unavailable; provide randomUUID");
        }
        return crypto.randomUUID();
      });
    this.now = options.now ?? Date.now;
    this.clientId = options.clientId ?? this.randomUUID();
  }

  get connected(): boolean {
    return this.state === "connected";
  }

  get connectionState(): WakuConnectionState {
    return this.state;
  }

  /** When the daemon last sent anything on the current or most recent connection; 0 before the first. */
  get lastMessageAt(): number {
    return this.receivedAt;
  }

  /** The close reason of the last established connection that ended remotely, when the peer gave one. */
  get lastDisconnectReason(): string | null {
    return this.disconnectReason;
  }

  /** Connects, or reconnects while replaying events after the last seen sequence. */
  connect(options: RequestOptions = {}): Promise<void> {
    if (this.state === "connected") return Promise.resolve();
    if (this.state === "connecting") {
      return Promise.reject(new Error("Waku client is already connecting"));
    }

    this.setConnectionState("connecting");
    const generation = ++this.connectionGeneration;
    let socket: WebSocketLike;
    try {
      socket = this.socketFactory(daemonUrl(this.address));
    } catch (error) {
      this.setConnectionState("disconnected");
      return Promise.reject(asError(error));
    }
    this.socket = socket;

    return new Promise((resolve, reject) => {
      let handshakeSettled = false;
      let established = false;
      let socketErrored = false;
      let connectTimer: ReturnType<typeof setTimeout> | undefined;
      const failHandshake = (error: Error) => {
        if (handshakeSettled) return;
        handshakeSettled = true;
        clearTimeout(connectTimer);
        if (this.rejectConnect === failHandshake) this.rejectConnect = undefined;
        this.setConnectionState("disconnected");
        reject(error);
      };
      this.rejectConnect = failHandshake;
      connectTimer = setTimeout(() => {
        if (handshakeSettled) return;
        // Abandon the socket: whatever it reports later must not touch this
        // client, which may already be on its next attempt.
        ++this.connectionGeneration;
        if (this.socket === socket) this.socket = undefined;
        failHandshake(
          new WakuConnectionError("timeout", "timed out connecting to Waku daemon"),
        );
        socket.close(1000, "connect timeout");
      }, options.timeoutMs ?? this.connectTimeoutMs);

      socket.addEventListener("open", () => {
        if (generation !== this.connectionGeneration) return;
        const hello: ClientMessage = {
          type: "hello",
          protocolVersion: PROTOCOL_VERSION,
          token: this.token,
          clientId: this.clientId,
          resumeFrom: this.replayCursors(),
          // Omitted while undefined, so a client that has never read the
          // stream asks for the retained tail.
          diagnosticFrom: this.lastDiagnostic,
        };
        socket.send(JSON.stringify(hello));
      });
      socket.addEventListener("message", (event) => {
        if (generation !== this.connectionGeneration) return;
        this.receivedAt = this.now();
        let message: ServerMessage;
        try {
          message = JSON.parse(String(event.data)) as ServerMessage;
        } catch {
          failHandshake(
            new WakuConnectionError("handshake", "Waku daemon sent invalid JSON"),
          );
          return;
        }

        if (!handshakeSettled) {
          if (message.type === "hello") {
            if (message.protocolVersion !== PROTOCOL_VERSION) {
              failHandshake(
                new WakuConnectionError(
                  "protocol",
                  `daemon protocol ${message.protocolVersion} does not match client protocol ${PROTOCOL_VERSION}`,
                ),
              );
              socket.close(1002, "protocol version mismatch");
              return;
            }
            handshakeSettled = true;
            established = true;
            clearTimeout(connectTimer);
            if (this.rejectConnect === failHandshake) this.rejectConnect = undefined;
            this.setConnectionState("connected");
            resolve();
            return;
          }
          if (message.type === "rejected") {
            failHandshake(rejectionError(message.message));
            socket.close(1008, "authentication rejected");
            return;
          }
          failHandshake(
            new WakuConnectionError(
              "handshake",
              "Waku daemon sent an invalid handshake response",
            ),
          );
          socket.close(1002, "invalid handshake");
          return;
        }
        this.handleMessage(message);
      });
      socket.addEventListener("error", () => {
        // React Native puts the useful native network error on the close
        // event's reason, immediately after this otherwise-empty error event.
        socketErrored = true;
      });
      socket.addEventListener("close", (event) => {
        if (generation !== this.connectionGeneration) return;
        const reason = event.reason?.trim();
        if (established) this.disconnectReason = reason || null;
        failHandshake(
          new WakuConnectionError(
            "unreachable",
            reason ||
              (socketErrored
                ? "Waku daemon connection failed"
                : "Waku daemon disconnected during handshake"),
          ),
        );
        this.markDisconnected(
          new WakuConnectionError("closed", "Waku daemon disconnected"),
        );
      });
    });
  }

  request(
    command: Command,
    sessionId = NIL_UUID,
    runtimeId = NIL_UUID,
    options: RequestOptions = {},
  ): Promise<ResponsePayload> {
    let socket: WebSocketLike;
    try {
      socket = this.requireSocket();
    } catch (error) {
      return Promise.reject(asError(error));
    }
    const requestId = this.randomUUID();
    const message: ClientMessage = {
      type: "request",
      requestId,
      sessionId,
      runtimeId,
      command,
    };

    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        this.pending.delete(requestId);
        reject(new Error("timed out waiting for Waku daemon"));
      }, options.timeoutMs ?? this.requestTimeoutMs);
      this.pending.set(requestId, { resolve, reject, timeout, startedAt: this.now() });
      try {
        socket.send(JSON.stringify(message));
      } catch (error) {
        clearTimeout(timeout);
        this.pending.delete(requestId);
        reject(asError(error));
      }
    });
  }

  /** Lists durable workflow state; reconnecting never restarts an agent. */
  async listIssueRuns(taskId: string | null = null, options: RequestOptions = {}): Promise<IssueRun[]> {
    const response = await this.request(
      { type: "listIssueRuns", taskId }, NIL_UUID, NIL_UUID, options,
    );
    if (response.type !== "issueRuns") throw new Error("Waku daemon returned an invalid issue-runs response");
    return response.runs;
  }

  /** Keep requestId for retries after a timeout or reconnect. It identifies the
   * user's start intent, independently of this transport request's UUID. */
  async startIssueRun(
    taskId: string,
    requestId: string,
    workflow: IssueWorkflow,
    options: RequestOptions = {},
  ): Promise<IssueRun> {
    const response = await this.request(
      { type: "startIssueRun", taskId, requestId, workflow }, NIL_UUID, NIL_UUID, options,
    );
    if (response.type !== "issueRun") throw new Error("Waku daemon returned an invalid issue-run response");
    return response.run;
  }

  async controlIssueRun(
    runId: string,
    action: IssueRunAction,
    options: RequestOptions = {},
  ): Promise<IssueRun> {
    const response = await this.request(
      { type: "controlIssueRun", runId, action }, NIL_UUID, NIL_UUID, options,
    );
    if (response.type !== "issueRun") throw new Error("Waku daemon returned an invalid issue-run response");
    return response.run;
  }

  async notify(
    command: Command,
    sessionId = NIL_UUID,
    runtimeId = NIL_UUID,
  ): Promise<void> {
    const message: ClientMessage = {
      type: "request",
      // The nil request id is the protocol's fire-and-forget marker. Runtime
      // ordering is preserved, but high-frequency controls such as terminal
      // input do not create response traffic or response-cache entries.
      requestId: NIL_UUID,
      sessionId,
      runtimeId,
      command,
    };
    this.requireSocket().send(JSON.stringify(message));
  }

  subscribe(sessionId: string, runtimeId: string, listener: EventListener): () => void {
    const key = subscriptionKey(sessionId, runtimeId);
    let listeners = this.subscriptions.get(key);
    if (!listeners) {
      listeners = new Set();
      this.subscriptions.set(key, listeners);
    }
    listeners.add(listener);
    const buffered = this.pendingEvents.get(key);
    if (buffered) {
      this.pendingEvents.delete(key);
      for (const event of buffered) listener(event);
    }
    return () => {
      listeners?.delete(listener);
      if (listeners?.size === 0) this.subscriptions.delete(key);
    };
  }

  subscribeTaskState(listener: (revision: number) => void): () => void {
    this.taskStateListeners.add(listener);
    return () => this.taskStateListeners.delete(listener);
  }

  /** Uses the same authenticated connection as every other daemon request.
   * Executor errors retain their complete bundle in WakuAutomationError.error. */
  async automation(command: AutomationCommand, options: RequestOptions = {}): Promise<AutomationSuccess> {
    const resuming = command.type === "resume" ? command.sessionId : undefined;
    if (resuming) {
      if (this.resumingAutomation.has(resuming)) throw new Error(`automation session ${resuming} is already resuming`);
      this.resumingAutomation.set(resuming, []);
    }
    try {
      const response = await this.request({ type: "automation", command }, NIL_UUID, NIL_UUID, options);
      if (response.type !== "automation") throw new Error("unexpected automation response");
      const result = response.result;
      if (result.type === "error") throw new WakuAutomationError(result.error);
      if (result.type === "attached") {
        const sessionId = result.session.sessionId;
        // Hold live events until replay has been delivered. Otherwise a newer
        // live cursor could make the client silently discard older replay.
        const live = this.resumingAutomation.get(sessionId) ?? [];
        const dropped = this.droppedResumeAutomation.get(sessionId);
        this.resumingAutomation.delete(sessionId);
        this.droppedResumeAutomation.delete(sessionId);
        let notifications: AutomationMessage[] = [];
        if (resuming === sessionId) {
          // Replace stale unread stream history only after a successful resume.
          // Wait/run notifications are not part of replay and must survive it.
          notifications = (this.pendingAutomation.get(sessionId)?.messages ?? [])
            .filter(message => message.type === "waitResolved" || message.type === "runChanged");
          this.pendingAutomation.set(sessionId, { messages: [], limit: MAX_BUFFERED_AUTOMATION_REPLAY });
        }
        if (result.replay.expiry) this.deliverAutomation(sessionId, { type: "expired", expiry: result.replay.expiry });
        // Explicit replay follows the caller's consumption cursor. The receive
        // high-water mark can include events lost to a local buffer overflow.
        for (const event of result.replay.events) this.deliverAutomationEvent(event);
        const cursor = this.automationCursors.get(sessionId);
        if (!cursor || cursor.epoch !== result.replay.cursor.epoch || cursor.sequence < result.replay.cursor.sequence) {
          this.automationCursors.set(sessionId, result.replay.cursor);
        }
        for (const notification of notifications) this.deliverAutomation(sessionId, notification);
        this.flushAutomationResume(sessionId, live, dropped);
      }
      if (command.type === "detach") {
        this.automationCursors.delete(command.sessionId);
        this.pendingAutomation.delete(command.sessionId);
        this.automationListeners.delete(command.sessionId);
      }
      return result;
    } finally {
      if (resuming) {
        const live = this.resumingAutomation.get(resuming) ?? [];
        const dropped = this.droppedResumeAutomation.get(resuming);
        this.resumingAutomation.delete(resuming);
        this.droppedResumeAutomation.delete(resuming);
        this.flushAutomationResume(resuming, live, dropped);
      }
    }
  }

  async automationInstances(): Promise<AutomationInstance[]> {
    const result = await this.automation({ type: "instances" });
    if (result.type !== "instances") throw new Error("unexpected automation instances response");
    return result.instances;
  }

  async attachAutomation(instanceId: string): Promise<AutomationSession> {
    const result = await this.automation({ type: "attach", instanceId });
    if (result.type !== "attached") throw new Error("unexpected automation attach response");
    return result.session;
  }

  async resumeAutomation(sessionId: string, after = this.lastAutomationCursor(sessionId)): Promise<AutomationSession> {
    const result = await this.automation({ type: "resume", sessionId, after: after ?? null });
    if (result.type !== "attached") throw new Error("unexpected automation resume response");
    return result.session;
  }

  async automationCall(sessionId: string, call: AutomationCall): Promise<AutomationCallResult> {
    const result = await this.automation({ type: "call", sessionId, call });
    if (result.type !== "called") throw new Error("unexpected automation call response");
    return result.result;
  }

  /** Local listener, independent of the daemon's Subscribe stream selection.
   * Attach/resume replay, pushed changes and wait/run notifications all land here. */
  subscribeAutomation(sessionId: string, listener: AutomationListener): () => void {
    let listeners = this.automationListeners.get(sessionId);
    if (!listeners) this.automationListeners.set(sessionId, listeners = new Set());
    listeners.add(listener);
    const pending = this.pendingAutomation.get(sessionId)?.messages ?? [];
    this.pendingAutomation.delete(sessionId);
    for (const message of pending) listener(message);
    return () => {
      listeners.delete(listener);
      if (!listeners.size) this.automationListeners.delete(sessionId);
    };
  }

  lastAutomationCursor(sessionId: string): AutomationCursor | undefined {
    return this.automationCursors.get(sessionId);
  }

  /** Incoming work is serialized on a promise queue, outside socket dispatch.
   * An async executor can wait without delaying RPC replies or stream delivery. */
  setAutomationExecutorHandler(handler: AutomationExecutorHandler,
    interest?: (interest: AutomationInterest) => void): () => void {
    this.executorHandler = handler;
    this.executorInterest = interest;
    return () => {
      if (this.executorHandler === handler) {
        this.executorHandler = undefined;
        this.executorInterest = undefined;
      }
    };
  }

  async registerAutomationExecutor(instance: AutomationInstance): Promise<AutomationInstance> {
    const result = await this.automation({ type: "registerExecutor", instance });
    if (result.type !== "registered") throw new Error("unexpected automation registration response");
    return result.instance;
  }

  publishAutomationEvent(event: AutomationExecutorEvent): void {
    this.requireSocket().send(JSON.stringify({ type: "automationExecutorEvent", event } satisfies ClientMessage));
  }

  async diagnosticLogs(query: DiagnosticQuery = {}): Promise<DiagnosticLogs> {
    const response = await this.request({ type: "diagnosticLogs", query });
    if (response.type !== "diagnosticLogs") throw new Error("unexpected diagnostic logs response");
    return response.result;
  }

  async diagnosticTimeline(query: DiagnosticTimelineQuery): Promise<DiagnosticTimeline> {
    const response = await this.request({ type: "diagnosticTimeline", query });
    if (response.type !== "diagnosticTimeline") throw new Error("unexpected diagnostic timeline response");
    return response.result;
  }

  async diagnosticCounters(): Promise<DiagnosticCounters> {
    const response = await this.request({ type: "diagnosticCounters" });
    if (response.type !== "diagnosticCounters") throw new Error("unexpected diagnostic counters response");
    return response.result;
  }

  async diagnosticVerbosity(update?: DiagnosticVerbosity): Promise<DiagnosticVerbosity> {
    const response = await this.request({ type: "diagnosticVerbosity", update: update ?? null });
    if (response.type !== "diagnosticVerbosity") throw new Error("unexpected diagnostic verbosity response");
    // Ordered broadcasts own the cached filter; this response can predate a
    // later update already received while the caller was awaiting it.
    return response.verbosity;
  }

  diagnosticEnabled(level: DiagnosticLevel, origin: DiagnosticOrigin, source: string): boolean {
    let threshold = this.verbosity.defaultLevel;
    for (const rule of this.verbosity.overrides) {
      if ((rule.process == null || rule.process === origin.process)
          && (rule.source == null || rule.source === source)) threshold = rule.level;
    }
    const levels: DiagnosticLevel[] = ["trace", "debug", "info", "warn", "error"];
    return levels.indexOf(level) >= levels.indexOf(threshold);
  }

  lastRequestRoundTripMs(): number { return this.requestRoundTripMs; }

  reportDiagnosticCounters(report: DiagnosticCounterReport): void {
    const message: ClientMessage = { type: "diagnosticCounters", ...report };
    this.requireSocket().send(JSON.stringify(message));
  }

  recordDiagnostic(record: DiagnosticRecord): void {
    const message: ClientMessage = { type: "diagnostic", ...record };
    this.requireSocket().send(JSON.stringify(message));
  }

  /** Read the instance's diagnostic stream from the next record onward.
   *
   * A fresh client asks for the retained tail in the handshake; a reconnect
   * passes the last cursor automatically. Records that arrive before the
   * first subscriber are buffered, so a subscription sees the whole handshake
   * replay. */
  subscribeDiagnostics(listener: DiagnosticListener): () => void {
    this.diagnosticListeners.add(listener);
    const pending = this.pendingDiagnostics;
    this.pendingDiagnostics = [];
    for (const message of pending) listener(message);
    return () => this.diagnosticListeners.delete(listener);
  }

  /** The last diagnostic record this client observed; the next connect resumes
   * after it. */
  lastDiagnosticCursor(): DiagnosticCursor | undefined {
    return this.lastDiagnostic;
  }

  /** Observes connection changes, including remote socket closure. */
  subscribeConnectionState(listener: ConnectionStateListener): () => void {
    this.connectionStateListeners.add(listener);
    listener(this.state);
    return () => this.connectionStateListeners.delete(listener);
  }

  replayCursors(): ReplayCursor[] {
    return [...this.sequences].map(([key, cursor]) => {
      const [sessionId, runtimeId] = key.split(":", 2) as [string, string];
      return { sessionId, runtimeId, ...cursor };
    });
  }

  /** Closes only this client connection; it never stops a remotely managed daemon. */
  disconnect(): void {
    this.rejectConnect?.(new WakuConnectionError("aborted", "Waku client disconnected"));
    ++this.connectionGeneration;
    const socket = this.socket;
    this.socket = undefined;
    this.markDisconnected(new WakuConnectionError("aborted", "Waku client disconnected"));
    socket?.close(1000, "client disconnected");
  }

  /** Explicitly requests daemon shutdown, then closes this connection. */
  shutdownDaemon(): void {
    const socket = this.requireSocket();
    socket.send(JSON.stringify({ type: "shutdown" } satisfies ClientMessage));
    this.disconnect();
  }

  private requireSocket(): WebSocketLike {
    if (this.state !== "connected" || !this.socket || this.socket.readyState !== OPEN) {
      throw new Error("Waku daemon is disconnected");
    }
    return this.socket;
  }

  private handleMessage(message: ServerMessage): void {
    if (message.type === "automationExecutorRequest") {
      const generation = this.connectionGeneration;
      const handler = this.executorHandler;
      if (this.executorQueueLength >= MAX_AUTOMATION_EVENTS) {
        this.requireSocket().send(JSON.stringify({ type: "automationExecutorReply", reply: {
          requestId: message.request.requestId,
          outcome: { status: "error", error: { code: "executor_busy", message: "automation executor work queue is full" } },
        } } satisfies ClientMessage));
        return;
      }
      this.executorQueueLength++;
      this.executorQueue = this.executorQueue.then(async () => {
        if (generation !== this.connectionGeneration || !this.connected) return;
        let outcome: AutomationExecutorOutcome;
        try {
          outcome = handler ? await handler(message.request) : {
            status: "error", error: { code: "executor_unavailable", message: "no automation executor handler is listening" },
          };
        } catch (error) {
          outcome = { status: "error", error: error instanceof WakuAutomationError ? error.error : {
            code: "executor_failed", message: asError(error).message,
          } };
        }
        // An answer from a dead connection must never answer a request on its
        // replacement, even if a slow handler finishes after reconnect.
        if (generation !== this.connectionGeneration || !this.connected) return;
        this.requireSocket().send(JSON.stringify({ type: "automationExecutorReply", reply: {
          requestId: message.request.requestId, outcome,
        } } satisfies ClientMessage));
      }).catch(() => { /* Socket closure is reported by the connection lifecycle. */ }).finally(() => {
        if (generation === this.connectionGeneration && this.connected) this.executorQueueLength--;
      });
      return;
    }
    if (message.type === "automationInterest") {
      const listener = this.executorInterest;
      const generation = this.connectionGeneration;
      queueMicrotask(() => {
        if (generation === this.connectionGeneration && this.connected) listener?.(message);
      });
      return;
    }
    if (message.type === "automationEvent") {
      this.automationEvent(message.event);
      return;
    }
    if (message.type === "automationWaitResolved") {
      this.automationNotification(message.sessionId, { type: "waitResolved", wait: message.wait });
      return;
    }
    if (message.type === "automationRunChanged") {
      this.automationNotification(message.sessionId, { type: "runChanged", run: message.run });
      return;
    }

    if (message.type === "response") {
      const pending = this.pending.get(message.requestId);
      if (!pending) return;
      this.pending.delete(message.requestId);
      clearTimeout(pending.timeout);
      this.requestRoundTripMs = Math.max(0, this.now() - pending.startedAt);
      if (message.outcome.status === "ok") pending.resolve(message.outcome.payload);
      else pending.reject(new WakuRpcError(message.outcome.error));
      return;
    }
    if (message.type === "event") {
      const key = subscriptionKey(message.sessionId, message.runtimeId);
      const previous = this.sequences.get(key);
      if (
        previous?.epoch === message.epoch &&
        message.sequence <= previous.sequence
      ) {
        return;
      }
      this.sequences.set(key, {
        epoch: message.epoch,
        sequence: message.sequence,
      });
      const listeners = this.subscriptions.get(key);
      if (listeners?.size) {
        for (const listener of listeners) listener(message);
      } else {
        const buffered = this.pendingEvents.get(key) ?? [];
        buffered.push(message);
        if (buffered.length > MAX_BUFFERED_EVENTS_PER_RUNTIME) {
          buffered.splice(0, buffered.length - MAX_BUFFERED_EVENTS_PER_RUNTIME);
        }
        this.pendingEvents.set(key, buffered);
      }
      return;
    }
    if (message.type === "taskStateChanged") {
      for (const listener of this.taskStateListeners) listener(message.revision);
      return;
    }
    if (message.type === "diagnosticVerbosity") {
      this.verbosity = message.verbosity;
      return;
    }
    if (message.type === "diagnostic") {
      const previous = this.lastDiagnostic;
      if (
        previous?.epoch === message.epoch &&
        message.sequence <= previous.sequence
      ) {
        return;
      }
      this.lastDiagnostic = { epoch: message.epoch, sequence: message.sequence };
      this.deliverDiagnostic(message);
      return;
    }
    if (message.type === "diagnosticCursorExpired") {
      this.deliverDiagnostic(message);
      return;
    }
    if (message.type === "shuttingDown") {
      this.socket?.close(1000, "daemon shutting down");
    }
  }

  private automationEvent(event: AutomationEvent): void {
    if (this.bufferAutomationResume(event.sessionId, { type: "event", event })) return;
    const previous = this.automationCursors.get(event.sessionId);
    if (previous?.epoch === event.cursor.epoch && previous.sequence >= event.cursor.sequence) return;
    this.deliverAutomationEvent(event);
  }

  private automationNotification(sessionId: string, message: AutomationMessage): void {
    if (!this.bufferAutomationResume(sessionId, message)) this.deliverAutomation(sessionId, message);
  }

  private bufferAutomationResume(sessionId: string, message: AutomationMessage): boolean {
    const resuming = this.resumingAutomation.get(sessionId);
    if (!resuming) return false;
    if (resuming.length >= MAX_AUTOMATION_EVENTS) {
      const dropped = resuming.shift()!;
      this.droppedResumeAutomation.set(sessionId, {
        cursor: automationMessageCursor(dropped),
        notification: dropped.type !== "event" || (this.droppedResumeAutomation.get(sessionId)?.notification ?? false),
      });
    }
    resuming.push(message);
    return true;
  }

  private deliverAutomationEvent(event: AutomationEvent): void {
    const previous = this.automationCursors.get(event.sessionId);
    if (!previous || previous.epoch !== event.cursor.epoch || previous.sequence < event.cursor.sequence) {
      this.automationCursors.set(event.sessionId, event.cursor);
    }
    this.deliverAutomation(event.sessionId, { type: "event", event });
  }

  private flushAutomationResume(sessionId: string, messages: AutomationMessage[], dropped?: AutomationResumeGap): void {
    if (dropped) {
      const requested = this.automationCursors.get(sessionId) ?? { epoch: dropped.cursor.epoch, sequence: 0 };
      if (dropped.notification || requested.epoch !== dropped.cursor.epoch || requested.sequence < dropped.cursor.sequence) {
        this.deliverAutomation(sessionId, { type: "expired", expiry: {
          requested, oldestSequence: messages[0] ? automationMessageCursor(messages[0]).sequence : dropped.cursor.sequence + 1,
          cursor: messages.length ? automationMessageCursor(messages.at(-1)!) : dropped.cursor,
        } });
      }
    }
    for (const message of messages) {
      if (message.type === "event") this.automationEvent(message.event);
      else this.deliverAutomation(sessionId, message);
    }
  }

  private deliverAutomation(sessionId: string, message: AutomationMessage): void {
    const listeners = this.automationListeners.get(sessionId);
    if (listeners?.size) {
      for (const listener of listeners) {
        // One consumer must not interrupt replay or another consumer's stream.
        try { listener(message); } catch { /* Listener failures belong to the consumer. */ }
      }
      return;
    }
    const buffer = this.pendingAutomation.get(sessionId) ?? { messages: [], limit: MAX_BUFFERED_AUTOMATION_MESSAGES };
    const pending = buffer.messages;
    if (pending.length >= buffer.limit) {
      const first = pending[0]!;
      const cursor = automationMessageCursor(message);
      const previous = automationMessageCursor(first);
      const requested = first.type === "expired" ? first.expiry.requested
        : { ...previous, sequence: Math.max(0, previous.sequence - 1) };
      // Never silently discard a pre-listener backlog. Consumers can resync
      // and query wait/run results after this explicit local notification gap.
      pending.length = 0;
      pending.push({ type: "expired", expiry: { requested, oldestSequence: cursor.sequence, cursor } });
    }
    pending.push(message);
    this.pendingAutomation.set(sessionId, buffer);
  }

  private deliverDiagnostic(message: DiagnosticMessage): void {
    if (this.diagnosticListeners.size === 0) {
      this.pendingDiagnostics.push(message);
      // The daemon's own retention bound, so a handshake replay is never
      // truncated by a narrower client buffer.
      if (this.pendingDiagnostics.length > MAX_DIAGNOSTIC_EVENTS) {
        this.pendingDiagnostics.splice(
          0,
          this.pendingDiagnostics.length - MAX_DIAGNOSTIC_EVENTS,
        );
      }
      return;
    }
    for (const listener of this.diagnosticListeners) listener(message);
  }

  /** Drops the socket and settles its requests before telling listeners, so a
   * listener that reconnects on the spot works against a consistent client. */
  private markDisconnected(error: Error): void {
    this.socket = undefined;
    // Old handlers may finish, but must not stall a replacement connection.
    this.executorQueue = Promise.resolve();
    this.executorQueueLength = 0;
    const pending = [...this.pending.values()];
    this.pending.clear();
    for (const request of pending) {
      clearTimeout(request.timeout);
      request.reject(error);
    }
    this.setConnectionState("disconnected");
  }

  private setConnectionState(state: WakuConnectionState): void {
    if (this.state === state) return;
    this.state = state;
    for (const listener of this.connectionStateListeners) listener(state);
  }
}

export function daemonUrl(address: string): string {
  const normalized = /^(?:ws|wss):\/\//.test(address) ? address : `ws://${address}`;
  const url = new URL(normalized);
  url.pathname = "/v1";
  url.search = "";
  url.hash = "";
  return url.toString();
}

function subscriptionKey(sessionId: string, runtimeId: string): string {
  return `${sessionId}:${runtimeId}`;
}

/** The daemon rejects an unsupported protocol before it can say hello, so a
 * version mismatch arrives as a rejection rather than a mismatched hello. */
function rejectionError(message: string): WakuConnectionError {
  const kind = /^protocol \d+ is unsupported/.test(message) ? "protocol" : "rejected";
  return new WakuConnectionError(kind, `daemon rejected connection: ${message}`);
}

function asError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error));
}

function automationMessageCursor(message: AutomationMessage): AutomationCursor {
  switch (message.type) {
    case "event": return message.event.cursor;
    case "expired": return message.expiry.cursor;
    case "waitResolved": return message.wait.cursor;
    case "runChanged": return message.run.cursor;
  }
}
