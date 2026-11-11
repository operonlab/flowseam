//! The tap's decision-maker: a `Forwarder` behind `CaptureSink`. Every captured event and cursor
//! sample goes to the forwarder; its `Disposition` is the tap's verdict. The forwarder is the single
//! owner of the switch decision (zone, dwell, chord, grab).

use crate::sink::EmitSink;
use flowseam_core::backend::{CaptureFault, CaptureSink, CapturedEvent, Disposition, InputEmitter};
use flowseam_core::forward::{Forwarder, Outbox};
use flowseam_core::proto::{Point, Rect};
use std::sync::{Arc, Mutex};

pub type Shared<E, O> = Arc<Mutex<Forwarder<EmitSink<E>, O>>>;
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Display rects are re-read at most this often; the tap callback must stay in microseconds.
const DISPLAY_REFRESH_MS: u64 = 1000;

pub struct ForwardSink<E: InputEmitter + Send, O: Outbox + Send> {
    fwd: Shared<E, O>,
    now: Clock,
    displays: Box<dyn FnMut() -> Option<Vec<Rect>> + Send>,
    last_refresh: Option<u64>,
    faults: Box<dyn CaptureSink>,
}

impl<E: InputEmitter + Send, O: Outbox + Send> ForwardSink<E, O> {
    /// `faults` only receives `on_tap_fault`; its `on_event` is never called.
    pub fn new(
        fwd: Shared<E, O>,
        now: Clock,
        displays: Box<dyn FnMut() -> Option<Vec<Rect>> + Send>,
        faults: Box<dyn CaptureSink>,
    ) -> Self {
        Self { fwd, now, displays, last_refresh: None, faults }
    }

    /// A grab that failed inside the forwarder leaves it in RemoteOut with the cursor free:
    /// back out (peer is told) so local input keeps working and the next dwell or chord retries.
    fn settle(&mut self, d: Disposition) -> Disposition {
        let mut f = self.fwd.lock().unwrap_or_else(|e| e.into_inner());
        if f.sink_mut().take_grab_failed() {
            f.abort_switch();
            return Disposition::Pass;
        }
        d
    }
}

impl<E: InputEmitter + Send, O: Outbox + Send> CaptureSink for ForwardSink<E, O> {
    fn on_event(&mut self, ev: CapturedEvent) -> Disposition {
        let now = (self.now)();
        let (d, need) = {
            let mut f = self.fwd.lock().unwrap_or_else(|e| e.into_inner());
            let d = f.on_captured(ev, now);
            (d, f.needs_tick())
        };
        if need { crate::front::wake_if_idle(); }
        self.settle(d)
    }

    fn on_cursor(&mut self, pos: Point) {
        let now = (self.now)();
        if self.last_refresh.is_none_or(|t| now.saturating_sub(t) >= DISPLAY_REFRESH_MS) {
            self.last_refresh = Some(now);
            match (self.displays)() {
                Some(d) => self.fwd.lock().unwrap_or_else(|e| e.into_inner()).set_displays(d),
                None => eprintln!("flowseam: display list unavailable; keeping the last one"),
            }
        }
        let need = {
            let mut f = self.fwd.lock().unwrap_or_else(|e| e.into_inner());
            f.on_cursor(pos, now);
            f.needs_tick()
        };
        if need { crate::front::wake_if_idle(); }
        self.settle(Disposition::Pass);
    }

    fn on_emergency(&mut self) {
        let now = (self.now)();
        let ev = CapturedEvent::Key { hid: flowseam_core::emergency::HID_ESCAPE, down: true };
        self.fwd.lock().unwrap_or_else(|e| e.into_inner()).on_captured(ev, now);
    }

    fn on_tap_fault(&mut self, f: CaptureFault) {
        self.faults.on_tap_fault(f);
    }
}
