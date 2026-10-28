"""Spindle's runner agent in the browser Linux.

It runs the commands the browser asks for over the serial line, several at once (the
browser decides how many), each with bash in /workspace (the command's copy of its
thread's workspace: a directory of the 9p root the browser filled, bound there), as the
unprivileged user `spindle`, in namespaces of its own: its own mounts (a fresh /tmp,
/var/tmp, /dev/shm and home, as a native sandbox gives each run), its own processes (it
sees no other command's, and whatever it leaves running ends with it, whatever group it
put itself in) and its own System V IPC. A command whose thread has no network gets a
network of its own too, with only loopback in it, so it reaches nothing, not even this
Linux's web proxy. The agent sends each command's output and exit status back once its
writes are synced to the share. Frames are lines: STX, a kind, an id, base64. A payload
over 16 KiB goes as a file on the share instead (a burst that large into the serial line
wedges it): its frame's kind ends with `+` and its base64 names the file (in /runs,
root's), which the reader removes.

  browser -> agent  RUN <id> <base64 JSON {command, timeout_s, dir, network, now}> | STOP <id> | PING <n>
                    | REPLY <id> <base64 JSON {seq, reply}>
  agent -> browser  READY <n> | OUT <id> <base64 bytes> | END <id> <base64 JSON {code, timed_out, stopped}>
                    | RPC <id> <base64 JSON {seq, request}>

The browser's clock (`now`) sets this one's first: a resumed machine wakes with the time
it was saved at. A command whose thread has network gets it as `network` says: through
Chrome (`chrome`: its environment names the web proxy, proxy.py, and trusts this
Linux's authority) or the user's relay (`relay`: connections go out as they are). The
browser runs one such command at a time and lets its traffic through only meanwhile.

While a command runs, its scripts ask Spindle for calls (model, tool, batch, search,
describe, limits: as native run_code scripts do) through /run/spindle.sock (a socket of
that command's alone, bound there in its namespace), one JSON line each way (the
`spindle` command and modules speak it). The agent relays them one at a time per
command, as a native script makes them (calls from several processes wait their turn; a
batch runs its calls together), numbering each call's children in order.
"""
import base64
import fcntl
import json
import os
import select
import signal
import socket
import struct
import time
import tty

PORT = '/dev/ttyS0'
CHUNK = 2048
# Where a command's scripts reach Spindle (in its namespace), and where each command's
# own socket is made (root's, closed to commands).
SOCKET = '/run/spindle.sock'
CALLS = '/run/spindle-calls'
# What puts a command in namespaces of its own and starts it (a static program: forks of
# this Python cost far more in the emulated machine).
SANDBOX = '/usr/local/bin/spindle-sandbox'
ENV = {
    'HOME': '/home/spindle', 'LANG': 'C.UTF-8', 'TERM': 'dumb', 'USER': 'spindle', 'LOGNAME': 'spindle',
    'PATH': '/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin',
}
# A command whose network goes through Chrome: the web proxy, for every client that
# takes one from the environment (Node's own fetch too), and this Linux's authority
# for those with their own lists of authorities.
PROXY = 'http://127.0.0.1:3128'
BUNDLE = '/etc/ssl/certs/ca-certificates.crt'
THROUGH_CHROME = {
    'http_proxy': PROXY, 'https_proxy': PROXY, 'HTTP_PROXY': PROXY, 'HTTPS_PROXY': PROXY,
    'no_proxy': 'localhost,127.0.0.1,::1', 'NO_PROXY': 'localhost,127.0.0.1,::1', 'NODE_USE_ENV_PROXY': '1',
    'NODE_EXTRA_CA_CERTS': '/etc/spindle/ca.crt', 'SSL_CERT_FILE': BUNDLE, 'REQUESTS_CA_BUNDLE': BUNDLE,
    'PIP_CERT': BUNDLE,
}
# The browser keeps 4 MiB of output; one byte more tells it the output was cut.
MAX_OUT = 4 * 1024 * 1024 + 1
# After bash exits, how long output something it left still holds open is read.
DRAIN_S = 0.5
# Scripts' connections one command may hold at once; the longest request line, and the
# most replies a script may leave untaken.
MAX_CLIENTS = 64
MAX_LINE = 64 * 1024 * 1024
MAX_PENDING = 64 * 1024 * 1024
# Calls that are children (numbered as a native script numbers them); a batch counts each.
CHILDREN = ('model', 'tool', 'http')
# The most a frame carries on the serial line; more goes as a file on the share.
INLINE = 16 * 1024
FRAMES = '/runs'
sent_files = 0
made = 0
# Networks of their own for commands without network, each made ready while the agent is
# idle (making one costs more than the rest of a command's setup) and used once.
spare_networks = []

