//! JSON encoding of the keychain value: `{"username": "<name>" | null, "token": "<token>"}`.
//!
//! Copies of the token made here are wiped: the output buffer is pre-sized so `serde_json` never reallocates (which
//! would leave a stale copy in freed memory) and is returned as `Zeroizing`; the decoded token string is wiped if
//! decoding fails part-way. Copies made inside the platform keychain APIs are outside our control.

use std::fmt;

use gityas_core::{Credential, GitYasError, Result};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

#[derive(Serialize)]
struct StoredRef<'a> {
    username: Option<&'a str>,
    token: &'a str,
}

#[derive(Deserialize)]
struct StoredOwned {
    #[serde(default)]
    username: Option<String>,
    token: WipeOnDrop,
}

/// A `String` that is zeroized when dropped (so a partially decoded value does not linger).
#[derive(Deserialize)]
#[serde(transparent)]
struct WipeOnDrop(String);

impl Drop for WipeOnDrop {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

pub(crate) fn encode(credential: &Credential) -> Result<Zeroizing<String>> {
    let token = credential.token.expose_secret();
    let username = credential.username.as_deref();
    // Worst case every byte is escaped as `\uXXXX` (6 bytes); the fixed JSON skeleton is < 64 bytes.
    let capacity = 64 + 6 * (token.len() + username.map_or(0, str::len));
    let mut buf = Zeroizing::new(Vec::with_capacity(capacity));
    serde_json::to_writer(&mut *buf, &StoredRef { username, token })
        .map_err(|e| GitYasError::Auth(format!("Could not encode the credential ({:?}).", e.classify())))?;
    match String::from_utf8(std::mem::take(&mut *buf)) {
        Ok(json) => Ok(Zeroizing::new(json)),
        Err(e) => {
            e.into_bytes().zeroize();
            Err(GitYasError::Auth("Could not encode the credential (invalid UTF-8).".into()))
        }
    }
}

/// Why a stored value could not be decoded. Deliberately carries no part of the input (a `serde_json` message can quote
/// the offending value, which might be the token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodeError {
    pub(crate) reason: &'static str,
    pub(crate) line: usize,
    pub(crate) column: usize,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at line {} column {}", self.reason, self.line, self.column)
    }
}

pub(crate) fn decode(json: &str) -> std::result::Result<Credential, DecodeError> {
    let stored: StoredOwned = serde_json::from_str(json).map_err(|e| DecodeError {
        reason: match e.classify() {
            serde_json::error::Category::Io => "I/O error",
            serde_json::error::Category::Syntax => "malformed JSON",
            serde_json::error::Category::Data => "unexpected JSON shape",
            serde_json::error::Category::Eof => "truncated JSON",
        },
        line: e.line(),
        column: e.column(),
    })?;
    let StoredOwned { username, mut token } = stored;
    if token.0.trim().is_empty() {
        return Err(DecodeError {
            reason: "empty token",
            line: 0,
            column: 0,
        });
    }
    let username = username.filter(|u| !u.trim().is_empty());
    Ok(Credential {
        username,
        token: SecretString::from(std::mem::take(&mut token.0)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "ghp_SuperSecretValue123";

    fn cred(username: Option<&str>, token: &str) -> Credential {
        Credential {
            username: username.map(str::to_owned),
            token: SecretString::from(token),
        }
    }

    #[test]
    fn encodes_exact_json_shape() {
        let json = encode(&cred(Some("alice"), TOKEN)).unwrap();
        assert_eq!(json.as_str(), r#"{"username":"alice","token":"ghp_SuperSecretValue123"}"#);
        let json = encode(&cred(None, TOKEN)).unwrap();
        assert_eq!(json.as_str(), r#"{"username":null,"token":"ghp_SuperSecretValue123"}"#);
    }

    #[test]
    fn round_trips_including_escapes_and_unicode() {
        for (u, t) in [
            (Some("alice"), TOKEN),
            (None, "tok\"en\\with\u{1}escapes"),
            (Some("jörg"), "ünï-cødé"),
        ] {
            let json = encode(&cred(u, t)).unwrap();
            let back = decode(&json).unwrap();
            assert_eq!(back.username.as_deref(), u);
            assert_eq!(back.token.expose_secret(), t);
        }
    }

    #[test]
    fn decode_accepts_missing_username_and_unknown_fields() {
        let c = decode(r#"{"token":"abc","future":1}"#).unwrap();
        assert_eq!(c.username, None);
        assert_eq!(c.token.expose_secret(), "abc");
        assert_eq!(decode(r#"{"username":"","token":"abc"}"#).unwrap().username, None);
    }

    #[test]
    fn decode_rejects_corrupt_values_without_quoting_them() {
        let cases = [
            "".to_owned(),
            "not json".to_owned(),
            TOKEN.to_owned(), // an old-style bare-token value
            format!(r#"{{"username":"a","token":{{"x":"{TOKEN}"}}}}"#),
            format!(r#"{{"username":"a","tokn":"{TOKEN}"}}"#),
            format!(r#"{{"username":["{TOKEN}"],"token":"x"}}"#),
            r#"{"username":"a","token":"  "}"#.to_owned(),
            format!(r#"{{"username":"a","token":"{TOKEN}""#),
        ];
        for case in cases {
            let err = decode(&case).err().unwrap_or_else(|| panic!("decoded corrupt value {case:?}"));
            let shown = format!("{err} {err:?}");
            assert!(
                !shown.contains(TOKEN) && !shown.contains("SuperSecret"),
                "decode error leaked input: {shown}"
            );
        }
    }
}
