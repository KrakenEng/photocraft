//! `POST /mcp`: JSON-RPC 2.0 over HTTP, answered by the same [`PhotocraftMcp`] the stdio server
//! uses.
//!
//! ## Why each request gets its own short-lived MCP session
//!
//! rmcp's tool dispatch takes a `ToolCallContext`, which needs a `RequestContext`, which needs a
//! `Peer` — and `Peer::new` is `pub(crate)`. There is no supported way to call a tool without a
//! live service, so hand-rolling `tools/call` against the `ToolRouter` is not an option. The other
//! direction — one long-lived service with an id-multiplexer in front of it — means owning request
//! routing, id rewriting and a reader task, and getting any of that subtly wrong produces crossed
//! replies, which is a worse failure than anything it would save.
//!
//! So: for every request, prime a service over an in-memory duplex with `initialize` and
//! `notifications/initialized`, write the caller's own frame after them, close the input, and read
//! the reply back out. rmcp owns the protocol — capabilities, tool schemas, error codes, argument
//! validation — and this module owns only the framing. The cost is an in-process handshake over
//! memory pipes per call, which is nothing next to the work PhotoCraft's tools actually do (a
//! 36 MP save is seconds).
//!
//! This also answers a requirement that is easy to miss: **a tool call arrives with no prior
//! handshake on that connection.** Erasmus discovers tools over `GET /tools` when a manifest
//! declares `toolsPath` (ours does), so `initialize` may never be sent at all, and `tools/call`
//! goes straight to a fresh connection (`../Erasmus/electron/main/bridge/client.ts`). A server
//! holding initialization state per connection would reject exactly the calls that matter. Here
//! every request is self-contained, so the question does not arise.
//!
//! ## What this module deliberately does not do
//!
//! It never looks at `Accept`. Erasmus sends `Accept: application/json, text/event-stream` on
//! every `POST /mcp`, so a server that branches on that header takes its streaming path for *every*
//! request — which is precisely how the sibling integration shipped a response with no terminator
//! and left callers blocked until their own timeout on every single call. Replies here are always
//! plain JSON. See `docs/erasmus.md` §3a.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{Dispatch, HealthDetail, MCP_PROTOCOL, Mcp};
use crate::server::PhotocraftMcp;

/// JSON-RPC error codes used here. The rest come from rmcp.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const INTERNAL_ERROR: i64 = -32603;

/// The duplex buffer for one session. Generous because a single reply can be large — a
/// `ui_screenshot` result is a base64 PNG — and a reply that does not fit would otherwise block
/// the service's write while this side is still writing its request.
const PIPE_BYTES: usize = 8 << 20;

/// The id of the priming `initialize`, which the caller never sees. A string no caller would
/// choose, so the reply it produces is unambiguous to skip even if a caller used id `1`.
const PRIMER_ID: &str = "__photocraft_bridge_init";

/// Serves the bridge's `/mcp` and `/tools` routes over a [`PhotocraftMcp`].
///
/// `health` is supplied as a closure rather than computed here: what counts as healthy is the
/// application's business (unsaved documents, a wedged UI thread), and this module has no way to
/// ask. The desktop app passes one that queries its UI thread within
/// [`super::HEALTH_PROBE_BUDGET`]; a headless caller passes one that reports
/// [`HealthDetail::headless`].
pub struct McpDispatch {
    server: PhotocraftMcp,
    health: Box<dyn Fn() -> HealthDetail + Send + Sync>,
    runtime: Arc<tokio::runtime::Handle>,
}

impl McpDispatch {
    pub fn new(server: PhotocraftMcp, runtime: tokio::runtime::Handle, health: impl Fn() -> HealthDetail + Send + Sync + 'static) -> McpDispatch {
        McpDispatch { server, health: Box::new(health), runtime: Arc::new(runtime) }
    }

