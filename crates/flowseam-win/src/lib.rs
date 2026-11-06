//! flowseam_win: text clipboard sync for Windows. The pure logic (`Pump`) talks to a
//! `Clipboard` trait; Win32 lives in `win32` (cfg(windows)) and a stdin/stdout
//! stand-in in `stdio`, so the logic tests run on any host.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use flowseam_core::proto::Msg;
use flowseam_net::clipboard::ClipboardSync;

pub mod app;
pub mod inject;
pub mod keymap;
pub mod stdio;
#[cfg(windows)]
pub mod win32;
#[cfg(windows)]
pub mod win_input;

/// Platform clipboard. Errors are text: they are logged, never swallowed.
pub trait Clipboard {
    /// Changes whenever the clipboard content changes (GetClipboardSequenceNumber).
    fn seq(&mut self) -> Result<u32, String>;
    /// `Ok(None)` means the clipboard holds no text, which is not a failure.
    fn read_text(&mut self) -> Result<Option<String>, String>;
    fn write_text(&mut self, text: &str) -> Result<(), String>;
}

pub struct Pump {
    clip: Box<dyn Clipboard>,
    sync: ClipboardSync,
    last_seq: Option<u32>,
}

impl Pump {
    pub fn new(clip: Box<dyn Clipboard>) -> Self {
        Pump { clip, sync: ClipboardSync::new(), last_seq: None }
    }

    /// Adopt the current clipboard as the baseline without sending it, so a
    /// reconnect does not overwrite the peer's clipboard.
    pub fn prime(&mut self) -> Result<(), String> {
        let seq = self.clip.seq()?;
        if let Some(t) = self.clip.read_text()? {
            // Oversize baseline: nothing to adopt; later changes are checked on their own.
            let _ = self.sync.on_local_change(&t);
        }
        self.last_seq = Some(seq);
        Ok(())
    }

    /// Message to send if the local clipboard changed since the last poll.
    /// A failed read leaves the sequence unconsumed so the next poll retries.
    pub fn poll(&mut self) -> Result<Option<Msg>, String> {
        let seq = self.clip.seq()?;
        if self.last_seq == Some(seq) {
            return Ok(None);
        }
        let text = self.clip.read_text()?;
        self.last_seq = Some(seq);
        match text {
            None => Ok(None),
            Some(t) => self.sync.on_local_change(&t).map_err(|e| e.to_string()),
        }
    }

    /// Apply a message from the peer. Non-clipboard messages are ignored.
    pub fn apply(&mut self, m: &Msg) -> Result<(), String> {
        if let Msg::ClipboardText { gen, text } = m {
            if let Some(t) = self.sync.on_remote(*gen, text).map_err(|e| e.to_string())? {
                self.clip.write_text(&t)?;
            }
        }
        Ok(())
    }
}

