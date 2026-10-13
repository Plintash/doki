# Session titles

How each session in the sidebar gets its name: who writes the title, how Waku
learns about it, and when.

The governing rule is that **the provider names its own session wherever it
can**. Every coding agent except Codex already generates a title for its own
UI, on its own cheap model, as part of work it is doing anyway. Waku's job is
to find that title and show it promptly — not to pay for a second opinion.
Generating one ourselves is the exception, and it exists for exactly one
provider that does not.

## The two title fields

[`AgentSession`](../crates/waku-protocol/src/model.rs) carries two:

| Field | Owner | Set by |
| --- | --- | --- |
| `title` | the user | inline rename; wins whenever it differs from `DEFAULT_TITLE` |
| `auto_title` | the provider | `DriverEvent::AutoTitleUpdated`, and the local fallback below |

[`display_title`](../crates/waku-protocol/src/model.rs#L940) resolves them:
an explicit `title` first, then `auto_title`, then `DEFAULT_TITLE`
(`"New task"`, [model.rs:841](../crates/waku-protocol/src/model.rs#L841)).
A provider title therefore never overwrites a name the user typed.

### The local fallback

[`set_title_from_prompt`](../crates/waku-protocol/src/model.rs#L964) takes the
**first seven words** of the first prompt, capped at 54 characters, and writes
them into `auto_title`. It is called once per session from
[runtime.rs:2949](../src/app/runtime.rs#L2949) and no-ops if the session
already has a second message, a user title, or any `auto_title`.

This is a placeholder, not a title. It shares the `auto_title` field precisely
so the real one replaces it silently when it arrives — which makes **latency
the thing that matters**. A provider title that lands after the turn ends is,
from the user's side, indistinguishable from no title at all: they stare at
truncated prompt text for the entire run. Every path below is judged on how
fast it replaces that placeholder, not merely on whether it eventually does.

## The objective beside the title

A task also carries [`AgentSession::objective`](../crates/waku-protocol/src/model.rs):
one sentence describing what will be true when the task is done. It is a field
of its own on purpose. `title` is the anchor a user recognizes a task by, so it
has to hold still; the objective is allowed to change as the work changes, and a
field that changed could not do the title's job. Nothing about the objective
writes a title, and nothing about a title writes the objective.

The sidebar row and the card show it (`task-digest` spec). The value that
reaches a client is resolved in one place — a goal the user set wins over the
generated text (`AgentSession::resolved_objective`) — while the row's second
line prefers the provider's own plan step whenever the live turn has one, so a
sentence about the turn that just ended never passes itself off as progress.

Waku generates the objective for **Pi only**. Codex, Claude, and the rest have
no plan visible to Waku and no title they need help with; a task there renders
with no objective, which is a gap in a second line, not a missing task.

### The mechanism: the task's own provider process

The objective is generated *inside* the Pi process that already serves the task,
not by a second one. Waku ships a Pi extension
([`resources/pi-extensions/task-digest.ts`](../resources/pi-extensions/task-digest.ts),
bundled into `Resources/pi-extensions/task-digest.ts` by
[`scripts/bundle.sh`](../scripts/bundle.sh) and handed to Pi with the same
`--extension` argument the Computer Use bridge uses), and the daemon asks it a
question over the RPC pipe it already owns.

The trigger is a **command**, and the shape of it is the whole trick:

```
-> {"type":"prompt","message":"/waku:digest <dispatch-uuid>"}
<- {"type":"response","success":true,"data":{"disposition":"handled"}}
```

- Pi dispatches an extension command immediately, *even while the task's turn is
  still streaming*, and answers `disposition: "handled"` — no run starts, so the
  task gains no turn, no message, and no instruction in its own conversation.
  Verified against Pi 1.0.0: a live test
  ([`pi_answers_the_digest_trigger_without_a_run_against_the_real_rpc`](../crates/waku-core/src/driver/pi.rs))
  asserts a trigger that opens no turn, settles none, writes nothing to the
  transcript and adds nothing to the context, and a trigger sent into a
  streaming run was answered `{"disposition":"handled"}` with the entry right
  behind it.
- The dispatch id in the message is what makes the trigger internal. Only the
  daemon writes a prompt that names one, so the driver can tell its own question
  from a submission (`task_digest::trigger_dispatch`): a person typing
  `/waku:digest` takes the ordinary prompt path, and a session whose provider
  never reported the command is never sent a trigger at all — there it would be
  an ordinary prompt, and a real turn.
- The extension reads the live branch with `ctx.sessionManager.getBranch()`, cuts
  a bounded slice of it, and makes one nested call through
  `ctx.modelRegistry.complete(...)` — provider-neutral, on the session's own
  model with the instance's own credentials, with a 60-second abort. It does not
  touch the task's model, session, or transcript.
- The result is published with `pi.appendEntry("waku:digest", {v: 1, dispatch,
  objective})`: a `custom` session entry, which Pi **does not send to the
  model**, and which arrives on the RPC stream as `entry_appended`. That event is
  how the sentence gets back to Waku; the entry never joins the conversation.

Everything Waku decides about the sentence happens on the Rust side
([`crates/waku-core/src/task_digest.rs`](../crates/waku-core/src/task_digest.rs)),
not in the extension: the extension returns raw text, and the parser is the one
copy of the rules.

### When it runs

The daemon triggers on **settlement**, not on activity, not on a timer, and not
when a task is opened or resumed — only a turn that announced itself can settle
(a refusal and a handled command answer a prompt with no run behind them, and
neither is work to describe). On top of that:

| Rule | Value |
| --- | --- |
| Quiet period after a settle | 15 s, replaced by a newer settle |
| Generations per task per hour | 6 |
| Timeout per generation | 60 s |
| Back-fill over stored history | none, ever |

A failure is silent: a generation that errors, times out, or produces something
the parser rejects leaves the previous objective in place and shows nothing — no
error, no transcript row, no idle task that looks busy.

### What the parser rejects

The objective states an outcome, so a sentence that names the means is not
stored. Against Pi's own generated text and a hostile fixture alike, these keep
the objective the task already had:

- a path separator (`src/app/sidebar.rs`), a file extension (`schema.ts`), or a
  symbol (`render_sidebar_session_item`, `RenderSidebarSessionItem`);
- a sentence that restates the title, and one that says what the stored text
  already says — the same significant-word overlap decides both, because an
  objective that churns between two wordings of the same thing is worse than a
  stale one;
- a task whose goal the user set: the goal owns the field while it is set, and
  the generated value is what a cleared goal falls back to.

Only a changed objective is written, and only a write publishes a task-catalog
revision, so a regeneration that decides nothing costs no client a reload. The
update never touches `updated_at`, so a task does not jump to the top of the
list because it was described.

## The three delivery shapes

Every provider funnels into `DriverEvent::AutoTitleUpdated(Option<String>)`,
consumed once in [streaming.rs:258](../src/app/streaming.rs#L258) →
`set_auto_title`, which trims, maps empty to `None`, and is the universal
last-stage normalizer. `AutoTitleUpdated` is in the `force_save` set
([runtime.rs](../src/app/runtime.rs)), so a title persists the moment it lands.

What differs is how the title reaches that event:

1. **Pushed on the provider's own stream.** The agent sends a title event and
   the driver forwards it. Nothing to poll, nothing to schedule. OpenCode, Pi,
   Oh My Pi, Kimi Code, DeepSeek, and Codex's native paths work this way.
2. **Polled from a native store.** The agent writes the title to disk (or to a
   store reachable by CLI) but never puts it on the wire. The driver polls with
   [`NativeTitleRefresh`](../crates/waku-core/src/driver/title_refresh.rs) on a
   backoff schedule, latching `RESOLVED` on the first hit. Claude, Amp, and
   Grok work this way.
3. **Generated by Waku.** Only Codex.

### `NativeTitleRefresh`

A single `AtomicU8` — `IDLE` / `RUNNING` / `RESOLVED`. `start()` spawns a named
thread that walks a `Vec<Duration>`, sleeping each delay before its lookup, and
on the first non-empty result emits the event and latches `RESOLVED` for the
life of the driver. A schedule that runs dry returns to `IDLE`, so the next
trigger re-arms it. Concurrent `start()` calls are no-ops.

The schedule is the whole design decision: it must be dense enough to catch the
title near when the provider writes it, and long enough to survive a slow start.

## Per provider

| Provider | Author | Transport | Trigger | Emit site |
| --- | --- | --- | --- | --- |
| Claude Code | Claude (Haiku 4.5) | transcript JSONL on disk | poll from each prompt, plus turn end | [title_refresh.rs:53](../crates/waku-core/src/driver/title_refresh.rs#L53), [claude.rs:1300](../crates/waku-core/src/driver/claude.rs#L1300) |
| Codex CLI | **Waku** (`gpt-5.6-luna`, effort `low`) | second `codex app-server` process | first `turn/started`, new threads only | [codex.rs:636](../crates/waku-core/src/driver/codex.rs#L636) |
| Codex CLI | Codex | JSON-RPC stream | thread start/resume, `thread/name/updated` | [codex.rs:1612](../crates/waku-core/src/driver/codex.rs#L1612), [codex.rs:1735](../crates/waku-core/src/driver/codex.rs#L1735) |
| Amp | Amp | `amp threads list --json` subprocess | poll at turn end | [title_refresh.rs:53](../crates/waku-core/src/driver/title_refresh.rs#L53) |
| Grok Build | Grok | `summary.json` on disk | poll at turn end | [title_refresh.rs:53](../crates/waku-core/src/driver/title_refresh.rs#L53) |
| OpenCode | OpenCode | SSE stream | `session.updated` | [opencode.rs:894](../crates/waku-core/src/driver/opencode.rs#L894) |
| Pi | Pi | NDJSON stream | connect, `session_info_changed` | [pi.rs:472](../crates/waku-core/src/driver/pi.rs#L472), [pi.rs:1214](../crates/waku-core/src/driver/pi.rs#L1214) |
| Oh My Pi | Oh My Pi | NDJSON stream | connect, `session_info_update` | [pi.rs:472](../crates/waku-core/src/driver/pi.rs#L472), [pi.rs:1214](../crates/waku-core/src/driver/pi.rs#L1214) |
| DeepSeek | Harness | stream + projections | `session/title`, projection replay | [deepseek.rs:782](../crates/waku-core/src/driver/deepseek.rs#L782), [deepseek.rs:1139](../crates/waku-core/src/driver/deepseek.rs#L1139) |
| Kimi Code | Kimi (placeholder) | ACP stream | `session_info_update` | [acp.rs:1305](../crates/waku-core/src/driver/acp.rs#L1305) |
| Cursor CLI | — | — | — | none; fallback only |

### Claude Code

Claude Code titles the session itself and **already uses Haiku 4.5** for it.
Verified against the real CLI (2.1.228) with `--debug api`: the run issues two
requests within the same millisecond, one for the turn and one for the title.

```
[API:timing] dispatching to firstParty model=claude-haiku-4-5-20251001
[API:timing] dispatching to firstParty model=claude-opus-5
[API REQUEST] /v1/messages … source=generate_session_title
[API REQUEST] /v1/messages … source=sdk
```

The title call is fired in parallel with the turn's own first model call, so it
does not wait on the turn. Measured end to end, the title reaches disk **~2.9s
after the prompt**.

The model is the CLI's Haiku tier, and setting `ANTHROPIC_DEFAULT_HAIKU_MODEL`
moves the title call with it — verified by running with
`ANTHROPIC_DEFAULT_HAIKU_MODEL=claude-sonnet-5`, which dispatched
`generate_session_title` to Sonnet while the turn stayed on Opus. Waku does not
set it: the default is already the cheapest current Haiku, so there is nothing
to pin.

**Claude does not title every session.** A first prompt shorter than **10
characters** never fires `generate_session_title` at all, so no `ai-title` is
ever written and no amount of polling will find one. Measured on 2.1.228 by
probing prompt lengths: 9 characters fires nothing, 10 fires the request.
`"1+1 is?"` is 7, so such a session keeps the truncated-prompt fallback
permanently — which is the right answer anyway, since the whole prompt already
fits in the row. Treat a missing title on a very short prompt as correct
behavior, not a regression.

Claude never puts that title on the stream — it writes it to the native
transcript, `~/.claude/projects/*/<session_id>.jsonl` (or under
`$CLAUDE_CONFIG_DIR`), as a standalone entry:

```json
{"type":"ai-title","aiTitle":"Refactor sidebar row builder allocations","sessionId":"…"}
```

[`claude_session::session_metadata`](../crates/waku-core/src/claude_session.rs#L27)
returns the last `ai-title` (`aiTitle`) or `custom-title` (`customTitle`) entry.
`custom-title` is the explicit one — a rename inside Claude Code, or the title
Waku writes when it forks a session — and it wins by being later in the file.

Waku reads it on two paths:

- [`start_claude_title_refresh`](../crates/waku-core/src/driver/claude.rs#L124),
  armed from the writer thread on **every prompt**, polling at `5s, 10s` —
  attempts land at 5s and 15s, since each delay is slept before its lookup.
  The ~2.9s write is already there on the first look; the second is insurance
  for a slow start.
- The turn-end read at [claude.rs:1295](../crates/waku-core/src/driver/claude.rs#L1295),
  on the `result` message, deduplicated against `last_auto_title`. This one
  also carries the rewind cursor, and it is what picks up a *later* retitle,
  since the poll latches after its first success.

The turn-end read used to be the only path. Because Claude settles `result`
only when the whole turn is done, an agentic first turn showed the truncated
prompt for its entire run, and an interrupted one never got a title at all —
even though the real title had been sitting on disk since second three.

### Codex — the one Waku generates

The Codex app-server **persists** a thread name but never generates one; naming
is the client's job, and Codex Desktop does it client-side too. So Waku matches
that behavior in
[`generate_codex_title`](../crates/waku-core/src/driver/codex.rs#L1006): a
second, short-lived `codex app-server --stdio` process runs one ephemeral,
read-only turn.

- Model `gpt-5.6-luna` ([codex.rs:958](../crates/waku-core/src/driver/codex.rs#L958)),
  `effort: "low"`, `summary: "none"` — the cheapest tier, since a title is a
  fixed classification.
- `CODEX_TITLE_INSTRUCTIONS` is passed as both `baseInstructions` and
  `developerInstructions`; the turn's only input is the raw user prompt.
- An `outputSchema` pins `{title: string}`, max 80 chars.
- `ephemeral: true`, so it leaves no thread behind. 60s timeout.
- Gated to sessions Codex did not already name (`enabled: provider_session_id
  .is_none()`) and to one run per driver.

The result is emitted as `AutoTitleUpdated` **before** being written back to
Codex with `thread/name/set`, so the sidebar updates even if that write fails.
Output is cleaned by `normalize_codex_title` — it decodes JSON or `{title}`,
drops code fences, strips a `Title:` prefix, trims quotes and markdown emphasis,
collapses whitespace, and caps at 80 characters.

### Amp

Amp generates the title but keeps it off the streaming channel entirely, so
[`amp_session::thread_title`](../crates/waku-core/src/amp_session.rs#L49) shells
out to `amp threads list --json --include-archived --limit 100` and matches the
thread id. Polled at `500ms, 2s` — armed at **turn end**, not at prompt time,
because Amp does not name a thread until it has produced something.

### Grok Build

Grok writes `<grok_home>/sessions/*/<session_id>/summary.json` with a
`generated_title` field ([grok_session.rs:22](../crates/waku-core/src/grok_session.rs#L22)).
Polled at `0, 250ms, 750ms, 1.5s, 3s, 5s, 7.5s, 10s`, armed at turn end from
either the xAI `_x.ai/session/prompt_complete` notification or the ACP prompt
result. `grok_home` follows `$GROK_HOME`, or the isolated home the Computer Use
runtime supplies.

### OpenCode, Pi, DeepSeek

All three push titles on their own streams, so there is nothing to schedule.

- **OpenCode** — `session.updated` → `properties./info/title`. Titles beginning
  with `"New session - "` are dropped ([opencode.rs:892](../crates/waku-core/src/driver/opencode.rs#L892)):
  that is OpenCode's own placeholder, and letting it through would replace
  Waku's prompt fallback with something strictly less useful.
- **Pi and Oh My Pi** — `/data/sessionName` at connect, then the stream event.
  The connect read is shared, but the event is not: Pi sends
  `session_info_changed` with the title under `name`, Oh My Pi sends
  `session_info_update` with it under `title`, and `PiFlavor` resolves both
  ([pi.rs:69](../crates/waku-core/src/driver/pi.rs#L69)). Either may send an
  empty value, which becomes `AutoTitleUpdated(None)` and clears the title back
  to the fallback.
- **DeepSeek** — the Harness `session/title` event, plus a `title` projection
  replayed from history at connect, so a reattached session gets its title
  without waiting for a new one.

### Kimi Code

Kimi is the first provider to actually exercise the generic ACP
`session_info_update` handler
([acp.rs:1305](../crates/waku-core/src/driver/acp.rs#L1305)): it emits one at
the start of the first turn and Waku forwards the `title` as `AutoTitleUpdated`,
no polling and no extra process.

What it sends, though, is **the first prompt verbatim** — the same content as
Waku's own fallback, arriving through a different door. Kimi files it in its
session state as `titleKind: "replaceable"`, so it treats the prompt as a
placeholder it may overwrite later; no model-generated replacement has been
observed yet, and every failed turn keeps the placeholder. Nothing is lost
either way — the fallback would have shown the same words — but do not read a
Kimi title as evidence that an agent summarized anything.

### Cursor

Cursor has no title path. It shows the truncated-prompt fallback for the life
of the session. It shares the `session_info_update` handler Kimi uses, but there
is no evidence in Cursor's protocol traffic that it ever sends one.

## Adding a provider

In order of preference:

1. **Does the agent already generate a title?** Nearly all do — for their own
   UI, on their own cheap model. Find where it surfaces before writing any
   model call.
2. **Does it push that title on the stream?** Then forward it and stop. Prefer
   trimming and dropping the provider's own placeholder strings, as OpenCode
   does.
3. **Does it only write the title to a native store?** Use `NativeTitleRefresh`.
   Measure when the title actually lands and pick a schedule that brackets it —
   do not guess. Arm it from the earliest point the title could exist: prompt
   time if the agent titles eagerly (Claude), turn end if it titles from the
   response (Amp, Grok).
4. **Only if the agent genuinely never generates one**, generate it, following
   the Codex shape: cheapest model, lowest effort, an ephemeral session, the
   raw user prompt as the only input, a structured output schema, a timeout,
   once per session, never on resume. Reuse `normalize_codex_title`.

Whatever the path, failure must be silent and the fallback must hold. A missing
title is a cosmetic gap; a driver that errors or blocks on one is not.
