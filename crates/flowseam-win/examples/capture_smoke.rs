//! Smoke check of the capture path on a real Windows desktop: `hook::start` puts the hooks in, a recording
//! target is attached, and a few events are injected: F24 by scan code without our mark (input at this
//! machine) and with it (a peer's input arriving here), then a short move there and back. The target
//! swallows F24 and nothing else, so no window gets the key, the person at the machine keeps typing and
//! moving, and the cursor ends where it was. Last, the veil hides the cursor for a third of a second and
//! shows it again. Run it in the logged-in session (hooks see nothing over ssh);
//! it exits within seconds, a watchdog ends it otherwise, and the hooks go with the process.

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    imp::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("capture_smoke runs on Windows only");
}

#[cfg(windows)]
mod imp {
    use std::mem::size_of;
    use std::process::ExitCode;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use flowseam_core::backend::{CapturedEvent, Disposition};
    use flowseam_core::proto::Point;
    use flowseam_win::capture::{self, Target, INJECT_MARK};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
        MOUSEEVENTF_MOVE, MOUSEINPUT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorInfo, CURSORINFO, CURSOR_SHOWING};

    /// F24's set-1 scan code, and its HID usage.
    const F24_SCAN: u16 = 0x76;
    const F24_HID: u16 = 0x73;

    #[derive(Default)]
    struct Rec {
        events: Mutex<Vec<CapturedEvent>>,
        cursors: Mutex<Vec<Point>>,
    }

    impl Target for Rec {
        fn event(&self, ev: CapturedEvent) -> Disposition {
            let d = match ev {
                CapturedEvent::Key { hid: F24_HID, .. } => Disposition::Swallow,
                _ => Disposition::Pass,
            };
            self.events.lock().unwrap().push(ev);
            d
        }
        fn cursor(&self, at: Point) {
            self.cursors.lock().unwrap().push(at);
        }
        fn tick(&self) -> bool {
            false
        }
        fn needs_tick(&self) -> bool {
            false
        }
        fn grabbed(&self) -> bool {
            false
        }
    }

    fn key(scan: u16, up: bool, extra: usize) -> INPUT {
        let flags = KEYEVENTF_SCANCODE | if up { KEYEVENTF_KEYUP } else { 0 };
        INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: 0, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: extra } } }
    }

    fn nudge(dx: i32) -> INPUT {
        INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy: 0, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: 0 } } }
    }

    /// Whether the cursor shows now: `None` when Windows will not say.
    fn cursor_shown() -> Option<bool> {
        // SAFETY: one CURSORINFO with its size set, as GetCursorInfo requires.
        let mut ci: CURSORINFO = unsafe { std::mem::zeroed() };
        ci.cbSize = size_of::<CURSORINFO>() as u32;
        if unsafe { GetCursorInfo(&mut ci) } == 0 {
            return None;
        }
        Some(ci.flags & CURSOR_SHOWING != 0 && !ci.hCursor.is_null())
    }

    fn send(inputs: &[INPUT]) {
        // SAFETY: fully initialised INPUTs and their size.
        let n = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32) };
        if n as usize != inputs.len() {
            println!("SendInput inserted {n} of {}: {}", inputs.len(), std::io::Error::last_os_error());
        }
    }

    pub fn main() -> ExitCode {
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(10));
            println!("FAIL watchdog: still running after 10 s");
            std::process::exit(3);
        });
        if let Err(e) = flowseam_win::win_input::dpi_aware() {
            println!("dpi: {e}");
        }
        if let Err(e) = flowseam_win::hook::start() {
            println!("FAIL hooks: {e}");
            return ExitCode::from(1);
        }
        let rec = Arc::new(Rec::default());
        capture::attach(rec.clone());
        println!("hooks in, target attached");
        send(&[key(F24_SCAN, false, 0), key(F24_SCAN, true, 0)]);
        send(&[key(F24_SCAN, false, INJECT_MARK), key(F24_SCAN, true, INJECT_MARK)]);
        // Twenty counts: with "Enhance pointer precision" a one-count move comes out as no pixel at all (measured).
        send(&[nudge(20)]);
        std::thread::sleep(Duration::from_millis(200));
        send(&[nudge(-20)]);
        std::thread::sleep(Duration::from_millis(500));
        capture::detach();
        let events = rec.events.lock().unwrap().clone();
        let cursors = rec.cursors.lock().unwrap().clone();
        println!("events {events:?}");
        println!("cursor samples {cursors:?}");
        // Only F24 counts: a key the person at the machine pressed meanwhile is theirs, not a failure.
        let keys: Vec<_> = events.iter().filter(|e| matches!(e, CapturedEvent::Key { hid: F24_HID, .. })).cloned().collect();
        let moves: Vec<i32> = events.iter().filter_map(|e| match e { CapturedEvent::Motion { dx, .. } => Some(*dx), _ => None }).collect();
        let keys_ok = keys == [CapturedEvent::Key { hid: F24_HID, down: true }, CapturedEvent::Key { hid: F24_HID, down: false }];
        println!("keys: {} (unmarked F24 down and up, the marked pair not seen)", if keys_ok { "ok" } else { "WRONG" });
        if moves.len() != 2 || cursors.len() != 2 {
            // The person at the machine moved the mouse too: their moves cannot be told from ours.
            println!("moves: cannot tell ({} moves, {} samples; other mouse input during the check)", moves.len(), cursors.len());
            println!("UNKNOWN");
            return ExitCode::from(4);
        }
        let moves_ok = moves[0] > 0 && moves[1] < 0;
        println!("moves: {} (one right, one back, each with a cursor sample)", if moves_ok { "ok" } else { "WRONG" });
        // The veil: the cursor is gone while it covers it, and back after. A third of a second each.
        let mut grab = flowseam_win::hook::grab();
        let before = cursor_shown();
        let hid = grab.hide().map(|()| {
            std::thread::sleep(Duration::from_millis(300));
            cursor_shown()
        });
        let back = grab.show().map(|()| {
            std::thread::sleep(Duration::from_millis(300));
            cursor_shown()
        });
        println!("cursor shown before {before:?}, under the veil {hid:?}, after {back:?}");
        let veil_ok = before == Some(true) && hid == Ok(Some(false)) && back == Ok(Some(true));
        println!("veil: {}", if veil_ok { "ok" } else { "WRONG" });
        // The liveness probe: both hooks see theirs (and swallow them).
        let before = capture::probes_seen();
        let sent = flowseam_win::hook::send_probe();
        std::thread::sleep(Duration::from_millis(300));
        let after = capture::probes_seen();
        let gone = capture::hooks_gone(before, after);
        println!("probe sent {sent:?}, counts {before:?} -> {after:?}, gone {gone:?}");
        let probe_ok = sent.is_ok() && gone.is_none();
        println!("probe: {}", if probe_ok { "ok" } else { "WRONG" });
        let moves_ok = moves_ok && veil_ok && probe_ok;
        if keys_ok && moves_ok {
            println!("PASS");
            ExitCode::SUCCESS
        } else {
            println!("FAIL");
            ExitCode::from(2)
        }
    }
}
