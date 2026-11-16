//! Frame bytes, checksum seed, write repetition and waits, and the display → AV-service lookup
//! are ported from m1ddc (MIT, github.com/waydabber/m1ddc, `sources/i2c.m`,
//! `sources/ioregistry.m`). One deliberate difference: m1ddc decodes whatever comes back, so a
//! failed read prints `0`. Here a reply is validated, and a failed read is an error, never a value.
//!
//! Frames and the readback policy are pure and compile everywhere; `mac` holds the I/O.

use std::time::Duration;

/// MCCS input source select.
pub const VCP_INPUT: u8 = 0x60;
/// MCCS luminance (brightness).
pub const VCP_LUMINANCE: u8 = 0x10;

/// Sub-address of the display's DDC/CI interface (m1ddc `DEFAULT_INPUT_ADDRESS`).
pub const SUB_ADDR: u8 = 0x51;
/// The display's write address (7-bit 0x37 shifted); seeds the request checksum and is the
/// source byte of a reply.
const DISPLAY_ADDR: u8 = 0x6E;
/// Seeds the reply checksum: DDC/CI checksums a reply over the host's virtual address 0x50.
const HOST_VIRTUAL_ADDR: u8 = 0x50;
/// Bytes read per reply, as m1ddc reads them; the Get-VCP reply itself is the first 11.
pub const REPLY_LEN: usize = 12;

/// Measured on a BenQ EW2780Q flipping HDMI2 → DP: reads fail for 3–5 s after the flip, then
/// settle. Twelve one-second attempts cover that with margin.
pub const CONFIRM_ATTEMPTS: u32 = 12;
pub const CONFIRM_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    pub current: u16,
    pub max: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DdcError {
    /// IOAVService returned this non-zero IOReturn.
    Io(i32),
    /// Bytes came back but are not a valid Get-VCP reply for the code asked; kept for diagnosis.
    BadReply([u8; REPLY_LEN]),
    /// The display answered that it does not support this VCP code.
    Unsupported(u8),
    /// The capabilities string ran past `CAPS_MAX`: the display is repeating itself.
    CapsTooLong,
}

/// What reading back after a set established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readback {
    /// The display reports the value asked for.
    Confirmed(u16),
    /// The display answered with this other value on its last valid read.
    Mismatch(u16),
    /// No read produced a valid reply, so nothing is known about whether the set landed.
    Unreadable { last_error: Option<DdcError> },
}

/// Get-VCP request: `82 01 <vcp> <checksum>`.
pub fn get_request(vcp: u8) -> [u8; 4] {
    let mut f = [0x82, 0x01, vcp, 0];
    f[3] = xor(DISPLAY_ADDR ^ SUB_ADDR, &f[..3]);
    f
}

/// Set-VCP request: `84 03 <vcp> <value hi> <value lo> <checksum>`.
pub fn set_request(vcp: u8, value: u16) -> [u8; 6] {
    let [hi, lo] = value.to_be_bytes();
    let mut f = [0x84, 0x03, vcp, hi, lo, 0];
    f[5] = xor(DISPLAY_ADDR ^ SUB_ADDR, &f[..5]);
    f
}

/// Get-VCP reply: `6E 88 02 <result> <vcp> <type> <max hi> <max lo> <cur hi> <cur lo> <checksum>`.
pub fn parse_reply(vcp: u8, b: &[u8; REPLY_LEN]) -> Result<Reply, DdcError> {
    let framed = b[0] == DISPLAY_ADDR && b[1] == 0x88 && b[2] == 0x02;
    if !framed || b[10] != xor(HOST_VIRTUAL_ADDR, &b[..10]) {
        return Err(DdcError::BadReply(*b));
    }
    match (b[3], b[4] == vcp) {
        (0x00, true) => Ok(Reply { max: u16::from_be_bytes([b[6], b[7]]), current: u16::from_be_bytes([b[8], b[9]]) }),
        (0x01, true) => Err(DdcError::Unsupported(vcp)),
        _ => Err(DdcError::BadReply(*b)),
    }
}

