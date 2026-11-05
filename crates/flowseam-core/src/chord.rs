//! Switch chord text (`ctrl+alt+cmd+s`) to a `ChordSpec`. Shared by the GUI (validate on save)
//! and the worker (build the policy), so both reject the same strings.

use crate::proto::ModifierSet;
use crate::switch::ChordSpec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChordError {
    Empty,
    UnknownToken(String),
    /// No modifier: a bare key would fire while typing.
    NoModifier,
    NoKey,
    TwoKeys(String, String),
}

impl std::fmt::Display for ChordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChordError::Empty => write!(f, "chord is empty"),
            ChordError::UnknownToken(t) => write!(f, "chord token {t:?} not recognised"),
            ChordError::NoModifier => write!(f, "chord needs at least one modifier"),
            ChordError::NoKey => write!(f, "chord needs one non-modifier key"),
            ChordError::TwoKeys(a, b) => write!(f, "chord has two keys: {a:?} and {b:?}"),
        }
    }
}

fn key_hid(tok: &str) -> Option<u16> {
    let b = tok.as_bytes();
    match tok {
        "space" => return Some(0x2C),
        "return" | "enter" => return Some(0x28),
        "tab" => return Some(0x2B),
        _ => {}
    }
    if b.len() == 1 {
        return match b[0] {
            c @ b'a'..=b'z' => Some(0x04 + (c - b'a') as u16),
            c @ b'1'..=b'9' => Some(0x1E + (c - b'1') as u16),
            b'0' => Some(0x27),
            _ => None,
        };
    }
    let n: u16 = tok.strip_prefix('f')?.parse().ok()?;
    (1..=12).contains(&n).then(|| 0x3A + n - 1)
}

/// Parse `ctrl+alt+cmd+s` (case-insensitive, `+`-separated). Errors rather than defaulting.
pub fn parse(s: &str) -> Result<ChordSpec, ChordError> {
    if s.trim().is_empty() {
        return Err(ChordError::Empty);
    }
    let mut mods = 0u8;
    let mut key: Option<(String, u16)> = None;
    for raw in s.split('+') {
        let tok = raw.trim().to_ascii_lowercase();
        let bit = match tok.as_str() {
            "ctrl" | "control" => Some(ModifierSet::CTRL),
            "alt" | "opt" | "option" => Some(ModifierSet::ALT),
            "cmd" | "command" | "gui" | "win" | "super" => Some(ModifierSet::GUI),
            "shift" => Some(ModifierSet::SHIFT),
            _ => None,
        };
        if let Some(bit) = bit {
            mods |= bit;
            continue;
        }
        let hid = key_hid(&tok).ok_or_else(|| ChordError::UnknownToken(tok.clone()))?;
        if let Some((prev, _)) = &key {
            return Err(ChordError::TwoKeys(prev.clone(), tok));
        }
        key = Some((tok, hid));
    }
    let (_, hid) = key.ok_or(ChordError::NoKey)?;
    if mods == 0 {
        return Err(ChordError::NoModifier);
    }
    Ok(ChordSpec { mods: ModifierSet(mods), hid })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_chord_default_matches_policy_default() {
        assert_eq!(parse("ctrl+alt+cmd+s").unwrap(), crate::switch::SwitchPolicy::default().chord);
        assert_eq!(parse(" Ctrl + Option + Command + S ").unwrap(), crate::switch::SwitchPolicy::default().chord);
    }

    #[test]
    fn parse_chord_keys_and_modifiers() {
        let c = parse("shift+f12").unwrap();
        assert_eq!((c.mods.0, c.hid), (ModifierSet::SHIFT, 0x45));
        assert_eq!(parse("cmd+1").unwrap().hid, 0x1E);
        assert_eq!(parse("cmd+0").unwrap().hid, 0x27);
        assert_eq!(parse("ctrl+space").unwrap().hid, 0x2C);
        assert_eq!(parse("ctrl+z").unwrap().hid, 0x1D);
    }

    #[test]
    fn parse_chord_rejects_bad_input() {
        assert_eq!(parse(""), Err(ChordError::Empty));
        assert_eq!(parse("s"), Err(ChordError::NoModifier));
        assert_eq!(parse("ctrl+alt"), Err(ChordError::NoKey));
        assert!(matches!(parse("ctrl+f13"), Err(ChordError::UnknownToken(_))));
        assert!(matches!(parse("ctrl+banana"), Err(ChordError::UnknownToken(_))));
        assert!(matches!(parse("ctrl+a+b"), Err(ChordError::TwoKeys(_, _))));
    }
}
