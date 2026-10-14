//! The Erasmus bridge over a real loopback socket.
//!
//! The unit tests in `erasmus::{mod,mcp}` cover routing, auth and the JSON-RPC dispatch against
//! in-memory values. This covers the part only a socket can: that a real HTTP/1.1 request on the
//! wire produces a well-framed reply a real client can read to the end and parse.
//!
//! That framing is the thing worth testing on a socket, because getting it wrong is how the
//! sibling integration hung every one of its callers — a response with no `Content-Length`, no
//! chunked encoding and no `Connection: close` from a server speaking HTTP/1.1 has no terminator,
//! and a keep-alive client waits out its own timeout on every call with nothing to show for it.
//! The checklist in `../Erasmus/Skills/DESKTOP_MARKETPLACE_SKILLS.md` asks for the live endpoint
//! to be read by eye against the schema rather than only through our own validator; this is that
//! check, automated.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use photocraft_automation::PhotocraftMcp;
use photocraft_automation::erasmus::mcp::McpDispatch;
use photocraft_automation::erasmus::{AUTH_HEADER, Bridge, HealthDetail, serve_connection};
use serde_json::Value;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// A bridge on an ephemeral loopback port, serving until the test drops it.
struct Server {
    port: u16,
    _runtime: tokio::runtime::Runtime,
}

fn start() -> Server {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("runtime");
    let dispatch = McpDispatch::new(PhotocraftMcp::headless(), runtime.handle().clone(), HealthDetail::headless);
    let bridge = Arc::new(Bridge::new(TOKEN, false, dispatch));
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let bridge = Arc::clone(&bridge);
            std::thread::spawn(move || serve_connection(&stream, &bridge));
        }
    });
    Server { port, _runtime: runtime }
}

/// One request, read to end-of-stream. A reply that never terminates hangs here, which is the
/// failure these tests exist to catch.
fn send(port: u16, request: &str) -> (u16, Vec<(String, String)>, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.set_read_timeout(Some(std::time::Duration::from_secs(20))).expect("timeout");
    stream.write_all(request.as_bytes()).expect("write");
    stream.flush().expect("flush");

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).expect("status line");
    let status: u16 = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or_else(|| panic!("no status in {status_line:?}"));

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("header");
        let line = line.trim_end().to_owned();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap_or_else(|| panic!("malformed header {line:?}"));
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let mut body = Vec::new();
    reader.read_to_end(&mut body).expect("body to end of stream");
    (status, headers, String::from_utf8(body).expect("utf-8 body"))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn get(port: u16, path: &str, token: Option<&str>) -> (u16, Vec<(String, String)>, String) {
    let auth = token.map(|t| format!("{AUTH_HEADER}: {t}\r\n")).unwrap_or_default();
    send(port, &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}\r\n"))
}

