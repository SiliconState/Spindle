#!/usr/bin/env python3
"""A Pack Runtime v1 helper for Spindle's session-pack tests. Its only state is the
host-held `state`; every invocation is appended to the log named by argv[1]."""
import json
import sys
import time

log_path = sys.argv[1]
req = json.load(sys.stdin)
state = req.get("state") or {}
event = req["event"]
context = req.get("context") or {}
with open(log_path, "a", encoding="utf-8") as log:
    log.write(f"{event} {req.get('tool') or '-'} {','.join(context.get('ui_methods', [])) or '-'}\n")
with open(log_path + ".ids", "a", encoding="utf-8") as log:
    log.write(f"{event} {req.get('session_id')} {context.get('turn_id')}\n")
out = {"version": 1, "content": "", "effects": []}
LOOKUP = {
    "name": "lookup",
    "description": "Look up a key",
    "risk": "read",
    "input_schema": {
        "type": "object",
        "properties": {"key": {"type": "string"}},
        "required": ["key"],
        "additionalProperties": False,
    },
}
if event == "activate":
    state["activations"] = state.get("activations", 0) + 1
    state.setdefault("count", 0)
    out["content"] = "activated"
elif event == "idle":
    state["idles"] = state.get("idles", 0) + 1
    if state.pop("idle_continue", None):
        out["effects"].append({"type": "continue", "prompt": "idle follow-up", "delay_ms": 10})
elif event == "tool":
    tool = req["tool"]
    arg = req.get("input") or {}
    if tool == "bump":
        state["count"] = state.get("count", 0) + arg.get("by", 1)
        out["content"] = f"count={state['count']}"
        if arg.get("steer"):
            out["effects"].append({"type": "steer", "text": arg["steer"]})
        if arg.get("continue"):
            out["effects"].append({"type": "continue", "prompt": arg["continue"], "delay_ms": arg.get("delay_ms", 0)})
        if arg.get("view"):
            out["effects"].append({"type": "view", "title": "Counter", "markdown": arg["view"]})
        if arg.get("idle_continue"):
            state["idle_continue"] = True
        if arg.get("promote"):
            definition = dict(LOOKUP)
            if arg["promote"] == "changed":
                definition["description"] = "Changed lookup"
            if arg["promote"] == "danger":
                definition = dict(LOOKUP, name="wipe", risk="danger")
            if arg["promote"] == "write":
                definition = dict(LOOKUP, name="stash", risk="write")
            out["effects"].append({"type": "tools", "tools": [definition]})
        if arg.get("ask"):
            out["ui_request"] = {
                "id": "q1",
                "method": arg["ask"],
                "params": {"title": "Name?", "fields": [{"id": "name", "label": "Name", "type": "text", "required": True}]},
            }
        if arg.get("sleep"):
            time.sleep(arg["sleep"])
    elif tool in ("lookup", "stash"):
        out["content"] = f"{tool} {arg['key']} count={state.get('count')}"
    elif tool == "fail":
        out["is_error"] = True
        out["content"] = arg.get("message", "plain failure")
    elif tool == "hang":
        time.sleep(60)
elif event == "ui_response":
    response = req["ui"]["response"]
    state["answer"] = response
    out["content"] = "answer=" + json.dumps(response, sort_keys=True)
out["state"] = state
print(json.dumps(out))
