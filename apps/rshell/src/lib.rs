//! The remote-shell agent: a headless daemon that turns the phone into a Bluetooth serial
//! server a paired laptop drives to browse the filesystem, read files and write them.
//!
//! # Shape
//!
//! The phone is the RFCOMM *server* (there is no dial-out). On the first timer tick it opens
//! the listener, registers an SPP SDP record and starts accepting; a periodic tick keeps it
//! resilient. Everything after that is event-driven through [`DaemonApp::handle_raw`]:
//! `SHIM_EV_BT_ACCEPTED` gives a client, `SHIM_EV_BT_RECV` delivers bytes, `SHIM_EV_BT_SENT`
//! drains the outbound queue. A dropped link surfaces as an error on a recv or send; the
//! daemon closes the socket and goes back to accepting — which is what "cannot fall" means
//! here: the process is invisible to the shell's shutdown broadcast (headless, no window
//! group), and the link re-establishes itself.
//!
//! # Wire protocol
//!
//! Length-prefixed frames both ways: `[u32 big-endian len][payload]`. A frame from the laptop
//! is a UTF-8 command line (`ls`, `stat`, `cd`, `pwd`, `cat`/`get`, `put`, `rm`, `mkdir`,
//! `find`, `grep`, `exec`, `reboot`, `ps`, `cenrep`, `hal`, `ping`, `quit`) — except while a
//! `put` is in flight, when frames carry the file bytes. A frame to the laptop is a one-byte
//! tag then data: `+` OK (text), `-` ERR (text), `.` DATA (raw bytes
//! of a listing or file, possibly several), so a `cat` is zero-or-more `.` frames then one `+`
//! with the byte count.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use symbian::bt::rfcomm;
use symbian::fs::{self, Fs, ShimFs, Utf16Path};
use symbian_app::DaemonApp;
use symbian_sys as sys;
#[cfg(feature = "gui")]
use symbian_gfx::Align;
#[cfg(feature = "gui")]
use symbian_ui::{chrome, App, Handled, Key, KeyEvent, Rect, Softkey, Theme};

/// How much one `RecvOneOrMore` can land. RFCOMM hands over whatever arrived; the frame
/// assembler stitches partial frames back together, so this is a throughput knob, not a limit.
const RX_CAP: usize = 1024;
/// Refuse an inbound frame claiming to be larger than this — a bad length must not turn into a
/// multi-megabyte allocation.
const MAX_FRAME: usize = 512 * 1024;
/// Biggest file `cat`/`put` will move in one go. A shell reads configs and small binaries; a
/// real bulk transfer is a different tool.
const MAX_FILE: usize = 1024 * 1024;
/// DATA payload size for streaming a file out.
const DATA_CHUNK: usize = 4096;

/// The file server (`efile.exe`), UID3 read out of its ROM image on the E72. It is
/// system-permanent, so killing it faults the kernel and the device resets — a genuine reboot.
///
/// The gentler candidates were tried first and measured on this handset: killing SysAp
/// (`essysapp.exe`, 0x20002532) just respawns it (no reboot), and killing the window server
/// (`EwSrv.exe`, 0x10003b20) triggers a clean power-OFF, not a restart. The file server is the
/// only reachable process whose death actually RESETS the phone — at the cost of being a hard
/// reset with nothing flushed, which is why `reboot` demands the literal `now`.
const EFILE_UID3: u32 = 0x1000_39e3;
/// The resilience heartbeat: often enough to re-arm a dropped accept promptly, rare enough to
/// cost nothing on the battery.
const SUPERVISION_MS: i32 = 5_000;
/// How long one `find` or `grep` may run inside a single `rust_step`.
///
/// This is a hard ceiling, not a nicety. Avkon owns the loop and a `rust_step` that does not
/// return promptly starves the window server, which freezes the *whole phone* rather than just
/// this app. Two seconds is long enough to sweep a directory of a few thousand entries and short
/// enough that nobody reaches for the battery; a scan that hits it reports `BUDGET EXPIRED`
/// instead of quietly returning partial results as if they were the whole answer.
const SCAN_BUDGET_US: u64 = 2_000_000;

/// A `put` in progress: where it lands, how many bytes are still to come, and what has arrived.
struct Put {
    path: String,
    remaining: usize,
    acc: Vec<u8>,
}

/// The Publish & Subscribe contract between the daemon (rshelld) and the GUI panel (rshell).
/// The category is the daemon's own SID (its UID3), so it defines the keys cap-free with an open
/// read policy (`define_public`) and the panel — a different UID — reads them. Int-only, so the
/// human-readable "recent activity" comes from tailing the daemon's log, not from here.
pub mod pubsub {
    /// rshelld's UID3 — the P&S category (the daemon's SID).
    pub const CAT: u32 = 0xE0AA_00F2;
    /// 0 down, 1 listening, 2 client connected.
    pub const KEY_STATE: u32 = 1;
    pub const KEY_CHANNEL: u32 = 2;
    pub const KEY_CMDS: u32 = 3;
    pub const KEY_CLIENTS: u32 = 4;
    pub const STATE_DOWN: i32 = 0;
    pub const STATE_LISTENING: i32 = 1;
    pub const STATE_CONNECTED: i32 = 2;
}

pub struct Shell {
    started: bool,
    listener_up: bool,
    channel: u8,
    client: Option<i32>,
    accepting: bool,
    rxbuf: [u8; RX_CAP],
    /// Inbound bytes not yet consumed as whole frames.
    inbuf: Vec<u8>,
    /// Complete outbound frames waiting to go, and the one in flight (held so its buffer stays
    /// alive until `SHIM_EV_BT_SENT`).
    out: VecDeque<Vec<u8>>,
    inflight: Option<Vec<u8>>,
    cwd: String,
    put: Option<Put>,
    quit_requested: bool,
    /// Set by `reboot`, acted on once the reply has actually left the wire — see [`Shell::on_sent`].
    reboot_requested: bool,
    exit: bool,
    /// The last few notable events, newest last, for the on-screen status.
    log_lines: Vec<String>,
    /// Counters published to the panel over P&S.
    cmds: i32,
    clients: i32,
}