/// Bytes read per capabilities fragment: source, length, `E3`, offset (2), up to 32 data bytes, checksum.
pub const CAPS_REPLY_LEN: usize = 38;
/// A capabilities string longer than this is a display looping on its offsets, not a real one.
pub const CAPS_MAX: usize = 2048;

/// Capabilities request for the fragment at `offset`: `83 F3 <offset hi> <offset lo> <checksum>`
/// (ddc-rs `CapabilitiesRequest`, MIT, github.com/arcnmx/ddc-rs `src/commands.rs`).
pub fn caps_request(offset: u16) -> [u8; 5] {
    let [hi, lo] = offset.to_be_bytes();
    let mut f = [0x83, 0xF3, hi, lo, 0];
    f[4] = xor(DISPLAY_ADDR ^ SUB_ADDR, &f[..4]);
    f
}

/// Capabilities reply: `6E <80|len> E3 <offset hi> <offset lo> <data…> <checksum>`; the data of the
/// fragment at `offset`. Empty data ends the string.
pub fn parse_caps_reply(offset: u16, b: &[u8; CAPS_REPLY_LEN]) -> Result<Vec<u8>, DdcError> {
    let bad = || DdcError::BadReply([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11]]);
    let len = usize::from(b[1] & 0x7F);
    if b[0] != DISPLAY_ADDR || b[1] & 0x80 == 0 || !(3..=35).contains(&len) || b[2] != 0xE3 {
        return Err(bad());
    }
    if b[2 + len] != xor(HOST_VIRTUAL_ADDR, &b[..2 + len]) || u16::from_be_bytes([b[3], b[4]]) != offset {
        return Err(bad());
    }
    Ok(b[5..2 + len].to_vec())
}

/// The input-source values a capabilities string lists under VCP 60: `Some(vec![])` when it names 60
/// without values, `None` when it does not name 60 or has no readable `vcp(…)` list.
pub fn input_sources(caps: &str) -> Option<Vec<u16>> {
    let start = caps.find("vcp(")? + "vcp(".len();
    let mut chars = caps[start..].chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let code: String = std::iter::from_fn(|| chars.next_if(char::is_ascii_hexdigit)).collect();
        if code.is_empty() {
            return None; // the closing `)` of vcp(…), or something this parser does not read
        }
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let mut values = Vec::new();
        if chars.next_if_eq(&'(').is_some() {
            let inner: String = std::iter::from_fn(|| chars.next_if(|c| *c != ')')).collect();
            chars.next()?;
            values = inner.split_whitespace().map(|v| u16::from_str_radix(v, 16).ok()).collect::<Option<_>>()?;
        }
        if u16::from_str_radix(&code, 16).ok() == Some(u16::from(VCP_INPUT)) {
            return Some(values);
        }
    }
}

/// MCCS 2.x names for VCP 60 values (mccs-rs `db/data/mccs.yml`, MIT, github.com/arcnmx/mccs-rs).
/// Values the standard reserves, which some displays use anyway, show as hex.
pub fn input_name(v: u16) -> String {
    let name = match v {
        0x01 => "VGA 1",
        0x02 => "VGA 2",
        0x03 => "DVI 1",
        0x04 => "DVI 2",
        0x05 => "Composite 1",
        0x06 => "Composite 2",
        0x07 => "S-video 1",
        0x08 => "S-video 2",
        0x09 => "Tuner 1",
        0x0A => "Tuner 2",
        0x0B => "Tuner 3",
        0x0C => "Component 1",
        0x0D => "Component 2",
        0x0E => "Component 3",
        0x0F => "DisplayPort 1",
        0x10 => "DisplayPort 2",
        0x11 => "HDMI 1",
        0x12 => "HDMI 2",
        0x19 => "USB-C 1",
        0x1B => "USB-C 2",
        _ => return format!("0x{v:02X}"),
    };
    name.to_string()
}

/// A monitor's EDID identity: manufacturer id (EDID bytes 8–9), product code (10–11), serial (12–15).
/// The same monitor reports the same values on every input, so a peer attached to it can be recognized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdidId {
    pub vendor: u32,
    pub product: u32,
    pub serial: u32,
}

