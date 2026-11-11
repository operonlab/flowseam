//! The settings' monitor switch for one peer: adding its monitors and their inputs, and what a person's answer
//! after a test does to the config. No egui here, so it is tested without a window.
//!
//! A monitor switch is confirmed only by a person: DDC tells which input a monitor shows, never which
//! computer is plugged into it. The test shows the candidate input for a few seconds; the person says what
//! they saw.

use flowseam_core::config::DisplaySwitch;
use flowseam_core::status::{InspectStatus, TrialMonitor, TrialStatus};

/// What the person saw on the monitor while the test showed the candidate input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    ThePeer,
    AnotherComputer,
    Nothing,
}

/// What an answer did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Confirmed in the form; the window saves it right away.
    Confirmed,
    /// Not confirmed; the person should try another input.
    WrongComputer,
    NoSignal,
    /// The monitor never showed the input, so there was nothing to judge.
    DidNotSwitch,
}

/// Adds a monitor to the peer's switch, unconfirmed; its inputs are filled in once it is inspected.
pub fn add_monitor(displays: &mut Vec<DisplaySwitch>, uuid: &str) {
    if !displays.iter().any(|d| d.uuid == uuid) {
        displays.push(DisplaySwitch { uuid: uuid.into(), input: 0, home: 0, verified: false });
    }
}

/// Fills in what an inspect of a monitor read, where nothing is chosen yet: home is the input showing now
/// (this Mac: the person is looking at it to use these settings), the peer's input the first other one the
/// monitor lists. A current input that could not be read leaves home for the person to pick.
pub fn apply_inspect(displays: &mut [DisplaySwitch], inspect: &InspectStatus) {
    let Some(d) = displays.iter_mut().find(|d| d.uuid == inspect.uuid) else { return };
    if d.home == 0 {
        if let Some(current) = inspect.current {
            d.home = current;
        }
    }
    if d.input == 0 && d.home != 0 {
        if let Some(first) = peer_inputs(inspect, d.home).first() {
            d.input = *first;
            d.verified = false;
        }
    }
}

/// The inputs the peer can be put on: what the monitor lists, minus the one showing this Mac.
pub fn peer_inputs(inspect: &InspectStatus, home: u16) -> Vec<u16> {
    inspect.inputs.iter().flatten().copied().filter(|v| *v != home).collect()
}

/// Another input is a new claim about the cabling: it is unconfirmed until tested.
pub fn choose_input(display: &mut DisplaySwitch, input: u16) {
    if display.input != input {
        display.input = input;
        display.verified = false;
    }
}

/// This Mac's input, picked by hand when the monitor could not say which it shows. Picking the peer's
/// input clears that one, since a monitor cannot show both computers on one input.
pub fn choose_home(display: &mut DisplaySwitch, home: u16) {
    if display.home != home {
        display.home = home;
        display.verified = false;
        if display.input == home {
            display.input = 0;
        }
    }
}

/// Both inputs are known and differ: a test can show the peer's input and come back to this Mac's.
pub fn testable(d: &DisplaySwitch) -> bool {
    d.input != 0 && d.home != 0 && d.input != d.home
}

/// What "test all" shows together: the unconfirmed monitors that can be tested, or, once all are
/// confirmed, every one that can (a retest).
pub fn to_test(displays: &[DisplaySwitch]) -> Vec<DisplaySwitch> {
    let ready: Vec<DisplaySwitch> = displays.iter().filter(|d| testable(d)).cloned().collect();
    let unconfirmed: Vec<DisplaySwitch> = ready.iter().filter(|d| !d.verified).cloned().collect();
    if unconfirmed.is_empty() { ready } else { unconfirmed }
}

/// The worker's command for a test of `monitors`: `test <token> <uuid>:<input>:<home>[,…] <peer>`, the
/// peer last since a name may hold spaces.
pub fn test_line(token: u64, peer: &str, monitors: &[DisplaySwitch]) -> String {
    let list: Vec<String> = monitors.iter().map(|d| format!("{}:{}:{}", d.uuid, d.input, d.home)).collect();
    format!("test {token} {} {peer}", list.join(","))
}

/// The person's answer about one monitor of `trial`. Only "I saw that peer" after a test that showed the
/// input confirms, and it confirms exactly what was tested, whatever the form says by now. Any other answer
/// about a configured monitor and input takes back an earlier confirmation: the cabling is not what it was.
/// A test that never showed the inputs changes nothing: monitors switch together or not at all, so one that
/// failed says nothing about the cabling of the others.
pub fn answer(displays: &mut Vec<DisplaySwitch>, trial: &TrialStatus, monitor: &TrialMonitor, a: Answer) -> Outcome {
    if !trial.ok {
        return Outcome::DidNotSwitch;
    }
    if a == Answer::ThePeer {
        let tested = DisplaySwitch { uuid: monitor.uuid.clone(), input: monitor.input, home: monitor.home, verified: true };
        match displays.iter_mut().find(|d| d.uuid == monitor.uuid) {
            Some(d) => *d = tested,
            None => displays.push(tested),
        }
        return Outcome::Confirmed;
    }
    if let Some(d) = displays.iter_mut().find(|d| d.uuid == monitor.uuid && d.input == monitor.input) {
        d.verified = false;
    }
    match a {
        Answer::Nothing => Outcome::NoSignal,
        _ => Outcome::WrongComputer,
    }
}

