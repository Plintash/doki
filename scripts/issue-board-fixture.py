#!/usr/bin/env python3
"""Isolated, deterministic issue-board fixture for the Linux debug app.

Never copies a developer database, invokes a provider CLI, or writes outside
this checkout's marker-owned temp directory. The real daemon owns migrations.
"""
from __future__ import annotations

import argparse
import contextlib
import fcntl
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
TEMP = ROOT / "temp"
MARKER = TEMP / ".issue-board-qa-fixture"
MARKER_TEXT = "doki-issue-board-fixture-v1\n" + str(ROOT) + "\n"
DB = TEMP / "app.db"
QA = TEMP / "issue-board-qa"
PROVIDERS = ["amp", "claude", "codex", "cursor", "deepSeek", "fx", "openCode", "grok", "kimi", "ohMyPi", "pi"]
STAMP = 1791892800  # 2026-10-13 12:00 UTC; fixture chronology never depends on now.


def uid(kind: int, number: int) -> str:
    return f"9e5e{kind:04x}-0000-4000-8000-{number:012x}"


def guard(create: bool = False) -> None:
    if TEMP.is_symlink():
        raise SystemExit("Refusing a symlinked temp directory")
    if not TEMP.exists() and create:
        TEMP.mkdir(mode=0o700)
        MARKER.write_text(MARKER_TEXT)
    if not MARKER.is_file() or MARKER.read_text() != MARKER_TEXT:
        raise SystemExit("Refusing unmarked temp directory; no data was changed")
    if DB.is_symlink():
        raise SystemExit("Refusing a symlinked database")


def write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n")


def environment() -> dict[str, str]:
    # Keep renderer plumbing, never inherited provider credentials or accounts.
    keep = ("DISPLAY", "XAUTHORITY", "WAYLAND_DISPLAY", "LD_LIBRARY_PATH",
            "LIBGL_ALWAYS_SOFTWARE", "GALLIUM_DRIVER", "VK_ICD_FILENAMES",
            "VK_DRIVER_FILES", "MESA_LOADER_DRIVER_OVERRIDE", "RUST_LOG",
            "GPUI_DISABLE_GPU", "GPUI_SCREEN_CAPTURE", "FONTCONFIG_FILE",
            "FONTCONFIG_PATH")
    env = {name: os.environ[name] for name in keep if name in os.environ}
    env.update(HOME=str(QA / "home"), XDG_CONFIG_HOME=str(QA / "config"),
               XDG_DATA_HOME=str(QA / "data"), XDG_CACHE_HOME=str(QA / "cache"),
               XDG_RUNTIME_DIR=str(QA / "runtime"), TMPDIR=str(QA / "tmp"),
               PATH="/usr/bin:/bin", SHELL=str(QA / "safe-shell"), LANG="C.UTF-8",
               GIT_CONFIG_NOSYSTEM="1", GIT_TERMINAL_PROMPT="0",
               RUST_BACKTRACE="1", WAKU_DAEMON_TOKEN="fixture-only-local-capability",
               WAKU_APP_EXECUTABLE=str(ROOT / "target/debug/waku"))
    return env


def git(path: Path, *args: str) -> None:
    env = environment()
    env.update(GIT_AUTHOR_NAME="QA Fixture", GIT_AUTHOR_EMAIL="fixture@example.invalid",
               GIT_COMMITTER_NAME="QA Fixture", GIT_COMMITTER_EMAIL="fixture@example.invalid",
               GIT_AUTHOR_DATE="2026-10-13T12:00:00+00:00", GIT_COMMITTER_DATE="2026-10-13T12:00:00+00:00")
    subprocess.run(["/usr/bin/git", "-C", str(path), *args], env=env,
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)


