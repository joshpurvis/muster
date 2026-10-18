"""Private fixture-only Unix proxy for deterministic terminal protocol races.

Renames only the supplied fixture socket, pauses one spawn request/response, and
restores the socket on exit. Never discovers an instance or signals a process.
"""
import json
from pathlib import Path
import select
import socket
import struct
import threading


def frame(stream):
    def read(size):
        data = bytearray()
        while len(data) < size:
            part = stream.recv(size - len(data))
            if not part:
                raise EOFError("proxy peer disconnected")
            data.extend(part)
        return bytes(data)
    header = read(5)
    size = struct.unpack(">I", header[1:])[0]
    assert size <= 16 * 1024 * 1024
    return header + read(size)


class SpawnGate:
    def __init__(self, fixture_runtime, phase):
        assert phase in ("before", "after")
        self.path = Path(fixture_runtime) / "terminal.sock"
        # This class is deliberately restricted to the integration fixture tree.
        assert self.path.parent.parent.name.startswith("muster-backend-smoke-")
        self.upstream = self.path.with_name("terminal-fault-upstream.sock")
        self.phase = phase
        self.entered = threading.Event()
        self.release = threading.Event()
        self.stopping = threading.Event()
        self.lock = threading.Lock()
        self.selected = False
        self.request = self.response = None
        self.errors = []
        self.workers = []

    def __enter__(self):
        assert self.path.is_socket() and not self.upstream.exists()
        self.path.rename(self.upstream)
        self.listener = socket.socket(socket.AF_UNIX)
        try:
            self.listener.bind(str(self.path))
            self.path.chmod(0o600)
            self.listener.listen(128)
            self.listener.settimeout(0.1)
        except BaseException:
            self.listener.close()
            self.path.unlink(missing_ok=True)
            self.upstream.rename(self.path)
            raise
        self.acceptor = threading.Thread(target=self.accept)
        self.acceptor.start()
        return self

    def accept(self):
        while not self.stopping.is_set():
            try:
                client, _ = self.listener.accept()
            except TimeoutError:
                continue
            except OSError:
                return
            worker = threading.Thread(target=self.forward, args=(client,))
            self.workers.append(worker)
            worker.start()

    def forward(self, client):
        try:
            with client, socket.socket(socket.AF_UNIX) as upstream:
                client.settimeout(2)
                upstream.settimeout(2)
                upstream.connect(str(self.upstream))
                request = frame(client)
                command = json.loads(request[5:])["command"]
                with self.lock:
                    selected = command["method"] == "spawn" and not self.selected
                    if selected:
                        self.selected = True
                        self.request = command["params"]
                if selected and self.phase == "before":
                    self.entered.set()
                    assert self.release.wait(12), "fixture did not release the spawn gate"
                upstream.sendall(request)
                response = frame(upstream)
                if selected and self.phase == "after":
                    self.response = json.loads(response[5:])
                    self.entered.set()
                    assert self.release.wait(12), "fixture did not release the acknowledgement gate"
                client.sendall(response)
                if command["method"] in ("events", "attach", "input_stream"):
                    while not self.stopping.is_set():
                        readable, _, _ = select.select([client, upstream], [], [], 0.1)
                        for source in readable:
                            data = source.recv(64 * 1024)
                            if not data:
                                return
                            (upstream if source is client else client).sendall(data)
        except (EOFError, BrokenPipeError, ConnectionResetError):
            pass  # Killing a fixture backend deliberately disconnects its requests.
        except Exception as error:
            self.errors.append(error)

    def __exit__(self, kind, value, traceback):
        self.release.set()
        self.stopping.set()
        self.acceptor.join(timeout=3)
        self.listener.close()
        for worker in self.workers:
            worker.join(timeout=3)
        self.path.unlink(missing_ok=True)
        self.upstream.rename(self.path)
        assert not self.acceptor.is_alive() and not any(worker.is_alive() for worker in self.workers)
        if kind is None:
            assert not self.errors, self.errors
