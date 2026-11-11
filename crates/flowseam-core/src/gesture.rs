//! A trackpad gesture as the shortcut that does the same thing on a machine that cannot take the gesture
//! itself: a PC, or a Mac speaking protocol 4. Mac shortcuts are the macOS defaults (Keyboard Shortcuts >
//! Mission Control); PC ones are what Windows does for the same move on its own touchpads.
//!
//! | Gesture                    | Mac            | PC                            |
//! |----------------------------|----------------|-------------------------------|
//! | pinch                      | ⌘= / ⌘− a step | Ctrl + one wheel notch a step |
//! | smart zoom                 | ⌘0             | Ctrl+0                        |
//! | space ← →                  | ⌃← ⌃→          | Ctrl+Win+← →                  |
//! | swipe up                   | ⌃↑             | Win+Tab                       |
//! | swipe down                 | ⌃↓             | Win+Tab, or Win+D (Windows habit) |
//! | thumb and three spread     | F11            | Win+D                         |
//! | thumb and three pinched    | ⌘Space         | Win (Start)                   |
//!
//! A PC follows the Mac's habit or Windows'. They differ only in swiping down: Windows has no App Exposé,
//! so the Mac habit shows all windows in Task View, while a Windows touchpad shows the desktop. Sideways
//! swipes switch desktops in both, since the Mac reports a swipe's direction and not its finger count.
//!
//! As on the trackpad, the opposite gesture closes what one opened: swipe down after swipe up leaves
//! Mission Control (Esc), pinching after spreading brings the windows back (Show Desktop again).

use crate::proto::Gesture;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Mac,
    Pc(Habit),
}

/// Whose touchpad habit a PC's gestures follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Habit {
    Mac,
    Windows,
}

