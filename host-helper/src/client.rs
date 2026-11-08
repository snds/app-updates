//! The client a server uses to ask the helper for `status`, `update` or `restart`.
//!
//! ```no_run
//! use host_helper::{Client, UpdateRequest};
//!
//! // HOST_HELPER_URL, HOST_HELPER_STACK and HOST_HELPER_KEY, from the server's env file.
//! let helper = Client::from_env()?;
//! let status = helper.status()?;
//! let done = helper.update(&UpdateRequest {
//!     tag: Some("0.1.0-main.20261108093000".into()),
//!     expect_digest: None,
//! })?;
//! # Ok::<(), host_helper::ClientError>(())
//! ```
//!
//! Calls block (an update waits for the pull and the restart); an async server runs them on a
//! blocking thread. Errors carry the helper's own sentence, ready to show on the admin page.

use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;

use crate::http::{self, ReadError};
use crate::protocol::{
    self, ENV_KEY, ENV_STACK, ENV_URL, ErrorReply, H_NONCE, H_SIGNATURE, H_STACK, H_TIME, Key,
    Operation, RestartReply, StatusReply, UpdateReply, UpdateRequest,
};

/// The largest reply the client reads.
const MAX_REPLY: usize = 256 * 1024;

/// How long `status` may take.
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(30);

/// How long `update` and `restart` may take (a pull of a large image on a slow line).
pub const CHANGE_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Where the helper listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// `http://host-helper:7480`: a host name or address and a port, on the stack's private
    /// network.
    Tcp(String),
    /// `unix:/run/host-helper/helper.sock`: a socket file shared with the stack.
    Unix(PathBuf),
}

impl Endpoint {
    pub fn parse(url: &str) -> Result<Self, ClientError> {
        if let Some(path) = url.strip_prefix("unix:") {
            if !path.starts_with('/') {
                return Err(ClientError::Config(
                    "The helper's socket path must be absolute, like unix:/run/host-helper/helper.sock.".into(),
                ));
            }
            return Ok(Endpoint::Unix(PathBuf::from(path)));
        }
        let rest = url.strip_prefix("http://").ok_or_else(|| {
            ClientError::Config(
                "The helper's address must look like http://host-helper:7480 or unix:/path/to/helper.sock.".into(),
            )
        })?;
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        let ok = rest.rsplit_once(':').is_some_and(|(host, port)| {
            !host.is_empty() && !host.contains(['/', '@', '?', '#']) && port.parse::<u16>().is_ok()
        });
        if !ok {
            return Err(ClientError::Config(
                "The helper's address needs a host and a port, like http://host-helper:7480."
                    .into(),
            ));
        }
        Ok(Endpoint::Tcp(rest.to_owned()))
    }
}

