// Source guard complements the live scenario. GPUI cannot warn about a raw
// handwritten Div that never has either id or role; shared primitives carry
// their local contract, and changes to these core callsites must be explicit.
import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
const root = join(import.meta.dir, "..");
const read = (path: string) => readFileSync(join(root, path), "utf8");
const scenario = JSON.parse(read("rpa/scenarios/core-smoke.json"));

test("core smoke pins actual source controls and contains no model calls", () => {
  const ids = scenario.steps.filter((step: any) => step.type === "wait").map((step: any) => step.condition.id);
  expect(ids).toEqual(["composer-field", "send-or-stop", "toggle-sidebar", "open-settings"]);
  for (const [path, id] of [["src/app/composer.rs", "send-or-stop"], ["src/app/sidebar.rs", "toggle-sidebar"], ["src/app/sidebar.rs", "open-settings"]]) {
    const source = read(path!);
    const marker = `.id("${id}")`;
    const index = source.indexOf(marker);
    expect(index, `${id} remains a semantic core control`).toBeGreaterThanOrEqual(0);
    expect(source.slice(index, index + 350)).toMatch(/\.(role|semantic_control)\(gpui::Role::/);
  }
  const input = read("src/input.rs");
  expect(input).toContain('.semantic_id("composer-field")');
  expect(input).toContain(".id(self.semantic_id.clone())");
  expect(input).toContain('format!("text-input-{}", cx.entity_id().as_u64())');
  expect(scenario.steps.every((step: any) => step.type === "wait" || step.call?.type === "snapshot")).toBe(true);
});

test("shared primitives retain identity and disabled-state annotation", () => {
  const primitives = read("src/ui/mod.rs");
  for (const name of ["icon_button", "toggle_switch", "MenuChip", "ProjectNameSelector"]) expect(primitives).toContain(name);
  expect(primitives.match(/\.semantic_control\(/g)!.length).toBeGreaterThanOrEqual(6);
  const semantics = read("src/ui/semantics.rs");
  expect(semantics).toContain("missing_identity(self.interactivity().element_id.as_ref())");
  expect(semantics).toContain("builder.parent_node().set_disabled()");
  expect(read("src/input.rs")).toContain("semantic_control(gpui::Role::TextInput, self.read_only)");
});


test("new workspaces inherit existing attachment and announce readiness off-lease", () => {
  const executor = read("src/app/rpa/daemon.rs");
  const observer = executor.slice(executor.indexOf("cx.observe_new::<Waku>"), executor.indexOf("let (outgoing, output)"));
  expect(observer).toMatch(/cx\.defer\(\|app\|\s*\{\s*super::sync_automation\(app\);\s*model_changed\(\);/);
  expect(observer).not.toContain("on_next_frame");
  expect(observer).toMatch(/cx\.observe\(&entity, \|_, _, _\| painted\(\)\)/);
});

test("macOS watcher forwards only explicitly supplied daemon connection values", () => {
  const watcher = read("scripts/dev.ts");
  const launch = watcher.slice(watcher.indexOf("function launchApp()"), watcher.indexOf("/// Constructs the checkout"));
  expect(launch).toContain('for (const key of ["WAKU_DAEMON_ADDRESS", "WAKU_DAEMON_TOKEN"])');
  expect(launch).toContain("const value = process.env[key]");
  expect(launch).toContain("if (value) launchEnvironment.push([key, value])");
  expect(launch).not.toMatch(/console\.[a-z]+\([^;]*(launchEnvironment|DAEMON_TOKEN)/);
});

test("issue and conversation verbs retain separate handlers and snapshot identities", () => {
  const executor = read("src/app/rpa.rs");
  const verb = (name: string, next: string) => executor.slice(executor.indexOf(`"${name}" => {`), executor.indexOf(`"${next}" => {`));
  expect(verb("openTask", "openSession")).toContain("require_task(&self.state.tasks, id)");
  expect(verb("openTask", "openSession")).toContain("self.open_task_from_card(id, cx)");
  expect(verb("openSession", "selectProject")).toContain("self.select_session(id, cx)");
  expect(verb("newTask", "newSession")).toContain("self.new_task_action(&crate::NewTask, window, cx)");
  expect(verb("newTask", "newSession")).toContain("self.commit_new_task(cx)");
  expect(verb("newSession", "submitPrompt")).toContain("self.start_task_session(id, window, cx)");
  expect(verb("newSession", "submitPrompt")).toContain("self.new_session_action(&crate::NewSession, window, cx)");
  expect(executor).toContain('"tasks" => json_of(&self.state.tasks)');
  expect(executor).toContain('"sessions" => observable_session_list(&self.state.sessions)');

  const fanOut = JSON.parse(read("rpa/scenarios/fan-out-real-model.json"));
  const creation = fanOut.steps.find((step: any) => step.call?.verb === "newSession");
  expect(creation?.call.args).toEqual({});
  expect(fanOut.steps.some((step: any) => step.call?.verb === "newTask")).toBe(false);
});


test("Board controls expose semantic identity and disabled state", () => {
  const details = read("src/app/task_details.rs");
  const button = details.slice(details.indexOf("fn task_action_button"), details.indexOf("fn start_task_session"));
  expect(button).toContain("semantic_control(gpui::Role::Button, !enabled)");
  expect(button).toContain(".aria_label(label.clone())");
  expect(details).toContain('format!("task-session-{id}")');
  const attachPicker = read("src/app/task_details/attach_picker.rs");
  expect(attachPicker).toContain('format!("task-attach-{id}")');
  expect(attachPicker).toContain("semantic_control(gpui::Role::Button, false)");
  expect(attachPicker).toContain('.semantic_id("task-attach-search")');
  const board = read("src/app/board.rs");
  for (const id of ["task-complete-cancel", "task-complete-confirm"]) {
    const position = board.indexOf(`.id("${id}")`);
    expect(board.slice(position, position + 250)).toContain("semantic_control(gpui::Role::Button, false)");
  }
  expect(board).toContain(".aria_label(task.title.clone())");
  const menus = read("src/ui/menu.rs");
  expect(menus).toContain("semantic_control(gpui::Role::MenuItem, disabled)");
  const submenu = menus.slice(menus.indexOf("MenuItem::Submenu {\n            semantic_id"), menus.indexOf("MenuItem::Custom { render"));
  expect(submenu).toContain("semantic_control(gpui::Role::MenuItem, false)");
  expect(submenu).toContain(".aria_label(label.clone())");
  const composer = read("src/app/composer.rs");
  const checkbox = composer.slice(composer.indexOf('.id("composer-create-worktree")'), composer.indexOf('.id("composer-create-worktree")') + 550);
  expect(checkbox).toContain("semantic_control(gpui::Role::CheckBox, !enabled)");
  expect(checkbox).toContain("aria_toggled");
  const completion = read("src/app/task_completion.rs");
  const rollback = completion.slice(completion.indexOf("if rollback.apply(*attempt, task)"), completion.indexOf("if rollback.apply(*attempt, task)") + 210);
  expect(rollback).toContain("super::rpa::daemon::model_changed()");
});


test("annotation overlays expose inspectable identities instead of requiring coordinates", () => {
  const source = read("src/app/transcript_view.rs");
  for (const id of ["selection-add-to-chat", "annotation-editor-cancel", "annotation-editor-save"]) {
    const offset = source.indexOf(`.id("${id}")`);
    expect(offset).toBeGreaterThanOrEqual(0);
    expect(source.slice(offset, offset + 230)).toContain("semantic_control(gpui::Role::Button, false)");
  }
  expect(source).toContain('.semantic_id("annotation-editor-input")');
  expect(source).toContain('.semantic_id("annotation-comment-input")');
  expect(read("src/app/rpa.rs")).toContain('"toolbarVisible": self.selection_toolbar.borrow().is_some()');
});


test("agent context presets agree across app discovery, CLI and REPL", () => {
  const info = read("src/app/rpa/info.rs");
  const rust = info.slice(info.indexOf("const CONTEXT_SECTIONS"),info.indexOf("pub(super) fn capabilities"));
  const sections = [...rust.matchAll(/"([a-zA-Z]+)"/g)].map(match => match[1]);
  const cli = read("scripts/rpa.ts").match(/const CONTEXT_SECTIONS = (\[[^;]+\]);/)![1]!;
  const repl = read("src/js_repl_bootstrap.js").match(/const contextSections = (\[[^;]+\]);/)![1]!;
  expect(JSON.parse(cli)).toEqual(sections);
  expect(JSON.parse(repl)).toEqual(sections);
});
