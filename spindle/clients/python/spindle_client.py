"""Spindle API client, standard library only (Python 3.8+).

Two transports of the same v1 API (docs/API.md):

* ``StdioClient`` runs ``spindle api`` and speaks JSON-RPC 2.0 over NDJSON on its
  stdin/stdout. Sessions are named by directory.
* ``HttpClient`` calls the ``/v1`` routes of ``spindle serve`` with a bearer token
  (``spindle --dir HOST token new NAME``). Sessions are named by id.

Errors from either transport raise ``SpindleError`` whose ``kind`` is the stable
error code (``busy``, ``head_moved``, ``not_found``, ...).
"""

import json
import os
import queue
import subprocess
import threading
import time
import traceback
import urllib.error
import urllib.parse
import urllib.request
import uuid
from concurrent.futures import Future

API_VERSION = 1


class SpindleError(Exception):
    """A refusal or failure: ``kind`` is the stable code, ``data`` any detail."""

    def __init__(self, kind, message, status=None, data=None):
        super().__init__(f"{kind}: {message}")
        self.kind = kind
        self.message = message
        self.status = status
        self.data = data or {}


class Tools:
    """Tools an app supplies (docs/API.md, app tools), shared by both transports.

    ``add(name, description, run, input_schema=None, rerun="no", timeout_s=None,
    again=None)`` registers one: ``run(input, call)`` returns text, a dict with
    ``content`` or ``error`` and ``failed``, ``files``, ``state``, or any other value
    (sent as its JSON); raising answers with the error. A call cut off by a crash or
    stop is asked again (every continue, until answered) with ``call["again"]`` true: a
    tool declared ``rerun="safe"`` simply runs again; any other answers "unknown" unless
    it gives ``again(input, call)``, which can look up its own record of ``call["call"]``
    (stable across restarts) and answer from it. Only registered tools are answered
    automatically: an app answers others itself (``app_tool_result``)."""

    def __init__(self):
        self.by_name = {}

    def add(self, name, description, run, input_schema=None, rerun="no", timeout_s=None, again=None, max_output=None):
        # The server takes True/False as well; one spelling here keeps ``answer`` simple.
        if isinstance(rerun, bool):
            rerun = "safe" if rerun else "no"
        self.by_name[name] = {
            "name": name,
            "description": description,
            "input_schema": input_schema or {"type": "object", "properties": {}},
            "rerun": rerun,
            "timeout_s": timeout_s,
            "max_output": max_output,
            "run": run,
            "again": again,
        }

    def definitions(self):
        """The definitions to offer, as ``create``/``settings`` take them (``tools``)."""
        out = []
        for tool in self.by_name.values():
            definition = {k: tool[k] for k in ("name", "description", "input_schema", "rerun")}
            if tool["timeout_s"] is not None:
                definition["timeout_s"] = tool["timeout_s"]
            if tool["max_output"] is not None:
                definition["max_output"] = tool["max_output"]
            out.append(definition)
        return out

    def answer(self, event):
        """The answer to an ``app_tool`` event (never raises)."""
        tool = self.by_name.get(event.get("name"))
        try:
            if tool is None:
                return {"error": "this app has no handler for the tool %s" % event.get("name")}
            if event.get("again") and tool["rerun"] != "safe":
                if tool["again"] is None:
                    return {"unknown": True}
                return _answer_of(tool["again"](event.get("input"), event))
            return _answer_of(tool["run"](event.get("input"), event))
        except Exception as error:  # noqa: BLE001
            return {"error": str(error)}


_ANSWER_KEYS = ("content", "error", "failed", "unknown", "files", "state", "details")

# Longest request line ``spindle api`` reads.
_MAX_LINE = 64 * 1024 * 1024


def _answer_of(value):
    """A tool's return value as an answer (the same shaping as the JS client)."""
    if isinstance(value, str):
        return {"content": value}
    if value is None:
        return {"content": ""}
    if isinstance(value, dict) and any(key in value for key in _ANSWER_KEYS):
        answer = {k: value[k] for k in _ANSWER_KEYS if k in value}
        # An answer needs text: one that only writes files or state says nothing more.
        if not any(key in answer for key in ("content", "error")) and not answer.get("unknown"):
            answer["content"] = ""
        return answer
    return {"content": json.dumps(value)}