SIOCGIFFLAGS, SIOCSIFFLAGS, IFF_UP = 0x8913, 0x8914, 1


def loopback_up():
    """Brings up `lo` in a command's own network (it starts down)."""
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        request = struct.pack('16sH14x', b'lo', 0)
        flags = struct.unpack('16sH14x', fcntl.ioctl(sock, SIOCGIFFLAGS, request))[1]
        fcntl.ioctl(sock, SIOCSIFFLAGS, struct.pack('16sH14x', b'lo', flags | IFF_UP))


def new_network():
    """A network of its own (only loopback, up), held by the descriptor returned: a
    child makes it and waits while the agent opens it."""
    ready, done = os.pipe()
    hold, release = os.pipe()
    child = os.fork()
    if child == 0:
        try:
            os.close(ready)
            os.close(release)
            os.unshare(os.CLONE_NEWNET)
            loopback_up()
            os.write(done, b'k')
            os.read(hold, 1)
        finally:
            os._exit(0)
    os.close(done)
    os.close(hold)
    try:
        if os.read(ready, 1) != b'k':
            raise OSError('a network of its own was not made')
        return os.open(f'/proc/{child}/ns/net', os.O_RDONLY)
    finally:
        os.close(ready)
        os.close(release)
        os.waitpid(child, 0)


def spare_network():
    """Keeps one network of its own ready."""
    if not spare_networks:
        try:
            spare_networks.append(new_network())
        except OSError:
            pass  # made when a command needs it


fd = os.open(PORT, os.O_RDWR | os.O_NOCTTY)
tty.setraw(fd)
pending = b''
os.makedirs(CALLS, mode=0o700, exist_ok=True)
os.chmod(CALLS, 0o700)
# What each command's socket is bound over (a file; nothing listens at it out here).
for stale in (SOCKET,):
    try:
        os.unlink(stale)
    except FileNotFoundError:
        pass
os.close(os.open(SOCKET, os.O_WRONLY | os.O_CREAT, 0o644))


def send(kind, ident, payload=b''):
    global sent_files
    if len(payload) > INLINE:
        sent_files += 1
        path = f'{FRAMES}/out-{os.getpid()}-{sent_files}'
        out = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        try:
            view = memoryview(payload)
            while view:
                view = view[os.write(out, view):]
            os.fsync(out)  # on the share before the browser looks
        finally:
            os.close(out)
        kind, payload = kind + '+', path.lstrip('/').encode()
    frame = b'\x02' + kind.encode() + b' ' + str(ident).encode() + b' ' + base64.b64encode(payload) + b'\n'
    view = memoryview(frame)
    while view:
        view = view[os.write(fd, view):]


def frames():
    """The whole frames read so far, as (kind, id, payload bytes); the rest waits. A
    frame that does not read (cut, or its file gone) is dropped."""
    global pending
    pending += os.read(fd, 65536)
    found = []
    while b'\n' in pending:
        line, pending = pending.split(b'\n', 1)
        start = line.find(b'\x02')
        if start < 0:
            continue
        parts = line[start + 1:].split(b' ')
        if len(parts) != 3:
            continue
        try:
            kind, ident, payload = parts[0].decode(), parts[1].decode(), base64.b64decode(parts[2], validate=True)
            if kind.endswith('+'):
                path = os.path.normpath(os.path.join('/', payload.decode().lstrip('/')))
                if os.path.dirname(path) != FRAMES:
                    continue
                with open(path, 'rb') as held:
                    payload = held.read()
                os.unlink(path)
                kind = kind[:-1]
        except (ValueError, OSError):
            continue
        found.append((kind, ident, payload))
    return found