impl Shell {
    pub fn new() -> Self {
        // Cheap constructor: the daemon shim signals its rendezvous right after this returns,
        // so the real bring-up waits for the first timer event.
        let _ = symbian::timer_after(1);
        Shell {
            started: false,
            listener_up: false,
            channel: 0,
            client: None,
            accepting: false,
            rxbuf: [0; RX_CAP],
            inbuf: Vec::new(),
            out: VecDeque::new(),
            inflight: None,
            cwd: String::from("Z:\\"),
            put: None,
            quit_requested: false,
            reboot_requested: false,
            exit: false,
            log_lines: Vec::new(),
            cmds: 0,
            clients: 0,
        }
    }

    /// Record a line for the status screen, keeping only the most recent handful.
    fn note(&mut self, s: &str) {
        self.log_lines.push(String::from(s));
        let n = self.log_lines.len();
        if n > 8 {
            self.log_lines.drain(..n - 8);
        }
    }

    fn start(&mut self) {
        // Publish state for the GUI panel (rshell) to read. define_public so a different-SID app
        // can read; cap-free because CAT is this daemon's own SID. Best effort throughout.
        let _ = symbian::prop::define_public(pubsub::CAT, pubsub::KEY_STATE);
        let _ = symbian::prop::define_public(pubsub::CAT, pubsub::KEY_CHANNEL);
        let _ = symbian::prop::define_public(pubsub::CAT, pubsub::KEY_CMDS);
        let _ = symbian::prop::define_public(pubsub::CAT, pubsub::KEY_CLIENTS);
        match rfcomm::listen("rshell", 1) {
            Ok(ch) => {
                self.listener_up = true;
                self.channel = ch;
                symbian::log!("rshell: listening, RFCOMM channel {}", ch as i32);
                self.note(&format!("listening on channel {}", ch));
                self.publish(pubsub::KEY_CHANNEL, ch as i32);
                self.publish(pubsub::KEY_STATE, pubsub::STATE_LISTENING);
                self.begin_accept();
            }
            Err(e) => {
                symbian::log!("rshell: listen failed {:?}", e);
                self.note(&format!("listen failed: {:?}", e));
                self.publish(pubsub::KEY_STATE, pubsub::STATE_DOWN);
            }
        }
        // Resilience heartbeat, from here on.
        let _ = symbian::timer_every(SUPERVISION_MS);
    }

    fn tick(&mut self) {
        if !self.listener_up {
            // Listener never came up (or was lost) — try again.
            if let Ok(ch) = rfcomm::listen("rshell", 1) {
                self.listener_up = true;
                self.channel = ch;
                symbian::log!("rshell: listener recovered, channel {}", ch as i32);
                self.publish(pubsub::KEY_CHANNEL, ch as i32);
                self.publish(pubsub::KEY_STATE, pubsub::STATE_LISTENING);
            }
        }
        if self.listener_up && self.client.is_none() {
            self.begin_accept();
        }
    }

    fn begin_accept(&mut self) {
        if self.accepting || self.client.is_some() || !self.listener_up {
            return;
        }
        match rfcomm::accept() {
            Ok(()) => self.accepting = true,
            Err(e) => symbian::log!("rshell: accept failed {:?}", e),
        }
    }

    fn on_accepted(&mut self, handle: i32, status: i32) {
        self.accepting = false;
        if status == 0 && handle >= 0 {
            self.client = Some(handle);
            self.inbuf.clear();
            self.out.clear();
            self.inflight = None;
            self.put = None;
            symbian::log!("rshell: client on handle {}", handle);
            self.note("client connected");
            self.clients += 1;
            self.publish(pubsub::KEY_CLIENTS, self.clients);
            self.publish(pubsub::KEY_STATE, pubsub::STATE_CONNECTED);
            self.send_ok("rshell ready");
            self.issue_recv();
        } else {
            symbian::log!("rshell: accept status {}", status);
            self.begin_accept();
        }
    }

    fn issue_recv(&mut self) {
        let Some(h) = self.client else { return };
        if let Err(e) = rfcomm::recv(h, &mut self.rxbuf) {
            symbian::log!("rshell: recv issue err {:?}", e);
            self.drop_client();
        }
    }

    fn on_recv(&mut self, status: i32, len: usize) {
        // A non-zero status or a zero-length read is how the stack reports the peer going away.
        if status != 0 || len == 0 {
            self.drop_client();
            return;
        }
        let n = len.min(RX_CAP);
        self.inbuf.extend_from_slice(&self.rxbuf[..n]);
        self.process_frames();
        // Keep reading unless a frame closed the connection.
        self.issue_recv();
    }

    fn process_frames(&mut self) {
        loop {
            if self.inbuf.len() < 4 {
                break;
            }
            let l = u32::from_be_bytes([self.inbuf[0], self.inbuf[1], self.inbuf[2], self.inbuf[3]])
                as usize;
            if l > MAX_FRAME {
                symbian::log!("rshell: frame too big ({}), dropping client", l as i32);
                self.drop_client();
                return;
            }
            if self.inbuf.len() < 4 + l {
                break;
            }
            let payload: Vec<u8> = self.inbuf[4..4 + l].to_vec();
            self.inbuf.drain(..4 + l);
            self.handle_payload(&payload);
            if self.client.is_none() {
                return;
            }
        }
    }

    fn handle_payload(&mut self, payload: &[u8]) {
        if let Some(put) = self.put.take() {
            self.recv_put_data(put, payload);
            return;
        }
        let line = core::str::from_utf8(payload).unwrap_or("");
        self.cmds += 1;
        self.publish(pubsub::KEY_CMDS, self.cmds);
        self.dispatch(line);
    }

    /// Publish one P&S value for the GUI panel. Best effort — a failure here must never disturb
    /// the shell.
    fn publish(&self, key: u32, val: i32) {
        let _ = symbian::prop::set(pubsub::CAT, key, val);
    }

    fn recv_put_data(&mut self, mut put: Put, data: &[u8]) {
        let take = data.len().min(put.remaining);
        put.acc.extend_from_slice(&data[..take]);
        put.remaining -= take;
        if put.remaining == 0 {
            self.write_file(&put.path, &put.acc);
        } else {
            self.put = Some(put);
        }
    }

    fn write_file(&mut self, path: &str, data: &[u8]) {
        let mut d = ShimFs;
        match Utf16Path::new(path) {
            Ok(p) => match fs::write_atomic(&mut d, &p, data) {
                Ok(()) => {
                    let n = data.len();
                    self.send_ok(&format!("wrote {} bytes to {}", n, path));
                }
                Err(e) => self.send_err(&format!("write failed {:?}", e)),
            },
            Err(_) => self.send_err("bad path"),
        }
    }

