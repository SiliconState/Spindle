"""Smoke test of spindle_client against this build, with a scripted loopback provider
(no paid calls). Run: SPINDLE_BIN=target/debug/spindle python3 clients/python/smoke_test.py

It drives `spindle api` (stdio) through a turn with a bash call, large requests and
replies at once and a failing event callback, then `spindle serve`'s HTTP /v1 API with
a named token through a turn of its own.
"""

import http.server
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from spindle_client import HttpClient, SpindleError, StdioClient  # noqa: E402


UNKNOWN_SENDS = 0
UNKNOWN_LOCK = threading.Lock()


class FakeProvider(http.server.BaseHTTPRequestHandler):
    """OpenAI chat-completions SSE: a bash call that writes hello.txt, then text."""

    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        last = body["messages"][-1]
        if "SUPERVISE_UNKNOWN" in json.dumps(body):
            global UNKNOWN_SENDS
            with UNKNOWN_LOCK:
                UNKNOWN_SENDS += 1
            self.close_connection = True
            return

        def chunk(delta, finish=None):
            return {"id": "x", "object": "chat.completion.chunk", "model": body["model"],
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}

        lookup = any(tool["function"]["name"] == "lookup" for tool in body.get("tools", []))
        if lookup and last["role"] != "tool":
            call = {"index": 0, "id": "call_%d" % len(body["messages"]), "type": "function",
                    "function": {"name": "lookup", "arguments": json.dumps({"q": "notes"})}}
            chunks = [chunk({"role": "assistant", "content": "Asking the app."}),
                      chunk({"tool_calls": [call]}), chunk({}, "tool_calls")]
        elif last["role"] == "tool" or "tools" not in body:
            chunks = [chunk({"role": "assistant", "content": "All done."}), chunk({}, "stop")]
        else:
            call = {"index": 0, "id": "call_%d" % len(body["messages"]), "type": "function",
                    "function": {"name": "bash",
                                 "arguments": json.dumps({"command": "echo hello > hello.txt && cat hello.txt"})}}
            chunks = [chunk({"role": "assistant", "content": "Writing."}),
                      chunk({"tool_calls": [call]}), chunk({}, "tool_calls")]
        out = "".join("data: %s\n\n" % json.dumps(c) for c in chunks)
        out += 'data: {"id":"x","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}\n\n'
        out += "data: [DONE]\n\n"
        data = out.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def check(condition, what):
    if not condition:
        raise AssertionError(what)


def stdio(env, work):
    seen = []

    def on_event(session, request, event):
        seen.append(event["type"])
        if seen.count("model_step") == 1 and event["type"] == "model_step":
            raise RuntimeError("an on_event that fails once (expected: the client carries on)")

    big = "y" * (1 << 20)
    with StdioClient(env=env, on_event=on_event) as api:
        info = api.info()
        check(info["api_version"] == 1 and "steering" in info["features"], info)
        created = api.create(work, "local", isolation="off", files=[{"path": "big.txt", "text": big}])
        check(created["phase"] == "empty", created)
        done = api.prompt(work, "write hello.txt", timeout=120)
        check(done["stopped"]["complete"], done)
        for kind in ("prompt", "tool_call", "output", "tool_result", "stopped"):
            check(kind in seen, "%s missing from %s" % (kind, seen))
        status = api.status(work)
        check(status["stop"] == "complete" and not status["busy"], status)
        check(api.read(work, "hello.txt") == b"hello\n", "hello.txt")
        # Large replies stream back while a large request is written: the reader
        # thread must never wait for the writer.
        finished = threading.Event()

        def flood():
            futures = [api.call_async("read", dir=str(work), path="big.txt") for _ in range(8)]
            missing = os.path.join(str(work), "none")
            futures.append(api.call_async("prompt", dir=missing, text="z" * (2 << 20)))
            for future in futures:
                try:
                    future.result()
                except SpindleError:
                    pass
            finished.set()

        threading.Thread(target=flood, daemon=True).start()
        if not finished.wait(60):
            api._process.kill()
            raise AssertionError("large requests and replies deadlocked the client")
        try:
            api.prompt(work, "again", expected_head=status["head"] + 1)
            raise AssertionError("a stale expected_head was accepted")
        except SpindleError as error:
            check(error.kind == "head_moved" and error.data["head"] == status["head"], error)
        try:
            api.retry(work, expected_head=status["head"] + 1)
            raise AssertionError("a stale expected_head was accepted by retry")
        except SpindleError as error:
            check(error.kind == "head_moved", error)
        try:
            api.call("nope")
            raise AssertionError("an unknown method was accepted")
        except SpindleError as error:
            check(error.kind == "method_not_found", error)
        # A session moves by bundle: the copy imported elsewhere is the same session.
        check("session_transfer" in info["features"], info["features"])
        bundle = work + ".spindle"
        kept = api.export_session(work, bundle, keep=True)
        check(not kept["moved"] and kept["head"] == status["head"], kept)
        imported = api.import_session(work + "-copy", bundle)
        check(imported["head"] == status["head"], imported)
        check(api.close_session(work + "-copy")["closed"], "close the copy")
        check(api.close_session(work)["closed"], "close")
    # Once the process has ended, calls fail at once instead of waiting forever.
    try:
        api.call("info", timeout=10)
        raise AssertionError("a call after close was accepted")
    except SpindleError as error:
        check(error.kind == "closed", error)
    print("stdio ok:", len(seen), "events")


