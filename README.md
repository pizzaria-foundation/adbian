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

This is the one part that cannot use ADBian, because ADBian is what is being installed. The
agent has to reach the phone some other way, and the SDK's `serve.py` is that way: it serves a
directory over HTTP with the `application/vnd.symbian.install` MIME type, which is what makes
the phone's browser hand the file to the installer instead of saving an unknown blob.

```sh
# 1. Pair once (BT 2.0, legacy PIN)
python3 ../SDK/tools/btpair.py <MAC> 0000

# 2. Build the agent (signed; autostarts on boot)
../SDK/tools/epoc build apps/rshelld

# 3. (optional) the on-phone panel
../SDK/tools/epoc build apps/rshell

# 4. Serve them, then open http://<this-machine>:8000/ in the phone's browser
python3 ../SDK/tools/serve.py apps/rshelld/build 8000
```

Install both on the phone (App manager) and reboot. `rshelld` comes up on its own — you never
start it by hand.

**Updating the agent is the one install that cannot be done casually.** The installer will not
replace the image of a running process, and `rshelld` is running — it is what `sideload` talks to.
So: sideload the new package, then on the phone install it **last**, after everything else, and
reboot. If App mgr. refuses, the daemon is still up; reboot first and install before anything
reconnects to it. Nothing else in this project has that constraint, because nothing else in this
project is the channel it arrives through.

Once the agent is on the phone, nothing needs the browser again: `sideload` puts every later
build in `C:\Data\_app_install\`, which is one tap in File mgr. (The SDK used to carry an OBEX
object push for this bootstrap. It went: it buried every package in Messaging, and the E72 drops
its ACL link after each transfer, so every second push failed as though the phone had no OBEX
at all.)

---

## Quick start

```sh
# interactive — no address, no channel: it finds the phone and the channel itself
python3 client/rshell.py
  Z:\> cd C:\Data
  C:\Data> ls
  C:\Data> pull C:\Data\_logs\rshelld.txt .
  C:\Data> sideload ../cal/apps/cal/build/cal.sis
  C:\Data> exit          # leaves; the agent keeps running

# one-shot (no prompt; agent stays connectable for the next call)
python3 client/rsh.py 'ls C:\sys\bin' 'stat C:\Data\log.txt'
python3 client/rsh.py --pull 'Z:\sys\bin\startup.exe' out/
python3 client/rsh.py --mpull files.txt out/         # one remote path per line
python3 client/rsh.py --sideload build/cal.sis

# from the SDK's front door, which is usually where you already are
../SDK/tools/epoc sh 'ping'
../SDK/tools/epoc sideload apps/cal/build/cal.sis
```

Override the guesses when you want to: `rshell.py nokia` (a piece of a paired device's name),
`rshell.py 3C:F7:2A:6B:0B:4A 13`, or `rsh.py --device nokia --channel 13 'ping'`.

**Finding the phone.** With no argument the client takes the device that answered last, or the
only paired one, or — with several paired — the one whose SDP says it is running the agent.

**Finding the channel.** The RFCOMM channel is allocated at boot, so it moves; it is also
*advertised*, as an SPP service record named `rshell`. The client asks SDP for it (a
ServiceSearchAttributeRequest spoken straight over an L2CAP socket — no `sdptool`, no PyBluez),
which answers in well under a second. Failing that it tries the channel that worked last time,
and failing *that* it scans 1..30 in ten threads. Whatever answered is cached in
`~/.cache/adbian/devices.json`. Reading the channel off the phone screen is no longer part of
anyone's day.

### Sideloading

A package pushed over OBEX (`epoc push`) lands in the phone's **Inbox** — fine for one file,
tedious when you are reinstalling a build every ten minutes. `sideload` puts it in
`C:\Data\_app_install\` instead:

```sh
../SDK/tools/epoc sideload apps/cal/build/cal.sis
#   push …/cal.sis -> C:\Data\_app_install\cal.sis (191 KB)
#   ready to install: C:\Data\_app_install\cal.sis
```

Then on the phone: **File mgr. > Phone memory > Data > _app_install >** tap the package. The
leading underscore is the point — it sorts the folder to the top of `C:\Data`, so getting there
is two keypresses rather than a hunt. The installer still has to be tapped: the agent can write
files, but nothing here asks `SWInstall` to install one.

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
| `reboot now` | Hard-reset the phone by killing the file server (a genuine kernel reset). The literal word `now` is required; nothing is flushed, and the reply is sent before the reset. |
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
| `rshell.py` | `pull <remote…> [dir]` | Download. Big files come down in `cat` ranges, so size is not a limit. |
| `rshell.py` | `push <local…> [remote]` | Upload; globs expand here, a remote directory keeps the local names. |
| `rshell.py` | `sideload <file.sis…>` | Upload into `C:\Data\_app_install\`, ready to tap. |
| `rshell.py` | `logs [app] [-f]` | Show `C:\Data\_logs\<app>.txt` (default `rshelld`). `-f` follows it — it polls `stat` and reads only what is new, so it also reports the restart when the file passes its size cap. |
| `rshell.py` | `lls [dir]` / `lcd <dir>` / `!<cmd>` | Look around, and run things, on *this* machine. |
| `rshell.py` | `help` / `exit` | Command list / leave (the agent keeps running). |
| `rsh.py` | `--pull <remote> [dir]` | Single download (creates the parent dir). |
| `rsh.py` | `--mpull <listfile> <outdir>` | Download every remote path in `<listfile>`, reconnecting on a dropped link. |
| `rsh.py` | `--push <local> [remote]` | Single upload. |
| `rsh.py` | `--sideload <file…>` | Into `C:\Data\_app_install\`. |

`get`/`put` still work as aliases of `pull`/`push`, and `--get`/`--mget` as aliases of
`--pull`/`--mpull`, so old notes and scripts keep working. A dropped link is reconnected in
place: the interactive session keeps its working directory and your history, which matters
because Bluetooth drops and the daemon re-arms `accept` every 5 s.

---

## The on-phone panel

Open the **rshell** app on the handset to see a live dashboard of the daemon:

- **Daemon state** — `stopped` / `listening (ch N)` / `client connected (ch N)`.
- **Counters** — commands handled, clients accepted.
- **Recent activity** — the tail of `C:\Data\_logs\rshelld.txt`.
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
client/         btlink.py (discovery, framing, transfers) + rshell.py (interactive) + rsh.py
```

Depends on `../SDK` by path. The RFCOMM shim (`shim/src/shim_btsock.cpp`, `shim_bt.cpp`) and the
Rust binding (`crates/symbian/src/bt.rs`) live in the SDK — they are toolkit, not this app.
