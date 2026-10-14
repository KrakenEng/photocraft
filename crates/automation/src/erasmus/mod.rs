//! The Erasmus program bridge: PhotoCraft's loopback HTTP control plane for the Erasmus harness.
//!
//! Erasmus installs PhotoCraft, launches it itself, allocates the port this server listens on,
//! generates the credential, and puts PhotoCraft's own window inside its shell. Its AI then drives
//! the editor over this bridge — a local model compiles the tool list into a grammar, an agent CLI
//! is handed the bridge as an MCP server. `docs/erasmus.md` records the protocol review, the
//! design decisions and the plan; `../Erasmus/Skills/erasmus-connection.md` is the wire contract.
//!
//! This is a second front end on the same engine, not a replacement for anything.
//! `photocraft-cli mcp --bridge` stays exactly as it is for the standalone agent case, and the
//! JSON-lines control protocol (`docs/control-protocol.md`) is untouched. What is new is the
//! transport: JSON-RPC 2.0 over HTTP with a header credential, which is what the harness speaks.
//!
//! ## The routes
//!
//! | Route | Auth | What it is |
//! |---|---|---|
//! | `GET /health` | **no** | Polled on an interval to decide ready / unreachable / still starting |
//! | `POST /mcp` | yes | Real MCP: `initialize`, `notifications/initialized`, `tools/list`, `tools/call` |
//! | `GET /tools` | yes | A cheap `GET` tool list, rendered from the same source as `tools/list` |
//! | `GET /skills` | yes | [`skills::DOCUMENT`], validated before it is served |
//! | `GET /actions` | yes | [`actions::document`], the named operations with a declared effect |
//!
//! Health is unauthenticated on purpose, and it is not a shortcut. It is the only thing that makes
//! "not running yet" distinguishable from "running, wrong credential" from outside: a credential
//! that only exists partway through startup would otherwise make every launch report a spurious
//! authentication failure and then silently correct itself, and a failure that resolves itself is
//! one nobody trusts the log about afterwards.
//!
//! Replies are always plain `application/json`. An event-stream response that stays open past its
//! own answer is correct for a genuine stream and catastrophic for a request/response call: it is
//! indistinguishable from "more data is coming", so a client reading to end-of-stream waits out
//! the full call timeout on every request with no CPU used and nothing logged. That already
//! happened to one integration. PhotoCraft streams no incremental progress here — slow work goes
//! through `wait: false` and `jobs_list` instead — so there is nothing to gain by risking it.

pub mod actions;
pub mod http;
pub mod mcp;
pub mod skills;

use self::http::{Method, Request, Response};

/// The route paths. These are also what `packaging/erasmus/photocraft.json` declares, and the
/// manifest is how the host learns them — so a change here is a change there.
pub const HEALTH_PATH: &str = "/health";
pub const MCP_PATH: &str = "/mcp";
pub const TOOLS_PATH: &str = "/tools";
pub const SKILLS_PATH: &str = "/skills";
pub const ACTIONS_PATH: &str = "/actions";

/// The header carrying the credential. Named here and declared as `bridge.authHeader` in the
/// manifest; the host is simply told the name.
pub const AUTH_HEADER: &str = "X-PhotoCraft-Token";

/// The MCP protocol revision this bridge answers, matching `bridge.mcpProtocol`.
pub const MCP_PROTOCOL: &str = "2025-06-18";

/// The largest request body accepted, which in practice bounds one `tools/call`'s arguments.
/// Shares the control protocol's own limit so the two front ends cannot disagree about what is
/// too big.
pub const MAX_BODY_BYTES: usize = crate::security::MAX_REQUEST_BYTES;

/// How long the health route may wait for the UI thread before answering without it.
///
/// Health is polled every 1.5 seconds and is the one route that reports on the application before
/// the credential is checked, so it has to answer while the UI is busy. A probe that blocks for
/// three seconds inside a large filter is a bridge the host marks unreachable — and "the UI thread
/// is busy" is a more useful thing to report than a stale answer anyway.
pub const HEALTH_PROBE_BUDGET: std::time::Duration = std::time::Duration::from_millis(250);

