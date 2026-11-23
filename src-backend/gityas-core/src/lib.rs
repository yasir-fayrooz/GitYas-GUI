//! Pure types shared by every GitYas crate.
//!
//! This crate performs no IO and does **not** depend on `git2`: conversions from `git2` types are free
//! functions in `gityas-git` (orphan rule).

pub mod diff;
pub mod error;
pub mod events;
pub mod ids;
pub mod models;
pub mod provider;

pub use error::{GitYasError, Result};
pub use events::{AppEvent, EventSink};
pub use ids::{Oid, RepoId};
pub use provider::{Credential, CredentialProvider, Provider};

/// Helpers shared by the unit tests of every module.
#[cfg(test)]
pub(crate) mod test_util {
    use std::fmt::Debug;

    use serde::Serialize;
    use serde::de::DeserializeOwned;

    use crate::Oid;

    /// Serialises `value` to JSON, deserialises it back, asserts equality and returns the JSON.
    pub fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) -> String {
        let json = serde_json::to_string(value).expect("serialise");
        let back: T = serde_json::from_str(&json).unwrap_or_else(|e| panic!("deserialise {json}: {e}"));
        assert_eq!(&back, value, "round trip changed the value; json = {json}");
        json
    }

    /// A deterministic SHA-1-sized oid filled with `byte`.
    pub fn oid(byte: u8) -> Oid {
        Oid::from_bytes(&[byte; 20]).expect("non-empty")
    }
}
