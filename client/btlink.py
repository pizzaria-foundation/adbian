#!/usr/bin/env python3
r"""Finding the phone, and the wire to it — shared by rshell.py and rsh.py.

Three things live here, in the order a session needs them:

1. **Which phone.** `resolve_mac` takes a MAC, a piece of a name ("nokia"), or nothing at all
   and turns it into an address, using the paired-device list and a small cache of what worked
   last time. Typing an address by hand is a fallback, not the interface.

2. **Which channel.** The daemon's RFCOMM channel is allocated at boot, so it moves. It is also
   *advertised*: the daemon registers an SPP service record named `rshell`, and SDP will hand it
   over in well under a second. `find_channel` asks SDP first, falls back to the cache, and only
   then to the old brute-force scan of 1..30 — which is now threaded, so even the fallback is
   quick. No sdptool, no PyBluez: SDP is spoken directly over an L2CAP socket, because Linux
   gives us one and the request we need is forty bytes long.

3. **The link.** `Link` wraps the framing (`[u32 len][body]`) and the reply grammar (`.` data
   frames, terminated by `+`/`-`) into `command()`, plus the two transfers worth having in one
   place: `get_file` reads a file of any size by asking for byte ranges, and `put_file` streams
   one up in frames the daemon's 512 KB frame limit accepts.

`sideload` is `put_file` aimed at `C:\Data\_app_install\` — the directory the phone's own file
browser shows at the top of `C:\Data`, which is the whole reason it is spelled with a leading
underscore.
"""

import json
import os
import socket
import struct
import subprocess
import sys
import threading
import time

AF_BLUETOOTH = socket.AF_BLUETOOTH
BTPROTO_RFCOMM = socket.BTPROTO_RFCOMM
BTPROTO_L2CAP = socket.BTPROTO_L2CAP

GREETING = b"rshell ready"
SERVICE_NAME = "rshell"

#: Where a sideloaded package lands. The underscore sorts it to the top of C:\Data in the
#: phone's file browser, which is the difference between "two taps" and "scroll and hunt".
INSTALL_DIR = r"C:\Data\_app_install"

#: Where the SDK's `symbian::log!` writes: one file per app, `<app>.txt`. Must match
#: `DATA_LOG_DIR` in ../SDK/crates/symbian/src/lib.rs — this is the whole contract between an
#: app logging on the phone and anything reading it from here.
#:
#: The underscore is the same trick as INSTALL_DIR's: it sorts to the top of C:\Data in the
#: phone's own file browser, so the two directories a person opens by hand sit together.
LOG_DIR = r"C:\Data\_logs"


def log_path(app: str) -> str:
    """The remote path of an app's log. A name with a backslash in it is already a path."""
    return app if "\\" in app else f"{LOG_DIR}\\{app}.txt"

#: The daemon refuses an inbound frame larger than this (MAX_FRAME in apps/rshell/src/lib.rs),
#: so a put is streamed in pieces comfortably under it.
PUT_CHUNK = 128 * 1024
#: And it will not hold more than this for one file (MAX_FILE), which caps a single put.
MAX_PUT = 1024 * 1024
#: One `cat` range. Well inside MAX_FILE, and small enough that a stall is visible as a stall.
GET_CHUNK = 256 * 1024

CACHE_DIR = os.path.join(
    os.environ.get("XDG_CACHE_HOME") or os.path.expanduser("~/.cache"), "adbian"
)
CACHE_FILE = os.path.join(CACHE_DIR, "devices.json")


# ----------------------------------------------------------------------------- cache


def _cache_read() -> dict:
    try:
        with open(CACHE_FILE) as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


def _cache_write(data: dict) -> None:
    try:
        os.makedirs(CACHE_DIR, exist_ok=True)
        with open(CACHE_FILE, "w") as f:
            json.dump(data, f, indent=1)
    except OSError:
        pass  # A cache that cannot be written is a slower session, not a failure.


def cache_get(mac: str) -> dict:
    return _cache_read().get("devices", {}).get(mac.upper(), {})


