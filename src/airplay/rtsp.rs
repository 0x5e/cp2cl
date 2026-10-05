//! RTSP request/response plumbing.
//!
//! CarPlay's control channel is HTTP-shaped but not HTTP: there is no SDP and
//! no DESCRIBE. What the phone actually sends is a short list of methods, and
//! anything unrecognised should be answered `200 OK` rather than `501`,
//! because the phone treats an unknown-but-successful status as "carry on".

use std::collections::HashMap;

pub const PLIST_CONTENT_TYPE: &str = "application/x-apple-binary-plist";
pub const PAIRING_CONTENT_TYPE: &str = "application/pairing+tlv8";
pub const OCTET_CONTENT_TYPE: &str = "application/octet-stream";

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub version: String,
    /// Lower-cased keys, original values.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn cseq(&self) -> u32 {
        self.headers
            .get("cseq")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn content_length(&self) -> usize {
        self.headers
            .get("content-length")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("empty request line")]
    Empty,
    #[error("malformed request line: {0}")]
    Line(String),
    #[error("bad header: {0}")]
    Header(String),
    #[error("short body: want {want}, have {have}")]
    ShortBody { want: usize, have: usize },
}

/// Splits the bytes that have arrived so far into (request, consumed).
///
/// `None` means "keep reading". The interesting case is a body that has not
/// fully arrived yet, which is the normal state on a TCP stream.
pub fn parse(buf: &[u8]) -> Result<Option<(Request, usize)>, ParseError> {
    let Some(end) = find_header_end(buf) else {
        return Ok(None);
    };
    let head = &buf[..end];
    let text = std::str::from_utf8(head).map_err(|_| ParseError::Line("not utf-8".into()))?;
    let mut lines = text.split("\r\n");

    let request_line = lines.next().unwrap_or("");
    if request_line.trim().is_empty() {
        return Err(ParseError::Empty);
    }
    let mut parts = request_line.split_whitespace();
    let (method, path, version) = {
        let m = parts
            .next()
            .ok_or_else(|| ParseError::Line(request_line.into()))?;
        let p = parts
            .next()
            .ok_or_else(|| ParseError::Line(request_line.into()))?;
        let v = parts.next().unwrap_or("RTSP/1.0");
        (m.to_string(), p.to_string(), v.to_string())
    };

    let mut headers: HashMap<String, String> = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            return Err(ParseError::Header(line.into()));
        };
        headers.insert(k.trim().to_lowercase(), v.trim().to_string());
    }

    let body_start = end + 4;
    let want = headers
        .get("content-length")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let have = buf.len().saturating_sub(body_start);
    if have < want {
        return Ok(None);
    }

    let body = buf[body_start..body_start + want].to_vec();
    Ok(Some((
        Request {
            method,
            path,
            version,
            headers,
            body,
        },
        body_start + want,
    )))
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub reason: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16) -> Self {
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "Unknown",
        };
        Self {
            status,
            reason,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }

    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    pub fn plist(mut self, body: Vec<u8>) -> Self {
        self.headers
            .push(("Content-Type".into(), PLIST_CONTENT_TYPE.into()));
        self.headers
            .push(("Content-Length".into(), body.len().to_string()));
        self.body = body;
        self
    }

    /// The response carries its own Content-Length, except when the caller
    /// built the headers already (the pairing handlers do).
    pub fn encode(&self, cseq: u32) -> Vec<u8> {
        let mut out = format!("RTSP/1.0 {} {}\r\n", self.status, self.reason).into_bytes();
        let mut has_len = false;
        let mut has_server = false;
        for (k, v) in &self.headers {
            if k.eq_ignore_ascii_case("content-length") {
                has_len = true;
            }
            if k.eq_ignore_ascii_case("server") {
                has_server = true;
            }
            out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
        }
        if cseq > 0 {
            out.extend_from_slice(format!("CSeq: {cseq}\r\n").as_bytes());
        }
        if !has_server {
            out.extend_from_slice(b"Server: AirTunes/320.20\r\n");
        }
        if !has_len {
            out.extend_from_slice(format!("Content-Length: {}\r\n", self.body.len()).as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.body);
        out
    }
}
