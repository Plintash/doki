#!/usr/bin/env python3
"""Seed duplicate registration metadata inside the marker-owned Board fixture.

Run after issue-board-fixture.py prepare. Refuses non-fixture data, uses the
real daemon's migrations, and never copies a developer database or credentials.
The ordinary scratch API intentionally rewrites every project to a unique path,
so this regression needs a deliberately duplicated isolated native fixture.
"""
import fcntl
import importlib.util
import json
from pathlib import Path
import sqlite3

spec = importlib.util.spec_from_file_location("board_fixture", Path(__file__).with_name("issue-board-fixture.py"))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


def seed():
    """Caller holds fixture.lock across preparation, seeding, and any launch."""
    f.seed()  # Owns marker/row checks and actual daemon migrations.
    paths = [f.QA / "projects/orion", f.QA / "projects/orion",
             f.QA / "projects/other/orion", f.QA / "projects/orion-issue-navigation"]
    for path in paths:
        path.mkdir(parents=True, exist_ok=True)
    projects = [dict(id=f.uid(1, i + 1), name="Repeated project" if i < 3 else "Independent worktree",
                     path=str(path), created_at=f.STAMP) for i, path in enumerate(paths)]
    issues = [dict(id=f.uid(2, i + 1), title=f"Project identity issue {i}", description="Synthetic binding to retain",
                   state="backlog", position=i, workspace=dict(kind="local", projectId=project["id"]),
                   created_at=f.STAMP, updated_at=f.STAMP) for i, project in enumerate(projects[:3])]
    turn_id = f.uid(4, 1)
    conversation = dict(id=f.uid(3, 1), title="Existing standalone conversation", project_id=projects[1]["id"],
                        provider="pi", runtime_mode="ask", status="idle", created_at=f.STAMP, updated_at=f.STAMP,
                        provider_cursor=None, transcript_blocks=[], is_draft=False,
                        turns=[dict(id=turn_id, turn_count=1, status="completed", provider_turn_started=True,
                                    started_at=f.STAMP, completed_at=f.STAMP)],
                        workspace=dict(kind="worktree", path=str(paths[3]), branch="feature/issue-navigation"))
    with sqlite3.connect(f.DB) as db:
        for table in ["messages", "session_details", "sessions", "tasks", "projects"]:
            db.execute(f"DELETE FROM {table}")  # f.seed verified marker-owned synthetic rows.
        for position, project in enumerate(projects):
            db.execute("INSERT INTO projects(id,name,path,position,created_at) VALUES(?,?,?,?,?)",
                       (project["id"], project["name"], project["path"], position, f.STAMP))
        for issue in issues:
            db.execute("INSERT INTO tasks(id,title,description,state,position,project_id,worktree,created_at,updated_at) VALUES(?,?,?,?,?,?,0,?,?)",
                       (issue["id"], issue["title"], issue["description"], issue["state"], issue["position"],
                        issue["workspace"]["projectId"], f.STAMP, f.STAMP))
        db.execute("INSERT INTO sessions(id,project_id,title,provider,status,created_at,updated_at,is_draft,workspace) VALUES(?,?,?,?,?,?,?,0,?)",
                   (conversation["id"], conversation["project_id"], conversation["title"], "pi", "idle", f.STAMP, f.STAMP, json.dumps(conversation["workspace"])))
        db.execute("INSERT INTO session_details(session_id,data) VALUES(?,?)", (conversation["id"], json.dumps(conversation)))
        for position, (role, content) in enumerate([("user", "Synthetic project binding to retain"),
                                                   ("assistant", "Static provider-free fixture reply")]):
            db.execute("INSERT INTO messages(id,session_id,turn_id,position,role,content,created_at,streaming) VALUES(?,?,?,?,?,?,?,0)",
                       (f.uid(5, position + 1), conversation["id"], turn_id, position, role, content, f.STAMP))
        assert db.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
    f.write_json(f.TEMP / "composer-drafts.json", {})
    f.write_json(f.TEMP / "state.json", {"app_state_version": 1, "selected_project": projects[1]["id"],
                 "selected_session": conversation["id"], "last_provider": "pi", "last_runtime_mode": "ask",
                 "sidebar_grouping": "task", "sidebar_grouping_chosen": True,
                 "window_state": {"x": 40, "y": 40, "width": 1280, "height": 860}})
    f.write_json(f.QA / "project-dedup.json", dict(checkout=str(f.ROOT), database=str(f.DB), projects=projects, tasks=issues, conversation=conversation))


if __name__ == "__main__":
    f.guard(create=True)
    f.QA.mkdir(exist_ok=True, mode=0o700)
    with (f.QA / "fixture.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise SystemExit("Another fixture operation is active; stop it before continuing")
        f.check_processes_stopped()
        f.prepare()
        seed()
