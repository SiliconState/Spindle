"""Spindle's calls for scripts, as native run_code scripts have them: model, models,
tool, tools (search, describe, call), batch, limits, image. Each goes to the runner
agent of the command running the script (/run/spindle.sock), which asks Spindle;
Spindle journals every call before it runs, so one cut off is never sent again by
itself (its error then says [may_have_run]).

    import spindle
    print(spindle.model("Say hello in French")["text"])
    page = spindle.tool("read_page")
"""
import base64
import json
import socket

SOCKET = '/run/spindle.sock'


class SpindleError(RuntimeError):
    """A call that failed (`may_have_run`: whether it may have taken effect)."""

    def __init__(self, message, may_have_run=False):
        super().__init__(message + (' [may_have_run]' if may_have_run else ''))
        self.may_have_run = may_have_run


def _ask(method, input):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
        try:
            conn.connect(SOCKET)
        except OSError as error:
            raise SpindleError(f'Spindle is not reachable from here ({error})') from None
        conn.sendall(json.dumps({'method': method, 'input': input}).encode() + b'\n')
        data = b''
        while not data.endswith(b'\n'):
            chunk = conn.recv(65536)
            if not chunk:
                raise SpindleError('the runner closed the connection', True)
            data += chunk
    return json.loads(data)


def _call(method, input):
    reply = _ask(method, input)
    if not reply.get('ok'):
        raise SpindleError(reply.get('error') or 'the call failed', bool(reply.get('may_have_run')))
    return reply.get('data')


def model(prompt, **options):
    """One model call: {text, usage, provider, model}. Options: provider, model,
    max_tokens, system, effort, images (image(path) values)."""
    return _call('model', {'prompt': prompt, **options})


def models(prompts, **options):
    """Model calls run together: each a reply dict or {error, may_have_run}."""
    replies = _call('batch', {'calls': [{'method': 'model', 'input': {'prompt': p, **options}} for p in prompts]})
    return [r['data'] if r.get('ok') else {'error': r.get('error'), 'may_have_run': bool(r.get('may_have_run'))}
            for r in replies]


def tool(name, arguments=None):
    """Calls one of the session's tools (the page tools in Chrome, an app's tools)."""
    return _call('tool', {'name': name, 'arguments': arguments or {}})


class _Tools:
    @staticmethod
    def search(query=''):
        return _call('search', {'query': query})

    @staticmethod
    def describe(name):
        return _call('describe', {'name': name})

    call = staticmethod(tool)


tools = _Tools()


def batch(calls):
    """Calls run together: [(method, input), ...] or [{'method':..., 'input':...}];
    each result is {ok, data | error}."""
    listed = []
    for call in calls:
        method, input = (call['method'], call.get('input', {})) if isinstance(call, dict) else call
        if method == 'tool':
            input = {'name': input['name'], 'arguments': input.get('arguments', {})}
        listed.append({'method': method, 'input': input})
    return _call('batch', {'calls': listed})


def limits():
    """This script's call limits."""
    return _call('limits', {})


def http(url, **options):
    """An HTTP request to a site the user granted (method, headers, body or json,
    timeout): {status, headers, json | body | body_base64, truncated}."""
    return _call('http', {**options, 'url': url})


def image(path):
    """A PNG, JPEG, GIF or WebP file, for model(..., images=[image(path)])."""
    with open(path, 'rb') as file:
        data = file.read()
    kinds = [(b'\x89PNG\r\n\x1a\n', 'image/png'), (b'\xff\xd8\xff', 'image/jpeg'), (b'GIF8', 'image/gif')]
    kind = next((media for magic, media in kinds if data.startswith(magic)), None)
    if kind is None and data[:4] == b'RIFF' and data[8:12] == b'WEBP':
        kind = 'image/webp'
    if kind is None:
        raise SpindleError(f'image: {path} is not PNG, JPEG, GIF or WebP')
    return {'media_type': kind, 'data': base64.b64encode(data).decode()}
