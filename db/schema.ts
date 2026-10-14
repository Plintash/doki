/**
 * Waku local state schema.
 *
 * Drizzle is a build-time tool here: `bun run db:generate` diffs this file and
 * writes plain SQL into `db/migrations`, which the Rust app applies at startup
 * (see `apply_migrations` in `src/persistence.rs`). drizzle-orm never ships in
 * the binary — Rust owns every query.
 *
 * Session history is kept out of the `sessions` row: the row holds only what
 * the session list renders, so listing is a scan over narrow rows. The
 * transcript lives in `session_details` and messages in `messages`, both
 * fetched only when a session is opened.
 */

import { index, integer, sqliteTable, text } from "drizzle-orm/sqlite-core";

export const projects = sqliteTable("projects", {
  id: text("id").primaryKey(),
  name: text("name").notNull(),
  path: text("path").notNull(),
  /** Order shown in the sidebar. */
  position: integer("position").notNull(),
  /** When the project was added, unix seconds. */
  createdAt: integer("created_at").notNull(),
});

export const sessions = sqliteTable(
  "sessions",
  {
    id: text("id").primaryKey(),
    projectId: text("project_id").notNull(),
    /** The task this conversation is filed under, when the user filed it. */
    taskId: text("task_id"),
    /** Distinguishes persisted issue drafts from unloaded history. */
    isDraft: integer("is_draft", { mode: "boolean" }).notNull().default(false),
    /** Compact SessionWorkspace JSON for accurate unloaded conversation rows. */
    workspace: text("workspace").notNull().default('{"kind":"local"}'),
    /** Explicit user title; "New task" means the automatic fallback is active. */
    title: text("title").notNull(),
    /** Provider-generated title, with the first prompt as a local fallback. */
    autoTitle: text("auto_title"),
    provider: text("provider").notNull(),
    model: text("model"),
    status: text("status").notNull(),
    /** Session creation time, unix seconds. */
    createdAt: integer("created_at").notNull(),
    /** Any mutation, unix seconds — including title edits and truncation. */
    updatedAt: integer("updated_at").notNull(),
    /** Completion of the most recent assistant turn, unix seconds. */
    lastReplyAt: integer("last_reply_at"),
  },
  (table) => [
    index("sessions_by_project").on(table.projectId, table.updatedAt),
    index("sessions_by_updated_at").on(table.updatedAt),
    index("sessions_by_last_reply_at").on(table.lastReplyAt),
    index("sessions_by_task").on(table.taskId),
  ],
);

/**
 * User-declared tasks: the board's cards.
 *
 * A task exists on its own (it may have no conversations yet), groups any
 * number of sessions, and moves between the fixed board states by user
 * action. The daemon owns the rows so every client sees the same board.
 */
export const tasks = sqliteTable(
  "tasks",
  {
    id: text("id").primaryKey(),
    title: text("title").notNull(),
    description: text("description").notNull().default(""),
    state: text("state").notNull(),
    /** Order within the column, ascending. */
    position: integer("position").notNull(),
    /** Bound workspace, inferred from the first conversation filed under it. */
    projectId: text("project_id"),
    path: text("path"),
    branch: text("branch"),
    worktree: integer("worktree", { mode: "boolean" }).notNull().default(false),
    /** Deferred creation intent; preserves an explicitly selected local checkout. */
    newWorktree: integer("new_worktree", { mode: "boolean" }).notNull().default(false),
    createdAt: integer("created_at").notNull(),
    /** Any mutation, including state moves and title edits. */
    updatedAt: integer("updated_at").notNull(),
    /** When the task moved to `done`. */
    completedAt: integer("completed_at"),
  },
  (table) => [index("tasks_by_state").on(table.state, table.position)],
);

/**
 * Conversation messages, one row each.
 *
 * Split out of `sessions.data` so appending to a long conversation writes one
 * small row instead of rewriting the whole history, and so a message can be
 * read or counted without deserializing a transcript.
 */
export const messages = sqliteTable(
  "messages",
  {
    id: text("id").primaryKey(),
    sessionId: text("session_id").notNull(),
    turnId: text("turn_id"),
    /** Ordinal within the session; conversation order, not wall-clock. */
    position: integer("position").notNull(),
    role: text("role").notNull(),
    content: text("content").notNull(),
    /** User-visible text before provider-facing attachment mentions. */
    displayContent: text("display_content"),
    /** JSON-serialized MessageAttachment array. */
    attachments: text("attachments").notNull().default("[]"),
    /** JSON-serialized MessageAnnotation array. */
    annotations: text("annotations").notNull().default("[]"),
    createdAt: integer("created_at").notNull(),
    streaming: integer("streaming", { mode: "boolean" }).notNull(),
  },
  (table) => [index("messages_by_session").on(table.sessionId, table.position)],
);

/**
 * The rest of `AgentSession` as JSON — transcript blocks, turns, provider
 * cursor.
 *
 * Split from `sessions` because it is large and rarely read: keeping it in the
 * row would mean listing sessions pages through every transcript, and every
 * title edit rewrites a transcript-sized row.
 */
export const sessionDetails = sqliteTable("session_details", {
  sessionId: text("session_id").primaryKey(),
  data: text("data").notNull(),
});