def prepare() -> None:
    guard(create=True)
    for folder in ["home/.waku", "config", "data", "cache", "runtime", "tmp", "logs", "screenshots", "projects"]:
        (QA / folder).mkdir(parents=True, exist_ok=True, mode=0o700)
    # Daemon shell-env discovery can only capture this fixture environment.
    # Ignore its login-shell flags instead of loading any host shell profiles.
    safe_shell = QA / "safe-shell"
    safe_shell.write_text('#!/bin/sh\nexec /usr/bin/env -0 > "$WAKU_SHELL_ENV_CAPTURE_FILE"\n')
    safe_shell.chmod(0o700)
    no_provider = QA / "provider-cli-not-installed"
    if no_provider.exists():
        raise SystemExit("Refusing fixture: provider blocker path must not exist")
    write_json(QA / "home/.waku/settings.json", {
        "computer_use_enabled": False, "computer_use_allowed_apps": [],
        "disabled_providers": PROVIDERS,
        "provider_binary_overrides": {name: str(no_provider) for name in PROVIDERS},
    })
    for name, content in [("orion", "# Orion\n\nSynthetic issue-board QA project.\n"),
                          ("atlas-docs", "# Atlas docs\n\nSynthetic documentation QA project.\n")]:
        path = QA / "projects" / name
        path.mkdir(exist_ok=True)
        if not (path / ".git").exists():
            git(path, "init", "--initial-branch=main")
            (path / "README.md").write_text(content)
            git(path, "add", "README.md")
            git(path, "commit", "-m", "Add deterministic QA fixture")
    worktree = QA / "projects/orion-issue-navigation"
    if not worktree.exists():
        git(QA / "projects/orion", "worktree", "add", "-b", "feature/issue-navigation", str(worktree))
    print(f"Prepared isolated fixture: {TEMP}", flush=True)


def check_processes_stopped() -> None:
    running = QA / "running.json"
    if running.exists():
        # PID namespaces can differ between tool invocations, so absence from
        # this /proc does not prove that another invocation's app has stopped.
        raise SystemExit(f"Refusing while {running} exists; stop its owning runner first")


@contextlib.contextmanager
def daemon():
    guard()
    binary = ROOT / "target/debug/waku-daemon"
    if not binary.is_file():
        raise SystemExit(f"Build the debug daemon first: {binary}")
    with (QA / "logs/daemon.stdout.log").open("w+") as stdout, (QA / "logs/daemon.stderr.log").open("w") as stderr:
        proc = subprocess.Popen([str(binary), "--bind", "127.0.0.1:0", "--parent-pid", str(os.getpid())],
                                cwd=QA, env=environment(), stdout=stdout, stderr=stderr)
        try:
            deadline = time.monotonic() + 30
            ready = None
            while time.monotonic() < deadline:
                if proc.poll() is not None:
                    raise RuntimeError(f"Daemon exited ({proc.returncode}); inspect {QA / 'logs/daemon.stderr.log'}")
                stdout.seek(0)
                line = stdout.readline()
                if line:
                    ready = json.loads(line)
                # Ready stdout precedes backend initialization; check migrations too.
                expected = {path.stem for path in (ROOT / "db/migrations").glob("*.sql")}
                if ready and DB.exists():
                    try:
                        with sqlite3.connect(f"file:{DB}?mode=ro", uri=True) as db:
                            applied = {row[0] for row in db.execute("SELECT tag FROM migrations")}
                        if expected <= applied:
                            break
                    except sqlite3.OperationalError:
                        pass
                time.sleep(0.05)
            else:
                raise RuntimeError("Daemon did not apply all source migrations; rebuild the latest daemon")
            yield proc, ready
        finally:
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()


