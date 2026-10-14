// Provider-free compatibility test using Pi's official loader and the real REPL.
// Usage: node scripts/test-pi-extension.mjs <waku_js_repl> <pi-install-root> <exact-version> [...]
// Install each Pi version separately, pinning all @earendil-works dependencies
// with npm overrides. A package-lock.json and matching installed packages are required.
// This tests the registered extension contract, not Pi RPC sessions, models, or UI.
import assert from "node:assert/strict";
import childProcess from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import {
  existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync,
  realpathSync, rmSync,
} from "node:fs";
import { createRequire, syncBuiltinESMExports } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const script = fileURLToPath(import.meta.url);
const extensionPath = resolve(dirname(script), "../resources/computer-use/pi-extension.ts");
const packageName = "@earendil-works/pi-coding-agent";
const readJson = (path) => JSON.parse(readFileSync(path, "utf8"));
const sha256 = (path) => createHash("sha256").update(readFileSync(path)).digest("hex");

// Read manifests without importing Pi. Check the entire installed Pi cohort and
// resolve every internal dependency from its actual consumer, including nesting.
function verifyVersions(root, version) {
  assert.match(version, /^\d+\.\d+\.\d+$/, "Use an exact stable Pi version");
  const manifest = readJson(join(root, "package.json"));
  const lock = readJson(join(root, "package-lock.json"));
  assert.equal(manifest.dependencies?.[packageName], version);
  assert.equal(lock.packages?.[""]?.dependencies?.[packageName], version);
  const packages = new Map();
  for (const [relativePath, locked] of Object.entries(lock.packages)) {
    if (!/node_modules\/@earendil-works\/[^/]+$/.test(relativePath)) continue;
    const path = realpathSync(join(root, relativePath));
    const installed = readJson(join(path, "package.json"));
    assert.equal(locked.version, version, `Lockfile version: ${relativePath}`);
    assert.equal(installed.version, version, `Installed version: ${relativePath}`);
    assert.equal(installed.name, `@earendil-works/${relativePath.split("/").at(-1)}`);
    if (installed.name !== packageName) {
      assert.equal(manifest.overrides?.[installed.name], version, `Pin ${installed.name} with an npm override`);
    }
    packages.set(path, installed);
  }
  const agentRoot = realpathSync(join(root, "node_modules", packageName));
  assert(packages.has(agentRoot), "Pi must be present in package-lock.json");
  for (const [path, installed] of packages) {
    const require = createRequire(join(path, "package.json"));
    for (const name of Object.keys(installed.dependencies ?? {})) {
      if (!name.startsWith("@earendil-works/")) continue;
      // Some Pi packages have import-only exports, so inspect Node's package
      // search paths without selecting a CommonJS entrypoint or executing code.
      const dependencyPath = require.resolve.paths(name)
        .map((directory) => join(directory, name))
        .find((directory) => existsSync(join(directory, "package.json")));
      assert(dependencyPath, `${installed.name} cannot resolve ${name}`);
      assert.equal(packages.get(realpathSync(dependencyPath))?.name, name,
        `${installed.name} resolves ${name} outside the verified Pi cohort`);
    }
  }
  console.log(`Verified ${packages.size} installed Pi packages at ${version}`);
  return agentRoot;
}

