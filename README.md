# ADBian

An **ADB-style remote shell and file bridge for Symbian S60**, over Bluetooth RFCOMM.
Like `adb` for an Android phone: from your computer you get a shell on the handset — list and
read files, push and pull, run a binary, read Publish & Subscribe / Central Repository / HAL,
reboot — without a cable and without unlocking anything on the phone.

Built on the [epoc SDK](../SDK) (the C++ shim, toolchain and Symbian crates), reached by path as
the sibling checkout `../SDK`.

## Two halves

### `apps/` — the on-device agent (Symbian)
- **`rshell`** — the agent. One `Shell` in `src/lib.rs`; builds in two shapes from the `gui`
  feature: the tappable app (on-screen status) or, with `default-features = false`, a headless
  daemon.
- **`rshelld`** — the headless build: autostarts at boot, no window group, survives the shell's
  "close background apps" broadcast. It reconnects on its own. **Do not run rshell and rshelld at
  once** — there is one SPP channel to bind and the second listener loses.
- **`rfprobe`** — a tap-to-run go/no-go check that the handset can act as an RFCOMM server.

Build with the SDK's toolchain, e.g. `../SDK/tools/epoc build apps/rshelld`.

### `client/` — the host side (your machine)
- **`rshell.py`** — interactive client: `python3 client/rshell.py <MAC> [channel]`.
- **`rsh.py`** — batch driver (retries the connect, never sends `quit`, `--get`/`--mget`):
  `python3 client/rsh.py <MAC> <channel> 'ls C:\sys\bin' 'cat C:\Data\log.txt'`.

Pairing and OBEX push live in the SDK's shared BT tools (`../SDK/tools/btpair.py`,
`../SDK/tools/btpush.py`).

## Wire protocol
Framed as `[u32 big-endian length][body]`.
- Host → phone: a UTF-8 command line.
- Phone → host: one tag byte + data — `+` OK(text), `-` ERR(text), `.` DATA chunk. A reply is
  zero-or-more `.` frames terminated by one `+`/`-`.

Commands: `ls cd pwd stat cat find grep exec reboot ps cenrep hal rm mkdir ping quit`, plus the
host-side `get`/`put`. `cat <path> <off> <len>` reads a byte range; `grep <dir> hex:<bytes>`
matches raw bytes. Caps: `LocalServices AllFiles ReadDeviceData WriteDeviceData PowerMgmt`.
Limits: 1 MB per `cat`/`put`, 512 KB per inbound frame.

The RFCOMM channel is allocated at runtime (SPP record named "rshell"); the daemon logs it to
`C:\Data\logs_rshelld.txt`. Omit the channel to let the client scan.

## Layout
```
apps/rshell     device agent (gui app + headless lib)
apps/rshelld    headless daemon build (reuses the rshell crate)
apps/rfprobe    RFCOMM server capability probe
client/         rshell.py (interactive) + rsh.py (batch)
```
Depends on `../SDK` by path. The RFCOMM shim (`shim/src/shim_btsock.cpp`, `shim_bt.cpp`) and the
Rust binding (`crates/symbian/src/bt.rs`) live in the SDK — they are toolkit, not this app.
