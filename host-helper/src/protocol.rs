//! What travels between a server and the helper: the signature, the names a request may use,
//! and the JSON shapes of each reply. Shared by the helper and the client, so the two can't
//! drift.
//!
//! A request is `POST /v1/<operation>` with a JSON body and four headers:
//!
//! | Header | Holds |
//! |---|---|
//! | `x-helper-stack` | The stack's name in the allow-list (`lockdown`). |
//! | `x-helper-time` | Unix time in seconds when it was signed. |
//! | `x-helper-nonce` | 32 random hex characters, never used twice. |
//! | `x-helper-signature` | HMAC-SHA256 with the stack's key over [`request_input`], in hex. |
//!
//! A reply to a signed request carries `x-helper-signature` too, over [`reply_input`], so a
//! server knows the answer came from the helper and belongs to its own request.

use std::fmt;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// The first line of everything signed, so a signature made for anything else never verifies.
pub const DOMAIN: &str = "snds-app-updates-host-helper/1";

/// How far a request's time may be from the helper's clock, either way.
pub const MAX_SKEW_SECS: u64 = 90;

/// The largest request body the helper reads.
pub const MAX_BODY: usize = 16 * 1024;

/// The header names, lower case.
pub const H_STACK: &str = "x-helper-stack";
pub const H_TIME: &str = "x-helper-time";
pub const H_NONCE: &str = "x-helper-nonce";
pub const H_SIGNATURE: &str = "x-helper-signature";

/// The environment variables a server reads its helper settings from.
pub const ENV_URL: &str = "HOST_HELPER_URL";
pub const ENV_STACK: &str = "HOST_HELPER_STACK";
pub const ENV_KEY: &str = "HOST_HELPER_KEY";

/// The three operations, and their paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Status,
    Update,
    Restart,
}

impl Operation {
    pub fn path(self) -> &'static str {
        match self {
            Operation::Status => "/v1/status",
            Operation::Update => "/v1/update",
            Operation::Restart => "/v1/restart",
        }
    }

    pub fn from_path(path: &str) -> Option<Self> {
        [Operation::Status, Operation::Update, Operation::Restart]
            .into_iter()
            .find(|op| op.path() == path)
    }

    pub fn name(self) -> &'static str {
        match self {
            Operation::Status => "status",
            Operation::Update => "update",
            Operation::Restart => "restart",
        }
    }

    /// Whether it changes anything on the host (one at a time per stack).
    pub fn changes_things(self) -> bool {
        !matches!(self, Operation::Status)
    }
}

/// A stack's shared secret: 32 random bytes, written as 64 hex characters. Its `Debug` and
/// `Display` never show it.
#[derive(Clone, PartialEq, Eq)]
pub struct Key([u8; 32]);

#[derive(Debug, PartialEq, Eq)]
pub struct BadKey;

impl fmt::Display for BadKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the key must be 64 hex characters (made by `app-updates-host-helper keygen`)")
    }
}

impl std::error::Error for BadKey {}

impl Key {
    /// A new key from the operating system's random source.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)?;
        Ok(Key(bytes))
    }

    /// Reads a key written by [`Key::to_hex`]; surrounding white space is ignored.
    pub fn parse(text: &str) -> Result<Self, BadKey> {
        let bytes = from_hex(text.trim()).ok_or(BadKey)?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| BadKey)?;
        Ok(Key(bytes))
    }

    /// The key as 64 hex characters, for its key file and the server's env file only.
    pub fn to_hex(&self) -> String {
        to_hex(&self.0)
    }

    fn mac(&self) -> HmacSha256 {
        HmacSha256::new_from_slice(&self.0).expect("HMAC takes a key of any length")
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key(hidden)")
    }
}

/// What a request's signature covers: the domain, the method, the path, the stack, the time,
/// the nonce and the body's SHA-256, one per line.
pub fn request_input(
    method: &str,
    path: &str,
    stack: &str,
    time: u64,
    nonce: &str,
    body: &[u8],
) -> Vec<u8> {
    format!(
        "{DOMAIN}\nrequest\n{method}\n{path}\n{stack}\n{time}\n{nonce}\n{}",
        to_hex(&Sha256::digest(body))
    )
    .into_bytes()
}

/// What a reply's signature covers: the request's nonce ties it to that one request.
pub fn reply_input(nonce: &str, status: u16, body: &[u8]) -> Vec<u8> {
    format!(
        "{DOMAIN}\nreply\n{nonce}\n{status}\n{}",
        to_hex(&Sha256::digest(body))
    )
    .into_bytes()
}

