//! Just enough HTTP/1.1 for one request and one reply per connection, on both sides. Strict on
//! purpose: a fixed size limit for the head and the body, `Content-Length` only (no chunked
//! bodies), and the connection closes after the reply.

use std::io::{self, Read, Write};

/// The largest request or reply head (the first line and the headers).
pub const MAX_HEAD: usize = 8 * 1024;

/// A parsed message: the first line's parts, the headers (names lower case) and the body.
#[derive(Debug, PartialEq, Eq)]
pub struct Message {
    /// `POST /v1/status HTTP/1.1` or `HTTP/1.1 200 OK`, split on spaces (at most three parts).
    pub start: Vec<String>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Message {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug)]
pub enum ReadError {
    Io(io::Error),
    /// The message isn't one this reader accepts, in plain words.
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    Bad(&'static str),
    TooLarge,
}

impl From<io::Error> for ReadError {
    fn from(e: io::Error) -> Self {
        ReadError::Io(e)
    }
}

/// Reads one message. A body longer than `max_body` is refused before it is read.
/// With `body_to_eof` (a reply without `Content-Length`), the body runs to the end of the stream.
pub fn read_message(
    stream: &mut impl Read,
    max_body: usize,
    body_to_eof: bool,
) -> Result<Message, ReadError> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err(ReadError::TooLarge);
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(ReadError::Bad(
                "the connection closed before the message ended",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    if head_end > MAX_HEAD {
        return Err(ReadError::TooLarge);
    }
    let head =
        std::str::from_utf8(&buf[..head_end]).map_err(|_| ReadError::Bad("the head isn't text"))?;
    let mut lines = head.split("\r\n");
    let start: Vec<String> = lines
        .next()
        .unwrap_or_default()
        .splitn(3, ' ')
        .map(str::to_owned)
        .collect();
    if start.len() < 2 {
        return Err(ReadError::Bad("the first line is malformed"));
    }
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or(ReadError::Bad("a header line is malformed"))?;
        if name.is_empty()
            || name
                .bytes()
                .any(|b| !(b.is_ascii_alphanumeric() || b == b'-'))
        {
            return Err(ReadError::Bad("a header name is malformed"));
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let mut msg = Message {
        start,
        headers,
        body: Vec::new(),
    };
    if msg.header("transfer-encoding").is_some() {
        return Err(ReadError::Bad("chunked bodies aren't accepted"));
    }
    if msg
        .headers
        .iter()
        .filter(|(n, _)| n == "content-length")
        .count()
        > 1
    {
        return Err(ReadError::Bad("Content-Length is given twice"));
    }
    let rest = &buf[head_end + 4..];
    match msg.header("content-length") {
        Some(len) => {
            let len: usize = len
                .parse()
                .map_err(|_| ReadError::Bad("Content-Length isn't a number"))?;
            if len > max_body {
                return Err(ReadError::TooLarge);
            }
            if rest.len() > len {
                return Err(ReadError::Bad("more bytes came than Content-Length says"));
            }
            let mut body = rest.to_vec();
            body.resize(len, 0);
            stream.read_exact(&mut body[rest.len()..])?;
            msg.body = body;
        }
        None if body_to_eof => {
            let mut body = rest.to_vec();
            stream
                .take((max_body + 1 - body.len().min(max_body)) as u64)
                .read_to_end(&mut body)?;
            if body.len() > max_body {
                return Err(ReadError::TooLarge);
            }
            msg.body = body;
        }
        None if !rest.is_empty() => {
            return Err(ReadError::Bad("a body came without Content-Length"));
        }
        None => {}
    }
    Ok(msg)
}

/// Writes one request.
pub fn write_request(
    stream: &mut impl Write,
    path: &str,
    headers: &[(&str, String)],
    body: &[u8],
) -> io::Result<()> {
    let mut head = format!(
        "POST {path} HTTP/1.1\r\nhost: host-helper\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Writes one reply.
#[cfg(feature = "server")]
pub fn write_reply(
    stream: &mut impl Write,
    status: u16,
    headers: &[(&str, String)],
    body: &[u8],
) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
        reason(status),
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(feature = "server")]
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &[u8]) -> Result<Message, ReadError> {
        read_message(&mut &text[..], 64, false)
    }

    #[test]
    fn a_request_round_trips() {
        let mut out = Vec::new();
        write_request(
            &mut out,
            "/v1/status",
            &[("x-helper-stack", "lockdown".into())],
            b"{}",
        )
        .unwrap();
        let msg = read(&out).unwrap();
        assert_eq!(msg.start, ["POST", "/v1/status", "HTTP/1.1"]);
        assert_eq!(msg.header("x-helper-stack"), Some("lockdown"));
        assert_eq!(msg.body, b"{}");
    }

    #[test]
    fn strict_about_bodies_and_heads() {
        assert!(matches!(
            read(b"POST / HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n"),
            Err(ReadError::Bad(_))
        ));
        assert!(matches!(
            read(b"POST / HTTP/1.1\r\ncontent-length: 65\r\n\r\n"),
            Err(ReadError::TooLarge)
        ));
        assert!(matches!(
            read(b"POST / HTTP/1.1\r\ncontent-length: 1\r\ncontent-length: 1\r\n\r\nx"),
            Err(ReadError::Bad(_))
        ));
        assert!(matches!(
            read(b"POST / HTTP/1.1\r\n\r\nbody"),
            Err(ReadError::Bad(_))
        ));
        assert!(matches!(
            read(b"POST / HTTP/1.1\r\nbad header\r\n\r\n"),
            Err(ReadError::Bad(_))
        ));
        let mut long = b"POST / HTTP/1.1\r\nx: ".to_vec();
        long.extend(std::iter::repeat_n(b'a', MAX_HEAD + 10));
        assert!(matches!(read(&long), Err(ReadError::TooLarge)));
        assert!(matches!(
            read(b"POST / HTTP/1.1\r\n"),
            Err(ReadError::Bad(_))
        ));
    }

    #[test]
    fn a_reply_body_may_run_to_the_end() {
        let msg = read_message(&mut &b"HTTP/1.1 200 OK\r\n\r\n{\"a\":1}"[..], 64, true).unwrap();
        assert_eq!(msg.start[1], "200");
        assert_eq!(msg.body, b"{\"a\":1}");
        assert!(matches!(
            read_message(&mut &b"HTTP/1.1 200 OK\r\n\r\n0123456789"[..], 4, true),
            Err(ReadError::TooLarge)
        ));
    }
}
