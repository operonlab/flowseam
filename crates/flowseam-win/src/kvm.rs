//! Windows DDC adapter. One thread owns all physical display handles and runs the shared flow.

pub use flowseam_core::kvm::*;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

pub struct WindowsBackend;

#[cfg(any(windows, test))]
fn validate_id(id: &str) -> std::io::Result<()> {
    if crate::ddc::openable_id(id) { Ok(()) }
    else { Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "unstable monitor id cannot be opened safely")) }
}

#[cfg(any(windows, test))]
impl WindowsBackend {
    fn open_with<D>(id: &str, open: impl FnOnce(&str) -> std::io::Result<D>) -> std::io::Result<D> {
        validate_id(id)?;
        open(id)
    }
}

#[cfg(windows)]
impl DdcDisplay for crate::ddc::win::Display {
    fn get(&self, vcp: u8) -> Result<flowseam_core::ddc::Reply, flowseam_core::ddc::DdcError> {
        crate::ddc::win::Display::get(self, vcp).map_err(io_error)
    }
    fn set(&self, vcp: u8, value: u16) -> Result<(), flowseam_core::ddc::DdcError> {
        crate::ddc::win::Display::set(self, vcp, value).map_err(io_error)
    }
    fn capabilities(&self) -> Result<String, flowseam_core::ddc::DdcError> {
        crate::ddc::win::Display::capabilities(self).map_err(io_error)
    }
}

#[cfg(windows)]
fn io_error(e: std::io::Error) -> flowseam_core::ddc::DdcError {
    flowseam_core::ddc::DdcError::Io(e.raw_os_error().unwrap_or(-1))
}

#[cfg(windows)]
impl DdcOpener for WindowsBackend {
    type Display = crate::ddc::win::Display;
    type Error = std::io::Error;
    fn open(&mut self, id: &str) -> Result<Self::Display, Self::Error> {
        Self::open_with(id, crate::ddc::win::open)
    }
    fn failure(&self, error: &Self::Error) -> OpenFailure {
        match error.kind() {
            std::io::ErrorKind::NotFound => OpenFailure::NotFound,
            _ => OpenFailure::Other,
        }
    }
    fn local_name(&self) -> &'static str { "PC" }
    fn local_name_possessive(&self) -> &'static str { "this PC's" }
    fn no_channel_reason(&self) -> &'static str { "the monitor has no DDC channel" }
    fn retain_away_handles(&self) -> bool { true }
}

/// The opener and every display it opens stay on this one thread.
pub fn spawn<O: DdcOpener + Send + 'static>(opener: O) -> std::io::Result<(Sender<Cmd>, Receiver<Done>)> {
    let (tx, rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    thread::Builder::new().name("flowseam-ddc".into()).spawn(move || {
        let mut runner = DdcRunner::new(opener, thread::sleep as fn(Duration));
        let mut queue = std::collections::VecDeque::new();
        loop {
            if queue.is_empty() {
                match rx.recv() {
                    Ok(cmd) => queue.push_back(cmd),
                    Err(_) => break,
                }
            }
            queue.extend(rx.try_iter());
            let Some(cmd) = next_cmd(&mut queue) else { continue };
            if done_tx.send(runner.run(cmd)).is_err() { break; }
        }
    })?;
    Ok((tx, done_rx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flowseam_core::ddc::{DdcError, Reply};
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Fake { writes: Arc<Mutex<Vec<u16>>> }
    struct FakeDisplay { writes: Arc<Mutex<Vec<u16>>> }
    impl DdcDisplay for FakeDisplay {
        fn get(&self, _: u8) -> Result<Reply, DdcError> {
            Ok(Reply { current: self.writes.lock().unwrap().last().copied().unwrap_or(18), max: 21 })
        }
        fn set(&self, _: u8, value: u16) -> Result<(), DdcError> {
            self.writes.lock().unwrap().push(value);
            Ok(())
        }
        fn capabilities(&self) -> Result<String, DdcError> { Ok("(vcp(60(0f 12)))".into()) }
    }
    impl DdcOpener for Fake {
        type Display = FakeDisplay;
        type Error = String;
        fn open(&mut self, id: &str) -> Result<FakeDisplay, String> {
            if id == "offline" { return Err("offline".into()); }
            Ok(FakeDisplay { writes: self.writes.clone() })
        }
        fn failure(&self, _: &String) -> OpenFailure { OpenFailure::NotFound }
        fn local_name(&self) -> &'static str { "PC" }
        fn local_name_possessive(&self) -> &'static str { "this PC's" }
        fn no_channel_reason(&self) -> &'static str { "no DDC channel" }
    }
    fn monitor(id: &str) -> Monitor { Monitor { uuid: id.into(), input: 15, home: 18 } }

    #[test]
    fn thread_waits_for_a_command_before_any_write() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = spawn(Fake { writes: writes.clone() }).unwrap();
        assert!(writes.lock().unwrap().is_empty());
        tx.send(Cmd::Away { peer: "p".into(), target: Target::one(monitor("ok")) }).unwrap();
        assert!(matches!(rx.recv().unwrap(), Done::Away { result: Ok(()), .. }));
        assert_eq!(*writes.lock().unwrap(), [15]);
    }

    #[test]
    fn windows_backend_open_rejects_unstable_id_before_hardware_opener() {
        let error = WindowsBackend::open_with("unstable:123", |_| -> std::io::Result<()> { panic!("must not open a physical monitor") }).unwrap_err();
        assert!(error.to_string().contains("unstable"));
        assert_eq!(WindowsBackend::open_with("instance:DISPLAY\\MONITOR", |id| Ok(id.to_string())).unwrap(), "instance:DISPLAY\\MONITOR");
    }

    #[test]
    fn shared_probe_and_inspect_report_inputs_without_writing() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut runner = DdcRunner::new(Fake { writes: writes.clone() }, |_| {});
        let Done::Probe { check, .. } = runner.run(Cmd::Probe { peer: "p".into(), monitor: monitor("ok") }) else { panic!("probe") };
        assert_eq!(check.state, flowseam_core::status::CheckState::Ok);
        let Done::Inspect(status) = runner.run(Cmd::Inspect { uuid: "ok".into() }) else { panic!("inspect") };
        assert_eq!(status.inputs, Some(vec![15, 18]));
        assert_eq!(status.current, Some(18));
        assert!(writes.lock().unwrap().is_empty());
    }

    #[test]
    fn policy_returns_the_monitor_after_a_session_end() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut runner = DdcRunner::new(Fake { writes: writes.clone() }, |_| {});
        let mut policy = Kvm::new();
        let steps = policy.request("p", Some(Target::one(monitor("ok"))), flowseam_core::forward::WayBack::Open);
        let Step::Ddc(cmd) = steps.into_iter().next().unwrap() else { panic!("DDC first") };
        assert!(matches!(policy.done(runner.run(cmd)).as_slice(), [Step::SwitchInput(..)]));
        let Step::Ddc(cmd) = policy.tick(None, 1).into_iter().next().unwrap() else { panic!("return") };
        policy.done(runner.run(cmd));
        assert_eq!(*writes.lock().unwrap(), [15, 18]);
        assert_eq!(policy.phase(), &Phase::Idle);
    }
}