/// The part of the health answer only the running application can give.
///
/// Both fields exist because of something that has already cost people work in the sibling
/// integration this was reviewed against:
///
/// * `unsaved_documents` — a document that has never been written to disk is lost outright when
///   the application restarts, and restarting the harness is an ordinary thing to do. The manifest
///   declares `survivesRestart: false`, so Erasmus closes PhotoCraft when it quits and has no way
///   to save first; naming those documents in the one payload the host polls constantly is how it
///   can warn instead. Note that *dirty* is not the signal — a document with unsaved edits but a
///   file on disk still has a file on disk. Having no path at all is the unambiguous case.
/// * `ui_responsive` — false means the UI thread did not answer within [`HEALTH_PROBE_BUDGET`]:
///   busy, not dead. A host that can say "unresponsive" has something true to tell the user; one
///   that only knows the bridge's own thread answered does not.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HealthDetail {
    /// Names of open documents that have never been saved to a file.
    pub unsaved_documents: Vec<String>,
    /// Whether the UI thread answered within the probe's budget.
    pub ui_responsive: bool,
}

impl HealthDetail {
    /// What a backend with no window and no documents of its own reports.
    pub fn headless() -> HealthDetail {
        HealthDetail { unsaved_documents: Vec::new(), ui_responsive: true }
    }

    /// The sentence a host can show a person as-is, or `None` when there is nothing to warn about.
    pub fn warning(&self) -> Option<String> {
        if self.unsaved_documents.is_empty() {
            return None;
        }
        Some(format!(
            "{} document(s) have never been saved and will be lost if PhotoCraft restarts: {}",
            self.unsaved_documents.len(),
            self.unsaved_documents.join(", ")
        ))
    }
}

/// One JSON-RPC exchange's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mcp {
    /// A serialized JSON-RPC response to write back.
    Reply(Vec<u8>),
    /// A notification: `202` with no body, per the MCP HTTP transport.
    Notification,
}

/// What the router needs from the MCP layer above it.
///
/// A trait rather than a concrete type so the routing, the auth and the limits can be tested
/// without an engine, a window or a tokio runtime behind them — and so the MCP dispatch can be
/// written against the real `ToolRouter` without the router caring.
///
/// Synchronous on purpose: each connection is served on its own thread (as the JSON-lines control
/// server already does), so an implementation is free to block on its own runtime. An async trait
/// here would buy nothing and cost a boxed future per call.
pub trait Dispatch: Send + Sync {
    /// Handle one `POST /mcp` body. `Err` is a transport-level failure to report as 500; a
    /// JSON-RPC *error* is a successful exchange and belongs in [`Mcp::Reply`].
    fn mcp(&self, body: &[u8]) -> Result<Mcp, String>;

    /// The `GET /tools` body: the same tools, named the same way, as `tools/list` returns.
    ///
    /// Rendered from one source deliberately. MCP names a tool's argument schema `inputSchema`,
    /// and one real integration's convenience endpoint served `input_schema` instead — so a
    /// caller checking only the MCP-correct key read every tool as taking no arguments, attempted
    /// a destructive call with none, was refused for the argument it had not sent, and retried
    /// identically forever. Two endpoints on one program must never disagree about its own tools.
    fn tools(&self) -> Result<Vec<u8>, String>;

    /// The application's own contribution to `GET /health`.
    ///
    /// Required rather than defaulted: the honest answer differs between a backend with a window
    /// and one without, and a default would have to guess. Must return within
    /// [`HEALTH_PROBE_BUDGET`] — report `ui_responsive: false` rather than waiting longer, because
    /// a slow health probe is read by the host as an unreachable program.
    ///
    /// Infallible on purpose. Health is a diagnostic, and a diagnostic that can fail is one more
    /// thing to diagnose; a backend that cannot answer reports what it does know.
    fn health(&self) -> HealthDetail;
}

/// The bridge's HTTP surface: routing, auth and the static documents.
pub struct Bridge<D: Dispatch> {
    token: String,
    allow_token_in_query: bool,
    dispatch: D,
}