    /// Run one JSON-RPC frame through a fresh session and return the matching reply.
    ///
    /// `None` means the frame produced no reply, which for a notification is correct and is
    /// answered with `202`.
    async fn exchange(&self, frame: &Value) -> Result<Option<Value>, String> {
        let is_notification = frame.get("id").is_none();
        let own_id = frame.get("id").cloned();
        // A caller sending `initialize` itself gets it forwarded alone; priming a session that is
        // about to be initialized again would be the one case rmcp could reasonably reject.
        let priming = frame.get("method").and_then(Value::as_str) != Some("initialize");

        let mut input = Vec::new();
        if priming {
            let init = json!({
                "jsonrpc": "2.0",
                "id": PRIMER_ID,
                "method": "initialize",
                "params": {
                    "protocolVersion": MCP_PROTOCOL,
                    "capabilities": {},
                    "clientInfo": { "name": "photocraft-erasmus-bridge", "version": env!("CARGO_PKG_VERSION") }
                }
            });
            input.extend_from_slice(init.to_string().as_bytes());
            input.push(b'\n');
            input.extend_from_slice(json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}).to_string().as_bytes());
            input.push(b'\n');
        }
        input.extend_from_slice(frame.to_string().as_bytes());
        input.push(b'\n');

        let (ours, theirs) = tokio::io::duplex(PIPE_BYTES);
        let (their_read, their_write) = tokio::io::split(theirs);
        let server = self.server.clone();
        let service = self.runtime.spawn(async move { server.serve_io(their_read, their_write).await });

        // Write and read concurrently, on split halves. Writing the whole request first and only
        // then reading would deadlock on a reply larger than the pipe buffer, which a screenshot
        // can be.
        let (mut our_read, mut our_write) = tokio::io::split(ours);
        let mut output = Vec::new();
        let write = async {
            our_write.write_all(&input).await?;
            // EOF on the input is what ends the service's read loop and so the session.
            our_write.shutdown().await
        };
        let (written, read) = tokio::join!(write, our_read.read_to_end(&mut output));
        written.map_err(|e| format!("could not write to the MCP session: {e}"))?;
        read.map_err(|e| format!("could not read from the MCP session: {e}"))?;

        match service.await {
            Ok(Ok(())) => {}
            // The service having errored does not mean there is no usable reply: it may have
            // answered and then failed on shutdown. Fall through and look.
            Ok(Err(e)) if output.is_empty() => return Err(format!("the MCP session failed: {e}")),
            Err(e) if output.is_empty() => return Err(format!("the MCP session panicked: {e}")),
            Ok(Err(_)) | Err(_) => {}
        }

        if is_notification {
            return Ok(None);
        }
        let want = own_id.unwrap_or(Value::Null);
        for line in output.split(|b| *b == b'\n') {
            let Ok(message) = serde_json::from_slice::<Value>(line) else { continue };
            // Skip the primer's own reply, and anything the service volunteered.
            if message.get("id") == Some(&want) {
                return Ok(Some(message));
            }
        }
        Err("the MCP session produced no reply for this request".to_owned())
    }
}

impl Dispatch for McpDispatch {
    fn health(&self) -> HealthDetail {
        (self.health)()
    }

    fn mcp(&self, body: &[u8]) -> Result<Mcp, String> {
        let frame: Value = match serde_json::from_slice(body) {
            Ok(frame) => frame,
            // A parse failure is a JSON-RPC answer, not an HTTP one: the caller gets -32700 with a
            // null id, which is what the specification asks for.
            Err(e) => return Ok(Mcp::Reply(rpc_error(Value::Null, PARSE_ERROR, &format!("parse error: {e}")))),
        };
        if !frame.is_object() {
            // Batches are legal JSON-RPC and nothing in this ecosystem sends them: Erasmus
            // stringifies a single object per request. Refusing one plainly beats half-supporting
            // it, and the error names the shape rather than the symptom.
            let why = if frame.is_array() { "a JSON-RPC batch is not served; send one request per POST" } else { "a JSON-RPC request must be an object" };
            return Ok(Mcp::Reply(rpc_error(Value::Null, INVALID_REQUEST, why)));
        }

        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        // `block_on` is correct here and not a shortcut: each connection is served on its own
        // thread (as the JSON-lines control server already does), so blocking it is blocking only
        // this request.
        let outcome = self.runtime.block_on(self.exchange(&frame));
        match outcome {
            Ok(Some(reply)) => Ok(Mcp::Reply(reply.to_string().into_bytes())),
            Ok(None) => Ok(Mcp::Notification),
            // A dispatch failure is reported as a JSON-RPC error carrying the caller's own id,
            // rather than an HTTP 500: the caller is waiting on a reply to a specific request, and
            // an error it can match to that request is more use than a transport-shaped one.
            Err(why) => Ok(Mcp::Reply(rpc_error(id, INTERNAL_ERROR, &why))),
        }
    }

