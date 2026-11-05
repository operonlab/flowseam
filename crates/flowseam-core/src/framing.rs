//! Length-prefixed framing (design §2): `u32` big-endian length, then body.
//! A frame whose declared length exceeds `MAX_FRAME_BYTES` is a protocol
//! fault: the decoder returns the frames completed before it alongside the
//! error and clears its buffer, and the caller must drop the connection.

pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const HEADER: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    TooLarge { len: usize, limit: usize },
}

pub fn encode_frame(body: &[u8]) -> Result<Vec<u8>, FrameError> {
    if body.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge {
            len: body.len(),
            limit: MAX_FRAME_BYTES,
        });
    }
    let mut out = Vec::with_capacity(HEADER + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

/// Outcome of one `push`: every frame body completed by the chunk, plus the
/// fault if one was hit after them. Frames decoded before a fault are valid
/// and must be delivered; the caller must still drop the connection when
/// `error` is set (the buffer is cleared at that point).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pushed {
    pub frames: Vec<Vec<u8>>,
    pub error: Option<FrameError>,
}

#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes; returns every frame body completed by this chunk,
    /// together with a protocol fault if one occurred after them.
    pub fn push(&mut self, bytes: &[u8]) -> Pushed {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            if self.buf.len() < HEADER {
                break;
            }
            let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])
                as usize;
            if len > MAX_FRAME_BYTES {
                self.buf.clear();
                return Pushed {
                    frames,
                    error: Some(FrameError::TooLarge {
                        len,
                        limit: MAX_FRAME_BYTES,
                    }),
                };
            }
            if self.buf.len() < HEADER + len {
                break;
            }
            frames.push(self.buf[HEADER..HEADER + len].to_vec());
            self.buf.drain(..HEADER + len);
        }
        Pushed { frames, error: None }
    }

    /// Bytes received but not yet forming a complete frame.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_frames_in_one_chunk() {
        let mut wire = encode_frame(b"alpha").unwrap();
        wire.extend(encode_frame(b"beta").unwrap());
        let mut d = FrameDecoder::new();
        let frames = d.push(&wire).frames;
        assert_eq!(frames, vec![b"alpha".to_vec(), b"beta".to_vec()]);
        assert_eq!(d.buffered(), 0);
    }

    #[test]
    fn one_frame_split_across_three_pushes() {
        let wire = encode_frame(b"split-body").unwrap();
        let (a, rest) = wire.split_at(2); // mid-header
        let (b, c) = rest.split_at(5); // mid-body
        let mut d = FrameDecoder::new();
        assert!(d.push(a).frames.is_empty());
        assert!(d.push(b).frames.is_empty());
        assert_eq!(d.push(c).frames, vec![b"split-body".to_vec()]);
    }

    #[test]
    fn byte_by_byte_feed_yields_the_same_frames() {
        let mut wire = encode_frame(b"x").unwrap();
        wire.extend(encode_frame(b"yz").unwrap());
        let mut d = FrameDecoder::new();
        let mut got = Vec::new();
        for b in &wire {
            got.extend(d.push(&[*b]).frames);
        }
        assert_eq!(got, vec![b"x".to_vec(), b"yz".to_vec()]);
    }

    #[test]
    fn complete_frames_before_an_oversize_header_are_not_lost() {
        let mut wire = encode_frame(b"one").unwrap();
        wire.extend(encode_frame(b"two").unwrap());
        wire.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        let mut d = FrameDecoder::new();
        let p = d.push(&wire);
        assert_eq!(p.frames, vec![b"one".to_vec(), b"two".to_vec()]);
        assert!(matches!(p.error, Some(FrameError::TooLarge { .. })));
        assert_eq!(d.buffered(), 0, "fault still clears the buffer");
    }

    #[test]
    fn zero_length_frame_is_an_empty_body() {
        let wire = encode_frame(&[]).unwrap();
        assert_eq!(wire, vec![0, 0, 0, 0]);
        assert_eq!(FrameDecoder::new().push(&wire).frames, vec![Vec::<u8>::new()]);
    }

    #[test]
    fn length_at_limit_waits_and_one_over_is_an_error() {
        let at = (MAX_FRAME_BYTES as u32).to_be_bytes();
        let mut d = FrameDecoder::new();
        let p = d.push(&at);
        assert!(p.error.is_none() && p.frames.is_empty(), "exact limit must be accepted, not faulted");
        assert_eq!(d.buffered(), 4);

        let over = ((MAX_FRAME_BYTES + 1) as u32).to_be_bytes();
        let mut d = FrameDecoder::new();
        assert_eq!(
            d.push(&over).error,
            Some(FrameError::TooLarge { len: MAX_FRAME_BYTES + 1, limit: MAX_FRAME_BYTES })
        );
        assert_eq!(d.buffered(), 0, "fault must clear the buffer");
    }

    #[test]
    fn huge_declared_length_is_an_error_not_a_truncation() {
        let mut d = FrameDecoder::new();
        assert!(matches!(
            d.push(&[0xFF, 0xFF, 0xFF, 0xFF, 1, 2, 3]).error,
            Some(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn encoder_refuses_oversized_body() {
        let big = vec![0u8; MAX_FRAME_BYTES + 1];
        assert!(matches!(encode_frame(&big), Err(FrameError::TooLarge { .. })));
        assert!(encode_frame(&vec![0u8; MAX_FRAME_BYTES]).is_ok());
    }
}
