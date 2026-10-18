#!/usr/bin/env python3
"""Provider-free native issue-review fixture. No provider executable or account is used.

Run from the desktop's terminal with the normal Doki build environment. The
existing marked Board fixture owns the isolated HOME, database and worktrees.
Set temp/issue-board-qa/review-mode to success, refusal, empty, or hold before
sending a review. hold waits for the app's Stop; all outputs are synthetic.
"""
from __future__ import annotations
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import uuid

ROOT = Path(__file__).resolve().parents[1]


def provider(qa: Path) -> None:
    if "--version" in sys.argv:
        print("codex-cli 0.159.2 (provider-free fixture)")
        return
    thread_id, turn_id = "", ""
    def emit(value):
        print(json.dumps(value), flush=True)
    def event(method, **params):
        emit({"method": method, "params": {"threadId": thread_id, **params}})
    for line in sys.stdin:
        value = json.loads(line)
        with (qa / "review-requests.jsonl").open("a") as log:
            log.write(json.dumps({"cwd": os.getcwd(), "request": value}) + "\n")
        method, rid = value.get("method"), value.get("id")
        if method == "initialized":
            continue
        if method == "initialize":
            result = {}
        elif method == "model/list":
            result = {"data": [{"model": "fixture-review", "displayName": "Provider-free QA", "isDefault": True,
                      "supportedReasoningEfforts": [{"reasoningEffort": "high", "description": "Fixture"}],
                      "defaultReasoningEffort": "high"}], "nextCursor": None}
        elif method in ("thread/start", "thread/resume"):
            thread_id = value.get("params", {}).get("threadId", "fixture-" + str(uuid.uuid4()))
            result = {"thread": {"id": thread_id, "turns": []}}
        elif method == "thread/backgroundTerminals/list":
            result = {"data": [], "nextCursor": None}
        elif method == "thread/goal/get":
            result = {"goal": None}
        elif method == "review/start":
            mode = (qa / "review-mode").read_text().strip() if (qa / "review-mode").exists() else "success"
            if mode == "refusal":
                emit({"id": rid, "error": {"code": -32001, "message": "Fixture: sign in to Codex before review"}})
                continue
            turn_id = "fixture-review-" + str(uuid.uuid4())
            emit({"id": rid, "result": {"reviewThreadId": thread_id, "turn": {"id": turn_id, "status": "inProgress"}}})
            event("turn/started", turn={"id": turn_id, "status": "inProgress"})
            event("item/started", turnId=turn_id, item={"id": "enter-" + turn_id, "type": "enteredReviewMode", "review": "Synthetic issue review"})
            if mode == "hold":
                continue
            review = "" if mode == "empty" else (
                "[P1] Preserve the issue worktree when returning to Pi\n\n"
                "src/workflow.rs:42 — A new correction session must inherit the exact reviewed path, "
                "rather than creating another worktree. Keep the issue and final findings attached.\n\n"
                "[P2] Keep an existing implementation draft intact\n\n"
                "src/workflow.rs:67 — Prepare a separate correction draft so unsent user notes are not replaced.\n\n"
                "Provider-free QA fixture output. This is synthetic evidence, not an authenticated model review."
            )
            item = {"id": "exit-" + turn_id, "type": "exitedReviewMode", "review": review}
            event("item/started", turnId=turn_id, item=item)
            event("item/completed", turnId=turn_id, item=item)
            event("turn/completed", turn={"id": turn_id, "status": "completed"})
            continue
        elif method == "turn/interrupt":
            emit({"id": rid, "result": {}})
            event("turn/completed", turn={"id": turn_id, "status": "interrupted"})
            continue
        else:
            emit({"id": rid, "error": {"code": -32601, "message": "Provider-free fixture does not implement " + str(method)}})
            continue
        emit({"id": rid, "result": result})


def run() -> None:
    spec = importlib.util.spec_from_file_location("board_fixture", ROOT / "scripts/issue-board-fixture.py")
    f = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(f)
    original = f.environment
    def environment():
        env = {**original(), "WAKU_RPA_DATA_DIR": str(f.TEMP),
               "WAKU_APP_NAME": "Doki Debug — Issue Review QA", "WAKU_APP_ID": "sh.doki.dev.issue-review-qa"}
        for key in ("CARGO_HOME", "RUSTUP_HOME", "BUN_INSTALL", "CARGO_TARGET_DIR", "CARGO_INCREMENTAL",
                    "DOKI_NATIVE_PREFIX", "PKG_CONFIG_PATH", "PKG_CONFIG_SYSROOT_DIR", "LIBRARY_PATH",
                    "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH", "LIBCLANG_PATH"):
            if key in os.environ:
                env[key] = os.environ[key]
        env["PATH"] = os.environ.get("PATH", "/usr/bin:/bin")
        env["CARGO_BUILD_JOBS"] = "2"
        return env
    f.environment = environment
    f.prepare()
    if not (f.QA / "review-seeded").exists():
        f.seed()
        (f.QA / "review-seeded").write_text("Synthetic Board data seeded once\n")
    f.check_processes_stopped()
    wrapper = f.QA / "codex-review-fixture"
    import shlex
    wrapper.write_text("#!/bin/sh\nexec /usr/bin/python3 " + shlex.quote(str(Path(__file__).resolve())) + " --provider " + shlex.quote(str(f.QA)) + ' "$@"\n')
    wrapper.chmod(0o700)
    settings = json.loads((f.QA / "home/.waku/settings.json").read_text())
    settings["disabled_providers"] = [name for name in f.PROVIDERS if name != "codex"]
    settings["provider_binary_overrides"]["codex"] = str(wrapper)
    f.write_json(f.QA / "home/.waku/settings.json", settings)
    signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt()))
    with f.daemon() as (daemon, ready):
        env = {**environment(), "WAKU_DAEMON_ADDRESS": ready["address"]}
        with (f.QA / "logs/watcher.log").open("w") as log:
            watcher = subprocess.Popen([str(Path(os.environ["BUN_INSTALL"]) / "bin/bun"), "scripts/dev.ts", "Issue Review QA"],
                                       cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            f.write_json(f.QA / "running.json", {"runner": os.getpid(), "daemon": daemon.pid,
                         "watcher": watcher.pid, "address": ready["address"], "database": str(f.DB)})
            try:
                print("Provider-free issue review fixture running", flush=True)
                watcher.wait()
            except KeyboardInterrupt:
                pass
            finally:
                if watcher.poll() is None:
                    os.killpg(watcher.pid, signal.SIGTERM)
                    try:
                        watcher.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        os.killpg(watcher.pid, signal.SIGKILL)
                        watcher.wait()
                (f.QA / "running.json").unlink(missing_ok=True)


if __name__ == "__main__":
    if len(sys.argv) > 2 and sys.argv[1] == "--provider":
        provider(Path(sys.argv[2]))
    else:
        run()