def wait(client, request_id, seconds=120):
    """The command's final state, once it is no longer running."""
    deadline = time.time() + seconds
    while time.time() < deadline:
        state = client.receipt(request_id)["state"]
        if state != "running":
            return state
        time.sleep(0.2)
    raise AssertionError("%s still running" % request_id)


def over_http(env, root):
    host = os.path.join(root, "host")
    binary = env["SPINDLE_BIN"]
    token = subprocess.run([binary, "--dir", host, "token", "new", "smoke"], env=env,
                           check=True, capture_output=True, text=True).stdout.strip()
    serve = subprocess.Popen([binary, "--dir", host, "serve", "--port", "0"], env=env,
                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    try:
        port = re.search(r"127\.0\.0\.1:(\d+)", serve.stdout.readline()).group(1)
        client = HttpClient("http://127.0.0.1:%s" % port, token)
        check(client.info()["api_version"] == 1, "GET /v1")
        receipt = client.command("new", provider="local", text="write hello.txt")
        session = receipt["session"]
        check(receipt["state"] == "running", receipt)
        again = client.command("new", request_id=receipt["request_id"], provider="local",
                               text="write hello.txt")
        check(again["session"] == session, "a repeated request_id returns its receipt")
        wait(client, receipt["request_id"])
        messages = client.messages(session)["messages"]
        check(any(block["type"] == "tool_use" for m in messages for block in m["content"]), messages)
        try:
            next(client.events("nope"))
            raise AssertionError("events of an unknown session")
        except SpindleError as error:
            check(error.kind == "not_found" and error.status == 404, error)
        # Subscribe first (the snapshot names the head), then act, then read to the end.
        stream = client.events(session)
        _, kind, snapshot = next(stream)
        check(kind == "snapshot" and not snapshot["busy"], snapshot)
        upload = client.upload(session, b"spaced\n")
        say = client.command("say", session=session, text="once more", expected_head=snapshot["head"],
                             attachments=[{"path": "a b+c.txt", "digest": upload["digest"]}])
        kinds = []
        for _, kind, data in stream:
            if kind == "event":
                kinds.append(data["type"])
                if data["type"] == "command_done":
                    break
        stream.close()
        check("tool_call" in kinds and kinds[-1] == "command_done", kinds)
        check(wait(client, say["request_id"]) == "completed", "say receipt")
        check(client.read(session, "a b+c.txt") == b"spaced\n", "a path with a space and a plus")
        try:
            client.command("continue", session=session, expected_head=10**6)
            raise AssertionError("a stale expected_head was accepted")
        except SpindleError as error:
            check(error.kind == "head_moved" and error.status == 409, error)
        print("http ok:", len(kinds), "events")
    finally:
        serve.terminate()
        serve.wait(10)


def app_tools(env, work):
    """An app tool answered by the client: its text reaches the model and the file it
    writes lands in the workspace with that step."""
    with StdioClient(env=env) as api:
        api.tool("lookup", "Looks notes up.",
                 lambda q, call: {"content": "Found %s." % q["q"], "files": {"write": {"notes.md": "# Notes\n"}},
                                  "state": {"looked": q["q"]}},
                 input_schema={"type": "object", "properties": {"q": {"type": "string"}}})
        api.create(work, "local", builtin_tools="files")
        check(api.settings(work)["tools"][0]["name"] == "lookup", "tool offered")
        api.prompt(work, "find my notes", timeout=120, state={"topic": "notes"})
        check(api.read(work, "notes.md") == b"# Notes\n", "tool's file written")
        check(api.state(work)["state"] == {"topic": "notes", "looked": "notes"}, "prompt and answer state")
        revision = api.set_state(work, {"topic": None, "count": 1})["revision"]
        check(api.state(work, revision)["state"] == {"looked": "notes", "count": 1}, "state step")
        check("Found notes." in json.dumps(api.history(work)), "tool's answer kept")
        task = api.task_start(work, "look-again", "find my notes again", timeout=120)
        check(not task["failed"] and len(api.tasks(work)["tasks"]) == 1, "app-started task")
        check("Found notes." in json.dumps(api.history(task["task"])), "task asked the app")
    print("app tools ok")


def supervised(env, work):
    ended = threading.Event()
    recovered = threading.Event()
    calls = []
    old_released = threading.Event()

    def event(session, request, data):
        if data["type"] == "stopped":
            ended.set()

    def notice(kind, dir, data):
        if kind == "recovered":
            check(data["report"]["wake"]["work"] == "ready", data)
            recovered.set()

    with StdioClient(env=env, supervise=True, on_event=event, on_supervise=notice) as api:
        def first(input, call):
            calls.append(call["call"])
            api._process.kill()
            old_released.wait(30)
            return "stale answer from old process"

        def again(input, call):
            calls.append(call["call"])
            return "Found after Python restart."

        api.tool("lookup", "Looks notes up.", first, again=again)
        api.create(work, "local")
        try:
            api.prompt(work, "find my notes", timeout=30)
            raise AssertionError("killed prompt unexpectedly completed")
        except SpindleError as error:
            check(error.kind == "closed", error)
        check(recovered.wait(30), "no recovery notification")
        check(ended.wait(30), "supervisor did not finish known work")
        old_released.set()
        check(len(calls) == 2 and calls[0] == calls[1], calls)
        history = json.dumps(api.history(work))
        check("Found after Python restart." in history and "stale answer" not in history, history)
    print("supervised ok")


def supervised_unknown_and_loop_guard(env, work):
    recovered = threading.Event()
    reports = []

    def notice(kind, dir, data):
        if kind == "recovered":
            reports.append(data["report"])
            recovered.set()

    with StdioClient(env=env, supervise=True, on_supervise=notice) as api:
        api.create(work, "local", builtin_tools="none", resend_unknown=True)
        api.prompt(work, "seed known history")
        try:
            api.prompt(work, "SUPERVISE_UNKNOWN", timeout=30)
            raise AssertionError("unknown model send unexpectedly completed")
        except SpindleError as error:
            check("unknown outcome" in error.message, error)
        with UNKNOWN_LOCK:
            before = UNKNOWN_SENDS
        api._process.kill()
        check(recovered.wait(30), "unknown session was not inspected")
        check(reports[-1]["wake"]["work"] == "ready", reports)
        time.sleep(1.5)
        with UNKNOWN_LOCK:
            check(UNKNOWN_SENDS == before, "known-only supervision resent an unknown request with consent")
        head = api.recover(work)["head"]
        check(api.status(work)["head"] == head, "supervisor moved a blocked head")

    ended = threading.Event()
    with StdioClient(env=env, supervise=True,
                     on_supervise=lambda kind, dir, data: ended.set() if kind == "supervise_ended" else None) as api:
        api._ends = [time.monotonic()] * 5
        api._process.kill()
        check(ended.wait(10), "rapid process ends did not stop supervision")
        check(api._process.poll() is not None, "loop guard restarted the process")
    print("supervised unknown and loop guard ok")


def supervised_close_during_respawn(env):
    from unittest.mock import patch

    api = StdioClient(env=env, supervise=True)
    api.info()
    spawning = threading.Event()
    release = threading.Event()
    closing = threading.Event()
    closed = threading.Event()
    processes = []
    failures = []
    popen = subprocess.Popen

    def delayed_spawn(*args, **kwargs):
        process = popen(*args, **kwargs)
        processes.append(process)
        spawning.set()
        check(release.wait(10), "respawn barrier was never released")
        return process

    def close():
        closing.set()
        try:
            api.close(timeout=10)
        except Exception as error:
            failures.append(error)
        finally:
            closed.set()

    try:
        with patch("subprocess.Popen", side_effect=delayed_spawn):
            api._process.kill()
            check(spawning.wait(10), "supervisor did not reach respawn")
            closer = threading.Thread(target=close)
            closer.start()
            check(closing.wait(10), "close thread never started")
            check(not closed.wait(0.1), "close ignored the in-flight respawn")
            release.set()
            closer.join(15)
            check(closed.is_set() and not failures, failures)
            check(processes and all(process.poll() is not None for process in processes),
                  "close left a respawned child alive")
    finally:
        release.set()
        for process in processes:
            if process.poll() is None:
                process.kill()
                process.wait()
        api.close()
    print("supervised close during respawn ok")


def main():
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeProvider)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    root = tempfile.mkdtemp(prefix="spindle-py-smoke-")
    env = dict(os.environ)
    # A throwaway Dext home (built-in catalog, no logins) unless one is given.
    if "DEXT_HOME" not in env:
        env["HOME"] = os.path.join(root, "home")
        env["DEXT_HOME"] = os.path.join(env["HOME"], ".dext")
    os.makedirs(env["DEXT_HOME"], exist_ok=True)
    env["DEXT_BASE_URL"] = "http://127.0.0.1:%d" % server.server_address[1]
    env.setdefault("SPINDLE_BIN", "spindle")
    try:
        stdio(env, os.environ.get("SPINDLE_SMOKE_DIR") or os.path.join(root, "work"))
        app_tools(env, os.path.join(root, "app"))
        supervised(env, os.path.join(root, "supervised"))
        supervised_unknown_and_loop_guard(env, os.path.join(root, "supervised-unknown"))
        supervised_close_during_respawn(env)
        over_http(env, root)
    finally:
        server.shutdown()
        # Sandbox scratch can leave directories without owner permissions.
        subprocess.run(["chmod", "-R", "u+rwx", root], stderr=subprocess.DEVNULL)
        shutil.rmtree(root, ignore_errors=True)
    print("ok")


if __name__ == "__main__":
    main()
