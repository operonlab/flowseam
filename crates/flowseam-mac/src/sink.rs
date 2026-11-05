//! `InputSink` for the forwarder: injected input goes through an `InputEmitter`, cursor
//! and grab effects go through the shared `GrabGuard`, so every restore path stays single.

use crate::grab::GrabGuard;
use flowseam_core::backend::{BackendError, InputEmitter};
use flowseam_core::forward::InputSink;
use flowseam_core::proto::Point;
use flowseam_net::files::DragHub;
use std::sync::{Arc, Mutex};

/// Ends the drag in progress here without dropping it (`sys::cancel_drag_in_front` on a real Mac).
pub type CancelDrag = Box<dyn Fn() + Send>;

pub struct EmitSink<E: InputEmitter> {
    em: E,
    guard: GrabGuard,
    grab_failed: bool,
    drags: Option<(Arc<Mutex<DragHub>>, CancelDrag)>,
}

impl<E: InputEmitter> EmitSink<E> {
    pub fn new(em: E, guard: GrabGuard) -> Self {
        Self { em, guard, grab_failed: false, drags: None }
    }

    /// File drags: presses and crossings go to `hub`; a drag whose files left is ended here with `cancel`.
    pub fn with_drags(mut self, hub: Arc<Mutex<DragHub>>, cancel: CancelDrag) -> Self {
        self.drags = Some((hub, cancel));
        self
    }

    /// True once if a `set_grab(true)` since the last call failed. `InputSink::set_grab` has no
    /// return value, so the driver polls this and backs the forwarder out of the switch.
    pub fn take_grab_failed(&mut self) -> bool {
        std::mem::take(&mut self.grab_failed)
    }
}

fn log<T>(what: &str, r: Result<T, BackendError>) {
    if let Err(e) = r {
        eprintln!("flowseam: {what} failed: {e:?}");
    }
}

