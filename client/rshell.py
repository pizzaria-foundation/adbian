#!/usr/bin/env python3
r"""Drive the phone's remote-shell agent (apps/rshell) over Bluetooth RFCOMM.

The phone runs the "RFCOMM shell" app (it shows its channel on screen); this connects to it
and gives you an interactive shell to browse the filesystem, read files and write them.

    python3 tools/rshell.py <MAC> [channel]

No PyBluez needed — Linux exposes RFCOMM through the standard socket module. If you omit the
channel, it scans 1..30 for the one answering the agent's greeting (slower; passing the channel
shown on the phone screen is instant). The phone must be paired.

Wire protocol (see apps/rshell/src/lib.rs): length-prefixed frames, [u32 big-endian len][body].
A command to the phone is a UTF-8 line; a reply is a 1-byte tag then data — '+' OK, '-' ERR,
'.' a chunk of listing/file bytes. A response is zero or more '.' frames then one '+'/'-'.

Interactive commands:
    ls [path]              list a directory (subdirectories show a trailing \)
    cd <path> | pwd        move around
    stat <path>            size, modification date and attributes, without pulling the file
    cat <path>             read a whole file
    cat <path> <off> <len> read a byte range — how to look inside a binary over the 1 MB cap
    find <dir> <substr>    recursive filename search (case-insensitive)
    grep <dir> <pattern>   search file CONTENTS in one directory. `hex:66871f10` matches raw
                           bytes (a little-endian UID); anything else matches ASCII text
    exec <path>            launch an executable on the phone and return at once
    reboot now             restart the phone by killing the system-critical SysAp. Nothing is
                           flushed first, so a daemon mid-write loses that write; the literal
                           word `now` is required. The reply is sent before the kill
    ps <cat> <key>         read a Publish & Subscribe integer (hex ok: ps 0x101f75b6 0x1020363a)
    cenrep <repo> <key>    read a Central Repository value, as int then as string
    hal [attr]             a HAL attribute; with no argument, the machine UID (for .rmp guards)
    rm <path> | mkdir <path> | ping | quit
    get <remote> <local>   pull a file off the phone into a local file
    put <local> <remote>   push a local file to the phone
    help                   this list        exit / Ctrl-D   leave (agent keeps running)

`find` and `grep` are bounded to ~2 s per call on the phone (a long call would starve the
window server and freeze the handset), so a big sweep reports BUDGET EXPIRED — narrow the path
and run it again rather than trusting a partial answer.
"""

import os
import socket
import struct
import sys
import time

AF_BLUETOOTH = socket.AF_BLUETOOTH
BTPROTO_RFCOMM = socket.BTPROTO_RFCOMM

GREETING = b"rshell ready"


def send_frame(sock, body: bytes) -> None:
    sock.sendall(struct.pack(">I", len(body)) + body)


def recv_exactly(sock, n: int) -> bytes:
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("link closed")
        buf.extend(chunk)
    return bytes(buf)


def recv_frame(sock) -> bytes:
    (length,) = struct.unpack(">I", recv_exactly(sock, 4))
    return recv_exactly(sock, length)


def read_reply(sock):
    """Read frames until a terminating '+' or '-'. Returns (ok: bool, text, data: bytes)."""
    data = bytearray()
    while True:
        frame = recv_frame(sock)
        if not frame:
            continue
        tag, body = frame[:1], frame[1:]
        if tag == b".":
            data.extend(body)
        elif tag == b"+":
            return True, body.decode("utf-8", "replace"), bytes(data)
        elif tag == b"-":
            return False, body.decode("utf-8", "replace"), bytes(data)
        # Unknown tag: ignore, keep reading.


def try_connect(mac: str, channel: int, timeout: float):
    s = socket.socket(AF_BLUETOOTH, socket.SOCK_STREAM, BTPROTO_RFCOMM)
    s.settimeout(timeout)
    s.connect((mac, channel))
    return s


def open_session(mac: str, channel):
    """Connect and swallow the agent's one-line greeting. Returns a connected socket."""
    if channel is not None:
        print(f"connecting to {mac} channel {channel} ...")
        s = try_connect(mac, int(channel), 20.0)
    else:
        print(f"no channel given; scanning 1..30 on {mac} (pass the channel on screen to skip) ...")
        s = None
        for ch in range(1, 31):
            try:
                cand = try_connect(mac, ch, 3.0)
            except OSError:
                continue
            try:
                cand.settimeout(2.0)
                frame = recv_frame(cand)
                if frame[1:].startswith(GREETING):
                    print(f"  found the agent on channel {ch}")
                    cand.settimeout(None)
                    return cand
            except OSError:
                pass
            cand.close()
        if s is None:
            sys.exit("no agent answered on channels 1..30 — is the RFCOMM shell app open?")
    # Channel was given: read and show the greeting.
    s.settimeout(None)
    ok, text, _ = read_reply(s)
    print(f"  {'OK' if ok else 'ERR'}: {text}")
    return s


def do_command(sock, line: str, prompt_box: list) -> None:
    parts = line.split()
    verb = parts[0]

    if verb == "get":
        if len(parts) != 3:
            print("usage: get <remote> <local>")
            return
        remote, local = parts[1], parts[2]
        send_frame(sock, f"cat {remote}".encode("utf-8"))
        ok, text, data = read_reply(sock)
        if not ok:
            print(f"ERR: {text}")
            return
        with open(local, "wb") as f:
            f.write(data)
        print(f"OK: {text} -> {local} ({len(data)} bytes)")
        return

    if verb == "put":
        if len(parts) != 3:
            print("usage: put <local> <remote>")
            return
        local, remote = parts[1], parts[2]
        if not os.path.isfile(local):
            print(f"no such local file: {local}")
            return
        with open(local, "rb") as f:
            payload = f.read()
        send_frame(sock, f"put {remote} {len(payload)}".encode("utf-8"))
        ok, text, _ = read_reply(sock)
        if not ok:
            print(f"ERR: {text}")
            return
        if payload:
            send_frame(sock, payload)
            ok, text, _ = read_reply(sock)
        print(f"{'OK' if ok else 'ERR'}: {text}")
        return

    # Plain passthrough command.
    send_frame(sock, line.encode("utf-8"))
    ok, text, data = read_reply(sock)
    if data:
        sys.stdout.write(data.decode("utf-8", "replace"))
        if not data.endswith(b"\n"):
            sys.stdout.write("\n")
    print(f"{'OK' if ok else 'ERR'}: {text}")
    # pwd/cd report the working directory in their OK text — use it as the prompt.
    if ok and verb in ("pwd", "cd"):
        prompt_box[0] = text


def main() -> None:
    if len(sys.argv) not in (2, 3):
        sys.exit(f"usage: {sys.argv[0]} <MAC> [channel]")
    mac = sys.argv[1]
    channel = sys.argv[2] if len(sys.argv) == 3 else None

    sock = open_session(mac, channel)
    print("connected. type 'help' for commands, 'exit' or Ctrl-D to leave.")
    prompt_box = ["Z:\\"]
    try:
        while True:
            try:
                line = input(f"{prompt_box[0]}> ").strip()
            except EOFError:
                print()
                break
            if not line:
                continue
            if line in ("exit",):
                break
            if line == "help":
                print(__doc__)
                continue
            try:
                do_command(sock, line, prompt_box)
            except (ConnectionError, OSError) as e:
                sys.exit(f"link error: {e} (the agent may have dropped; reopen it and reconnect)")
            if line.split()[0] == "quit":
                break
    finally:
        try:
            sock.close()
        except OSError:
            pass


if __name__ == "__main__":
    main()
