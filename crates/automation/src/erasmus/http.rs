//! A minimal HTTP/1.1 request reader and response writer for the Erasmus bridge.
//!
//! The bridge is six routes on loopback, answering plain JSON (never an event stream — see
//! `docs/erasmus.md` §1 for why a stream that outlives its own answer is indistinguishable from a
//! hang). That does not need a web framework, and a hand-written reader is the smaller risk here:
//! every length in an HTTP request is attacker-controlled, and the whole of it fits in one file
//! that can be read in an afternoon and fuzzed in a loop.
//!
//! Everything is bounded before it is allocated: the request line, the header block, the header
//! count and the body. A request that exceeds any of them is refused with a status rather than
//! read to completion, because the alternative is a 1 MiB header block costing a megabyte of
//! `String` per connection on a server that allows sixteen of them.
//!
//! Responses close the connection. Keep-alive would save a syscall on a health poll every 1.5
//! seconds and cost connection state in exchange; the health route's own job is to be cheap and
//! stateless, and `Connection: close` is how it stays that way.

use std::io::{BufRead, Write};

/// The methods the bridge answers. Anything else is a 405, which is a shorter conversation than
/// pretending to understand it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// The longest request line (method, target, version) accepted. A bridge target is `/actions` or
/// `/mcp?token=…`; anything near this is either a mistake or a probe.
pub const MAX_REQUEST_LINE: usize = 8 * 1024;

/// The longest single header line accepted.
pub const MAX_HEADER_LINE: usize = 8 * 1024;

/// The most headers accepted. A real client sends a handful; this is generous for a proxy that
/// adds its own and still refuses an unbounded block.
pub const MAX_HEADERS: usize = 64;

/// Why a request could not be read. Each maps to the status the caller answers with, so a
/// malformed request still gets a reply rather than a dropped socket: a client that cannot tell
/// "refused" from "crashed" retries, and a retry loop against a parser is the thing to avoid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The peer closed before sending anything. Not an error worth a reply.
    Closed,
    /// Syntactically not an HTTP request. 400.
    Malformed(&'static str),
    /// A known method we do not serve. 405.
    MethodNotAllowed,
    /// Over one of the limits above. 413.
    TooLarge(&'static str),
    /// The socket failed. No reply is possible.
    Io(String),
}

/// One parsed request. `path` is the target with any query string removed; `query` keeps it,
/// because `allowTokenInQuery` lets a client that cannot set a header pass `?token=` instead.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub query: Option<String>,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    /// The first value of `name`, matched case-insensitively (RFC 9110 §5.1 field names are
    /// case-insensitive, and a client that sends `x-photocraft-token` is not wrong).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The value of `key` in the query string, with no percent-decoding: the only parameter the
    /// bridge reads is a token, and a token that needed escaping would not survive the round trip
    /// anyway. Decoding here would be a second, subtly different unescaper to keep correct.
    pub fn query_param(&self, key: &str) -> Option<&str> {
        self.query.as_deref()?.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then_some(v)
        })
    }

    /// Read one request. `max_body` bounds `Content-Length` before anything is allocated for it.
    pub fn read(reader: &mut impl BufRead, max_body: usize) -> Result<Request, ReadError> {
        let line = read_line(reader, MAX_REQUEST_LINE, "request line")?;
        if line.is_empty() {
            return Err(ReadError::Closed);
        }
        let mut parts = line.split(' ');
        let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(ReadError::Malformed("request line needs method, target and version"));
        };
        if parts.next().is_some() {
            return Err(ReadError::Malformed("request line has trailing data"));
        }
        if !version.starts_with("HTTP/1.") {
            return Err(ReadError::Malformed("only HTTP/1.x is served"));
        }
        let method = match method {
            "GET" => Method::Get,
            "POST" => Method::Post,
            // A known-but-unserved method and an unparseable one are the same answer here, and
            // 405 is the more useful of the two to a client that sent PUT by mistake.
            _ => return Err(ReadError::MethodNotAllowed),
        };
        if !target.starts_with('/') {
            // Absolute-form targets are legal to a proxy and meaningless to us.
            return Err(ReadError::Malformed("request target must be an origin-form path"));
        }
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_owned(), Some(q.to_owned())),
            None => (target.to_owned(), None),
        };

        let mut headers: Vec<(String, String)> = Vec::new();
        loop {
            let line = read_line(reader, MAX_HEADER_LINE, "header")?;
            if line.is_empty() {
                break;
            }
            if headers.len() >= MAX_HEADERS {
                return Err(ReadError::TooLarge("too many headers"));
            }
            let Some((name, value)) = line.split_once(':') else {
                return Err(ReadError::Malformed("header without a colon"));
            };
            // A space before the colon is a request-smuggling shape, not a formatting quirk.
            if name.is_empty() || name.ends_with(' ') || name.ends_with('\t') {
                return Err(ReadError::Malformed("malformed header name"));
            }
            headers.push((name.to_owned(), value.trim().to_owned()));
        }

        let length = match headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")) {
            None => 0,
            Some((_, raw)) => raw.trim().parse::<usize>().map_err(|_| ReadError::Malformed("Content-Length is not a number"))?,
        };
        if length > max_body {
            return Err(ReadError::TooLarge("body too large"));
        }
        // `Transfer-Encoding` is refused rather than implemented: a second framing is a second
        // way to disagree with Content-Length about where a request ends, and no client of this
        // bridge needs one.
        if headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding")) {
            return Err(ReadError::Malformed("Transfer-Encoding is not served; use Content-Length"));
        }
        let mut body = vec![0u8; length];
        if length > 0 {
            reader.read_exact(&mut body).map_err(|e| ReadError::Io(e.to_string()))?;
        }
        Ok(Request { method, path, query, headers, body })
    }
}

