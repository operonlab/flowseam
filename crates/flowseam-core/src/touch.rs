//! Touchpad gestures from raw contact frames, by what they mean.
//!
//! A Windows Precision Touchpad reports finger positions, not gestures (Windows keeps the 3 and 4 finger
//! ones for itself). This reads frames of contacts and says which `Gesture` a peer should get, matching
//! what the Mac recognizer (`flowseam-mac::gesture`) sends for the same movement:
//!
//! - 3 or 4 fingers swiped sideways: fingers right is `SpaceLeft`, fingers left is `SpaceRight`. The Mac
//!   sends the same gesture for 3 and 4 fingers (its Trackpad settings decide the count), so does this.
//! - 3 or 4 fingers swiped vertically: up is `Overview`, down is `AppWindows`.
//! - 2 fingers pinching: `Zoom(steps)`, spread positive, a step per 15% change of the distance between them.
//!
//! Y is taken to grow downward, the Precision Touchpad convention (origin top left); unconfirmed on hardware.
//! Thumb-and-fingers pinch (`ShowDesktop`, `Launcher`) is not recognized. Two-finger scrolling arrives as
//! wheel events and is not this module's business: it only has to not be taken for a pinch.
//!
//! Pure logic: compiles everywhere.

use crate::proto::{Gesture, MAX_ZOOM_STEPS};

/// One finger in one frame, in device units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Contact {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    /// The finger touches the surface (a hovering one has no tip).
    pub tip: bool,
    /// The device does not think this contact is a palm.
    pub confident: bool,
}

/// The device's logical extents (the HID logical minimum and maximum of X and Y).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extents {
    pub x_min: i32,
    pub x_max: i32,
    pub y_min: i32,
    pub y_max: i32,
}

impl Extents {
    fn width(&self) -> f64 {
        f64::from(self.x_max) - f64::from(self.x_min)
    }
    fn height(&self) -> f64 {
        f64::from(self.y_max) - f64::from(self.y_min)
    }
}

/// Fraction of the pad's width (height for vertical) the fingers' centre travels for a swipe.
pub const SWIPE_FRACTION: f64 = 0.20;
/// The travel along the swipe's axis must be this many times the travel along the other axis.
pub const AXIS_RATIO: f64 = 1.5;
/// Fraction of the pad's diagonal the two fingers' distance must change before it can be a pinch.
pub const PINCH_START_FRACTION: f64 = 0.03;
/// Two fingers closer than this fraction of the diagonal are one finger and its shadow: no pinch.
pub const PINCH_MIN_DISTANCE_FRACTION: f64 = 0.05;
/// Change of the distance per zoom step, as the Mac counts magnification.
pub const ZOOM_STEP: f64 = 0.15;
/// A silence this long between frames means frames were lost: whatever was in progress is dropped.
pub const MAX_FRAME_GAP_MS: u32 = 500;
/// Most contacts that can be a gesture; more is a palm or a hand resting, and means nothing.
const MAX_FINGERS: usize = 4;

#[derive(Debug)]
enum Mode {
    Idle,
    Swipe { ids: Vec<u32>, start: (f64, f64) },
    Pinch { ids: [u32; 2], start: [(f64, f64); 2], reference: f64, latched: bool },
    /// A gesture fired, ended unfinished or was refused: nothing else until every finger is up.
    Spent,
}

/// One per touchpad: follows the contacts frame by frame.
#[derive(Debug)]
pub struct TouchReader {
    ext: Extents,
    mode: Mode,
    last_ms: Option<u32>,
}