/// Drive one session: apply inbound messages, poll the clipboard every
/// `poll`, send changes. Returns when `incoming` is closed or `outgoing` is gone.
pub fn pump_loop(
    mut pump: Pump,
    incoming: &Receiver<Msg>,
    outgoing: &Sender<Msg>,
    poll: Duration,
    log: &dyn Fn(&str),
) {
    if let Err(e) = pump.prime() {
        log(&format!("clipboard baseline failed: {e}"));
    }
    loop {
        match incoming.recv_timeout(poll) {
            Ok(m) => match pump.apply(&m) {
                Ok(()) => {
                    if let Msg::ClipboardText { gen, text } = &m {
                        log(&format!("recv gen={gen} {} bytes", text.len()));
                    }
                }
                Err(e) => log(&format!("apply failed: {e}")),
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        match pump.poll() {
            Ok(Some(m)) => {
                if let Msg::ClipboardText { gen, text } = &m {
                    log(&format!("send gen={gen} {} bytes", text.len()));
                }
                if outgoing.send(m).is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(e) => log(&format!("poll failed: {e}")),
        }
    }
}

#[cfg(windows)]
pub type InjectorFactory = Box<dyn Fn() -> Box<dyn inject::Injector> + Sync>;
#[cfg(windows)]
pub type DisplayFactory = Box<dyn Fn() -> Result<Vec<flowseam_core::proto::Rect>, String> + Sync>;
#[cfg(windows)]
pub type ClipboardFactory = Box<dyn Fn() -> Result<Box<dyn Clipboard>, String> + Sync>;

#[cfg(windows)]
pub fn real_host_parts() -> (ClipboardFactory, InjectorFactory, DisplayFactory) {
    if let Err(e) = win_input::dpi_aware() {
        app::log(&format!("warning: {e}"));
    }
    (
        Box::new(|| Ok(Box::new(win32::WinClipboard::new()?) as Box<dyn Clipboard>)),
        Box::new(|| Box::new(win_input::SendInputInjector) as Box<dyn inject::Injector>),
        Box::new(win_input::displays),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[derive(Default)]
    struct State {
        text: Option<String>,
        seq: u32,
        fail_reads: u32,
        fail_writes: bool,
        writes: Vec<String>,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<State>>);

    impl Fake {
        fn user_copy(&self, t: &str) {
            let mut s = self.0.lock().unwrap();
            s.text = Some(t.into());
            s.seq += 1;
        }
        fn text(&self) -> Option<String> {
            self.0.lock().unwrap().text.clone()
        }
        fn writes(&self) -> Vec<String> {
            self.0.lock().unwrap().writes.clone()
        }
    }

    impl Clipboard for Fake {
        fn seq(&mut self) -> Result<u32, String> {
            Ok(self.0.lock().unwrap().seq)
        }
        fn read_text(&mut self) -> Result<Option<String>, String> {
            let mut s = self.0.lock().unwrap();
            if s.fail_reads > 0 {
                s.fail_reads -= 1;
                return Err("busy".into());
            }
            Ok(s.text.clone())
        }
        fn write_text(&mut self, t: &str) -> Result<(), String> {
            let mut s = self.0.lock().unwrap();
            if s.fail_writes {
                return Err("denied".into());
            }
            s.text = Some(t.into());
            s.seq += 1;
            s.writes.push(t.into());
            Ok(())
        }
    }

    fn gen_text(m: &Msg) -> (u64, String) {
        match m {
            Msg::ClipboardText { gen, text } => (*gen, text.clone()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn local_change_is_sent_once() {
        let f = Fake::default();
        let mut p = Pump::new(Box::new(f.clone()));
        p.prime().unwrap();
        assert_eq!(p.poll().unwrap(), None);
        f.user_copy("hi");
        assert_eq!(gen_text(&p.poll().unwrap().unwrap()), (1, "hi".into()));
        assert_eq!(p.poll().unwrap(), None, "same sequence is not re-read");
        f.user_copy("hi");
        assert_eq!(p.poll().unwrap(), None, "same text under a new sequence is not resent");
    }

    #[test]
    fn prime_adopts_existing_text_without_sending() {
        let f = Fake::default();
        f.user_copy("old");
        let mut p = Pump::new(Box::new(f.clone()));
        p.prime().unwrap();
        assert_eq!(p.poll().unwrap(), None);
        f.user_copy("new");
        assert_eq!(gen_text(&p.poll().unwrap().unwrap()).1, "new");
    }

    #[test]
    fn remote_text_is_written_and_not_echoed() {
        let f = Fake::default();
        let mut p = Pump::new(Box::new(f.clone()));
        p.prime().unwrap();
        p.apply(&Msg::ClipboardText { gen: 1, text: "from peer".into() }).unwrap();
        assert_eq!(f.text().as_deref(), Some("from peer"));
        assert_eq!(p.poll().unwrap(), None, "our own write must not bounce back");
        p.apply(&Msg::ClipboardText { gen: 1, text: "dup".into() }).unwrap();
        assert_eq!(f.writes(), vec!["from peer".to_string()], "stale gen not written");
    }

    #[test]
    fn failed_read_is_retried_not_dropped() {
        let f = Fake::default();
        let mut p = Pump::new(Box::new(f.clone()));
        p.prime().unwrap();
        f.user_copy("x");
        f.0.lock().unwrap().fail_reads = 1;
        assert!(p.poll().is_err());
        assert_eq!(gen_text(&p.poll().unwrap().unwrap()).1, "x");
    }

    #[test]
    fn failed_write_is_an_error() {
        let f = Fake::default();
        f.0.lock().unwrap().fail_writes = true;
        let mut p = Pump::new(Box::new(f));
        assert!(p.apply(&Msg::ClipboardText { gen: 1, text: "a".into() }).is_err());
    }

    #[test]
    fn oversize_local_text_is_refused_then_recovers() {
        let f = Fake::default();
        let mut p = Pump::new(Box::new(f.clone()));
        p.prime().unwrap();
        f.user_copy(&"a".repeat(flowseam_core::proto::MAX_CLIPBOARD_BYTES + 1));
        assert!(p.poll().is_err());
        f.user_copy("small");
        assert_eq!(gen_text(&p.poll().unwrap().unwrap()).1, "small");
    }

    #[test]
    fn two_pumps_sync_both_ways_without_ping_pong() {
        let (fa, fb) = (Fake::default(), Fake::default());
        let (a2b_tx, a2b_rx) = channel::<Msg>();
        let (b2a_tx, b2a_rx) = channel::<Msg>();
        let (ca, cb) = (fa.clone(), fb.clone());
        let _ha = thread::spawn(move || pump_loop(Pump::new(Box::new(ca)), &b2a_rx, &a2b_tx, Duration::from_millis(5), &|_| {}));
        let _hb = thread::spawn(move || pump_loop(Pump::new(Box::new(cb)), &a2b_rx, &b2a_tx, Duration::from_millis(5), &|_| {}));
        thread::sleep(Duration::from_millis(50));
        fa.user_copy("from a 剪貼簿");
        wait_for(|| fb.text().as_deref() == Some("from a 剪貼簿"));
        fb.user_copy("from b");
        wait_for(|| fa.text().as_deref() == Some("from b"));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(fb.writes(), vec!["from a 剪貼簿".to_string()], "b wrote only what a sent");
        assert_eq!(fa.writes(), vec!["from b".to_string()], "a wrote only what b sent");
    }

    fn wait_for(f: impl Fn() -> bool) {
        for _ in 0..200 {
            if f() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("condition not reached in 2s");
    }
}
