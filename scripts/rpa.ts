#!/usr/bin/env bun
// The command-line client for the debug app's RPA endpoint.
//
// Reads the instance publication (`temp/rpa.json` under a checkout, or the
// path in `--config`/`WAKU_RPA_CONFIG`), sends one newline-delimited JSON
// call, and prints the answer. Commands mirror the `waku.rpa` bindings so the
// same mental model works from a shell and from an agent session.
//
// Usage: bun scripts/rpa.ts [--config <path>] [--json] <command> [args]

import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";

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

type Reply = {
  id: number;
  ok: boolean;
  result?: any;
  error?: { code: string; message: string; target?: string; nearest?: string[] };
};

function usage(): never {
  console.log(`Usage: bun scripts/rpa.ts [--config <path>] [--json] <command> [args]

Commands:
  describe                       instance and build identity
  tree [pattern]                 controls on screen (id, role, label)
  state [pointer]                app state (optionally one JSON pointer)
  snap                           the full snapshot as JSON
  click <id>                     click a control by semantic id
  focus <id>                     focus a control by semantic id
  type <text> [--into <id>]      type text, optionally focusing a control first
  key <chord>                    send a chord (e.g. secondary-l)
  invoke <verb> [args-json]      run a workspace verb (openTask, selectProject, newTask)
  wait [--id ID [--gone]] [--text ID=SUBSTR] [--state PTR=VALUE] [--state-contains PTR=SUBSTR] [--timeout MS]
  raw '<json>'                   send one raw request object

Discovery: --config <path>, else WAKU_RPA_CONFIG, else ./temp/rpa.json.`);
  process.exit(2);
}