def cache_put(mac: str, **fields) -> None:
    data = _cache_read()
    devices = data.setdefault("devices", {})
    entry = devices.setdefault(mac.upper(), {})
    entry.update(fields)
    data["last"] = mac.upper()
    _cache_write(data)


def cache_last() -> str | None:
    return _cache_read().get("last")


# ----------------------------------------------------------------------------- devices


def paired_devices() -> list[tuple[str, str]]:
    """Every paired device, as (mac, name). Empty if bluetoothctl is not around."""
    try:
        out = subprocess.run(
            ["bluetoothctl", "devices"], capture_output=True, text=True, timeout=10
        ).stdout
    except (OSError, subprocess.SubprocessError):
        return []
    devices = []
    for line in out.splitlines():
        parts = line.split(None, 2)
        if len(parts) >= 2 and parts[0] == "Device":
            devices.append((parts[1], parts[2] if len(parts) > 2 else ""))
    return devices


def looks_like_mac(s: str) -> bool:
    parts = s.split(":")
    return len(parts) == 6 and all(len(p) == 2 for p in parts)


def resolve_mac(spec: str | None, verbose: bool = True) -> str:
    """A MAC, a fragment of a paired device's name, or nothing → an address.

    With nothing: the device that answered last, or — if there is exactly one paired phone
    advertising the agent — that one. Ambiguity is reported rather than guessed at.
    """
    if spec and looks_like_mac(spec):
        return spec.upper()

    devices = paired_devices()
    if spec:
        matches = [d for d in devices if spec.lower() in d[1].lower()]
        if not matches:
            sys.exit(f"no paired device matching {spec!r} (bluetoothctl devices)")
        if len(matches) > 1:
            names = ", ".join(f"{n} ({m})" for m, n in matches)
            sys.exit(f"{spec!r} matches several devices: {names}")
        if verbose:
            print(f"device: {matches[0][1]} ({matches[0][0]})")
        return matches[0][0].upper()

    last = cache_last()
    if last:
        name = dict((m.upper(), n) for m, n in devices).get(last, "")
        if verbose:
            print(f"device: {name or 'cached'} ({last})")
        return last

    if len(devices) == 1:
        if verbose:
            print(f"device: {devices[0][1]} ({devices[0][0]})")
        return devices[0][0].upper()

    # Several paired devices and no history: ask SDP which of them is even running the agent.
    candidates = []
    for mac, name in devices:
        try:
            if any(n == SERVICE_NAME for n, _ in sdp_spp_records(mac, timeout=4.0)):
                candidates.append((mac, name))
        except OSError:
            continue
    if len(candidates) == 1:
        if verbose:
            print(f"device: {candidates[0][1]} ({candidates[0][0]}) — advertises {SERVICE_NAME}")
        return candidates[0][0].upper()
    if not candidates:
        listing = "\n".join(f"  {n} ({m})" for m, n in devices) or "  (none paired)"
        sys.exit(f"no paired device is advertising {SERVICE_NAME}:\n{listing}")
    listing = ", ".join(f"{n} ({m})" for m, n in candidates)
    sys.exit(f"several devices advertise {SERVICE_NAME}: {listing} — name one")


# ----------------------------------------------------------------------------- SDP


def _sdp_elem(b: bytes, i: int):
    """One SDP data element at `b[i:]` → (value, index just past it).

    UUIDs come back as ("uuid", int) so a protocol layer can be recognised without confusing
    the UUID 3 (RFCOMM) with the integer 3 that follows it as the channel.
    """
    hdr = b[i]
    i += 1
    kind, size_idx = hdr >> 3, hdr & 7
    if kind == 0:  # nil
        return None, i
    if kind in (1, 2, 3):  # uint, twos-complement int, uuid
        n = (1, 2, 4, 8, 16)[size_idx]
        raw = b[i : i + n]
        i += n
        if kind == 3:
            return ("uuid", int.from_bytes(raw, "big")), i
        return int.from_bytes(raw, "big", signed=(kind == 2)), i
    if kind == 5:  # bool
        v = b[i]
        i += 1
        return bool(v), i
    # text(4), url(8), sequence(6), alternative(7): sized by the low bits.
    if size_idx == 5:
        n = b[i]
        i += 1
    elif size_idx == 6:
        n = int.from_bytes(b[i : i + 2], "big")
        i += 2
    elif size_idx == 7:
        n = int.from_bytes(b[i : i + 4], "big")
        i += 4
    else:
        n = (1, 2, 4, 8, 16)[size_idx]
    body = b[i : i + n]
    i += n
    if kind in (4, 8):
        return body.decode("utf-8", "replace"), i
    items, j = [], 0
    while j < len(body):
        value, j = _sdp_elem(body, j)
        items.append(value)
    return items, i