class Bridge:
    """The scripts of one command: its socket, their connections (what each sent so far
    and the replies not yet taken), the calls waiting their turn, the one with Spindle,
    and the number the next child takes. Nothing a script does (not reading its replies,
    a line without end) holds up the agent or another command."""

    def __init__(self, ident, server):
        self.ident = ident
        self.server = server
        self.child = 0
        self.seq = 0
        self.clients = {}  # socket -> bytes read so far
        self.outgoing = {}  # socket -> replies it has not taken yet
        self.queue = []  # (socket or None, request) waiting their turn
        self.asking = None  # (seq, socket or None): the call with Spindle

    def accept(self):
        try:
            conn, _ = self.server.accept()
        except OSError:  # none waiting, or no descriptors left
            return
        conn.setblocking(False)
        if len(self.clients) >= MAX_CLIENTS:
            try:
                conn.send(json.dumps({'ok': False, 'error': f'at most {MAX_CLIENTS} connections to Spindle at once'}).encode() + b'\n')
            except OSError:
                pass
            conn.close()
            return
        self.clients[conn] = bytearray()
        self.outgoing[conn] = bytearray()

    def read(self, conn):
        try:
            data = conn.recv(65536)
        except BlockingIOError:
            return
        except OSError:
            data = b''
        if not data:
            self.drop(conn)
            return
        held = self.clients[conn]
        held += data
        while conn in self.clients and b'\n' in held:
            line, _, rest = bytes(held).partition(b'\n')
            held[:] = rest
            self.ask(conn, line)
        if conn in self.clients and len(held) > MAX_LINE:
            self.answer(conn, {'ok': False, 'error': f'a request is one line of at most {MAX_LINE // 1048576} MiB'})
            self.drop(conn)

    def ask(self, conn, line):
        try:
            request = json.loads(line, parse_constant=refuse_constant, parse_float=finite)
            method = request['method']
            if not isinstance(method, str):
                raise TypeError
        except (ValueError, KeyError, TypeError):
            self.answer(conn, {'ok': False, 'error': 'a request is {"method": ..., "input": ...}'})
            return
        self.queue.append((conn, {'method': method, 'input': request.get('input', {})}))
        self.next()

    def next(self):
        """Sends the next waiting call once none is with Spindle (numbered as it goes)."""
        if self.asking is not None or not self.queue:
            return
        conn, request = self.queue.pop(0)
        method = request['method']
        if method in CHILDREN or method == 'batch':
            request['id'] = self.child
            calls = request['input'].get('calls') if method == 'batch' and isinstance(request['input'], dict) else None
            self.child += len(calls) if isinstance(calls, list) else 1
        self.seq += 1
        self.asking = (self.seq, conn)
        send('RPC', self.ident, json.dumps({'seq': self.seq, 'request': request}, allow_nan=False).encode())

    def reply(self, payload):
        message = json.loads(payload)
        if self.asking is None or message.get('seq') != self.asking[0]:
            return
        conn = self.asking[1]
        self.asking = None
        if conn is not None:
            self.answer(conn, message.get('reply') or {'ok': False, 'error': 'no reply'})
        self.next()

    def answer(self, conn, reply):
        """Queues a reply for its script, which takes it when it reads."""
        if conn not in self.outgoing:
            return
        self.outgoing[conn] += json.dumps(reply).encode() + b'\n'
        self.flush(conn)
        if conn in self.outgoing and len(self.outgoing[conn]) > MAX_PENDING:
            self.drop(conn)  # a script that never reads its replies

    def flush(self, conn):
        pending = self.outgoing.get(conn)
        try:
            while pending:
                del pending[:conn.send(pending)]
        except BlockingIOError:
            pass
        except OSError:
            self.drop(conn)

    def writers(self):
        return [conn for conn, pending in self.outgoing.items() if pending]

    def drop(self, conn):
        if conn not in self.clients:
            conn.close()
            return
        self.clients.pop(conn, None)
        self.outgoing.pop(conn, None)
        self.queue = [(waiting, request) for waiting, request in self.queue if waiting is not conn]
        # A call already with Spindle still holds the turn until its reply comes.
        if self.asking is not None and self.asking[1] is conn:
            self.asking = (self.asking[0], None)
        conn.close()

    def close(self):
        for conn in list(self.clients):
            self.drop(conn)
        self.server.close()


def finite(text):
    """A JSON number as a float, refused when it is none (1e999 is no number to relay)."""
    value = float(text)
    if value in (float('inf'), float('-inf')) or value != value:
        raise ValueError(f'{text} is not a finite number')
    return value