def _refused_answer(error):
    """What to send when Spindle refused an answer (or it could not be sent): the call
    fails with the reason, instead of waiting out its timeout."""
    why = error.message if isinstance(error, SpindleError) else str(error)
    return {"error": "the app's answer was refused: %s" % why}


class StdioClient:
    """Drives ``spindle api``. Calls may overlap: ``call_async`` returns a Future and
    ``call`` waits for one. Events arrive on ``events`` (a queue of
    ``(session, request_id, event)``) and, if given, ``on_event`` (called on the
    reader thread; an exception it raises is printed to stderr and reading goes on).
    Once the process has ended, pending and new calls fail with kind ``closed``.
    Registered app tools' calls are answered automatically; an answer that cannot be
    delivered is passed to ``on_app_tool_error(error, session, event)`` if given."""

    def __init__(self, binary=None, env=None, on_event=None, stderr=None, on_app_tool_error=None,
                 supervise=False, on_supervise=None):
        self._binary = binary or os.environ.get("SPINDLE_BIN", "spindle")
        self._env = env
        self._stderr = stderr
        self.supervise = supervise
        self.on_supervise = on_supervise
        self._ending = threading.Event()
        self._sessions = {}
        self._ends = []
        # The pending table has its own lock, never held while writing: the reader
        # thread needs it to deliver replies, and a large request blocks in write()
        # until the process reads, which may first need its replies to be read.
        self._lock = threading.Lock()
        self._write_lock = threading.Lock()
        self._lifecycle_lock = threading.Lock()
        self._pending = {}
        self._next = 0
        self._closed = False
        self.on_event = on_event
        self.on_app_tool_error = on_app_tool_error
        self.events = queue.Queue()
        self.tools = Tools()
        self._start()

    def _start(self):
        with self._lifecycle_lock:
            if self._ending.is_set():
                return False
            process = subprocess.Popen([self._binary, "api"], stdin=subprocess.PIPE,
                                       stdout=subprocess.PIPE, stderr=self._stderr, env=self._env)
            with self._lock:
                self._process = process
                self._closed = False
            self._reader = threading.Thread(target=self._read, args=(process,), daemon=True)
            self._reader.start()
            return True

    def _notice(self, kind, dir=None, **data):
        event = dict(type=kind, **data)
        self.events.put((dir, None, event))
        if self.on_supervise:
            try:
                self.on_supervise(kind, dir, event)
            except Exception:
                traceback.print_exc()

    def _restart(self, process):
        if self._ending.wait(0.1):
            return
        with self._lock:
            if process is not self._process or self._ending.is_set():
                return
            now = time.monotonic()
            self._ends = [end for end in self._ends if now - end < 60] + [now]
        if len(self._ends) > 5:
            self._notice("supervise_ended", error="more than five process ends within a minute")
            return
        try:
            if not self._start():
                return
        except OSError as error:
            self._notice("supervise_ended", error=str(error))
            return
        self._notice("restarted")
        with self._lock:
            sessions = list(self._sessions.values())
        for dir in sessions:
            if self._ending.is_set():
                return
            try:
                report = self.recover(dir)
                self._notice("recovered", dir, report=report)
                unknown = any(item.get("next") != "none" and
                              (item.get("outcome") == "unknown" or item.get("unknown") is True)
                              for item in report.get("items", []))
                if report.get("wake", {}).get("work") == "ready" and not unknown:
                    resumed = self.call_async("continue", dir=dir)
                    def settled(future, dir=dir):
                        error = future.exception()
                        if error is not None:
                            self._notice("recover_error", dir, error=str(error))
                    resumed.add_done_callback(settled)
            except SpindleError as error:
                self._notice("recover_error", dir, error=str(error))

    def _read(self, process):
        try:
            for line in process.stdout:
                try:
                    self._frame(json.loads(line), process)
                except Exception:
                    traceback.print_exc()
        finally:
            process.wait()
            with self._lock:
                current = process is self._process
                if current:
                    self._closed = True
                    pending, self._pending = self._pending, {}
                else:
                    pending = {}
            for future in pending.values():
                _fail(future, SpindleError("closed", "spindle api ended"))
            if current and self.supervise and not self._ending.is_set():
                threading.Thread(target=self._restart, args=(process,), daemon=True).start()

    def _frame(self, frame, process):
        if frame.get("method") == "event":
            params = frame["params"]
            item = (params["session"], params.get("request"), params["event"])
            event = item[2]
            if event.get("type") == "app_tool" and event.get("name") in self.tools.by_name:
                # Answered off the reader thread: the handler may take its time.
                threading.Thread(target=self._answer, args=(*item[::2], process), daemon=True).start()
            self.events.put(item)
            if self.on_event:
                self.on_event(*item)
            return
        with self._lock:
            future = self._pending.pop(frame.get("id"), None)
        if future is None:
            return
        try:
            if "error" in frame:
                error = frame["error"]
                data = error.get("data") or {}
                future.set_exception(
                    SpindleError(data.get("code", "internal"), error["message"], error["code"], data)
                )
            else:
                future.set_result(frame["result"])
        except Exception as error:  # noqa: BLE001
            _fail(future, SpindleError("internal", "malformed reply: %r" % error))

    def call_async(self, method, _generation=None, **params):
        """Sends a request; the Future resolves with its result (prompt, continue,
        retry and rewind resolve when the turn ends). One that cannot be sent (not
        JSON, or a line over 64 MiB) fails with kind ``unsendable``, never written."""
        future = Future()
        with self._lock:
            self._next += 1
            request = self._next
        future.request_id = request
        frame = {"jsonrpc": "2.0", "id": request, "method": method, "params": params}
        try:
            line = json.dumps(frame).encode() + b"\n"
        except (TypeError, ValueError) as error:
            future.set_exception(SpindleError("unsendable", "the request is not JSON: %s" % error))
            return future
        if len(line) > _MAX_LINE:
            future.set_exception(SpindleError("unsendable", "the request is over 64 MiB"))
            return future
        with self._lock:
            if self._closed or self._ending.is_set() or (_generation is not None and _generation is not self._process):
                future.set_exception(SpindleError("closed", "spindle api ended"))
                return future
            process = self._process
            dir = params.get("dir")
            if dir and method in ("create", "open", "prompt", "continue", "retry", "rewind", "task_start"):
                self._sessions[os.path.realpath(dir)] = str(dir)
            elif dir and method == "close":
                self._sessions.pop(os.path.realpath(dir), None)
            self._pending[request] = future
        try:
            with self._write_lock:
                process.stdin.write(line)
                process.stdin.flush()
        except (OSError, ValueError):
            with self._lock:
                self._pending.pop(request, None)
            _fail(future, SpindleError("closed", "spindle api ended"))
        return future

    def call(self, method, timeout=None, **params):
        return self.call_async(method, **params).result(timeout)

    def tool(self, name, description, run, **options):
        """Registers an app tool (see ``Tools``); sessions this client creates offer it."""
        self.tools.add(name, description, run, **options)
        return self

    def _answer(self, session, event, process):
        answer = self.tools.answer(event)
        try:
            try:
                self.call("app_tool_result", _generation=process, dir=session, call=event["call"], **answer)
            except SpindleError as error:
                # Refused as malformed, or not sendable at all (not JSON, too long).
                if error.kind not in ("bad_request", "unsendable"):
                    raise
                self.call("app_tool_result", _generation=process, dir=session, call=event["call"], **_refused_answer(error))
        except SpindleError as error:
            # Stale (the run stopped first) or the process ended: it is asked again.
            if self.on_app_tool_error:
                try:
                    self.on_app_tool_error(error, session, event)
                except Exception:  # noqa: BLE001
                    traceback.print_exc()

    # The methods of the API, by name (params as in docs/API.md).
    def info(self):
        return self.call("info")

    def providers(self):
        return self.call("providers")

    def create(self, dir, provider, **settings):
        """Tools registered with ``tool()`` are offered unless ``tools`` is given."""
        if self.tools.by_name and "tools" not in settings:
            settings["tools"] = self.tools.definitions()
        return self.call("create", dir=str(dir), provider=provider, **settings)

    def open(self, dir):
        return self.call("open", dir=str(dir))

    def close_session(self, dir):
        return self.call("close", dir=str(dir))

    def prompt(self, dir, text, files=None, expected_head=None, timeout=None, state=None):
        """``state``: a JSON merge patch to the app state, committed with the prompt."""
        params = {"dir": str(dir), "text": text}
        if files:
            params["files"] = files
        if state is not None:
            params["state"] = state
        if expected_head is not None:
            params["expected_head"] = expected_head
        return self.call("prompt", timeout=timeout, **params)

    def resume(self, dir, expected_head=None, until=None):
        params = {"dir": str(dir)}
        if until is not None:
            params["until"] = until
        if expected_head is not None:
            params["expected_head"] = expected_head
        return self.call("continue", **params)

    def retry(self, dir, expected_head=None):
        params = {"dir": str(dir)}
        if expected_head is not None:
            params["expected_head"] = expected_head
        return self.call("retry", **params)

    def rewind(self, dir, revision, expected_head=None):
        params = {"dir": str(dir), "revision": revision}
        if expected_head is not None:
            params["expected_head"] = expected_head
        return self.call("rewind", **params)

    def stop(self, dir):
        return self.call("stop", dir=str(dir))

    def steer(self, dir, text):
        return self.call("steer", dir=str(dir), text=text)

    def status(self, dir):
        return self.call("status", dir=str(dir))

    def history(self, dir, revision=None):
        return self.call("history", dir=str(dir), revision=revision)

    def targets(self, dir):
        return self.call("targets", dir=str(dir))["targets"]

    def files(self, dir, revision=None):
        return self.call("files", dir=str(dir), revision=revision)

    def read(self, dir, path, revision=None):
        """The file's bytes."""
        import base64

        reply = self.call("read", dir=str(dir), path=path, revision=revision)
        if reply["encoding"] == "utf8":
            return reply["content"].encode()
        return base64.b64decode(reply["content"])

    def export(self, dir, target):
        return self.call("export", dir=str(dir), target=str(target))

    def export_session(self, dir, target, keep=False):
        """Writes the session to a bundle file (`target`, new); it moves there unless `keep`."""
        params = {"dir": str(dir), "target": str(target)}
        if keep:
            params["keep"] = True
        return self.call("export_session", **params)

    def import_session(self, dir, source):
        """Continues a bundled session in `dir` (new or empty)."""
        return self.call("import_session", dir=str(dir), source=str(source))

    def settings(self, dir, **changes):
        return self.call("settings", dir=str(dir), **changes)

    def usage(self, dir):
        return self.call("usage", dir=str(dir))

    def recover(self, dir):
        """What the session's owner left unfinished and what is safe to resume: items
        and ``wake`` (``none``, ``ready`` under the saved retry policy, or
        ``blocked`` with ``why``). Supervision excludes actionable unknown items
        even when ordinary resume would resend them under explicit consent."""
        return self.call("recover", dir=str(dir))

    def task_start(self, dir, task, prompt, model=None, effort=None, timeout=None, expected_head=None,
                   report="wait", settings=None, files=None):
        """Runs the task ``task`` (the same id finds the same task) from the session's
        files to the end of its turn: ``{task, answer, failed}``, where ``task`` is its
        directory, a session of its own."""
        options = {key: value for key, value in (("model", model), ("effort", effort),
                   ("report", report), ("settings", settings), ("files", files)) if value is not None}
        if expected_head is not None:
            options["expected_head"] = expected_head
        return self.call("task_start", timeout, dir=str(dir), task=task, prompt=prompt, **options)

    def task_wait(self, dir, task, timeout_s=600):
        return self.call("task_wait", dir=str(dir), task=task, timeout_s=timeout_s)

    def task_stop(self, dir, task):
        return self.call("task_stop", dir=str(dir), task=task)

    def tasks(self, dir):
        """The session's tasks with their head state: ``{tasks: [{task, dir, status}]}``."""
        return self.call("tasks", dir=str(dir))

    def state(self, dir, revision=None):
        """The app state as of a revision (the head by default): ``{head, revision, state}``."""
        params = {} if revision is None else {"revision": revision}
        return self.call("state", dir=str(dir), **params)

    def set_state(self, dir, patch, expected_head=None):
        """Applies a JSON merge patch to the app state: a step of its own between turns
        (``{revision}``), or at the running turn's next step boundary (``{queued}``)."""
        params = {"patch": patch}
        if expected_head is not None:
            params["expected_head"] = expected_head
        return self.call("state_set", dir=str(dir), **params)

    def doctor(self, dir):
        return self.call("doctor", dir=str(dir))

    def close(self, timeout=30):
        """Ends input (running turns are asked to stop) and waits for the process."""
        with self._lifecycle_lock:
            self._ending.set()
            process = self._process
            reader = self._reader
        try:
            with self._write_lock:
                process.stdin.close()
        except OSError:
            pass
        code = process.wait(timeout)
        reader.join(timeout)
        return code

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


