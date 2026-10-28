"""Spindle's web proxy in the browser Linux (root, started at boot).

Commands whose thread has network on, with no relay set, reach the web through Chrome:
this is their HTTP proxy (the agent sets it in their environment). A plain-HTTP request
goes on to Spindle's gateway in the browser (the router, 192.168.86.1:3128) as it came.
HTTPS (CONNECT) is opened here, with a certificate for the site that this Linux's own
authority signs (made at its first start; its key never leaves it, and commands trust
it), and what is asked inside goes on to the gateway after a line naming the site.
Chrome makes each real request (its own TLS, never the user's cookies); the gateway
refuses the connection while no command with network runs.
"""
import asyncio
import ipaddress
import os
import re
import ssl

LISTEN = ('127.0.0.1', 3128)
GATEWAY = ('192.168.86.1', 3128)
CA_CERT = '/etc/spindle/ca.crt'
CA_KEY = '/etc/spindle/ca.key'
SITE_KEY = '/etc/spindle/site.key'
CONFIG = '/etc/spindle/openssl.cnf'
CERTS = '/run/spindle-certs'
# A request's head (its line and headers) is at most this long.
MAX_HEAD = 64 * 1024
NAME = re.compile(r'^(?!-)[A-Za-z0-9-]{1,63}(?:\.(?!-)[A-Za-z0-9-]{1,63})*\.?$')

# Site -> its TLS context (a task while its certificate is being made).
contexts = {}


def authority(target):
    """host and port of a CONNECT target (`host:port`, `[v6]:port`)."""
    host, _, port = target.rpartition(':')
    host = host[1:-1] if host.startswith('[') and host.endswith(']') else host
    if not host or not port.isdigit() or not 0 < int(port) < 65536:
        raise ValueError(f'not a host and port: {target!r}')
    return host, int(port)


async def context_for(host):
    if host not in contexts:
        contexts[host] = asyncio.ensure_future(make_context(host))
    try:
        return await contexts[host]
    except Exception:
        contexts.pop(host, None)
        raise


async def make_context(host):
    try:
        alt = f'IP:{ipaddress.ip_address(host)}'
    except ValueError:
        if len(host) > 253 or not NAME.match(host):
            raise ValueError(f'not a host name: {host!r}')
        alt = f'DNS:{host}'
    path = os.path.join(CERTS, f'{host}.crt')
    process = await asyncio.create_subprocess_exec(
        'openssl', 'req', '-x509', '-new', '-key', SITE_KEY, '-CA', CA_CERT, '-CAkey', CA_KEY,
        '-subj', f'/CN={host[:64]}', '-days', '397', '-config', CONFIG, '-extensions', 'site',
        '-addext', f'subjectAltName={alt}', '-out', path,
        stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.PIPE)
    _, error = await process.communicate()
    if process.returncode:
        raise OSError(f'no certificate for {host}: {error.decode(errors="replace").strip()}')
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(path, SITE_KEY)
    context.set_alpn_protocols(['http/1.1'])
    return context


def refusal(status, text):
    body = text.encode() + b'\n'
    return (f'HTTP/1.1 {status}\r\ncontent-type: text/plain; charset=utf-8\r\n'
            f'content-length: {len(body)}\r\nconnection: close\r\n\r\n').encode() + body


async def pipe(reader, writer, half_close):
    """Copies until the reader ends; then half-closes (TCP) or closes the writer."""
    try:
        while data := await reader.read(65536):
            writer.write(data)
            await writer.drain()
        if half_close and writer.can_write_eof():
            writer.write_eof()
        else:
            writer.close()
    except (OSError, ssl.SSLError):
        writer.close()


async def handle(reader, writer):
    upstream = None
    try:
        try:
            head = await reader.readuntil(b'\r\n\r\n')
        except (asyncio.IncompleteReadError, asyncio.LimitOverrunError):
            return
        method, _, rest = head.partition(b' ')
        target = rest.partition(b' ')[0].decode('latin-1')
        try:
            up_reader, upstream = await asyncio.open_connection(*GATEWAY)
        except OSError:
            writer.write(refusal('403 Forbidden', "This command's thread has no network: the user turns it on in "
                                 "Spindle (the thread's model menu)."))
            return
        if method == b'CONNECT':
            try:
                host, port = authority(target)
                context = await context_for(host)
            except (OSError, ValueError) as error:
                writer.write(refusal('502 Bad Gateway', f'Spindle could not open {target}: {error}'))
                return
            writer.write(b'HTTP/1.1 200 Connection established\r\n\r\n')
            await writer.drain()
            await writer.start_tls(context)
            named = f'[{host}]' if ':' in host else host
            upstream.write(f'SPINDLE-ORIGIN https://{named}:{port}\r\n'.encode())
        else:
            upstream.write(head)
        await asyncio.gather(pipe(reader, upstream, True), pipe(up_reader, writer, False))
    except (OSError, ssl.SSLError, ValueError):
        pass
    finally:
        for stream in (upstream, writer):
            if stream is not None:
                stream.close()


async def main():
    os.makedirs(CERTS, mode=0o700, exist_ok=True)
    server = await asyncio.start_server(handle, *LISTEN, limit=MAX_HEAD)
    async with server:
        await server.serve_forever()


asyncio.run(main())
