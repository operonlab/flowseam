//! Peer links for the worker. The link layer is shared with every platform (`flowseam_net::links`); this
//! is what it needs from a Mac, and the test that runs it with this Mac's own input sink.

pub use flowseam_net::links::*;

/// This Mac's side of the link layer: its interfaces, free space, and the event tap's wake.
pub fn host() -> LinkHost {
    LinkHost {
        interfaces: crate::ifaces::local_interfaces,
        addrs: || Some(crate::ifaces::local_addrs()),
        free_bytes: crate::sys::free_bytes,
        wake: crate::front::wake_if_idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grab::mock::{Log, MockOs};
    use crate::grab::GrabGuard;
    use crate::sink::EmitSink;
    use crate::wire::Shared;
    use flowseam_core::backend::{BackendError, EmitOutcome, InputEmitter, Permission, Preflight};
    use flowseam_core::forward::{FwdConfig, Forwarder, Mode, PeerSpec};
    use flowseam_core::proto::{Edge, ModifierSet, Point, Rect};
    use flowseam_core::switch::SwitchPolicy;
    use flowseam_net::files::DragHub;
    use flowseam_net::identity::{fingerprint, Identity};
    use flowseam_net::pins::PinSet;
    use flowseam_net::session::{PeerInfo, Timing};
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    struct NullEm;
    type R = Result<EmitOutcome, BackendError>;
    impl InputEmitter for NullEm {
        fn preflight(&self) -> Preflight { Preflight::Ok }
        fn move_rel(&mut self, _: i32, _: i32) -> R { Ok(EmitOutcome::Delivered) }
        fn move_abs(&mut self, _: Point) -> R { Ok(EmitOutcome::Delivered) }
        fn button(&mut self, _: u8, _: bool) -> R { Ok(EmitOutcome::Delivered) }
        fn wheel(&mut self, _: i32, _: i32) -> R { Ok(EmitOutcome::Delivered) }
        fn key(&mut self, _: u16, _: bool, _: bool) -> R { Ok(EmitOutcome::Delivered) }
        fn set_modifiers(&mut self, _: ModifierSet) -> R { Ok(EmitOutcome::Delivered) }
        fn release_all(&mut self) -> Result<(), BackendError> { Ok(()) }
        fn set_cursor_visible(&mut self, _: bool) -> Result<(), BackendError> { Ok(()) }
    }

    struct Node {
        fwd: Shared<NullEm, RouteOutbox>,
        links: Links,
        _log: Log,
    }

    fn timing() -> Timing {
        Timing { ping_every: Duration::from_millis(50), dead_after: Duration::from_secs(2), handshake_timeout: Duration::from_secs(3) }
    }

    fn node(name: &str, id: &Identity, other: &Identity, other_name: &str, edge: Edge, dial: SocketAddr, drags: DragCfg) -> Node {
        let screen = Rect { x: 0, y: 0, w: 1000, h: 800 };
        let routes = RouteOutbox::default();
        let log = Log::default();
        let os = MockOs { log: log.clone(), preflight: Preflight::Missing(Permission::Accessibility), secure: Ok(Some(false)), fail_show: false };
        let guard = GrabGuard::new(Box::new(os), std::env::temp_dir().join(format!("flowseam-mac-links-{name}-{}.lock", std::process::id())));
        let cfg = FwdConfig { peers: vec![PeerSpec { name: other_name.into(), edge, keys: flowseam_core::config::KeyMap::None, gestures: Default::default() }], policy: SwitchPolicy::default(), boundary_px: 2 };
        let sink = EmitSink::new(NullEm, guard).with_drags(drags.hub.clone(), Box::new(|_| {}));
        let fwd = Arc::new(Mutex::new(Forwarder::new(cfg, vec![screen], sink, routes.clone())));
        let other_fp = fingerprint(&other.cert_der);
        let node_id: [u8; 16] = id.fingerprint()[..16].try_into().unwrap();
        let other_node: [u8; 16] = other_fp[..16].try_into().unwrap();
        let params = LinkParams {
            identity: id.clone(),
            me: PeerInfo { node: node_id, name: name.into(), displays: vec![screen] },
            monitors: vec![],
            peer_monitors: PeerMonitors::default(),
            monitors_home: MonitorsHome::default(),
            pins: PinSet::new([other_fp]),
            peers: vec![(other_name.into(), dial, other_node)],
            listen: "127.0.0.1:0".parse().unwrap(),
            timing: timing(),
            status: None,
            host: host(),
        };
        let t0 = Instant::now();
        let clock: Clock = Arc::new(move || t0.elapsed().as_millis() as u64);
        let links = Links::start(params, fwd.clone(), routes, clock, Arc::new(|| {}), None, Some(drags)).unwrap();
        Node { fwd, links, _log: log }
    }

    fn wait(secs: u64, mut f: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if f() {
                return true;
            }
            thread::sleep(Duration::from_millis(30));
        }
        false
    }

    /// The host hands the links this Mac's own interfaces, addresses and free space.
    #[test]
    fn the_host_is_this_macs_own() {
        use std::net::{IpAddr, Ipv4Addr};
        let h = host();
        assert!((h.addrs)().is_some_and(|a| a.contains(&IpAddr::V4(Ipv4Addr::LOCALHOST))), "a Mac lists its addresses, loopback among them");
        let names = |ifs: Vec<LocalIfOf>| ifs.into_iter().map(|i| i.name).collect::<Vec<_>>();
        assert_eq!(names((h.interfaces)()), names(crate::ifaces::local_interfaces()));
        assert!((h.free_bytes)(&std::env::temp_dir()).is_ok_and(|n| n > 0), "free space of the temp volume");
    }
    type LocalIfOf = flowseam_net::ifaces::LocalIf;

    struct Gen(Arc<Mutex<u64>>, Vec<PathBuf>);
    impl flowseam_net::files::DragSource for Gen {
        fn generation(&self) -> Result<u64, String> {
            Ok(*self.0.lock().unwrap())
        }
        fn files(&self) -> Result<Vec<PathBuf>, String> {
            Ok(self.1.clone())
        }
    }

    /// The whole path over real pinned TLS: A presses, a file drag starts, A's cursor dwells out to B, the
    /// button is released on B, and the file is in B's destination with the same bytes. Nothing lands
    /// before the release.
    #[test]
    fn a_file_dragged_across_lands_where_it_is_dropped() {
        use flowseam_core::backend::CapturedEvent;
        use flowseam_core::proto::PRIMARY_BUTTON;
        let root = std::env::temp_dir().join(format!("flowseam-mac-links-drag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["src", "sa", "da", "sb", "db"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(root.join("src/clip.mov"), &data).unwrap();
        let gen = Arc::new(Mutex::new(1));
        let cfg = |hub: DragHub, s: &str, d: &str| DragCfg { hub: Arc::new(Mutex::new(hub)), staging: root.join(s), dest: root.join(d), native: None };
        let (a_id, b_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let b = node("b", &b_id, &a_id, "a", Edge::Left, "127.0.0.1:1".parse().unwrap(), cfg(DragHub::new(None), "sb", "db"));
        let a_hub = DragHub::new(Some(Box::new(Gen(gen.clone(), vec![root.join("src/clip.mov")]))));
        let a = node("a", &a_id, &b_id, "b", Edge::Right, b.links.listening(), cfg(a_hub, "sa", "da"));
        assert!(wait(8, || a.links.live() == 1 && b.links.live() == 1), "link never came up");
        let t0 = Instant::now();
        let now = || t0.elapsed().as_millis() as u64;
        a.fwd.lock().unwrap().on_captured(CapturedEvent::Button { button: PRIMARY_BUTTON, down: true }, now());
        *gen.lock().unwrap() += 1; // the drag starts during the press
        let edge = Point { x: 999, y: 400 };
        assert!(wait(5, || {
            let mut f = a.fwd.lock().unwrap();
            f.on_cursor(edge, now());
            matches!(f.mode(), Mode::RemoteOut { .. })
        }));
        assert!(wait(5, || matches!(b.fwd.lock().unwrap().mode(), Mode::RemoteIn { .. })));
        thread::sleep(Duration::from_millis(500));
        assert!(std::fs::read_dir(root.join("db")).unwrap().next().is_none(), "nothing lands before the drop");
        a.fwd.lock().unwrap().on_captured(CapturedEvent::Button { button: PRIMARY_BUTTON, down: false }, now());
        assert!(
            wait(5, || std::fs::read(root.join("db/clip.mov")).map(|d| d == data).unwrap_or(false)),
            "file never landed: {:?}",
            std::fs::read_dir(root.join("db")).unwrap().map(|e| e.unwrap().file_name()).collect::<Vec<_>>()
        );
        a.links.stop();
        b.links.stop();
        let _ = std::fs::remove_dir_all(&root);
    }
}
