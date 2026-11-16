//! The injection sink of the headless peer: nothing is injected, what would be is printed as `INJECT` lines.
//! Keys are the wire's USB HID usages; mapping them to evdev codes belongs to the slice that really injects.
//!
//! The forwarder calls the sink with its mutex held, so the sink must never block: printing is handed to a
//! writer thread through a bounded channel, and lines that do not fit are dropped and counted.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;

use flowseam_core::forward::InputSink;
use flowseam_core::proto::Point;

/// The lines recorded so far (recording mode only).
pub type Tap = Arc<Mutex<Vec<String>>>;

/// Lines that may wait for the writer before new ones are dropped.
const QUEUE: usize = 1024;

#[derive(Clone)]
enum Mode {
    Recording(Tap),
    Printing { tx: SyncSender<String>, dropped: Arc<AtomicU64> },
}

#[derive(Clone)]
pub struct PrintSink(Mode);

impl Default for PrintSink {
    fn default() -> Self {
        Self::new()
    }
}

impl PrintSink {
    /// Prints to stdout through a writer thread.
    pub fn new() -> Self {
        Self::to_writer(std::io::stdout(), QUEUE)
    }

    /// Prints to `w` through a writer thread; at most `queue` lines wait for it.
    pub fn to_writer(mut w: impl Write + Send + 'static, queue: usize) -> Self {
        let (tx, rx) = sync_channel::<String>(queue);
        let dropped = Arc::new(AtomicU64::new(0));
        let pending = dropped.clone();
        thread::Builder::new()
            .name("flowseam-inject-print".into())
            .spawn(move || {
                while let Ok(line) = rx.recv() {
                    let _ = writeln!(w, "INJECT {line}").and_then(|()| w.flush());
                    let lost = pending.swap(0, Ordering::SeqCst);
                    if lost > 0 {
                        crate::app::log(&format!("the INJECT output fell behind: {lost} line(s) dropped"));
                    }
                }
            })
            .expect("spawn the inject printer");
        PrintSink(Mode::Printing { tx, dropped })
    }

    /// Records only: nothing is printed (tests).
    pub fn recording() -> Self {
        PrintSink(Mode::Recording(Tap::default()))
    }

    /// The recorded lines; empty for a printing sink, which keeps nothing.
    pub fn tap(&self) -> Tap {
        match &self.0 {
            Mode::Recording(t) => t.clone(),
            Mode::Printing { .. } => Tap::default(),
        }
    }

    /// Lines dropped since the writer last caught up (printing mode).
    pub fn dropped(&self) -> u64 {
        match &self.0 {
            Mode::Printing { dropped, .. } => dropped.load(Ordering::SeqCst),
            Mode::Recording(_) => 0,
        }
    }

    fn out(&self, line: String) {
        match &self.0 {
            Mode::Recording(t) => t.lock().unwrap_or_else(|e| e.into_inner()).push(line),
            Mode::Printing { tx, dropped } => match tx.try_send(line) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    dropped.fetch_add(1, Ordering::SeqCst);
                }
                Err(TrySendError::Disconnected(_)) => {}
            },
        }
    }
}

impl InputSink for PrintSink {
    fn motion_rel(&mut self, dx: i32, dy: i32) {
        self.out(format!("move {dx} {dy}"));
    }
    fn button(&mut self, button: u8, down: bool) {
        self.out(format!("button {button} {}", if down { "down" } else { "up" }));
    }
    fn key(&mut self, hid: u16, down: bool, repeat: bool) {
        self.out(format!("key {hid:#04x} {}{}", if down { "down" } else { "up" }, if repeat { " repeat" } else { "" }));
    }
    fn wheel(&mut self, dx: i32, dy: i32) {
        self.out(format!("wheel {dx} {dy}"));
    }
    fn warp(&mut self, p: Point) {
        self.out(format!("warp {} {}", p.x, p.y));
    }
    fn set_grab(&mut self, on: bool) {
        self.out(format!("grab {}", if on { "on" } else { "off" }));
    }
    fn hide(&mut self) {
        self.out("hide".into());
    }
    fn show(&mut self) {
        self.out("show".into());
    }
    fn set_association(&mut self, connected: bool) {
        self.out(format!("association {}", if connected { "on" } else { "off" }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Receiver};
    use std::time::{Duration, Instant};

    #[test]
    fn every_effect_is_one_line_and_kept() {
        let mut s = PrintSink::recording();
        let tap = s.tap();
        s.key(0x28, true, false);
        s.key(0x28, false, false);
        s.button(0, true);
        s.warp(Point { x: 3, y: 4 });
        assert_eq!(*tap.lock().unwrap(), ["key 0x28 down", "key 0x28 up", "button 0 down", "warp 3 4"]);
    }

    /// A pipe nobody reads: the first write blocks until the gate opens.
    struct Stuck(Receiver<()>, Arc<Mutex<Vec<u8>>>);
    impl Write for Stuck {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            self.1.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_printer_that_never_reads_does_not_block_the_sink_and_keeps_no_history() {
        let (open, gate) = channel();
        let written = Arc::new(Mutex::new(Vec::new()));
        let s = PrintSink::to_writer(Stuck(gate, written.clone()), 8);
        // On a thread of its own: a sink that blocks must fail this test, not hang the suite.
        let (done_tx, done) = channel();
        let mut driven = s.clone();
        thread::spawn(move || {
            for i in 0..10_000 {
                driven.motion_rel(i, 0);
            }
            let _ = done_tx.send(());
        });
        assert!(done.recv_timeout(Duration::from_secs(3)).is_ok(), "the sink blocked on a writer that never reads");
        assert!(s.dropped() > 9_000, "the overflow is counted: {}", s.dropped());
        assert!(s.tap().lock().unwrap().is_empty(), "a printing sink keeps no history");
        for _ in 0..20 {
            let _ = open.send(());
        }
        let end = Instant::now() + Duration::from_secs(5);
        while s.dropped() > 0 && Instant::now() < end {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(s.dropped(), 0, "the count is reported and reset once the writer catches up");
        assert!(String::from_utf8(written.lock().unwrap().clone()).unwrap().starts_with("INJECT move 0 0\n"));
    }
}
