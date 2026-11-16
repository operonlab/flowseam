//! Monitors that switch along with a session started from the menu. The button first moves the chosen
//! monitors to the peer's inputs and only then hands over the keyboard and mouse; when that session
//! ends by any path (the peer's own "back" button, the chord, the edge, a dropped link, a display
//! topology change, the worker stopping), the monitors go back to this Mac's inputs.
//!
//! `Kvm` is the policy and holds no OS state. `mac::spawn` runs the DDC side on one thread of its
//! own: the display handle cannot leave the thread that opened it, and a single owner keeps this
//! process to one writer on the I²C bus (two writers at once read back garbage).

pub use flowseam_core::kvm::*;

#[cfg(target_os = "macos")]
pub mod mac {
    use super::{Cmd, DdcDisplay, DdcOpener, DdcRunner, Done, OpenFailure};
    use crate::ddc::{mac::Display, mac::OpenError, DdcError, Reply};
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::thread;
    use std::time::Duration;

    pub struct MacOpener;

    impl DdcDisplay for Display {
        fn get(&self, vcp: u8) -> Result<Reply, DdcError> { Display::get(self, vcp) }
        fn set(&self, vcp: u8, value: u16) -> Result<(), DdcError> { Display::set(self, vcp, value) }
        fn capabilities(&self) -> Result<String, DdcError> { Display::capabilities(self) }
    }

    impl DdcOpener for MacOpener {
        type Display = Display;
        type Error = OpenError;
        fn open(&mut self, uuid: &str) -> Result<Display, OpenError> { crate::ddc::mac::open(uuid) }
        fn failure(&self, error: &OpenError) -> OpenFailure {
            match error {
                OpenError::NotFound => OpenFailure::NotFound,
                OpenError::NoAvService => OpenFailure::NoChannel,
                _ => OpenFailure::Other,
            }
        }
        fn local_name(&self) -> &'static str { "Mac" }
        fn local_name_possessive(&self) -> &'static str { "this Mac's" }
        fn no_channel_reason(&self) -> &'static str { "the monitor has no DDC channel (built-in, virtual or AirPlay)" }
    }

    /// Starts the DDC thread. It ends when the `Cmd` sender is dropped.
    pub fn spawn() -> std::io::Result<(Sender<Cmd>, Receiver<Done>)> {
        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (done_tx, done_rx) = channel::<Done>();
        thread::Builder::new().name("flowseam-ddc".into()).spawn(move || {
            let mut runner = DdcRunner::new(MacOpener, thread::sleep as fn(Duration));
            let mut queue = std::collections::VecDeque::new();
            loop {
                if queue.is_empty() {
                    match cmd_rx.recv() {
                        Ok(cmd) => queue.push_back(cmd),
                        Err(_) => break,
                    }
                }
                queue.extend(cmd_rx.try_iter());
                let Some(cmd) = super::next_cmd(&mut queue) else { continue };
                if done_tx.send(runner.run(cmd)).is_err() { break; }
            }
        })?;
        Ok((cmd_tx, done_rx))
    }

    /// Runs one command on this thread, for a caller with no DDC thread (the GUI after a worker died).
    pub fn run_now(cmd: Cmd) -> Done {
        DdcRunner::new(MacOpener, thread::sleep as fn(Duration)).run(cmd)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn not_found_open_error_is_not_found() {
            assert_eq!(MacOpener.failure(&OpenError::NotFound), OpenFailure::NotFound);
        }

        #[test]
        fn missing_av_service_is_no_channel() {
            assert_eq!(MacOpener.failure(&OpenError::NoAvService), OpenFailure::NoChannel);
        }

        #[test]
        fn other_open_errors_are_other() {
            assert_eq!(MacOpener.failure(&OpenError::DisplayList(-1)), OpenFailure::Other);
            assert_eq!(MacOpener.failure(&OpenError::MissingSymbol("symbol")), OpenFailure::Other);
        }
    }
}
