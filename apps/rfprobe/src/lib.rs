//! Can this handset be a Bluetooth RFCOMM *server*?
//!
//! One tap answers the question the remote-shell agent is blocked on. On launch this runs
//! [`symbian::bt::rfcomm_probe`] once — open an RFCOMM socket, claim a server channel, bind,
//! register and delete a throwaway SPP SDP record, listen — and shows the result of each step
//! on screen, in reading order, so a failure early explains every `--` after it. The same
//! findings are written to `C:\Data\dump-71-btsock.txt` so they can be pulled off the device.
//!
//! # Why this is a GUI app and not a headless daemon
//!
//! The probe is fast and entirely synchronous — no ten-second inquiry, no `WaitForRequest`
//! that could steal another request's completion — so running it on the UI thread is safe,
//! unlike the Bluetooth *inquiry* probe next door. And a headless daemon has no icon: nothing
//! to tap. The whole point here is that the operator taps once and reads the answer, with no
//! launcher to orchestrate it and no pull required.
//!
//! # Nothing is left behind
//!
//! The SDP record is registered only to prove the database accepts it, then deleted; every
//! socket and session is closed inside the shim before the call returns. The one artefact is
//! the report file in `C:\Data`, which needs no capability and any file manager can reach.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use symbian::bt::{self, RfcommProbe};
use symbian::fs::{self, ShimFs, Utf16Path};
use symbian_gfx::Align;
use symbian_ui::{chrome, App, Handled, Key, KeyEvent, Rect, Softkey, Theme};

/// Where the findings land. `C:\Data` needs no capability, so `epoc db pull` and any on-device
/// file manager can reach it; the name matches the `btsock` probe's section so a reader looking
/// for one file finds it whichever way the check was run.
const OUT_PATH: &str = "C:\\Data\\dump-71-btsock.txt";

/// One step of the bring-up and the Symbian code it returned.
struct Step {
    label: &'static str,
    code: i32,
}

impl Step {
    fn ok(&self) -> bool {
        self.code == bt::RFCOMM_STEP_OK
    }
    fn skipped(&self) -> bool {
        self.code == bt::RFCOMM_STEP_SKIPPED
    }
}

pub struct RfProbe {
    steps: Vec<Step>,
    summary: String,
    exit: bool,
}

impl RfProbe {
    pub fn new() -> Self {
        let mut me = RfProbe { steps: Vec::new(), summary: String::new(), exit: false };
        me.run();
        me
    }

    /// Run the probe once and turn its result into the on-screen steps, the summary line, and
    /// the report file. Called from the constructor: the work is a few synchronous IPC calls,
    /// so it costs a beat of the first paint and nothing more.
    fn run(&mut self) {
        match bt::rfcomm_probe() {
            Ok(p) => {
                self.push_steps(&p);
                let all_ok = self.steps.iter().all(Step::ok);
                self.summary = if all_ok {
                    format!("ALL OK - RFCOMM server works (channel {})", p.channel)
                } else {
                    String::from("blocked - a step failed (see above)")
                };
            }
            Err(e) => {
                // The whole sequence could not run — most likely a build with no USE_BTSOCK.
                self.summary = format!("probe could not run: {:?}", e);
            }
        }
        self.write_report();
    }

    fn push_steps(&mut self, p: &RfcommProbe) {
        self.steps.push(Step { label: "socket server (Connect)", code: p.serv_err });
        self.steps.push(Step { label: "open RFCOMM socket", code: p.open_err });
        self.steps.push(Step { label: "claim server channel", code: p.channel_err });
        self.steps.push(Step { label: "bind to the channel", code: p.bind_err });
        self.steps.push(Step { label: "open SDP database", code: p.sdp_open_err });
        self.steps.push(Step { label: "register SPP record", code: p.sdp_reg_err });
        self.steps.push(Step { label: "listen (LocalServices)", code: p.listen_err });
    }

    /// Same shape as a `devdump` section, so a reader knows it: a BEGIN, one line per step,
    /// the summary, an END. The END is the "ran to completion" sentinel — its absence would
    /// mean the app died mid-write.
    fn write_report(&self) {
        let mut out = String::from("BEGIN btsock\n");
        for s in &self.steps {
            let state = if s.ok() {
                String::from("OK")
            } else if s.skipped() {
                String::from("skipped")
            } else {
                format!("err {}", s.code)
            };
            out.push_str(&format!("{}: {}\n", s.label, state));
        }
        out.push_str(&self.summary);
        out.push_str("\nEND btsock\n");

        let mut d = ShimFs;
        if let Ok(path) = Utf16Path::new(OUT_PATH) {
            let _ = fs::write_atomic(&mut d, &path, out.as_bytes());
        }
    }
}

impl Default for RfProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl App for RfProbe {
    fn handle_key(&mut self, ev: KeyEvent, _theme: &Theme<'_>, _screen: Rect) -> Handled {
        // Any way out the operator reaches for: the right softkey, or the red key (advisory,
        // but honoured here so the app closes cleanly rather than being killed).
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
        chrome::title_bar(c, frame.title, theme, "RFCOMM probe", None);
        chrome::softkey_bar(c, frame.softkeys, theme, [None, None, Some("Exit")]);

        let line_h = theme.fonts.small.line_height().max(1);
        let status_h = line_h + 2;
        let list = Rect { y1: frame.content.y1 - status_h, ..frame.content };

        let mut y = list.y0;
        for s in &self.steps {
            let (mark, colour) = if s.ok() {
                ("ok ", theme.palette.accent)
            } else if s.skipped() {
                ("-- ", theme.palette.dim)
            } else {
                ("ERR", theme.palette.unread)
            };
            let text = if s.ok() || s.skipped() {
                format!("[{}] {}", mark, s.label)
            } else {
                format!("[{}] {} ({})", mark, s.label, s.code)
            };
            let r = Rect { y0: y, y1: y + line_h, ..list };
            c.draw_text_in(r, &text, theme.fonts.small, colour, Align::Start);
            y += line_h;
        }

        let status = Rect { y0: frame.content.y1 - status_h, ..frame.content };
        c.draw_text_in(status, &self.summary, theme.fonts.small, theme.palette.text, Align::Start);
    }

    fn should_exit(&self) -> bool {
        self.exit
    }

    fn title(&self) -> &str {
        "RFCOMM probe"
    }
}