function argument(name: string): string | undefined {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

function loadDiscovery(): Discovery {
  const explicit =
    argument("--config") ?? process.env.WAKU_RPA_CONFIG ?? resolve("temp/rpa.json");
  if (!existsSync(explicit)) {
    console.error(
      `rpa: no discovery file at ${explicit}; is the debug app running, and are you in its checkout?`,
    );
    process.exit(1);
  }
  const discovery = JSON.parse(readFileSync(explicit, "utf8")) as Discovery;
  return discovery;
}

async function call(discovery: Discovery, request: Record<string, unknown>): Promise<Reply> {
  const [host, port] = discovery.address.split(":");
  return await new Promise<Reply>((resolveReply, rejectReply) => {
    let buffer = "";
    Bun.connect({
      hostname: host,
      port: Number(port),
      socket: {
        open(socket) {
          socket.write(
            JSON.stringify({
              id: 1,
              version: discovery.protocolVersion,
              token: discovery.token,
              ...request,
            }) + "\n",
          );
        },
        data(socket, data) {
          buffer += data.toString();
          const newline = buffer.indexOf("\n");
          if (newline >= 0) {
            // Resolve before closing: `end()` can emit `close` synchronously.
            resolveReply(JSON.parse(buffer.slice(0, newline)) as Reply);
            socket.end();
          }
        },
        error(_socket, error) {
          rejectReply(error);
        },
        close() {
          rejectReply(new Error("the endpoint closed before answering"));
        },
      },
    }).catch(rejectReply);
  });
}

function reportFailure(reply: Reply): never {
  const error = reply.error;
  console.error(`rpa: ${error?.code ?? "error"}: ${error?.message ?? "unknown failure"}`);
  if (error?.nearest?.length) {
    console.error(`  nearest: ${error.nearest.join(", ")}`);
  }
  process.exit(1);
}

function nodesOf(snapshot: any): any[] {
  return snapshot?.tree?.nodes ?? [];
}

async function main() {
  const command = process.argv[2];
  if (!command || command.startsWith("--")) usage();
  const discovery = loadDiscovery();
  const asJson = process.argv.includes("--json");

  const send = async (request: Record<string, unknown>) => {
    const reply = await call(discovery, request);
    if (!reply.ok) reportFailure(reply);
    return reply.result;
  };

  switch (command) {
    case "describe": {
      const result = await send({ type: "describe" });
      if (asJson) console.log(JSON.stringify(result, null, 2));
      else {
        const instance = result.instance ?? {};
        console.log(
          `${instance.app} ${instance.build} · pid ${instance.pid} · ${instance.checkout} · ${instance.protocolVersion ? `protocol v${instance.protocolVersion}` : ""}`.trim(),
        );
      }
      break;
    }
    case "tree": {
      const pattern = process.argv[3];
      const snapshot = await send({ type: "snapshot" });
      const nodes = nodesOf(snapshot).filter((node: any) => node.element_id);
      const filtered = pattern ? nodes.filter((node: any) => matches(node, pattern)) : nodes;
      if (asJson) console.log(JSON.stringify(filtered, null, 2));
      else {
        for (const node of filtered) {
          const label = node.label ?? node.value ?? "";
          const flags = [
            node.disabled ? "disabled" : "",
            (node.actions ?? []).includes("Click") ? "clickable" : "",
          ]
            .filter(Boolean)
            .join(",");
          console.log(`${node.element_id}\t${node.role}\t${label}${flags ? `\t${flags}` : ""}`);
        }
        console.log(`(${filtered.length} of ${nodes.length} controls)`);
      }
      break;
    }
    case "state": {
      const pointer = process.argv[3];
      const snapshot = await send({ type: "snapshot" });
      let value = snapshot.state;
      if (pointer) {
        value = pointer
          .split("/")
          .filter(Boolean)
          .reduce((current: any, key: string) => current?.[key], value);
      }
      console.log(asJson ? JSON.stringify(value) : JSON.stringify(value, null, 2));
      break;
    }
    case "snap": {
      const snapshot = await send({ type: "snapshot" });
      console.log(JSON.stringify(snapshot, null, 2));
      break;
    }
    case "click":
    case "focus": {
      const id = process.argv[3];
      if (!id) usage();
      const result = await send({ type: "act", action: command, target: { id } });
      console.log(asJson ? JSON.stringify(result) : `ok ${command} ${id}`);
      break;
    }
    case "type": {
      const text = process.argv[3];
      if (text === undefined) usage();
      const into = argument("--into");
      const result = await send({
        type: "type",
        text,
        target: into ? { id: into } : undefined,
      });
      console.log(asJson ? JSON.stringify(result) : `ok typed ${result.typed} chars`);
      break;
    }
    case "key": {
      const chord = process.argv[3];
      if (!chord) usage();
      const result = await send({ type: "key", chord });
      console.log(asJson ? JSON.stringify(result) : `ok keys ${result.keys}`);
      break;
    }
    case "invoke": {
      const verb = process.argv[3];
      if (!verb) usage();
      const args = process.argv[4] ? JSON.parse(process.argv[4]) : {};
      const result = await send({ type: "invoke", verb, args });
      console.log(asJson ? JSON.stringify(result) : `ok ${verb} ${JSON.stringify(result)}`);
      break;
    }
    case "wait": {
      const condition = conditionFromArguments();
      const timeout = Number(argument("--timeout") ?? 10000);
      const result = await send({ type: "wait", condition, timeout_ms: timeout });
      console.log(asJson ? JSON.stringify(result) : `ok waited ${result.elapsedMs}ms`);
      break;
    }
    case "raw": {
      const request = JSON.parse(process.argv[3] ?? "{}");
      const result = await send(request);
      console.log(JSON.stringify(result, null, 2));
      break;
    }
    default:
      usage();
  }
}

function matches(node: any, pattern: string): boolean {
  const needle = pattern.toLowerCase();
  return [node.element_id, node.role, node.label, node.value]
    .filter(Boolean)
    .some((field: string) => String(field).toLowerCase().includes(needle));
}

function conditionFromArguments(): Record<string, unknown> {
  const id = argument("--id");
  if (id) {
    const gone = process.argv.includes("--gone");
    return { on: "node", id, present: !gone };
  }
  const text = argument("--text");
  if (text) {
    const separator = text.indexOf("=");
    if (separator < 0) {
      console.error("rpa: --text wants ID=SUBSTRING");
      process.exit(2);
    }
    return { on: "text", id: text.slice(0, separator), contains: text.slice(separator + 1) };
  }
  const state = argument("--state");
  if (state) {
    const separator = state.indexOf("=");
    if (separator < 0) {
      console.error("rpa: --state wants POINTER=VALUE");
      process.exit(2);
    }
    return {
      on: "state",
      pointer: state.slice(0, separator),
      equals: JSON.parse(state.slice(separator + 1)),
    };
  }
  const stateContains = argument("--state-contains");
  if (stateContains) {
    const separator = stateContains.indexOf("=");
    if (separator < 0) {
      console.error("rpa: --state-contains wants POINTER=SUBSTRING");
      process.exit(2);
    }
    return {
      on: "state_contains",
      pointer: stateContains.slice(0, separator),
      contains: stateContains.slice(separator + 1),
    };
  }
  usage();
}

await main();