impl TouchReader {
    /// `Err` for extents that cannot map positions (empty or inverted): the caller disables the path.
    pub fn new(ext: Extents) -> Result<Self, &'static str> {
        if ext.width() <= 0.0 || ext.height() <= 0.0 {
            return Err("touchpad logical extents are empty");
        }
        Ok(Self { ext, mode: Mode::Idle, last_ms: None })
    }

    /// The gesture this frame completes or advances, if any; at most one per frame. `time_ms` wraps.
    pub fn feed(&mut self, time_ms: u32, contacts: &[Contact]) -> Option<Gesture> {
        let gap = self.last_ms.is_some_and(|t| time_ms.wrapping_sub(t) > MAX_FRAME_GAP_MS);
        self.last_ms = Some(time_ms);
        if gap && !matches!(self.mode, Mode::Spent) {
            self.mode = Mode::Idle;
        }

        let mut live: Vec<(u32, (f64, f64))> = contacts
            .iter()
            .filter(|c| c.tip && c.confident)
            .map(|c| (c.id, (f64::from(c.x) - f64::from(self.ext.x_min), f64::from(c.y) - f64::from(self.ext.y_min))))
            .collect();
        live.sort_by_key(|(id, _)| *id);
        live.dedup_by_key(|(id, _)| *id);

        let n = live.len();
        if n == 0 {
            self.mode = Mode::Idle;
            return None;
        }
        if n > MAX_FINGERS {
            self.mode = Mode::Spent;
            return None;
        }
        let ids: Vec<u32> = live.iter().map(|(id, _)| *id).collect();
        let pts: Vec<(f64, f64)> = live.iter().map(|(_, p)| *p).collect();

        match &mut self.mode {
            Mode::Spent => None,
            Mode::Idle => {
                self.mode = Self::baseline(&ids, &pts);
                None
            }
            Mode::Swipe { ids: start_ids, start } => {
                if n < 3 {
                    // Fingers leaving: the swipe is over, and its tail must not become a pinch.
                    self.mode = Mode::Spent;
                    return None;
                }
                if *start_ids != ids {
                    self.mode = Self::baseline(&ids, &pts);
                    return None;
                }
                let c = centroid(&pts);
                let (fx, fy) = ((c.0 - start.0) / self.ext.width(), (c.1 - start.1) / self.ext.height());
                let g = swipe_gesture(fx, fy)?;
                self.mode = Mode::Spent;
                Some(g)
            }
            Mode::Pinch { ids: start_ids, start, reference, latched } => {
                if n != 2 {
                    // A pinch in progress ends with the count; one not begun hands over to a swipe or to nothing.
                    self.mode = if *latched {
                        Mode::Spent
                    } else if n >= 3 {
                        Self::baseline(&ids, &pts)
                    } else {
                        Mode::Idle
                    };
                    return None;
                }
                if start_ids[..] != ids[..] {
                    self.mode = if *latched { Mode::Spent } else { Self::baseline(&ids, &pts) };
                    return None;
                }
                let diag = self.ext.width().hypot(self.ext.height());
                let d = dist(pts[0], pts[1]);
                if !*latched {
                    let d0 = dist(start[0], start[1]);
                    if d0 < PINCH_MIN_DISTANCE_FRACTION * diag {
                        return None;
                    }
                    let v0 = (pts[0].0 - start[0].0, pts[0].1 - start[0].1);
                    let v1 = (pts[1].0 - start[1].0, pts[1].1 - start[1].1);
                    let opposed = v0.0 * v1.0 + v0.1 * v1.1 <= 0.0;
                    let shift = v0.0.midpoint(v1.0).hypot(v0.1.midpoint(v1.1));
                    let change = (d - d0).abs();
                    if !(opposed && change >= PINCH_START_FRACTION * diag && change >= AXIS_RATIO * shift) {
                        return None;
                    }
                    *latched = true;
                    *reference = d0;
                }
                let max = f64::from(MAX_ZOOM_STEPS);
                let steps = (((d / *reference) - 1.0) / ZOOM_STEP).trunc().clamp(-max, max);
                if steps == 0.0 {
                    return None;
                }
                // What is left of a step carries over: the reference moves by whole steps only.
                *reference = (*reference * (1.0 + ZOOM_STEP * steps)).max(f64::EPSILON);
                Some(Gesture::Zoom(steps as i8))
            }
        }
    }

    fn baseline(ids: &[u32], pts: &[(f64, f64)]) -> Mode {
        match ids.len() {
            2 => Mode::Pinch { ids: [ids[0], ids[1]], start: [pts[0], pts[1]], reference: dist(pts[0], pts[1]), latched: false },
            3 | 4 => Mode::Swipe { ids: ids.to_vec(), start: centroid(pts) },
            _ => Mode::Idle,
        }
    }
}

fn centroid(pts: &[(f64, f64)]) -> (f64, f64) {
    let n = pts.len() as f64;
    (pts.iter().map(|p| p.0).sum::<f64>() / n, pts.iter().map(|p| p.1).sum::<f64>() / n)
}

fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

