"""Multiplex a private, current-user Unix socket over SSH stdin/stdout."""
import os
import socket
import stat
import struct
import sys
import threading


def relay(mode, path, incoming=None, outgoing=None):
    incoming = incoming or sys.stdin.buffer
    outgoing = outgoing or sys.stdout.buffer
    opened, eof, closed = 0xffffffff, 0xfffffffe, 0xfffffffd
    peers, lock, writing = {}, threading.Condition(), threading.Lock()
    stopping = threading.Event()
    listener = None

    def write_all(payload):
        view = memoryview(payload)
        offset = 0
        while offset < len(view):
            written = outgoing.write(view[offset:])
            if not written:
                raise OSError('relay stream closed during write')
            offset += written
        outgoing.flush()

    def send(identifier, kind, data=b''):
        with writing:
            write_all(struct.pack('!II', identifier, kind) + data)

    def forget(identifier, notify=False):
        with lock:
            peer = peers.pop(identifier, None)
            if peer:
                lock.notify_all()
        if peer:
            try:
                peer[0].shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            peer[0].close()
            if notify:
                send(identifier, closed)

    def half_closed(identifier, direction):
        with lock:
            peer = peers.get(identifier)
            if peer:
                peer[direction] = True
                finished = peer[1] and peer[2]
            else:
                finished = False
        if finished:
            forget(identifier)

    def read_socket(identifier, client):
        try:
            while not stopping.is_set():
                data = client.recv(65536)
                if not data:
                    half_closed(identifier, 1)
                    send(identifier, eof)
                    return
                send(identifier, len(data), data)
        except (OSError, ValueError):
            try:
                forget(identifier, notify=not stopping.is_set())
            except (OSError, ValueError):
                pass

    def register(identifier, client, announce=False):
        with lock:
            if identifier in peers or (announce and len(peers) >= 4):
                client.close()
                raise RuntimeError('relay connection limit or duplicate ID')
            peers[identifier] = [client, False, False]
        client.settimeout(6)
        if announce:
            send(identifier, opened)
        threading.Thread(target=read_socket, args=(identifier, client), daemon=True).start()

    def accept():
        identifier = 0
        while not stopping.is_set():
            try:
                client, _ = listener.accept()
                with lock:
                    if not lock.wait_for(lambda: len(peers) < 4 or stopping.is_set(), timeout=1):
                        client.close()
                        continue
                    if stopping.is_set():
                        client.close()
                        return
                identifier += 1
                register(identifier, client, announce=True)
            except (OSError, ValueError, RuntimeError):
                stopping.set()
                return

    def read_exact(size):
        result = bytearray()
        while len(result) < size:
            chunk = incoming.read(size - len(result))
            if not chunk:
                if result:
                    raise RuntimeError('truncated relay frame')
                return None
            result.extend(chunk)
        return result

    try:
        if mode == 'listen':
            # The launcher creates this private directory before starting us.
            parent = os.lstat(os.path.dirname(path))
            if (not stat.S_ISDIR(parent.st_mode) or parent.st_uid != os.getuid()
                    or stat.S_IMODE(parent.st_mode) != 0o700):
                raise RuntimeError('relay requires a private current-user directory')
            listener = socket.socket(socket.AF_UNIX)
            listener.bind(path)
            os.chmod(path, 0o600)
            listener.listen(4)
            write_all(b'READY\n')
            threading.Thread(target=accept, daemon=True).start()
        elif mode != 'connect':
            raise ValueError('invalid relay mode')
        while header := read_exact(8):
            identifier, kind = struct.unpack('!II', header)
            if not identifier:
                raise RuntimeError('invalid relay connection ID')
            if kind == opened:
                if mode != 'connect':
                    raise RuntimeError('unexpected relay open')
                client = socket.socket(socket.AF_UNIX)
                try:
                    client.connect(path)
                    register(identifier, client)
                except (OSError, RuntimeError):
                    client.close()
                    send(identifier, closed)
                continue
            if kind == closed:
                forget(identifier)
                continue
            if kind != eof and not 1 <= kind <= 65536:
                raise RuntimeError('invalid relay frame size')
            data = read_exact(kind) if kind != eof else None
            if kind != eof and data is None:
                raise RuntimeError('truncated relay data')
            with lock:
                peer = peers.get(identifier)
            if peer is None:
                continue  # A socket error may race with its final frames.
            try:
                if kind == eof:
                    try:
                        peer[0].shutdown(socket.SHUT_WR)
                    except OSError:
                        pass
                    half_closed(identifier, 2)
                else:
                    peer[0].sendall(data)
            except OSError:
                forget(identifier, notify=True)
    finally:
        stopping.set()
        if listener is not None:
            listener.close()
            try:
                os.unlink(path)
            except FileNotFoundError:
                pass
        with lock:
            lock.notify_all()
            identifiers = list(peers)
        for identifier in identifiers:
            forget(identifier)


if __name__ == '__main__':
    relay(sys.argv[1], sys.argv[2])