/// A POST shaped exactly as Erasmus sends one, `Accept` included.
fn post_mcp(port: u16, body: &str) -> (u16, Vec<(String, String)>, String) {
    send(
        port,
        &format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\n{AUTH_HEADER}: {TOKEN}\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
}

#[test]
fn health_answers_on_the_wire_without_a_credential() {
    let s = start();
    let (status, headers, body) = get(s.port, "/health", None);
    assert_eq!(status, 200);
    assert_eq!(header(&headers, "content-type"), Some("application/json"));
    assert_eq!(header(&headers, "connection"), Some("close"));
    assert!(header(&headers, "content-length").is_some(), "a reply with no length has no terminator");
    let parsed: Value = serde_json::from_str(&body).expect("JSON body");
    assert_eq!(parsed.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(parsed.get("program").and_then(Value::as_str), Some("photocraft"));
}

#[test]
fn every_other_route_is_401_on_the_wire_without_a_credential() {
    let s = start();
    for path in ["/tools", "/skills", "/actions", "/mcp", "/nope"] {
        let (status, _, _) = get(s.port, path, None);
        assert_eq!(status, 401, "{path}");
    }
}

/// The framing test that matters. Erasmus always sends `Accept: ..., text/event-stream`; a server
/// that branches on it serves an event stream for every request, and that is the shape that hung.
#[test]
fn a_tool_call_answers_plain_json_even_though_the_client_accepts_an_event_stream() {
    let s = start();
    let (status, headers, body) = post_mcp(s.port, r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"session_list","arguments":{}}}"#);
    assert_eq!(status, 200);
    assert_eq!(header(&headers, "content-type"), Some("application/json"), "Accept must not steer us into a stream");
    assert_eq!(header(&headers, "content-length").map(|l| l.parse::<usize>().ok()), Some(Some(body.len())), "the declared length must be the real one");
    let parsed: Value = serde_json::from_str(&body).expect("JSON body");
    assert_eq!(parsed.get("id").and_then(Value::as_i64), Some(11));
    assert!(parsed.get("error").is_none(), "{parsed}");
    assert!(parsed.pointer("/result/content").is_some(), "{parsed}");
}

#[test]
fn a_document_created_over_the_wire_is_visible_to_the_next_request() {
    let s = start();
    let (status, _, body) =
        post_mcp(s.port, r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"doc_new","arguments":{"width":64,"height":48}}}"#);
    assert_eq!(status, 200);
    assert!(!body.contains("\"isError\":true"), "{body}");

    // A separate connection and a separate MCP session: the editing session must outlive both.
    let (_, _, body) = post_mcp(s.port, r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"doc_inspect","arguments":{}}}"#);
    assert!(body.contains("64"), "the document from the previous connection is gone: {body}");
}