impl<E: InputEmitter> InputSink for EmitSink<E> {
    fn motion_rel(&mut self, dx: i32, dy: i32) {
        log("move_rel", self.em.move_rel(dx, dy));
    }
    fn button(&mut self, button: u8, down: bool) {
        log("button", self.em.button(button, down));
    }
    fn key(&mut self, hid: u16, down: bool, repeat: bool) {
        log("key", self.em.key(hid, down, repeat));
    }
    fn wheel(&mut self, dx: i32, dy: i32) {
        log("wheel", self.em.wheel(dx, dy));
    }
    fn warp(&mut self, p: Point) {
        log("warp", self.guard.with_os(|os, _| os.warp(p)));
    }
    fn set_grab(&mut self, on: bool) {
        if on {
            if let Err(e) = self.guard.grab() {
                eprintln!("flowseam: grab failed: {e:?}");
                self.grab_failed = true;
            }
        } else {
            log("restore", self.guard.restore());
        }
    }
    fn hide(&mut self) {
        log("hide", self.guard.hide());
    }
    fn show(&mut self) {
        log("show", self.guard.show());
    }
    fn set_association(&mut self, connected: bool) {
        log("set_association", self.guard.with_os(|os, _| os.set_association(connected)));
    }
    fn primary(&mut self, down: bool) {
        if let Some((hub, _)) = &self.drags {
            hub.lock().unwrap_or_else(|e| e.into_inner()).primary(down);
        }
    }
    fn drag_started_here(&mut self) -> bool {
        self.drags.as_ref().is_none_or(|(hub, _)| hub.lock().unwrap_or_else(|e| e.into_inner()).drag_started())
    }
    fn drag_away(&mut self, peer: &str) {
        let Some((hub, cancel)) = &self.drags else { return };
        if hub.lock().unwrap_or_else(|e| e.into_inner()).drag_away(peer) {
            // The files went to the peer; the drag here must end without dropping them at the edge.
            cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grab::mock::{Log, MockOs};
    use flowseam_core::backend::{EmitOutcome, Preflight};
    use flowseam_core::proto::ModifierSet;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Vec<String>>>);
    impl Fake {
        fn rec(&self, s: String) -> Result<EmitOutcome, BackendError> {
            self.0.lock().unwrap().push(s);
            Ok(EmitOutcome::Delivered)
        }
    }
    impl InputEmitter for Fake {
        fn preflight(&self) -> Preflight {
            Preflight::Ok
        }
        fn move_rel(&mut self, dx: i32, dy: i32) -> Result<EmitOutcome, BackendError> {
            self.rec(format!("move_rel {dx} {dy}"))
        }
        fn move_abs(&mut self, _: Point) -> Result<EmitOutcome, BackendError> {
            unreachable!("sink never uses move_abs")
        }
        fn button(&mut self, b: u8, d: bool) -> Result<EmitOutcome, BackendError> {
            self.rec(format!("button {b} {d}"))
        }
        fn wheel(&mut self, dx: i32, dy: i32) -> Result<EmitOutcome, BackendError> {
            self.rec(format!("wheel {dx} {dy}"))
        }
        fn key(&mut self, h: u16, d: bool, r: bool) -> Result<EmitOutcome, BackendError> {
            self.rec(format!("key {h} {d} {r}"))
        }
        fn set_modifiers(&mut self, _: ModifierSet) -> Result<EmitOutcome, BackendError> {
            unreachable!()
        }
        fn release_all(&mut self) -> Result<(), BackendError> {
            unreachable!()
        }
        fn set_cursor_visible(&mut self, _: bool) -> Result<(), BackendError> {
            unreachable!("hide/show go through the guard")
        }
    }

    fn rig(secure: bool) -> (EmitSink<Fake>, Fake, GrabGuard, Log) {
        let log = Log::default();
        let os = MockOs { log: log.clone(), preflight: Preflight::Ok, secure: Ok(Some(secure)), fail_show: false };
        let guard = GrabGuard::new(Box::new(os), std::env::temp_dir().join(format!("flowseam-mac-sink-{}-{secure}.lock", std::process::id())));
        let fake = Fake::default();
        (EmitSink::new(fake.clone(), guard.clone()), fake, guard, log)
    }

    #[test]
    fn input_methods_reach_the_emitter_with_their_arguments() {
        let (mut s, fake, _g, _l) = rig(false);
        s.motion_rel(3, -4);
        s.button(2, true);
        s.key(0x04, false, true);
        s.wheel(-1, 5);
        assert_eq!(*fake.0.lock().unwrap(), ["move_rel 3 -4", "button 2 true", "key 4 false true", "wheel -1 5"]);
    }

    #[test]
    fn cursor_effects_go_through_the_guard() {
        let (mut s, _f, g, l) = rig(false);
        s.warp(Point { x: 1, y: 2 });
        s.set_association(false);
        s.set_grab(true);
        assert!(g.is_grabbed());
        assert!(!s.take_grab_failed());
        s.set_grab(false);
        assert!(!g.is_grabbed());
        let calls = l.calls();
        assert!(calls.contains(&"warp".to_string()) && calls.contains(&"assoc:false".to_string()));
        assert!(calls.contains(&"hide".to_string()) && calls.contains(&"show".to_string()), "{calls:?}");
    }

    struct Bumped(Arc<Mutex<u64>>, std::path::PathBuf);
    impl flowseam_net::files::DragSource for Bumped {
        fn generation(&self) -> Result<u64, String> {
            Ok(*self.0.lock().unwrap())
        }
        fn files(&self) -> Result<Vec<std::path::PathBuf>, String> {
            Ok(vec![self.1.clone()])
        }
    }

    /// A file drag that leaves is cancelled here with Escape; a crossing that carries no new drag
    /// (a window dragged to the edge) must not press Escape on it.
    #[test]
    fn escape_cancels_only_a_drag_whose_files_left() {
        let dir = std::env::temp_dir().join(format!("flowseam-mac-sink-drag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f"), b"x").unwrap();
        let gen = Arc::new(Mutex::new(1));
        let mut hub = DragHub::new(Some(Box::new(Bumped(gen.clone(), dir.join("f")))));
        let pump = Arc::new(Mutex::new(flowseam_net::files::DragPump::new(dir.join("s"), dir.join("d"))));
        hub.attach("peer", pump, Arc::new(|_| true));
        let (s, fake, _g, _l) = rig(false);
        let cancels = Arc::new(Mutex::new(0));
        let c = cancels.clone();
        let mut s = s.with_drags(Arc::new(Mutex::new(hub)), Box::new(move || *c.lock().unwrap() += 1));
        s.primary(true);
        s.drag_away("peer");
        assert_eq!(*cancels.lock().unwrap(), 0, "no new drag: cancelling would end the window drag");
        s.primary(true);
        *gen.lock().unwrap() += 1;
        s.drag_away("peer");
        assert_eq!(*cancels.lock().unwrap(), 1);
        assert!(fake.0.lock().unwrap().is_empty(), "nothing goes through the emitter");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_grab_is_reported_once_and_leaves_no_grab() {
        let (mut s, _f, g, _l) = rig(true); // Secure Input on: the guard refuses to hide
        s.set_grab(true);
        assert!(!g.is_grabbed());
        assert!(s.take_grab_failed());
        assert!(!s.take_grab_failed());
    }
}
