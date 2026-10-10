// Waku's task-digest extension: one sentence for what a task is for.
//
// The daemon dispatches `/waku:digest <dispatch>` over the same RPC pipe it
// already drives the session with, fifteen seconds after a turn settles. A
// command is handled inside this process and starts no run, so the exchange
// adds no message, no turn, and no context to the task's own conversation; the
// sentence travels back as a custom session entry that Pi does not send to the
// model. See `docs/titles.md` for the trigger shape and for how the title and
// the objective divide the field.
//
// This file returns raw text and nothing else. Waku's Rust side owns the
// parser: the format version, the dispatch correlation, and the rules that
// reject an objective naming a path, an extension, or a symbol. Keeping one
// copy of those rules is the point of the split, so do not add another here.

import type {
  ExtensionAPI,
  ExtensionCommandContext,
} from "@earendil-works/pi-coding-agent";

/** The format version of the entry this extension publishes. */
const ENTRY_VERSION = 1;

/** The namespaced command Pi dispatches and the entry type the result rides in. */
const SURFACE = "waku:digest";

/** How long the nested call may take. The daemon gives up on the same budget. */
const TIMEOUT_MS = 60_000;

/** How much of the conversation the model sees, and how much of each message. */
const MAX_INPUT_CHARS = 4_000;
const MAX_MESSAGE_CHARS = 1_200;
const MAX_MESSAGES = 10;

/** The most the completion may ask for: one short sentence, plus thinking. */
const MAX_OUTPUT_TOKENS = 256;

const SYSTEM_PROMPT = [
  "You write the outcome line for a coding task: one sentence, in the present tense, describing what will be true when the task is done.",
  "",
  "Rules:",
  "- At most 15 words. It is one line in a list, and a line that does not fit is not shown at all.",
  "- Write what the task is for, never how it is done or what is being changed right now.",
  "- Never name a file, a path, a file extension, a symbol, a function, or a tool.",
  "- Do not restate the task's title.",
  "- Answer with the sentence alone: no quotes, no prefix, no markdown, no explanation.",
].join("\n");

/** One text message the digest can read. */
interface DigestMessage {
  id: string;
  role: "user" | "assistant";
  text: string;
}

/**
 * The plain text of a session entry, or nothing for an entry that carries no
 * conversation: a tool call, a tool result, a custom message, or one of the
 * entries that only exist for bookkeeping.
 */
function messageText(entry: unknown): string {
  if (!entry || typeof entry !== "object") return "";
  const record = entry as Record<string, unknown>;
  if (record["type"] !== "message") return "";
  const message = record["message"];
  if (!message || typeof message !== "object") return "";
  const body = message as Record<string, unknown>;
  const role = body["role"];
  if (role !== "user" && role !== "assistant") return "";
  const content = body["content"];
  const text =
    typeof content === "string"
      ? content
      : Array.isArray(content)
        ? content
            .filter(
              (block): block is { type: string; text: string } =>
                !!block &&
                typeof block === "object" &&
                (block as Record<string, unknown>)["type"] === "text" &&
                typeof (block as Record<string, unknown>)["text"] === "string",
            )
            .map((block) => block.text)
            .join("\n")
        : "";
  if (!text.trim()) return "";
  return text;
}

/** The conversation the digest reads: what the task is about and where it got to. */
function digestInput(entries: readonly unknown[]): string {
  const messages: DigestMessage[] = [];
  for (const entry of entries) {
    const text = messageText(entry);
    if (!text) continue;
    const record = entry as Record<string, unknown>;
    const message = record["message"] as Record<string, unknown>;
    messages.push({
      id: typeof record["id"] === "string" ? record["id"] : String(messages.length),
      role: message["role"] as "user" | "assistant",
      text,
    });
  }
  if (messages.length === 0) return "";

  const recent = messages.slice(-MAX_MESSAGES);
  const opening = messages.find((message) => message.role === "user");
  const parts: string[] = [];
  if (opening && !recent.some((message) => message.id === opening.id)) {
    parts.push(`The task started with:\n${clamp(opening.text, MAX_MESSAGE_CHARS)}`);
  }
  for (const message of recent) {
    parts.push(
      `${message.role === "user" ? "User" : "Assistant"}:\n${clamp(message.text, MAX_MESSAGE_CHARS)}`,
    );
  }
  return clamp(parts.join("\n\n"), MAX_INPUT_CHARS, true);
}

/**
 * Bounds `text`, keeping its tail when `fromEnd` asks for it — the newest part
 * of a long input is what describes where the task stands now.
 */
function clamp(text: string, limit: number, fromEnd = false): string {
  if (text.length <= limit) return text;
  return fromEnd ? text.slice(-limit) : text.slice(0, limit);
}

/** The completion's text, or nothing when the provider returned none. */
function completionText(message: unknown): string {
  if (!message || typeof message !== "object") return "";
  const content = (message as Record<string, unknown>)["content"];
  if (!Array.isArray(content)) return "";
  return content
    .filter(
      (block): block is { type: string; text: string } =>
        !!block &&
        typeof block === "object" &&
        (block as Record<string, unknown>)["type"] === "text" &&
        typeof (block as Record<string, unknown>)["text"] === "string",
    )
    .map((block) => block.text)
    .join("\n")
    .trim();
}

/**
 * Generates one objective and publishes it.
 *
 * Every failure — no dispatch, no usable model, a provider error, the timeout —
 * ends in silence with nothing appended, so the task keeps the objective it
 * already had and its conversation never learns a generation happened.
 */
async function publishDigest(
  pi: ExtensionAPI,
  ctx: ExtensionCommandContext,
  dispatch: string,
): Promise<void> {
  // Only a dispatch names a generation. Anything else that reaches this
  // command — a person typing it — has no result to report.
  if (!dispatch) return;
  const model = ctx.model;
  if (!model || !ctx.modelRegistry.hasConfiguredAuth(model)) return;
  const input = digestInput(ctx.sessionManager.getBranch());
  if (!input) return;

  const message = await ctx.modelRegistry.complete(
    model,
    {
      systemPrompt: SYSTEM_PROMPT,
      messages: [
        {
          role: "user",
          content: [{ type: "text", text: input }],
          timestamp: Date.now(),
        },
      ],
    },
    { maxTokens: MAX_OUTPUT_TOKENS, signal: AbortSignal.timeout(TIMEOUT_MS) },
  );
  const objective = completionText(message);
  if (!objective) return;
  pi.appendEntry(SURFACE, {
    v: ENTRY_VERSION,
    dispatch,
    objective,
  });
}

export default function wakuTaskDigest(pi: ExtensionAPI) {
  pi.registerCommand(SURFACE, {
    description: "Waku: describe what this task is for",
    handler: async (args, ctx) => {
      // The engine this runs in is the task's own session, so a throw here
      // would travel the transport as this session's failure. A generation
      // that cannot happen is not news: it leaves nothing behind.
      try {
        await publishDigest(pi, ctx, args.trim());
      } catch {
        // Silent by design.
      }
    },
  });
}
