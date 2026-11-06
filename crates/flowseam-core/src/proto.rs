//! Wire protocol types (design §2) and their postcard encoding.
//!
//! Decoding is a trust boundary: `decode` rejects trailing bytes and any
//! message that violates the size or range limits from §2 (name ≤ 64 B,
//! displays ≤ 16, HID < 256, clipboard ≤ 1 MiB, known modifier bits only).

use serde::{Deserialize, Serialize};

/// 2: file drags (`DragBegin`, `FileChunk`, `DragAck`, `DragCancel`). 3: `DragSoon`.
pub const PROTO_VERSION: u16 = 4;
pub const MIN_PROTO_VERSION: u16 = 3;
pub const MAX_PATH_ADDRS: usize = 16;
pub const MAX_NAME_BYTES: usize = 64;
pub const MAX_DISPLAYS: usize = 16;
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
/// One file name in a drag. 255 bytes is what macOS and NTFS allow for one path component.
pub const MAX_FILE_NAME_BYTES: usize = 255;
pub const MAX_DRAG_FILES: usize = 256;
pub const MAX_CHUNK_BYTES: usize = 64 * 1024;
/// Button number of the primary (left) mouse button, as macOS numbers buttons and the wire carries them.
pub const PRIMARY_BUTTON: u8 = 0;
/// Upper bound for one pairing element or MAC. Real SPAKE2 elements are tens of bytes.
pub const MAX_PAIR_BYTES: usize = 256;
/// HID keyboard usages are carried as `u16`, but the local key set is a 256-bit
/// bitset (design §6), so anything at or above this is rejected on the wire.
pub const HID_LIMIT: u16 = 256;