    fn dispatch(&mut self, line: &str) {
        let line = line.trim();
        if !line.is_empty() {
            self.note(line);
        }
        let (verb, arg) = match line.find(' ') {
            Some(i) => (&line[..i], line[i + 1..].trim()),
            None => (line, ""),
        };
        match verb {
            "" => {}
            "pwd" => {
                let c = self.cwd.clone();
                self.send_ok(&c);
            }
            "cd" => self.cmd_cd(arg),
            "ls" => self.cmd_ls(arg),
            "cat" | "get" => self.cmd_cat(arg),
            "put" => self.cmd_put(arg),
            "rm" => self.cmd_rm(arg),
            "mkdir" => self.cmd_mkdir(arg),
            "stat" => self.cmd_stat(arg),
            "find" => self.cmd_find(arg),
            "grep" => self.cmd_grep(arg),
            "exec" => self.cmd_exec(arg),
            "reboot" => self.cmd_reboot(arg),
            "ps" => self.cmd_ps(arg),
            "cenrep" => self.cmd_cenrep(arg),
            "hal" => self.cmd_hal(arg),
            "ping" => self.send_ok("pong"),
            "quit" => {
                self.send_ok("bye");
                self.quit_requested = true;
            }
            _ => self.send_err(
                "unknown verb (pwd cd ls stat cat get put rm mkdir find grep exec reboot ps cenrep hal ping quit)",
            ),
        }
    }

    fn cmd_cd(&mut self, arg: &str) {
        let a = arg.trim();
        self.cwd = if a == ".." {
            parent_of(&self.cwd)
        } else if a.is_empty() {
            self.cwd.clone()
        } else {
            resolve_dir(&self.cwd, a)
        };
        let c = self.cwd.clone();
        self.send_ok(&c);
    }

    fn cmd_ls(&mut self, arg: &str) {
        let dir = if arg.trim().is_empty() {
            self.cwd.clone()
        } else {
            resolve_dir(&self.cwd, arg)
        };
        let units: Vec<u16> = dir.encode_utf16().collect();
        // Big enough for a full Z:\sys\bin (which runs to a thousand-odd entries) without
        // truncating; a directory larger than this still lists what fits and reports the
        // count, so a short count is the signal to look closer.
        let mut buf = vec![0u16; 64 * 1024];
        let mut d = ShimFs;
        match d.list_entries(&units, &mut buf) {
            Ok(count) => {
                let listing = decode_listing(&buf, count);
                if !listing.is_empty() {
                    self.enqueue_data(listing.as_bytes());
                }
                self.send_ok(&format!("{} entries in {}", count, dir));
            }
            Err(e) => self.send_err(&format!("ls failed {:?}", e)),
        }
    }

    /// `cat <path>` for a whole file, or `cat <path> <off> <len>` for a range — which is how a
    /// binary far over [`MAX_FILE`] still gives up the twenty bytes that matter.
    fn cmd_cat(&mut self, arg: &str) {
        // Split off an optional trailing "<off> <len>". A path may contain spaces, so the range
        // is recognised only when the last two words are both numbers.
        let (path_part, range) = split_range(arg);

        let path = resolve_path(&self.cwd, path_part);
        let mut d = ShimFs;
        let p = match Utf16Path::new(&path) {
            Ok(p) => p,
            Err(_) => return self.send_err("bad path"),
        };

        if let Some((off, len)) = range {
            if len > MAX_FILE {
                return self.send_err(&format!("{} bytes is too large for one range", len));
            }
            let mut f = match symbian::fs::File::open(&mut d, &p, symbian::OpenMode::Read) {
                Ok(f) => f,
                Err(e) => return self.send_err(&format!("open failed {:?}", e)),
            };
            let total = f.size().unwrap_or(0);
            if let Err(e) = f.seek(off) {
                return self.send_err(&format!("seek failed {:?}", e));
            }
            let mut buf = vec![0u8; len];
            match f.read_fully(&mut buf) {
                Ok(n) => {
                    for chunk in buf[..n].chunks(DATA_CHUNK) {
                        self.enqueue_data(chunk);
                    }
                    self.send_ok(&format!("{} bytes at {} (file is {})", n, off, total));
                }
                Err(e) => self.send_err(&format!("read failed {:?}", e)),
            }
            return;
        }

        match fs::read(&mut d, &p) {
            Ok(Some(bytes)) => {
                if bytes.len() > MAX_FILE {
                    return self.send_err(&format!(
                        "file is {} bytes; too large for cat - use: cat <path> <off> <len>",
                        bytes.len()
                    ));
                }
                for chunk in bytes.chunks(DATA_CHUNK) {
                    self.enqueue_data(chunk);
                }
                self.send_ok(&format!("{} bytes", bytes.len()));
            }
            Ok(None) => self.send_err("not found"),
            Err(e) => self.send_err(&format!("read failed {:?}", e)),
        }
    }

    /// `stat <path>` — size, modification date and attributes, without pulling the file.
    fn cmd_stat(&mut self, arg: &str) {
        if arg.trim().is_empty() {
            return self.send_err("usage: stat <path>");
        }
        let path = resolve_path(&self.cwd, arg);
        let units: Vec<u16> = path.encode_utf16().collect();
        let mut d = ShimFs;
        match d.stat(&units) {
            Ok(s) => {
                let line = format!(
                    "{}  {} bytes  {:04}-{:02}-{:02} {:02}:{:02}:{:02}  att 0x{:x}{}",
                    path,
                    s.size,
                    s.year,
                    s.month,
                    s.day,
                    s.hour,
                    s.minute,
                    s.second,
                    s.attributes,
                    if s.is_dir { "  <dir>" } else { "" }
                );
                self.send_ok(&line);
            }
            Err(e) => self.send_err(&format!("stat failed {:?}", e)),
        }
    }