async function runIsolated(repl, root, version) {
  const allowedEnv = ["HOME", "USERPROFILE", "TMPDIR", "TMP", "TEMP", "PATH", "WAKU_JS_REPL_SERVER"];
  if (process.platform === "win32") allowedEnv.push("SystemRoot");
  assert.deepEqual(Object.keys(process.env).sort(), allowedEnv.sort(), "Unexpected inherited environment");
  assert.deepEqual(readdirSync(process.env.HOME), [], "HOME must start empty");
  assert.equal(process.env.WAKU_JS_REPL_SERVER, repl);
  assert.equal(process.cwd(), dirname(process.env.HOME));
  const agentRoot = verifyVersions(root, version);
  const loader = join(agentRoot, "dist/core/extensions/loader.js");
  console.log(`Pi ${version}; loader sha256=${sha256(loader)}`);

  const children = [];
  const spawn = childProcess.spawn;
  // Observe real subprocesses without replacing the REPL or its transport.
  // No shell, provider, native helper, or desktop process may be launched.
  childProcess.spawn = (command, args, options) => {
    assert.equal(command, repl, "Only the selected REPL may be spawned");
    assert.deepEqual(args, []);
    assert.deepEqual(options.env, { ...process.env }, "REPL environment must stay isolated");
    const child = spawn(command, args, options);
    children.push({ child, closed: once(child, "close") });
    return child;
  };
  syncBuiltinESMExports();
  const watchdog = setTimeout(() => {
    console.error(`FAIL Pi ${version}: extension test exceeded 45 seconds`);
    for (const { child } of children) child.kill("SIGKILL");
    process.exit(1);
  }, 45_000);

  let shutdown;
  let passed = 0;
  async function check(name, action) {
    await action();
    console.log(`PASS ${++passed}: ${name}`);
  }
  try {
    // This is deliberately the first third-party import, after isolation/pins.
    const { loadExtensions } = await import(pathToFileURL(loader).href);
    const result = await loadExtensions([extensionPath], process.cwd());
    assert.deepEqual(result.errors, [], "Official Pi loader rejected extension");
    assert.deepEqual(result.warnings, []);
    assert.equal(result.extensions.length, 1);
    const extension = result.extensions[0];
    const js = extension.tools.get("js")?.definition;
    const reset = extension.tools.get("js_reset")?.definition;
    shutdown = async () => {
      for (const handler of extension.handlers.get("session_shutdown") ?? []) {
        await handler({ type: "session_shutdown" }, {});
      }
    };
    let callId = 0;
    const execute = (code, params = {}) => js.execute(
      `pi-compat-${++callId}`, { code, ...params }, undefined, undefined, {},
    );
    const text = (output) => output.content.filter((item) => item.type === "text")
      .map((item) => item.text).join("\n");

    await check("official loader registration and schemas", () => {
      assert.deepEqual([...extension.tools.keys()].sort(), ["js", "js_reset"]);
      assert.deepEqual([...extension.handlers.keys()], ["session_shutdown"]);
      assert.equal(extension.handlers.get("session_shutdown").length, 1);
      assert.equal(js.executionMode, "sequential");
      assert.equal(reset.executionMode, "sequential");
      assert.equal(js.parameters.type, "object");
      assert.deepEqual(js.parameters.required, ["code"]);
      assert.equal(js.parameters.properties.code.type, "string");
      assert.equal(js.parameters.properties.timeout_ms.type, "integer");
      assert.equal(js.parameters.properties.timeout_ms.minimum, 1);
      assert.equal(js.parameters.properties.title.maxLength, 80);
      assert.equal(js.parameters.additionalProperties, false);
      assert.equal(reset.parameters.additionalProperties, false);
      assert.deepEqual(reset.parameters.properties, {});
      assert(!js.description.includes("Debug app automation"));
      assert(!js.description.includes("Computer Use is enabled"));
      assert.equal(children.length, 0, "Loading must not start a kernel");
    });
    await check("text, top-level await, and safe runtime defaults", async () => {
      const output = await execute(`
        var compatCounter = await Promise.resolve(40);
        jsRepl.write(JSON.stringify({ value: compatCounter, cua: typeof cua,
          setup: typeof setupComputerUseRuntime, env: jsRepl.env }));
      `, { title: "Pi extension smoke test", timeout_ms: 5_000 });
      assert.deepEqual(JSON.parse(text(output)), { value: 40, cua: "undefined", setup: "function", env: {} });
      assert.deepEqual(output.details, {});
      assert.equal(children.length, 1);
    });
    await check("persistent bindings across calls", async () => {
      assert.equal(text(await execute("jsRepl.write(++compatCounter)")), "41");
    });
    await check("MCP response metadata becomes Pi details", async () => {
      const output = await execute('jsRepl.setResponseMeta({ source: "pi-compat", count: 2 }); jsRepl.write("metadata")');
      assert.equal(text(output), "metadata");
      assert.deepEqual(output.details, { source: "pi-compat", count: 2 });
    });
    await check("image content passes through unchanged", async () => {
      const png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aGxkAAAAASUVORK5CYII=";
      const output = await execute(`await jsRepl.emitImage({ type: "image", mimeType: "image/png", data: "${png}" })`);
      assert.deepEqual(output.content.filter((item) => item.type === "image"),
        [{ type: "image", mimeType: "image/png", data: png }]);
      assert.deepEqual(output.details, {}, "Metadata must not leak into later calls");
    });
    await check("execution errors reject and retain prior bindings", async () => {
      await assert.rejects(execute('throw new Error("pi-compat deliberate error")'), /pi-compat deliberate error/);
      assert.equal(text(await execute("jsRepl.write(++compatCounter)")), "42");
    });
    await check("reset clears bindings and reinstalls helpers", async () => {
      const output = await reset.execute("pi-compat-reset", {}, undefined, undefined, {});
      assert.equal(text(output), "js kernel reset");
      assert.deepEqual(output.details, {});
      assert.equal(text(await execute('jsRepl.write([typeof compatCounter, typeof jsRepl.write, typeof setupComputerUseRuntime, typeof cua].join(":"))')),
        "undefined:function:function:undefined");
      assert.equal(children.length, 1, "Reset must reuse the same REPL process");
    });
    await check("session_shutdown closes the real REPL and is idempotent", async () => {
      await shutdown();
      await children[0].closed;
      assert(children[0].child.exitCode !== null || children[0].child.signalCode !== null);
      await shutdown();
      assert.equal(children.length, 1);
    });
    console.log(`Pi ${version}: ${passed} checks passed (provider-free; no desktop actions)`);
  } finally {
    // The watchdog also bounds cleanup if a broken extension never acknowledges shutdown.
    if (shutdown) await shutdown();
    for (const { child } of children) {
      if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
    }
    await Promise.all(children.map(({ closed }) => closed));
    clearTimeout(watchdog);
    childProcess.spawn = spawn;
    syncBuiltinESMExports();
  }
}

