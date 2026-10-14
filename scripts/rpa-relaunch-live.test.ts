// Opt-in native fresh-build acceptance. Start this before rebuilding/relaunching
// the target app against its existing stable external daemon. No model call.
import { expect, test } from "bun:test";
import { WakuClient } from "../packages/waku-client/src/client";
import { selectInstance } from "./rpa";

const liveTest = process.env.WAKU_RPA_RELAUNCH_TEST === "1" ? test : test.skip;
liveTest("native fresh-build wait follows a replacement process on the same daemon", async () => {
  const address = process.env.WAKU_DAEMON_ADDRESS;
  const token = process.env.WAKU_DAEMON_TOKEN;
  if (!address || !token) throw new Error("native relaunch test requires the existing daemon connection");
  const client = new WakuClient({ address, token });
  let child: ReturnType<typeof Bun.spawn> | undefined;
  try {
    await client.connect();
    const before = selectInstance(await client.automationInstances(), { checkout: process.env.WAKU_RPA_TEST_CHECKOUT ?? process.cwd() });
    console.log(`Waiting for a fresh native build of ${before.checkout}; previous instance ${before.instanceId}`);
    const launched = Bun.spawn([process.execPath, `${import.meta.dir}/rpa.ts`, "--checkout", before.checkout,
      "wait-build", "--after-build", before.build, "--timeout", "120000", "--json"], {
      env: { ...process.env, WAKU_RPA_CONFIG: "" }, stdout: "pipe", stderr: "pipe",
    });
    child = launched;
    const [code, stdout, stderr] = await Promise.all([launched.exited, new Response(launched.stdout).text(), new Response(launched.stderr).text()]);
    if (stdout.includes(token) || stderr.includes(token)) throw new Error("relaunch command exposed its transport credential");
    if (code !== 0) throw new Error(`fresh-build command failed: ${stderr}`);
    const after = JSON.parse(stdout);
    expect(after.checkout).toBe(before.checkout);
    expect(after.instanceId).not.toBe(before.instanceId);
    expect(after.pid).not.toBe(before.pid);
    expect(after.build).not.toBe(before.build);
    expect(after.executorConnected).toBe(true);
    const session = await client.attachAutomation(after.instanceId);
    try {
      const snapshot = await client.automationCall(session.sessionId, { type: "snapshot", sections: ["tree", "route"] });
      expect(snapshot.observed).toBeObject();
    } finally { await client.automation({ type: "detach", sessionId: session.sessionId }); }
    console.log(JSON.stringify({ oldInstance: before.instanceId, newInstance: after.instanceId, oldBuild: before.build, newBuild: after.build, sameDaemon: true }));
  } finally {
    child?.kill();
    client.disconnect();
  }
}, 130_000);
