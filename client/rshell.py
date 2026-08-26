#!/usr/bin/env python3
r"""An interactive shell on the phone, over Bluetooth RFCOMM.

    rshell.py                      find the phone and the channel by itself
    rshell.py nokia                a paired device by a piece of its name
    rshell.py 00:1B:AF:12:34:56 13 an address and a channel, when you want no guessing

The channel the daemon listens on is allocated at boot, so it moves; it is also *advertised*,
and that is what we ask. SDP first (instant), then the channel that worked last time, then a
threaded scan of 1..30. You should never have to read a channel off the phone screen again.

Phone commands (sent to the agent):
    ls [path]              list a directory (subdirectories show a trailing \)
    cd <path> | pwd        move around
    stat <path>            size, modification date and attributes, without pulling the file
    cat <path>             read a whole file
    cat <path> <off> <len> read a byte range — how to look inside a binary over the 1 MB cap
    find <dir> <substr>    recursive filename search (case-insensitive)
    grep <dir> <pattern>   search file CONTENTS in one directory. `hex:66871f10` matches raw
                           bytes (a little-endian UID); anything else matches ASCII text
    exec <path>            launch an executable on the phone and return at once
    reboot now             restart the phone by killing the system-critical file server. Nothing
                           is flushed first, so a daemon mid-write loses that write; the literal
                           word `now` is required. The reply is sent before the kill
    ps <cat> <key>         read a Publish & Subscribe integer (hex ok: ps 0x101f75b6 0x1020363a)
    cenrep <repo> <key>    read a Central Repository value, as int then as string
    hal [attr]             a HAL attribute; with no argument, the machine UID (for .rmp guards)
    rm <path> | mkdir <path> | ping | quit

Host commands (run here, on your computer):
    push <local...> [remote]  upload files; a single trailing remote path renames, a remote
                              directory or nothing keeps the local names
    pull <remote...> [dir]    download, in ranges, so file size is not a limit
    sideload <file.sis...>    put packages in C:\Data\_app_install\ — the folder that sorts to
                              the top of C:\Data in the phone's file browser, ready to tap
    logs [app] [-f]           show C:\Data\_logs\<app>.txt (default: rshelld); -f follows it,
                              reprinting as the app writes, until Ctrl-C
    lls [dir] | lcd <dir>     list / change the directory on THIS machine
    !<command>                run a shell command here
    help                      this list        exit / Ctrl-D   leave (agent keeps running)

`get`/`put` still work as aliases of `pull`/`push`. `find` and `grep` are bounded to ~2 s per
call on the phone (a long call would starve the window server and freeze the handset), so a big
sweep reports BUDGET EXPIRED — narrow the path and run it again rather than trusting a partial
answer.

A dropped link is reconnected in place: the session keeps its working directory and you keep
your history, which matters because Bluetooth drops and the daemon re-arms `accept` every 5 s.
"""

import atexit
import glob
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import btlink
from btlink import INSTALL_DIR, Link, human, log_path, read_reply, recv_frame, send_frame, try_connect

HISTORY = os.path.join(btlink.CACHE_DIR, "history")

PHONE_VERBS = [
    "ls", "cd", "pwd", "stat", "cat", "get", "put", "rm", "mkdir", "find", "grep",
    "exec", "reboot", "ps", "cenrep", "hal", "ping", "quit",
]
HOST_VERBS = ["push", "pull", "sideload", "logs", "lls", "lcd", "help", "exit"]


def setup_readline(link: "Link") -> None:
    """History across sessions, and Tab completion for verbs and remote paths."""
    try:
        import readline
    except ImportError:
        return
    try:
        os.makedirs(btlink.CACHE_DIR, exist_ok=True)
        readline.read_history_file(HISTORY)
    except OSError:
        pass
    readline.set_history_length(2000)
    atexit.register(lambda: _save_history(readline))

    # Backslash is a path separator here, not a word break, or completion would split C:\Data.
    readline.set_completer_delims(" \t\n")

    def complete(text, state):
        typing_the_verb = " " not in readline.get_line_buffer().lstrip()
        if typing_the_verb:
            options = [v + " " for v in PHONE_VERBS + HOST_VERBS if v.startswith(text)]
        else:
            options = _complete_remote(link, text)
        return options[state] if state < len(options) else None

    readline.set_completer(complete)
    readline.parse_and_bind("tab: complete")


def _save_history(readline) -> None:
    try:
        readline.write_history_file(HISTORY)
    except OSError:
        pass


def _complete_remote(link: "Link", text: str) -> list[str]:
    """Complete a remote path by listing its parent on the phone."""
    sep = text.rfind("\\")
    directory, stem = (text[: sep + 1], text[sep + 1 :]) if sep >= 0 else ("", text)
    try:
        ok, _, data = link.command(f"ls {directory or link.cwd}")
    except OSError:
        return []
    if not ok:
        return []
    names = [n.strip() for n in data.decode("utf-8", "replace").splitlines() if n.strip()]
    return [directory + n for n in names if n.lower().startswith(stem.lower())]


def show(data: bytes) -> None:
    if not data:
        return
    sys.stdout.write(data.decode("utf-8", "replace"))
    if not data.endswith(b"\n"):
        sys.stdout.write("\n")


def expand_local(patterns: list[str]) -> list[str]:
    """Globs expanded here, so `push build/*.sis` works even from a non-glob shell."""
    files = []
    for p in patterns:
        hits = sorted(glob.glob(p)) or [p]
        files += [h for h in hits if os.path.isfile(h)]
    return files