impl<D: Dispatch> Bridge<D> {
    /// `token` is the credential Erasmus generated and injected at launch.
    ///
    /// `allow_token_in_query` lets a client that cannot set a custom header pass `?token=`
    /// instead; it must match `bridge.allowTokenInQuery` in the manifest, and defaults off
    /// because a token in a URL ends up in logs that a header does not.
    pub fn new(token: impl Into<String>, allow_token_in_query: bool, dispatch: D) -> Bridge<D> {
        Bridge { token: token.into(), allow_token_in_query, dispatch }
    }

    /// Route one request. Never fails: every outcome is a response, because a client that cannot
    /// tell "refused" from "crashed" retries, and a retry loop against a parser is the thing to
    /// avoid.
    pub fn handle(&self, request: &Request) -> Response {
        if request.path == HEALTH_PATH {
            return match request.method {
                Method::Get => self.health(),
                Method::Post => method_not_allowed("GET"),
            };
        }
        if !self.authorized(request) {
            // No detail about *why*. A caller holding the wrong credential learns only that it is
            // wrong, which is all it needs to know.
            return error(401, "authentication required");
        }
        match (request.method, request.path.as_str()) {
            (Method::Post, MCP_PATH) => self.mcp(request),
            // A streamable-HTTP MCP client may open a GET here to receive server-initiated
            // messages. Nothing in PhotoCraft's bridge ever pushes unprompted, and the MCP spec
            // is explicit that a server not offering an SSE stream at its endpoint must answer
            // 405 — so this is the compliant reply, not a gap. The sibling integration answers
            // 200 with a keep-alive comment instead, on the grounds that some clients read a
            // refusal as a failed connection; if one ever does, that is the change to make, and
            // it is a client bug being accommodated rather than a protocol requirement.
            (Method::Get, MCP_PATH) => method_not_allowed("POST"),
            (Method::Get, TOOLS_PATH) => self.tools(),
            (Method::Get, SKILLS_PATH) => self.skills(),
            (Method::Get, ACTIONS_PATH) => self.actions(),
            (Method::Post, TOOLS_PATH | SKILLS_PATH | ACTIONS_PATH) => method_not_allowed("GET"),
            _ => error(404, "no such route"),
        }
    }

    /// Unauthenticated, and `ok: true` whenever the bridge itself answered.
    ///
    /// `ok` is deliberately about reachability and nothing else: the host polls this every 1.5
    /// seconds to decide whether the program is up, and a health check that can go false for a
    /// second reason is one that reports the wrong thing. Everything the application knows that a
    /// person might need rides alongside it in [`HealthDetail`] without ever making `ok` false —
    /// unsaved documents and a wedged UI thread are both facts about a perfectly reachable
    /// program.
    fn health(&self) -> Response {
        let detail = self.dispatch.health();
        json_body(
            200,
            &serde_json::json!({
                "ok": true,
                "program": "photocraft",
                "version": env!("CARGO_PKG_VERSION"),
                "unsaved_documents": detail.unsaved_documents,
                "ui_responsive": detail.ui_responsive,
                "warning": detail.warning(),
            }),
        )
    }

    fn mcp(&self, request: &Request) -> Response {
        match self.dispatch.mcp(&request.body) {
            Ok(Mcp::Reply(body)) => Response::json(200, body),
            Ok(Mcp::Notification) => Response::accepted(),
            Err(why) => error(500, &why),
        }
    }

    fn tools(&self) -> Response {
        match self.dispatch.tools() {
            Ok(body) => Response::json(200, body),
            Err(why) => error(500, &why),
        }
    }

    /// The structured skills document, validated first.
    ///
    /// A document the host rejects is discarded entirely and silently on its side — no retry,
    /// nothing logged, indistinguishable from a program that published no skills at all. So the
    /// failure is made loud here instead: 500, naming the specific problems. `skills::validate`
    /// is also asserted clean by a unit test, so this path should be unreachable; "should be" is
    /// not a reason to serve a malformed document when it is not.
    fn skills(&self) -> Response {
        let problems = skills::validate(skills::SKILLS);
        if !problems.is_empty() {
            return error(500, &format!("the skills document does not validate: {}", problems.join("; ")));
        }
        json_body(200, &skills::DOCUMENT)
    }

