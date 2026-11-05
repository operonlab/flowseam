//! Switch policy: when the controlling side hands the cursor to the other
//! machine. Pure logic. Time is injected by the caller (`now_ms`); nothing in
//! this module reads a clock.
//!
//! Two independent triggers, OR-ed together:
//! - (a) dwell: the cursor stays inside the boundary zone for `dwell_ms` without
//!   leaving it. Touching the edge does nothing on its own.
//! - (b) chord: the `chord` key combination is pressed down.
//!
//! After a switch the policy is disarmed. It re-arms only once the cursor has
//! left the boundary zone and moved at least `arm_dist` px from the point where
//! the switch happened. This keeps trackpad jitter from switching back and forth.
//! The chord ignores the arm state: it is an explicit user action, not a
//! boundary touch, so it cannot jitter.

use crate::proto::{ModifierSet, Point};

pub const DEFAULT_DWELL_MS: u64 = 200;
pub const DEFAULT_ARM_DIST: i32 = 4;
/// HID usage for the `S` key.
pub const HID_S: u16 = 0x16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChordSpec {
    /// Modifier bits that must all be held (extra modifiers do not block it).
    pub mods: ModifierSet,
    pub hid: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwitchPolicy {
    pub dwell_ms: u64,
    pub chord: ChordSpec,
    pub arm_dist: i32,
}

impl Default for SwitchPolicy {
    /// dwell 200 ms; chord Ctrl+Opt+Cmd+S; arm distance 4 px.
    fn default() -> Self {
        Self {
            dwell_ms: DEFAULT_DWELL_MS,
            chord: ChordSpec {
                mods: ModifierSet(ModifierSet::CTRL | ModifierSet::ALT | ModifierSet::GUI),
                hid: HID_S,
            },
            arm_dist: DEFAULT_ARM_DIST,
        }
    }
}

impl SwitchPolicy {
    /// Build the production policy from the user's config. A bad chord is an error, not a default.
    pub fn from_config(sw: &crate::config::Switch) -> Result<Self, crate::chord::ChordError> {
        Ok(Self { dwell_ms: sw.dwell_ms, chord: crate::chord::parse(&sw.chord)?, arm_dist: sw.arm_dist })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchEvent {
    /// Cursor sample. `in_boundary` is whether it currently sits in the boundary zone.
    Cursor { pos: Point, in_boundary: bool },
    /// Key transition with the modifiers held at that moment.
    Key { hid: u16, down: bool, mods: ModifierSet },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwitchState {
    pub armed: bool,
    /// Time (ms) the cursor entered the boundary zone; `None` when not dwelling.
    pub dwell_start: Option<u64>,
    /// Last cursor position seen. Used as the anchor when a chord switches.
    pub last_pos: Point,
    /// Point where the last switch happened.
    pub anchor: Option<Point>,
}

impl SwitchState {
    pub fn new(start: Point) -> Self {
        Self { armed: true, dwell_start: None, last_pos: start, anchor: None }
    }
}

/// Returns true when the cursor should be handed to the other machine now.
/// Updates `state`; the caller passes the same state back on every event.
pub fn decide(policy: &SwitchPolicy, state: &mut SwitchState, event: SwitchEvent, now_ms: u64) -> bool {
    match event {
        SwitchEvent::Key { hid, down, mods } => {
            let fire = down && hid == policy.chord.hid && mods.contains(policy.chord.mods.0);
            if fire {
                commit_switch(state);
            }
            fire
        }
        SwitchEvent::Cursor { pos, in_boundary } => {
            state.last_pos = pos;
            if !state.armed {
                // Rearm only after leaving the zone and moving arm_dist from the switch point.
                let moved = state.anchor.is_none_or(|a| chebyshev(pos, a) >= policy.arm_dist as i64);
                if in_boundary || !moved {
                    state.dwell_start = None;
                    return false;
                }
                state.armed = true;
            }
            if !in_boundary {
                // Leaving the zone resets the dwell timer.
                state.dwell_start = None;
                return false;
            }
            let start = *state.dwell_start.get_or_insert(now_ms);
            if now_ms.saturating_sub(start) >= policy.dwell_ms {
                commit_switch(state);
                true
            } else {
                false
            }
        }
    }
}

fn commit_switch(state: &mut SwitchState) {
    state.armed = false;
    state.dwell_start = None;
    state.anchor = Some(state.last_pos);
}

fn chebyshev(a: Point, b: Point) -> i64 {
    let dx = (a.x as i64 - b.x as i64).abs();
    let dy = (a.y as i64 - b.y as i64).abs();
    dx.max(dy)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL: u8 = ModifierSet::CTRL;
    const ALT: u8 = ModifierSet::ALT;
    const GUI: u8 = ModifierSet::GUI;

    fn p(x: i32, y: i32) -> Point {
        Point { x, y }
    }

    fn at_edge(pos: Point) -> SwitchEvent {
        SwitchEvent::Cursor { pos, in_boundary: true }
    }

    fn away(pos: Point) -> SwitchEvent {
        SwitchEvent::Cursor { pos, in_boundary: false }
    }

    fn chord(mods: u8) -> SwitchEvent {
        SwitchEvent::Key { hid: HID_S, down: true, mods: ModifierSet(mods) }
    }

    fn setup() -> (SwitchPolicy, SwitchState) {
        (SwitchPolicy::default(), SwitchState::new(p(500, 500)))
    }

    #[test]
    fn touching_the_edge_alone_does_not_switch() {
        let (pol, mut st) = setup();
        // Entering the boundary with no elapsed dwell must not switch.
        assert!(!decide(&pol, &mut st, at_edge(p(0, 10)), 0));
    }

    #[test]
    fn dwell_below_threshold_does_not_switch() {
        let (pol, mut st) = setup();
        assert!(!decide(&pol, &mut st, at_edge(p(0, 10)), 0));
        assert!(!decide(&pol, &mut st, at_edge(p(0, 10)), 199));
    }

    #[test]
    fn dwell_reaching_threshold_switches() {
        let (pol, mut st) = setup();
        assert!(!decide(&pol, &mut st, at_edge(p(0, 10)), 0));
        assert!(decide(&pol, &mut st, at_edge(p(0, 10)), 200));
    }

    #[test]
    fn leaving_boundary_mid_dwell_resets_the_timer() {
        let (pol, mut st) = setup();
        assert!(!decide(&pol, &mut st, at_edge(p(0, 10)), 0));
        assert!(!decide(&pol, &mut st, away(p(20, 10)), 100));
        // Re-enter at t=150. Only 150 ms of dwell by t=300, so no switch yet.
        assert!(!decide(&pol, &mut st, at_edge(p(0, 10)), 150));
        assert!(!decide(&pol, &mut st, at_edge(p(0, 10)), 300));
        // Full 200 ms since re-entry (t=350) switches.
        assert!(decide(&pol, &mut st, at_edge(p(0, 10)), 350));
    }

    #[test]
    fn chord_alone_switches() {
        let (pol, mut st) = setup();
        assert!(decide(&pol, &mut st, chord(CTRL | ALT | GUI), 0));
    }

    #[test]
    fn chord_missing_a_modifier_does_not_switch() {
        let (pol, mut st) = setup();
        assert!(!decide(&pol, &mut st, chord(CTRL | ALT), 0));
        assert!(!decide(&pol, &mut st, chord(CTRL | GUI), 0));
        assert!(!decide(&pol, &mut st, chord(ALT | GUI), 0));
    }

    #[test]
    fn chord_key_up_does_not_switch() {
        let (pol, mut st) = setup();
        let up = SwitchEvent::Key { hid: HID_S, down: false, mods: ModifierSet(CTRL | ALT | GUI) };
        assert!(!decide(&pol, &mut st, up, 0));
    }

    #[test]
    fn after_switch_cursor_must_leave_and_move_arm_dist_to_rearm() {
        let (pol, mut st) = setup();
        // Switch via chord with the cursor at (0,10); that point becomes the anchor.
        st.last_pos = p(0, 10);
        assert!(decide(&pol, &mut st, chord(CTRL | ALT | GUI), 0));
        // Still inside the boundary: disarmed, so dwell alone cannot switch back.
        assert!(!decide(&pol, &mut st, at_edge(p(0, 10)), 1000));
        assert!(!decide(&pol, &mut st, at_edge(p(0, 10)), 2000));
        // Leaves the zone but moves only 3 px from the anchor: still disarmed.
        assert!(!decide(&pol, &mut st, away(p(3, 10)), 2100));
        assert!(!decide(&pol, &mut st, at_edge(p(0, 10)), 2300));
        // Short of arm_dist the policy is still disarmed: a full dwell after re-entry must not fire.
        assert!(!decide(&pol, &mut st, at_edge(p(0, 10)), 2600));
    }

    #[test]
    fn rearms_after_leaving_and_moving_arm_dist() {
        let (pol, mut st) = setup();
        st.last_pos = p(0, 10);
        assert!(decide(&pol, &mut st, chord(CTRL | ALT | GUI), 0));
        // Leaves the zone and moves 4 px from the anchor: rearmed.
        assert!(!decide(&pol, &mut st, away(p(4, 10)), 100));
        // Back in the zone: dwell now counts from re-entry (t=200).
        assert!(!decide(&pol, &mut st, at_edge(p(0, 10)), 200));
        assert!(decide(&pol, &mut st, at_edge(p(0, 10)), 400));
    }

    #[test]
    fn from_config_carries_every_field_and_errors_on_bad_chord() {
        let sw = crate::config::Switch { dwell_ms: 350, arm_dist: 9, chord: "ctrl+shift+x".into(), boundary_px: 3 };
        let p = SwitchPolicy::from_config(&sw).unwrap();
        assert_eq!((p.dwell_ms, p.arm_dist), (350, 9));
        assert_eq!(p.chord.hid, 0x1B);
        assert_eq!(p.chord.mods.0, ModifierSet::CTRL | ModifierSet::SHIFT);
        let bad = crate::config::Switch { chord: "nope".into(), ..sw };
        assert!(SwitchPolicy::from_config(&bad).is_err());
    }

    #[test]
    fn moving_far_inside_a_wide_zone_does_not_rearm() {
        let (pol, mut st) = setup();
        st.last_pos = p(0, 10);
        assert!(decide(&pol, &mut st, chord(CTRL | ALT | GUI), 0));
        // Still in the zone, 10 px from the anchor: leaving the zone is what re-arms, not distance alone.
        assert!(!decide(&pol, &mut st, at_edge(p(10, 10)), 1000));
        assert!(!decide(&pol, &mut st, at_edge(p(10, 10)), 1300));
    }
}