/// Read one CRLF- or LF-terminated line, without the terminator, refusing one over `max`.
///
/// `BufRead::read_line` would grow without a limit, so this consumes the buffer itself and stops
/// at the cap. The partial line is dropped: a prefix of a too-long header is not something to
/// make decisions from.
fn read_line(reader: &mut impl BufRead, max: usize, what: &'static str) -> Result<String, ReadError> {
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(|e| ReadError::Io(e.to_string()))?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(String::new());
            }
            return Err(ReadError::Malformed("stream ended mid-line"));
        }
        let newline = available.iter().position(|&b| b == b'\n');
        let take = newline.map_or(available.len(), |at| at + 1);
        if take > max.saturating_sub(bytes.len()) {
            return Err(ReadError::TooLarge(what));
        }
        bytes.extend_from_slice(available.get(..take).unwrap_or_default());
        reader.consume(take);
        if newline.is_some() {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            return String::from_utf8(bytes).map_err(|_| ReadError::Malformed("line is not UTF-8"));
        }
    }
}

/// One response, written and then the connection closed.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    /// A JSON body. The caller has already serialized it, because a serialization failure is the
    /// caller's to report as a 500 rather than something to paper over here.
    pub fn json(status: u16, body: impl Into<Vec<u8>>) -> Response {
        Response { status, content_type: "application/json", body: body.into() }
    }

    /// `202 Accepted`, no body: what a JSON-RPC notification gets.
    pub fn accepted() -> Response {
        Response { status: 202, content_type: "application/json", body: Vec::new() }
    }

    /// A `text/plain` body, for a status with nothing structured to say.
    pub fn text(status: u16, body: impl Into<Vec<u8>>) -> Response {
        Response { status, content_type: "text/plain; charset=utf-8", body: body.into() }
    }

    /// A markdown body, for the flat `/skill` document.
    pub fn markdown(body: impl Into<Vec<u8>>) -> Response {
        Response { status: 200, content_type: "text/markdown; charset=utf-8", body: body.into() }
    }

    pub fn write_to(&self, out: &mut impl Write) -> std::io::Result<()> {
        let reason = reason_phrase(self.status);
        // No CORS headers, deliberately: the bridge is loopback-only and nothing in a browser is
        // meant to reach it. Adding `Access-Control-Allow-Origin` would make any page the user
        // visits a client of their own editor.
        write!(
            out,
            "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            self.status,
            self.content_type,
            self.body.len()
        )?;
        out.write_all(&self.body)?;
        out.flush()
    }
}

/// The statuses this bridge actually sends. An unknown one is still a valid response line, so an
/// added status that nobody remembered to list here degrades to a vague reason phrase rather than
/// to a panic.
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