/// Reads until `want` comes back or `attempts` run out, calling `pause` between reads. A failed
/// read says nothing about the value, so it never counts as a mismatch.
// ponytail: compares all 16 bits; some monitors pad the input code's high byte, mask to the low
// byte when one turns up.
pub fn confirm(
    mut read: impl FnMut() -> Result<u16, DdcError>,
    want: u16,
    attempts: u32,
    pause: impl FnMut(),
) -> Readback {
    confirm_all(|_| read(), &[want], attempts, pause).remove(0)
}

/// [`confirm`] for several monitors at once: each round reads every one not confirmed yet (by index into
/// `wants`), then pauses, so N monitors that switch together take as long as the slowest, not the sum.
pub fn confirm_all(
    mut read: impl FnMut(usize) -> Result<u16, DdcError>,
    wants: &[u16],
    attempts: u32,
    mut pause: impl FnMut(),
) -> Vec<Readback> {
    let mut done: Vec<Option<Readback>> = vec![None; wants.len()];
    let mut last_value = vec![None; wants.len()];
    let mut last_error = vec![None; wants.len()];
    for round in 0..attempts {
        if done.iter().all(Option::is_some) {
            break;
        }
        if round > 0 {
            pause();
        }
        for (i, &want) in wants.iter().enumerate() {
            if done[i].is_some() {
                continue;
            }
            match read(i) {
                Ok(v) if v == want => done[i] = Some(Readback::Confirmed(v)),
                Ok(v) => last_value[i] = Some(v),
                Err(e) => last_error[i] = Some(e),
            }
        }
    }
    done.into_iter()
        .zip(last_value.into_iter().zip(last_error))
        .map(|(d, (v, e))| d.unwrap_or_else(|| v.map_or(Readback::Unreadable { last_error: e }, Readback::Mismatch)))
        .collect()
}

