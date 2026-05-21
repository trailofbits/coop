use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};

/// SHA-256 digest stored as the raw 32-byte array.
///
/// Round-trips through serde and `Display`/`FromStr` as 64 lowercase
/// hex characters with no prefix. Equality compares the raw bytes, so
/// a truncated or padded hex string cannot match a real digest by
/// accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256Hash([u8; 32]);

impl Sha256Hash {
    /// Compute the SHA-256 digest of `bytes`.
    pub fn of(bytes: impl AsRef<[u8]>) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes.as_ref());
        Self(hasher.finalize().into())
    }
}

impl fmt::Display for Sha256Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl FromStr for Sha256Hash {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        if s.len() != 64 {
            bail!("expected 64 hex characters, got {}", s.len());
        }
        let bytes =
            hex::decode(s).with_context(|| format!("invalid hex in sha256 digest: {s:?}"))?;
        let array: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("internal: hex::decode returned wrong length"))?;
        Ok(Self(array))
    }
}

impl Serialize for Sha256Hash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Sha256Hash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        s.parse()
            .map_err(|e: anyhow::Error| serde::de::Error::custom(format!("{e:#}")))
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests use unwrap for brevity")]
mod tests {
    use super::*;

    // sha256("hello world") = b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9
    const HELLO_WORLD_HEX: &str =
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

    #[test]
    fn of_matches_known_vector() {
        let hash = Sha256Hash::of("hello world");
        assert_eq!(hash.to_string(), HELLO_WORLD_HEX);
    }

    #[test]
    fn display_is_lowercase_64_chars_with_no_prefix() {
        let rendered = Sha256Hash::of(b"abc").to_string();
        assert_eq!(rendered.len(), 64);
        assert!(rendered.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(rendered.bytes().all(|b| !b.is_ascii_uppercase()));
        assert!(!rendered.contains(':'));
    }

    #[test]
    fn from_str_round_trips_through_display() {
        let original = Sha256Hash::of(b"round-trip me");
        let parsed: Sha256Hash = original.to_string().parse().unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn from_str_accepts_uppercase_hex() {
        let lower: Sha256Hash = HELLO_WORLD_HEX.parse().unwrap();
        let upper: Sha256Hash = HELLO_WORLD_HEX.to_ascii_uppercase().parse().unwrap();
        assert_eq!(lower, upper);
    }

    #[test]
    fn from_str_rejects_short_input() {
        let too_short = &HELLO_WORLD_HEX[..63];
        assert!(too_short.parse::<Sha256Hash>().is_err());
    }

    #[test]
    fn from_str_rejects_long_input() {
        let too_long = format!("{HELLO_WORLD_HEX}0");
        assert!(too_long.parse::<Sha256Hash>().is_err());
    }

    #[test]
    fn from_str_rejects_non_hex_characters() {
        let mut bad = String::from(HELLO_WORLD_HEX);
        bad.replace_range(0..1, "z");
        assert!(bad.parse::<Sha256Hash>().is_err());
    }

    #[test]
    fn from_str_rejects_legacy_sha256_prefix() {
        let prefixed = format!("sha256:{HELLO_WORLD_HEX}");
        assert!(prefixed.parse::<Sha256Hash>().is_err());
    }

    #[test]
    fn from_str_rejects_empty_string() {
        assert!("".parse::<Sha256Hash>().is_err());
    }

    #[test]
    fn serde_round_trip_through_json() {
        let original = Sha256Hash::of(b"json-trip");
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(json, format!("\"{original}\""));
        let parsed: Sha256Hash = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn serde_rejects_non_string_json() {
        assert!(serde_json::from_str::<Sha256Hash>("12345").is_err());
        assert!(serde_json::from_str::<Sha256Hash>("null").is_err());
    }

    #[test]
    fn serde_rejects_truncated_hex_in_json() {
        let truncated = format!("\"{}\"", &HELLO_WORLD_HEX[..63]);
        assert!(serde_json::from_str::<Sha256Hash>(&truncated).is_err());
    }

    #[test]
    fn equality_distinguishes_distinct_inputs() {
        assert_ne!(Sha256Hash::of(b"a"), Sha256Hash::of(b"b"));
        assert_eq!(Sha256Hash::of(b"a"), Sha256Hash::of(b"a"));
    }
}