/// The hex HMAC-SHA256 of `input` under `key`.
pub fn sign(key: &Key, input: &[u8]) -> String {
    let mut mac = key.mac();
    mac.update(input);
    to_hex(&mac.finalize().into_bytes())
}

/// Whether `signature` (hex) is `key`'s over `input`. The comparison takes the same time
/// whatever the signature holds.
pub fn verify(key: &Key, input: &[u8], signature: &str) -> bool {
    let Some(given) = from_hex(signature) else {
        return false;
    };
    let mut mac = key.mac();
    mac.update(input);
    mac.verify_slice(&given).is_ok()
}

/// A new nonce: 16 random bytes as 32 hex characters.
pub fn new_nonce() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)?;
    Ok(to_hex(&bytes))
}

/// A nonce is exactly 32 lower-case hex characters.
pub fn valid_nonce(nonce: &str) -> bool {
    nonce.len() == 32
        && nonce
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A stack, project or service name: lower-case letters, digits, `-` and `_` (and `.` for a
/// service), starting with a letter or digit, at most 63 characters. Nothing that could read
/// as a command-line option or a path.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
}

/// A Docker tag: letters, digits, `_`, `.` and `-`, starting with a letter, digit or `_`, at
/// most 128 characters.
pub fn valid_tag(tag: &str) -> bool {
    let bytes = tag.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// A main-channel version tag, the form every snds image is published under:
/// `x.y.z-main.<commit time, UTC, YYYYMMDDHHMMSS>` (`0.1.0-main.20261108093000`).
pub fn is_main_version(tag: &str) -> bool {
    let Some((version, time)) = tag.split_once("-main.") else {
        return false;
    };
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 9 && p.bytes().all(|b| b.is_ascii_digit()))
        && time.len() == 14
        && time.bytes().all(|b| b.is_ascii_digit())
}

/// An image digest: `sha256:` and 64 lower-case hex characters.
pub fn valid_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// An image repository without a tag or digest: a registry host (with an optional port) and
/// a lower-case path (`ghcr.io/snds/lockdown-server`).
pub fn valid_repository(repo: &str) -> bool {
    let Some((host, path)) = repo.split_once('/') else {
        return false;
    };
    let (name, port) = match host.split_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    let host_ok = name.contains('.')
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'))
        && port
            .is_none_or(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()));
    let path_ok = !path.is_empty()
        && path.split('/').all(|part| {
            !part.is_empty()
                && part.as_bytes()[0].is_ascii_alphanumeric()
                && part.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
                })
        });
    host_ok && path_ok
}

/// The body of `POST /v1/update`. Both fields are optional: without a tag the stack's
/// `compose_tag` is pulled (usually `main`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRequest {
    /// The tag to pull: one the allow-list names, or a main-channel version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// The digest the server expects the pulled image to have (`sha256:…`), from its release
    /// manifest. When given, any other digest is refused and nothing restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_digest: Option<String>,
}

/// One allowed service, as `status` reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub service: String,
    /// `running`, `exited`, `restarting`… or `missing` when it has no container.
    pub state: String,
    /// The image the container was started from, as written (`ghcr.io/snds/lockdown-server:main`).
    pub image: Option<String>,
    /// The running image's registry digest for the allowed repository (`sha256:…`), if it came
    /// from the registry.
    pub digest: Option<String>,
    /// The image's `org.opencontainers.image.version` label, if it has one.
    pub version: Option<String>,
}

/// The reply to `status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReply {
    pub stack: String,
    /// The repository this stack may pull from.
    pub repository: String,
    pub services: Vec<ServiceStatus>,
}

/// The reply to `update`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateReply {
    pub stack: String,
    /// What was pulled (`ghcr.io/snds/lockdown-server:0.1.0-main.20261108093000`).
    pub pulled: String,
    /// Its registry digest.
    pub digest: String,
    /// The services recreated on it.
    pub services: Vec<String>,
}

/// The reply to `restart`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartReply {
    pub stack: String,
    pub services: Vec<String>,
}

/// Every refusal: a short code a program can match, and a sentence a person can read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorReply {
    pub error: String,
    pub message: String,
}

pub(crate) fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

