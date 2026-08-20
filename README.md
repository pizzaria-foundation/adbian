# ADBian

**An ADB-style remote shell and file bridge for Symbian S60, over Bluetooth RFCOMM.**

Like `adb` for an old Nokia: from your computer you get a shell on the handset — browse and read
files, push and pull, run a binary, read Publish & Subscribe / Central Repository / HAL values,
reboot — all over Bluetooth, with no cable and nothing to unlock on the phone.

Built on the [epoc SDK](../SDK) (its C++ shim, toolchain and Symbian crates), reached by path as
the sibling checkout `../SDK`.

---

## How it works

```
   your machine                         Bluetooth RFCOMM (SPP)                the phone
 ┌───────────────┐                                                   ┌──────────────────────────┐
 │ client/        │                                                  │ apps/rshelld  (daemon)   │
 │  rshell.py  ───┼──────────  [u32 len][command]  ───────────────▶  │  the ONE RFCOMM listener │
 │  rsh.py        │  ◀───────  [tag][data] frames  ───────────────   │  autostarts at boot      │
 └───────────────┘                                                   │  publishes state (P&S)   │
                                                                      └───────────┬──────────────┘
                                                                    reads P&S +   │ (no BT listener)
                                                                    tails the log │
                                                                      ┌───────────▼──────────────┐
                                                                      │ apps/rshell   (GUI panel)│
                                                                      │  live dashboard on-screen│
                                                                      └──────────────────────────┘
```

- **`rshelld`** — the agent. A headless daemon: no window group, invisible to the task list,
  autostarts at boot and survives the "close background apps" broadcast. It is the **only** thing
  that binds an RFCOMM server channel and speaks the protocol. It also **publishes its state** over
  Publish & Subscribe so the on-phone panel can show it.
- **`rshell`** — the GUI **panel**. A read-only dashboard: it opens **no** BT listener, it just
  reads the daemon's P&S state and tails its log. Because it never binds RFCOMM, **you can open it
  while the daemon is running** — the two coexist.
- **`client/`** — what you run on your computer to actually drive the phone.

> One agent, two on-phone shapes: `rshelld` does the work, `rshell` shows what it's doing. Don't
> expect the panel to serve connections — that's the daemon's job.

---

## Setup

Pairing and OBEX push use the SDK's shared Bluetooth tools.

```sh
# 1. Pair once (BT 2.0, legacy PIN)
python3 ../SDK/tools/btpair.py <MAC> 0000

# 2. Build and push the agent (signed; autostarts on boot)
../SDK/tools/epoc build apps/rshelld
python3 ../SDK/tools/btpush.py <MAC> apps/rshelld/build/rshelld.sisx

# 3. (optional) the on-phone panel
../SDK/tools/epoc build apps/rshell
python3 ../SDK/tools/btpush.py <MAC> apps/rshell/build/rshell.sis
```

Install both on the phone (App manager) and reboot. `rshelld` comes up on its own — you never
start it by hand.

---

## Quick start

```sh
# interactive
python3 client/rshell.py <MAC> [channel]
  Z:\> cd C:\Data
  C:\Data> ls
  C:\Data> get C:\Data\logs_rshelld.txt ./log.txt
  C:\Data> exit          # leaves; the agent keeps running

# batch (no prompt; agent stays connectable for the next call)
python3 client/rsh.py <MAC> <channel> 'ls C:\sys\bin' 'stat C:\Data\log.txt'
python3 client/rsh.py <MAC> <channel> --get 'Z:\sys\bin\startup.exe' out/startup.exe
python3 client/rsh.py <MAC> <channel> --mget files.txt out/     # one remote path per line
```

**Finding the channel.** The RFCOMM channel is allocated at runtime and advertised as the SPP
record named `rshell`. Pass it if you know it; omit it in `rshell.py` to scan 1..30; or read it
from the phone panel / `C:\Data\logs_rshelld.txt` (`listening, RFCOMM channel N`).

---

## Command reference