    /// `find <dir> <substring>` — walk `dir` recursively and report every path whose name
    /// contains `substring` (case-insensitive). Bounded by [`SCAN_BUDGET_US`]; a scan that runs
    /// out of time says so, with the directory it had reached, so it can be resumed narrower.
    fn cmd_find(&mut self, arg: &str) {
        let (dir_part, needle) = match arg.find(' ') {
            Some(i) => (arg[..i].trim(), arg[i + 1..].trim()),
            None => return self.send_err("usage: find <dir> <substring>"),
        };
        if needle.is_empty() {
            return self.send_err("usage: find <dir> <substring>");
        }
        let root = resolve_dir(&self.cwd, dir_part);
        let needle_lc = lower(needle);
        let deadline = symbian::monotonic_us() + SCAN_BUDGET_US;

        let mut hits = 0usize;
        let mut scanned = 0usize;
        let mut out = String::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.push_back(root.clone());
        let mut ran_out = false;

        while let Some(dir) = queue.pop_front() {
            if symbian::monotonic_us() > deadline {
                ran_out = true;
                break;
            }
            for name in self.entries(&dir) {
                scanned += 1;
                let is_dir = name.ends_with('\\');
                if lower(&name).contains(&needle_lc) {
                    hits += 1;
                    out.push_str(&dir);
                    out.push_str(&name);
                    out.push('\n');
                }
                if is_dir {
                    let mut sub = dir.clone();
                    sub.push_str(&name);
                    queue.push_back(sub);
                }
            }
        }
        if !out.is_empty() {
            self.enqueue_data(out.as_bytes());
        }
        let note = if ran_out {
            format!("{} hits, {} entries scanned - BUDGET EXPIRED, narrow the path", hits, scanned)
        } else {
            format!("{} hits, {} entries scanned", hits, scanned)
        };
        self.send_ok(&note);
    }

    /// `grep <dir> <pattern>` — search file *contents* under `dir`. A pattern of the form
    /// `hex:xxxx` matches raw bytes (e.g. `hex:66871f10` for a little-endian UID); anything
    /// else matches ASCII text. Reports the path and the offset of the first hit per file.
    ///
    /// This is the verb that replaces reaching for BusyBox: it runs inside a process that
    /// already holds `AllFiles`, so it reads the ROM directly with no POSIX layer.
    fn cmd_grep(&mut self, arg: &str) {
        let (dir_part, pat) = match arg.find(' ') {
            Some(i) => (arg[..i].trim(), arg[i + 1..].trim()),
            None => return self.send_err("usage: grep <dir> <text|hex:bytes>"),
        };
        let needle: Vec<u8> = if let Some(h) = pat.strip_prefix("hex:") {
            match parse_hex(h) {
                Some(v) if !v.is_empty() => v,
                _ => return self.send_err("bad hex pattern (use hex:66871f10)"),
            }
        } else if pat.is_empty() {
            return self.send_err("usage: grep <dir> <text|hex:bytes>");
        } else {
            pat.as_bytes().to_vec()
        };

        let root = resolve_dir(&self.cwd, dir_part);
        let deadline = symbian::monotonic_us() + SCAN_BUDGET_US;
        let mut d = ShimFs;
        let mut out = String::new();
        let mut hits = 0usize;
        let mut files = 0usize;
        let mut skipped = 0usize;
        let mut ran_out = false;

        // One directory only, not recursive: a content scan is expensive enough that the
        // caller should say where. `find` is the recursive one.
        for name in self.entries(&root) {
            if symbian::monotonic_us() > deadline {
                ran_out = true;
                break;
            }
            if name.ends_with('\\') {
                continue;
            }
            let mut full = root.clone();
            full.push_str(&name);
            let Ok(p) = Utf16Path::new(&full) else {
                skipped += 1;
                continue;
            };
            match fs::read(&mut d, &p) {
                Ok(Some(bytes)) => {
                    files += 1;
                    if let Some(at) = find_bytes(&bytes, &needle) {
                        hits += 1;
                        out.push_str(&format!("{}  @{}\n", full, at));
                    }
                }
                _ => skipped += 1,
            }
        }
        if !out.is_empty() {
            self.enqueue_data(out.as_bytes());
        }
        let note = if ran_out {
            format!("{} hits in {} files ({} unreadable) - BUDGET EXPIRED", hits, files, skipped)
        } else {
            format!("{} hits in {} files ({} unreadable)", hits, files, skipped)
        };
        self.send_ok(&note);
    }

    /// The entries of one directory as owned strings; directories keep their trailing `\`.
    fn entries(&self, dir: &str) -> Vec<String> {
        let units: Vec<u16> = dir.encode_utf16().collect();
        let mut buf = vec![0u16; 64 * 1024];
        let mut d = ShimFs;
        match d.list_entries(&units, &mut buf) {
            Ok(count) => split_listing(&buf, count),
            Err(_) => Vec::new(),
        }
    }

    /// `exec <path>` — launch an executable and return at once. `spawn`, never `start`: the
    /// blocking form waits on a rendezvous with `User::WaitForRequest`, which on a thread that
    /// already runs an active scheduler steals another request's completion and takes the
    /// process down with a kernel panic.
    fn cmd_exec(&mut self, arg: &str) {
        if arg.trim().is_empty() {
            return self.send_err("usage: exec <path>  (e.g. exec C:\\sys\\bin\\foo.exe)");
        }
        let path = resolve_path(&self.cwd, arg);
        match Utf16Path::new(&path) {
            Ok(p) => match symbian::process::spawn(&p) {
                Ok(()) => self.send_ok(&format!("spawned {}", path)),
                Err(e) => self.send_err(&format!("spawn failed {:?}", e)),
            },
            Err(_) => self.send_err("bad path"),
        }
    }

    /// `reboot now` — restart the handset by killing a process the kernel treats as
    /// system-critical.
    ///
    /// There is no published restart call in this SDK (`starterclient.dll`'s `RStarterSession` is
    /// the canonical one, but its import library was never shipped here and the device copy is a
    /// stripped stub that exports by ordinal only). What is left is the platform's own rule: when
    /// a system-permanent process dies, the kernel resets the device. Measured on this E72:
    /// killing SysAp just respawns it (no reboot); killing the window server powers the phone OFF
    /// (a clean shutdown, not a restart); killing the file server RESETS it. So `reboot` ends the
    /// file server — a hard reset with nothing flushed.
    ///
    /// That makes this a blunt instrument and it is deliberately not spelled `reboot`: nothing
    /// gets a chance to flush, so a daemon halfway through a write loses that write. The literal
    /// word `now` is required so a typo or a stale line in a script cannot take the phone down.
    ///
    /// The reply is queued, not sent, and the kill waits for it to reach the wire (`on_sent`),
    /// because a client that never hears back cannot tell "rebooting" from "the link broke".
    fn cmd_reboot(&mut self, arg: &str) {
        if arg.trim() != "now" {
            return self.send_err(
                "usage: reboot now  (hard reset via the file server; nothing is flushed first)",
            );
        }
        self.reboot_requested = true;
        self.send_ok("rebooting: killing the file server once this reply is on the wire");
    }