    fn tools(&self) -> Result<Vec<u8>, String> {
        // Answered by running `tools/list` through the same path `/mcp` uses, so the two routes
        // serve identical bytes **by construction** rather than by two serializers that agree
        // today. Serializing the `ToolRouter`'s own `Vec<Tool>` here looked equivalent and was
        // not: the MCP response carries `title` and `annotations` that a direct serialization
        // omits, so the convenience route would have quietly described a different tool.
        //
        // That difference is the mild version of a diagnosed failure. One integration served
        // `input_schema` on this route and `inputSchema` over MCP; a caller checking the
        // MCP-correct key read every tool as taking no arguments, attempted a destructive call
        // with none, was refused for the argument it had not sent, and retried identically
        // forever. Two endpoints on one program must never disagree about its own tools.
        //
        // The cost is one in-process session per call, and Erasmus fetches this once per launch.
        let frame = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} });
        let reply = self.runtime.block_on(self.exchange(&frame))?.ok_or("tools/list produced no reply")?;
        let tools = reply.pointer("/result/tools").ok_or_else(|| {
            let detail = reply.pointer("/error/message").and_then(Value::as_str).unwrap_or("no tools in the reply");
            format!("could not list tools: {detail}")
        })?;
        serde_json::to_vec(&json!({ "tools": tools })).map_err(|e| format!("could not serialize the tool list: {e}"))
    }
}

