//! Standard (RFC 4648) base64 with padding, shared by the devcontainer Feature
//! installer (encode) and the Apple sandbox host-key parser (decode).

const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode `bytes` with `=` padding.
pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Decode `s`, with or without trailing `=` padding. `None` when a character
/// is outside the alphabet.
#[cfg(all(target_os = "macos", feature = "apple-container"))]
pub(crate) fn decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = u32::try_from(TABLE.iter().position(|&b| b == c)?).ok()?;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).ok()?);
        }
    }
    Some(out)
}

#[cfg(test)]
#[cfg_attr(
    all(target_os = "macos", feature = "apple-container"),
    expect(clippy::unwrap_used, reason = "tests")
)]
mod tests {
    use super::*;

    #[test]
    fn encodes_known_vectors() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"archive-bytes"), "YXJjaGl2ZS1ieXRlcw==");
    }

    #[cfg(all(target_os = "macos", feature = "apple-container"))]
    #[test]
    fn decodes_known_vectors_and_round_trips() {
        for (enc, dec) in [
            ("", &b""[..]),
            ("Zg==", b"f"),
            ("Zm8", b"fo"),
            ("Zm9vYmFy", b"foobar"),
        ] {
            assert_eq!(decode(enc).unwrap(), dec);
        }
        assert!(decode("Zm9v*").is_none());
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&bytes)).unwrap(), bytes);
    }
}