/// What carries a path at one end. Ordered best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Wired,
    Other,
    WiFi,
}
impl Kind {
    pub fn rank(self) -> u8 {
        match self {
            Self::Wired => 0,
            Self::Other => 1,
            Self::WiFi => 2,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathAddr {
    pub addr: std::net::SocketAddr,
    pub kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// Logical-pixel rectangle. Half-open: covers `x..x+w` and `y..y+h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// Modifier bitmask. CapsLock is toggle state, not a held key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModifierSet(pub u8);

impl ModifierSet {
    pub const SHIFT: u8 = 1 << 0;
    pub const CTRL: u8 = 1 << 1;
    pub const ALT: u8 = 1 << 2;
    /// Cmd on macOS, Win on Windows.
    pub const GUI: u8 = 1 << 3;
    pub const CAPS: u8 = 1 << 4;
    pub const KNOWN: u8 = Self::SHIFT | Self::CTRL | Self::ALT | Self::GUI | Self::CAPS;

    pub fn contains(self, bit: u8) -> bool {
        self.0 & bit == bit
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
    Edge,
    Emergency,
    Timeout,
    Shutdown,
    TopologyChange,
    UserEscape,
    /// The recipient of an `Enter` is driving or being driven by another machine and cannot take it.
    Busy,
}

/// One file of a drag: its bare name (no directories) and exact length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Msg {
    Hello {
        ver: u16,
        node: [u8; 16],
        name: String,
        displays: Vec<Rect>,
    },
    HelloAck {
        ver: u16,
        accepted: bool,
        reason: Option<String>,
    },
    Enter {
        seq: u32,
        edge: Edge,
        frac: u16,
        mods: ModifierSet,
        pressed: Vec<u16>,
    },
    Leave {
        seq: u32,
        edge: Edge,
        frac: u16,
    },
    MotionRel {
        seq: u32,
        dx: i32,
        dy: i32,
    },
    Button {
        seq: u32,
        button: u8,
        down: bool,
    },
    Wheel {
        seq: u32,
        dx: i32,
        dy: i32,
    },
    Key {
        seq: u32,
        hid: u16,
        down: bool,
        repeat: bool,
    },
    ModSync {
        seq: u32,
        mods: ModifierSet,
        pressed: Vec<u16>,
    },
    Release {
        seq: u32,
        reason: Reason,
    },
    ClipboardText {
        gen: u64,
        text: String,
    },
    Ping {
        t: u64,
    },
    Pong {
        t: u64,
    },
    Fault {
        code: u16,
        msg: String,
    },
    /// Pairing (SPAKE2). `msg` is the opaque protocol element; flowseam_core does not
    /// interpret it. Only the type and the size limit live here.
    PairInit {
        node: [u8; 16],
        msg: Vec<u8>,
    },
    PairResp {
        node: [u8; 16],
        msg: Vec<u8>,
    },
    PairConfirm {
        mac: Vec<u8>,
    },
    /// Sent at the moment a file drag crosses, before any later input from the sender: `DragBegin` for
    /// this drag follows. A release the recipient sees in between is the drop.
    DragSoon {
        drag: u32,
    },
    /// A file drag crossed from the sender to the recipient. The files follow at once as `FileChunk`s,
    /// before any drop, so they are ready when the recipient sees the primary button released.
    DragBegin {
        drag: u32,
        files: Vec<FileMeta>,
    },
    /// The next bytes of `files[file]` of that drag. Chunks arrive in order, file after file.
    FileChunk {
        drag: u32,
        file: u16,
        data: Vec<u8>,
    },
    /// Recipient to sender: total bytes of the drag written so far. The sender keeps a bounded
    /// amount unacknowledged, so file data never queues far ahead of input on the shared link.
    DragAck {
        drag: u32,
        bytes: u64,
    },
    /// Either side abandons the drag; the recipient deletes what it staged.
    DragCancel {
        drag: u32,
    },
    Paths {
        kind: Kind,
        addrs: Vec<PathAddr>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    Encode(String),
    Decode(String),
    TrailingBytes(usize),
    Invalid(&'static str),
}

/// Reject messages that violate the §2 limits. Runs on both encode and decode.
pub fn validate(m: &Msg) -> Result<(), ProtoError> {
    match m {
        Msg::Hello { name, displays, .. } => {
            if name.len() > MAX_NAME_BYTES {
                return Err(ProtoError::Invalid("name exceeds 64 bytes"));
            }
            if displays.len() > MAX_DISPLAYS {
                return Err(ProtoError::Invalid("more than 16 displays"));
            }
            if displays.iter().any(|r| r.w <= 0 || r.h <= 0) {
                return Err(ProtoError::Invalid("display with non-positive size"));
            }
            Ok(())
        }
        Msg::Enter { mods, pressed, .. } | Msg::ModSync { mods, pressed, .. } => {
            check_mods(*mods)?;
            check_pressed(pressed)
        }
        Msg::Key { hid, .. } => check_hid(*hid),
        Msg::PairInit { msg, .. } | Msg::PairResp { msg, .. } | Msg::PairConfirm { mac: msg } => {
            if msg.len() > MAX_PAIR_BYTES {
                Err(ProtoError::Invalid("pairing bytes exceed 256"))
            } else {
                Ok(())
            }
        }
        Msg::ClipboardText { text, .. } => {
            if text.len() > MAX_CLIPBOARD_BYTES {
                Err(ProtoError::Invalid("clipboard exceeds 1 MiB"))
            } else {
                Ok(())
            }
        }
        Msg::DragBegin { files, .. } => {
            if files.is_empty() || files.len() > MAX_DRAG_FILES {
                return Err(ProtoError::Invalid("drag needs 1 to 256 files"));
            }
            if files.iter().any(|f| f.name.is_empty() || f.name.len() > MAX_FILE_NAME_BYTES) {
                return Err(ProtoError::Invalid("file name empty or over 255 bytes"));
            }
            Ok(())
        }
        Msg::FileChunk { data, .. } => {
            if data.is_empty() || data.len() > MAX_CHUNK_BYTES {
                Err(ProtoError::Invalid("file chunk empty or over 64 KiB"))
            } else {
                Ok(())
            }
        }
        Msg::Paths { addrs, .. } if addrs.len() > MAX_PATH_ADDRS => {
            Err(ProtoError::Invalid("more than 16 path addresses"))
        }
        _ => Ok(()),
    }
}

fn check_mods(mods: ModifierSet) -> Result<(), ProtoError> {
    if mods.0 & !ModifierSet::KNOWN != 0 {
        Err(ProtoError::Invalid("unknown modifier bits"))
    } else {
        Ok(())
    }
}

fn check_hid(hid: u16) -> Result<(), ProtoError> {
    if hid >= HID_LIMIT {
        Err(ProtoError::Invalid("hid outside 256-key set"))
    } else {
        Ok(())
    }
}

fn check_pressed(pressed: &[u16]) -> Result<(), ProtoError> {
    if pressed.len() > HID_LIMIT as usize {
        return Err(ProtoError::Invalid("pressed list longer than key set"));
    }
    pressed.iter().try_for_each(|h| check_hid(*h))
}

pub fn encode(m: &Msg) -> Result<Vec<u8>, ProtoError> {
    validate(m)?;
    postcard::to_allocvec(m).map_err(|e| ProtoError::Encode(e.to_string()))
}

pub fn decode(bytes: &[u8]) -> Result<Msg, ProtoError> {
    let (m, rest) =
        postcard::take_from_bytes::<Msg>(bytes).map_err(|e| ProtoError::Decode(e.to_string()))?;
    if !rest.is_empty() {
        return Err(ProtoError::TrailingBytes(rest.len()));
    }
    validate(&m)?;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> Vec<Msg> {
        vec![
            Msg::Hello {
                ver: PROTO_VERSION,
                node: [7; 16],
                name: "mac-studio".into(),
                displays: vec![
                    Rect { x: -1920, y: -1080, w: 1920, h: 1080 },
                    Rect { x: 0, y: 0, w: 2560, h: 1440 },
                ],
            },
            Msg::HelloAck { ver: 1, accepted: false, reason: Some("fingerprint".into()) },
            Msg::HelloAck { ver: 1, accepted: true, reason: None },
            Msg::Enter {
                seq: u32::MAX,
                edge: Edge::Bottom,
                frac: 65535,
                mods: ModifierSet(ModifierSet::CTRL | ModifierSet::CAPS),
                pressed: vec![0, 0x04, 0xE7],
            },
            Msg::Leave { seq: 0, edge: Edge::Top, frac: 0 },
            Msg::MotionRel { seq: 1, dx: i32::MIN, dy: i32::MAX },
            Msg::Button { seq: 2, button: 3, down: true },
            Msg::Wheel { seq: 3, dx: -1, dy: 1 },
            Msg::Key { seq: 4, hid: 0xE1, down: false, repeat: true },
            Msg::ModSync { seq: 5, mods: ModifierSet(0), pressed: vec![] },
            Msg::Release { seq: 6, reason: Reason::Emergency },
            Msg::ClipboardText { gen: u64::MAX, text: "剪貼簿 ✓".into() },
            Msg::Ping { t: u64::MAX },
            Msg::Pong { t: 0 },
            Msg::Fault { code: 9, msg: "uipi".into() },
            Msg::PairInit { node: [3; 16], msg: vec![0x01, 0x02, 0xFF] },
            Msg::PairResp { node: [4; 16], msg: vec![0xAA; 33] },
            Msg::PairConfirm { mac: vec![0; 32] },
            Msg::DragBegin { drag: 7, files: vec![FileMeta { name: "報告.pdf".into(), size: 1 << 40 }, FileMeta { name: "a".into(), size: 0 }] },
            Msg::FileChunk { drag: 7, file: 1, data: vec![0xAB; 3] },
            Msg::DragAck { drag: 7, bytes: u64::MAX },
            Msg::DragCancel { drag: u32::MAX },
            Msg::DragSoon { drag: 9 },
            Msg::Paths {
                kind: Kind::Wired,
                addrs: vec![PathAddr {
                    addr: "127.0.0.1:42".parse().unwrap(),
                    kind: Kind::WiFi,
                }],
            },
        ]
    }

    #[test]
    fn every_variant_round_trips() {
        let all = fixtures();
        // Exhaustive match: adding a variant without a fixture fails to compile
        // or leaves its index unseen below.
        let mut seen = [false; 23];
        for m in &all {
            let i = match m {
                Msg::Hello { .. } => 0,
                Msg::HelloAck { .. } => 1,
                Msg::Enter { .. } => 2,
                Msg::Leave { .. } => 3,
                Msg::MotionRel { .. } => 4,
                Msg::Button { .. } => 5,
                Msg::Wheel { .. } => 6,
                Msg::Key { .. } => 7,
                Msg::ModSync { .. } => 8,
                Msg::Release { .. } => 9,
                Msg::ClipboardText { .. } => 10,
                Msg::Ping { .. } => 11,
                Msg::Pong { .. } => 12,
                Msg::Fault { .. } => 13,
                Msg::PairInit { .. } => 14,
                Msg::PairResp { .. } => 15,
                Msg::PairConfirm { .. } => 16,
                Msg::DragBegin { .. } => 17,
                Msg::FileChunk { .. } => 18,
                Msg::DragAck { .. } => 19,
                Msg::DragCancel { .. } => 20,
                Msg::DragSoon { .. } => 21,
                Msg::Paths { .. } => 22,
            };
            seen[i] = true;
        }
        assert!(seen.iter().all(|&s| s), "every one of the 23 variants needs a fixture");
        for m in all {
            let bytes = encode(&m).expect("encode");
            let back = decode(&bytes).expect("decode");
            assert_eq!(back, m);
        }
    }

    #[test]
    fn every_strict_prefix_is_an_error_not_a_panic() {
        for m in fixtures() {
            let bytes = encode(&m).unwrap();
            for cut in 0..bytes.len() {
                assert!(decode(&bytes[..cut]).is_err(), "prefix {cut} of {m:?} decoded");
            }
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = encode(&Msg::Ping { t: 5 }).unwrap();
        bytes.push(0);
        assert_eq!(decode(&bytes), Err(ProtoError::TrailingBytes(1)));
    }

    #[test]
    fn oversized_name_is_rejected_at_both_ends() {
        let m = Msg::Hello { ver: 1, node: [0; 16], name: "n".repeat(65), displays: vec![] };
        assert!(matches!(encode(&m), Err(ProtoError::Invalid(_))));
        // Same message forged past the encoder: decode must still reject it.
        let forged = postcard::to_allocvec(&m).unwrap();
        assert!(matches!(decode(&forged), Err(ProtoError::Invalid(_))));
    }

    #[test]
    fn name_at_limit_is_accepted() {
        let m = Msg::Hello { ver: 1, node: [0; 16], name: "n".repeat(64), displays: vec![] };
        assert!(encode(&m).is_ok());
    }

    #[test]
    fn display_count_and_shape_limits() {
        let r = Rect { x: 0, y: 0, w: 1, h: 1 };
        let seventeen = Msg::Hello { ver: 1, node: [0; 16], name: String::new(), displays: vec![r; 17] };
        assert!(encode(&seventeen).is_err());
        let sixteen = Msg::Hello { ver: 1, node: [0; 16], name: String::new(), displays: vec![r; 16] };
        assert!(encode(&sixteen).is_ok());
        let empty = Msg::Hello {
            ver: 1,
            node: [0; 16],
            name: String::new(),
            displays: vec![Rect { x: 0, y: 0, w: 0, h: 10 }],
        };
        assert!(encode(&empty).is_err());
    }

    #[test]
    fn paths_count_limit() {
        let addr = PathAddr {
            addr: "127.0.0.1:42".parse().unwrap(),
            kind: Kind::Other,
        };
        let sixteen = Msg::Paths {
            kind: Kind::Wired,
            addrs: vec![addr; 16],
        };
        assert_eq!(decode(&encode(&sixteen).unwrap()).unwrap(), sixteen);
        let seventeen = Msg::Paths {
            kind: Kind::Wired,
            addrs: vec![addr; 17],
        };
        assert!(encode(&seventeen).is_err());
        assert!(decode(&postcard::to_allocvec(&seventeen).unwrap()).is_err());
    }

    #[test]
    fn v3_golden_bytes() {
        assert_eq!(
            encode(&Msg::Hello {
                ver: 3,
                node: [1; 16],
                name: "a".into(),
                displays: vec![]
            })
            .unwrap(),
            [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 97, 0]
        );
        assert_eq!(
            encode(&Msg::MotionRel {
                seq: 1,
                dx: 2,
                dy: -3
            })
            .unwrap(),
            [4, 1, 4, 5]
        );
    }

    #[test]
    fn hid_outside_key_set_is_rejected() {
        assert!(encode(&Msg::Key { seq: 0, hid: 256, down: true, repeat: false }).is_err());
        assert!(encode(&Msg::Key { seq: 0, hid: 255, down: true, repeat: false }).is_ok());
        let bad_enter = Msg::Enter {
            seq: 0,
            edge: Edge::Left,
            frac: 0,
            mods: ModifierSet(0),
            pressed: vec![300],
        };
        assert!(encode(&bad_enter).is_err());
    }

    #[test]
    fn unknown_modifier_bits_are_rejected() {
        let m = Msg::ModSync { seq: 0, mods: ModifierSet(0x80), pressed: vec![] };
        assert!(matches!(encode(&m), Err(ProtoError::Invalid(_))));
    }

    #[test]
    fn clipboard_limit_is_exactly_one_mib() {
        let at = Msg::ClipboardText { gen: 1, text: "a".repeat(MAX_CLIPBOARD_BYTES) };
        assert!(encode(&at).is_ok());
        let over = Msg::ClipboardText { gen: 1, text: "a".repeat(MAX_CLIPBOARD_BYTES + 1) };
        assert!(encode(&over).is_err());
    }

    #[test]
    fn oversized_pairing_bytes_are_rejected_at_both_ends() {
        let big = vec![0u8; MAX_PAIR_BYTES + 1];
        let init = Msg::PairInit { node: [0; 16], msg: big.clone() };
        assert!(matches!(encode(&init), Err(ProtoError::Invalid(_))));
        let forged = postcard::to_allocvec(&init).unwrap();
        assert!(matches!(decode(&forged), Err(ProtoError::Invalid(_))));
        let confirm = Msg::PairConfirm { mac: big };
        assert!(matches!(encode(&confirm), Err(ProtoError::Invalid(_))));
    }

    #[test]
    fn drag_limits_hold_at_both_ends() {
        let meta = |name: &str| FileMeta { name: name.into(), size: 1 };
        let bad = [
            Msg::DragBegin { drag: 1, files: vec![] },
            Msg::DragBegin { drag: 1, files: vec![meta(""); 1] },
            Msg::DragBegin { drag: 1, files: vec![meta(&"n".repeat(MAX_FILE_NAME_BYTES + 1))] },
            Msg::DragBegin { drag: 1, files: vec![meta("a"); MAX_DRAG_FILES + 1] },
            Msg::FileChunk { drag: 1, file: 0, data: vec![] },
            Msg::FileChunk { drag: 1, file: 0, data: vec![0; MAX_CHUNK_BYTES + 1] },
        ];
        for m in bad {
            assert!(matches!(encode(&m), Err(ProtoError::Invalid(_))), "{m:?} encoded");
            let forged = postcard::to_allocvec(&m).unwrap();
            assert!(matches!(decode(&forged), Err(ProtoError::Invalid(_))), "{m:?} decoded");
        }
        let at_limits = [
            Msg::DragBegin { drag: 1, files: vec![meta(&"n".repeat(MAX_FILE_NAME_BYTES)); MAX_DRAG_FILES] },
            Msg::FileChunk { drag: 1, file: 0, data: vec![0; MAX_CHUNK_BYTES] },
        ];
        for m in at_limits {
            assert!(encode(&m).is_ok(), "{m:?} refused");
        }
    }

    #[test]
    fn garbage_bytes_do_not_panic() {
        for seed in 0u8..=255 {
            let junk = [seed, 0xFF, 0xFF, 0xFF, 0x0F, seed.wrapping_mul(3)];
            let _ = decode(&junk);
        }
    }
}