    /// `ps <cat> <key>` — read one Publish & Subscribe integer. Both arguments are hex or
    /// decimal, so a UID copied from a header pastes straight in.
    fn cmd_ps(&mut self, arg: &str) {
        let Some((cat, key)) = two_numbers(arg) else {
            return self.send_err("usage: ps <category> <key>  (hex ok: ps 0x101f75b6 0x10203642)");
        };
        match symbian::prop::get(cat, key) {
            Ok(v) => self.send_ok(&format!("0x{:08x}/0x{:08x} = {} (0x{:x})", cat, key, v, v)),
            Err(e) => self.send_err(&format!("ps failed {:?}", e)),
        }
    }

    /// `cenrep <repo> <key>` — read one Central Repository value, as an integer and, if that
    /// fails, as a string. Two attempts because a key's type is not knowable in advance, and
    /// "wrong type" is the most common reason a probe reports nothing.
    fn cmd_cenrep(&mut self, arg: &str) {
        let Some((repo, key)) = two_numbers(arg) else {
            return self
                .send_err("usage: cenrep <repo> <key>  (hex ok: cenrep 0x101f8766 0x1)");
        };
        match symbian::cenrep::get(repo, key) {
            Ok(v) => self.send_ok(&format!("0x{:08x}/0x{:x} = {} (0x{:x})", repo, key, v, v)),
            Err(ie) => match symbian::cenrep::get_string(repo, key) {
                Ok(s) => self.send_ok(&format!("0x{:08x}/0x{:x} = \"{}\"", repo, key, s)),
                Err(se) => self
                    .send_err(&format!("cenrep int {:?}, string {:?}", ie, se)),
            },
        }
    }

    /// `hal [attr]` — one HAL attribute by number, or with no argument the machine UID, which
    /// is what a RomPatcher `.rmp` guard (`#ifdef MACHINE_xxxxxxxx`) needs.
    fn cmd_hal(&mut self, arg: &str) {
        let a = arg.trim();
        if a.is_empty() {
            // EMachineUid is attribute 5.
            return match symbian::hal::get(5) {
                Ok(v) => self
                    .send_ok(&format!("machine uid = 0x{:08x} ({})", v as u32, v)),
                Err(e) => self.send_err(&format!("hal failed {:?}", e)),
            };
        }
        let Some(n) = parse_num(a) else {
            return self.send_err("usage: hal [attribute-number]   (no argument = machine uid)");
        };
        match symbian::hal::get(n as i32) {
            Ok(v) => self.send_ok(&format!("hal[{}] = {} (0x{:x})", n, v, v)),
            Err(e) => self.send_err(&format!("hal[{}] failed {:?}", n, e)),
        }
    }

    fn cmd_put(&mut self, arg: &str) {
        let (path_part, len_part) = match arg.rfind(' ') {
            Some(i) => (arg[..i].trim(), arg[i + 1..].trim()),
            None => return self.send_err("usage: put <path> <len>"),
        };
        let len: usize = match len_part.parse() {
            Ok(n) => n,
            Err(_) => return self.send_err("usage: put <path> <len>"),
        };
        if len > MAX_FILE {
            return self.send_err(&format!("{} bytes is too large for put", len));
        }
        let path = resolve_path(&self.cwd, path_part);
        if len == 0 {
            self.write_file(&path, &[]);
        } else {
            self.send_ok(&format!("send {} bytes", len));
            self.put = Some(Put { path, remaining: len, acc: Vec::with_capacity(len) });
        }
    }

    fn cmd_rm(&mut self, arg: &str) {
        if arg.trim().is_empty() {
            return self.send_err("usage: rm <path>");
        }
        let path = resolve_path(&self.cwd, arg);
        let units: Vec<u16> = path.encode_utf16().collect();
        let mut d = ShimFs;
        match d.delete(&units) {
            Ok(()) => self.send_ok(&format!("removed {}", path)),
            Err(e) => self.send_err(&format!("rm failed {:?}", e)),
        }
    }

    fn cmd_mkdir(&mut self, arg: &str) {
        if arg.trim().is_empty() {
            return self.send_err("usage: mkdir <path>");
        }
        // A directory path needs its trailing separator, or the platform creates nothing and
        // still reports success (see the MkDirEnsure note in the shim).
        let dir = resolve_dir(&self.cwd, arg);
        let units: Vec<u16> = dir.encode_utf16().collect();
        let mut d = ShimFs;
        match d.mkdir(&units) {
            Ok(()) => self.send_ok(&format!("created {}", dir)),
            Err(e) => self.send_err(&format!("mkdir failed {:?}", e)),
        }
    }

    fn send_ok(&mut self, msg: &str) {
        self.enqueue_tagged(b'+', msg.as_bytes());
    }
    fn send_err(&mut self, msg: &str) {
        self.enqueue_tagged(b'-', msg.as_bytes());
    }
    fn enqueue_data(&mut self, data: &[u8]) {
        self.enqueue_tagged(b'.', data);
    }

    fn enqueue_tagged(&mut self, tag: u8, data: &[u8]) {
        let mut p = Vec::with_capacity(1 + data.len());
        p.push(tag);
        p.extend_from_slice(data);
        self.out.push_back(frame(&p));
        self.kick_tx();
    }

    fn kick_tx(&mut self) {
        if self.inflight.is_some() {
            return;
        }
        let Some(h) = self.client else { return };
        let Some(f) = self.out.pop_front() else { return };
        self.inflight = Some(f);
        let res = {
            let buf = self.inflight.as_ref().unwrap();
            rfcomm::send(h, buf)
        };
        if let Err(e) = res {
            symbian::log!("rshell: send err {:?}", e);
            self.drop_client();
        }
    }

