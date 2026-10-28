"""Run one prompt in a session directory and print what happens.

    python3 example.py DIR PROVIDER "PROMPT"

Uses `spindle` from PATH (or $SPINDLE_BIN) and your Dext logins.
"""

import sys

from spindle_client import SpindleError, StdioClient


def show(session, request, event):
    kind = event["type"]
    if kind == "model_delta":
        print(event["text"], end="", flush=True)
    elif kind == "tool_call":
        print(f"\n$ {event['name']} {event['input']}")
    elif kind == "tool_result":
        print(event["content"][:400])
    elif kind == "stopped":
        print(f"\n[{event['reason']}] revision {event['revision']}")


def main():
    directory, provider, prompt = sys.argv[1:4]
    with StdioClient(on_event=show) as api:
        try:
            api.open(directory)
        except SpindleError as error:
            if error.kind != "not_found":
                raise
            api.create(directory, provider)
        result = api.prompt(directory, prompt)
        print("head:", result["head"])
        for entry in api.files(directory)["files"]:
            print(f"{entry['size']:>9}  {entry['path']}")


if __name__ == "__main__":
    main()