/// The status a read failure is answered with. `Closed` and `Io` have no reply.
pub fn status_for(error: &ReadError) -> Option<(u16, String)> {
    match error {
        ReadError::Closed | ReadError::Io(_) => None,
        ReadError::Malformed(why) => Some((400, (*why).to_owned())),
        ReadError::MethodNotAllowed => Some((405, "only GET and POST are served".to_owned())),
        ReadError::TooLarge(what) => Some((413, format!("{what} is too large"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn parse(raw: &str) -> Result<Request, ReadError> {
        Request::read(&mut BufReader::new(raw.as_bytes()), 1024)
    }

    #[test]
    fn reads_a_get_with_headers() {
        let r = parse("GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nX-PhotoCraft-Token: abc\r\n\r\n").expect("parses");
        assert_eq!(r.method, Method::Get);
        assert_eq!(r.path, "/health");
        assert_eq!(r.query, None);
        assert_eq!(r.header("x-photocraft-token"), Some("abc"), "header lookup is case-insensitive");
        assert!(r.body.is_empty());
    }

    #[test]
    fn reads_a_post_body_of_exactly_content_length() {
        let r = parse("POST /mcp HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}trailing").expect("parses");
        assert_eq!(r.method, Method::Post);
        assert_eq!(r.body, b"{}", "only Content-Length bytes are the body");
    }

    #[test]
    fn splits_the_query_string_off_the_path() {
        let r = parse("GET /mcp?token=deadbeef&x=1 HTTP/1.1\r\n\r\n").expect("parses");
        assert_eq!(r.path, "/mcp");
        assert_eq!(r.query_param("token"), Some("deadbeef"));
        assert_eq!(r.query_param("x"), Some("1"));
        assert_eq!(r.query_param("missing"), None);
    }

    #[test]
    fn accepts_bare_lf_line_endings() {
        let r = parse("GET /tools HTTP/1.1\nAccept: application/json\n\n").expect("parses");
        assert_eq!(r.path, "/tools");
        assert_eq!(r.header("accept"), Some("application/json"));
    }

    #[test]
    fn empty_stream_is_closed_not_malformed() {
        assert_eq!(parse("").expect_err("a closed stream is not a request"), ReadError::Closed);
    }

    #[test]
    fn refuses_a_body_over_the_cap_without_reading_it() {
        let err = parse("POST /mcp HTTP/1.1\r\nContent-Length: 99999\r\n\r\n").expect_err("refused");
        assert_eq!(err, ReadError::TooLarge("body too large"));
    }

    #[test]
    fn refuses_an_oversized_header_block() {
        let long = "x".repeat(MAX_HEADER_LINE + 1);
        let err = parse(&format!("GET / HTTP/1.1\r\nX-Big: {long}\r\n\r\n")).expect_err("refused");
        assert_eq!(err, ReadError::TooLarge("header"));
    }

    #[test]
    fn refuses_too_many_headers() {
        let block: String = (0..=MAX_HEADERS).map(|i| format!("X-H{i}: v\r\n")).collect();
        let err = parse(&format!("GET / HTTP/1.1\r\n{block}\r\n")).expect_err("refused");
        assert_eq!(err, ReadError::TooLarge("too many headers"));
    }

    #[test]
    fn refuses_transfer_encoding_rather_than_guessing_the_framing() {
        let err = parse("POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n").expect_err("refused");
        assert!(matches!(err, ReadError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn refuses_a_space_before_the_header_colon() {
        let err = parse("GET / HTTP/1.1\r\nContent-Length : 0\r\n\r\n").expect_err("refused");
        assert_eq!(err, ReadError::Malformed("malformed header name"));
    }

    #[test]
    fn refuses_other_methods_with_405() {
        let err = parse("DELETE /mcp HTTP/1.1\r\n\r\n").expect_err("refused");
        assert_eq!(err, ReadError::MethodNotAllowed);
        assert_eq!(status_for(&err).map(|(s, _)| s), Some(405));
    }

    #[test]
    fn refuses_a_non_origin_form_target() {
        let err = parse("GET http://example.com/mcp HTTP/1.1\r\n\r\n").expect_err("refused");
        assert!(matches!(err, ReadError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn refuses_a_non_numeric_content_length() {
        let err = parse("POST /mcp HTTP/1.1\r\nContent-Length: twelve\r\n\r\n").expect_err("refused");
        assert!(matches!(err, ReadError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn a_truncated_line_is_malformed_not_a_partial_request() {
        let err = parse("GET /health HTTP/1.1\r\nHost: 127.0").expect_err("refused");
        assert_eq!(err, ReadError::Malformed("stream ended mid-line"));
    }

    #[test]
    fn writes_a_json_response_with_close_and_no_store() {
        let mut out = Vec::new();
        Response::json(200, b"{\"ok\":true}".to_vec()).write_to(&mut out).expect("writes");
        let text = String::from_utf8(out).expect("utf-8");
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(text.contains("Content-Type: application/json\r\n"), "{text}");
        assert!(text.contains("Content-Length: 11\r\n"), "{text}");
        assert!(text.contains("Connection: close\r\n"), "{text}");
        assert!(text.contains("Cache-Control: no-store\r\n"), "{text}");
        assert!(text.ends_with("\r\n\r\n{\"ok\":true}"), "{text}");
    }

    #[test]
    fn a_notification_is_202_with_no_body() {
        let mut out = Vec::new();
        Response::accepted().write_to(&mut out).expect("writes");
        let text = String::from_utf8(out).expect("utf-8");
        assert!(text.starts_with("HTTP/1.1 202 Accepted\r\n"), "{text}");
        assert!(text.contains("Content-Length: 0\r\n"), "{text}");
    }

    #[test]
    fn no_cors_header_is_ever_sent() {
        let mut out = Vec::new();
        Response::text(401, "nope").write_to(&mut out).expect("writes");
        let text = String::from_utf8(out).expect("utf-8");
        assert!(!text.to_ascii_lowercase().contains("access-control-allow-origin"), "{text}");
    }
}