/// Why a call failed, in words a person can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// The server's helper settings are missing or wrong.
    Config(String),
    /// The helper couldn't be reached, or the connection broke.
    Unreachable(String),
    /// The helper answered with a refusal. `message` is its sentence.
    Refused {
        status: u16,
        code: String,
        message: String,
    },
    /// The answer wasn't one the helper would give (unsigned, wrongly signed or garbled).
    BadReply(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Config(m) | ClientError::Unreachable(m) | ClientError::BadReply(m) => {
                f.write_str(m)
            }
            ClientError::Refused { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for ClientError {}

/// One server's connection settings for its helper.
#[derive(Clone, Debug)]
pub struct Client {
    endpoint: Endpoint,
    stack: String,
    key: Key,
}

impl Client {
    pub fn new(url: &str, stack: &str, key: Key) -> Result<Self, ClientError> {
        if !protocol::valid_name(stack) {
            return Err(ClientError::Config(format!(
                "The stack name {stack:?} isn't one the helper accepts: lower-case letters, digits, - and _."
            )));
        }
        Ok(Client {
            endpoint: Endpoint::parse(url)?,
            stack: stack.to_owned(),
            key,
        })
    }

    /// Reads `HOST_HELPER_URL`, `HOST_HELPER_STACK` and `HOST_HELPER_KEY`.
    pub fn from_env() -> Result<Self, ClientError> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// The same, from any lookup (tests, or a server's own settings).
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Self, ClientError> {
        let get = |name: &str| {
            var(name).filter(|v| !v.trim().is_empty()).ok_or_else(|| {
                ClientError::Config(format!(
                    "One-click Update and Reload need the host helper: {name} isn't set in this server's env file."
                ))
            })
        };
        let url = get(ENV_URL)?;
        let stack = get(ENV_STACK)?;
        let key = Key::parse(&get(ENV_KEY)?).map_err(|_| {
            ClientError::Config(format!(
                "{ENV_KEY} isn't a helper key: it must be the 64 hex characters keygen wrote."
            ))
        })?;
        Self::new(url.trim(), stack.trim(), key)
    }

    pub fn stack(&self) -> &str {
        &self.stack
    }

    /// The running image of each allowed service.
    pub fn status(&self) -> Result<StatusReply, ClientError> {
        self.call(Operation::Status, b"{}", STATUS_TIMEOUT)
    }

    /// Pulls the image (at `request.tag`, or the stack's own tag) and recreates the allowed
    /// services on it.
    pub fn update(&self, request: &UpdateRequest) -> Result<UpdateReply, ClientError> {
        let body = serde_json::to_vec(request).expect("an update request always serializes");
        self.call(Operation::Update, &body, CHANGE_TIMEOUT)
    }

    /// Restarts the allowed services.
    pub fn restart(&self) -> Result<RestartReply, ClientError> {
        self.call(Operation::Restart, b"{}", CHANGE_TIMEOUT)
    }

    fn call<T: DeserializeOwned>(
        &self,
        op: Operation,
        body: &[u8],
        timeout: Duration,
    ) -> Result<T, ClientError> {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let nonce = protocol::new_nonce()
            .map_err(|_| ClientError::Config("This system's random source failed.".into()))?;
        let signature = protocol::sign(
            &self.key,
            &protocol::request_input("POST", op.path(), &self.stack, time, &nonce, body),
        );
        let headers = [
            (H_STACK, self.stack.clone()),
            (H_TIME, time.to_string()),
            (H_NONCE, nonce.clone()),
            (H_SIGNATURE, signature),
        ];
        let reply = match &self.endpoint {
            Endpoint::Tcp(addr) => {
                let unreachable = |e: std::io::Error| {
                    ClientError::Unreachable(format!(
                        "The host helper at {addr} can't be reached ({e}). Is it running, and is this server on its network?"
                    ))
                };
                let sock = addr
                    .to_socket_addrs()
                    .map_err(unreachable)?
                    .next()
                    .ok_or_else(|| unreachable(std::io::Error::other("no address")))?;
                let mut stream = TcpStream::connect_timeout(&sock, Duration::from_secs(5))
                    .map_err(unreachable)?;
                stream
                    .set_read_timeout(Some(timeout))
                    .map_err(unreachable)?;
                stream
                    .set_write_timeout(Some(Duration::from_secs(10)))
                    .map_err(unreachable)?;
                exchange(&mut stream, op, &headers, body)?
            }
            #[cfg(unix)]
            Endpoint::Unix(path) => {
                let unreachable = |e: std::io::Error| {
                    ClientError::Unreachable(format!(
                        "The host helper's socket {} can't be reached ({e}). Is it running, and is the socket shared with this server?",
                        path.display()
                    ))
                };
                let mut stream =
                    std::os::unix::net::UnixStream::connect(path).map_err(unreachable)?;
                stream
                    .set_read_timeout(Some(timeout))
                    .map_err(unreachable)?;
                stream
                    .set_write_timeout(Some(Duration::from_secs(10)))
                    .map_err(unreachable)?;
                exchange(&mut stream, op, &headers, body)?
            }
            #[cfg(not(unix))]
            Endpoint::Unix(_) => {
                return Err(ClientError::Config(
                    "A socket address works only on Linux and macOS.".into(),
                ));
            }
        };
        self.judge(&nonce, reply)
    }

    fn judge<T: DeserializeOwned>(
        &self,
        nonce: &str,
        reply: http::Message,
    ) -> Result<T, ClientError> {
        let status: u16 = reply
            .start
            .get(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| ClientError::BadReply("The helper's answer was garbled.".into()))?;
        let signed = reply.header(H_SIGNATURE).is_some_and(|sig| {
            protocol::verify(
                &self.key,
                &protocol::reply_input(nonce, status, &reply.body),
                sig,
            )
        });
        if status == 200 {
            if !signed {
                return Err(ClientError::BadReply(
                    "The answer wasn't signed with this server's helper key, so it was ignored."
                        .into(),
                ));
            }
            return serde_json::from_slice(&reply.body).map_err(|_| {
                ClientError::BadReply("The helper's answer couldn't be read.".into())
            });
        }
        // A refusal before the helper knew who was asking (a bad signature, an old request)
        // can't be signed. Its words are still worth showing; it changes nothing.
        let ErrorReply { error, message } = serde_json::from_slice(&reply.body).map_err(|_| {
            ClientError::BadReply(format!("The helper answered {status} without saying why."))
        })?;
        Err(ClientError::Refused {
            status,
            code: error,
            message,
        })
    }
}

fn exchange(
    stream: &mut (impl Read + Write),
    op: Operation,
    headers: &[(&str, String)],
    body: &[u8],
) -> Result<http::Message, ClientError> {
    http::write_request(stream, op.path(), headers, body).map_err(|e| {
        ClientError::Unreachable(format!("The request to the host helper broke off ({e})."))
    })?;
    http::read_message(stream, MAX_REPLY, true).map_err(|e| match e {
        ReadError::Io(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
            ClientError::Unreachable(format!(
                "The host helper didn't answer in time. The {} may still be running there; check again in a minute.",
                op.name()
            ))
        }
        ReadError::Io(e) => {
            ClientError::Unreachable(format!("The host helper's answer broke off ({e})."))
        }
        ReadError::Bad(_) | ReadError::TooLarge => {
            ClientError::BadReply("The helper's answer was garbled.".into())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        assert_eq!(
            Endpoint::parse("http://host-helper:7480").unwrap(),
            Endpoint::Tcp("host-helper:7480".into())
        );
        assert_eq!(
            Endpoint::parse("http://10.0.0.2:7480/").unwrap(),
            Endpoint::Tcp("10.0.0.2:7480".into())
        );
        assert_eq!(
            Endpoint::parse("unix:/run/host-helper/helper.sock").unwrap(),
            Endpoint::Unix("/run/host-helper/helper.sock".into())
        );
        for bad in [
            "https://host-helper:7480",
            "http://host-helper",
            "http://host-helper:99999",
            "http://user@host:1",
            "http://host:1/path",
            "unix:relative.sock",
            "host-helper:7480",
        ] {
            assert!(Endpoint::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn settings_come_from_three_variables() {
        let key = Key::generate().unwrap().to_hex();
        let vars = |url: Option<&str>, stack: Option<&str>, k: Option<&str>| {
            let (url, stack, k) = (
                url.map(String::from),
                stack.map(String::from),
                k.map(String::from),
            );
            move |name: &str| match name {
                ENV_URL => url.clone(),
                ENV_STACK => stack.clone(),
                ENV_KEY => k.clone(),
                _ => None,
            }
        };
        let c = Client::from_vars(vars(
            Some("http://host-helper:7480"),
            Some("lockdown"),
            Some(&key),
        ))
        .unwrap();
        assert_eq!(c.stack(), "lockdown");
        let missing = Client::from_vars(vars(None, Some("lockdown"), Some(&key))).unwrap_err();
        assert!(missing.to_string().contains("HOST_HELPER_URL isn't set"));
        let bad_key =
            Client::from_vars(vars(Some("http://h:1"), Some("lockdown"), Some("abc"))).unwrap_err();
        assert!(bad_key.to_string().contains("isn't a helper key"));
        // The key never shows in an error.
        let bad_stack =
            Client::from_vars(vars(Some("http://h:1"), Some("Lock down"), Some(&key))).unwrap_err();
        assert!(!bad_stack.to_string().contains(&key));
        assert!(!format!("{c:?}").contains(&key));
    }
}
