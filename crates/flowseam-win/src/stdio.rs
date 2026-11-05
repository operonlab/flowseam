//! Stand-in clipboard for hosts without the Win32 backend (and for `--stdio`
//! runs): each line on stdin becomes the clipboard text, and text written by
//! the peer is printed as `CLIPBOARD<- <text>`. It never touches the real
//! system clipboard, which is what makes cross-machine tests safe on a Mac.

use std::io::BufRead;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::Clipboard;

#[derive(Default)]
struct State {
    text: Option<String>,
    seq: u32,
}

#[derive(Clone, Default)]
pub struct StdioClipboard(Arc<Mutex<State>>);

impl StdioClipboard {
    /// Start the stdin reader thread. Call once; clone the handle per session.
    pub fn spawn_stdin_reader() -> Self {
        let c = StdioClipboard::default();
        let h = c.clone();
        thread::spawn(move || {
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                h.set(&line);
            }
        });
        c
    }

    pub fn set(&self, t: &str) {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        s.text = Some(t.to_string());
        s.seq = s.seq.wrapping_add(1);
    }
}

impl Clipboard for StdioClipboard {
    fn seq(&mut self) -> Result<u32, String> {
        Ok(self.0.lock().map_err(|e| e.to_string())?.seq)
    }
    fn read_text(&mut self) -> Result<Option<String>, String> {
        Ok(self.0.lock().map_err(|e| e.to_string())?.text.clone())
    }
    fn write_text(&mut self, text: &str) -> Result<(), String> {
        self.set(text);
        println!("CLIPBOARD<- {text}");
        Ok(())
    }
}
