// Opt-in provider-free native acceptance. This attaches to the already-running
// scripts/issue-review-fixture.py app; it never starts/reseeds an app or a model.
// Run only after that fixture is ready, with its existing daemon connection:
// WAKU_RPA_ISSUE_REVIEW_TEST=1 WAKU_RPA_TEST_CHECKOUT="$PWD" \
//   bun test scripts/rpa-issue-review-live.test.ts
// WAKU_DAEMON_ADDRESS/TOKEN must already be supplied by the fixture owner.
// Synthetic review/fix conversations are retained as acceptance evidence.
import { expect, test } from "bun:test";
import {
  closeSync, existsSync, lstatSync, openSync, readFileSync, readSync,
  realpathSync, statSync, unlinkSync, writeFileSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { WakuClient, type AutomationMessage } from "../packages/waku-client/src/client";
import type {
  AgentSession, AutomationCondition, DaemonSettings,
  IssueReviewTarget, Task,
} from "../packages/waku-client/src/generated";

const nativeTest = process.env.WAKU_RPA_ISSUE_REVIEW_TEST === "1" ? test : test.skip;
const providers = ["amp", "claude", "codex", "cursor", "deepSeek", "fx", "openCode", "grok", "kimi", "ohMyPi", "pi"] as const;
const fixtureOutput = "[P1] Preserve the issue worktree when returning to Pi\n\n"
  + "src/workflow.rs:42 — A new correction session must inherit the exact reviewed path, "
  + "rather than creating another worktree. Keep the issue and final findings attached.\n\n"
  + "[P2] Keep an existing implementation draft intact\n\n"
  + "src/workflow.rs:67 — Prepare a separate correction draft so unsent user notes are not replaced.\n\n"
  + "Provider-free QA fixture output. This is synthetic evidence, not an authenticated model review.";
const at = (value: unknown, ...keys: string[]): unknown => keys.reduce((value, key) =>
  value !== null && typeof value === "object" ? (value as Record<string, unknown>)[key] : undefined, value);
const json = (path: string): Record<string, unknown> => JSON.parse(readFileSync(path, "utf8"));
const quote = (value: string) => /^[A-Za-z0-9_@%+=:,./-]+$/.test(value) ? value : `'${value.replaceAll("'", `'"'"'`)}'`;

function plainPath(path: string): void {
  if (lstatSync(path).isSymbolicLink()) throw new Error(`fixture guard refuses a symlink: ${path}`);
  if (realpathSync(path) !== resolve(path)) throw new Error(`fixture path escapes its checkout: ${path}`);
}

function settingsGuard(settings: DaemonSettings, wrapper: string, blocker: string): void {
  if (settings.computer_use_enabled) throw new Error("fixture computer use must remain disabled");
  const disabled = new Set(settings.disabled_providers);
  if (disabled.has("codex") || providers.some(provider => provider !== "codex" && !disabled.has(provider))) {
    throw new Error("fixture must enable only its synthetic Codex provider");
  }
  for (const provider of providers) {
    if (settings.provider_binary_overrides[provider] !== (provider === "codex" ? wrapper : blocker)) {
      throw new Error(`fixture provider override differs for ${provider}`);
    }
  }
}

// Local identity and executable validation is repeated before every submit.
// The wrapper must exactly invoke the checked-in Python fixture's --provider
// branch. Matching a filename or merely finding the opt-in variable is not enough.
function fixtureGuard(root: string, address: string) {
  const temp = join(root, "temp");
  const qa = join(temp, "issue-board-qa");
  const database = join(temp, "app.db");
  const wrapper = join(qa, "codex-review-fixture");
  const blocker = join(qa, "provider-cli-not-installed");
  const settingsPath = join(qa, "home/.waku/settings.json");
  for (const path of [root, temp, qa, database, wrapper, settingsPath, join(temp, ".issue-board-qa-fixture"), join(qa, "review-seeded"), join(qa, "manifest.json"), join(qa, "running.json")]) plainPath(path);
  if (readFileSync(join(temp, ".issue-board-qa-fixture"), "utf8") !== `doki-issue-board-fixture-v1\n${root}\n`) {
    throw new Error("fixture marker does not name this exact checkout");
  }
  if (readFileSync(join(qa, "review-seeded"), "utf8") !== "Synthetic Board data seeded once\n") {
    throw new Error("issue-review fixture has not been explicitly seeded");
  }
  const script = join(root, "scripts/issue-review-fixture.py");
  plainPath(script);
  const expectedWrapper = `#!/bin/sh\nexec /usr/bin/python3 ${quote(script)} --provider ${quote(qa)} "$@"\n`;
  if (readFileSync(wrapper, "utf8") !== expectedWrapper || !(statSync(wrapper).mode & 0o100)) {
    throw new Error("Codex override is not the exact executable provider-free wrapper");
  }
  if (existsSync(blocker)) throw new Error("disabled-provider blocker must not exist");
  const running = json(join(qa, "running.json"));
  if (running.address !== address || running.database !== database) throw new Error("daemon connection is not this running fixture");
  const manifest = json(join(qa, "manifest.json"));
  if (manifest.database !== database) throw new Error("seed manifest names a different database");
  settingsGuard(json(settingsPath) as DaemonSettings, wrapper, blocker);
  const worktree = join(qa, "projects/orion-issue-navigation");
  plainPath(worktree);
  return { qa, database, wrapper, blocker, worktree, manifest };
}

function pushedWaits(client: WakuClient, sessionId: string) {
  const received: AutomationMessage[] = [];
  const listeners = new Set<() => void>();
  const close = client.subscribeAutomation(sessionId, message => {
    received.push(message);
    for (const listener of listeners) listener();
  });
  return {
    close,
    async wait(condition: AutomationCondition, timeoutMs = 15_000): Promise<void> {
      const result = await client.automation({ type: "wait", sessionId, condition, timeoutMs });
      if (result.type !== "wait") throw new Error("unexpected native wait response");
      if (result.wait.status !== "pending") {
        expect(result.wait.status, JSON.stringify(result.wait)).toBe("succeeded");
        return;
      }
      await new Promise<void>((resolve, reject) => {
        const check = () => {
          const found = received.find(message => message.type === "waitResolved" && message.wait.waitId === result.wait.waitId);
          if (found?.type !== "waitResolved") return;
          clearTimeout(timer); listeners.delete(check);
          if (found.wait.status === "succeeded") resolve();
          else reject(new Error(JSON.stringify(found.wait)));
        };
        const timer = setTimeout(() => { listeners.delete(check); reject(new Error("native review wait notification deadline")); }, timeoutMs + 5_000);
        listeners.add(check); check();
      });
    },
  };
}

function requestTail(path: string, offset: number): Array<{ cwd: string; request: Record<string, unknown> }> {
  plainPath(path);
  const size = statSync(path).size;
  if (size < offset || size - offset > 1_000_000) throw new Error("fixture request-log tail is invalid or unexpectedly large");
  const buffer = Buffer.alloc(size - offset);
  const fd = openSync(path, "r");
  try {
    let count = 0;
    while (count < buffer.length) {
      const read = readSync(fd, buffer, count, buffer.length - count, offset + count);
      if (!read) throw new Error("fixture request-log read ended early");
      count += read;
    }
  } finally { closeSync(fd); }
  return buffer.toString("utf8").split("\n").filter(Boolean).map(line => JSON.parse(line));
}

nativeTest("native issue review preserves issue scope, findings, and Pi drafts with synthetic success/failure/cancel", async () => {
  const root = realpathSync(resolve(dirname(fileURLToPath(import.meta.url)), ".."));
  if (!process.env.WAKU_RPA_TEST_CHECKOUT || realpathSync(process.env.WAKU_RPA_TEST_CHECKOUT) !== root) {
    throw new Error("set WAKU_RPA_TEST_CHECKOUT to this exact marked fixture checkout");
  }
  const address = process.env.WAKU_DAEMON_ADDRESS;
  const token = process.env.WAKU_DAEMON_TOKEN;
  if (!address || !token) throw new Error("use the already-running fixture's daemon connection");
  const fixture = fixtureGuard(root, address);
  const lockPath = join(fixture.qa, "issue-review-live-test.lock");
  const modePath = join(fixture.qa, "review-mode");
  const previousMode = existsSync(modePath) ? (plainPath(modePath), readFileSync(modePath, "utf8")) : undefined;
  let ownedMode: string | undefined;
  const logPath = join(fixture.qa, "review-requests.jsonl");
  if (existsSync(logPath)) plainPath(logPath);
  const logOffset = existsSync(logPath) ? statSync(logPath).size : 0;
  const client = new WakuClient({ address, token, requestTimeoutMs: 40_000 });
  let attachment: string | undefined;
  let waits: ReturnType<typeof pushedWaits> | undefined;
  let activeReview: string | undefined;
  const lock = openSync(lockPath, "wx", 0o600);
  try {
    await client.connect();
    const instances = (await client.automationInstances()).filter(instance =>
      instance.executorConnected && !instance.scratch && !instance.headless &&
      instance.checkout === root && instance.database === fixture.database &&
      (!process.env.WAKU_RPA_TEST_INSTANCE || instance.instanceId === process.env.WAKU_RPA_TEST_INSTANCE));
    expect(instances).toHaveLength(1);
    const instance = instances[0]!;
    expect(realpathSync(instance.checkout)).toBe(root);
    expect(instance.build.length).toBeGreaterThan(0);
    attachment = (await client.attachAutomation(instance.instanceId)).sessionId;
    waits = pushedWaits(client, attachment);
    await client.automation({ type: "subscribe", sessionId: attachment, streams: ["ui", "model"] });
    const snapshot = async (sections: string[]) => {
      const result = await client.automationCall(attachment!, { type: "snapshot", sections });
      return result.value as { state: Record<string, unknown>; tree?: unknown };
    };
    const invoke = (verb: string, args: Record<string, string> = {}) => client.automationCall(attachment!, { type: "invoke", verb, args });
    const click = (id: string) => client.automationCall(attachment!, { type: "act", action: "click", target: { id } });
    const replaceText = async (id: string, text: string) => {
      await client.automationCall(attachment!, { type: "act", action: "focus", target: { id } });
      await client.automationCall(attachment!, { type: "key", chord: process.platform === "darwin" ? "cmd-a" : "ctrl-a" });
      await client.automationCall(attachment!, { type: "type", text, target: { id } });
    };
    const appendComposer = async (edit: string): Promise<string> => {
      // RPA Type turns a newline into Enter. These user edits are deliberately
      // one line: editing the disabled Pi composer must never submit it.
      expect(edit).not.toMatch(/[\r\n]/);
      const before = at((await snapshot(["composer"])).state, "composer", "text") as string;
      await client.automationCall(attachment!, { type: "act", action: "focus", target: { id: "composer-field" } });
      await client.automationCall(attachment!, { type: "key", chord: process.platform === "darwin" ? "cmd-a right" : "ctrl-a right" });
      await client.automationCall(attachment!, { type: "type", text: edit, target: { id: "composer-field" } });
      const expected = before + edit;
      await waits!.wait({ on: "state", pointer: "/composer/text", equals: expected });
      expect(at((await snapshot(["composer"])).state, "composer", "text")).toBe(expected);
      return expected;
    };
    const selected = async () => (await snapshot(["session"])).state.session as AgentSession;
    const catalog = async () => (await snapshot(["sessions"])).state.sessions as AgentSession[];
    const open = async (id: string) => {
      await invoke("openSession", { id });
      await waits!.wait({ on: "state", pointer: "/selection/id", equals: id });
      await waits!.wait({ on: "state", pointer: "/selection/detailLoaded", equals: true });
    };
    const assertSafety = async (reviewId?: string) => {
      const current = fixtureGuard(root, address);
      const response = await client.request({ type: "getSettings" });
      if (response.type !== "settings") throw new Error("unexpected daemon settings response");
      settingsGuard(response.settings, current.wrapper, current.blocker);
      const alive = (await client.automationInstances()).find(candidate => candidate.instanceId === instance.instanceId);
      if (!alive?.executorConnected || alive.build !== instance.build || alive.database !== current.database || realpathSync(alive.checkout) !== root) {
        throw new Error("fixture executor/build changed; rerun against its new verified identity");
      }
      if (reviewId) {
        const state = (await snapshot(["session", "workspace", "providers"])).state;
        const session = state.session as AgentSession;
        expect(session.id).toBe(reviewId);
        expect(session.provider).toBe("codex");
        expect(session.review?.status).toBe("draft");
        expect(session.model).toBe("fixture-review");
        expect(session.status).toBe("idle");
        expect(session.runtime_mode).toBe("ask");
        expect(session.workspace).toEqual({ kind: "worktree", path: current.worktree, branch: "feature/issue-navigation" });
        expect(at(state, "workspace", "path")).toBe(current.worktree);
        expect(session.provider_cursor == null || session.provider_cursor.provider === "codex").toBe(true);
        const items = at(state, "providers", "items") as Array<{ provider: string; enabled: boolean; path: string | null }>;
        expect(items.filter(item => item.enabled).map(item => item.provider)).toEqual(["codex"]);
        expect(items.find(item => item.provider === "codex")?.path).toBe(current.wrapper);
      }
    };
    await assertSafety();
    const issueId = at(fixture.manifest, "issues", "Keep issue context across conversations") as string;
    const sourceId = at(fixture.manifest, "conversations", "Review workspace inheritance") as string;
    const oldDraftId = at(fixture.manifest, "conversations", "Unsent implementation follow-up") as string;
    for (const id of [issueId, sourceId, oldDraftId]) expect(id).toMatch(/^9e5e/);
    await open(oldDraftId);
    const oldDraft = (await snapshot(["composer"])).state.composer as { text: string };
    expect(oldDraft.text).toContain("This draft has never been sent.");
    expect((await selected()).provider).toBe("pi");
    await open(sourceId);
    const source = await selected();
    expect(source.provider).toBe("pi");
    expect(source.task_id).toBe(issueId);
    expect(source.workspace).toEqual({ kind: "worktree", path: fixture.worktree, branch: "feature/issue-navigation" });
    const sourceReply = [...source.messages].reverse().find(message => message.role === "assistant")!;
    expect(sourceReply.content).toContain("static, idle QA fixture");
    await invoke("openTask", { id: issueId });
    const issue = (await snapshot(["task"])).state.task as Task;
    expect(issue.id).toBe(issueId);
    expect(issue.state).toBe("doing");
    expect(issue.description).toContain("Acceptance criteria:");

    const prepare = async (scope: "Uncommitted" | "BaseBranch" | "Commit" | "CommitRange", reference: string, target: IssueReviewTarget) => {
      await invoke("openTask", { id: issueId });
      await waits!.wait({ on: "node", id: "issue-start-review", present: true });
      await click("issue-review-scope");
      await click(`issue-review-scope-${scope}`);
      if (reference) await replaceText("issue-review-reference", reference);
      await click("issue-start-review");
      await waits!.wait({ on: "state", pointer: "/route/view", equals: "session" });
      await waits!.wait({ on: "state", pointer: "/selection/detailLoaded", equals: true });
      const review = await selected();
      expect(review.review?.status).toBe("draft");
      expect(review.review?.target).toEqual(target);
      expect(review.review?.source_session_id).toBe(sourceId);
      expect(review.task_id).toBe(issueId);
      expect(review.provider).toBe("codex");
      expect(review.provider_cursor).toBeNull();
      expect(review.messages).toHaveLength(0);
      expect(review.workspace).toEqual(source.workspace);
      const text = at((await snapshot(["composer"])).state, "composer", "text") as string;
      expect(text).toContain(issue.title);
      expect(text).toContain(issue.description);
      expect(text).toContain(sourceReply.display_content ?? sourceReply.content);
      return { id: review.id, text };
    };
    const send = async (reviewId: string, mode: "success" | "hold" | "refusal" | "empty") => {
      await waits!.wait({ on: "state", pointer: "/composer/providerAvailable", equals: true });
      // Enabled/installed can arrive before asynchronous model discovery.
      // ProviderKind::ALL fixes item order; this fixture returns one model.
      // Observe the index once, then let a pushed wait track its exact ID.
      const providers = at((await snapshot(["providers"])).state, "providers", "items") as Array<{ provider: string }>;
      const codexIndex = providers.findIndex(provider => provider.provider === "codex");
      expect(codexIndex).toBeGreaterThanOrEqual(0);
      await waits!.wait({ on: "state", pointer: `/providers/items/${codexIndex}/models/0/id`, equals: "fixture-review" });
      await invoke("chooseModel", { provider: "codex", model: "fixture-review" });
      await assertSafety(reviewId);
      if (existsSync(modePath)) plainPath(modePath);
      writeFileSync(modePath, mode + "\n");
      ownedMode = mode + "\n";
      // Recheck current daemon settings and executable immediately before the
      // only submission entry point in this file. Never submit a Pi draft.
      await assertSafety(reviewId);
      activeReview = reviewId;
      await invoke("submitPrompt");
    };

    const primary = await prepare("BaseBranch", "main", { kind: "baseBranch", branch: "main" });
    const reviewEdit = ` QA user review amendment: verify unsent notes survive reopening. ${crypto.randomUUID()}`;
    const reviewText = await appendComposer(reviewEdit);
    expect(reviewText).not.toBe(primary.text);
    const draftCount = (await catalog()).filter(session => session.task_id === issueId && session.review).length;
    const repeated = await prepare("BaseBranch", "main", { kind: "baseBranch", branch: "main" });
    expect(repeated).toEqual({ id: primary.id, text: reviewText });
    expect(repeated.text).toContain(reviewEdit);
    expect((await catalog()).filter(session => session.task_id === issueId && session.review)).toHaveLength(draftCount);
    await send(primary.id, "success");
    await waits.wait({ on: "state", pointer: "/selection/review/status", equals: "completed" });
    await waits.wait({ on: "state", pointer: "/selection/status", equals: "idle" });
    activeReview = undefined;
    const complete = await selected();
    const result = complete.messages.find(message => message.id === complete.review?.result_message_id)!;
    expect(result?.role).toBe("assistant");
    expect(result?.content).toBe(fixtureOutput);
    expect(complete.messages.filter(message => message.content === fixtureOutput)).toHaveLength(1);
    expect(complete.review?.summary?.length).toBeGreaterThan(0);
    expect(Array.from(complete.review!.summary!).length).toBeLessThanOrEqual(400);
    expect(complete.provider_cursor?.provider).toBe("codex");
    expect(complete.messages.find(message => message.role === "user")?.content).toBe(reviewText);

    await invoke("openTask", { id: issueId });
    await waits.wait({ on: "node", id: "issue-fix-review", present: true });
    await waits.wait({ on: "node", id: "issue-open-review", present: true });
    const detail = await snapshot(["task", "sessions"]);
    expect((detail.state.task as Task).state).toBe(issue.state);
    expect((detail.state.sessions as AgentSession[]).find(session => session.id === primary.id)?.review).toEqual(complete.review);
    // GPUI's semantic tree exposes actionable controls, not every rendered
    // status/summary text span. Pixel checks are separate; here prove the
    // promoted metadata and the actual Open review / Board / Fix routes.
    await click("issue-open-review");
    await waits.wait({ on: "state", pointer: "/selection/id", equals: primary.id });
    await waits.wait({ on: "state", pointer: "/route/view", equals: "session" });
    await invoke("openTask", { id: issueId });
    await click("board-detail-back");
    await waits.wait({ on: "node", id: `task-card-${issueId}`, present: true });
    const board = await snapshot(["route", "sessions"]);
    expect(at(board.state, "route", "view")).toBe("board");
    const cardReview = (board.state.sessions as AgentSession[]).find(session => session.id === primary.id);
    expect(cardReview?.task_id).toBe(issueId);
    expect(cardReview?.review).toEqual(complete.review);
    expect(cardReview?.review?.status).toBe("completed");
    expect(cardReview?.review?.summary).toContain("Preserve the issue worktree");

    await click(`task-card-${issueId}`);
    await waits.wait({ on: "state", pointer: "/route/view", equals: "issue" });
    await waits.wait({ on: "state", pointer: "/task/id", equals: issueId });
    await waits.wait({ on: "node", id: "issue-fix-review", present: true });
    await click("issue-fix-review");
    await waits.wait({ on: "state", pointer: "/route/view", equals: "session" });
    const fix = await selected();
    expect(fix.id).not.toBe(oldDraftId);
    expect(fix.id).not.toBe(sourceId);
    expect(fix.provider).toBe("pi");
    expect(fix.provider_cursor).toBeNull();
    expect(fix.task_id).toBe(issueId);
    expect(fix.review_correction_source_id).toBe(primary.id);
    expect(fix.workspace).toEqual(complete.workspace);
    expect(fix.messages).toHaveLength(0);
    const fixText = at((await snapshot(["composer"])).state, "composer", "text") as string;
    expect(fixText).toContain(issue.title);
    expect(fixText).toContain(issue.description);
    expect(fixText).toContain(fixtureOutput);
    const fixEdit = ` QA user correction amendment: preserve these implementation notes. ${crypto.randomUUID()}`;
    const editedFixText = await appendComposer(fixEdit);
    expect(editedFixText).not.toBe(fixText);
    const fixCount = (await catalog()).length;
    await invoke("openTask", { id: issueId });
    await click("issue-fix-review");
    await waits.wait({ on: "state", pointer: "/selection/id", equals: fix.id });
    const reopenedFixText = at((await snapshot(["composer"])).state, "composer", "text");
    expect(reopenedFixText).toBe(editedFixText);
    expect(reopenedFixText).toContain(fixEdit);
    expect((await catalog()).length).toBe(fixCount);
    expect((await catalog()).find(session => session.id === primary.id)?.review?.fix_session_id).toBe(fix.id);
    await open(oldDraftId);
    expect(at((await snapshot(["composer"])).state, "composer", "text")).toBe(oldDraft.text);

    const outcomes: Record<string, string> = { success: primary.id };
    for (const scenario of [
      { mode: "hold", scope: "Commit", reference: "HEAD", target: { kind: "commit", sha: "HEAD" }, status: "interrupted" },
      { mode: "refusal", scope: "CommitRange", reference: "main..HEAD", target: { kind: "commitRange", base: "main", head: "HEAD" }, status: "failed" },
      { mode: "empty", scope: "Uncommitted", reference: "", target: { kind: "uncommittedChanges" }, status: "failed" },
    ] as const) {
      const review = await prepare(scenario.scope, scenario.reference, scenario.target);
      await send(review.id, scenario.mode);
      if (scenario.mode === "hold") {
        await waits.wait({ on: "state", pointer: "/selection/review/status", equals: "running" });
        await waits.wait({ on: "state", pointer: "/selection/status", equals: "working" });
        // Running metadata is optimistic at submission. Wait for the actual
        // provider turn before Stop so this exercises native turn/interrupt.
        await waits.wait({ on: "state", pointer: "/session/turns/0/provider_turn_started", equals: true });
        await invoke("cancelPrompt");
      }
      await waits.wait({ on: "state", pointer: "/selection/review/status", equals: scenario.status });
      await waits.wait({ on: "state", pointer: "/selection/activeTurnId", equals: null });
      activeReview = undefined;
      const settled = await selected();
      expect(settled.review?.result_message_id ?? null).toBeNull();
      expect(settled.review?.fix_session_id ?? null).toBeNull();
      expect(settled.messages.some(message => message.content === fixtureOutput)).toBe(false);
      outcomes[scenario.mode] = review.id;
    }
    await open(oldDraftId);
    expect(at((await snapshot(["composer"])).state, "composer", "text")).toBe(oldDraft.text);
    const requests = requestTail(logPath, logOffset);
    const reviews = requests.filter(entry => entry.request.method === "review/start");
    expect(reviews).toHaveLength(4);
    expect(requests.filter(entry => entry.request.method === "turn/interrupt")).toHaveLength(1);
    expect(requests.some(entry => entry.request.method === "turn/start")).toBe(false);
    for (const entry of requests.filter(entry => entry.request.method === "thread/start" || entry.request.method === "thread/resume")) {
      expect(at(entry.request, "params", "model")).toBe("fixture-review");
      expect(at(entry.request, "params", "config", "review_model")).toBe("fixture-review");
    }
    // These are asserted per native request in submission order. Exact
    // authoritative scope lines prove the selected references reach Codex,
    // and distinguish parent-commit and two-endpoint comparisons from merge base.
    const expectedScopes = [
      { name: "BaseBranch(main)", line: 'Review this branch\'s changes against the merge base with the named base branch "main". Resolve the branch locally; do not fetch or switch branches.' },
      { name: "Commit(HEAD)", line: 'Review exactly the commit identified by "HEAD", comparing it with its parent. Do not include unrelated working-tree changes.' },
      { name: "CommitRange(main..HEAD)", line: 'Review the exact two-endpoint diff from "main" to "HEAD" (git diff BASE..HEAD, not merge-base comparison). Do not include unrelated working-tree changes.' },
      { name: "Uncommitted", line: "Review all current uncommitted changes in this workspace: staged, unstaged, and untracked files." },
    ];
    for (const [index, entry] of reviews.entries()) {
      const scope = expectedScopes[index]!;
      expect(entry.cwd).toBe(fixture.worktree);
      expect(at(entry.request, "params", "delivery")).toBe("inline");
      expect(at(entry.request, "params", "target", "type")).toBe("custom");
      const instructions = at(entry.request, "params", "target", "instructions") as string;
      expect(typeof instructions, scope.name).toBe("string");
      expect(instructions.split("\n")[0], scope.name).toBe("Review scope (authoritative):");
      expect(instructions.split("\n")[1], scope.name).toBe(scope.line);
      expect(instructions, scope.name).toContain("If a reference cannot be resolved, report that blocker rather than choosing another scope.");
      expect(instructions, scope.name).toContain(issue.description);
    }
    expect(at(reviews[0]!.request, "params", "target", "instructions")).toContain(reviewEdit);
    expect((at(reviews[0]!.request, "params", "target", "instructions") as string).endsWith(reviewText)).toBe(true);
    await invoke("openTask", { id: issueId });
    expect(((await snapshot(["task"])).state.task as Task).state).toBe(issue.state);
    console.info(JSON.stringify({ issueReviewAcceptance: {
      instance: instance.instanceId, build: instance.build, issueId, worktree: fixture.worktree,
      sourceId, fixId: fix.id, outcomes, nativeReviewRequests: reviews.length,
      oldPiDraftPreserved: true, editedReviewDraftPreserved: true, editedFixDraftPreserved: true, modelCalls: false,
    } }));
  } finally {
    if (activeReview && attachment) {
      try {
        fixtureGuard(root, address);
        const state = await client.automationCall(attachment, { type: "snapshot", sections: ["selection"] });
        if (at(state.value, "state", "selection", "id") === activeReview) {
          await client.automationCall(attachment, { type: "invoke", verb: "cancelPrompt", args: {} });
        }
      } catch { /* Do not act on an unverified or replaced executor during cleanup. */ }
    }
    waits?.close();
    if (attachment) await client.automation({ type: "detach", sessionId: attachment }).catch(() => {});
    client.disconnect();
    try {
      fixtureGuard(root, address);
      if (existsSync(modePath)) plainPath(modePath);
      if (ownedMode !== undefined && existsSync(modePath) && readFileSync(modePath, "utf8") === ownedMode) {
        if (previousMode === undefined) unlinkSync(modePath);
        else writeFileSync(modePath, previousMode);
      }
    } finally { closeSync(lock); unlinkSync(lockPath); }
  }
}, 180_000);