def seed() -> None:
    guard()
    check_processes_stopped()
    with daemon():
        pass  # Use the actual compiled apply_migrations path, then close its DB.
    projects = [(uid(1, 1), "Orion", QA / "projects/orion"),
                (uid(1, 2), "Atlas docs", QA / "projects/atlas-docs")]
    worktree = str(QA / "projects/orion-issue-navigation")
    tasks = [
        (uid(2, 1), "Clarify empty-state guidance", "backlog", None, None, None, 0,
         "Explain what happens before the first conversation.\n\nAcceptance criteria:\n- Keep the issue visible with zero conversations\n- Let the issue choose project context later"),
        (uid(2, 2), "Keep issue context across conversations", "doing", uid(1, 1), worktree, "feature/issue-navigation", 1,
         "Keep the project and isolated worktree attached to the issue.\n\nAcceptance criteria:\n- Open the issue without jumping into chat\n- Start a follow-up in the same worktree\n- Keep the review and implementation together"),
        (uid(2, 3), "Publish contributor checklist", "done", uid(1, 2), None, None, 0,
         "Document the contributor checks and keep the finished discussion readable.\n\nCompleted: the checklist has been reviewed and approved for this synthetic fixture."),
    ]
    conversations = [
        (uid(3, 1), uid(1, 1), uid(2, 2), "Explore issue navigation", "pi", "gpt-5.4", "worktree", "Sketch the issue-first navigation flow", "The issue detail should show its summary, workspace, and related conversations before a chat is selected."),
        (uid(3, 2), uid(1, 1), uid(2, 2), "Review workspace inheritance", "pi", "gpt-5.4", "worktree", "Review how a follow-up inherits the workspace", "The follow-up should retain Orion and feature/issue-navigation. This transcript is a static, idle QA fixture."),
        (uid(3, 3), uid(1, 2), uid(2, 3), "Review contributor checklist", "pi", "gpt-5.4", "local", "Review the contributor checklist", "The synthetic checklist is complete. Its discussion remains available from the completed issue."),
        (uid(3, 4), uid(1, 2), None, "Unfiled documentation question", "pi", "gpt-5.4", "local", "Where should general documentation questions go?", "Leave exploratory conversations unfiled until they belong to a concrete issue."),
    ]
    with sqlite3.connect(DB) as db:
        if "description" not in {row[1] for row in db.execute("PRAGMA table_info(tasks)")}:
            raise SystemExit("The description migration is missing; rebuild the updated daemon and seed again")
        has_workspace = "workspace" in {row[1] for row in db.execute("PRAGMA table_info(sessions)")}
        for table in ["projects", "tasks", "sessions"]:
            if db.execute(f"SELECT COUNT(*) FROM {table} WHERE id NOT LIKE '9e5e%' ").fetchone()[0]:
                raise SystemExit(f"Refusing to reseed: non-fixture rows exist in {table}")
        for table in ["messages", "session_details", "sessions", "tasks", "projects"]:
            db.execute(f"DELETE FROM {table}")
        for position, (pid, name, path) in enumerate(projects):
            db.execute("INSERT INTO projects(id,name,path,position,created_at) VALUES(?,?,?,?,?)",
                       (pid, name, str(path), position, STAMP))
        for position, (tid, title, state, pid, path, branch, is_worktree, description) in enumerate(tasks):
            db.execute("INSERT INTO tasks(id,title,description,state,position,project_id,path,branch,worktree,created_at,updated_at,completed_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                       (tid, title, description, state, position, pid, path, branch, is_worktree,
                        STAMP - 3600, STAMP + position * 60, STAMP + 120 if state == "done" else None))
        for index, (sid, pid, tid, title, provider, model, workspace, prompt, reply) in enumerate(conversations):
            created, updated = STAMP + index * 600, STAMP + index * 600 + 120
            turn_id = uid(4, index + 1)
            detail = dict(id=sid, title=title, project_id=pid, task_id=tid,
                          provider=provider, model=model, runtime_mode="ask", status="idle",
                          created_at=created, updated_at=updated, last_reply_at=updated,
                          provider_cursor=None, transcript_blocks=[], queued_messages=[],
                          turns=[dict(id=turn_id, turn_count=1, status="completed",
                                      provider_turn_started=True, started_at=created, completed_at=updated)])
            if workspace == "worktree":
                detail["workspace"] = dict(kind="worktree", path=worktree, branch="feature/issue-navigation")
            db.execute("INSERT INTO sessions(id,project_id,task_id,title,auto_title,provider,model,status,created_at,updated_at,last_reply_at) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
                       (sid, pid, tid, title, None, provider, model, "idle", created, updated, updated))
            db.execute("INSERT INTO session_details(session_id,data) VALUES(?,?)", (sid, json.dumps(detail)))
            if has_workspace:
                db.execute("UPDATE sessions SET workspace=? WHERE id=?", (json.dumps(detail.get("workspace", {"kind": "local"})), sid))
            for position, (role, content) in enumerate([("user", prompt), ("assistant", reply)]):
                db.execute("INSERT INTO messages(id,session_id,turn_id,position,role,content,created_at,streaming) VALUES(?,?,?,?,?,?,?,0)",
                           (uid(5, index * 2 + position + 1), sid, turn_id, position, role, content, created + position * 120))
        draft_id = uid(3, 5)
        draft = dict(id=draft_id, title="Unsent implementation follow-up", project_id=uid(1, 1),
                     task_id=uid(2, 2), provider="pi", model="gpt-5.4", runtime_mode="ask",
                     status="idle", is_draft=True, created_at=STAMP + 3000, updated_at=STAMP + 3000,
                     provider_cursor=None, turns=[], transcript_blocks=[], queued_messages=[],
                     workspace=dict(kind="worktree", path=worktree, branch="feature/issue-navigation"))
        db.execute("INSERT INTO sessions(id,project_id,task_id,title,provider,model,status,created_at,updated_at,is_draft) VALUES(?,?,?,?,?,?,?,?,?,1)",
                   (draft_id, uid(1, 1), uid(2, 2), draft["title"], "pi", "gpt-5.4", "idle", STAMP + 3000, STAMP + 3000))
        db.execute("INSERT INTO session_details(session_id,data) VALUES(?,?)", (draft_id, json.dumps(draft)))
        if has_workspace:
            db.execute("UPDATE sessions SET workspace=? WHERE id=?", (json.dumps(draft["workspace"]), draft_id))
        assert db.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
    write_json(TEMP / "composer-drafts.json", {"sessions": {uid(3, 5): {
        "text": "Review the inherited Orion worktree before implementing the next change. This draft has never been sent."}}})
    write_json(TEMP / "app.json", {"theme": "dark", "language": "english", "ui_font_size": 14.0, "code_font_size": 13.0})
    write_json(TEMP / "state.json", {"app_state_version": 1, "selected_project": uid(1, 1),
               "selected_session": None, "last_provider": "pi", "last_runtime_mode": "ask",
               "sidebar_grouping": "task", "sidebar_grouping_chosen": True,
               "window_state": {"x": 40, "y": 40, "width": 1280, "height": 860}})
    write_json(QA / "manifest.json", {"database": str(DB), "timestamp": STAMP,
               "projects": {name: pid for pid, name, _ in projects},
               "issues": {row[1]: row[0] for row in tasks},
               "conversations": {**{row[3]: row[0] for row in conversations}, "Unsent implementation follow-up": uid(3, 5)}})
    print("Seeded 2 projects, 3 issues, 4 idle conversations + 1 durable unsent draft, 8 messages; no provider cursor or queued prompt", flush=True)