/// `fx` / `fy`: the fingers' travel as a fraction of the pad's width / height.
fn swipe_gesture(fx: f64, fy: f64) -> Option<Gesture> {
    if fx.abs() >= SWIPE_FRACTION && fx.abs() >= AXIS_RATIO * fy.abs() {
        Some(if fx > 0.0 { Gesture::SpaceLeft } else { Gesture::SpaceRight })
    } else if fy.abs() >= SWIPE_FRACTION && fy.abs() >= AXIS_RATIO * fx.abs() {
        Some(if fy < 0.0 { Gesture::Overview } else { Gesture::AppWindows })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXT: Extents = Extents { x_min: 0, x_max: 1000, y_min: 0, y_max: 600 };

    fn reader() -> TouchReader {
        TouchReader::new(EXT).unwrap()
    }

    fn c(id: u32, x: i32, y: i32) -> Contact {
        Contact { id, x, y, tip: true, confident: true }
    }

    /// `n` fingers 60 units apart, centred at (cx, cy) shifted by (dx, dy).
    fn fingers(n: u32, cx: i32, cy: i32, dx: i32, dy: i32) -> Vec<Contact> {
        (0..n).map(|i| c(i + 1, cx + dx + 60 * i as i32 - 30 * (n as i32 - 1), cy + dy)).collect()
    }

    /// Feeds a straight move of `n` fingers from the centre by (dx, dy) in 10 frames, then lifts.
    /// Returns every gesture emitted and the frame index of each.
    fn stroke(r: &mut TouchReader, t0: u32, n: u32, dx: i32, dy: i32) -> Vec<(usize, Gesture)> {
        let mut out = Vec::new();
        for i in 0..=10 {
            let f = fingers(n, 500, 300, dx * i / 10, dy * i / 10);
            if let Some(g) = r.feed(t0 + 8 * i as u32, &f) {
                out.push((i as usize, g));
            }
        }
        assert_eq!(r.feed(t0 + 200, &[]), None);
        out
    }

    fn gestures(v: &[(usize, Gesture)]) -> Vec<Gesture> {
        v.iter().map(|(_, g)| *g).collect()
    }

    #[test]
    fn swipes_in_both_directions_with_three_and_four_fingers() {
        for n in [3, 4] {
            let mut r = reader();
            assert_eq!(gestures(&stroke(&mut r, 0, n, 300, 0)), [Gesture::SpaceLeft], "{n} right");
            assert_eq!(gestures(&stroke(&mut r, 1000, n, -300, 0)), [Gesture::SpaceRight], "{n} left");
            assert_eq!(gestures(&stroke(&mut r, 2000, n, 0, -200)), [Gesture::Overview], "{n} up");
            assert_eq!(gestures(&stroke(&mut r, 3000, n, 0, 200)), [Gesture::AppWindows], "{n} down");
        }
    }

    #[test]
    fn movement_below_the_threshold_emits_nothing() {
        let mut r = reader();
        for (dx, dy) in [(180, 0), (-180, 0), (0, 100), (0, -100), (0, 0)] {
            assert_eq!(stroke(&mut r, 0, 3, dx, dy), [], "({dx}, {dy})");
        }
        // Past it on both axes at once, with neither dominating: a diagonal is no swipe.
        assert_eq!(stroke(&mut r, 0, 3, 300, 250), []);
    }

    #[test]
    fn a_swipe_fires_once_however_long_it_goes_on() {
        let mut r = reader();
        let mut got = 0;
        for i in 0..60 {
            if r.feed(8 * i, &fingers(3, 500, 300, 15 * i as i32, 0)).is_some() {
                got += 1;
            }
        }
        assert_eq!(got, 1);
        // Back and forth with the fingers still down: still the one.
        for i in 0..60 {
            let x = if i % 20 < 10 { -300 } else { 300 };
            assert_eq!(r.feed(1000 + 8 * i, &fingers(3, 500, 300, x, 0)), None);
        }
        // Fingers up, down again: a new swipe counts.
        r.feed(2000, &[]);
        assert_eq!(gestures(&stroke(&mut r, 2100, 3, 300, 0)), [Gesture::SpaceLeft]);
    }

    #[test]
    fn a_swipe_ending_in_staggered_lifts_does_not_become_a_pinch() {
        let mut r = reader();
        let f = fingers(3, 500, 300, 100, 0);
        r.feed(0, &fingers(3, 500, 300, 0, 0));
        r.feed(8, &f);
        // Lift one at a time while the two left drift apart a lot.
        assert_eq!(r.feed(16, &[c(1, 300, 300), c(2, 700, 300)]), None);
        assert_eq!(r.feed(24, &[c(1, 100, 300), c(2, 900, 300)]), None);
        assert_eq!(r.feed(32, &[c(1, 0, 300)]), None);
        assert_eq!(r.feed(40, &[]), None);
    }

    #[test]
    fn the_finger_count_changing_before_a_swipe_restarts_it() {
        let mut r = reader();
        // Three fingers move 150 (below), a fourth lands: the travel so far does not count towards the swipe.
        for i in 0..=5 {
            assert_eq!(r.feed(8 * i, &fingers(3, 500, 300, 30 * i as i32, 0)), None);
        }
        assert_eq!(r.feed(48, &fingers(4, 500, 300, 150, 0)), None);
        assert_eq!(r.feed(56, &fingers(4, 500, 300, 250, 0)), None, "only 100 since the fourth landed");
        assert_eq!(r.feed(64, &fingers(4, 500, 300, 380, 0)), Some(Gesture::SpaceLeft), "230 since");
    }

    #[test]
    fn the_finger_count_changing_after_the_swipe_fires_emits_nothing_more() {
        let mut r = reader();
        assert_eq!(r.feed(0, &fingers(3, 500, 300, 0, 0)), None);
        assert_eq!(r.feed(8, &fingers(3, 500, 300, 300, 0)), Some(Gesture::SpaceLeft));
        assert_eq!(r.feed(16, &fingers(4, 500, 300, 300, 0)), None);
        assert_eq!(r.feed(24, &fingers(4, 500, 300, 700, 0)), None);
        assert_eq!(r.feed(32, &fingers(2, 500, 300, 700, 0)), None);
    }

    #[test]
    fn palm_and_hovering_contacts_are_ignored() {
        let mut r = reader();
        // Two real fingers plus a palm make no three-finger swipe...
        let mut got = None;
        for i in 0..=10 {
            let mut f = fingers(2, 500, 300, 40 * i, 0);
            f.push(Contact { id: 9, x: 100, y: 550, tip: true, confident: false });
            got = got.or(r.feed(8 * i as u32, &f));
        }
        assert_eq!(got, None);
        // ...and three fingers with one hovering are two.
        let mut r = reader();
        for i in 0..=10 {
            let mut f = fingers(3, 500, 300, 40 * i, 0);
            f[1].tip = false;
            assert_eq!(r.feed(8 * i as u32, &f), None);
        }
        // A palm among a real three does not stop the swipe or count towards it.
        let mut r = reader();
        let mut got = Vec::new();
        for i in 0..=10 {
            let mut f = fingers(3, 500, 300, 30 * i, 0);
            f.push(Contact { id: 9, x: 100, y: 550, tip: true, confident: false });
            got.extend(r.feed(8 * i as u32, &f));
        }
        assert_eq!(got, [Gesture::SpaceLeft]);
    }

    #[test]
    fn five_fingers_mean_nothing() {
        let mut r = reader();
        assert_eq!(stroke(&mut r, 0, 5, 400, 0), []);
    }

    /// Two fingers `d` apart on a horizontal line through the centre.
    fn pair(d: i32) -> Vec<Contact> {
        vec![c(1, 500 - d / 2, 300), c(2, 500 + d / 2, 300)]
    }

    #[test]
    fn pinch_steps() {
        let mut r = reader();
        r.feed(0, &pair(200));
        // 200 -> 260 is 30%: past the dead zone (35 units of change needed: 3% of 1166).
        let mut out = Vec::new();
        for (i, d) in [210, 225, 240, 260, 300].into_iter().enumerate() {
            out.extend(r.feed(8 * (i as u32 + 1), &pair(d)));
        }
        assert!(!out.is_empty() && out.iter().all(|g| matches!(g, Gesture::Zoom(s) if *s > 0)), "{out:?}");
        let total: i32 = out.iter().map(|g| if let Gesture::Zoom(s) = g { i32::from(*s) } else { 0 }).sum();
        assert_eq!(total, 3, "200 -> 300 is 50%: three whole steps");

        let mut r = reader();
        r.feed(0, &pair(300));
        let mut total = 0;
        for (i, d) in [280, 250, 220, 200, 150].into_iter().enumerate() {
            if let Some(Gesture::Zoom(s)) = r.feed(8 * (i as u32 + 1), &pair(d)) {
                assert!(s < 0);
                total += i32::from(s);
            }
        }
        assert_eq!(total, -4, "300 -> 150: each step is 15% of what is left, so 4 steps out");
    }

    #[test]
    fn a_pinch_with_one_finger_still_is_a_pinch() {
        let mut r = reader();
        r.feed(0, &[c(1, 400, 300), c(2, 600, 300)]);
        let mut out = Vec::new();
        for i in 1..=8 {
            out.extend(r.feed(8 * i, &[c(1, 400, 300), c(2, 600 + 15 * i as i32, 300)]));
        }
        assert!(!out.is_empty() && out.iter().all(|g| matches!(g, Gesture::Zoom(s) if *s > 0)), "{out:?}");
    }

    #[test]
    fn a_pinch_ends_with_the_finger_count() {
        let mut r = reader();
        r.feed(0, &pair(200));
        let mut n = 0;
        for (i, d) in [230, 260, 300].into_iter().enumerate() {
            n += usize::from(r.feed(8 * (i as u32 + 1), &pair(d)).is_some());
        }
        assert!(n > 0);
        // A third finger lands: no swipe, no more zoom, until everything is lifted.
        assert_eq!(r.feed(40, &[c(1, 350, 300), c(2, 650, 300), c(3, 500, 400)]), None);
        assert_eq!(r.feed(48, &[c(1, 100, 300), c(2, 900, 300), c(3, 500, 400)]), None);
        assert_eq!(r.feed(56, &pair(700)), None);
        r.feed(64, &[]);
        r.feed(72, &pair(200));
        assert!(r.feed(80, &pair(300)).is_some(), "a fresh pinch after lifting");
    }

    #[test]
    fn jittery_two_finger_scroll_is_not_a_pinch() {
        let mut r = reader();
        // A deterministic jitter: both fingers move down 400 units with a wobble on every axis.
        let wobble = [0, 9, -7, 12, -11, 6, -9, 14, -13, 8];
        for i in 0..40 {
            let w = wobble[i as usize % 10];
            let w2 = wobble[(i as usize + 3) % 10];
            let y = 100 + 10 * i;
            let f = [c(1, 450 + w, y + w2), c(2, 550 - w2, y - w)];
            assert_eq!(r.feed(8 * i as u32, &f), None, "frame {i}");
        }
        // A scroll where the fingers drift apart a little while moving together is still a scroll.
        let mut r = reader();
        for i in 0..40 {
            let y = 100 + 10 * i;
            assert_eq!(r.feed(8 * i as u32, &[c(1, 450 - i / 2, y), c(2, 550 + i / 2, y + 3)]), None, "drift {i}");
        }
    }

    #[test]
    fn two_fingers_close_together_do_not_pinch() {
        let mut r = reader();
        r.feed(0, &[c(1, 500, 300), c(2, 530, 300)]);
        for i in 1..10 {
            assert_eq!(r.feed(8 * i, &[c(1, 500 - 20 * i as i32, 300), c(2, 530 + 20 * i as i32, 300)]), None);
        }
    }

    #[test]
    fn zoom_steps_stay_within_bounds() {
        let mut r = reader();
        r.feed(0, &pair(60));
        let g = r.feed(8, &pair(1800));
        // The pinch has to begin first; the frame that starts it also carries the whole change.
        assert_eq!(g, Some(Gesture::Zoom(MAX_ZOOM_STEPS)));
    }

    #[test]
    fn lost_frames_drop_a_gesture_in_progress() {
        let mut r = reader();
        r.feed(0, &fingers(3, 500, 300, 0, 0));
        r.feed(8, &fingers(3, 500, 300, 100, 0));
        // 2 s of silence, then fingers elsewhere: the old start is gone, the move since does not reach 20%.
        assert_eq!(r.feed(2000, &fingers(3, 500, 300, 250, 0)), None);
        assert_eq!(r.feed(2008, &fingers(3, 500, 300, 350, 0)), None);
        assert_eq!(r.feed(2016, &fingers(3, 500, 300, 460, 0)), Some(Gesture::SpaceLeft));
    }

    #[test]
    fn the_clock_wrapping_is_not_a_gap() {
        let mut r = reader();
        let t = u32::MAX - 8;
        r.feed(t, &fingers(3, 500, 300, 0, 0));
        assert_eq!(r.feed(t.wrapping_add(16), &fingers(3, 500, 300, 300, 0)), Some(Gesture::SpaceLeft));
    }

    #[test]
    fn extents_are_honoured_and_degenerate_ones_refused() {
        assert!(TouchReader::new(Extents { x_min: 0, x_max: 0, y_min: 0, y_max: 100 }).is_err());
        assert!(TouchReader::new(Extents { x_min: 10, x_max: 5, y_min: 0, y_max: 100 }).is_err());
        // Thresholds follow the pad's size: 250 units is a swipe on a 1000 wide pad but not on a 2000 wide one.
        let wide = Extents { x_min: 0, x_max: 2000, y_min: 0, y_max: 1200 };
        let mut r = TouchReader::new(wide).unwrap();
        assert_eq!(stroke(&mut r, 0, 3, 250, 0), []);
        assert_eq!(gestures(&stroke(&mut r, 1000, 3, 450, 0)), [Gesture::SpaceLeft]);
        // A pad whose origin is not 0.
        let off = Extents { x_min: -500, x_max: 500, y_min: -300, y_max: 300 };
        let mut r = TouchReader::new(off).unwrap();
        r.feed(0, &fingers(3, 0, 0, 0, 0));
        assert_eq!(r.feed(8, &fingers(3, 0, 0, -300, 0)), Some(Gesture::SpaceRight));
    }
}
