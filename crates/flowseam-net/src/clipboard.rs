//! Text-only clipboard sync state (design §2, §7). No OS calls here: the
//! platform layer feeds `on_local_change` with what it reads and applies what
//! `on_remote` returns.
//!
//! Loop prevention: text that arrived from the peer becomes the "last text",
//! so the local watcher seeing the same value afterwards sends nothing back.
//! Remote generations are only compared with each other, so a stale or
//! duplicated message cannot roll the clipboard back.

use flowseam_core::clipboard::ClipboardOs;
use flowseam_core::proto::{Msg, MAX_CLIPBOARD_BYTES};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::NetError;

#[derive(Debug, Default)]
pub struct ClipboardSync {
    last_text: Option<String>,
    local_gen: u64,
    last_remote_gen: u64,
}

impl ClipboardSync {
    pub fn new() -> Self {
        Self::default()
    }

    /// The local clipboard changed to `text`. Returns the message to send, or
    /// `None` when nothing changed or the text is the one we just received.
    /// Oversize text is an error. It is never truncated.
    pub fn on_local_change(&mut self, text: &str) -> Result<Option<Msg>, NetError> {
        if self.last_text.as_deref() == Some(text) {
            return Ok(None);
        }
        check_size(text)?;
        self.local_gen += 1;
        self.last_text = Some(text.to_string());
        Ok(Some(Msg::ClipboardText { gen: self.local_gen, text: text.to_string() }))
    }

    /// The peer sent `text` with generation `gen`. Returns the text to write
    /// to the local clipboard, or `None` if the message is stale or changes
    /// nothing.
    pub fn on_remote(&mut self, gen: u64, text: &str) -> Result<Option<String>, NetError> {
        if gen <= self.last_remote_gen {
            return Ok(None);
        }
        check_size(text)?;
        self.last_remote_gen = gen;
        if self.last_text.as_deref() == Some(text) {
            return Ok(None);
        }
        self.last_text = Some(text.to_string());
        Ok(Some(text.to_string()))
    }
}

fn check_size(text: &str) -> Result<(), NetError> {
    if text.len() > MAX_CLIPBOARD_BYTES {
        Err(NetError::Oversize { len: text.len(), limit: MAX_CLIPBOARD_BYTES })
    } else {
        Ok(())
    }
}

/// Glue between the OS clipboard and `ClipboardSync` for one session. Every
/// failure is logged and swallowed: clipboard trouble never ends the link.
pub struct ClipboardPump {
    os: Arc<dyn ClipboardOs + Send + Sync>,
    sync: ClipboardSync,
    last_count: Option<u64>,
    /// Change count whose read last failed, so a stuck read is logged once, not every tick.
    failed_count: Option<u64>,
}

impl ClipboardPump {
    /// Starts from the current change count, so the text already on the
    /// clipboard when the session comes up is not sent.
    pub fn new(os: Arc<dyn ClipboardOs + Send + Sync>) -> Self {
        let last_count = match os.change_count() {
            Ok(n) => Some(n),
            Err(e) => {
                eprintln!("flowseam clipboard: initial change_count failed: {e}");
                None
            }
        };
        Self { os, sync: ClipboardSync::new(), last_count, failed_count: None }
    }