def cmd_push(link: Link, args: list[str]) -> None:
    if not args:
        return print(r"usage: push <local...> [remote]   (remote may be a dir like C:\Data)")
    # A trailing word that is not an existing local file is the destination.
    remote = None
    if len(args) > 1 and not os.path.exists(args[-1]) and not glob.glob(args[-1]):
        remote, args = args[-1], args[:-1]
    files = expand_local(args)
    if not files:
        return print(f"no such local file: {' '.join(args)}")
    for local in files:
        if remote is None:
            target = os.path.basename(local)
        elif len(files) > 1 or remote.endswith("\\") or remote.endswith(":"):
            target = remote.rstrip("\\") + "\\" + os.path.basename(local)
        else:
            target = remote
        try:
            link.put_file(local, target)
        except IOError as e:
            print(f"ERR {local}: {e}")


def cmd_pull(link: Link, args: list[str]) -> None:
    if not args:
        return print("usage: pull <remote...> [local-dir]")
    target = "."
    if len(args) > 1 and (os.path.isdir(args[-1]) or "\\" not in args[-1]):
        target, args = args[-1], args[:-1]
    for remote in args:
        try:
            link.get_file(remote, target)
        except IOError as e:
            print(f"ERR {remote}: {e}")


def cmd_sideload(link: Link, args: list[str]) -> None:
    files = expand_local(args)
    if not files:
        return print("usage: sideload <file.sis...>")
    for local in files:
        try:
            remote = link.sideload(local)
        except IOError as e:
            print(f"ERR {local}: {e}")
            continue
        print(f"  ready to install: {remote}")
    print(
        "On the phone: File mgr. > Phone memory > Data > _app_install > tap the package.\n"
        "(_app_install sorts to the top of C:\\Data, which is why it is spelled that way.)"
    )


#: How long to wait between size checks while following a log. The phone answers a `stat` in
#: milliseconds, and a diagnostic that costs a Bluetooth round trip every second is cheap; going
#: much below this spends radio to watch a file that a human is reading.
FOLLOW_POLL_S = 1.0
#: Bytes per range read while following. Far under the agent's frame limit, so a burst of
#: output arrives in a few reads rather than one that the daemon refuses.
FOLLOW_CHUNK = 32 * 1024


def cmd_logs(link: Link, args: list[str]) -> None:
    follow = any(a in ("-f", "--follow") for a in args)
    rest = [a for a in args if a not in ("-f", "--follow")]
    path = log_path(rest[0] if rest else "rshelld")

    if not follow:
        ok, text, data = link.command(f"cat {path}")
        show(data)
        print(f"{'OK' if ok else 'ERR'}: {text}")
        return

    # Follow by polling `stat` and reading only what is new. There is no push notification in
    # the protocol and there should not be one for this: the agent is single-client and
    # single-threaded, so a subscription would hold the session that everything else needs.
    print(f"--- following {path}  (Ctrl-C to stop)")
    offset = 0
    try:
        while True:
            size = link.size_of(path)
            if size is None:
                # Not there yet. An app with DEBUG=0, or one that has not logged its first
                # line — both are ordinary, so this waits rather than failing.
                time.sleep(FOLLOW_POLL_S)
                continue
            if size < offset:
                # `fs::append_capped` starts the file over at LOG_MAX rather than dropping the
                # oldest half, so a shrink is not corruption — it is the cap, and everything
                # before it is gone. Say so, because a silent jump reads as lost output.
                print("--- log restarted (it passed its size cap)")
                offset = 0
            while size > offset:
                n = min(size - offset, FOLLOW_CHUNK)
                ok, text, data = link.command(f"cat {path} {offset} {n}")
                if not ok:
                    print(f"ERR: {text}")
                    break
                show(data)
                offset += n
            time.sleep(FOLLOW_POLL_S)
    except KeyboardInterrupt:
        print()


def do_command(link: Link, line: str) -> None:
    if line.startswith("!"):
        subprocess.run(line[1:], shell=True)
        return

    parts = line.split()
    verb, args = parts[0], parts[1:]

    if verb in ("push", "put") and (verb == "push" or len(args) == 2):
        return cmd_push(link, args)
    if verb in ("pull",) or (verb == "get" and len(args) == 2):
        return cmd_pull(link, args)
    if verb == "sideload":
        return cmd_sideload(link, args)
    if verb == "logs":
        return cmd_logs(link, args)
    if verb == "lls":
        return print("\n".join(sorted(os.listdir(args[0] if args else "."))))
    if verb == "lcd":
        try:
            os.chdir(os.path.expanduser(args[0]) if args else os.path.expanduser("~"))
        except OSError as e:
            return print(e)
        return print(os.getcwd())

    ok, text, data = link.command(line)
    show(data)
    print(f"{'OK' if ok else 'ERR'}: {text}")


def main() -> None:
    args = [a for a in sys.argv[1:] if not a.startswith("-")]
    if "-h" in sys.argv or "--help" in sys.argv:
        print(__doc__)
        return
    mac = args[0] if args else None
    channel = int(args[1]) if len(args) > 1 else None

    link = Link.open(mac, channel)
    setup_readline(link)
    print("type 'help' for commands, 'exit' or Ctrl-D to leave (the agent keeps running).")
    try:
        while True:
            try:
                line = input(f"{link.cwd}> ").strip()
            except EOFError:
                print()
                break
            except KeyboardInterrupt:
                print()
                continue
            if not line:
                continue
            if line == "exit":
                break
            if line == "help":
                print(__doc__)
                continue
            try:
                do_command(link, line)
            except KeyboardInterrupt:
                # The reply is half-read, so the socket is no longer in a known state.
                print("\ninterrupted; reconnecting")
                link.reconnect()
                continue
            except (ConnectionError, OSError) as e:
                print(f"link error: {e}; reconnecting")
                link.reconnect()
                continue
            if line.split()[0] == "quit":
                break
    finally:
        link.close()


if __name__ == "__main__":
    main()