/// What became of a test's answers on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Saved {
    /// They changed nothing (a retest of confirmed monitors): nothing to write.
    Unchanged,
    Yes,
    Failed(String),
}

/// The line shown under a monitor after its answer. `input` names the input tested; `reason` is why the
/// test failed, if it did.
pub fn note(outcome: Outcome, saved: &Saved, peer: &str, input: &str, reason: &str) -> String {
    let (tr, trf) = (crate::i18n::tr, crate::i18n::trf);
    match (outcome, saved) {
        (Outcome::Confirmed, Saved::Yes) => tr("已確認並儲存。").to_string(),
        (Outcome::Confirmed, Saved::Unchanged) => tr("已確認。").to_string(),
        (Outcome::Confirmed, Saved::Failed(e)) => trf("已確認，但沒能儲存：{}", &[e]),
        (Outcome::WrongComputer, _) => trf("這個輸入接的不是 {}，換一個輸入再測試。", &[&peer]),
        (Outcome::NoSignal, _) => tr("這個輸入沒有訊號，換一個輸入再測試。").to_string(),
        (Outcome::DidNotSwitch, _) => trf("螢幕沒有切到 {}：{}", &[&input, &reason]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inspect(current: Option<u16>) -> InspectStatus {
        InspectStatus { uuid: "U".into(), inputs: Some(vec![0x0F, 0x11, 0x12]), current, reason: None }
    }
    fn trial(ok: bool) -> TrialStatus {
        TrialStatus { peer: "pc".into(), monitors: vec![TrialMonitor { uuid: "U".into(), input: 0x0F, home: 0x12 }], ok, reason: None, id: 1 }
    }
    fn on_u(d: &mut Vec<DisplaySwitch>, t: &TrialStatus, a: Answer) -> Outcome {
        answer(d, t, &t.monitors[0], a)
    }

    #[test]
    fn test_all_takes_the_unconfirmed_or_else_every_testable_monitor() {
        let d = |uuid: &str, input, verified| DisplaySwitch { uuid: uuid.into(), input, home: 0x12, verified };
        let uuids = |v: Vec<DisplaySwitch>| v.into_iter().map(|d| d.uuid).collect::<Vec<_>>();
        assert_eq!(uuids(to_test(&[d("U", 0x0F, true), d("V", 0x0F, false), d("W", 0, false)])), ["V"], "W has no input yet");
        assert_eq!(uuids(to_test(&[d("U", 0x0F, true), d("V", 0x11, true)])), ["U", "V"], "all confirmed: a retest of all");
        assert!(to_test(&[d("W", 0, false)]).is_empty());
    }

    #[test]
    fn a_test_line_lists_every_monitor_and_ends_with_the_peer() {
        let ms = [DisplaySwitch { uuid: "U-1".into(), input: 15, home: 18, verified: false }, DisplaySwitch { uuid: "V-2".into(), input: 17, home: 19, verified: true }];
        // The same line the worker's parser is tested with (flowseam-mac worker.rs, `menu_command`).
        assert_eq!(test_line(77, "my pc", &ms[..1]), "test 77 U-1:15:18 my pc");
        assert_eq!(test_line(77, "pc", &ms), "test 77 U-1:15:18,V-2:17:19 pc");
    }

    #[test]
    fn a_test_that_did_not_switch_leaves_every_confirmation_alone() {
        let t = TrialStatus { monitors: vec![TrialMonitor { uuid: "U".into(), input: 0x0F, home: 0x12 }, TrialMonitor { uuid: "V".into(), input: 0x11, home: 0x13 }], ..trial(false) };
        let mut d = vec![DisplaySwitch { uuid: "U".into(), input: 0x0F, home: 0x12, verified: true }, DisplaySwitch { uuid: "V".into(), input: 0x11, home: 0x13, verified: true }];
        let before = d.clone();
        for m in &t.monitors {
            for a in [Answer::Nothing, Answer::AnotherComputer, Answer::ThePeer] {
                assert_eq!(answer(&mut d, &t, m, a), Outcome::DidNotSwitch);
            }
        }
        assert_eq!(d, before, "one monitor failing says nothing about the others' cabling");
    }

    #[test]
    fn a_retest_that_changes_nothing_reads_as_confirmed_not_as_a_failed_save() {
        let confirmed = note(Outcome::Confirmed, &Saved::Unchanged, "pc", "HDMI 1", "");
        let failed = note(Outcome::Confirmed, &Saved::Failed(String::new()), "pc", "HDMI 1", "");
        assert!(!confirmed.starts_with(&failed), "nothing failed: {confirmed}");
        assert_ne!(confirmed, note(Outcome::Confirmed, &Saved::Yes, "pc", "HDMI 1", ""), "and nothing was written");
        assert!(note(Outcome::Confirmed, &Saved::Failed("disk full".into()), "pc", "HDMI 1", "").contains("disk full"), "a failed save says why");
    }

    #[test]
    fn each_monitor_of_a_test_takes_its_own_answer() {
        let t = TrialStatus { monitors: vec![TrialMonitor { uuid: "U".into(), input: 0x0F, home: 0x12 }, TrialMonitor { uuid: "V".into(), input: 0x11, home: 0x13 }], ..trial(true) };
        let mut d = vec![DisplaySwitch { uuid: "U".into(), input: 0x0F, home: 0x12, verified: false }, DisplaySwitch { uuid: "V".into(), input: 0x11, home: 0x13, verified: true }];
        assert_eq!(answer(&mut d, &t, &t.monitors[0], Answer::ThePeer), Outcome::Confirmed);
        assert_eq!(answer(&mut d, &t, &t.monitors[1], Answer::Nothing), Outcome::NoSignal);
        assert_eq!((d[0].verified, d[1].verified), (true, false));
    }

    #[test]
    fn an_added_monitor_takes_this_macs_input_as_home_and_offers_the_others() {
        let mut d = vec![];
        add_monitor(&mut d, "U");
        add_monitor(&mut d, "U");
        assert_eq!(d.len(), 1, "added once");
        apply_inspect(&mut d, &inspect(Some(0x12)));
        assert_eq!(d, [DisplaySwitch { uuid: "U".into(), input: 0x0F, home: 0x12, verified: false }]);
        assert_eq!(peer_inputs(&inspect(Some(0x12)), 0x12), [0x0F, 0x11]);
        apply_inspect(&mut d, &InspectStatus { uuid: "other".into(), ..inspect(Some(0x11)) });
        assert_eq!(d[0].home, 0x12, "another monitor's inspect changes nothing");
    }

    #[test]
    fn each_monitor_takes_its_own_inspect() {
        let mut d = vec![];
        add_monitor(&mut d, "U");
        add_monitor(&mut d, "V");
        apply_inspect(&mut d, &InspectStatus { uuid: "V".into(), inputs: Some(vec![0x11, 0x13]), current: Some(0x13), reason: None });
        assert_eq!((d[0].input, d[0].home), (0, 0));
        assert_eq!((d[1].input, d[1].home), (0x11, 0x13));
    }

    /// A monitor whose current input could not be read once was saved with home 0 and never asked again.
    #[test]
    fn a_monitor_whose_current_input_is_unknown_gets_no_guess_and_cannot_be_tested_until_home_is_picked() {
        let mut d = vec![];
        add_monitor(&mut d, "U");
        apply_inspect(&mut d, &inspect(None));
        assert_eq!((d[0].input, d[0].home), (0, 0), "no peer input either: it would be guessed against an unknown home");
        assert!(!testable(&d[0]));
        choose_home(&mut d[0], 0x12);
        apply_inspect(&mut d, &inspect(None));
        assert_eq!(d[0].input, 0x0F, "the next inspect fills the peer's input against the picked home");
        assert!(testable(&d[0]));
    }

    #[test]
    fn picking_the_peers_input_as_home_clears_the_peers_input() {
        let mut d = DisplaySwitch { uuid: "U".into(), input: 0x0F, home: 0x12, verified: true };
        choose_home(&mut d, 0x0F);
        assert_eq!((d.input, d.home, d.verified), (0, 0x0F, false));
        assert!(!testable(&d));
    }

    #[test]
    fn only_seeing_the_peer_after_a_test_that_switched_confirms_and_it_confirms_what_was_tested() {
        let mut d = vec![DisplaySwitch { uuid: "U".into(), input: 0x11, home: 0x12, verified: false }, DisplaySwitch { uuid: "V".into(), input: 0x11, home: 0x13, verified: false }];
        assert_eq!(on_u(&mut d, &trial(true), Answer::ThePeer), Outcome::Confirmed);
        assert_eq!(d[0], DisplaySwitch { uuid: "U".into(), input: 0x0F, home: 0x12, verified: true });
        assert!(!d[1].verified, "the other monitor is untouched");
        let mut d = vec![];
        assert_eq!(on_u(&mut d, &trial(false), Answer::ThePeer), Outcome::DidNotSwitch);
        assert!(d.is_empty());
    }

    #[test]
    fn any_other_answer_takes_back_a_confirmation_of_that_input() {
        for (a, want) in [(Answer::AnotherComputer, Outcome::WrongComputer), (Answer::Nothing, Outcome::NoSignal)] {
            let mut d = vec![DisplaySwitch { uuid: "U".into(), input: 0x0F, home: 0x12, verified: true }];
            assert_eq!(on_u(&mut d, &trial(true), a), want);
            assert!(!d[0].verified);
        }
    }

    #[test]
    fn another_input_is_unconfirmed() {
        let mut d = DisplaySwitch { uuid: "U".into(), input: 0x0F, home: 0x12, verified: true };
        choose_input(&mut d, 0x0F);
        assert!(d.verified, "the same input stays confirmed");
        choose_input(&mut d, 0x11);
        assert!(!d.verified);
    }
}
