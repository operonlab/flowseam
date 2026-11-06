//! A peer's relative motion, injected as absolute positions without reading the cursor back between moves.
//!
//! The OS reports a posted move's position only once it has processed the move, a few milliseconds later.
//! Reading it back to place the next move drops every delta that arrives sooner. Measured on macOS 26:
//! 100 moves of +5 travelled 10 px when sent back to back, 445 px at 1 ms apart, 495 px at 4 ms and
//! 500 px at 8 ms. [`PostedCursor`] therefore bases each move on where the previous one was sent.

use crate::geometry::clamp_to_displays;
use crate::proto::{Point, Rect};
use std::time::{Duration, Instant};

/// How long a sent position stays the base. After a longer pause the cursor is read again, since the local
/// mouse may have moved it meanwhile. The OS had caught up within 8 ms in the measurement above.
// ponytail: a fixed window; follow the local mouse's own events if it ever fights the peer mid-motion.
pub const TRUST_FOR: Duration = Duration::from_millis(100);

#[derive(Debug, Default)]
pub struct PostedCursor {
    last: Option<(Point, Instant)>,
}

impl PostedCursor {
    /// Where a move by `(dx, dy)` goes, kept on `displays`. `read` (the OS's position) is called only when
    /// nothing was placed within [`TRUST_FOR`] of `now`.
    pub fn target<E>(
        &self,
        dx: i32,
        dy: i32,
        displays: &[Rect],
        now: Instant,
        read: impl FnOnce() -> Result<Point, E>,
    ) -> Result<Point, E> {
        let base = match self.last {
            Some((p, at)) if now.saturating_duration_since(at) < TRUST_FOR => p,
            _ => read()?,
        };
        Ok(clamp_to_displays(displays, Point { x: base.x.saturating_add(dx), y: base.y.saturating_add(dy) }))
    }

    /// The cursor was sent to `p`, by a posted move or a warp.
    pub fn placed(&mut self, p: Point, now: Instant) {
        self.last = Some((p, now));
    }

    /// The cursor went somewhere not known here; the next move reads it.
    pub fn forget(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect { x: 0, y: 0, w: 1280, h: 800 };

    /// An OS whose reported position is one posted move behind.
    struct LaggingOs {
        shown: Point,
        queued: Option<Point>,
    }

    impl LaggingOs {
        fn read(&mut self) -> Result<Point, ()> {
            Ok(self.shown)
        }
        fn post(&mut self, p: Point) {
            if let Some(q) = self.queued.replace(p) {
                self.shown = q;
            }
        }
    }

    fn drive(c: &mut PostedCursor, os: &mut LaggingOs, d: (i32, i32), now: Instant) -> Point {
        let p = c.target(d.0, d.1, &[SCREEN], now, || os.read()).unwrap();
        os.post(p);
        c.placed(p, now);
        p
    }

    #[test]
    fn moves_closer_together_than_the_os_reports_them_add_up() {
        let mut os = LaggingOs { shown: Point { x: 100, y: 400 }, queued: None };
        let mut c = PostedCursor::default();
        let t = Instant::now();
        let mut last = Point { x: 0, y: 0 };
        for i in 0..100 {
            last = drive(&mut c, &mut os, (5, -1), t + Duration::from_micros(i * 200));
        }
        assert_eq!(last, Point { x: 600, y: 300 });
    }

    #[test]
    fn motion_into_an_edge_stops_on_it_and_reverses_at_once() {
        let mut os = LaggingOs { shown: Point { x: 1270, y: 400 }, queued: None };
        let mut c = PostedCursor::default();
        let t = Instant::now();
        assert_eq!(drive(&mut c, &mut os, (50, 0), t), Point { x: 1279, y: 400 });
        assert_eq!(drive(&mut c, &mut os, (50, 0), t), Point { x: 1279, y: 400 });
        assert_eq!(drive(&mut c, &mut os, (-10, 0), t), Point { x: 1269, y: 400 });
    }

    #[test]
    fn after_a_pause_or_a_forget_the_os_position_is_the_base_again() {
        let mut os = LaggingOs { shown: Point { x: 100, y: 100 }, queued: None };
        let mut c = PostedCursor::default();
        let t = Instant::now();
        drive(&mut c, &mut os, (5, 0), t);
        // The local mouse moved it meanwhile.
        os.shown = Point { x: 700, y: 500 };
        assert_eq!(c.target(1, 0, &[SCREEN], t + TRUST_FOR - Duration::from_millis(1), || os.read()), Ok(Point { x: 106, y: 100 }));
        assert_eq!(c.target(1, 0, &[SCREEN], t + TRUST_FOR, || os.read()), Ok(Point { x: 701, y: 500 }));
        c.forget();
        assert_eq!(c.target(1, 0, &[SCREEN], t, || os.read()), Ok(Point { x: 701, y: 500 }));
    }

    #[test]
    fn a_warp_is_the_base_for_the_move_right_after_it() {
        let mut os = LaggingOs { shown: Point { x: 100, y: 100 }, queued: None };
        let mut c = PostedCursor::default();
        let t = Instant::now();
        c.placed(Point { x: 24, y: 300 }, t);
        assert_eq!(drive(&mut c, &mut os, (3, 0), t), Point { x: 27, y: 300 });
    }
}
