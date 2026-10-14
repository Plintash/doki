// Opt-in native acceptance against scripts/project-dedup-fixture.py only.
// No prompt is submitted and no developer database is read or edited.
import { expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { WakuClient, type AutomationMessage } from "../packages/waku-client/src/client";
import type { AgentSession, AutomationCondition, Task } from "../packages/waku-client/src/generated";

const liveTest = process.env.WAKU_RPA_PROJECT_DEDUP_TEST === "1" ? test : test.skip;
function field(value: unknown, ...keys: string[]): unknown {
  for (const key of keys) {
    if (value === null || typeof value !== "object") return undefined;
    value = (value as Record<string, unknown>)[key];
  }
  return value;
}

type Node = { element_id?: string; selected?: boolean; checked?: boolean };
liveTest("project choices collapse old exact paths while preserving all issue and conversation bindings", async () => {
  const { WAKU_DAEMON_ADDRESS: address, WAKU_DAEMON_TOKEN: token } = process.env;
  if (!address || !token) throw new Error("requires the isolated native fixture daemon");
  const client = new WakuClient({ address, token, requestTimeoutMs: 40_000 });
  let sessionId: string | undefined;
  let database: Database | undefined;
  let unsubscribe: (() => void) | undefined;
  const events: AutomationMessage[] = [];
  const listeners = new Set<() => void>();
  try {
    await client.connect();
    const checkout = process.env.WAKU_RPA_TEST_CHECKOUT;
    if (!checkout) throw new Error("requires the isolated fixture checkout");
    const fixture = JSON.parse(readFileSync(join(checkout, "temp/issue-board-qa/project-dedup.json"), "utf8"));
    expect(fixture.checkout).toBe(checkout);
    const instances = (await client.automationInstances()).filter(instance => instance.executorConnected && instance.checkout === checkout && instance.database === fixture.database);
    expect(instances.length).toBe(1);
    database = new Database(fixture.database, { readonly: true });
    const storedBindings = () => ({
      projects: database!.query("SELECT id,name,path FROM projects ORDER BY id").all(),
      issues: database!.query("SELECT id,project_id,path,branch,worktree FROM tasks ORDER BY id").all(),
      sessions: database!.query("SELECT id,project_id,task_id,workspace FROM sessions ORDER BY id").all(),
    });
    const bindingsBefore = storedBindings();
    sessionId = (await client.attachAutomation(instances[0]!.instanceId)).sessionId;
    const projects = fixture.projects as { id: string; name: string; path: string; created_at: number }[];
    const issues = fixture.tasks as Task[];
    const conversation = fixture.conversation as AgentSession;
    const original = projects[0]!.id;
    const selected = projects[1]!.id;
    unsubscribe = client.subscribeAutomation(sessionId, event => {
      events.push(event);
      for (const listener of listeners) listener();
    });
    const wait = async (condition: AutomationCondition) => {
      const result = await client.automation({ type: "wait", sessionId: sessionId!, condition, timeoutMs: 10_000 });
      if (result.type !== "wait") throw new Error("unexpected wait response");
      if (result.wait.status !== "pending") { expect(result.wait.status).toBe("succeeded"); return; }
      await new Promise<void>((resolve, reject) => {
        const finish = () => {
          const event = events.find(event => event.type === "waitResolved" && event.wait.waitId === result.wait.waitId);
          if (event?.type === "waitResolved") {
            clearTimeout(timer); listeners.delete(finish);
            if (event.wait.status === "succeeded") resolve(); else reject(new Error(JSON.stringify(event.wait)));
          }
        };
        const timer = setTimeout(() => { listeners.delete(finish); reject(new Error("native wait deadline")); }, 15_000);
        listeners.add(finish); finish();
      });
    };
    const call = (call: Parameters<WakuClient["automationCall"]>[1]) => client.automationCall(sessionId!, call);
    const click = (id: string) => call({ type: "act", action: "click", target: { id } });
    const invoke = (verb: string, args: Record<string, string> = {}) => call({ type: "invoke", verb, args });
    const nodes = async () => field(await call({ type: "snapshot", sections: ["tree"] }), "observed", "tree", "nodes") as Node[];
    const checkChoices = async (prefix: string, chosen: string, hidden: string) => {
      const choices = (await nodes()).filter(node => projects.some(project => node.element_id === `${prefix}${project.id}`));
      expect(choices.map(node => node.element_id).sort()).toEqual([chosen, projects[2]!.id, projects[3]!.id].map(id => `${prefix}${id}`).sort());
      expect(choices.some(node => node.element_id === `${prefix}${hidden}`)).toBe(false);
    };

    await invoke("openSession", { id: conversation.id });
    // Started conversations keep their project locked. Their durable bindings
    // are checked below; both editable pickers belong to a real new draft.
    // Unstarted standalone sessions are transient, so do not seed one in SQL.
    await invoke("newSession");
    await wait({ on: "node", id: "workspace-project", present: true });
    const draftBefore = field(await call({ type: "snapshot", sections: ["session"] }), "observed", "state", "session") as AgentSession;
    expect(draftBefore.project_id).toBe(selected);
    await call({ type: "act", action: "focus", target: { id: "composer-field" } });
    await call({ type: "key", chord: "ctrl-a" });
    await call({ type: "type", text: "Keep this unsent composer text", target: { id: "composer-field" } });
    await click("workspace-project");
    await checkChoices("composer-project-", selected, original);
    await click(`composer-project-${selected}`);
    await wait({ on: "state", pointer: "/route/selectedSession", equals: draftBefore.id });
    await wait({ on: "state", pointer: "/composer/text", equals: "Keep this unsent composer text" });
    // The empty-state project selector above the composer uses the same view.
    await wait({ on: "node", id: "empty-state-project", present: true });
    await click("empty-state-project");
    await checkChoices("empty-state-project-", selected, original);
    await click(`empty-state-project-${selected}`);
    await wait({ on: "state", pointer: "/route/selectedSession", equals: draftBefore.id });
    await wait({ on: "state", pointer: "/composer/text", equals: "Keep this unsent composer text" });
    const draftAfter = field(await call({ type: "snapshot", sections: ["session"] }), "observed", "state", "session") as AgentSession;
    expect(draftAfter.workspace).toEqual(draftBefore.workspace);
    expect(draftAfter.project_id).toBe(draftBefore.project_id);

    await invoke("openTask", { id: issues[1]!.id });
    await click("task-detail-project");
    await checkChoices("task-project-", selected, original);
    await click(`task-project-${selected}`);
    const issue = field(await call({ type: "snapshot", sections: ["task"] }), "observed", "state", "task");
    expect(field(issue, "workspace")).toEqual(issues[1]!.workspace);
    await click("board-detail-back");
    await click("board-project-filter");
    const filterChoices = (await nodes()).filter(node => projects.some(project => node.element_id === `board-project-${project.id}`));
    expect(filterChoices.map(node => node.element_id).sort()).toEqual([original, projects[2]!.id, projects[3]!.id].map(id => `board-project-${id}`).sort());
    await click(`board-project-${original}`);
    const boardNodes = await nodes();
    expect(boardNodes.some(node => node.element_id === `task-card-${issues[0]!.id}`)).toBe(true);
    expect(boardNodes.some(node => node.element_id === `task-card-${issues[1]!.id}`)).toBe(true);
    expect(boardNodes.some(node => node.element_id === `task-card-${issues[2]!.id}`)).toBe(false);

    const after = await call({ type: "snapshot", sections: ["projects", "tasks", "sessions"] });
    expect(field(after, "observed", "state", "projects")).toEqual(projects);
    expect(field(after, "observed", "state", "tasks")).toEqual(issues);
    const retained = (field(after, "observed", "state", "sessions") as AgentSession[]).find(session => session.id === conversation.id);
    expect(retained?.project_id).toBe(selected);
    expect(retained?.workspace).toEqual(conversation.workspace);
    expect(retained?.task_id ?? null).toBeNull();
    await wait({ on: "state", pointer: "/board/savePending", equals: false });
    expect(storedBindings()).toEqual(bindingsBefore);
  } finally {
    database?.close();
    unsubscribe?.();
    if (sessionId) await client.automation({ type: "detach", sessionId }).catch(() => {});
    await client.disconnect();
  }
}, 120_000);