#: Answers from this process's SDP queries. The phone's SDP server refuses a second connection
#: made immediately after the first (ECONNABORTED on the E72), and asking twice for something
#: that cannot change inside one run was the only reason we ever did.
_SDP_CACHE: dict[str, list[tuple[str, int]]] = {}


def sdp_spp_records(mac: str, timeout: float = 8.0, retries: int = 2) -> list[tuple[str, int]]:
    """Ask the phone for its Serial Port records: [(service name, RFCOMM channel), ...].

    A ServiceSearchAttributeRequest for UUID 0x1101 over the SDP L2CAP channel — the same
    question `sdptool browse` asks, without needing sdptool installed. Answered from a
    per-process cache when we already asked, and retried once when the phone slams the door.
    """
    key = mac.upper()
    if key in _SDP_CACHE:
        return _SDP_CACHE[key]
    last = None
    for attempt in range(retries):
        try:
            records = _sdp_query(mac, timeout)
        except OSError as e:
            last = e
            time.sleep(1.5)
            continue
        _SDP_CACHE[key] = records
        return records
    raise last  # type: ignore[misc]


def _sdp_query(mac: str, timeout: float) -> list[tuple[str, int]]:
    sock = socket.socket(AF_BLUETOOTH, socket.SOCK_SEQPACKET, BTPROTO_L2CAP)
    sock.settimeout(timeout)
    try:
        sock.connect((mac, 1))  # PSM 1 = SDP
        pattern = b"\x35\x03\x19\x11\x01"  # sequence(3): UUID16 0x1101 (SerialPort)
        attrs = b"\x35\x05\x0a\x00\x00\xff\xff"  # sequence(5): uint32 range 0x0000-0xffff
        cont = b"\x00"
        lists, txid = bytearray(), 1
        while True:
            params = pattern + struct.pack(">H", 0xFFFF) + attrs + cont
            sock.send(struct.pack(">BHH", 0x06, txid, len(params)) + params)
            resp = sock.recv(4096)
            txid += 1
            if len(resp) < 8 or resp[0] != 0x07:
                raise OSError("unexpected SDP response")
            count = int.from_bytes(resp[5:7], "big")
            lists += resp[7 : 7 + count]
            tail = resp[7 + count :]
            if not tail or tail[0] == 0:
                break
            cont = tail[: 1 + tail[0]]
    finally:
        sock.close()

    records, _ = _sdp_elem(bytes(lists), 0) if lists else ([], 0)
    out = []
    for record in records or []:
        if not isinstance(record, list):
            continue
        attrs_map = dict(zip(record[0::2], record[1::2]))
        channel = None
        for layer in attrs_map.get(0x0004) or []:  # ProtocolDescriptorList
            if isinstance(layer, list) and layer and layer[0] == ("uuid", 0x0003):
                channel = next((x for x in layer[1:] if isinstance(x, int)), None)
        name = attrs_map.get(0x0100)
        if channel is not None:
            out.append((name if isinstance(name, str) else "", channel))
    return out


# ----------------------------------------------------------------------------- connecting


def try_connect(mac: str, channel: int, timeout: float):
    s = socket.socket(AF_BLUETOOTH, socket.SOCK_STREAM, BTPROTO_RFCOMM)
    s.settimeout(timeout)
    s.connect((mac, channel))
    return s