Sent to the agent, one line each. Paths use `\` and drive letters (`Z:` = ROM, `C:` = phone,
`E:` = card); a bare name is relative to the current directory (starts at `Z:\`).

### Navigation
| Command | What it does |
|---|---|
| `pwd` | Print the working directory. |
| `cd <path>` | Change directory. The new path becomes the prompt. |
| `ls [path]` | List a directory; sub-directories get a trailing `\`. |
| `stat <path>` | Size, modified time and attributes of one entry — no transfer. |

### Files
| Command | What it does |
|---|---|
| `cat <path>` | Send the whole file (up to 1 MB). |
| `cat <path> <off> <len>` | Send a byte range — the way to read a big binary in pieces. |
| `get <path> [off len]` | Alias of `cat` on the agent (raw read). *(The host `get` verb wraps this and writes a local file — see below.)* |
| `put <path> <len>` | Receive `<len>` bytes into `<path>` (used by the host `put` verb). |
| `rm <path>` | Delete a file. |
| `mkdir <path>` | Create a directory. |

### Search
| Command | What it does |
|---|---|
| `find <dir> <substr>` | Recursive, case-insensitive filename search. Budgeted (~2 s); over-budget replies end with `BUDGET EXPIRED — narrow the path`, and the partial result is not complete. |
| `grep <dir> <pattern>` | Content search within one directory. `grep <dir> hex:66871f10` matches raw bytes. Same time budget as `find`. |

### System
| Command | What it does |
|---|---|
| `exec <path>` | Spawn an executable on the phone; returns immediately (fire-and-forget). |
| `reboot now` | Restart the phone (kills the system app). The literal word `now` is required; the reply is sent before the kill. |
| `ps <cat> <key>` | Read a Publish & Subscribe integer. Hex accepted, e.g. `ps 0xE0AA00F2 1`. |
| `cenrep <repo> <key>` | Read a Central Repository value (reports it as int then string). |
| `hal [attr]` | Read a HAL attribute; bare `hal` returns the machine UID. |

### Misc
| Command | What it does |
|---|---|
| `ping` | `pong`. Liveness check. |
| `quit` | End the agent's session (`bye`). |

### Host-side verbs (client only)
These run on your computer, not the phone.

| Where | Verb | What it does |
|---|---|---|
| `rshell.py` | `get <remote> <local>` | `cat` the remote file and write it to a local file. |
| `rshell.py` | `put <local> <remote>` | Push a local file to the phone (`put` protocol). |
| `rshell.py` | `help` / `exit` | Command list / leave (the agent keeps running). |
| `rsh.py` | `--get <remote> <local>` | Single pull (creates the parent dir). |
| `rsh.py` | `--mget <listfile> <outdir>` | Pull every remote path in `<listfile>`, reconnecting on a dropped link. |

---

## The on-phone panel

Open the **rshell** app on the handset to see a live dashboard of the daemon:

- **Daemon state** — `stopped` / `listening (ch N)` / `client connected (ch N)`.
- **Counters** — commands handled, clients accepted.
- **Recent activity** — the tail of `C:\Data\logs_rshelld.txt`.
- **Start** (left softkey) — appears only when the daemon is down, to (re)launch it.

The panel is read-only and opens no Bluetooth listener, so it is safe to run alongside the daemon.

### Publish & Subscribe contract
The daemon publishes under category `0xE0AA00F2` (its own SID, `define_public` so a different-UID
reader can read it). Anyone — the panel, or your client via `ps` — can read:

| Key | Name | Meaning |
|---|---|---|
| `1` | `STATE` | `0` down · `1` listening · `2` client connected |
| `2` | `CHANNEL` | the RFCOMM channel |
| `3` | `CMDS` | commands handled |
| `4` | `CLIENTS` | clients accepted since boot |

```sh
python3 client/rsh.py <MAC> <ch> 'ps 0xE0AA00F2 1' 'ps 0xE0AA00F2 2'
```

---

## Wire protocol

Every message is framed as `[u32 big-endian length][body]`.

- **Host → phone:** a UTF-8 command line (see the reference above).
- **Phone → host:** one tag byte then data — `+` OK (text), `-` ERR (text), `.` DATA (raw bytes).
  A reply is zero-or-more `.` frames terminated by exactly one `+` or `-`.

**Limits:** 1 MB per `cat`/`put`, 512 KB per inbound frame, ~2 s per `find`/`grep`. One client at a
time; the daemon re-arms `accept` on a 5 s supervision tick, so it recovers on its own from a
dropped link.

---

## Build

```sh
../SDK/tools/epoc build apps/rshelld     # signed daemon  -> build/rshelld.sisx
../SDK/tools/epoc build apps/rshell      # GUI panel      -> build/rshell.sis
../SDK/tools/epoc build apps/rfprobe     # RFCOMM probe   -> build/rfprobe.sis
```

Host tests: `cargo test -p rshell`.

---

## Layout

```
apps/rshell     the agent's source (one Shell) + the GUI panel (Viewer, gui feature)
apps/rshelld    the headless daemon build (reuses the rshell crate, no gui)
apps/rfprobe    a tap-to-run "can this phone be an RFCOMM server?" probe
client/         rshell.py (interactive) + rsh.py (batch, --get/--mget)
```

Depends on `../SDK` by path. The RFCOMM shim (`shim/src/shim_btsock.cpp`, `shim_bt.cpp`) and the
Rust binding (`crates/symbian/src/bt.rs`) live in the SDK — they are toolkit, not this app.