impl Platform {
    /// This machine.
    pub fn here() -> Self {
        if cfg!(windows) {
            Platform::Pc(Habit::Mac)
        } else {
            Platform::Mac
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stroke {
    /// HID usage, down or up.
    Key(u16, bool),
    /// Vertical wheel notches, positive away from the user.
    Wheel(i32),
}

const LCTRL: u16 = 0xE0;
const LGUI: u16 = 0xE3;
const D: u16 = 0x07;
const ZERO: u16 = 0x27;
const ESC: u16 = 0x29;
const TAB: u16 = 0x2B;
const SPACE: u16 = 0x2C;
const MINUS: u16 = 0x2D;
const EQUAL: u16 = 0x2E;
const F11: u16 = 0x44;
const RIGHT: u16 = 0x4F;
const LEFT: u16 = 0x50;
const DOWN: u16 = 0x51;
const UP: u16 = 0x52;

/// Keys pressed in order, released in reverse.
fn chord(keys: &[u16]) -> Vec<Stroke> {
    keys.iter().map(|&k| Stroke::Key(k, true)).chain(keys.iter().rev().map(|&k| Stroke::Key(k, false))).collect()
}

/// One session's gestures as shortcuts. Remembers what the last one opened, so its opposite closes it.
#[derive(Debug, Default)]
pub struct GestureKeys {
    shown: Option<Gesture>,
}

impl GestureKeys {
    pub fn new() -> Self {
        Self::default()
    }

    /// A key or click went to the same machine: what a gesture opened may be closed, picked from or typed
    /// into by now.
    pub fn other_input(&mut self) {
        self.shown = None;
    }

    pub fn strokes(&mut self, g: Gesture, to: Platform) -> Vec<Stroke> {
        if let Some(shown) = self.shown {
            // With the Windows habit, swiping down and spreading both show the desktop, so the opposite of
            // either brings the windows back, whichever of the two showed it.
            let undoes = opposite(g) == Some(shown)
                || (shows_desktop(shown, to) && opposite(g).is_some_and(|o| shows_desktop(o, to)));
            if undoes {
                self.shown = None;
                // Show Desktop is undone by its own shortcut; overviews and launchers close on Esc.
                return if shows_desktop(shown, to) { shortcut(shown, to) } else { chord(&[ESC]) };
            }
            if shortcut(shown, to) == shortcut(g, to) {
                // Already showing: these shortcuts toggle, so sending it again would close it. With the
                // Windows habit, swiping down and spreading both show the desktop.
                return Vec::new();
            }
        }
        if opposite(g).is_some() {
            self.shown = Some(g);
        }
        shortcut(g, to)
    }
}

fn shows_desktop(g: Gesture, to: Platform) -> bool {
    g == Gesture::ShowDesktop || (g == Gesture::AppWindows && to == Platform::Pc(Habit::Windows))
}

/// The gesture with the opposite motion, for the ones that open something.
fn opposite(g: Gesture) -> Option<Gesture> {
    match g {
        Gesture::Overview => Some(Gesture::AppWindows),
        Gesture::AppWindows => Some(Gesture::Overview),
        Gesture::ShowDesktop => Some(Gesture::Launcher),
        Gesture::Launcher => Some(Gesture::ShowDesktop),
        _ => None,
    }
}

fn shortcut(g: Gesture, to: Platform) -> Vec<Stroke> {
    use Gesture::*;
    use Platform::*;
    match (g, to) {
        (Zoom(n), Mac) => {
            let key = if n > 0 { EQUAL } else { MINUS };
            let mut v = vec![Stroke::Key(LGUI, true)];
            for _ in 0..n.unsigned_abs() {
                v.extend([Stroke::Key(key, true), Stroke::Key(key, false)]);
            }
            v.push(Stroke::Key(LGUI, false));
            v
        }
        (Zoom(n), Pc(_)) => vec![Stroke::Key(LCTRL, true), Stroke::Wheel(n.into()), Stroke::Key(LCTRL, false)],
        (SmartZoom, Mac) => chord(&[LGUI, ZERO]),
        (SmartZoom, Pc(_)) => chord(&[LCTRL, ZERO]),
        (SpaceLeft, Mac) => chord(&[LCTRL, LEFT]),
        (SpaceRight, Mac) => chord(&[LCTRL, RIGHT]),
        (SpaceLeft, Pc(_)) => chord(&[LCTRL, LGUI, LEFT]),
        (SpaceRight, Pc(_)) => chord(&[LCTRL, LGUI, RIGHT]),
        (Overview, Mac) => chord(&[LCTRL, UP]),
        (AppWindows, Mac) => chord(&[LCTRL, DOWN]),
        // A Windows touchpad shows the desktop on a swipe down.
        (AppWindows, Pc(Habit::Windows)) => chord(&[LGUI, D]),
        // Windows has no App Exposé: Task View shows the windows, and is what its own touchpad opens.
        (Overview | AppWindows, Pc(_)) => chord(&[LGUI, TAB]),
        (ShowDesktop, Mac) => chord(&[F11]),
        (ShowDesktop, Pc(_)) => chord(&[LGUI, D]),
        // Spotlight: macOS 26 moved the app grid into it.
        (Launcher, Mac) => chord(&[LGUI, SPACE]),
        (Launcher, Pc(_)) => chord(&[LGUI]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Gesture::*;
    use Platform::*;

    /// A PC with the Mac habit, and one with the Windows habit.
    const PC: Platform = Pc(Habit::Mac);
    const WIN: Platform = Pc(Habit::Windows);
    const ALL: [Gesture; 9] = [Zoom(3), Zoom(-2), SmartZoom, SpaceLeft, SpaceRight, Overview, AppWindows, ShowDesktop, Launcher];

    fn keys(s: &[Stroke]) -> Vec<(u16, bool)> {
        s.iter().map(|s| match *s {
            Stroke::Key(k, d) => (k, d),
            Stroke::Wheel(_) => panic!("wheel in {s:?}"),
        }).collect()
    }

    #[test]
    fn each_gesture_is_its_platform_shortcut() {
        let cases: [(Gesture, Platform, &[u16]); 15] = [
            (SmartZoom, Mac, &[LGUI, ZERO]),
            (SmartZoom, PC, &[LCTRL, ZERO]),
            (SpaceLeft, Mac, &[LCTRL, LEFT]),
            (SpaceRight, Mac, &[LCTRL, RIGHT]),
            (SpaceLeft, PC, &[LCTRL, LGUI, LEFT]),
            (SpaceRight, PC, &[LCTRL, LGUI, RIGHT]),
            (Overview, Mac, &[LCTRL, UP]),
            (AppWindows, Mac, &[LCTRL, DOWN]),
            (Overview, PC, &[LGUI, TAB]),
            (AppWindows, PC, &[LGUI, TAB]),
            (AppWindows, WIN, &[LGUI, D]),
            (ShowDesktop, Mac, &[F11]),
            (ShowDesktop, PC, &[LGUI, D]),
            (Launcher, Mac, &[LGUI, SPACE]),
            (Launcher, PC, &[LGUI]),
        ];
        for (g, p, want) in cases {
            assert_eq!(keys(&GestureKeys::new().strokes(g, p)), keys(&chord(want)), "{g:?} on {p:?}");
        }
    }

    #[test]
    fn the_habits_differ_only_in_swiping_down() {
        for g in ALL {
            let (mac, win) = (GestureKeys::new().strokes(g, PC), GestureKeys::new().strokes(g, WIN));
            assert_eq!(mac == win, g != AppWindows, "{g:?}: {mac:?} / {win:?}");
        }
    }

    #[test]
    fn every_chord_releases_what_it_pressed() {
        for g in ALL {
            for p in [Mac, PC, WIN] {
                let mut held = std::collections::BTreeSet::new();
                for s in GestureKeys::new().strokes(g, p) {
                    if let Stroke::Key(k, down) = s {
                        assert!(if down { held.insert(k) } else { held.remove(&k) }, "{g:?} on {p:?}: {k:#x} {down}");
                    }
                }
                assert!(held.is_empty(), "{g:?} on {p:?} leaves {held:?} down");
            }
        }
    }

    #[test]
    fn a_pinch_is_one_zoom_shortcut_a_step() {
        let mac = GestureKeys::new().strokes(Zoom(2), Mac);
        let eq = [Stroke::Key(EQUAL, true), Stroke::Key(EQUAL, false)];
        assert_eq!(mac, [&[Stroke::Key(LGUI, true)][..], &eq, &eq, &[Stroke::Key(LGUI, false)]].concat());
        assert!(GestureKeys::new().strokes(Zoom(-1), Mac).contains(&Stroke::Key(MINUS, true)));
        assert_eq!(
            GestureKeys::new().strokes(Zoom(-3), PC),
            [Stroke::Key(LCTRL, true), Stroke::Wheel(-3), Stroke::Key(LCTRL, false)],
            "wheel towards the user zooms out"
        );
    }

    #[test]
    fn the_opposite_gesture_closes_what_one_opened() {
        for p in [Mac, PC] {
            let mut k = GestureKeys::new();
            k.strokes(Overview, p);
            assert_eq!(k.strokes(AppWindows, p), chord(&[ESC]), "down after up leaves the overview on {p:?}");
            assert_eq!(k.strokes(AppWindows, p), shortcut(AppWindows, p), "and the next down opens App Exposé");
            assert_eq!(k.strokes(Overview, p), chord(&[ESC]), "up after down leaves it");

            k.strokes(ShowDesktop, p);
            assert_eq!(k.strokes(Launcher, p), shortcut(ShowDesktop, p), "pinching brings the windows back on {p:?}");
            k.strokes(Launcher, p);
            assert_eq!(k.strokes(ShowDesktop, p), chord(&[ESC]), "spreading closes the launcher on {p:?}");
        }
    }

    #[test]
    fn with_the_windows_habit_swiping_up_brings_the_windows_back() {
        let mut k = GestureKeys::new();
        assert_eq!(k.strokes(AppWindows, WIN), chord(&[LGUI, D]));
        assert_eq!(k.strokes(Overview, WIN), chord(&[LGUI, D]), "Win+D again restores what it minimised; Esc would not");
        assert_eq!(k.strokes(Overview, WIN), chord(&[LGUI, TAB]), "the next swipe up opens Task View");
        assert_eq!(k.strokes(AppWindows, WIN), chord(&[ESC]), "and a swipe down leaves it");
    }

    #[test]
    fn with_the_windows_habit_two_ways_to_the_desktop_do_not_undo_each_other() {
        for (first, then) in [(AppWindows, ShowDesktop), (ShowDesktop, AppWindows)] {
            for back in [Overview, Launcher] {
                let mut k = GestureKeys::new();
                assert_eq!(k.strokes(first, WIN), chord(&[LGUI, D]));
                assert!(k.strokes(then, WIN).is_empty(), "{then:?} after {first:?}: Win+D twice brings the windows back");
                assert_eq!(k.strokes(back, WIN), chord(&[LGUI, D]), "{back:?} after {first:?}, {then:?} restores the windows");
                assert_eq!(k.strokes(back, WIN), shortcut(back, WIN), "and the next {back:?} opens its own view");
            }
            let mut k = GestureKeys::new();
            k.strokes(first, WIN);
            let back = if first == AppWindows { Launcher } else { Overview };
            assert_eq!(k.strokes(back, WIN), chord(&[LGUI, D]), "{back:?} restores a desktop {first:?} showed");
        }
        // With the Mac habit, Task View and the desktop are different things: a swipe up after spreading
        // opens Task View, as it does on the Mac.
        let mut k = GestureKeys::new();
        k.strokes(ShowDesktop, PC);
        assert_eq!(k.strokes(Overview, PC), chord(&[LGUI, TAB]));
        // With the Mac habit swiping down is Task View, a different thing from the desktop.
        let mut k = GestureKeys::new();
        k.strokes(ShowDesktop, PC);
        assert_eq!(k.strokes(AppWindows, PC), chord(&[LGUI, TAB]));
    }

    #[test]
    fn a_repeat_or_other_input_does_not_close() {
        let mut k = GestureKeys::new();
        k.strokes(Overview, PC);
        assert!(k.strokes(Overview, PC).is_empty(), "a second swipe up would toggle Task View shut");
        k.strokes(SpaceRight, PC);
        assert_eq!(k.strokes(AppWindows, PC), chord(&[ESC]), "moving desktops inside Task View keeps it open");

        k.strokes(Overview, Mac);
        k.other_input();
        assert_eq!(k.strokes(AppWindows, Mac), shortcut(AppWindows, Mac), "a click in Mission Control closed it");
    }
}