fn xor(seed: u8, bytes: &[u8]) -> u8 {
    bytes.iter().fold(seed, |acc, b| acc ^ b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_frames_match_m1ddc() {
        assert_eq!(get_request(VCP_INPUT), [0x82, 0x01, 0x60, 0xDC]);
        assert_eq!(set_request(VCP_INPUT, 15), [0x84, 0x03, 0x60, 0x00, 0x0F, 0xD7]);
    }

    /// Captured from real displays with `examples/ddc.rs raw`: the independent check that the
    /// reply layout and checksum seed are right.
    const BENQ_INPUT: [u8; REPLY_LEN] = [0x6E, 0x88, 0x02, 0x00, 0x60, 0x00, 0x00, 0x15, 0x00, 0x12, 0xD3, 0x00];
    const BENQ_LUMINANCE: [u8; REPLY_LEN] = [0x6E, 0x88, 0x02, 0x00, 0x10, 0x00, 0x00, 0x64, 0x00, 0x3C, 0xFC, 0x00];
    const ASUS_INPUT: [u8; REPLY_LEN] = [0x6E, 0x88, 0x02, 0x00, 0x60, 0x00, 0x00, 0x13, 0x00, 0x13, 0xD4, 0x00];

    #[test]
    fn parses_replies_captured_from_real_displays() {
        assert_eq!(parse_reply(VCP_INPUT, &BENQ_INPUT), Ok(Reply { current: 18, max: 21 }));
        assert_eq!(parse_reply(VCP_LUMINANCE, &BENQ_LUMINANCE), Ok(Reply { current: 60, max: 100 }));
        assert_eq!(parse_reply(VCP_INPUT, &ASUS_INPUT), Ok(Reply { current: 19, max: 19 }));
    }

    /// The first capabilities fragment a BenQ EW2780Q sent, read with `examples/ddc.rs capsraw`; the bytes
    /// after its checksum are the display repeating itself into the rest of the buffer.
    const BENQ_CAPS_0: [u8; CAPS_REPLY_LEN] = [
        0x6E, 0x9D, 0xE3, 0x00, 0x00, 0x28, 0x70, 0x72, 0x6F, 0x74, 0x28, 0x6D, 0x6F, 0x6E, 0x69, 0x74, 0x6F, 0x72, 0x29,
        0x74, 0x79, 0x70, 0x65, 0x28, 0x4C, 0x43, 0x44, 0x29, 0x6D, 0x6F, 0x64, 0x28, 0x6E, 0x9D, 0xE3, 0x00, 0x00, 0x28,
    ];
    /// Capabilities strings as BetterDisplay read them (`--ddcCapabilitiesString`): a second reader, so the
    /// parser is not checked only against this module's own fetching.
    const BENQ_CAPS: &str = "(prot(monitor)type(LCD)model(EW2780Q)cmds(01 02 03 07 0C E3 F3)vcp(02 04 05 08 0B 0C 10 12 14(04 05 08 0B) 16 18 1A 52 60(0F 11 12) 62  86(01 02 05) 87 8D(01 02) AC AE B2 B5(00 01) B6 C0 C6 C8 C9 CA(01 02) CC(01 02 03 04 05 06 07 09 0A 0B 0D 0E 12 14 1A 1E 1F 20) D6(01 05) D8 DA(00 02) DC(00 03 0B 0C 0E 0F 10 11 12 13 14) DF E2(00 01) E3(00 01) E7(00 01) E8(00 01) E9(00 01 02 03) EA(00 01 02 03 04 05) EE(00 01) EF(00 01) F0(00 01 02) F1(00 01) F2(14 28 3C 50 64) F4(00 01) F5(00 01) F6(00 01) F7(00 01) F8(00 0A 14 1E) F9)mswhql(1)asset_eep(40)mccs_ver(2.2))";
    const ASUS_CAPS: &str = "(prot(monitor)type(lcd)model(XG17AHP)cmds(01 02 03 07 0C F3)vcp(02 04 05 08 10 12 14(05 06 08 0B) 16 18 1A 52 60(11 13) 62 72(50 78 96) 86(01 02 0B) 87(00 0A 14 1E 28 32 3C 46 50 5A 64) 8A AA(01 02 03 04 FF) AC AE B6 C6 C8 CC(01 02 03 04 05 06 07 08 09 0A 0C 0D 11 12 14 1A 1E 1F 30 23 31) D6(01 05) DF DC(03 0B 0D 0E 11 12 13 14) E2(00 01 02 03 04 05) E4(00 01) E6(00 01) E9(00 01) EA(00 01) EB(00 01) EC(00 01 02 03 04 05 06) EE(00 1E 28 32 3C 5A) EF(00 01 02 03) F0(00 01 02 03 04) F6(00 01 02)) mccs_ver(2.2))";

    #[test]
    fn a_real_capabilities_fragment_parses_and_a_wrong_one_does_not() {
        assert_eq!(caps_request(26), [0x83, 0xF3, 0x00, 0x1A, 0x6E ^ 0x51 ^ 0x83 ^ 0xF3 ^ 0x1A]);
        assert_eq!(parse_caps_reply(0, &BENQ_CAPS_0).unwrap(), b"(prot(monitor)type(LCD)mod");
        assert!(parse_caps_reply(26, &BENQ_CAPS_0).is_err(), "answer for another offset");
        let mut damaged = BENQ_CAPS_0;
        damaged[10] ^= 0x01;
        assert!(parse_caps_reply(0, &damaged).is_err());
        assert!(parse_caps_reply(0, &[0; CAPS_REPLY_LEN]).is_err());
    }

    #[test]
    fn the_end_of_the_string_is_an_empty_fragment() {
        let mut end = [0u8; CAPS_REPLY_LEN];
        end[..5].copy_from_slice(&[0x6E, 0x83, 0xE3, 0x01, 0x04]);
        end[5] = xor(HOST_VIRTUAL_ADDR, &end[..5]);
        assert_eq!(parse_caps_reply(0x104, &end), Ok(Vec::new()));
    }

    #[test]
    fn input_sources_come_from_vcp_60_only() {
        assert_eq!(input_sources(BENQ_CAPS), Some(vec![0x0F, 0x11, 0x12]));
        assert_eq!(input_sources(ASUS_CAPS), Some(vec![0x11, 0x13]));
        assert_eq!(input_sources("(vcp(10 12 60 62))"), Some(vec![]), "named without values");
        assert_eq!(input_sources("(vcp(10 12 62))"), None);
        assert_eq!(input_sources("(prot(monitor))"), None);
        assert_eq!(input_name(0x0F), "DisplayPort 1");
        assert_eq!(input_name(0x13), "0x13");
    }

    #[test]
    fn a_damaged_reply_is_an_error() {
        let mut flipped = BENQ_INPUT;
        flipped[9] ^= 0x01; // the value, so the checksum no longer covers it
        assert_eq!(parse_reply(VCP_INPUT, &flipped), Err(DdcError::BadReply(flipped)));
        assert_eq!(parse_reply(VCP_LUMINANCE, &BENQ_INPUT), Err(DdcError::BadReply(BENQ_INPUT)), "answer to another code");
    }

    #[test]
    fn an_unsupported_code_is_reported_as_such() {
        let mut r = BENQ_INPUT;
        r[3] = 0x01;
        r[10] ^= 0x01; // keep the checksum valid
        assert_eq!(parse_reply(VCP_INPUT, &r), Err(DdcError::Unsupported(VCP_INPUT)));
    }

    #[test]
    fn an_all_zero_reply_is_an_error_not_a_zero() {
        assert_eq!(parse_reply(VCP_LUMINANCE, &[0; REPLY_LEN]), Err(DdcError::BadReply([0; REPLY_LEN])));
    }

    #[test]
    fn confirm_skips_failed_reads_until_the_value_settles() {
        let mut script = [Err(DdcError::Io(-1)), Err(DdcError::BadReply([0; REPLY_LEN])), Ok(15)].into_iter();
        let mut pauses = 0;
        assert_eq!(confirm(|| script.next().unwrap(), 15, 5, || pauses += 1), Readback::Confirmed(15));
        assert_eq!(pauses, 2);
    }

    #[test]
    fn confirm_reports_the_last_valid_value_over_a_later_failure() {
        let mut script = [Ok(18), Ok(18), Err(DdcError::Io(-1))].into_iter();
        assert_eq!(confirm(|| script.next().unwrap(), 15, 3, || {}), Readback::Mismatch(18));
    }

    #[test]
    fn confirm_with_no_valid_read_is_unreadable_and_says_why() {
        let r = confirm(|| Err(DdcError::Io(-7)), 15, 3, || {});
        assert_eq!(r, Readback::Unreadable { last_error: Some(DdcError::Io(-7)) });
    }

    /// Two monitors switching together: a round reads each one still pending, a confirmed one is not read again,
    /// and the rounds stop once all are confirmed, so the wait is the slowest monitor's, not the sum.
    #[test]
    fn confirm_all_reads_the_pending_monitors_each_round_and_stops_when_all_land() {
        let mut first = [Ok(18), Ok(15)].into_iter();
        let mut second = [Ok(17), Ok(17), Ok(19)].into_iter();
        let mut reads = Vec::new();
        let mut pauses = 0;
        let r = confirm_all(|i| { reads.push(i); if i == 0 { first.next().unwrap() } else { second.next().unwrap() } }, &[15, 19], 12, || pauses += 1);
        assert_eq!(r, vec![Readback::Confirmed(15), Readback::Confirmed(19)]);
        assert_eq!(reads, [0, 1, 0, 1, 1]);
        assert_eq!(pauses, 2);
    }

    #[test]
    fn confirm_all_reports_each_monitor_on_its_own() {
        let r = confirm_all(|i| if i == 0 { Ok(15) } else if i == 1 { Ok(18) } else { Err(DdcError::Io(-7)) }, &[15, 19, 17], 3, || {});
        assert_eq!(r, vec![Readback::Confirmed(15), Readback::Mismatch(18), Readback::Unreadable { last_error: Some(DdcError::Io(-7)) }]);
    }
}
