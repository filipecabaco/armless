//! WebSocket framing (RFC 6455) for both ends armless plays: the relay inside the machine is the
//! server of the guest's connections, and the native host is a client of real servers (WebSockets
//! the guest opens, and tunnels).
//!
//! Lengths come from the peer (the guest, or a server), so parsing is checked arithmetic
//! throughout: a frame no buffer could hold never completes rather than wrapping a bound.

pub const CONTINUATION: u8 = 0;
pub const TEXT: u8 = 1;
pub const BINARY: u8 = 2;
pub const CLOSE: u8 = 8;
pub const PING: u8 = 9;
pub const PONG: u8 = 10;

/// "No status code": what a close frame without one means, and never sent (RFC 6455 7.4.1).
pub const NO_STATUS: u16 = 1005;

#[derive(Debug, PartialEq)]
pub struct Frame {
    pub fin: bool,
    pub opcode: u8,
    /// Unmasked.
    pub payload: Vec<u8>,
}

/// The whole frame at the start of `b`, and how many bytes it took. None while it is still
/// arriving.
pub fn parse(b: &[u8]) -> Option<(Frame, usize)> {
    let (&b0, &b1) = (b.first()?, b.get(1)?);
    let (len, mut at) = match b1 & 127 {
        126 => (u16::from_be_bytes(b.get(2..4)?.try_into().ok()?) as usize, 4),
        127 => (usize::try_from(u64::from_be_bytes(b.get(2..10)?.try_into().ok()?)).ok()?, 10),
        n => (n as usize, 2),
    };
    let mask = if b1 & 128 != 0 {
        let m: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
        at += 4;
        Some(m)
    } else {
        None
    };
    let end = at.checked_add(len)?;
    let mut payload = b.get(at..end)?.to_vec();
    if let Some(m) = mask {
        apply_mask(&mut payload, m);
    }
    Some((Frame { fin: b0 & 0x80 != 0, opcode: b0 & 15, payload }, end))
}

/// A whole (final) frame of `payload`. Clients mask what they send, servers don't.
pub fn encode(opcode: u8, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
    let n = payload.len();
    let masked = if mask.is_some() { 0x80 } else { 0 };
    let mut f = Vec::with_capacity(n + 14);
    f.push(0x80 | opcode);
    if n < 126 {
        f.push(masked | n as u8);
    } else if n < 65536 {
        f.extend_from_slice(&[masked | 126, (n >> 8) as u8, n as u8]);
    } else {
        f.push(masked | 127);
        f.extend_from_slice(&(n as u64).to_be_bytes());
    }
    let start = f.len();
    match mask {
        Some(m) => {
            f.extend_from_slice(&m);
            f.extend_from_slice(payload);
            apply_mask(&mut f[start + 4..], m);
        },
        None => f.extend_from_slice(payload),
    }
    f
}

fn apply_mask(data: &mut [u8], m: [u8; 4]) {
    for (i, x) in data.iter_mut().enumerate() {
        *x ^= m[i & 3];
    }
}

/// The status code a close frame's payload carries, or `NO_STATUS`.
pub fn close_code(payload: &[u8]) -> u16 {
    match payload {
        [a, b, ..] => u16::from_be_bytes([*a, *b]),
        _ => NO_STATUS,
    }
}

/// The payload of a close frame with `code`: empty for none (`NO_STATUS`, or 0).
pub fn close_payload(code: u16) -> Vec<u8> {
    if code == 0 || code == NO_STATUS {
        Vec::new()
    } else {
        code.to_be_bytes().to_vec()
    }
}

/// Data frames put back together into messages (a message may span several frames).
#[derive(Default)]
pub struct Messages {
    text: bool,
    data: Vec<u8>,
}