if (process.argv[2] === "--isolated") {
  await runIsolated(...process.argv.slice(3));
} else {
  const [replArgument, ...matrix] = process.argv.slice(2);
  assert(replArgument && matrix.length >= 2 && matrix.length % 2 === 0,
    "Usage: node scripts/test-pi-extension.mjs <waku_js_repl> <pi-install-root> <exact-version> [...]");
  const repl = realpathSync(resolve(replArgument));
  assert(existsSync(extensionPath));
  console.log(`Extension sha256=${sha256(extensionPath)}\nREPL sha256=${sha256(repl)}`);
  let failures = 0;
  for (let index = 0; index < matrix.length; index += 2) {
    const root = realpathSync(resolve(matrix[index]));
    const version = matrix[index + 1];
    const directory = mkdtempSync(join(tmpdir(), "waku-pi-extension-"));
    const home = join(directory, "home");
    const temp = join(directory, "tmp");
    mkdirSync(home);
    mkdirSync(temp);
    try {
      const env = {
        HOME: home, USERPROFILE: home, TMPDIR: temp, TMP: temp, TEMP: temp,
        PATH: dirname(process.execPath), WAKU_JS_REPL_SERVER: repl,
      };
      if (process.platform === "win32") env.SystemRoot = process.env.SystemRoot;
      const child = childProcess.spawnSync(process.execPath,
        [script, "--isolated", repl, root, version],
        { cwd: directory, env, encoding: "utf8", timeout: 60_000, maxBuffer: 4 * 1024 * 1024 });
      process.stdout.write(child.stdout ?? "");
      process.stderr.write(child.stderr ?? "");
      if (child.status !== 0 || child.error) {
        failures++;
        console.error(`FAIL Pi ${version}: ${child.error?.message ?? `exit ${child.status}, signal ${child.signal}`}`);
      }
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  }
  console.log(`Matrix: ${matrix.length / 2 - failures}/${matrix.length / 2} Pi versions passed`);
  process.exitCode = failures ? 1 : 0;
}