    /// One poll tick: a message to send if the local clipboard changed.
    pub fn poll(&mut self) -> Option<Msg> {
        let n = match self.os.change_count() {
            Ok(n) => n,
            Err(e) => {
                eprintln!("flowseam clipboard: change_count failed: {e}");
                return None;
            }
        };
        if self.last_count == Some(n) {
            return None;
        }
        // The count is consumed only by a read that worked: a clipboard held by another app is retried
        // on the next tick instead of losing that copy. Each failing count is logged once.
        let text = match self.os.read_text() {
            Ok(t) => {
                self.last_count = Some(n);
                self.failed_count = None;
                t?
            }
            Err(e) => {
                if self.failed_count != Some(n) {
                    eprintln!("flowseam clipboard: read failed (will retry): {e}");
                    self.failed_count = Some(n);
                }
                return None;
            }
        };
        match self.sync.on_local_change(&text) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("flowseam clipboard: local text not sent: {e}");
                None
            }
        }
    }

    /// A `ClipboardText` arrived from the peer.
    pub fn on_remote(&mut self, gen: u64, text: &str) {
        let to_write = match self.sync.on_remote(gen, text) {
            Ok(Some(t)) => t,
            Ok(None) => return,
            Err(e) => {
                eprintln!("flowseam clipboard: remote text refused: {e}");
                return;
            }
        };
        if let Err(e) = self.os.write_text(&to_write) {
            eprintln!("flowseam clipboard: write failed: {e}");
            return;
        }
        // Our own write bumped the counter; do not treat it as a local change.
        self.last_count = self.os.change_count().ok();
    }
}