/// A JSON-RPC error object as a serialized body.
fn rpc_error(id: Value, code: i64, message: &str) -> Vec<u8> {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string().into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A runtime plus a dispatch over it.
    ///
    /// The runtime is held alongside deliberately: `Dispatch::mcp` is synchronous and blocks on
    /// the handle, which panics if called from a runtime thread. The real caller is an HTTP
    /// connection thread, so the tests drive it the same way — from a plain test thread with the
    /// runtime kept alive beside it.
    struct Harness {
        _runtime: tokio::runtime::Runtime,
        dispatch: McpDispatch,
    }

    fn harness() -> Harness {
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("test runtime");
        let dispatch = McpDispatch::new(PhotocraftMcp::headless(), runtime.handle().clone(), HealthDetail::headless);
        Harness { _runtime: runtime, dispatch }
    }

    fn reply_of(outcome: Mcp) -> Value {
        match outcome {
            Mcp::Reply(body) => serde_json::from_slice(&body).expect("a JSON reply"),
            Mcp::Notification => panic!("expected a reply, got a notification"),
        }
    }

    #[test]
    fn initialize_is_answered_with_the_protocol_and_server_name() {
        let h = harness();
        let d = &h.dispatch;
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
        let reply = reply_of(d.mcp(body).expect("dispatches"));
        assert_eq!(reply.get("id").and_then(Value::as_i64), Some(1));
        let result = reply.get("result").expect("a result");
        assert_eq!(result.pointer("/serverInfo/name").and_then(Value::as_str), Some("photocraft"));
        assert!(result.get("capabilities").is_some(), "{result}");
    }

    #[test]
    fn tools_list_names_the_photocraft_tools() {
        let h = harness();
        let d = &h.dispatch;
        let reply = reply_of(d.mcp(br#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{}}"#).expect("dispatches"));
        assert_eq!(reply.get("id").and_then(Value::as_i64), Some(7), "the caller's own id comes back, not the primer's");
        let tools = reply.pointer("/result/tools").and_then(Value::as_array).cloned().expect("a tools array");
        let names: Vec<&str> = tools.iter().filter_map(|t| t.get("name").and_then(Value::as_str)).collect();
        for expected in ["session_list", "doc_new", "doc_inspect", "open_file"] {
            assert!(names.contains(&expected), "missing {expected}: {names:?}");
        }
    }

    /// The whole reason each request is self-contained: Erasmus discovers tools over `GET /tools`
    /// and then calls one on a fresh connection, with no handshake of its own.
    #[test]
    fn a_tool_call_works_with_no_initialize_from_the_caller() {
        let h = harness();
        let d = &h.dispatch;
        let body = br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"session_list","arguments":{}}}"#;
        let reply = reply_of(d.mcp(body).expect("dispatches"));
        assert_eq!(reply.get("id").and_then(Value::as_i64), Some(2));
        assert!(reply.get("error").is_none(), "{reply}");
        assert!(reply.pointer("/result/content").is_some(), "{reply}");
    }

    #[test]
    fn a_tool_call_that_edits_a_document_round_trips() {
        let h = harness();
        let d = &h.dispatch;
        let new = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"doc_new","arguments":{"width":32,"height":16}}}"#;
        let reply = reply_of(d.mcp(new).expect("dispatches"));
        assert_ne!(reply.pointer("/result/isError").and_then(Value::as_bool), Some(true), "{reply}");
        // A second request is a second session, and the document must still be there: the session
        // is a protocol session, not the editing session.
        let inspect = br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"doc_inspect","arguments":{}}}"#;
        let reply = reply_of(d.mcp(inspect).expect("dispatches"));
        let text = reply.to_string();
        assert!(text.contains("32"), "the document created by the previous request is gone: {text}");
    }

    #[test]
    fn a_failing_tool_is_an_mcp_error_result_not_a_transport_failure() {
        let h = harness();
        let d = &h.dispatch;
        let body = br#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"doc_open","arguments":{"path":"/nonexistent/nope.psd"}}}"#;
        let reply = reply_of(d.mcp(body).expect("dispatches"));
        assert_eq!(reply.get("id").and_then(Value::as_i64), Some(3));
        let failed = reply.pointer("/result/isError").and_then(Value::as_bool) == Some(true) || reply.get("error").is_some();
        assert!(failed, "opening a missing file must report a failure: {reply}");
    }

    #[test]
    fn an_unknown_tool_is_refused_rather_than_ignored() {
        let h = harness();
        let d = &h.dispatch;
        let body = br#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}"#;
        let reply = reply_of(d.mcp(body).expect("dispatches"));
        let failed = reply.get("error").is_some() || reply.pointer("/result/isError").and_then(Value::as_bool) == Some(true);
        assert!(failed, "{reply}");
    }

    #[test]
    fn a_notification_produces_no_reply() {
        let h = harness();
        let d = &h.dispatch;
        let outcome = d.mcp(br#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#).expect("dispatches");
        assert_eq!(outcome, Mcp::Notification);
    }

    #[test]
    fn malformed_json_is_a_parse_error_with_a_null_id() {
        let h = harness();
        let d = &h.dispatch;
        let reply = reply_of(d.mcp(b"{not json").expect("dispatches"));
        assert!(reply.get("id").is_some_and(Value::is_null));
        assert_eq!(reply.pointer("/error/code").and_then(Value::as_i64), Some(PARSE_ERROR));
    }

    #[test]
    fn a_batch_is_refused_by_name() {
        let h = harness();
        let d = &h.dispatch;
        let reply = reply_of(d.mcp(br#"[{"jsonrpc":"2.0","id":1,"method":"tools/list"}]"#).expect("dispatches"));
        assert_eq!(reply.pointer("/error/code").and_then(Value::as_i64), Some(INVALID_REQUEST));
        let message = reply.pointer("/error/message").and_then(Value::as_str).unwrap_or_default();
        assert!(message.contains("batch"), "{message}");
    }

    #[test]
    fn a_string_id_comes_back_unchanged() {
        let h = harness();
        let d = &h.dispatch;
        let reply = reply_of(d.mcp(br#"{"jsonrpc":"2.0","id":"abc","method":"tools/list","params":{}}"#).expect("dispatches"));
        assert_eq!(reply.get("id").and_then(Value::as_str), Some("abc"));
    }

    /// The primer's own reply must never be mistaken for the caller's, even when the caller uses
    /// an id the primer could plausibly have chosen.
    #[test]
    fn the_primers_reply_is_never_returned_to_the_caller() {
        let h = harness();
        let d = &h.dispatch;
        for id in ["1", "0", "\"__photocraft_bridge_init_x\""] {
            let body = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/list","params":{{}}}}"#);
            let reply = reply_of(d.mcp(body.as_bytes()).expect("dispatches"));
            assert!(reply.pointer("/result/tools").is_some(), "id {id} got the wrong reply: {reply}");
        }
    }

    #[test]
    fn the_tools_route_and_tools_list_agree_exactly() {
        let h = harness();
        let d = &h.dispatch;
        let convenience: Value = serde_json::from_slice(&d.tools().expect("tools route")).expect("JSON");
        let reply = reply_of(d.mcp(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#).expect("dispatches"));
        let over_mcp = reply.pointer("/result/tools").cloned().expect("a tools array");
        let ours = convenience.get("tools").and_then(Value::as_array).cloned().expect("array");
        let theirs = over_mcp.as_array().cloned().expect("array");
        assert_eq!(ours.len(), theirs.len());
        for (a, b) in ours.iter().zip(&theirs) {
            let keys = |v: &Value| v.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
            assert_eq!(keys(a), keys(b), "key sets differ for {:?}", a.get("name"));
        }
        assert_eq!(ours, theirs, "/tools and tools/list must serve the identical shape");
    }

    #[test]
    fn the_tools_route_uses_the_mcp_spelling_of_the_argument_schema() {
        let h = harness();
        let d = &h.dispatch;
        let text = String::from_utf8(d.tools().expect("tools route")).expect("utf-8");
        assert!(text.contains("inputSchema"), "{text}");
        assert!(!text.contains("input_schema"), "the snake_case spelling reads as `no arguments` to a correct client");
    }
}