def verify(baseline: bool = True) -> None:
    from websockets.sync.client import connect
    guard()
    check_processes_stopped()
    with daemon() as (_, ready):
        with connect("ws://" + ready["address"] + "/v1", open_timeout=5, proxy=None) as ws:
            ws.send(json.dumps(dict(type="hello", protocolVersion=ready["protocolVersion"],
                                    token=environment()["WAKU_DAEMON_TOKEN"], clientId=uid(6, 1))))
            assert json.loads(ws.recv())["type"] == "hello"
            def rpc(command: dict, session_id: str = str(uuid.UUID(int=0))) -> dict:
                rid = str(uuid.uuid4())
                ws.send(json.dumps(dict(type="request", requestId=rid, sessionId=session_id,
                                        runtimeId=str(uuid.UUID(int=0)), command=command)))
                while True:
                    response = json.loads(ws.recv())
                    if response.get("requestId") == rid:
                        assert response["outcome"]["status"] == "ok", response
                        return response["outcome"]["payload"]
            state = rpc(dict(type="loadTaskState"))
            assert len(state["projects"]) == 2, state
            if baseline:
                assert len(state["tasks"]) == 3 and len(state["sessions"]) == 5, state
                assert {row["state"] for row in state["tasks"]} == {"backlog", "doing", "done"}
            else:
                assert len(state["tasks"]) >= 3 and len(state["sessions"]) >= 5, state
                assert {row["state"] for row in state["tasks"]} <= {"backlog", "doing", "done"}
            message_count = 0
            for session in state["sessions"]:
                hydrated = rpc(dict(type="hydrateSession", sessionId=session["id"]))["session"]
                assert hydrated["status"] == "idle" and len(hydrated["messages"]) == (2 if session["id"] in {uid(3, i) for i in range(1, 5)} else 0)
                message_count += len(hydrated["messages"])
                assert not hydrated.get("provider_cursor") and not hydrated.get("queued_messages")
                attached = rpc(dict(type="attachSession"), session["id"])
                assert attached.get("runtimeId") is None, attached
            assert message_count == 8, "An unexpected message was added during UI-only QA"
            drafts = rpc(dict(type="loadComposerDrafts"))["drafts"]
            assert drafts["sessions"][uid(3, 5)]["text"].startswith("Review the inherited Orion worktree")
            draft = next(row for row in state["sessions"] if row["id"] == uid(3, 5))
            assert draft["is_draft"] and draft["task_id"] == uid(2, 2)
            write_json(QA / ("verified-state.json" if baseline else "verified-current-state.json"), state)
    print("Verified through real daemon RPC: all fixture rows decode, transcripts hydrate, zero provider runtimes", flush=True)


