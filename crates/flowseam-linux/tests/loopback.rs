//! A flowseam-linux worker linked with another node over real pinned TLS on 127.0.0.1: the other node drives
//! it, the worker receives Enter and a key, and its print sink records the key.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use flowseam_core::backend::CapturedEvent;
use flowseam_core::config::{Config, Edge, KeyMap, Peer};
use flowseam_core::forward::{Forwarder, FwdConfig, InputSink, Mode, PeerSpec};
use flowseam_core::proto::{Point, Rect};
use flowseam_core::switch::SwitchPolicy;
use flowseam_linux::app::{self, Machine};
use flowseam_linux::sink::PrintSink;
use flowseam_net::identity::{to_hex, Identity};
use flowseam_net::links::{Clock, LinkHost, LinkParams, Links, MonitorsHome, PeerMonitors, RouteOutbox};
use flowseam_net::pins::PinSet;
use flowseam_net::session::{PeerInfo, Timing};

struct NullSink;
impl InputSink for NullSink {
    fn motion_rel(&mut self, _: i32, _: i32) {}
    fn button(&mut self, _: u8, _: bool) {}
    fn key(&mut self, _: u16, _: bool, _: bool) {}
    fn wheel(&mut self, _: i32, _: i32) {}
    fn warp(&mut self, _: Point) {}
    fn set_grab(&mut self, _: bool) {}
    fn hide(&mut self) {}
    fn show(&mut self) {}
    fn set_association(&mut self, _: bool) {}
}

fn loopback() -> LinkHost {
    LinkHost { interfaces: Vec::new, addrs: || Some(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]), free_bytes: |_| Ok(u64::MAX), wake: app_wake }
}

fn app_wake() {
    flowseam_linux::host::wake_clocks();
}

fn wait(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn a_peer_drives_the_linux_worker_and_its_key_is_injected() {
    let root = std::env::temp_dir().join(format!("flowseam-linux-loopback-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let config = root.join("config.toml");

    // The Linux worker's identity exists before the config does, so the controller can pin it.
    let linux_id = Identity::load_or_create(&app::identity_dir(&config)).unwrap();
    let ctl_id = Identity::generate().unwrap();

    let mut cfg = Config { listen: "127.0.0.1:0".into(), ..Config::default() };
    cfg.peers.push(Peer { name: "ctl".into(), addr: "127.0.0.1:1".into(), edge: Edge::Left, fingerprint: to_hex(&ctl_id.fingerprint()), keys: KeyMap::None, gestures: Default::default(), displays: Vec::new() });
    cfg.save(&config).unwrap();

    let sink = PrintSink::recording();
    let tap = sink.tap();
    let worker = app::start(&config, Machine { sink, displays: vec![Rect { x: 0, y: 0, w: 1920, h: 1080 }], clipboard: None, link: loopback() }).unwrap();

    // The controller: a node with one forwarder that sends to "linux" from its right edge.
    let screen = Rect { x: 0, y: 0, w: 1000, h: 800 };
    let routes = RouteOutbox::default();
    let fcfg = FwdConfig { peers: vec![PeerSpec { name: "linux".into(), edge: Edge::Right.into(), keys: KeyMap::None, gestures: Default::default() }], policy: SwitchPolicy::default(), boundary_px: 2 };
    let ctl_fwd = Arc::new(Mutex::new(Forwarder::new(fcfg, vec![screen], NullSink, routes.clone())));
    let params = LinkParams {
        identity: ctl_id.clone(),
        me: PeerInfo { node: ctl_id.fingerprint()[..16].try_into().unwrap(), name: "ctl".into(), displays: vec![screen] },
        monitors: vec![],
        peer_monitors: PeerMonitors::default(),
        monitors_home: MonitorsHome::default(),
        pins: PinSet::new([linux_id.fingerprint()]),
        peers: vec![("linux".into(), worker.links.listening(), linux_id.fingerprint()[..16].try_into().unwrap())],
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        timing: Timing { ping_every: Duration::from_millis(50), dead_after: Duration::from_secs(2), handshake_timeout: Duration::from_secs(3) },
        status: None,
        host: loopback(),
    };
    let t0 = Instant::now();
    let clock: Clock = Arc::new(move || t0.elapsed().as_millis() as u64);
    let ctl_links = Links::start(params, ctl_fwd.clone(), routes, clock.clone(), Arc::new(|| {}), None, None).unwrap();

    assert!(wait(8, || ctl_links.live() == 1 && worker.links.live() == 1), "the link never came up");
    let went_out = wait(8, || {
        let mut f = ctl_fwd.lock().unwrap();
        f.on_cursor(Point { x: 999, y: 400 }, clock());
        matches!(f.mode(), Mode::RemoteOut { .. })
    });
    assert!(went_out, "the controller never switched out");
    assert!(wait(5, || matches!(worker.fwd.lock().unwrap().mode(), Mode::RemoteIn { .. })), "the worker never received Enter");
    assert!(tap.lock().unwrap().iter().all(|l| !l.starts_with("key ")), "no key before one was sent: {:?}", tap.lock().unwrap());

    for down in [true, false] {
        ctl_fwd.lock().unwrap().on_captured(CapturedEvent::Key { hid: 0x28, down }, clock());
    }
    assert!(wait(5, || {
        let t = tap.lock().unwrap();
        t.iter().any(|l| l == "key 0x28 down") && t.iter().any(|l| l == "key 0x28 up")
    }), "the key never reached the sink: {:?}", tap.lock().unwrap());

    ctl_links.stop();
    worker.stop();
    let _ = std::fs::remove_dir_all(&root);
}