#[test]
fn a_notification_is_202_with_an_empty_body() {
    let s = start();
    let (status, headers, body) = post_mcp(s.port, r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#);
    assert_eq!(status, 202);
    assert_eq!(header(&headers, "content-length"), Some("0"));
    assert!(body.is_empty());
}

#[test]
fn the_tools_route_serves_the_mcp_shape() {
    let s = start();
    let (status, _, body) = get(s.port, "/tools", Some(TOKEN));
    assert_eq!(status, 200);
    let parsed: Value = serde_json::from_str(&body).expect("JSON");
    let tools = parsed.get("tools").and_then(Value::as_array).expect("a tools array");
    assert!(tools.len() >= 25, "got {} tools", tools.len());
    for tool in tools {
        assert!(tool.get("name").and_then(Value::as_str).is_some_and(|n| !n.is_empty()), "{tool}");
        assert!(tool.get("inputSchema").is_some(), "the MCP spelling, every time: {tool}");
        assert!(tool.get("input_schema").is_none(), "the snake_case spelling reads as `no arguments`: {tool}");
    }
    assert!(tools.iter().any(|t| t.get("name").and_then(Value::as_str) == Some("open_file")), "open_file is what the manifest's `opens` promises");
}

/// Read against the shape the host's own zod schema accepts, field by field — the eyeball check
/// the publishing checklist asks for, as an assertion.
#[test]
fn the_skills_document_matches_the_hosts_schema() {
    let s = start();
    let (status, _, body) = get(s.port, "/skills", Some(TOKEN));
    assert_eq!(status, 200);
    let parsed: Value = serde_json::from_str(&body).expect("JSON");
    let skills = parsed.get("skills").and_then(Value::as_array).expect("a skills array");
    assert!(!skills.is_empty());
    assert!(skills.len() <= 50, "the schema caps the list at 50");
    let mut always = 0;
    for skill in skills {
        let name = skill.get("name").and_then(Value::as_str).expect("name");
        assert!(name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'), "{name}");
        assert!((1..=80).contains(&skill.get("title").and_then(Value::as_str).expect("title").chars().count()));
        assert!(skill.get("summary").and_then(Value::as_str).expect("summary").chars().count() <= 200);
        assert!((1..=4000).contains(&skill.get("body").and_then(Value::as_str).expect("body").chars().count()));
        for keyword in skill.get("keywords").and_then(Value::as_array).expect("keywords") {
            let keyword = keyword.as_str().expect("a string keyword");
            assert_eq!(keyword, keyword.to_lowercase(), "an uppercase keyword can never match what a user typed");
        }
        if skill.get("always").and_then(Value::as_bool) == Some(true) {
            always += 1;
        }
    }
    assert_eq!(always, 1, "exactly one universal skill");
}

#[test]
fn the_actions_document_declares_a_real_effect_for_a_real_tool() {
    let s = start();
    let (_, _, tools_body) = get(s.port, "/tools", Some(TOKEN));
    let tools: Value = serde_json::from_str(&tools_body).expect("JSON");
    let names: Vec<String> =
        tools.get("tools").and_then(Value::as_array).expect("array").iter().filter_map(|t| t.get("name").and_then(Value::as_str)).map(str::to_owned).collect();

    let (status, _, body) = get(s.port, "/actions", Some(TOKEN));
    assert_eq!(status, 200);
    let parsed: Value = serde_json::from_str(&body).expect("JSON");
    let actions = parsed.get("actions").and_then(Value::as_array).expect("an actions array");
    assert!(!actions.is_empty());
    let mut destructive = 0;
    for action in actions {
        let tool = action.get("tool").and_then(Value::as_str).expect("tool");
        // The host checks this at call time, after a person has clicked. Catch it here instead.
        assert!(names.contains(&tool.to_owned()), "action names `{tool}`, which is not in tools/list");
        let effect = action.get("effect").and_then(Value::as_str).expect("effect");
        assert!(matches!(effect, "read" | "modify" | "destructive"), "{effect}");
        // A fixed argument is merged under the caller's and never validated, so it constrains
        // nothing; nothing we publish may lean on one.
        assert_eq!(action.get("arguments"), Some(&serde_json::json!({})), "{action}");
        if effect == "destructive" {
            destructive += 1;
            assert_eq!(action.get("confirm").and_then(Value::as_bool), Some(true), "a destructive action must confirm: {action}");
        }
    }
    assert!(destructive >= 2, "saving over a file and closing a document both lose work");
    assert!(
        actions.iter().any(|a| a.get("effect").and_then(Value::as_str) == Some("modify") && a.get("confirm").and_then(Value::as_bool) == Some(false)),
        "an assistant needs at least one automatable way to act"
    );
}

#[test]
fn a_malformed_request_is_answered_rather_than_dropped() {
    let s = start();
    // Not an HTTP request line at all: one token, no target and no version.
    let (status, _, _) = send(s.port, "GARBAGE\r\n\r\n");
    assert_eq!(status, 400, "a malformed request line is answered, not a dropped socket");

    // A real method we do not serve.
    let (status, _, _) = send(s.port, "DELETE /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
    assert_eq!(status, 405);

    let (status, _, body) = post_mcp(s.port, "{not json");
    assert_eq!(status, 200, "a JSON-RPC parse error is a JSON-RPC answer, not an HTTP failure");
    let parsed: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(parsed.pointer("/error/code").and_then(Value::as_i64), Some(-32700));
}

#[test]
fn an_oversized_body_is_refused_with_a_status() {
    let s = start();
    let (status, _, _) = send(s.port, &format!("POST /mcp HTTP/1.1\r\n{AUTH_HEADER}: {TOKEN}\r\nContent-Length: 99999999\r\n\r\n"));
    assert_eq!(status, 413);
}

#[test]
fn no_cors_header_is_ever_served() {
    let s = start();
    for path in ["/health", "/tools", "/skills", "/actions"] {
        let (_, headers, _) = get(s.port, path, Some(TOKEN));
        assert!(header(&headers, "access-control-allow-origin").is_none(), "{path} must not be reachable from a page the user visits");
    }
}
