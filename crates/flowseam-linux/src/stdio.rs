//! The headless peer's clipboard: each line on stdin becomes the clipboard text, and text written by a peer is
//! printed as `CLIPBOARD<- <text>`. It never touches a real clipboard.

use std::io::BufRead;
use std::sync::{Arc, Mutex};
use std::thread;

use flowseam_core::clipboard::ClipboardOs;

#[derive(Default)]
struct State {
    text: Option<String>,
    seq: u64,
}

#[derive(Clone, Default)]
pub struct StdioClipboard(Arc<Mutex<State>>);

impl StdioClipboard {
    /// Feeds the clipboard from stdin on a thread of its own.
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
        s.seq += 1;
    }
}

impl ClipboardOs for StdioClipboard {
    fn change_count(&self) -> Result<u64, String> {
        Ok(self.0.lock().map_err(|e| e.to_string())?.seq)
    }
    fn read_text(&self) -> Result<Option<String>, String> {
        Ok(self.0.lock().map_err(|e| e.to_string())?.text.clone())
    }
    fn write_text(&self, text: &str) -> Result<(), String> {
        self.set(text);
        println!("CLIPBOARD<- {text}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_line_is_readable_and_bumps_the_count() {
        let c = StdioClipboard::default();
        assert_eq!((c.change_count().unwrap(), c.read_text().unwrap()), (0, None));
        c.set("hi");
        assert_eq!((c.change_count().unwrap(), c.read_text().unwrap().as_deref()), (1, Some("hi")));
    }
}
