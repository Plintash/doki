// Provider-free native acceptance for agent information. The test creates and
// removes its own scratch app; it never submits a prompt or changes providers.
import { expect, test } from "bun:test";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { WakuAutomationError, WakuClient, type AutomationMessage } from "../packages/waku-client/src/client";
import type { AgentSession, AutomationCondition, TranscriptQuery, TranscriptResult } from "../packages/waku-client/src/generated";

const nativeTest = process.env.WAKU_RPA_AGENT_INFO_TEST === "1" ? test : test.skip;
const at = (value: unknown, ...keys: string[]): unknown => keys.reduce((value, key) => value && typeof value === "object" ? (value as Record<string, unknown>)[key] : undefined, value);

nativeTest("native agent diagnosis stays bounded and reads unselected history without changing the draft", async () => {
  const address = process.env.WAKU_DAEMON_ADDRESS;
  const token = process.env.WAKU_DAEMON_TOKEN;
  if (!address || !token) throw new Error("supply the existing isolated daemon connection");
  const client = new WakuClient({ address, token, requestTimeoutMs: 40_000 });
  let scratchId: string | undefined;
  let attachment: string | undefined;
  let unsubscribe: (() => void) | undefined;
  try {
    await client.connect();
    const projectId = crypto.randomUUID();
    const largeId = crypto.randomUUID();
    const storedId = crypto.randomUUID();
    const storedMessageId = crypto.randomUUID();
    const activityId = crypto.randomUUID();
    const largeActivityId = crypto.randomUUID();
    const now = Math.floor(Date.now() / 1000);
    const huge = "A bounded observation must not carry this full transcript.\n".repeat(90_000).trimEnd();
    const unicode = '工具结果 🐱 中文 "quoted" \\ escaped\n'.repeat(5_000);
    const session = (id: string, title: string): AgentSession => ({
      id, title, project_id: projectId, provider: "pi", runtime_mode: "fullAccess", status: "idle", created_at: now, updated_at: now,
      provider_cursor: null, messages: [], transcript_blocks: [], turns: [],
    });
    const large = session(largeId, "Large selected fixture");
    large.messages = [{ id: crypto.randomUUID(), turn_id: null, role: "assistant", content: "Inspect the synthetic command output without expanding it.", created_at: now, streaming: false }];
    large.transcript_blocks = [{after_message:1,turn_id:null,content:{kind:"activities",data:[{
      id:largeActivityId,source_id:"large-fixture-tool",kind:"command",title:"Large synthetic command output",detail:null,arguments:"{}",output:huge,complete:true,failed:false,
    }]}}];
    const stored = session(storedId, "Unselected persisted fixture");
    stored.messages = [{ id: storedMessageId, turn_id: null, role: "assistant", content: unicode, created_at: now, streaming: false }];
    stored.transcript_blocks = [{ after_message: 1, turn_id: null, content: {kind:"activities", data:[{
      id:activityId, source_id:"fixture-tool", kind:"command", title:"Synthetic command output", detail:null, arguments:'{"command":"echo fixture"}', output:unicode,
      complete:true, failed:false,
    }]}}];
    const created = await client.automation({type:"scratchCreate", name:"Agent information acceptance", seed:{
      projects:[{id:projectId,name:"Information fixture",path:"/synthetic/agent-information",created_at:now}], sessions:[large,stored],
    }}, {timeoutMs:40_000});
    if (created.type !== "scratchCreated") throw new Error("scratch did not start");
    scratchId = created.instance.instanceId;
    // Scratch projects are newly created and owned by this test. An invalid
    // local Git boundary prevents discovery from escaping into the enclosing
    // checkout when the daemon stores scratch data under <checkout>/temp.
    writeFileSync(join(dirname(created.instance.database), "projects", projectId, ".git"), "gitdir: missing-agent-info-fixture-repository\n", {flag:"wx"});
    attachment = (await client.attachAutomation(scratchId)).sessionId;
    const notifications: AutomationMessage[] = [];
    const listeners = new Set<() => void>();
    unsubscribe = client.subscribeAutomation(attachment, message => { notifications.push(message); for (const listener of listeners) listener(); });
    const finishWait = (waitId: string) => new Promise<void>((resolve, reject) => {
      const check = () => {
        const found = notifications.find(message => message.type === "waitResolved" && message.wait.waitId === waitId);
        if (found?.type === "waitResolved") {
          clearTimeout(timer); listeners.delete(check);
          if (found.wait.status === "succeeded") resolve(); else reject(new Error(JSON.stringify(found.wait)));
        }
      };
      const timer = setTimeout(() => { listeners.delete(check); reject(new Error("wait notification deadline")); },20_000);
      listeners.add(check); check();
    });
    const wait = async (condition: AutomationCondition) => {
      const response = await client.automation({type:"wait",sessionId:attachment!,condition,timeoutMs:15_000});
      if (response.type !== "wait") throw new Error("unexpected wait result");
      if (response.wait.status !== "pending") { expect(response.wait.status).toBe("succeeded"); return; }
      await finishWait(response.wait.waitId);
    };
    await wait({on:"node",id:"board-new-issue",present:true});
    await client.automationCall(attachment,{type:"invoke",verb:"openSession",args:{id:largeId}});
    await wait({on:"state",pointer:"/selection/id",equals:largeId});
    await wait({on:"state",pointer:"/selection/detailLoaded",equals:true});
    await wait({on:"state",pointer:"/composer/providerAvailable",equals:false});
    const described = await client.automationCall(attachment,{type:"describe"});
    expect(at(described.value,"capabilities","readOnly")).toContain("transcript");
    expect(at(described.observed,"state","session")).toBeUndefined();
    expect(JSON.stringify(described).length).toBeLessThan(300_000);
    const context = await client.automationCall(attachment,{type:"snapshot",sections:["selection","providers","runtime","workspace","composer","ui"]});
    expect(at(context.value,"state","selection","historyCounts","messages")).toBe(1);
    const providers = at(context.value,"state","providers","items") as {enabled:boolean;authentication:string}[];
    expect(providers.every(provider => !provider.enabled && provider.authentication === "notChecked")).toBe(true);
    expect(at(context.value,"state","workspace","source")).toBe("cachedAppState");
    // A real daemon Git lookup of this scratch directory resolves through the
    // requested model section, without the test polling snapshots or a model.
    await wait({on:"state",pointer:"/workspace/branch/state",equals:"notRepository"});
    await client.automationCall(attachment,{type:"invoke",verb:"togglePanel",args:{panel:"rightPanel"}});
    await wait({on:"node",id:"right-panel-card-review",present:true});
    const beforeReview = await client.automationCall(attachment,{type:"snapshot",sections:["workspace"]});
    expect(at(beforeReview.value,"state","workspace","review","state")).toBe("notRequested");
    const reviewWait = await client.automation({type:"wait",sessionId:attachment,condition:{on:"state",pointer:"/workspace/review/state",equals:"error"},timeoutMs:15_000});
    if (reviewWait.type !== "wait") throw new Error("unexpected review wait result");
    expect(reviewWait.wait.status).toBe("pending");
    await client.automationCall(attachment,{type:"act",action:"click",target:{id:"right-panel-card-review"}});
    await finishWait(reviewWait.wait.waitId);
    const unavailableReview = await client.automationCall(attachment,{type:"snapshot",sections:["workspace"]});
    expect(at(unavailableReview.value,"state","workspace","review","error")).toBeString();
    await client.automationCall(attachment,{type:"invoke",verb:"togglePanel",args:{panel:"rightPanel"}});
    expect(at(context.value,"state","ui","annotations","toolbarVisible")).toBe(false);
    await client.automationCall(attachment,{type:"invoke",verb:"openSession",args:{id:largeId}});
    await wait({on:"node",id:"composer-field",present:true});
    const draft = "Keep this unsent fixture draft";
    await client.automationCall(attachment,{type:"type",text:draft,target:{id:"composer-field"}});
    const before = await client.automationCall(attachment,{type:"snapshot",sections:["route","composer","selection"]});
    const read = async (conversationId: string, query: TranscriptQuery) => {
      const result = await client.automationCall(attachment!,{type:"transcript",conversationId,query});
      expect(JSON.stringify(result).length).toBeLessThan(1_000_000);
      return result.value as TranscriptResult;
    };
    const live = await read(largeId,{type:"index",collection:"activities",offset:0,limit:1});
    expect(live.source).toBe("live");
    expect(live.detailLoaded).toBe(true);
    expect(live.page.type).toBe("index");
    if (live.page.type !== "index") throw new Error("index expected");
    const preview = live.page.items[0]!.fields.find(field => field.field === "output")!;
    expect(preview.totalBytes).toBe(Buffer.byteLength(huge));
    expect(preview.previewTruncated).toBe(true);
    const tail = await read(largeId,{type:"text",collection:"activities",itemId:largeActivityId,field:"output",offsetBytes:Buffer.byteLength(huge)-64,maxBytes:64});
    expect(tail.page.type === "text" && tail.page.nextOffsetBytes).toBeNull();
    const persisted = await read(storedId,{type:"index",collection:"messages",offset:0,limit:1});
    expect(persisted.source).toBe("persisted");
    expect(persisted.detailLoaded).toBe(false);
    const reconstruct = async (collection: "messages" | "activities", itemId: string, field: string) => {
      let offsetBytes = 0;
      let text = "";
      for (;;) {
        const page = (await read(storedId,{type:"text",collection,itemId,field,offsetBytes,maxBytes:65_533})).page;
        if (page.type !== "text") throw new Error("text expected");
        text += page.text;
        if (page.nextOffsetBytes === null) return text;
        expect(page.nextOffsetBytes).toBeGreaterThan(offsetBytes);
        offsetBytes = page.nextOffsetBytes;
      }
    };
    expect(await reconstruct("messages",storedMessageId,"content")).toBe(unicode);
    const activity = await read(storedId,{type:"index",collection:"activities",offset:0,limit:1});
    expect(activity.source).toBe("persisted");
    expect(activity.page.type === "index" && activity.page.items[0]!.id).toBe(activityId);
    expect(await reconstruct("activities",activityId,"output")).toBe(unicode);
    const after = await client.automationCall(attachment,{type:"snapshot",sections:["route","composer","selection"]});
    expect(at(after.value,"state")).toEqual(at(before.value,"state"));
    try {
      await client.automationCall(attachment,{type:"snapshot",sections:["session"]});
      throw new Error("oversized full history unexpectedly fit");
    } catch(error) {
      if (!(error instanceof WakuAutomationError)) throw error;
      expect(error.error.code).toBe("observation_too_large");
    }
    try {
      await client.automationCall(attachment,{type:"act",action:"click",target:{id:"deliberately-missing-agent-info-control"}});
      throw new Error("missing control unexpectedly acted");
    } catch(error) {
      if (!(error instanceof WakuAutomationError)) throw error;
      expect(error.error.code).toBe("not_found");
      expect(at(error.error.bundle?.observed,"state","selection","id")).toBe(largeId);
      expect(at(error.error.bundle?.observed,"state","composer","text")).toBe(draft);
      expect(error.error.bundle?.recentEvents).toBeDefined();
    }
    console.info(JSON.stringify({agentInformationAcceptance:{
      instance:created.instance.instanceId,build:created.instance.build,
      selectedHistoryBytes:Buffer.byteLength(huge),describeReplyBytes:Buffer.byteLength(JSON.stringify(described)),
      liveSource:live.source,persistedSource:persisted.source,persistedTextBytes:Buffer.byteLength(unicode),
      draftAndSelectionPreserved:true,oversizedFullSnapshotRejected:true,compactFailureEvidenceRetained:true,
      workspaceFinalStates:["notRepository","error"],
    }}));
  } finally {
    unsubscribe?.();
    if (attachment) await client.automation({type:"detach",sessionId:attachment}).catch(() => {});
    if (scratchId) await client.automation({type:"scratchRemove",instanceId:scratchId}).catch(() => {});
    client.disconnect();
  }
},180_000);