    fn on_sent(&mut self, status: i32) {
        self.inflight = None;
        if status != 0 {
            self.drop_client();
            return;
        }
        if self.out.is_empty() {
            if self.reboot_requested {
                symbian::log!("rshell: reboot requested, killing the file server");
                let _ = symbian::process::kill(EFILE_UID3);
                // If the kill returned without resetting the device, say so rather than leaving
                // the client waiting.
                self.reboot_requested = false;
                self.send_err("file-server kill returned but the phone is still up");
                self.kick_tx();
                return;
            }
            if self.quit_requested {
                self.exit = true;
            }
            return;
        }
        self.kick_tx();
    }

    /// Route one platform event. Everything the server does is driven from here.
    ///
    /// This is the whole daemon: the two entry shapes differ only in what they do with the
    /// return value. The GUI build turns `true` into `Handled::Consumed`, which both stops the
    /// event becoming a keystroke and marks the frame dirty so the status screen repaints; the
    /// headless build discards it, because there is nothing to repaint.
    fn route(&mut self, ev: &sys::ShimEvent) -> bool {
        match ev.kind {
            sys::SHIM_EV_TIMER => {
                if !self.started {
                    self.started = true;
                    self.start();
                } else {
                    self.tick();
                }
                true
            }
            sys::SHIM_EV_BT_ACCEPTED => {
                self.on_accepted(ev.handle, ev.status);
                true
            }
            sys::SHIM_EV_BT_RECV => {
                self.on_recv(ev.status, ev.a.max(0) as usize);
                true
            }
            sys::SHIM_EV_BT_SENT => {
                self.on_sent(ev.status);
                true
            }
            sys::SHIM_EV_BT_CLOSED => {
                self.drop_client();
                true
            }
            _ => false,
        }
    }

    fn drop_client(&mut self) {
        if let Some(h) = self.client.take() {
            let _ = rfcomm::close(h);
            symbian::log!("rshell: client {} dropped", h);
            self.note("client disconnected");
            self.publish(pubsub::KEY_STATE, pubsub::STATE_LISTENING);
        }
        self.inbuf.clear();
        self.out.clear();
        self.inflight = None;
        self.put = None;
        self.begin_accept();
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

/// The headless shape: no keys, no theme, nothing to draw. This is the form that survives a
/// reboot, because a start-up item launches an executable and an Avkon app launched at boot
/// would take the screen — the one thing an agent watching a boot must not do.
impl DaemonApp for Shell {
    fn handle_raw(&mut self, ev: &sys::ShimEvent) {
        self.route(ev);
    }

    fn should_exit(&self) -> bool {
        self.exit
    }
}

#[cfg(feature = "gui")]
impl App for Shell {
    fn handle_raw(&mut self, ev: &symbian_ui::RawEvent) -> Handled {
        if self.route(ev) {
            Handled::Consumed
        } else {
            Handled::Ignored
        }
    }

    fn handle_key(&mut self, ev: KeyEvent, _theme: &Theme<'_>, _screen: Rect) -> Handled {
        match ev.key {
            Key::Softkey(Softkey::Right) | Key::End => {
                self.exit = true;
                Handled::Consumed
            }
            _ => Handled::Ignored,
        }
    }

    fn draw(&mut self, c: &mut symbian_ui::Canvas<'_>, theme: &Theme<'_>) {
        let screen = Rect::from_size(c.size());
        let frame = chrome::Frame::split(screen, theme, true, true);
        chrome::clear(c, theme);

        let title = if self.listener_up {
            format!("RFCOMM shell  ch {}", self.channel)
        } else {
            String::from("RFCOMM shell")
        };
        chrome::title_bar(c, frame.title, theme, &title, None);
        chrome::softkey_bar(c, frame.softkeys, theme, [None, None, Some("Exit")]);

        let line_h = theme.fonts.small.line_height().max(1);
        let mut y = frame.content.y0;

        let status = if self.client.is_some() {
            "client: connected"
        } else if self.listener_up {
            "waiting for a client"
        } else {
            "starting..."
        };
        let sr = Rect { y0: y, y1: y + line_h, ..frame.content };
        c.draw_text_in(sr, status, theme.fonts.small, theme.palette.accent, Align::Start);
        y += line_h;

        for line in &self.log_lines {
            if y + line_h > frame.content.y1 {
                break;
            }
            let r = Rect { y0: y, y1: y + line_h, ..frame.content };
            c.draw_text_in(r, line, theme.fonts.small, theme.palette.text, Align::Start);
            y += line_h;
        }
    }

    fn should_exit(&self) -> bool {
        self.exit
    }

    fn title(&self) -> &str {
        "RFCOMM shell"
    }
}

/// The GUI panel: a read-only dashboard over the daemon (rshelld). It does NOT open an RFCOMM
/// listener, so it can be opened while the daemon is running — the whole reason the two used to
/// conflict. It reads the daemon's Publish & Subscribe state and tails its log on a timer.
#[cfg(feature = "gui")]
pub struct Viewer {
    alive: bool,
    state: i32,
    channel: i32,
    cmds: i32,
    clients: i32,
    log: Vec<String>,
    exit: bool,
}

#[cfg(feature = "gui")]
impl Viewer {
    pub fn new() -> Self {
        // Refresh a couple of times a second; cheap P&S reads plus a small log tail.
        let _ = symbian::timer_every(700);
        let mut v = Viewer {
            alive: false,
            state: -1,
            channel: -1,
            cmds: 0,
            clients: 0,
            log: Vec::new(),
            exit: false,
        };
        v.refresh();
        v
    }

    fn refresh(&mut self) {
        // is_running by UID is the liveness check: P&S values outlive the publisher within a boot,
        // so a stale STATE could otherwise read "listening" after the daemon died.
        self.alive = symbian::process::is_running(pubsub::CAT);
        self.state = symbian::prop::get(pubsub::CAT, pubsub::KEY_STATE).unwrap_or(-1);
        self.channel = symbian::prop::get(pubsub::CAT, pubsub::KEY_CHANNEL).unwrap_or(-1);
        self.cmds = symbian::prop::get(pubsub::CAT, pubsub::KEY_CMDS).unwrap_or(0);
        self.clients = symbian::prop::get(pubsub::CAT, pubsub::KEY_CLIENTS).unwrap_or(0);
        self.log = tail_log(8);
    }
}

#[cfg(feature = "gui")]
impl Default for Viewer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "gui")]
impl App for Viewer {
    fn handle_raw(&mut self, ev: &symbian_ui::RawEvent) -> Handled {
        if ev.kind == sys::SHIM_EV_TIMER {
            self.refresh();
            Handled::Consumed
        } else {
            Handled::Ignored
        }
    }