pub(crate) fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    };
    text.as_bytes()
        .chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> Key {
        Key([byte; 32])
    }

    #[test]
    fn a_key_round_trips_and_never_prints() {
        let k = Key::generate().unwrap();
        assert_eq!(Key::parse(&format!("  {}\n", k.to_hex())).unwrap(), k);
        assert_eq!(format!("{k:?}"), "Key(hidden)");
        assert_eq!(Key::parse("abcd"), Err(BadKey));
        assert_eq!(Key::parse(&"zz".repeat(32)), Err(BadKey));
        assert_ne!(Key::generate().unwrap(), k);
    }

    #[test]
    fn a_signature_covers_every_field() {
        let k = key(7);
        let input = request_input("POST", "/v1/update", "lockdown", 100, "n", b"{}");
        let sig = sign(&k, &input);
        assert!(verify(&k, &input, &sig));
        assert!(!verify(&key(8), &input, &sig));
        for other in [
            request_input("GET", "/v1/update", "lockdown", 100, "n", b"{}"),
            request_input("POST", "/v1/restart", "lockdown", 100, "n", b"{}"),
            request_input("POST", "/v1/update", "vale", 100, "n", b"{}"),
            request_input("POST", "/v1/update", "lockdown", 101, "n", b"{}"),
            request_input("POST", "/v1/update", "lockdown", 100, "m", b"{}"),
            request_input("POST", "/v1/update", "lockdown", 100, "n", b"{ }"),
        ] {
            assert!(!verify(&k, &other, &sig));
        }
        assert!(!verify(&k, &input, "not hex"));
        assert!(!verify(&k, &input, &sig[..10]));
        // A reply's signature never passes for a request, or the other way round.
        let reply = reply_input("n", 200, b"{}");
        assert!(!verify(&k, &reply, &sig));
    }

    #[test]
    fn names_tags_and_digests() {
        assert!(valid_name("lockdown"));
        assert!(valid_name("vale-gateway_2"));
        for bad in ["", "-rm", "Lockdown", "a/b", "a b", "../x", &"a".repeat(64)] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert!(valid_tag("main"));
        assert!(valid_tag("0.1.0-main.20261108093000"));
        for bad in ["", "-f", ".x", "a:b", "a/b", "a b", "a;b", &"a".repeat(129)] {
            assert!(!valid_tag(bad), "{bad}");
        }
        assert!(is_main_version("0.1.0-main.20261108093000"));
        assert!(is_main_version("12.0.3-main.20991231235959"));
        for bad in [
            "main",
            "0.1.0",
            "0.1-main.20261108093000",
            "0.1.0-main.2026110809300",
            "0.1.0-main.20261108093000x",
            "v0.1.0-main.20261108093000",
            "0.1.0-beta.20261108093000",
        ] {
            assert!(!is_main_version(bad), "{bad}");
        }
        assert!(valid_digest(&format!("sha256:{}", "a".repeat(64))));
        assert!(!valid_digest(&format!("sha256:{}", "A".repeat(64))));
        assert!(!valid_digest(&format!("sha512:{}", "a".repeat(64))));
        assert!(!valid_digest("sha256:abc"));
    }

    #[test]
    fn repositories() {
        assert!(valid_repository("ghcr.io/snds/lockdown-server"));
        assert!(valid_repository("registry.local:5000/x"));
        for bad in [
            "lockdown-server",
            "ghcr.io/snds/lockdown-server:main",
            "ghcr.io/snds/lockdown-server@sha256:00",
            "ghcr.io/Snds/x",
            "ghcr.io//x",
            "-ghcr.io/x",
            "localhost/x",
            "ghcr.io:/x",
        ] {
            assert!(!valid_repository(bad), "{bad}");
        }
    }

    #[test]
    fn operations_have_fixed_paths() {
        assert_eq!(Operation::from_path("/v1/update"), Some(Operation::Update));
        assert_eq!(Operation::from_path("/v1/update/"), None);
        assert_eq!(Operation::from_path("/v1/exec"), None);
        assert!(!Operation::Status.changes_things());
        assert!(Operation::Restart.changes_things());
    }

    #[test]
    fn nonces() {
        let n = new_nonce().unwrap();
        assert!(valid_nonce(&n));
        assert!(!valid_nonce(&n.to_uppercase()));
        assert!(!valid_nonce("abc"));
    }

    #[test]
    fn an_update_request_refuses_unknown_fields() {
        assert!(serde_json::from_str::<UpdateRequest>(r#"{"tag":"main","cmd":"rm"}"#).is_err());
        let r: UpdateRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(r, UpdateRequest::default());
    }
}
