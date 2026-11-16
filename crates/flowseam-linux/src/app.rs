//! Process-level wiring: config, identity, pairing and the worker. The worker runs the shared links
//! (`flowseam_net::links`) with one `Forwarder` for every configured peer, as the Windows and Mac workers do;
//! here the sink only prints what a peer's input would do.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use flowseam_core::clipboard::ClipboardOs;
use flowseam_core::config::{Config, KeyMap};
use flowseam_core::forward::{Forwarder, FwdConfig};
use flowseam_core::proto::Rect;
use flowseam_net::identity::Identity;
use flowseam_net::links::{Clock, LinkHost, LinkParams, Links, MonitorsHome, PeerMonitors, RouteOutbox};
use flowseam_net::node;
use flowseam_net::session::{PeerInfo, Timing};

use crate::host;
use crate::sink::PrintSink;

pub fn log(msg: &str) {
    flowseam_core::log::line(&format!("[flowseam-linux] {msg}"));
}

const NAMES: node::Names = node::Names { log, me: "this peer", connect_to: "here" };

pub use node::identity_dir;

/// Print the code, wait for one successful pairing, store the client's fingerprint.
pub fn pair_serve(config: &Path, peer: &str, peer_addr: &str) -> Result<(), String> {
    node::pair_serve_with(NAMES, config, peer, peer_addr, KeyMap::Position, |code| println!("PAIR CODE: {code}"), |fp| println!("PAIRED {peer} {fp}"))
}

/// Dial a pairing server and store its fingerprint under `peer`.
pub fn pair_dial(config: &Path, peer: &str, addr: &str, code: &str) -> Result<(), String> {
    node::pair_dial(config, peer, addr, code, KeyMap::Position)
}

/// What a worker needs from this machine besides the config.
pub struct Machine {
    pub sink: PrintSink,
    pub displays: Vec<Rect>,
    pub clipboard: Option<Arc<dyn ClipboardOs + Send + Sync>>,
    pub link: LinkHost,
}

/// The headless peer: a fixed 1920x1080 screen, stdin as the clipboard, input printed.
pub fn headless() -> Machine {
    Machine {
        sink: PrintSink::new(),
        displays: vec![Rect { x: 0, y: 0, w: 1920, h: 1080 }],
        clipboard: Some(Arc::new(crate::stdio::StdioClipboard::spawn_stdin_reader())),
        link: host::link_host(),
    }
}

pub type Fwd = Arc<Mutex<Forwarder<PrintSink, RouteOutbox>>>;

/// A running worker. Production never stops one; tests do.
pub struct Worker {
    pub links: Links,
    pub fwd: Fwd,
    stop: Arc<AtomicBool>,
    clock_thread: thread::JoinHandle<()>,
}

impl Worker {
    /// Blocks while the clock thread runs, which in production is for good.
    fn wait(self) -> Result<(), String> {
        Err(match self.clock_thread.join() {
            Ok(()) => "the clock thread stopped".to_string(),
            Err(_) => "the clock thread panicked".to_string(),
        })
    }

    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        self.links.stop();
        node::wake_clocks();
        let _ = self.clock_thread.join();
    }
}

fn fwd_config(cfg: &Config) -> Result<FwdConfig, String> {
    // "mac" keys translate a Mac keyboard's shortcuts for a PC; this peer sends its keys by position.
    node::fwd_config_all(cfg, |p| if p.keys == KeyMap::Mac { KeyMap::Position } else { p.keys })
}

/// The kernel's host name; `HOSTNAME` is a shell variable that is usually not exported to us.
fn host_name() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is writable for its whole length; gethostname NUL-terminates unless it truncates.
        if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let n = String::from_utf8_lossy(&buf[..end]).into_owned();
            if !n.is_empty() {
                return n;
            }
        }
    }
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "flowseam-linux".into())
}

fn node_info(id: &Identity, displays: Vec<Rect>) -> PeerInfo {
    node::node_info(id, displays, &host_name())
}

/// Start the worker for `config` on `m`.
pub fn start(config: &Path, m: Machine) -> Result<Worker, String> {
    let cfg = node::load(config)?;
    cfg.validate_for_run().map_err(|e| e.to_string())?;
    let fwd_cfg = fwd_config(&cfg)?;
    let id = Identity::load_or_create(&identity_dir(config)).map_err(|e| e.to_string())?;
    let routes = RouteOutbox::default();
    let fwd: Fwd = Arc::new(Mutex::new(Forwarder::new(fwd_cfg, m.displays.clone(), m.sink, routes.clone())));
    let t0 = Instant::now();
    let clock: Clock = Arc::new(move || t0.elapsed().as_millis() as u64);
    let home_asks = MonitorsHome::default();
    let params = LinkParams {
        identity: id.clone(),
        me: node_info(&id, m.displays),
        monitors: Vec::new(),
        peer_monitors: PeerMonitors::default(),
        monitors_home: home_asks.clone(),
        pins: node::pins_of(&cfg),
        peers: node::link_peers(&cfg, NAMES),
        listen: node::resolve(&cfg.listen)?,
        timing: Timing::default(),
        status: None,
        host: m.link,
    };
    let links = Links::start(params, fwd.clone(), routes.clone(), clock.clone(), Arc::new(|| log("a link thread panicked")), m.clipboard, None)
        .map_err(|e| format!("listen on {}: {e:?}", cfg.listen))?;
    log(&format!("listening on {}", links.listening()));
    let stop = Arc::new(AtomicBool::new(false));
    let clock_thread = {
        let (fwd, stop, routes) = (fwd.clone(), stop.clone(), routes.clone());
        thread::Builder::new()
            .name("flowseam-clock".into())
            .spawn(move || clock_loop(&fwd, routes, &clock, &stop, &home_asks))
            .map_err(|e| format!("cannot start the clock thread: {e}"))?
    };
    Ok(Worker { links, fwd, stop, clock_thread })
}

/// Run the worker for good. Returns only if the clock thread stops: a worker without its clock would stay
/// linked but never hand control back, so the process ends and its supervisor starts a new one.
pub fn run(config: &Path, m: Machine) -> Result<(), String> {
    start(config, m)?.wait()
}

/// The shared clock loop with nothing of its own to add: this peer has no menu, no hooks and no display refresh.
struct NoHooks;

impl node::ClockHooks<PrintSink, RouteOutbox> for NoHooks {}

fn clock_loop(fwd: &Fwd, routes: RouteOutbox, clock: &Clock, stop: &AtomicBool, home_asks: &MonitorsHome) {
    node::clock_loop(fwd, routes, &node::ClockEnv { clock, stop, peers: &[], home_asks, names: NAMES }, &mut NoHooks);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_name_is_not_empty_and_fits_the_wire() {
        let id = Identity::generate().unwrap();
        let n = node_info(&id, Vec::new()).name;
        assert!(!n.is_empty() && n.chars().count() <= 32, "{n:?}");
    }
}
