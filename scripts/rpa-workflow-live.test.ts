// Opt-in native scratch acceptance. Mutates only a disposable app and never
// submits a model prompt. Use the existing authenticated daemon connection.
import { expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { existsSync, readFileSync } from "node:fs";
import { WakuAutomationError, WakuClient, type AutomationMessage } from "../packages/waku-client/src/client";
import type { AgentSession, AutomationCondition, AutomationRun, AutomationScenario, AutomationWait, Task } from "../packages/waku-client/src/generated";

const liveTest = process.env.WAKU_RPA_WORKFLOW_TEST === "1" ? test : test.skip;

function field(value: unknown, ...keys: string[]): unknown {
  for (const key of keys) {
    if (value === null || typeof value !== "object") return undefined;
    value = (value as Record<string, unknown>)[key];
  }
  return value;
}

function connect() {
  const { WAKU_DAEMON_ADDRESS: address, WAKU_DAEMON_TOKEN: token } = process.env;
  if (!address || !token) throw new Error("native scratch test requires the existing daemon address/token");
  return new WakuClient({ address, token, requestTimeoutMs: 40_000 });
}

// Keep pushed completions even if they arrive before the initiating RPC reply.
// No snapshot polling: the daemon resolves waits and scenarios from its events.
function notifications(client: WakuClient, sessionId: string) {
  const received: AutomationMessage[] = [];
  const listeners = new Set<() => void>();
  const unsubscribe = client.subscribeAutomation(sessionId, message => {
    received.push(message);
    for (const listener of listeners) listener();
  });
  return {
    received,
    close: unsubscribe,
    async find(predicate: (message: AutomationMessage) => boolean): Promise<AutomationMessage> {
      const existing = received.find(predicate);
      if (existing) return existing;
      return new Promise((resolve, reject) => {
        const finish = () => {
          const message = received.find(predicate);
          if (message) { clearTimeout(timer); listeners.delete(finish); resolve(message); }
        };
        const timer = setTimeout(() => { listeners.delete(finish); reject(new Error("native automation notification deadline")); }, 15_000);
        listeners.add(finish);
        finish();
      });
    },
  };
}

liveTest("native scratch actions, pushed waits, independent resume and failed scenario leave the working app untouched", async () => {
  const client = connect();
  const other = connect();
  let workingSession: string | undefined;
  let scratchId: string | undefined;
  let scratchSession: string | undefined;
  let events: ReturnType<typeof notifications> | undefined;
  let resumedEvents: ReturnType<typeof notifications> | undefined;
  try {
    await client.connect();
    const instances = await client.automationInstances();
    const candidates = instances.filter(instance => !instance.scratch && instance.executorConnected &&
      (!process.env.WAKU_RPA_TEST_INSTANCE || instance.instanceId === process.env.WAKU_RPA_TEST_INSTANCE));
    expect(candidates.length).toBe(1);
    const working = candidates[0]!;
    workingSession = (await client.attachAutomation(working.instanceId)).sessionId;
    const before = await client.automationCall(workingSession, { type: "snapshot", sections: ["model", "composer", "route"] });

    const created = await client.automation({ type: "scratchCreate", headless: false, name: "Native isolated draft acceptance", seed: { projects: [], sessions: [] } }, { timeoutMs: 40_000 });
    if (created.type !== "scratchCreated") throw new Error("unexpected scratch response");
    scratchId = created.instance.instanceId;
    expect(created.instance.scratch).toBe(true);
    expect(created.instance.executorConnected).toBe(true);
    expect(created.instance.database).not.toBe(working.database);
    expect(created.instance.checkout).toBe(working.checkout);
    const scratchDatabase = created.instance.database;
    scratchSession = (await client.attachAutomation(scratchId)).sessionId;
    events = notifications(client, scratchSession);
    await client.automation({ type: "subscribe", sessionId: scratchSession, streams: ["ui", "model"] });

    const wait = async (condition: AutomationCondition, timeoutMs = 5_000): Promise<AutomationWait> => {
      const result = await client.automation({ type: "wait", sessionId: scratchSession!, condition, timeoutMs });
      if (result.type !== "wait") throw new Error("unexpected wait response");
      if (result.wait.status !== "pending") return result.wait;
      const pushed = await events!.find(message => message.type === "waitResolved" && message.wait.waitId === result.wait.waitId);
      if (pushed.type !== "waitResolved") throw new Error("unexpected wait notification");
      return pushed.wait;
    };
    expect((await wait({ on: "node", id: "board-new-issue", present: true })).status).toBe("succeeded");
    const opened = await client.automationCall(scratchSession, { type: "invoke", verb: "newSession", args: {} });
    expect(opened.treeDiff).toBeObject();
    expect((await wait({ on: "node", id: "composer-field", present: true })).status).toBe("succeeded");
    // Provider discovery starts asynchronously. Wait for the isolated,
    // all-disabled profile to reach its settled composer state before
    // asserting the disabled send affordance; never submit during startup.
    expect((await wait({ on: "state", pointer: "/composer/providerAvailable", equals: false }, 15_000)).status).toBe("succeeded");

    const text = `Isolated RPA draft ${crypto.randomUUID()}`;
    const pending = await client.automation({ type: "wait", sessionId: scratchSession, condition: { on: "state", pointer: "/composer/text", equals: text }, timeoutMs: 5_000 });
    if (pending.type !== "wait") throw new Error("unexpected pending wait response");
    expect(pending.wait.status).toBe("pending");
    // A registered wait cannot block an unrelated executor call.
    const description = await client.automationCall(scratchSession, { type: "describe" });
    expect(field(description.value, "instance", "instanceId")).toBe(scratchId);
    const typed = await client.automationCall(scratchSession, { type: "type", text, target: { id: "composer-field" } });
    expect(field(typed.observed, "state", "composer", "text")).toBe(text);
    expect(typed.stateDiff).toBeArray();
    expect(field(typed.observed, "state", "composer", "providerAvailable")).toBe(false);
    try {
      await client.automationCall(scratchSession, { type: "act", action: "click", target: { id: "send-or-stop" } });
      throw new Error("disabled send control unexpectedly accepted input");
    } catch (error) {
      if (!(error instanceof WakuAutomationError)) throw error;
      expect(error.error.code).toBe("disabled");
      expect(field(error.error.bundle?.observed, "state", "composer", "text")).toBe(text);
      expect(error.error.bundle?.counters.processes).toBeArray();
    }
    const completion = await events.find(message => message.type === "waitResolved" && message.wait.waitId === pending.wait.waitId);
    expect(completion.type === "waitResolved" && completion.wait.status).toBe("succeeded");

    // A second physical connection resumes the same epoch without creating a
    // new executor/session or relying on the first client's local buffers.
    await other.connect();
    resumedEvents = notifications(other, scratchSession);
    const resumed = await other.resumeAutomation(scratchSession, opened.cursor);
    expect(resumed.cursor.epoch).toBe(opened.cursor.epoch);
    expect(resumedEvents.received.some(message => message.type === "event")).toBe(true);
    const changed = await other.automation({ type: "diff", sessionId: scratchSession, after: opened.cursor, streams: ["ui", "model"] });
    expect(changed.type === "diff" && changed.diff.events.length > 0).toBe(true);

    const coreSmoke = JSON.parse(readFileSync(new URL("../rpa/scenarios/core-smoke.json", import.meta.url), "utf8")) as AutomationScenario;
    const started = await other.automation({ type: "scenarioStart", sessionId: scratchSession, scenario: {
      name: "Native core smoke and scratch failure preserve evidence", steps: [
        ...coreSmoke.steps,
        { name: "Read the typed draft", type: "call", call: { type: "snapshot", sections: ["tree", "composer"] } },
        { name: "Deliberately missing identity", type: "wait", condition: { on: "node", id: `missing.${crypto.randomUUID()}`, present: true }, timeoutMs: 50 },
        { name: "Must never execute", type: "call", call: { type: "key", chord: "escape" } },
      ],
    } });
    if (started.type !== "scenario") throw new Error("unexpected scenario response");
    let run: AutomationRun = started.run;
    if (run.status === "pending") {
      const terminal = await resumedEvents.find(message => message.type === "runChanged" && message.run.runId === run.runId && message.run.status !== "pending");
      if (terminal.type !== "runChanged") throw new Error("unexpected scenario notification");
      run = terminal.run;
    }
    expect(run.status).toBe("failed");
    for (const step of run.steps.slice(0, coreSmoke.steps.length + 1)) expect(step.status).toBe("succeeded");
    expect(run.steps[coreSmoke.steps.length + 1]?.status).toBe("failed");
    expect(run.steps[coreSmoke.steps.length + 2]?.status).not.toBe("succeeded");
    expect(field(run.error?.bundle?.observed, "state", "composer", "text")).toBe(text);
    expect(field(run.error?.bundle?.observed, "tree", "nodes", "length")).toBeGreaterThan(0);
    expect(run.error?.bundle?.recentEvents?.events).toBeArray();
    expect(run.error?.bundle?.counters.processes).toBeArray();

    const retained = await other.automation({ type: "scenarioResult", sessionId: scratchSession, runId: run.runId });
    expect(retained.type === "scenario" && retained.run.status).toBe("failed");
    const terminalSession = await other.resumeAutomation(scratchSession, run.cursor);
    expect(terminalSession.instance.instanceId).toBe(scratchId);
    // Explicit removal also waits for process reaping and directory deletion;
    // it is idempotent after scenario-owned automatic teardown.
    await other.automation({ type: "scratchRemove", instanceId: scratchId });
    expect((await other.automationInstances()).some(instance => instance.instanceId === scratchId)).toBe(false);
    expect(existsSync(scratchDatabase)).toBe(false);
    const after = await client.automationCall(workingSession, { type: "snapshot", sections: ["model", "composer", "route"] });
    expect(field(after.observed, "state")).toEqual(field(before.observed, "state"));
  } finally {
    events?.close();
    resumedEvents?.close();
    if (scratchId) await client.automation({ type: "scratchRemove", instanceId: scratchId }).catch(() => {});
    if (workingSession) await client.automation({ type: "detach", sessionId: workingSession }).catch(() => {});
    await other.disconnect();
    await client.disconnect();
  }
}, 90_000);


liveTest("native Board issues and conversations keep distinct identities and isolated draft intent", async () => {
  const client = connect();
  let scratchId: string | undefined;
  let sessionId: string | undefined;
  let events: ReturnType<typeof notifications> | undefined;
  try {
    await client.connect();
    const projectId = crypto.randomUUID();
    const issueId = crypto.randomUUID();
    const now = Math.floor(Date.now() / 1000);
    const issue: Task = {
      id: issueId, title: "RPA issue with two conversations", description: "Synthetic history, no provider calls",
      state: "backlog", position: 0, workspace: { kind: "newWorktree", projectId }, created_at: now, updated_at: now,
    };
    const sessions: AgentSession[] = ["First attempt", "Follow-up"].map(title => ({
      id: crypto.randomUUID(), title, project_id: projectId, task_id: issueId, provider: "pi", runtime_mode: "fullAccess", status: "idle",
      created_at: now, updated_at: now, provider_cursor: null,
      messages: [{ id: crypto.randomUUID(), turn_id: null, role: "assistant", content: "Synthetic fixture history", created_at: now, streaming: false }],
      transcript_blocks: [], turns: [],
    }));
    const created = await client.automation({ type: "scratchCreate", headless: false, name: "Board issue identity acceptance", seed: {
      projects: [{ id: projectId, name: "RPA isolated project", path: "/synthetic/rpa-project", created_at: now }],
      tasks: [issue], sessions,
    } }, { timeoutMs: 40_000 });
    if (created.type !== "scratchCreated") throw new Error("unexpected scratch response");
    scratchId = created.instance.instanceId;
    sessionId = (await client.attachAutomation(scratchId)).sessionId;
    events = notifications(client, sessionId);
    const wait = async (condition: AutomationCondition) => {
      const response = await client.automation({ type: "wait", sessionId: sessionId!, condition, timeoutMs: 10_000 });
      if (response.type !== "wait") throw new Error("unexpected wait response");
      const resolved = response.wait.status === "pending"
        ? await events!.find(message => message.type === "waitResolved" && message.wait.waitId === response.wait.waitId)
        : undefined;
      expect(resolved?.type === "waitResolved" ? resolved.wait.status : response.wait.status).toBe("succeeded");
    };
    const invoke = (verb: string, args: Record<string, string> = {}) => client.automationCall(sessionId!, { type: "invoke", verb, args });
    await wait({ on: "node", id: "board-new-issue", present: true });
    await wait({ on: "state", pointer: "/tasks/0/id", equals: issueId });
    const snapshot = await client.automationCall(sessionId, { type: "snapshot", sections: ["tasks", "sessions", "route", "board"] });
    expect(field(snapshot.observed, "state", "tasks")).toEqual([issue]);
    const catalog = field(snapshot.observed, "state", "sessions") as AgentSession[];
    expect(catalog.filter(session => session.task_id === issueId).map(session => session.id).sort()).toEqual(sessions.map(session => session.id).sort());
    expect(catalog.every(session => !("messages" in session))).toBe(true);

    const opened = await invoke("openTask", { id: issueId });
    expect(field(opened.observed, "state", "route", "view")).toBe("issue");
    expect(field(opened.observed, "state", "task", "id")).toBe(issueId);
    try {
      await invoke("openTask", { id: sessions[0]!.id });
      throw new Error("openTask accepted a conversation UUID");
    } catch (error) {
      if (!(error instanceof WakuAutomationError)) throw error;
      expect(error.error.code).toBe("not_found");
    }
    await invoke("openSession", { id: sessions[1]!.id });
    await wait({ on: "state", pointer: "/route/selectedSession", equals: sessions[1]!.id });
    const conversation = await client.automationCall(sessionId, { type: "snapshot", sections: ["task", "session", "route"] });
    expect(field(conversation.observed, "state", "task")).toBeNull();
    expect(field(conversation.observed, "state", "session", "task_id")).toBe(issueId);
    expect(field(conversation.observed, "state", "route", "view")).toBe("session");

    // Hold only this owned scratch database's write lock until the optimistic
    // Done state is observed. The trigger then rejects the save, proving a
    // model wait receives the asynchronous rollback rather than a stale Done.
    await invoke("openTask", { id: issueId });
    const failureDb = new Database(created.instance.database);
    try {
      failureDb.exec("CREATE TRIGGER rpa_reject_completion BEFORE INSERT ON tasks WHEN NEW.state = 'done' BEGIN SELECT RAISE(ABORT, 'rpa deliberate completion failure'); END");
      failureDb.exec("BEGIN IMMEDIATE");
      await client.automationCall(sessionId, { type: "act", action: "click", target: { id: "board-detail-complete" } });
      await wait({ on: "state", pointer: "/task/state", equals: "done" });
      const rollback = await client.automation({ type: "wait", sessionId, condition: { on: "state", pointer: "/task/state", equals: "backlog" }, timeoutMs: 10_000 });
      if (rollback.type !== "wait") throw new Error("unexpected rollback wait response");
      expect(rollback.wait.status).toBe("pending");
      failureDb.exec("COMMIT");
      const rolledBack = await events.find(message => message.type === "waitResolved" && message.wait.waitId === rollback.wait.waitId);
      expect(rolledBack.type === "waitResolved" && rolledBack.wait.status).toBe("succeeded");
      await wait({ on: "stateContains", pointer: "/board/saveError", contains: "rpa deliberate completion failure" });
    } finally {
      if (failureDb.inTransaction) failureDb.exec("ROLLBACK");
      failureDb.exec("DROP TRIGGER IF EXISTS rpa_reject_completion");
      failureDb.close();
    }

    // Finish and reopen through the same visible Board control. Synthetic
    // transcript history must remain accessible after both conversations are archived.
    await invoke("openTask", { id: issueId });
    await client.automationCall(sessionId, { type: "act", action: "click", target: { id: "board-detail-complete" } });
    await wait({ on: "state", pointer: "/task/state", equals: "done" });
    await wait({ on: "state", pointer: "/board/savePending", equals: false });
    const archived = await client.automationCall(sessionId, { type: "snapshot", sections: ["tree", "task", "sessions", "board"] });
    expect(field(archived.observed, "state", "board", "saveError")).toBeNull();
    expect((field(archived.observed, "state", "sessions") as AgentSession[]).filter(session => session.task_id === issueId).length).toBe(2);
    const archivedNodes = field(archived.observed, "tree", "nodes") as { element_id?: string }[];
    for (const session of sessions) expect(archivedNodes.some(node => node.element_id === `session-${session.id}`)).toBe(false);
    await invoke("openSession", { id: sessions[0]!.id });
    await wait({ on: "state", pointer: "/session/id", equals: sessions[0]!.id });
    await wait({ on: "state", pointer: "/session/messages/0/content", equals: "Synthetic fixture history" });
    await invoke("openTask", { id: issueId });
    const stillDone = await client.automationCall(sessionId, { type: "snapshot", sections: ["task"] });
    expect(field(stillDone.observed, "state", "task", "state")).toBe("done");
    await client.automationCall(sessionId, { type: "act", action: "click", target: { id: "board-detail-complete" } });
    await wait({ on: "state", pointer: "/task/state", equals: "doing" });
    await wait({ on: "state", pointer: "/board/savePending", equals: false });
    await invoke("openSession", { id: sessions[1]!.id });
    await wait({ on: "state", pointer: "/session/id", equals: sessions[1]!.id });
    await wait({ on: "state", pointer: "/session/messages/0/content", equals: "Synthetic fixture history" });
    await wait({ on: "node", id: `session-${sessions[1]!.id}`, present: true });

    // A menu opened in the old issue must not carry its cached callbacks
    // into the full New task editor (Cmd-N uses this same handler).
    await invoke("openTask", { id: issueId });
    await client.automationCall(sessionId, { type: "act", action: "click", target: { id: "task-detail-project" } });
    await invoke("newTask");
    const brief = await client.automationCall(sessionId, { type: "snapshot", sections: ["tree", "task", "tasks", "board"] });
    const draftIssueId = field(brief.observed, "state", "task", "id") as string;
    expect(field(brief.observed, "state", "board", "newTaskDialog")).toBe(false);
    expect(field(brief.observed, "state", "board", "newTaskDraft")).toBe(draftIssueId);
    expect((field(brief.observed, "state", "tasks") as Task[]).some(task => task.id === draftIssueId)).toBe(false);
    const briefNodes = field(brief.observed, "tree", "nodes") as { element_id?: string; disabled?: boolean }[];
    expect(briefNodes.some(node => node.element_id === `issue-title-${draftIssueId}`)).toBe(true);
    expect(briefNodes.some(node => node.element_id === `issue-description-${draftIssueId}`)).toBe(true);
    expect(briefNodes.some(node => node.element_id === "new-task-layer")).toBe(false);
    expect(briefNodes.some(node => node.element_id === `task-project-${projectId}`)).toBe(false);
    expect(briefNodes.some(node => node.element_id === "task-detail-new-session")).toBe(false);
    expect(briefNodes.find(node => node.element_id === "new-task-create")?.disabled).toBe(true);
    await client.automationCall(sessionId, { type: "type", text: "   ", target: { id: `issue-title-${draftIssueId}` } });
    const whitespace = await client.automationCall(sessionId, { type: "snapshot", sections: ["tree", "tasks", "sessions"] });
    const whitespaceNodes = field(whitespace.observed, "tree", "nodes") as { element_id?: string; disabled?: boolean }[];
    expect(whitespaceNodes.find(node => node.element_id === "new-task-create")?.disabled).toBe(true);
    expect((field(whitespace.observed, "state", "tasks") as Task[]).length).toBe(1);
    expect((field(whitespace.observed, "state", "sessions") as AgentSession[]).length).toBe(2);
    await client.automationCall(sessionId, { type: "type", text: "Discard this unsaved brief", target: { id: `issue-title-${draftIssueId}` } });
    await client.automationCall(sessionId, { type: "act", action: "click", target: { id: "new-task-cancel" } });
    await wait({ on: "state", pointer: "/board/newTaskDraft", equals: null });
    const cancelled = await client.automationCall(sessionId, { type: "snapshot", sections: ["tasks"] });
    expect((field(cancelled.observed, "state", "tasks") as Task[]).some(task => task.id === draftIssueId)).toBe(false);

    await invoke("openTask", { id: issueId });
    await client.automationCall(sessionId, { type: "act", action: "click", target: { id: "task-detail-workspace" } });
    await invoke("newTask");
    const fresh = await client.automationCall(sessionId, { type: "snapshot", sections: ["task", "tree"] });
    const freshNodes = field(fresh.observed, "tree", "nodes") as { element_id?: string }[];
    expect(freshNodes.some(node => node.element_id === "task-workspace-local")).toBe(false);
    const newIssueId = field(fresh.observed, "state", "task", "id") as string;
    const title = "Empty issue before any conversation";
    const description = "Keep the title, description, and chosen workspace together.";
    await client.automationCall(sessionId, { type: "type", text: title, target: { id: `issue-title-${newIssueId}` } });
    await client.automationCall(sessionId, { type: "type", text: description, target: { id: `issue-description-${newIssueId}` } });
    await client.automationCall(sessionId, { type: "act", action: "click", target: { id: "task-detail-workspace" } });
    await client.automationCall(sessionId, { type: "act", action: "click", target: { id: "task-workspace-local" } });
    // Navigating away and reopening New task retains the same uncommitted brief.
    await invoke("openTask", { id: issueId });
    await invoke("newTask");
    await wait({ on: "state", pointer: "/task/id", equals: newIssueId });
    await client.automationCall(sessionId, { type: "act", action: "click", target: { id: "new-task-create" } });
    await wait({ on: "state", pointer: "/board/newTaskDraft", equals: null });
    await wait({ on: "state", pointer: "/board/savePending", equals: false });
    const empty = await client.automationCall(sessionId, { type: "snapshot", sections: ["task", "tasks", "sessions", "board"] });
    expect(field(empty.observed, "state", "task", "id")).toBe(newIssueId);
    expect(field(empty.observed, "state", "task", "title")).toBe(title);
    expect(field(empty.observed, "state", "task", "description")).toBe(description);
    expect(field(empty.observed, "state", "task", "workspace", "kind")).toBe("local");
    expect((field(empty.observed, "state", "tasks") as Task[]).filter(task => task.id === newIssueId).length).toBe(1);
    expect((field(empty.observed, "state", "sessions") as AgentSession[]).some(session => session.task_id === newIssueId)).toBe(false);
    expect(field(empty.observed, "state", "board", "saveError")).toBeNull();

    const details = await client.automationCall(sessionId, { type: "snapshot", sections: ["tree"] });
    const detailNodes = field(details.observed, "tree", "nodes") as { element_id?: string }[];
    expect(detailNodes.some(node => node.element_id === `issue-title-${newIssueId}`)).toBe(true);
    expect(detailNodes.some(node => node.element_id === `issue-description-${newIssueId}`)).toBe(true);
    // Menu triggers and entries must advertise real semantic Click actions.
    await client.automationCall(sessionId, { type: "act", action: "click", target: { id: "task-detail-workspace" } });
    await wait({ on: "node", id: "task-workspace-local", present: true });
    await client.automationCall(sessionId, { type: "act", action: "click", target: { id: "task-workspace-local" } });
    await wait({ on: "state", pointer: "/task/workspace/kind", equals: "local" });
    await client.automationCall(sessionId, { type: "act", action: "click", target: { id: "task-detail-workspace" } });
    await wait({ on: "node", id: "task-workspace-new", present: true });
    await client.automationCall(sessionId, { type: "act", action: "click", target: { id: "task-workspace-new" } });
    await wait({ on: "state", pointer: "/task/workspace/kind", equals: "newWorktree" });

    const drafted = await invoke("newSession", { taskId: newIssueId });
    const draftId = field(drafted.value, "opened") as string;
    await wait({ on: "state", pointer: "/session/id", equals: draftId });
    expect(field(drafted.observed, "state", "selection", "task_id")).toBe(newIssueId);
    expect(field(drafted.observed, "state", "selection", "workspace", "kind")).toBe("newWorktree");
    expect(field(drafted.observed, "state", "composer", "text")).toBe(`${title}\n\n${description}`);
    await wait({ on: "node", id: "composer-create-worktree", present: true });
    const checkbox = await client.automationCall(sessionId, { type: "snapshot", sections: ["tree"] });
    const worktreeNode = (field(checkbox.observed, "tree", "nodes") as Record<string, unknown>[]).find(node => node.element_id === "composer-create-worktree");
    expect(worktreeNode?.role).toBe("CheckBox");
    expect(worktreeNode?.toggled).toBe("True");
    const unchecked = await client.automationCall(sessionId, { type: "act", action: "click", target: { id: "composer-create-worktree" } });
    // Local is the omitted default in the persisted AgentSession contract.
    expect(field(unchecked.observed, "state", "selection", "workspace", "kind") ?? "local").toBe("local");
    await client.automationCall(sessionId, { type: "act", action: "click", target: { id: "composer-create-worktree" } });
    await wait({ on: "state", pointer: "/session/workspace/kind", equals: "newWorktree" });
    expect(field((await invoke("newSession", { taskId: newIssueId })).value, "opened")).toBe(draftId);
    const standalone = await invoke("newSession");
    expect(field(standalone.value, "opened")).not.toBe(draftId);
    expect(field(standalone.observed, "state", "selection", "task_id") ?? null).toBeNull();
    expect(field(standalone.observed, "state", "composer", "text")).not.toBe(title);
    await invoke("openSession", { id: draftId });
    await wait({ on: "state", pointer: "/composer/text", equals: `${title}\n\n${description}` });
  } finally {
    events?.close();
    if (sessionId) await client.automation({ type: "detach", sessionId }).catch(() => {});
    if (scratchId) await client.automation({ type: "scratchRemove", instanceId: scratchId }).catch(() => {});
    await client.disconnect();
  }
}, 90_000);

liveTest("native attachment picker filters eligible conversations and keeps keyboard focus and distant rows usable", async () => {
  const client = connect();
  let scratchId: string | undefined;
  let sessionId: string | undefined;
  let events: ReturnType<typeof notifications> | undefined;
  try {
    await client.connect();
    const projectId = crypto.randomUUID();
    const otherProjectId = crypto.randomUUID();
    const issueId = crypto.randomUUID();
    const now = Math.floor(Date.now() / 1000);
    const sessions: AgentSession[] = Array.from({ length: 1_000 }, (_, index) => ({
      id: crypto.randomUUID(), title: `${index === 100 || index === 101 ? "needle " : ""}Conversation ${index}`,
      project_id: projectId, provider: "pi", runtime_mode: "ask", status: "idle",
      created_at: now, updated_at: now + index, provider_cursor: null, messages: [], transcript_blocks: [], turns: [],
    }));
    const otherProjectSession = { ...sessions[0]!, id: crypto.randomUUID(), project_id: otherProjectId, title: "needle wrong project" };
    const filedSession = { ...sessions[0]!, id: crypto.randomUUID(), task_id: issueId, title: "needle already filed" };
    const created = await client.automation({ type: "scratchCreate", headless: false, name: "Attachment picker keyboard acceptance", seed: {
      projects: [
        { id: projectId, name: "Attachment project", path: "/synthetic/attachment-project", created_at: now },
        { id: otherProjectId, name: "Other project", path: "/synthetic/other-project", created_at: now },
      ],
      tasks: [{ id: issueId, title: "Attach the selected conversation", description: "Provider-free fixture", state: "backlog", position: 0,
        workspace: { kind: "local", projectId }, created_at: now, updated_at: now }],
      sessions: [...sessions, otherProjectSession, filedSession],
    } }, { timeoutMs: 40_000 });
    if (created.type !== "scratchCreated") throw new Error("unexpected scratch response");
    scratchId = created.instance.instanceId;
    sessionId = (await client.attachAutomation(scratchId)).sessionId;
    events = notifications(client, sessionId);
    const wait = async (condition: AutomationCondition) => {
      const response = await client.automation({ type: "wait", sessionId: sessionId!, condition, timeoutMs: 10_000 });
      if (response.type !== "wait") throw new Error("unexpected wait response");
      const resolved = response.wait.status === "pending"
        ? await events!.find(message => message.type === "waitResolved" && message.wait.waitId === response.wait.waitId)
        : undefined;
      expect(resolved?.type === "waitResolved" ? resolved.wait.status : response.wait.status).toBe("succeeded");
    };
    const call = (command: Parameters<WakuClient["automationCall"]>[1]) => client.automationCall(sessionId!, command);
    const nodes = async () => field((await call({ type: "snapshot", sections: ["tree"] })).observed, "tree", "nodes") as { element_id?: string }[];
    await call({ type: "invoke", verb: "openTask", args: { id: issueId } });
    await call({ type: "act", action: "click", target: { id: "task-detail-attach" } });
    await wait({ on: "node", id: "task-attach-search", present: true });
    expect((await nodes()).filter(node => node.element_id?.startsWith("task-attach-")).length).toBeLessThan(30);
    await call({ type: "key", chord: "end" });
    await wait({ on: "node", id: `task-attach-${sessions[0]!.id}`, present: true });
    await call({ type: "key", chord: "home" });
    await wait({ on: "node", id: `task-attach-${sessions[999]!.id}`, present: true });
    // No explicit target: opening must have placed real focus in the search field.
    await call({ type: "type", text: "needle attachment" });
    await wait({ on: "node", id: `task-attach-${sessions[101]!.id}`, present: true });
    const filtered = await nodes();
    expect(filtered.some(node => node.element_id === `task-attach-${sessions[100]!.id}`)).toBe(true);
    expect(filtered.some(node => node.element_id === `task-attach-${otherProjectSession.id}`)).toBe(false);
    expect(filtered.some(node => node.element_id === `task-attach-${filedSession.id}`)).toBe(false);
    expect(filtered.filter(node => /^task-attach-[0-9a-f]{8}-/.test(node.element_id ?? "")).length).toBe(2);
    await call({ type: "key", chord: "down" });
    await call({ type: "key", chord: "enter" });
    await wait({ on: "node", id: "task-attach-search", present: false });
    await wait({ on: "state", pointer: "/board/savePending", equals: false });
    const catalog = field((await call({ type: "snapshot", sections: ["sessions"] })).observed, "state", "sessions") as AgentSession[];
    expect(catalog.find(session => session.id === sessions[100]!.id)?.task_id).toBe(issueId);
    expect(catalog.find(session => session.id === sessions[101]!.id)?.task_id ?? null).toBeNull();
    expect(catalog.find(session => session.id === otherProjectSession.id)?.task_id ?? null).toBeNull();
    await call({ type: "act", action: "click", target: { id: "task-detail-attach" } });
    await wait({ on: "node", id: "task-attach-search", present: true });
    await call({ type: "key", chord: "escape" });
    await wait({ on: "node", id: "task-attach-search", present: false });
    // Escape must restore trigger focus; Enter can open the same picker again.
    await call({ type: "key", chord: "enter" });
    await wait({ on: "node", id: "task-attach-search", present: true });
    await call({ type: "key", chord: "escape" });
  } finally {
    events?.close();
    if (sessionId) await client.automation({ type: "detach", sessionId }).catch(() => {});
    if (scratchId) await client.automation({ type: "scratchRemove", instanceId: scratchId }).catch(() => {});
    await client.disconnect();
  }
}, 90_000);