def refuse_constant(name):
    raise ValueError(f'{name} is not JSON')


class Command:
    """One running command: its namespaces' processes, its output, its scripts."""

    def __init__(self, ident, job):
        global made
        self.ident = ident
        workdir = os.path.normpath(os.path.join('/', str(job['dir']).lstrip('/')))
        if os.path.dirname(workdir) != FRAMES:
            raise ValueError(f'{job["dir"]} is not a command\'s copy')
        self.deadline = time.monotonic() + float(job.get('timeout_s') or 120)
        now = job.get('now')
        if isinstance(now, (int, float)) and abs(now - time.time()) > 1:
            try:
                time.clock_settime(time.CLOCK_REALTIME, now)
            except (OSError, OverflowError, ValueError):
                pass  # the command runs on the clock it has
        made += 1
        self.socket_path = f'{CALLS}/{made}.sock'
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            os.unlink(self.socket_path)
        except FileNotFoundError:
            pass
        server.bind(self.socket_path)
        os.chmod(self.socket_path, 0o666)
        server.listen(16)
        server.setblocking(False)
        self.bridge = Bridge(ident, server)
        self.out = self.status = None
        try:
            self.holder = self.spawn(job, workdir)
        except BaseException:
            # Nothing started: what was made for it goes.
            for descriptor in (self.out, self.status):
                if descriptor is not None:
                    os.close(descriptor)
            self.bridge.close()
            os.unlink(self.socket_path)
            raise
        self.sent = 0
        self.exited = None  # when bash exited, while something it left holds its output open
        self.code = None
        self.init = None
        self.failure = None
        self.stopped = self.timed_out = False
        self.lines = b''
        # Its init's pid, told as the helper forks it (a moment after the mounts; how bash
        # ended, or why it could not start, may come first): the command is ended by
        # ending that init, so it is waited for, unless the helper ends without one.
        end = time.monotonic() + 30
        while self.init is None and self.status is not None:
            left = end - time.monotonic()
            if left <= 0 or not select.select([self.status], [], [], left)[0]:
                self.failure = self.failure or 'its namespaces did not start'
                break
            self.read_status()
        os.set_blocking(self.out, False)

    def spawn(self, job, workdir):
        """Starts the helper that puts the command in namespaces of its own; its pid."""
        network = None if job.get('network') else (spare_networks.pop() if spare_networks else new_network())
        env = {**ENV, **(THROUGH_CHROME if job.get('network') == 'chrome' else {})}
        given = [] if network is None else [network]
        high = []
        try:
            self.out, out = os.pipe()
            given.append(out)
            self.status, status = os.pipe()
            given += [status, os.open('/dev/null', os.O_RDONLY)]
            # Where the helper finds them (0-4); the sources above that, so none is its own.
            for descriptor in [given[-1], out, out, status] + ([network] if network is not None else []):
                high.append(fcntl.fcntl(descriptor, fcntl.F_DUPFD_CLOEXEC, 10))
            return os.posix_spawn(
                SANDBOX, [SANDBOX, workdir, self.socket_path, '-' if network is None else '4', str(job['command'])], env,
                file_actions=[(os.POSIX_SPAWN_DUP2, source, target) for target, source in enumerate(high)],
                setsigdef=(signal.SIGPIPE, signal.SIGXFSZ),
            )
        finally:
            for descriptor in {*given, *high}:
                os.close(descriptor)

    def read_status(self):
        data = os.read(self.status, 512)
        if not data:
            os.close(self.status)
            self.status = None
            return
        self.lines += data
        while b'\n' in self.lines:
            line, self.lines = self.lines.split(b'\n', 1)
            kind, _, value = line.decode(errors='replace').partition(' ')
            if kind == 'init':
                self.init = int(value)
            elif kind == 'exit':
                self.code = int(value)
                self.exited = time.monotonic()
            elif kind == 'fail':
                self.failure = value

    def readers(self):
        return [self.bridge.server, *self.bridge.clients] + [fd for fd in (self.out, self.status) if fd is not None]

    def writers(self):
        return self.bridge.writers()

    def wait(self, now):
        """How long until this command must be looked at whatever comes."""
        left = self.deadline - now
        if self.exited is not None:
            left = min(left, DRAIN_S - (now - self.exited))
        return max(left, 0)

    def step(self, ready, writable, now):
        """Takes what is ready for it; True once it is over."""
        for conn in [conn for conn in writable if conn in self.bridge.outgoing]:
            self.bridge.flush(conn)
        if self.status is not None and self.status in ready:
            self.read_status()
        if self.bridge.server in ready:
            self.bridge.accept()
        for conn in [conn for conn in ready if conn in self.bridge.clients]:
            self.bridge.read(conn)
        if self.out is not None and self.out in ready:
            try:
                data = os.read(self.out, CHUNK)
            except BlockingIOError:
                data = None
            if data == b'':
                os.close(self.out)
                self.out = None
            elif data and self.sent < MAX_OUT:
                send('OUT', self.ident, data[:MAX_OUT - self.sent])
                self.sent += len(data[:MAX_OUT - self.sent])
        if self.failure is not None or self.stopped:
            return True
        if now >= self.deadline:
            self.timed_out = True
            return True
        # Bash is done: once its output is (or a moment after, for what it left holding
        # it). Output done while bash runs on means waiting for bash.
        if self.exited is not None and (self.out is None or now - self.exited >= DRAIN_S):
            return True
        # Its holder gone without bash's end (killed from outside): over.
        return self.status is None and self.exited is None

    def finish(self):
        """Ends every process of its namespaces, lets its writes reach the share and
        reports how it ended."""
        for step in (lambda: self.init and os.kill(self.init, signal.SIGKILL),
                     lambda: self.init is None and os.kill(self.holder, signal.SIGKILL),
                     lambda: os.waitpid(self.holder, 0), self.bridge.close,
                     lambda: os.unlink(self.socket_path), os.sync):
            try:
                step()
            except Exception:
                pass
        for descriptor in (self.out, self.status):
            if descriptor is not None:
                os.close(descriptor)
        if self.failure is not None:
            send('OUT', self.ident, f'spindle runner: {self.failure}\n'.encode())
            result = {'code': 126, 'timed_out': False, 'stopped': False}
        else:
            # Killed before bash ended: as a killed process reports.
            code = self.code if self.code is not None else -signal.SIGKILL
            result = {'code': code, 'timed_out': self.timed_out, 'stopped': self.stopped}
        send('END', self.ident, json.dumps(result).encode())