class HttpClient:
    """Calls ``spindle serve``'s ``/v1`` routes with a bearer token."""

    def __init__(self, base_url, token, timeout=60):
        self.base = base_url.rstrip("/")
        self.token = token
        self.timeout = timeout
        self.tools = Tools()

    def tool(self, name, description, run, **options):
        """Registers an app tool (see ``Tools``); offer it with
        ``tools=client.tools.definitions()`` in a ``new`` or ``settings`` command and
        answer its calls with ``answer_app_tools``."""
        self.tools.add(name, description, run, **options)
        return self

    def recovery(self, session):
        """What the session's owner left unfinished and what is safe to resume."""
        return self._request("GET", f"/v1/sessions/{session}/recovery")

    def tasks(self, session):
        """The session's tasks with their head state."""
        return self._request("GET", f"/v1/sessions/{session}/tasks")

    def task_messages(self, session, task, revision=None):
        """A task's messages form."""
        return self._request("GET", f"/v1/sessions/{session}/tasks/{task}/messages{_revision(revision)}")

    def state(self, session, revision=None):
        """The app state as of a revision (the head by default)."""
        return self._request("GET", f"/v1/sessions/{session}/state{_revision(revision)}")

    def queue_state(self, session, patch):
        """A state change for the running turn (between turns send a ``state`` command)."""
        return self._request("POST", f"/v1/sessions/{session}/state", {"patch": patch})

    def app_tool_result(self, session, call, answer):
        return self._request("POST", f"/v1/sessions/{session}/app_tool_result", dict(answer, call=call))

    def answer_app_tools(self, session, stop=None):
        """Answers the session's registered app tools' calls until ``stop`` (a
        threading.Event, checked at least every heartbeat) is set or the session is
        deleted: one waiting when the stream (re)connects, from its snapshot, then each
        ``app_tool`` event as it comes. The host ends a stream every five minutes; this
        reconnects where it left off."""
        # Calls being answered, or answered and not yet done: one announcement is
        # answered once (a snapshot and an event can name the same call).
        answered = set()

        def handle(event):
            if not event or event.get("type") != "app_tool" or event["call"] in answered:
                return
            if event.get("name") not in self.tools.by_name:
                return
            answered.add(event["call"])
            try:
                try:
                    self.app_tool_result(session, event["call"], self.tools.answer(event))
                except SpindleError as error:
                    if error.status != 400:
                        raise
                    self.app_tool_result(session, event["call"], _refused_answer(error))
            except SpindleError as error:
                # 409: no longer waiting (stopped, or answered elsewhere): it is asked
                # again, as a new announcement.
                answered.discard(event["call"])
                if error.status != 409:
                    raise

        last_event_id = None
        while not (stop is not None and stop.is_set()):
            for event_id, kind, data in self.events(session, last_event_id, _heartbeats=True):
                if stop is not None and stop.is_set():
                    return
                if event_id:
                    last_event_id = event_id
                if kind == "deleted":
                    return
                if kind == "snapshot":
                    for event in data.get("app_tools", [data.get("app_tool")]):
                        handle(event)
                elif kind == "event" and data.get("type") == "app_tool_done":
                    answered.discard(data.get("call"))
                elif kind == "event":
                    handle(data)

    def _request(self, method, path, body=None, raw=False):
        data = None if body is None else (body if isinstance(body, bytes) else json.dumps(body).encode())
        request = urllib.request.Request(self.base + path, data=data, method=method)
        request.add_header("Authorization", f"Bearer {self.token}")
        if body is not None:
            binary = isinstance(body, bytes)
            request.add_header("Content-Type", "application/octet-stream" if binary else "application/json")
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                payload = response.read()
                return payload if raw else json.loads(payload)
        except urllib.error.HTTPError as error:
            raise _refusal(error) from None

    def info(self):
        return self._request("GET", "/v1")

    def sessions(self):
        return self._request("GET", "/v1/sessions")["sessions"]

    def session(self, session):
        return self._request("GET", f"/v1/sessions/{session}")

    def command(self, kind, request_id=None, **fields):
        """Admits a command; returns its receipt. Reuse ``request_id`` to retry
        safely after a lost reply."""
        body = {"request_id": request_id or uuid.uuid4().hex, "kind": kind, **fields}
        return self._request("POST", "/v1/commands", body)

    def receipt(self, request_id):
        return self._request("GET", f"/v1/receipts/{request_id}")

    def messages(self, session, revision=None):
        return self._request("GET", f"/v1/sessions/{session}/messages{_revision(revision)}")

    def files(self, session, revision=None):
        return self._request("GET", f"/v1/sessions/{session}/files{_revision(revision)}")

    def read(self, session, path, revision=None):
        query = {"path": path}
        if revision is not None:
            query["revision"] = revision
        query = urllib.parse.urlencode(query, quote_via=urllib.parse.quote)
        return self._request("GET", f"/v1/sessions/{session}/file?{query}", raw=True)

    def task_start(self, session, task, prompt, report="wait", settings=None, request_id=None, **options):
        fields = dict(session=session, task=task, prompt=prompt, report=report, **options)
        if settings is not None:
            fields["settings"] = settings
        return self.command("task_start", request_id=request_id, **fields)

    def task_wait(self, session, task, timeout_s=600):
        task = urllib.parse.quote(task, safe="")
        return self._request("POST", f"/v1/sessions/{session}/tasks/{task}/wait", {"timeout_s": timeout_s})

    def task_stop(self, session, task):
        task = urllib.parse.quote(task, safe="")
        return self._request("POST", f"/v1/sessions/{session}/tasks/{task}/stop", {})

    def targets(self, session):
        return self._request("GET", f"/v1/sessions/{session}/targets")["targets"]

    def steer(self, session, text):
        return self._request("POST", f"/v1/sessions/{session}/steer", {"text": text})

    def upload(self, session, data):
        """Uploads bytes for a later ``say``'s ``attachments``; returns ``{digest, size}``."""
        return self._request("POST", f"/v1/sessions/{session}/upload", bytes(data))

    def events(self, session, last_event_id=None, _heartbeats=False):
        """Yields ``(id, kind, data)`` from the session's event stream: kind is
        ``snapshot``, ``event`` or ``deleted``. Pass the last id to resume. (With
        ``_heartbeats``, each heartbeat comment also yields ``(None, "heartbeat",
        None)``, so a caller can look up between events.)"""
        request = urllib.request.Request(f"{self.base}/v1/sessions/{session}/events")
        request.add_header("Authorization", f"Bearer {self.token}")
        if last_event_id:
            request.add_header("Last-Event-ID", last_event_id)
        try:
            response = urllib.request.urlopen(request, timeout=self.timeout)
        except urllib.error.HTTPError as error:
            raise _refusal(error) from None
        with response:
            frame = {}
            for raw in response:
                line = raw.decode().rstrip("\n")
                if not line:
                    if "data" in frame:
                        yield frame.get("id"), frame.get("event", "message"), json.loads(frame["data"])
                    frame = {}
                elif line.startswith(":"):
                    if _heartbeats:
                        yield None, "heartbeat", None
                elif ":" in line:
                    key, _, value = line.partition(":")
                    frame[key] = value[1:] if value.startswith(" ") else value


def _revision(revision):
    return "" if revision is None else f"?revision={revision}"


def _refusal(error):
    """A SpindleError from an HTTP error: the v1 envelope's ``code``, or ``http_N``."""
    text = error.read().decode(errors="replace")
    try:
        envelope = json.loads(text)
    except ValueError:
        envelope = None
    if not isinstance(envelope, dict):
        envelope = {"error": text}
    message = envelope.pop("error", text)
    kind = envelope.pop("code", "http_%d" % error.code)
    return SpindleError(kind, message, error.code, envelope)


def _fail(future, error):
    if not future.done():
        try:
            future.set_exception(error)
        except Exception:  # noqa: BLE001 (resolved meanwhile)
            pass