    /// The actions document, built and then validated.
    ///
    /// Building can fail, and that failure is the interesting one: it means a spec named a tool
    /// whose effect is its caller's choice rather than its own, so no honest effect could be
    /// derived for it. Serving the document without that entry would quietly drop an operation;
    /// serving it with a guessed effect would hand the harness a label it trusts for something
    /// that can do anything. Neither is acceptable, so the whole document is withheld.
    fn actions(&self) -> Response {
        let document = match actions::document() {
            Ok(document) => document,
            Err(e) => return error(500, &format!("the actions document could not be built: action `{}`: {}", e.action, e.why)),
        };
        let problems = actions::validate(&document.actions, actions::KNOWN_TOOLS);
        if !problems.is_empty() {
            return error(500, &format!("the actions document does not validate: {}", problems.join("; ")));
        }
        json_body(200, &document)
    }

    /// The credential, checked the same way on every route but health.
    fn authorized(&self, request: &Request) -> bool {
        if let Some(supplied) = request.header(AUTH_HEADER) {
            return constant_time_eq(&self.token, supplied);
        }
        // A header is always preferred; the query parameter exists only for a client that cannot
        // send one, and only when the manifest asked for it.
        if self.allow_token_in_query
            && let Some(supplied) = request.query_param("token")
        {
            return constant_time_eq(&self.token, supplied);
        }
        false
    }
}

/// Compare two credentials without an early exit on the first differing byte.
///
/// Deliberately *not* `security::token_matches`: that one is built for the control protocol's own
/// 64-hex-character token and both rejects anything of another length and compares
/// case-sensitively against a value its caller lowercased. The Erasmus credential is minted by
/// the harness, and its format is the harness's business, not ours to constrain.
///
/// The length check short-circuits, which leaks the credential's length. That is not worth hiding:
/// the length is in the manifest (`token.bytes`), and padding to a fixed comparison would make
/// this harder to read for no secret kept.
fn constant_time_eq(expected: &str, supplied: &str) -> bool {
    if expected.len() != supplied.len() {
        return false;
    }
    expected.bytes().zip(supplied.bytes()).fold(0u8, |differing, (a, b)| differing | (a ^ b)) == 0
}

/// Serialize a body, or answer 500 rather than a truncated document.
///
/// Our own documents are `const` and cannot fail to serialize, so this is a guard against a future
/// one that can — not a path with a known trigger. Returning a half-written document would be the
/// worse outcome: the host discards an invalid one silently.
fn json_body(status: u16, value: &impl serde::Serialize) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => Response::json(status, body),
        Err(e) => error(500, &format!("could not serialize the response: {e}")),
    }
}

/// An error body, in the one shape every route uses.
fn error(status: u16, message: &str) -> Response {
    match serde_json::to_vec(&serde_json::json!({ "ok": false, "error": message })) {
        Ok(body) => Response::json(status, body),
        // Serializing a string into a two-field object cannot fail; if it somehow did, the status
        // still has to reach the caller.
        Err(_) => Response::text(status, message.as_bytes().to_vec()),
    }
}

fn method_not_allowed(allowed: &str) -> Response {
    error(405, &format!("this route accepts {allowed}"))
}

/// Serve one connection: read a request, answer it, close.
///
/// One exchange per connection, deliberately. Keep-alive would save a syscall on a health poll
/// every 1.5 seconds and cost connection state to get wrong in exchange; `Connection: close` is
/// what the response already says.
///
/// A malformed request still gets a reply rather than a dropped socket: a client that cannot tell
/// "refused" from "crashed" retries, and a retry loop against a parser is the thing to avoid. The
/// two cases with nobody to answer — a closed socket and a failed read — return without writing.
pub fn serve_connection<D: Dispatch>(stream: &std::net::TcpStream, bridge: &Bridge<D>) {
    let Ok(read) = stream.try_clone() else { return };
    let mut out = match stream.try_clone() {
        Ok(out) => out,
        Err(_) => return,
    };
    let response = match Request::read(&mut std::io::BufReader::new(read), MAX_BODY_BYTES) {
        Ok(request) => bridge.handle(&request),
        Err(error) => match http::status_for(&error) {
            Some((status, why)) => Response::text(status, why.into_bytes()),
            None => return,
        },
    };
    let _ = response.write_to(&mut out);
}