impl Messages {
    /// Take a data frame (TEXT, BINARY or CONTINUATION): the message it completes, as
    /// (text, data).
    pub fn push(&mut self, f: Frame) -> Option<(bool, Vec<u8>)> {
        if f.opcode != CONTINUATION {
            self.text = f.opcode == TEXT;
            self.data.clear();
        }
        self.data.extend_from_slice(&f.payload);
        f.fin.then(|| (self.text, std::mem::take(&mut self.data)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_of_every_length_form_round_trip() {
        for n in [0, 1, 125, 126, 127, 65535, 65536, 70000] {
            let payload: Vec<u8> = (0..n).map(|i| (i * 7) as u8).collect();
            for mask in [None, Some([0x12, 0x34, 0x56, 0x78])] {
                let f = encode(BINARY, &payload, mask);
                let header = 2 + [0, 2, 8][(n >= 126) as usize + (n >= 65536) as usize];
                assert_eq!(f.len(), header + mask.map_or(0, |_| 4) + n, "{n} {mask:?}");
                assert_eq!(f[1] & 0x80 != 0, mask.is_some());
                let (back, used) = parse(&f).unwrap();
                assert_eq!(used, f.len());
                assert_eq!(back, Frame { fin: true, opcode: BINARY, payload: payload.clone() });
                // Any prefix is still arriving
                for cut in [0, 1, header - 1, f.len() - 1] {
                    if cut < f.len() {
                        assert_eq!(parse(&f[..cut]), None, "{n} cut at {cut}");
                    }
                }
            }
        }
    }

    #[test]
    fn rfc_6455_examples() {
        // 5.7: a single-frame unmasked "Hello", and the same masked
        let hello = [0x81, 0x05, 0x48, 0x65, 0x6c, 0x6c, 0x6f];
        assert_eq!(encode(TEXT, b"Hello", None), hello);
        let masked = [0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58];
        assert_eq!(encode(TEXT, b"Hello", Some([0x37, 0xfa, 0x21, 0x3d])), masked);
        assert_eq!(parse(&masked).unwrap().0.payload, b"Hello");
        // A fragmented unmasked text message: "Hel", then "lo"
        let mut m = Messages::default();
        let (f, n) = parse(&[0x01, 0x03, 0x48, 0x65, 0x6c]).unwrap();
        assert_eq!(n, 5);
        assert_eq!(m.push(f), None);
        let (f, _) = parse(&[0x80, 0x02, 0x6c, 0x6f]).unwrap();
        assert_eq!(m.push(f), Some((true, b"Hello".to_vec())));
    }

    #[test]
    fn lengths_no_buffer_holds_never_complete() {
        for len in [u64::MAX, u64::MAX - 3, u64::MAX - 9, u64::MAX - 13, 1 << 40] {
            for mask in [0, 0x80] {
                let mut f = vec![0x82, 127 | mask];
                f.extend_from_slice(&len.to_be_bytes());
                f.extend_from_slice(&[0; 16]);
                assert_eq!(parse(&f), None, "{len:#x}");
            }
        }
    }

    #[test]
    fn close_codes() {
        assert_eq!(close_code(&[]), NO_STATUS);
        assert_eq!(close_code(&[3]), NO_STATUS);
        assert_eq!(close_code(&[0x03, 0xe8, b'b', b'y', b'e']), 1000);
        assert_eq!(close_payload(1000), [0x03, 0xe8]);
        assert!(close_payload(NO_STATUS).is_empty() && close_payload(0).is_empty());
    }

    #[test]
    fn a_new_message_drops_an_unfinished_one() {
        let mut m = Messages::default();
        let frame = |fin, opcode, p: &[u8]| Frame { fin, opcode, payload: p.to_vec() };
        assert_eq!(m.push(frame(false, BINARY, b"lost")), None);
        assert_eq!(m.push(frame(true, TEXT, b"kept")), Some((true, b"kept".to_vec())));
        assert_eq!(m.push(frame(false, BINARY, &[1])), None);
        assert_eq!(m.push(frame(false, CONTINUATION, &[2])), None);
        assert_eq!(m.push(frame(true, CONTINUATION, &[3])), Some((false, vec![1, 2, 3])));
    }
}
