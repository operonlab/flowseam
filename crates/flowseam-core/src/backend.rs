//! Platform backend boundary (design §4). flowseam_core defines only the trait
//! surface; no OS API lives here.

use crate::proto::{Gesture, ModifierSet, Point, Rect};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Accessibility,
    InputMonitoring,
    PostEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preflight {
    Ok,
    Missing(Permission),
    Unknown(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Pass,
    Swallow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapturedEvent {
    Key { hid: u16, down: bool },
    Button { button: u8, down: bool },
    Motion { dx: i32, dy: i32 },
    Wheel { dx: i32, dy: i32 },
    /// A trackpad gesture, once the capture side knows what it means.
    Gesture(Gesture),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureFault {
    TapDisabledByTimeout,
    TapDisabledByUserInput,
    /// The local emergency chord fired. The cursor is already restored; the worker
    /// should release remote keys and tell the peer (design §7).
    Emergency,
}

/// Every backend failure is typed; none is a bare string (design §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    PermissionMissing(Permission),
    TapCreateFailed,
    SecureInput,
    Os { code: i32, api: &'static str },
    Unsupported,
}

/// Three-state emit result: OS APIs such as Windows UIPI may silently drop input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmitOutcome {
    Delivered,
    Unknown(String),
}

pub trait CaptureSink: Send {
    /// Must return in microseconds: channel send only, no blocking, no OS calls.
    fn on_event(&mut self, ev: CapturedEvent) -> Disposition;
    fn on_tap_fault(&mut self, f: CaptureFault);
    /// Cursor position sampled by the tap on non-key events. Default: ignored.
    fn on_cursor(&mut self, _pos: Point) {}
    /// The emergency chord fired on the tap. The Esc itself is not delivered as an event.
    fn on_emergency(&mut self) {}
}

pub trait InputCapture {
    fn preflight(&self) -> Preflight;
    fn displays(&self) -> Result<Vec<Rect>, BackendError>;
    fn set_display_change_sink(&mut self, cb: Box<dyn Fn() + Send>) -> Result<(), BackendError>;
    /// Installing the tap. `Preflight::Unknown` must not be treated as success.
    fn start(&mut self, sink: Box<dyn CaptureSink>) -> Result<(), BackendError>;
    fn stop(&mut self) -> Result<(), BackendError>;
    /// `grabbed = true`: swallow local input, hide and freeze the cursor. Idempotent.
    fn set_grabbed(&mut self, grabbed: bool) -> Result<(), BackendError>;
    fn cursor_position(&self) -> Result<Point, BackendError>;
    fn warp_cursor(&mut self, p: Point) -> Result<(), BackendError>;
    /// `Ok(None)` means the query itself cannot tell; it is not "inactive".
    fn secure_input_active(&self) -> Result<Option<bool>, BackendError>;
}

pub trait InputEmitter {
    fn preflight(&self) -> Preflight;
    fn move_rel(&mut self, dx: i32, dy: i32) -> Result<EmitOutcome, BackendError>;
    fn move_abs(&mut self, p: Point) -> Result<EmitOutcome, BackendError>;
    fn button(&mut self, button: u8, down: bool) -> Result<EmitOutcome, BackendError>;
    fn wheel(&mut self, dx: i32, dy: i32) -> Result<EmitOutcome, BackendError>;
    fn key(&mut self, hid: u16, down: bool, repeat: bool) -> Result<EmitOutcome, BackendError>;
    fn set_modifiers(&mut self, mods: ModifierSet) -> Result<EmitOutcome, BackendError>;
    /// Release every key this side injected and not yet released, then clear state.
    fn release_all(&mut self) -> Result<(), BackendError>;
    /// Idempotent. The backend tracks state itself; it does not count calls.
    fn set_cursor_visible(&mut self, visible: bool) -> Result<(), BackendError>;
    /// Something other than this emitter put the cursor at `at` (a warp), or somewhere unknown (`None`).
    /// An emitter that bases moves on its own last one starts from here instead.
    fn cursor_placed(&mut self, _at: Option<Point>) {}
}

pub trait Clipboard {
    fn change_count(&self) -> Result<u64, BackendError>;
    fn get_text(&self) -> Result<Option<String>, BackendError>;
    fn set_text(&mut self, s: &str) -> Result<(), BackendError>;
}

/// Composition convenience; capture and inject may also be split across machines.
pub trait InputBackend: InputCapture + InputEmitter + Clipboard {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what it is told. Exists so the trait surface is exercised as
    /// trait objects, which is the only test a trait can meaningfully have.
    #[derive(Default)]
    struct MockBackend {
        grabbed: bool,
        cursor_visible: bool,
        released: usize,
        events: usize,
    }

    impl CaptureSink for MockBackend {
        fn on_event(&mut self, _ev: CapturedEvent) -> Disposition {
            self.events += 1;
            Disposition::Swallow
        }
        fn on_tap_fault(&mut self, _f: CaptureFault) {}
    }

    impl InputCapture for MockBackend {
        fn preflight(&self) -> Preflight {
            Preflight::Missing(Permission::Accessibility)
        }
        fn displays(&self) -> Result<Vec<Rect>, BackendError> {
            Ok(vec![Rect { x: 0, y: 0, w: 10, h: 10 }])
        }
        fn set_display_change_sink(&mut self, _cb: Box<dyn Fn() + Send>) -> Result<(), BackendError> {
            Ok(())
        }
        fn start(&mut self, _sink: Box<dyn CaptureSink>) -> Result<(), BackendError> {
            Err(BackendError::TapCreateFailed)
        }
        fn stop(&mut self) -> Result<(), BackendError> {
            Ok(())
        }
        fn set_grabbed(&mut self, grabbed: bool) -> Result<(), BackendError> {
            self.grabbed = grabbed;
            Ok(())
        }
        fn cursor_position(&self) -> Result<Point, BackendError> {
            Err(BackendError::Unsupported)
        }
        fn warp_cursor(&mut self, _p: Point) -> Result<(), BackendError> {
            Ok(())
        }
        fn secure_input_active(&self) -> Result<Option<bool>, BackendError> {
            Ok(None)
        }
    }

    impl InputEmitter for MockBackend {
        fn preflight(&self) -> Preflight {
            Preflight::Ok
        }
        fn move_rel(&mut self, _dx: i32, _dy: i32) -> Result<EmitOutcome, BackendError> {
            Ok(EmitOutcome::Delivered)
        }
        fn move_abs(&mut self, _p: Point) -> Result<EmitOutcome, BackendError> {
            Ok(EmitOutcome::Unknown("uipi".into()))
        }
        fn button(&mut self, _b: u8, _d: bool) -> Result<EmitOutcome, BackendError> {
            Ok(EmitOutcome::Delivered)
        }
        fn wheel(&mut self, _dx: i32, _dy: i32) -> Result<EmitOutcome, BackendError> {
            Ok(EmitOutcome::Delivered)
        }
        fn key(&mut self, _h: u16, _d: bool, _r: bool) -> Result<EmitOutcome, BackendError> {
            Ok(EmitOutcome::Delivered)
        }
        fn set_modifiers(&mut self, _m: ModifierSet) -> Result<EmitOutcome, BackendError> {
            Ok(EmitOutcome::Delivered)
        }
        fn release_all(&mut self) -> Result<(), BackendError> {
            self.released += 1;
            Ok(())
        }
        fn set_cursor_visible(&mut self, visible: bool) -> Result<(), BackendError> {
            self.cursor_visible = visible;
            Ok(())
        }
    }

    impl Clipboard for MockBackend {
        fn change_count(&self) -> Result<u64, BackendError> {
            Ok(0)
        }
        fn get_text(&self) -> Result<Option<String>, BackendError> {
            Ok(None)
        }
        fn set_text(&mut self, _s: &str) -> Result<(), BackendError> {
            Ok(())
        }
    }

    impl InputBackend for MockBackend {}

    #[test]
    fn trait_objects_compile_and_dispatch() {
        let mut b: Box<dyn InputBackend> = Box::new(MockBackend::default());
        b.set_grabbed(true).unwrap();
        b.set_cursor_visible(false).unwrap();
        b.release_all().unwrap();
        assert_eq!(b.move_abs(Point { x: 1, y: 1 }).unwrap(), EmitOutcome::Unknown("uipi".into()));
        assert_eq!(InputEmitter::preflight(b.as_ref()), Preflight::Ok);
        assert_eq!(b.secure_input_active().unwrap(), None, "None means cannot tell");
        assert!(matches!(b.start(Box::new(MockBackend::default())), Err(BackendError::TapCreateFailed)));
    }

    #[test]
    fn capture_sink_is_a_boxed_object() {
        let mut sink: Box<dyn CaptureSink> = Box::new(MockBackend::default());
        assert_eq!(
            sink.on_event(CapturedEvent::Key { hid: 4, down: true }),
            Disposition::Swallow
        );
        sink.on_tap_fault(CaptureFault::TapDisabledByTimeout);
    }
}