def _probe(mac: str, channel: int, timeout: float) -> bool:
    """Does the agent answer on this channel? Judged by its greeting, not by the connect."""
    try:
        s = try_connect(mac, channel, timeout)
    except OSError:
        return False
    try:
        s.settimeout(timeout)
        return recv_frame(s)[1:].startswith(GREETING)
    except OSError:
        return False
    finally:
        s.close()


def scan_channels(mac: str, lo: int = 1, hi: int = 30, workers: int = 10) -> int | None:
    """The fallback: probe channels in parallel and return the lowest one that greets us."""
    found, lock = [], threading.Lock()
    channels = list(range(lo, hi + 1))

    def work(subset):
        for ch in subset:
            with lock:
                if found:
                    return
            if _probe(mac, ch, 3.0):
                with lock:
                    found.append(ch)
                return

    threads = [
        threading.Thread(target=work, args=(channels[i::workers],), daemon=True)
        for i in range(workers)
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    return min(found) if found else None


def find_channel(mac: str, verbose: bool = True) -> int:
    """SDP, then the cache, then a scan. Whatever answers is remembered for next time."""
    try:
        records = sdp_spp_records(mac)
    except OSError as e:
        records = []
        if verbose:
            print(f"  SDP unavailable ({e}); falling back")
    named = [ch for name, ch in records if name == SERVICE_NAME]
    if named:
        if verbose:
            print(f"  SDP: {SERVICE_NAME} on channel {named[0]}")
        cache_put(mac, channel=named[0])
        return named[0]

    cached = cache_get(mac).get("channel")
    if cached and _probe(mac, int(cached), 5.0):
        if verbose:
            print(f"  cached channel {cached} answers")
        return int(cached)

    if verbose:
        others = ", ".join(f"{n or '?'}:{c}" for n, c in records) or "none"
        print(f"  no {SERVICE_NAME} record (SPP records: {others}); scanning 1..30")
    ch = scan_channels(mac)
    if ch is None:
        sys.exit(
            "no agent answered — is rshelld running? (open the rshell panel on the phone, "
            "or reboot it: the daemon autostarts)"
        )
    if verbose:
        print(f"  scan: channel {ch}")
    cache_put(mac, channel=ch)
    return ch


# ----------------------------------------------------------------------------- framing


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
    """Read frames until a terminating '+' or '-'. Returns (ok, text, data)."""
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
        # Unknown tag: ignore it and keep reading.


def human(n: int) -> str:
    return f"{n/1_048_576:.1f} MB" if n >= 1_048_576 else f"{n/1024:.0f} KB" if n >= 1024 else f"{n} B"


class Link:
    """A live session with the agent, and the operations worth doing over one."""

    def __init__(self, sock, mac: str, channel: int, verbose: bool = True):
        self.sock = sock
        self.mac = mac
        self.channel = channel
        self.verbose = verbose
        self.cwd = "Z:\\"

    # -- lifecycle ---------------------------------------------------------

    @classmethod
    def open(
        cls,
        mac: str | None = None,
        channel: int | None = None,
        retries: int = 4,
        verbose: bool = True,
        timeout: float = 60.0,
    ) -> "Link":
        mac = resolve_mac(mac, verbose)
        last = None
        for attempt in range(retries):
            ch = channel if channel is not None else find_channel(mac, verbose)
            try:
                sock = try_connect(mac, int(ch), 20.0)
                sock.settimeout(timeout)
                ok, text, _ = read_reply(sock)  # the greeting
                if verbose:
                    print(f"connected on channel {ch}: {text}")
                cache_put(mac, channel=int(ch))
                return cls(sock, mac, int(ch), verbose)
            except OSError as e:
                last = e
                if verbose:
                    print(f"  connect failed ({e}); retrying {attempt + 1}/{retries}")
                # A stale cached channel must not be retried forever — re-ask SDP next round.
                if channel is None:
                    cache_put(mac, channel=None)
                time.sleep(2.0)
        sys.exit(f"could not reach the agent on {mac}: {last}")

    def reconnect(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass
        fresh = Link.open(self.mac, None, verbose=self.verbose)
        self.sock, self.channel = fresh.sock, fresh.channel
        if self.cwd != "Z:\\":
            self.command(f"cd {self.cwd}")

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass

    # -- commands ----------------------------------------------------------

    def command(self, line: str):
        """Send one command line; return (ok, text, data). Raises OSError if the link dies."""
        send_frame(self.sock, line.encode("utf-8"))
        ok, text, data = read_reply(self.sock)
        verb = line.split()[0] if line.split() else ""
        if ok and verb in ("cd", "pwd"):
            self.cwd = text
        return ok, text, data

    def size_of(self, remote: str) -> int | None:
        """The size `stat` reports, or None if the path is not there."""
        ok, text, _ = self.command(f"stat {remote}")
        if not ok:
            return None
        # "<path>  <n> bytes  <date> ..." — read the word before "bytes".
        words = text.split()
        for i, w in enumerate(words):
            if w == "bytes" and i:
                try:
                    return int(words[i - 1])
                except ValueError:
                    return None
        return None

    # -- transfers ---------------------------------------------------------

    def get_file(self, remote: str, local: str) -> int:
        """Pull `remote` into `local`, in ranges, so size is not a limit. Returns bytes written."""
        if os.path.isdir(local):
            local = os.path.join(local, remote.replace("/", "\\").split("\\")[-1])
        total = self.size_of(remote)
        parent = os.path.dirname(local)
        if parent:
            os.makedirs(parent, exist_ok=True)
        written = 0
        with open(local, "wb") as f:
            if total is None or total <= GET_CHUNK:
                ok, text, data = self.command(f"cat {remote}")
                if not ok:
                    raise IOError(text)
                f.write(data)
                written = len(data)
            else:
                while written < total:
                    want = min(GET_CHUNK, total - written)
                    # The agent finds the range by searching its own argument string for the
                    # offset word, so an offset and a length that read the same ("65536 65536")
                    # cut the path in the wrong place and it answers NotFound. Ask for one byte
                    # less rather than send a request we know it mis-parses.
                    if str(written) == str(want) and want > 1:
                        want -= 1
                    ok, text, data = self.command(f"cat {remote} {written} {want}")
                    if not ok:
                        raise IOError(text)
                    if not data:
                        break
                    f.write(data)
                    written += len(data)
                    self._progress("pull", written, total)
        self._progress_done("pull", remote, local, written)
        return written

    def put_file(self, local: str, remote: str) -> int:
        """Push `local` to `remote`, streamed in frames the daemon will accept."""
        payload = open(local, "rb").read()
        if len(payload) > MAX_PUT:
            raise IOError(
                f"{human(len(payload))} is over the agent's {human(MAX_PUT)} per-file limit"
            )
        ok, text, _ = self.command(f"put {remote} {len(payload)}")
        if not ok:
            raise IOError(text)
        if not payload:
            return 0
        sent = 0
        while sent < len(payload):
            chunk = payload[sent : sent + PUT_CHUNK]
            send_frame(self.sock, chunk)
            sent += len(chunk)
            if sent < len(payload):
                self._progress("push", sent, len(payload))
        ok, text, _ = read_reply(self.sock)  # the write result, once every byte is in
        if not ok:
            raise IOError(text)
        self._progress_done("push", local, remote, sent)
        return sent

    def sideload(self, local: str, directory: str = INSTALL_DIR) -> str:
        """Drop a package into the phone's install directory. Returns the remote path."""
        self.command(f"mkdir {directory}")  # already-there is not an error worth reporting
        remote = f"{directory}\\{os.path.basename(local)}"
        self.put_file(local, remote)
        return remote

    # -- output ------------------------------------------------------------

    def _progress(self, what: str, done: int, total: int) -> None:
        if self.verbose and sys.stdout.isatty():
            pct = 100 * done // max(total, 1)
            sys.stdout.write(f"\r  {what} {pct:3d}%  {human(done)} / {human(total)}")
            sys.stdout.flush()

    def _progress_done(self, what: str, src: str, dst: str, n: int) -> None:
        if self.verbose:
            if sys.stdout.isatty():
                sys.stdout.write("\r" + " " * 48 + "\r")
            print(f"  {what} {src} -> {dst} ({human(n)})")
