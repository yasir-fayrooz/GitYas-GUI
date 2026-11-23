use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::GitYasError;

/// Stable identifier of a registered repository (generated at add/clone time).
pub type RepoId = String;

/// A git object id, stored as raw bytes so both SHA-1 (20 bytes) and SHA-256 (32 bytes) repositories fit.
/// Serialises as lowercase hex.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid(Box<[u8]>);

impl Oid {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, GitYasError> {
        if bytes.is_empty() {
            return Err(GitYasError::InvalidInput("empty object id".into()));
        }
        Ok(Self(bytes.into()))
    }

    pub fn from_hex(hex: &str) -> Result<Self, GitYasError> {
        let invalid = || GitYasError::InvalidInput(format!("invalid object id: {hex:?}"));
        if hex.is_empty() || !hex.len().is_multiple_of(2) {
            return Err(invalid());
        }
        let bytes = hex
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let digits = std::str::from_utf8(pair).map_err(|_| invalid())?;
                u8::from_str_radix(digits, 16).map_err(|_| invalid())
            })
            .collect::<Result<Vec<u8>, _>>()?;
        Ok(Self(bytes.into()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// First `len` hex characters, for display.
    pub fn short(&self, len: usize) -> String {
        let mut hex = self.to_hex();
        hex.truncate(len);
        hex
    }
}

impl Oid {
    /// True for the all-zero id git uses for "no object" (e.g. the old side of a created ref).
    pub fn is_zero(&self) -> bool {
        self.0.iter().all(|b| *b == 0)
    }

    /// Case-insensitive prefix match on the hex form, for "jump to commit" by abbreviated id.
    /// An empty prefix never matches.
    pub fn starts_with_hex(&self, prefix: &str) -> bool {
        !prefix.is_empty() && prefix.len() <= self.0.len() * 2 && self.to_hex().starts_with(&prefix.to_ascii_lowercase())
    }
}

impl AsRef<[u8]> for Oid {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl std::str::FromStr for Oid {
    type Err = GitYasError;

    fn from_str(hex: &str) -> Result<Self, Self::Err> {
        Oid::from_hex(hex)
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", self.to_hex())
    }
}

impl Serialize for Oid {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Oid {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let hex = String::deserialize(deserializer)?;
        Oid::from_hex(&hex).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() -> Result<(), GitYasError> {
        let hex = "0123456789abcdef0123456789abcdef01234567";
        let oid = Oid::from_hex(hex)?;
        assert_eq!(oid.as_bytes().len(), 20);
        assert_eq!(oid.to_hex(), hex);
        assert_eq!(oid.short(7), "0123456");
        Ok(())
    }

    #[test]
    fn rejects_bad_hex() {
        assert!(Oid::from_hex("").is_err());
        assert!(Oid::from_hex("abc").is_err());
        assert!(Oid::from_hex("zz").is_err());
        assert!(Oid::from_hex("é1").is_err());
    }

    #[test]
    fn uppercase_hex_normalises_to_lowercase() -> Result<(), GitYasError> {
        let oid = Oid::from_hex("ABCDEF")?;
        assert_eq!(oid.to_hex(), "abcdef");
        Ok(())
    }

    #[test]
    fn sha256_length_round_trips() -> Result<(), GitYasError> {
        let hex = "ab".repeat(32);
        let oid: Oid = hex.parse()?;
        assert_eq!(oid.as_bytes().len(), 32);
        assert_eq!(oid.to_string(), hex);
        Ok(())
    }

    #[test]
    fn short_longer_than_id_returns_full_hex() -> Result<(), GitYasError> {
        let oid = Oid::from_hex("00ff")?;
        assert_eq!(oid.short(40), "00ff");
        assert_eq!(oid.short(0), "");
        Ok(())
    }

    #[test]
    fn zero_and_prefix() -> Result<(), GitYasError> {
        assert!(Oid::from_bytes(&[0; 20])?.is_zero());
        let oid = Oid::from_hex("abcd01")?;
        assert!(!oid.is_zero());
        assert!(oid.starts_with_hex("ABc"));
        assert!(oid.starts_with_hex("abcd01"));
        assert!(!oid.starts_with_hex(""));
        assert!(!oid.starts_with_hex("abcd012"));
        assert!(!oid.starts_with_hex("b"));
        Ok(())
    }

    #[test]
    fn empty_bytes_rejected_and_debug_is_hex() -> Result<(), GitYasError> {
        assert!(Oid::from_bytes(&[]).is_err());
        assert_eq!(format!("{:?}", Oid::from_hex("0a")?), "Oid(0a)");
        Ok(())
    }

    #[test]
    fn serde_rejects_invalid_hex() {
        assert!(serde_json::from_str::<Oid>("\"xyz\"").is_err());
        assert!(serde_json::from_str::<Oid>("\"\"").is_err());
        assert!(serde_json::from_str::<Oid>("12").is_err());
    }

    #[test]
    fn serde_as_hex_string() -> Result<(), Box<dyn std::error::Error>> {
        let oid = Oid::from_hex("00ff")?;
        let json = serde_json::to_string(&oid)?;
        assert_eq!(json, "\"00ff\"");
        assert_eq!(serde_json::from_str::<Oid>(&json)?, oid);
        Ok(())
    }
}
