//! Trackpad gestures from the tap's gesture events, by what they mean.
//!
//! WindowServer has already decided what the fingers did: a dock swipe event says horizontal, vertical or
//! pinch and how far it went, a zoom event how much it magnified, whatever the finger count and the
//! Trackpad settings. The fields are private (see `ffi::F_GESTURE_*`); what their values mean comes from
//! iss (github.com/joshuarli/iss) and TwinDesk (github.com/InukaAsith/TwinDesk), which read the same events.
//! Rotation, swipe-between-pages and the begin/end markers have no meaning here: swallowed while the peer
//! is driven, like every other gesture event.
//!
//! Pure logic: compiles everywhere.

use flowseam_core::proto::{Gesture, MAX_ZOOM_STEPS};

/// What the tap reads off one gesture event. Fields an event does not carry read as 0.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Fields {
    /// The event type the tap was called with.
    pub ty: u32,
    pub hid_type: i64,
    pub phase: i64,
    pub motion: i64,
    pub progress: f64,
    pub velocity: f64,
    pub zoom: f64,
}

pub const TY_SMART_MAGNIFY: u32 = 32;
const HID_ZOOM: i64 = 8;
const HID_ZOOM_TOGGLE: i64 = 22;
const HID_DOCK_SWIPE: i64 = 23;
const BEGAN: i64 = 1;
const CHANGED: i64 = 2;
const ENDED: i64 = 4;
const CANCELLED: i64 = 8;
const HORIZONTAL: i64 = 1;
const VERTICAL: i64 = 2;
/// Dock swipe progress past which a swipe counts, as TwinDesk measures it: the system itself snaps a
/// smaller one back.
const SWIPE_MIN: f64 = 0.1;
/// Magnification per zoom step.
const ZOOM_STEP: f64 = 0.15;

#[derive(Debug, Clone, Copy)]
struct Swipe {
    motion: i64,
    progress: f64,
    moved: bool,
}

/// One per tap: follows a dock swipe from its start to its end, and adds up a pinch into steps.
#[derive(Debug)]
pub struct GestureReader {
    /// macOS 26 reports horizontal progress with the opposite sign of the releases before and after it (iss).
    right_is_positive: bool,
    swipe: Option<Swipe>,
    zoom: f64,
}

impl GestureReader {
    pub fn new(macos_major: u32) -> Self {
        Self { right_is_positive: macos_major == 26, swipe: None, zoom: 0.0 }
    }

    /// The gesture this event completes, if any. A swipe means something only at its end; a pinch every
    /// time it adds up to another step.
    pub fn feed(&mut self, f: &Fields) -> Option<Gesture> {
        if f.ty == TY_SMART_MAGNIFY || f.hid_type == HID_ZOOM_TOGGLE {
            return Some(Gesture::SmartZoom);
        }
        match f.hid_type {
            HID_DOCK_SWIPE => self.dock_swipe(f),
            HID_ZOOM => self.pinch(f),
            _ => None,
        }
    }

    fn dock_swipe(&mut self, f: &Fields) -> Option<Gesture> {
        match f.phase {
            BEGAN => {
                self.swipe = Some(Swipe { motion: f.motion, progress: f.progress, moved: false });
                None
            }
            CHANGED => {
                let s = self.swipe.get_or_insert(Swipe { motion: f.motion, progress: 0.0, moved: false });
                s.progress = f.progress;
                s.moved |= f.progress != 0.0;
                None
            }
            ENDED => {
                let s = self.swipe.take()?;
                let progress = if f.progress != 0.0 { f.progress } else { s.progress };
                // A flick can end with no progress reported at all: then its speed says which way (iss).
                let way = if progress.abs() > SWIPE_MIN {
                    progress
                } else if !s.moved {
                    f.velocity
                } else {
                    0.0
                };
                if way == 0.0 || !way.is_finite() {
                    return None;
                }
                let motion = if s.motion != 0 { s.motion } else { f.motion };
                match motion {
                    HORIZONTAL if (way > 0.0) == self.right_is_positive => Some(Gesture::SpaceRight),
                    HORIZONTAL => Some(Gesture::SpaceLeft),
                    VERTICAL if way < 0.0 => Some(Gesture::Overview),
                    VERTICAL => Some(Gesture::AppWindows),
                    3 | 4 if way < 0.0 => Some(Gesture::Launcher),
                    3 | 4 => Some(Gesture::ShowDesktop),
                    _ => None,
                }
            }
            CANCELLED => {
                self.swipe = None;
                None
            }
            _ => None,
        }
    }