def run() -> None:
    guard()
    check_processes_stopped()
    binary = ROOT / "target/debug/waku"
    if not binary.is_file():
        raise SystemExit("Build target/debug/waku first")
    if not os.environ.get("DISPLAY") and not os.environ.get("WAYLAND_DISPLAY"):
        raise SystemExit("Source the verified private display environment first")
    signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt()))
    with daemon() as (server, ready):
        env = environment()
        env["WAKU_DAEMON_ADDRESS"] = ready["address"]
        with (QA / "logs/app.log").open("w") as log:
            app = subprocess.Popen([str(binary)], cwd=QA, env=env, stdout=log, stderr=log)
            write_json(QA / "running.json", dict(runner=os.getpid(), daemon=server.pid, app=app.pid,
                                                address=ready["address"], database=str(DB)))
            print(f"Fixture app PID {app.pid}; daemon PID {server.pid}; logs: {QA / 'logs'}", flush=True)
            try:
                code = app.wait()
                if code:
                    raise RuntimeError(f"App exited {code}; inspect {QA / 'logs/app.log'}")
            finally:
                if app.poll() is None:
                    app.terminate()
                    try:
                        app.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        app.kill()
                        app.wait()
                (QA / "running.json").unlink(missing_ok=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["prepare", "seed", "verify", "verify-current", "run"])
    args = parser.parse_args()
    guard(create=args.command == "prepare")
    QA.mkdir(exist_ok=True, mode=0o700)
    with (QA / "fixture.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise SystemExit("Another fixture operation is active; stop it before continuing")
        check_processes_stopped()
        try:
            {"prepare": prepare, "seed": seed, "verify": verify, "verify-current": lambda: verify(False), "run": run}[args.command]()
        except KeyboardInterrupt:
            print("Stopped fixture app and its daemon", flush=True)



if __name__ == "__main__":
    main()