/// Poll every `every` until `send` returns false (session gone) or `stop` is set.
pub fn run_poll(
    pump: &Mutex<ClipboardPump>,
    send: &dyn Fn(Msg) -> bool,
    stop: &dyn Fn() -> bool,
    every: Duration,
) {
    while !stop() {
        let m = pump.lock().unwrap_or_else(|e| e.into_inner()).poll();
        if let Some(m) = m {
            if !send(m) {
                return;
            }
        }
        std::thread::sleep(every);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_change_is_sent_once_with_increasing_gen() {
        let mut c = ClipboardSync::new();
        let m1 = c.on_local_change("hello").unwrap();
        assert_eq!(m1, Some(Msg::ClipboardText { gen: 1, text: "hello".into() }));
        assert_eq!(c.on_local_change("hello").unwrap(), None, "unchanged text is not resent");
        let m2 = c.on_local_change("world").unwrap();
        assert_eq!(m2, Some(Msg::ClipboardText { gen: 2, text: "world".into() }));
    }

    #[test]
    fn received_text_is_not_echoed_back() {
        let mut c = ClipboardSync::new();
        assert_eq!(c.on_remote(1, "from peer").unwrap(), Some("from peer".into()));
        // The local watcher now sees the value we just wrote: must stay silent.
        assert_eq!(c.on_local_change("from peer").unwrap(), None);
    }

    #[test]
    fn stale_or_duplicate_remote_generation_is_ignored() {
        let mut c = ClipboardSync::new();
        assert_eq!(c.on_remote(5, "new").unwrap(), Some("new".into()));
        assert_eq!(c.on_remote(5, "dup").unwrap(), None, "same gen twice");
        assert_eq!(c.on_remote(3, "old").unwrap(), None, "older gen after newer");
    }

    #[test]
    fn oversize_text_is_refused_not_truncated_both_ways() {
        let mut c = ClipboardSync::new();
        let big = "a".repeat(MAX_CLIPBOARD_BYTES + 1);
        assert!(matches!(c.on_local_change(&big), Err(NetError::Oversize { .. })));
        assert!(matches!(c.on_remote(1, &big), Err(NetError::Oversize { .. })));
        // Refusal must not poison state: the next valid change still goes out.
        assert!(c.on_local_change("ok").unwrap().is_some());
    }

    #[test]
    fn exactly_one_mib_is_accepted() {
        let mut c = ClipboardSync::new();
        let at = "a".repeat(MAX_CLIPBOARD_BYTES);
        assert!(c.on_local_change(&at).unwrap().is_some());
    }

    #[test]
    fn unicode_text_survives_unchanged() {
        let mut c = ClipboardSync::new();
        let s = "剪貼簿 ✓ 🙂";
        assert_eq!(c.on_remote(1, s).unwrap(), Some(s.to_string()));
    }

    #[derive(Default)]
    struct FakeInner {
        count: u64,
        text: Option<String>,
        fail_write: bool,
        fail_reads: u32,
        writes: Vec<String>,
    }
    #[derive(Default)]
    struct Fake(Mutex<FakeInner>);
    impl Fake {
        fn user_copies(&self, t: &str) {
            let mut g = self.0.lock().unwrap();
            g.count += 1;
            g.text = Some(t.into());
        }
    }
    impl ClipboardOs for Fake {
        fn change_count(&self) -> Result<u64, String> {
            Ok(self.0.lock().unwrap().count)
        }
        fn read_text(&self) -> Result<Option<String>, String> {
            let mut g = self.0.lock().unwrap();
            if g.fail_reads > 0 {
                g.fail_reads -= 1;
                return Err("held by another app".into());
            }
            Ok(g.text.clone())
        }
        fn write_text(&self, s: &str) -> Result<(), String> {
            let mut g = self.0.lock().unwrap();
            if g.fail_write {
                return Err("denied".into());
            }
            g.count += 1;
            g.text = Some(s.into());
            g.writes.push(s.into());
            Ok(())
        }
    }
    fn pump() -> (Arc<Fake>, ClipboardPump) {
        let f = Arc::new(Fake::default());
        let p = ClipboardPump::new(f.clone());
        (f, p)
    }

    #[test]
    fn pump_sends_local_change_once_and_not_before_a_change() {
        let (f, mut p) = pump();
        assert_eq!(p.poll(), None);
        f.user_copies("hi");
        assert_eq!(p.poll(), Some(Msg::ClipboardText { gen: 1, text: "hi".into() }));
        assert_eq!(p.poll(), None, "same change count: nothing more");
    }

    #[test]
    fn pump_writes_remote_text_and_does_not_echo_it() {
        let (f, mut p) = pump();
        p.on_remote(1, "from peer");
        assert_eq!(f.0.lock().unwrap().writes, vec!["from peer".to_string()]);
        assert_eq!(p.poll(), None, "our own write must not be sent back");
        // Even if the counter bump were seen as a change, the text matches: still silent.
        p.last_count = None;
        assert_eq!(p.poll(), None);
    }

    #[test]
    fn pump_drops_stale_remote_generation() {
        let (f, mut p) = pump();
        p.on_remote(5, "new");
        p.on_remote(3, "old");
        assert_eq!(f.0.lock().unwrap().writes, vec!["new".to_string()]);
    }

    #[test]
    fn pump_refuses_oversize_both_ways_without_truncating() {
        let (f, mut p) = pump();
        let big = "a".repeat(MAX_CLIPBOARD_BYTES + 1);
        f.user_copies(&big);
        assert_eq!(p.poll(), None);
        p.on_remote(1, &big);
        assert!(f.0.lock().unwrap().writes.is_empty());
        f.user_copies("small");
        assert!(p.poll().is_some(), "refusal does not wedge the pump");
    }

    #[test]
    fn pump_survives_write_failure() {
        let (f, mut p) = pump();
        f.0.lock().unwrap().fail_write = true;
        p.on_remote(1, "x");
        f.0.lock().unwrap().fail_write = false;
        f.user_copies("y");
        assert!(p.poll().is_some(), "link logic keeps working after a failed write");
    }

    #[test]
    fn pump_retries_a_change_whose_read_failed() {
        let (f, mut p) = pump();
        f.user_copies("copied while another app held the clipboard");
        f.0.lock().unwrap().fail_reads = 2;
        assert_eq!(p.poll(), None, "read failed");
        assert_eq!(p.poll(), None, "read failed again");
        assert_eq!(
            p.poll(),
            Some(Msg::ClipboardText { gen: 1, text: "copied while another app held the clipboard".into() }),
            "the copy must still go out once the read succeeds"
        );
        assert_eq!(p.poll(), None);
    }

    #[test]
    fn run_poll_ends_when_send_fails() {
        let (f, p) = pump();
        let p = Mutex::new(p);
        f.user_copies("z");
        let sent = Mutex::new(Vec::new());
        run_poll(&p, &|m| { sent.lock().unwrap().push(m); false }, &|| false, Duration::from_millis(1));
        assert_eq!(sent.lock().unwrap().len(), 1);
    }
}