    fn handle_key(&mut self, ev: KeyEvent, _theme: &Theme<'_>, _screen: Rect) -> Handled {
        match ev.key {
            Key::Softkey(Softkey::Right) | Key::End => {
                self.exit = true;
                Handled::Consumed
            }
            // When the daemon is down, the left softkey starts it. spawn (not start) so the GUI
            // thread never blocks on WaitForRequest.
            Key::Softkey(Softkey::Left) if !self.alive => {
                if let Ok(path) = Utf16Path::new("rshelld.exe") {
                    let _ = symbian::process::spawn(&path);
                }
                Handled::Consumed
            }
            _ => Handled::Ignored,
        }
    }

    fn draw(&mut self, c: &mut symbian_ui::Canvas<'_>, theme: &Theme<'_>) {
        let screen = Rect::from_size(c.size());
        let frame = chrome::Frame::split(screen, theme, true, true);
        chrome::clear(c, theme);
        chrome::title_bar(c, frame.title, theme, "ADBian", None);
        let left = if self.alive { None } else { Some("Start") };
        chrome::softkey_bar(c, frame.softkeys, theme, [left, None, Some("Exit")]);

        let line_h = theme.fonts.small.line_height().max(1);
        let mut y = frame.content.y0;
        let mut put = |c: &mut symbian_ui::Canvas<'_>, y: &mut i32, s: &str, col| {
            if *y + line_h > frame.content.y1 {
                return;
            }
            let r = Rect { y0: *y, y1: *y + line_h, ..frame.content };
            c.draw_text_in(r, s, theme.fonts.small, col, Align::Start);
            *y += line_h;
        };

        let head = if !self.alive {
            String::from("Daemon: stopped")
        } else if self.state == pubsub::STATE_CONNECTED {
            format!("Daemon: client connected (ch {})", self.channel)
        } else if self.state == pubsub::STATE_LISTENING {
            format!("Daemon: listening (ch {})", self.channel)
        } else {
            String::from("Daemon: starting...")
        };
        let head_col = if self.alive { theme.palette.accent } else { theme.palette.dim };
        put(c, &mut y, &head, head_col);
        put(c, &mut y, &format!("commands: {}   clients: {}", self.cmds, self.clients), theme.palette.dim);
        y += line_h / 2;
        for line in &self.log {
            put(c, &mut y, line, theme.palette.text);
        }
    }

    fn should_exit(&self) -> bool {
        self.exit
    }

    fn title(&self) -> &str {
        "ADBian"
    }
}

/// Read the last `n` lines of the daemon's log (C:\Data\logs_rshelld.txt). Best effort — an
/// absent or unreadable file just yields no lines.
#[cfg(feature = "gui")]
fn tail_log(n: usize) -> Vec<String> {
    let mut fs = ShimFs;
    let Ok(path) = Utf16Path::new("C:\\Data\\logs_rshelld.txt") else {
        return Vec::new();
    };
    let bytes = match fs::read(&mut fs, &path) {
        Ok(Some(b)) => b,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|l| l.trim_end_matches('\r').trim())
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    let len = lines.len();
    if len > n {
        lines.drain(..len - n);
    }
    lines
}

/// `[u32 big-endian len][payload]`.
fn frame(payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + payload.len());
    v.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// Resolve a path argument against the working directory. An argument with a drive letter
/// (`Z:\...`, `C:\...`) is absolute; anything else hangs off `cwd`.
fn resolve_path(cwd: &str, arg: &str) -> String {
    let a = arg.trim();
    if a.is_empty() {
        return String::from(cwd);
    }
    if a.as_bytes().get(1) == Some(&b':') {
        return String::from(a);
    }
    let mut p = String::from(cwd);
    if !p.ends_with('\\') {
        p.push('\\');
    }
    p.push_str(a);
    p
}

/// Like [`resolve_path`], but guarantees a trailing separator — for a directory.
fn resolve_dir(cwd: &str, arg: &str) -> String {
    let mut p = resolve_path(cwd, arg);
    if !p.ends_with('\\') {
        p.push('\\');
    }
    p
}

/// The parent of a `cwd` that ends in `\`. At a drive root it stays put.
fn parent_of(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches('\\');
    match trimmed.rfind('\\') {
        Some(i) => {
            // Keep the separator: "Z:\a\b" -> "Z:\a\".
            String::from(&trimmed[..i + 1])
        }
        None => String::from(cwd),
    }
}

/// ASCII-lowercase a string, for case-insensitive matching. `to_lowercase` would drag in the
/// Unicode tables, and a filesystem that is itself case-insensitive-ASCII needs nothing more.
fn lower(s: &str) -> String {
    s.chars().map(|c| c.to_ascii_lowercase()).collect()
}

/// Parse a hex or decimal number: `0x101f8766`, `101f8766` is *not* assumed hex, `4132` is
/// decimal. Only an explicit `0x` prefix means hex, so a decimal argument cannot be misread.
fn parse_num(s: &str) -> Option<u32> {
    let t = s.trim();
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u32::from_str_radix(h, 16).ok()
    } else {
        t.parse::<u32>().ok()
    }
}

/// Split `cat`'s argument into the path and an optional trailing `<off> <len>`.
///
/// A path may contain spaces, so a range is recognised only when the *last two* words are both
/// numbers — and the two words are then removed by their **length**, not by searching the string
/// for them. Searching was the original bug: `… \cal.sis 65536 65536` found the last `65536`
/// first, so the path kept a number glued to it and a perfectly present file answered NotFound.
/// Every chunked read whose offset happened to equal its length hit it.
fn split_range(arg: &str) -> (&str, Option<(u64, usize)>) {
    let trimmed = arg.trim();
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.len() >= 3 {
        if let (Some(off), Some(len)) =
            (parse_num(words[words.len() - 2]), parse_num(words[words.len() - 1]))
        {
            let cut = trimmed.len() - words[words.len() - 1].len();
            let cut = trimmed[..cut].trim_end().len();
            let cut = cut - words[words.len() - 2].len();
            return (trimmed[..cut].trim_end(), Some((off as u64, len as usize)));
        }
    }
    (trimmed, None)
}

