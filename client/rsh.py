#!/usr/bin/env python3
r"""One-shot driver for the phone's rshell agent: connects, does the work, leaves it running.

    rsh.py 'ls Z:\sys\bin' 'cat C:\Data\_logs\cal.txt'
    rsh.py --device nokia 'ping'
    rsh.py --channel 13 'hal'
    rsh.py --pull 'Z:\sys\bin\sysstart.exe' out/            pull one file (any size)
    rsh.py --mpull files.txt out/                           one remote path per line
    rsh.py --push apps/cal/build/cal.sis 'C:\Data'          upload
    rsh.py --sideload apps/cal/build/cal.sis                drop it in C:\Data\_app_install\
    rsh.py --logs connd                                     print C:\Data\_logs\connd.txt
    rsh.py --logs connd -f                                  and keep printing what it adds

The device and the RFCOMM channel are discovered (paired list + SDP + cache); name them only
when you want to override that. Deliberately does NOT send `quit` — the agent stays connectable
for the next call. Exit status is non-zero if any command or transfer failed.
"""

import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import btlink
from btlink import Link, log_path, read_reply, send_frame, try_connect  # re-exported for old callers

#: Seconds between size checks while following a log. Matches rshell.py's interactive follow.
FOLLOW_POLL_S = 1.0
#: Bytes per range read while following, well under the agent's frame limit.
FOLLOW_CHUNK = 32 * 1024


def follow(link: Link, path: str) -> int:
    """Print `path` and everything appended to it until interrupted. Returns a failure count.

    Polls `stat` and reads only the new bytes: the protocol has no push notification, and it
    should not grow one for this — the agent is single-client and single-threaded, so a
    subscription would hold the session everything else needs.
    """
    offset = 0
    failures = 0
    try:
        while True:
            size = link.size_of(path)
            if size is None:
                # No file yet: DEBUG=0, or the app has not written its first line. Both are
                # ordinary, so wait rather than fail.
                time.sleep(FOLLOW_POLL_S)
                continue
            if size < offset:
                # `fs::append_capped` starts the file over at its cap rather than dropping the
                # oldest half, so a shrink is the cap, not corruption — and what came before is
                # gone. Said out loud, because a silent jump reads as lost output.
                print("--- log restarted (it passed its size cap)")
                offset = 0
            while size > offset:
                n = min(size - offset, FOLLOW_CHUNK)
                ok, text, data = link.command(f"cat {path} {offset} {n}")
                if not ok:
                    print(f"--- ERR: {text}")
                    failures += 1
                    break
                sys.stdout.write(data.decode("utf-8", "replace"))
                sys.stdout.flush()
                offset += n
            time.sleep(FOLLOW_POLL_S)
    except KeyboardInterrupt:
        print()
    return failures


def connect(mac=None, ch=None, tries=6, wait=3.0):
    """Kept for callers that used this before Link existed."""
    return Link.open(mac, ch, retries=tries).sock


def main() -> int:
    argv = sys.argv[1:]
    if not argv or argv[0] in ("-h", "--help"):
        print(__doc__)
        return 0

    mac = channel = None
    rest = []
    i = 0
    while i < len(argv):
        a = argv[i]
        if a in ("--device", "-d"):
            mac, i = argv[i + 1], i + 2
        elif a in ("--channel", "-c"):
            channel, i = int(argv[i + 1]), i + 2
        elif a == "--quiet":
            i += 1
        else:
            rest = argv[i:]
            break
    # Compatibility with the old positional form: rsh.py <MAC> <channel> 'cmd' ...
    if rest and btlink.looks_like_mac(rest[0]):
        mac = rest[0]
        rest = rest[1:]
        if rest and rest[0].isdigit():
            channel, rest = int(rest[0]), rest[1:]

    link = Link.open(mac, channel)
    failures = 0
    try:
        if rest and rest[0] in ("--pull", "--get"):
            remote, local = rest[1], rest[2] if len(rest) > 2 else "."
            try:
                link.get_file(remote, local)
            except (IOError, OSError) as e:
                print(f"ERR {remote}: {e}")
                failures += 1

        elif rest and rest[0] in ("--mpull", "--mget"):
            listfile, outdir = rest[1], rest[2]
            paths = [l.strip() for l in open(listfile) if l.strip()]
            os.makedirs(outdir, exist_ok=True)
            for n, remote in enumerate(paths, 1):
                flat = remote.replace("\\", "_").replace(":", "")
                print(f"[{n}/{len(paths)}] {remote}")
                for attempt in (1, 2):
                    try:
                        link.get_file(remote, os.path.join(outdir, flat))
                        break
                    except OSError as e:
                        if attempt == 1 and isinstance(e, (ConnectionError, TimeoutError)):
                            print(f"  link died ({e}); reconnecting")
                            link.reconnect()
                            continue
                        print(f"  ERR {remote}: {e}")
                        failures += 1
                        break

        elif rest and rest[0] == "--push":
            local, remote = rest[1], rest[2] if len(rest) > 2 else os.path.basename(rest[1])
            if remote.endswith("\\") or remote.endswith(":"):
                remote = remote.rstrip("\\") + "\\" + os.path.basename(local)
            elif "\\" in remote and link.command(f"stat {remote}")[1].endswith("<dir>"):
                remote = remote.rstrip("\\") + "\\" + os.path.basename(local)
            try:
                link.put_file(local, remote)
            except (IOError, OSError) as e:
                print(f"ERR {local}: {e}")
                failures += 1

        elif rest and rest[0] == "--logs":
            args = rest[1:]
            follow_it = any(a in ("-f", "--follow") for a in args)
            names = [a for a in args if a not in ("-f", "--follow")]
            path = log_path(names[0] if names else "rshelld")
            if follow_it:
                print(f"--- following {path}  (Ctrl-C to stop)")
                failures += follow(link, path)
            else:
                ok, text, data = link.command(f"cat {path}")
                sys.stdout.write(data.decode("utf-8", "replace"))
                if data and not data.endswith(b"\n"):
                    print()
                if not ok:
                    print(f"--- ERR: {text}")
                    failures += 1

        elif rest and rest[0] == "--sideload":
            for local in rest[1:]:
                try:
                    remote = link.sideload(local)
                    print(f"  ready to install: {remote}")
                except (IOError, OSError) as e:
                    print(f"ERR {local}: {e}")
                    failures += 1
            if not failures:
                print(
                    "On the phone: File mgr. > Phone memory > Data > _app_install > tap it."
                )

        else:
            for cmd in rest:
                ok, text, data = link.command(cmd)
                print(f"=== {cmd}")
                if data:
                    sys.stdout.write(data.decode("utf-8", "replace"))
                    if not data.endswith(b"\n"):
                        print()
                print(f"--- {'OK' if ok else 'ERR'}: {text}")
                failures += 0 if ok else 1
    finally:
        link.close()
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