def wait_for_proxy(seconds=120):
    """The web proxy listens before the agent says it is ready (the machine is saved,
    and resumed, as it is then)."""
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        try:
            socket.create_connection(('127.0.0.1', 3128), timeout=1).close()
            return
        except OSError:
            time.sleep(0.2)


def answer(kind, ident, payload, commands):
    if kind == 'PING':
        send('READY', ident)
    elif kind == 'RUN':
        if ident in commands:
            return
        try:
            commands[ident] = Command(ident, json.loads(payload))
        except Exception as error:  # the browser hears of it; the agent goes on
            send('OUT', ident, f'spindle runner: {error}\n'.encode())
            send('END', ident, json.dumps({'code': 126, 'timed_out': False, 'stopped': False}).encode())
    elif kind == 'STOP' and ident in commands:
        commands[ident].stopped = True
    elif kind == 'REPLY' and ident in commands:
        try:
            commands[ident].bridge.reply(payload)
        except Exception as error:  # that command fails; the others go on
            commands[ident].failure = f'its Spindle call failed here: {error}'


def main():
    wait_for_proxy()
    # Ready before the machine is saved: a resumed one has it at once.
    spare_network()
    send('READY', 0)
    commands = {}
    while True:
        now = time.monotonic()
        waits = [command.wait(now) for command in commands.values()]
        readers = [fd] + [reader for command in commands.values() for reader in command.readers()]
        writers = [writer for command in commands.values() for writer in command.writers()]
        ready, writable, _ = select.select(readers, writers, [], min(waits) if waits else None)
        if fd in ready:
            for frame in frames():
                answer(*frame, commands)
        now = time.monotonic()
        for ident, command in list(commands.items()):
            try:
                over = command.step(ready, writable, now)
            except Exception as error:  # that command fails; the others go on
                command.failure = f'the runner failed it: {error}'
                over = True
            if over:
                del commands[ident]
                command.finish()
        # Idle (nothing more has come): the next command's network made meanwhile.
        if not spare_networks and not select.select([fd], [], [], 0)[0]:
            spare_network()


main()