/// Two numbers separated by whitespace, for the `ps`/`cenrep` verbs.
fn two_numbers(arg: &str) -> Option<(u32, u32)> {
    let mut it = arg.split_whitespace();
    let a = parse_num(it.next()?)?;
    let b = parse_num(it.next()?)?;
    Some((a, b))
}

/// Parse a run of hex digit pairs into bytes: `"66871f10"` -> `[0x66, 0x87, 0x1f, 0x10]`.
/// Whitespace is allowed between pairs so a pattern can be pasted with spaces.
fn parse_hex(s: &str) -> Option<Vec<u8>> {
    let digits: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).collect();
    if digits.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks(2) {
        let hi = pair[0].to_digit(16)?;
        let lo = pair[1].to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// The offset of the first occurrence of `needle` in `hay`, if any.
fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The shim's NUL-separated UTF-16 listing, split into owned strings.
fn split_listing(buf: &[u16], count: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(count);
    let mut i = 0usize;
    while out.len() < count && i < buf.len() {
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        out.push(String::from_utf16_lossy(&buf[start..i]));
        i += 1;
    }
    out
}

/// Turn the shim's NUL-separated UTF-16 listing into text, one entry per line. Directory names
/// arrive with a trailing `\`, which is kept so the listing shows what can be `cd`'d into.
fn decode_listing(buf: &[u16], count: usize) -> String {
    let mut s = String::new();
    let mut i = 0usize;
    let mut got = 0usize;
    while got < count && i < buf.len() {
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        s.push_str(&String::from_utf16_lossy(&buf[start..i]));
        s.push('\n');
        got += 1;
        i += 1; // skip the NUL
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_prefixes_big_endian_length() {
        assert_eq!(frame(b"hi"), alloc::vec![0, 0, 0, 2, b'h', b'i']);
        assert_eq!(&frame(&[0u8; 300])[..4], &[0, 0, 1, 44]);
    }

    #[test]
    fn resolve_absolute_vs_relative() {
        assert_eq!(resolve_path("Z:\\", "C:\\Data\\x.txt"), "C:\\Data\\x.txt");
        assert_eq!(resolve_path("Z:\\", "system"), "Z:\\system");
        assert_eq!(resolve_path("Z:\\sys\\", "bin"), "Z:\\sys\\bin");
    }

    #[test]
    fn dir_always_ends_in_sep() {
        assert_eq!(resolve_dir("Z:\\", "system"), "Z:\\system\\");
        assert_eq!(resolve_dir("Z:\\sys\\", "bin"), "Z:\\sys\\bin\\");
    }

    #[test]
    fn parent_walks_up_and_stops_at_root() {
        assert_eq!(parent_of("Z:\\sys\\bin\\"), "Z:\\sys\\");
        assert_eq!(parent_of("Z:\\sys\\"), "Z:\\");
        assert_eq!(parent_of("Z:\\"), "Z:\\");
    }

    #[test]
    fn a_range_whose_offset_reads_like_its_length_still_keeps_the_path() {
        // The chunked pull that found this: 64 KB at offset 64 KB.
        assert_eq!(
            split_range("C:\\Data\\_app_install\\cal.sis 65536 65536"),
            ("C:\\Data\\_app_install\\cal.sis", Some((65536, 65536)))
        );
        assert_eq!(
            split_range("C:\\Data\\x.txt 0 4096"),
            ("C:\\Data\\x.txt", Some((0, 4096)))
        );
        // No range: a bare path, even one that ends in digits.
        assert_eq!(split_range("C:\\Data\\log2.txt"), ("C:\\Data\\log2.txt", None));
        // A path with spaces keeps them, and only trailing numbers count as a range.
        assert_eq!(
            split_range("C:\\Data\\my file 2.txt"),
            ("C:\\Data\\my file 2.txt", None)
        );
    }

    #[test]
    fn numbers_take_hex_only_with_the_prefix() {
        assert_eq!(parse_num("0x101f8766"), Some(0x101f8766));
        assert_eq!(parse_num("0X10"), Some(16));
        // No prefix means decimal, so a plain count cannot be silently read as hex.
        assert_eq!(parse_num("4132"), Some(4132));
        assert_eq!(parse_num("zz"), None);
        assert_eq!(two_numbers("0x101f75b6 0x10203642"), Some((0x101f75b6, 0x10203642)));
        assert_eq!(two_numbers("5"), None);
    }

    #[test]
    fn hex_patterns_become_bytes() {
        assert_eq!(parse_hex("66871f10"), Some(alloc::vec![0x66, 0x87, 0x1f, 0x10]));
        assert_eq!(parse_hex("66 87 1f 10"), Some(alloc::vec![0x66, 0x87, 0x1f, 0x10]));
        assert_eq!(parse_hex("abc"), None, "odd digit count is a mistake, not a guess");
        assert_eq!(parse_hex("zz"), None);
    }

    #[test]
    fn byte_search_finds_the_first_offset() {
        // The little-endian spelling of 0x101F8766, the UID this shell went hunting for.
        let hay = alloc::vec![0, 1, 2, 0x66, 0x87, 0x1f, 0x10, 9];
        assert_eq!(find_bytes(&hay, &[0x66, 0x87, 0x1f, 0x10]), Some(3));
        assert_eq!(find_bytes(&hay, &[0xde, 0xad]), None);
        assert_eq!(find_bytes(&hay, &[]), None);
    }

    #[test]
    fn listing_splits_into_entries() {
        let mut buf: Vec<u16> = Vec::new();
        buf.extend("a.txt".encode_utf16());
        buf.push(0);
        buf.extend("dir\\".encode_utf16());
        buf.push(0);
        buf.resize(64, 0);
        let got = split_listing(&buf, 2);
        assert_eq!(got, alloc::vec![String::from("a.txt"), String::from("dir\\")]);
    }

    #[test]
    fn listing_decodes_names_and_keeps_dir_slash() {
        // "a.txt\0dir\\\0"
        let mut buf: Vec<u16> = Vec::new();
        buf.extend("a.txt".encode_utf16());
        buf.push(0);
        buf.extend("dir\\".encode_utf16());
        buf.push(0);
        buf.resize(64, 0);
        assert_eq!(decode_listing(&buf, 2), "a.txt\ndir\\\n");
    }
}