/// `^[a-z0-9][a-z0-9_-]{0,63}$`, the connector id shape shared by skills and actions.
///
/// Written out rather than pulled in as a regex dependency: one character class, and the cost of
/// a regex engine in a crate that does not otherwise need one is not worth it.
pub(crate) fn is_connector_id(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else { return false };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    name.len() <= 64 && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::BufReader;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    /// A dispatch that records nothing and answers predictably, so the routing and the auth are
    /// what is under test.
    struct Stub {
        fail: bool,
        health: HealthDetail,
    }

    impl Dispatch for Stub {
        fn health(&self) -> HealthDetail {
            self.health.clone()
        }

        fn mcp(&self, body: &[u8]) -> Result<Mcp, String> {
            if self.fail {
                return Err("dispatch unavailable".into());
            }
            if body == b"notify" {
                return Ok(Mcp::Notification);
            }
            Ok(Mcp::Reply(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_vec()))
        }

        fn tools(&self) -> Result<Vec<u8>, String> {
            if self.fail {
                return Err("dispatch unavailable".into());
            }
            Ok(br#"{"tools":[{"name":"doc_inspect","description":"d","inputSchema":{"type":"object"}}]}"#.to_vec())
        }
    }

    fn bridge(allow_query: bool) -> Bridge<Stub> {
        Bridge::new(TOKEN, allow_query, Stub { fail: false, health: HealthDetail::headless() })
    }

    fn request(raw: &str) -> Request {
        Request::read(&mut BufReader::new(raw.as_bytes()), MAX_BODY_BYTES).expect("the fixture parses")
    }

    fn get(path: &str, token: Option<&str>) -> Request {
        let auth = token.map(|t| format!("{AUTH_HEADER}: {t}\r\n")).unwrap_or_default();
        request(&format!("GET {path} HTTP/1.1\r\n{auth}\r\n"))
    }

    fn body_of(response: &Response) -> Value {
        serde_json::from_slice(&response.body).expect("a JSON body")
    }

    #[test]
    fn health_needs_no_credential() {
        let r = bridge(false).handle(&get(HEALTH_PATH, None));
        assert_eq!(r.status, 200, "health must answer before a credential exists, or every launch reports a spurious auth failure");
        assert_eq!(body_of(&r).get("ok").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn health_names_the_program_and_version() {
        let body = body_of(&bridge(false).handle(&get(HEALTH_PATH, None)));
        assert_eq!(body.get("program").and_then(Value::as_str), Some("photocraft"));
        assert!(body.get("version").and_then(Value::as_str).is_some_and(|v| !v.is_empty()));
    }

    #[test]
    fn health_reports_unsaved_documents_without_going_unhealthy() {
        let detail = HealthDetail { unsaved_documents: vec!["Untitled-1".into(), "Untitled-2".into()], ui_responsive: true };
        let b = Bridge::new(TOKEN, false, Stub { fail: false, health: detail });
        let body = body_of(&b.handle(&get(HEALTH_PATH, None)));
        assert_eq!(body.get("ok").and_then(Value::as_bool), Some(true), "unsaved work is a fact about a reachable program, not a failure");
        let names = body.get("unsaved_documents").and_then(|d| d.as_array()).cloned().unwrap_or_default();
        assert_eq!(names.len(), 2);
        let warning = body.get("warning").and_then(Value::as_str).unwrap_or_default().to_owned();
        assert!(warning.contains("Untitled-1") && warning.contains("lost if PhotoCraft restarts"), "{warning}");
    }

    #[test]
    fn health_reports_a_busy_ui_thread_as_still_reachable() {
        let detail = HealthDetail { unsaved_documents: Vec::new(), ui_responsive: false };
        let b = Bridge::new(TOKEN, false, Stub { fail: false, health: detail });
        let body = body_of(&b.handle(&get(HEALTH_PATH, None)));
        assert_eq!(body.get("ok").and_then(Value::as_bool), Some(true), "busy is not dead");
        assert_eq!(body.get("ui_responsive").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn health_has_no_warning_when_there_is_nothing_to_warn_about() {
        let body = body_of(&bridge(false).handle(&get(HEALTH_PATH, None)));
        assert!(body.get("warning").is_some_and(Value::is_null), "an empty warning must be null, not an empty sentence");
        assert_eq!(HealthDetail::headless().warning(), None);
    }

    #[test]
    fn every_other_route_refuses_without_a_credential() {
        let b = bridge(false);
        for path in [TOOLS_PATH, SKILLS_PATH, ACTIONS_PATH] {
            assert_eq!(b.handle(&get(path, None)).status, 401, "{path} must require the credential");
        }
        let post = request(&format!("POST {MCP_PATH} HTTP/1.1\r\nContent-Length: 2\r\n\r\n{{}}"));
        assert_eq!(b.handle(&post).status, 401, "{MCP_PATH} must require the credential");
    }

    #[test]
    fn a_wrong_credential_is_401_and_says_nothing_about_why() {
        let r = bridge(false).handle(&get(SKILLS_PATH, Some("0000000000000000000000000000000f")));
        assert_eq!(r.status, 401);
        let message = body_of(&r).get("error").and_then(Value::as_str).unwrap_or_default().to_owned();
        assert_eq!(message, "authentication required", "the reply must not hint at which part was wrong");
    }

    #[test]
    fn the_credential_header_is_matched_case_insensitively() {
        let r = request(&format!("GET {SKILLS_PATH} HTTP/1.1\r\nx-photocraft-token: {TOKEN}\r\n\r\n"));
        assert_eq!(bridge(false).handle(&r).status, 200, "HTTP field names are case-insensitive");
    }

    #[test]
    fn the_query_token_works_only_when_the_manifest_asked_for_it() {
        let r = request(&format!("GET {SKILLS_PATH}?token={TOKEN} HTTP/1.1\r\n\r\n"));
        assert_eq!(bridge(false).handle(&r).status, 401, "off by default: a token in a URL lands in logs a header does not");
        assert_eq!(bridge(true).handle(&r).status, 200, "and works when it is opted into");
    }

    #[test]
    fn the_skills_route_serves_the_validated_document() {
        let r = bridge(false).handle(&get(SKILLS_PATH, Some(TOKEN)));
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type, "application/json");
        let list = body_of(&r).get("skills").and_then(|s| s.as_array()).cloned().expect("a skills array");
        assert_eq!(list.len(), skills::SKILLS.len());
    }

    #[test]
    fn the_actions_route_serves_the_validated_document() {
        let r = bridge(false).handle(&get(ACTIONS_PATH, Some(TOKEN)));
        assert_eq!(r.status, 200);
        let list = body_of(&r).get("actions").and_then(|a| a.as_array()).cloned().expect("an actions array");
        assert_eq!(list.len(), actions::SPECS.len());
    }

    #[test]
    fn the_tools_route_uses_the_mcp_name_for_an_argument_schema() {
        let r = bridge(false).handle(&get(TOOLS_PATH, Some(TOKEN)));
        assert_eq!(r.status, 200);
        let text = String::from_utf8(r.body).expect("utf-8");
        assert!(text.contains("inputSchema"), "{text}");
        assert!(!text.contains("input_schema"), "the snake_case spelling is the one that silently reads as `no arguments`: {text}");
    }

    #[test]
    fn mcp_replies_with_plain_json_never_an_event_stream() {
        let post = request(&format!("POST {MCP_PATH} HTTP/1.1\r\n{AUTH_HEADER}: {TOKEN}\r\nContent-Length: 2\r\n\r\n{{}}"));
        let r = bridge(false).handle(&post);
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type, "application/json", "an event-stream reply to a request/response call reads as a hang");
    }

    #[test]
    fn a_notification_is_202_with_no_body() {
        let post = request(&format!("POST {MCP_PATH} HTTP/1.1\r\n{AUTH_HEADER}: {TOKEN}\r\nContent-Length: 6\r\n\r\nnotify"));
        let r = bridge(false).handle(&post);
        assert_eq!(r.status, 202);
        assert!(r.body.is_empty());
    }

    #[test]
    fn a_dispatch_failure_is_500_not_a_malformed_reply() {
        let b = Bridge::new(TOKEN, false, Stub { fail: true, health: HealthDetail::headless() });
        let post = request(&format!("POST {MCP_PATH} HTTP/1.1\r\n{AUTH_HEADER}: {TOKEN}\r\nContent-Length: 2\r\n\r\n{{}}"));
        assert_eq!(b.handle(&post).status, 500);
        assert_eq!(b.handle(&get(TOOLS_PATH, Some(TOKEN))).status, 500);
    }

    #[test]
    fn the_wrong_method_on_a_real_route_is_405() {
        let b = bridge(false);
        assert_eq!(b.handle(&get(MCP_PATH, Some(TOKEN))).status, 405, "GET /mcp");
        let post = request(&format!("POST {HEALTH_PATH} HTTP/1.1\r\n\r\n"));
        assert_eq!(b.handle(&post).status, 405, "POST /health");
    }

    #[test]
    fn an_unknown_route_is_404_and_still_needs_the_credential() {
        let b = bridge(false);
        assert_eq!(b.handle(&get("/admin", None)).status, 401, "an unknown route must not reveal itself to an unauthenticated caller");
        assert_eq!(b.handle(&get("/admin", Some(TOKEN))).status, 404);
    }

    #[test]
    fn health_is_the_only_unauthenticated_route() {
        let b = bridge(false);
        for path in [HEALTH_PATH, MCP_PATH, TOOLS_PATH, SKILLS_PATH, ACTIONS_PATH, "/anything"] {
            let status = b.handle(&get(path, None)).status;
            if path == HEALTH_PATH {
                assert_eq!(status, 200, "health answers unauthenticated");
            } else {
                assert_eq!(status, 401, "{path} answered {status} without a credential");
            }
        }
    }

    #[test]
    fn constant_time_eq_matches_only_an_exact_credential() {
        assert!(constant_time_eq(TOKEN, TOKEN));
        assert!(!constant_time_eq(TOKEN, &TOKEN.to_uppercase()), "the harness mints the credential; we compare it verbatim");
        assert!(!constant_time_eq(TOKEN, &TOKEN[..TOKEN.len() - 1]));
        assert!(!constant_time_eq(TOKEN, &format!("{TOKEN}x")));
        assert!(!constant_time_eq(TOKEN, ""));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn connector_ids_follow_the_schema() {
        for good in ["a", "0", "overview", "saving-and-exporting", "with_underscore", "a1-b2_c3"] {
            assert!(is_connector_id(good), "{good} should be valid");
        }
        for bad in ["", "-leading", "_leading", "Upper", "has space", "has.dot", &"x".repeat(65)] {
            assert!(!is_connector_id(bad), "{bad:?} should be invalid");
        }
    }

    /// The route constants and the manifest are two places that have to agree, and nothing but a
    /// person reading both enforces it. This at least fails if a constant is renamed to something
    /// the manifest cannot be declaring.
    #[test]
    fn route_paths_are_absolute_and_distinct() {
        let paths = [HEALTH_PATH, MCP_PATH, TOOLS_PATH, SKILLS_PATH, ACTIONS_PATH];
        for p in paths {
            assert!(p.starts_with('/'), "{p} must be an absolute path");
        }
        for (i, p) in paths.iter().enumerate() {
            assert!(!paths.iter().take(i).any(|q| q == p), "{p} is declared twice");
        }
    }
}
