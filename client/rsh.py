#!/usr/bin/env python3
"""Batch driver for the phone's rshell agent: retries the connect, runs commands, never quits.

    rsh.py <MAC> <channel> 'ls Z:\\sys\\bin' 'cat Z:\\foo'
    rsh.py <MAC> <channel> --get 'Z:\\sys\\bin\\sysstart.exe' out/sysstart.exe

Deliberately does NOT send 'quit' — the agent stays connectable for the next batch.
"""
import os, sys, time
sys.path.insert(0, os.path.join(os.path.dirname(__file__)))
import rshell

def connect(mac, ch, tries=12, wait=3.0):
    last = None
    for i in range(tries):
        try:
            s = rshell.try_connect(mac, int(ch), 20.0)
            s.settimeout(60.0)
            ok, text, _ = rshell.read_reply(s)   # greeting
            return s
        except OSError as e:
            last = e
            time.sleep(wait)
    sys.exit(f"could not reach the agent after {tries} tries: {last}")

def main():
    mac, ch, rest = sys.argv[1], sys.argv[2], sys.argv[3:]
    s = connect(mac, ch)
    if rest and rest[0] == "--mget":
        listfile, outdir = rest[1], rest[2]
        paths = [l.strip() for l in open(listfile) if l.strip()]
        os.makedirs(outdir, exist_ok=True)
        for i, remote in enumerate(paths, 1):
            name = remote.replace("\\", "_").replace(":", "")
            try:
                rshell.send_frame(s, f"cat {remote}".encode())
                ok, text, data = rshell.read_reply(s)
            except OSError as e:
                print(f"[{i}/{len(paths)}] link died on {remote}: {e}; reconnecting")
                s = connect(mac, ch)
                continue
            if ok:
                open(os.path.join(outdir, name), "wb").write(data)
                print(f"[{i}/{len(paths)}] {remote} {len(data)}B")
            else:
                print(f"[{i}/{len(paths)}] ERR {remote}: {text}")
        return

    if rest and rest[0] == "--get":
        remote, local = rest[1], rest[2]
        rshell.send_frame(s, f"cat {remote}".encode())
        ok, text, data = rshell.read_reply(s)
        if not ok:
            print(f"ERR {remote}: {text}"); sys.exit(2)
        os.makedirs(os.path.dirname(local) or ".", exist_ok=True)
        open(local, "wb").write(data)
        print(f"OK {remote} -> {local} ({len(data)} bytes)")
        return
    for cmd in rest:
        rshell.send_frame(s, cmd.encode())
        ok, text, data = rshell.read_reply(s)
        print(f"=== {cmd}")
        if data:
            sys.stdout.write(data.decode("utf-8", "replace"))
            if not data.endswith(b"\n"): print()
        print(f"--- {'OK' if ok else 'ERR'}: {text}")

main()