    fn pinch(&mut self, f: &Fields) -> Option<Gesture> {
        if f.phase == BEGAN {
            self.zoom = 0.0;
        }
        if f.zoom.is_finite() {
            self.zoom += f.zoom;
        }
        let max = f64::from(MAX_ZOOM_STEPS);
        let steps = (self.zoom / ZOOM_STEP).trunc().clamp(-max, max);
        let out = if steps != 0.0 {
            // What is left of a step carries over; whatever the bound cut off does not.
            self.zoom %= ZOOM_STEP;
            Some(Gesture::Zoom(steps as i8))
        } else {
            None
        };
        if matches!(f.phase, ENDED | CANCELLED) {
            self.zoom = 0.0;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dock(phase: i64, motion: i64, progress: f64, velocity: f64) -> Fields {
        Fields { ty: 30, hid_type: HID_DOCK_SWIPE, phase, motion, progress, velocity, zoom: 0.0 }
    }

    /// A whole swipe: began, a few changes towards `to`, ended there.
    fn swipe(r: &mut GestureReader, motion: i64, to: f64) -> Option<Gesture> {
        assert_eq!(r.feed(&dock(BEGAN, motion, 0.0, 0.0)), None);
        for i in 1..=4 {
            assert_eq!(r.feed(&dock(CHANGED, motion, to * f64::from(i) / 4.0, 0.0)), None, "only the end means something");
        }
        r.feed(&dock(ENDED, motion, to, 0.0))
    }

    #[test]
    fn horizontal_direction_follows_the_macos_release() {
        let mut r26 = GestureReader::new(26);
        assert_eq!(swipe(&mut r26, HORIZONTAL, 0.8), Some(Gesture::SpaceRight));
        assert_eq!(swipe(&mut r26, HORIZONTAL, -0.8), Some(Gesture::SpaceLeft));
        for other in [15, 27] {
            let mut r = GestureReader::new(other);
            assert_eq!(swipe(&mut r, HORIZONTAL, 0.8), Some(Gesture::SpaceLeft), "macOS {other}");
            assert_eq!(swipe(&mut r, HORIZONTAL, -0.8), Some(Gesture::SpaceRight), "macOS {other}");
        }
    }

    #[test]
    fn vertical_and_pinch_swipes() {
        let mut r = GestureReader::new(26);
        assert_eq!(swipe(&mut r, VERTICAL, -0.6), Some(Gesture::Overview));
        assert_eq!(swipe(&mut r, VERTICAL, 0.6), Some(Gesture::AppWindows));
        for pinch in [3, 4] {
            assert_eq!(swipe(&mut r, pinch, -0.6), Some(Gesture::Launcher));
            assert_eq!(swipe(&mut r, pinch, 0.6), Some(Gesture::ShowDesktop));
        }
        assert_eq!(swipe(&mut r, 9, 0.6), None, "an unknown motion means nothing");
    }

    #[test]
    fn a_swipe_the_system_would_snap_back_means_nothing() {
        let mut r = GestureReader::new(26);
        for small in [0.1, -0.1, 0.05, 0.0] {
            assert_eq!(swipe(&mut r, HORIZONTAL, small), None, "{small}");
        }
        assert_eq!(swipe(&mut r, HORIZONTAL, 0.11), Some(Gesture::SpaceRight), "just past the threshold");
    }

    #[test]
    fn a_cancelled_swipe_means_nothing() {
        let mut r = GestureReader::new(26);
        r.feed(&dock(BEGAN, HORIZONTAL, 0.0, 0.0));
        r.feed(&dock(CHANGED, HORIZONTAL, 0.9, 0.0));
        assert_eq!(r.feed(&dock(CANCELLED, HORIZONTAL, 0.9, 0.0)), None);
        assert_eq!(r.feed(&dock(ENDED, HORIZONTAL, 0.9, 0.0)), None, "the end of a cancelled swipe");
    }

    #[test]
    fn a_flick_with_no_progress_goes_by_its_speed() {
        let mut r = GestureReader::new(26);
        r.feed(&dock(BEGAN, HORIZONTAL, 0.0, 0.0));
        assert_eq!(r.feed(&dock(ENDED, HORIZONTAL, 0.0, 400.0)), Some(Gesture::SpaceRight));
        r.feed(&dock(BEGAN, HORIZONTAL, 0.0, 0.0));
        assert_eq!(r.feed(&dock(ENDED, HORIZONTAL, 0.0, -400.0)), Some(Gesture::SpaceLeft));
        // A swipe that moved and came back is not a flick: its speed on the way back does not count.
        r.feed(&dock(BEGAN, HORIZONTAL, 0.0, 0.0));
        r.feed(&dock(CHANGED, HORIZONTAL, 0.4, 0.0));
        r.feed(&dock(CHANGED, HORIZONTAL, 0.02, 0.0));
        assert_eq!(r.feed(&dock(ENDED, HORIZONTAL, 0.0, -300.0)), None);
    }

    #[test]
    fn the_end_takes_the_last_progress_when_it_carries_none() {
        let mut r = GestureReader::new(26);
        r.feed(&dock(BEGAN, VERTICAL, 0.0, 0.0));
        r.feed(&dock(CHANGED, VERTICAL, -0.7, 0.0));
        assert_eq!(r.feed(&dock(ENDED, 0, 0.0, 0.0)), Some(Gesture::Overview), "motion from the start");
    }

    fn zoom(phase: i64, m: f64) -> Fields {
        Fields { ty: 29, hid_type: HID_ZOOM, phase, zoom: m, ..Fields::default() }
    }

    #[test]
    fn a_pinch_adds_up_into_steps() {
        let mut r = GestureReader::new(26);
        assert_eq!(r.feed(&zoom(BEGAN, 0.0)), None);
        assert_eq!(r.feed(&zoom(CHANGED, 0.1)), None);
        assert_eq!(r.feed(&zoom(CHANGED, 0.1)), Some(Gesture::Zoom(1)));
        assert_eq!(r.feed(&zoom(CHANGED, 0.31)), Some(Gesture::Zoom(2)), "0.05 carried over");
        assert_eq!(r.feed(&zoom(CHANGED, -0.56)), Some(Gesture::Zoom(-3)), "0.06 - 0.56 = -0.5: three whole steps out");
        assert_eq!(r.feed(&zoom(ENDED, 0.0)), None);
        // What was left over does not carry into the next pinch.
        assert_eq!(r.feed(&zoom(BEGAN, 0.0)), None);
        assert_eq!(r.feed(&zoom(CHANGED, 0.14)), None);
        assert_eq!(r.feed(&zoom(CHANGED, 50.0)), Some(Gesture::Zoom(MAX_ZOOM_STEPS)), "bounded");
        assert_eq!(r.feed(&zoom(CHANGED, f64::NAN)), None, "NaN adds nothing, and what the bound cut off is gone");
        // Each step leaves less than one step behind, so the sum stays finite: never Zoom(0) from infinity.
        assert_eq!(r.feed(&zoom(CHANGED, f64::MAX)), Some(Gesture::Zoom(MAX_ZOOM_STEPS)));
        assert_eq!(r.feed(&zoom(CHANGED, f64::MAX)), Some(Gesture::Zoom(MAX_ZOOM_STEPS)));
    }

    #[test]
    fn what_is_left_of_a_step_counts_towards_the_next() {
        let mut r = GestureReader::new(26);
        r.feed(&zoom(BEGAN, 0.0));
        assert_eq!(r.feed(&zoom(CHANGED, 0.1)), None);
        assert_eq!(r.feed(&zoom(CHANGED, 0.1)), Some(Gesture::Zoom(1)), "0.2: one step, 0.05 left");
        assert_eq!(r.feed(&zoom(CHANGED, 0.1)), Some(Gesture::Zoom(1)), "0.05 + 0.1: a step only with the carry");
        assert_eq!(r.feed(&zoom(CHANGED, -0.14)), None, "0 - 0.14: not yet a step out");
    }

    #[test]
    fn smart_zoom_by_either_marker() {
        let mut r = GestureReader::new(26);
        assert_eq!(r.feed(&Fields { ty: TY_SMART_MAGNIFY, ..Fields::default() }), Some(Gesture::SmartZoom));
        assert_eq!(r.feed(&Fields { ty: 29, hid_type: HID_ZOOM_TOGGLE, ..Fields::default() }), Some(Gesture::SmartZoom));
    }

    #[test]
    fn other_gesture_events_mean_nothing() {
        let mut r = GestureReader::new(26);
        for ty in [18, 19, 20, 29, 31] {
            assert_eq!(r.feed(&Fields { ty, hid_type: 5, phase: ENDED, progress: 1.0, ..Fields::default() }), None, "type {ty}");
        }
    }
}
