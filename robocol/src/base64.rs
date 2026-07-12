//! Minimal standard-alphabet base64 (RFC 4648, with padding) — just enough
//! for `CMD_RECEIVE_FRAME_CHUNK`'s `encodedData` field, matching Android's
//! `Base64.encodeToString(data, offset, len, 0)` on the RC side.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub fn decode(s: &str) -> Option<Vec<u8>> {
    let val = |b: u8| -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    // The RC encodes with Android's Base64.DEFAULT (flag 0), which wraps the
    // output at 76 chars with '\n', so skip all whitespace rather than treating
    // it as invalid; stop at the first '=' padding.
    let mut sextets = Vec::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => {}
            b'=' => break,
            _ => sextets.push(val(b)?),
        }
    }
    let mut out = Vec::with_capacity(sextets.len() / 4 * 3 + 2);
    for chunk in sextets.chunks(4) {
        match *chunk {
            [c0, c1, c2, c3] => {
                out.push((c0 << 2) | (c1 >> 4));
                out.push((c1 << 4) | (c2 >> 2));
                out.push((c2 << 6) | c3);
            }
            [c0, c1, c2] => {
                out.push((c0 << 2) | (c1 >> 4));
                out.push((c1 << 4) | (c2 >> 2));
            }
            [c0, c1] => out.push((c0 << 2) | (c1 >> 4)),
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let cases: &[&[u8]] = &[
            b"",
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            &[0, 1, 2, 3, 4, 5, 255, 254],
        ];
        for data in cases {
            assert_eq!(decode(&encode(data)).unwrap(), *data);
        }
    }

    #[test]
    fn matches_known_vectors() {
        assert_eq!(encode(b"Man"), "TWFu");
        assert_eq!(encode(b"Ma"), "TWE=");
        assert_eq!(encode(b"M"), "TQ==");
        assert_eq!(decode("TWFu").unwrap(), b"Man");
    }

    // The RC's Android Base64.DEFAULT wraps every 76 chars with '\n'; the
    // decoder must skip that whitespace instead of failing the whole chunk.
    #[test]
    fn decodes_wrapped_output() {
        let data: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        let mut wrapped = String::new();
        for (i, c) in encode(&data).chars().enumerate() {
            if i > 0 && i % 76 == 0 {
                wrapped.push('\n');
            }
            wrapped.push(c);
        }
        assert!(wrapped.contains('\n'));
        assert_eq!(decode(&wrapped).unwrap(), data);
    }
}
