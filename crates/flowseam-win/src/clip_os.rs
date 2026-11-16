//! This PC's clipboard for the shared link layer, which calls it from any thread. A Win32 clipboard window
//! belongs to the thread that made it, so one thread makes it, owns it, and answers every call in turn.

use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;
use std::thread;

use flowseam_core::clipboard::ClipboardOs;

use crate::Clipboard;

enum Call {
    Count(Sender<Result<u32, String>>),
    Read(Sender<Result<Option<String>, String>>),
    Write(String, Sender<Result<(), String>>),
}

pub struct ClipThread {
    calls: Mutex<Sender<Call>>,
}

const GONE: &str = "the clipboard thread is gone";

impl ClipThread {
    /// Starts the thread and builds the clipboard there with `make`; its error is returned here.
    pub fn start(make: impl FnOnce() -> Result<Box<dyn Clipboard>, String> + Send + 'static) -> Result<Self, String> {
        let (calls, inbox) = channel::<Call>();
        let (ready_tx, ready) = channel();
        thread::Builder::new()
            .name("flowseam-clipboard".into())
            .spawn(move || {
                let mut clip = match make() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                // Ends when the last handle goes: the sender is dropped with it.
                while let Ok(call) = inbox.recv() {
                    match call {
                        Call::Count(r) => drop(r.send(clip.seq())),
                        Call::Read(r) => drop(r.send(clip.read_text())),
                        Call::Write(t, r) => drop(r.send(clip.write_text(&t))),
                    }
                }
            })
            .map_err(|e| format!("cannot start the clipboard thread: {e}"))?;
        ready.recv().map_err(|_| GONE.to_string())??;
        Ok(Self { calls: Mutex::new(calls) })
    }

    fn ask<T>(&self, call: impl FnOnce(Sender<Result<T, String>>) -> Call) -> Result<T, String> {
        let (tx, rx) = channel();
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).send(call(tx)).map_err(|_| GONE.to_string())?;
        rx.recv().map_err(|_| GONE.to_string())?
    }
}

impl ClipboardOs for ClipThread {
    fn change_count(&self) -> Result<u64, String> {
        self.ask(Call::Count).map(u64::from)
    }
    fn read_text(&self) -> Result<Option<String>, String> {
        self.ask(Call::Read)
    }
    fn write_text(&self, s: &str) -> Result<(), String> {
        self.ask(|r| Call::Write(s.to_owned(), r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Refuses to be used from any thread but the one that made it, as a Win32 clipboard window does.
    struct Bound {
        home: thread::ThreadId,
        seq: u32,
        text: Option<String>,
    }
    impl Bound {
        fn here(&self) -> Result<(), String> {
            (thread::current().id() == self.home).then_some(()).ok_or_else(|| "used from another thread".into())
        }
    }
    impl Clipboard for Bound {
        fn seq(&mut self) -> Result<u32, String> {
            self.here()?;
            Ok(self.seq)
        }
        fn read_text(&mut self) -> Result<Option<String>, String> {
            self.here()?;
            Ok(self.text.clone())
        }
        fn write_text(&mut self, t: &str) -> Result<(), String> {
            self.here()?;
            self.text = Some(t.into());
            self.seq += 1;
            Ok(())
        }
    }

    #[test]
    fn every_call_runs_on_the_thread_that_made_the_clipboard() {
        let clip = Arc::new(ClipThread::start(|| Ok(Box::new(Bound { home: thread::current().id(), seq: 7, text: None }))).unwrap());
        assert_eq!(clip.change_count(), Ok(7));
        let other = clip.clone();
        thread::spawn(move || other.write_text("from another thread").unwrap()).join().unwrap();
        assert_eq!(clip.read_text(), Ok(Some("from another thread".into())));
        assert_eq!(clip.change_count(), Ok(8));
    }

    #[test]
    fn a_clipboard_that_cannot_be_made_is_an_error_at_start() {
        assert_eq!(ClipThread::start(|| Err("no window station".into())).err().as_deref(), Some("no window station"));
    }
}
