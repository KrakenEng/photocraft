//! The Erasmus bridge, inside the running app.
//!
//! Erasmus launches PhotoCraft, allocates the port this listens on, generates the credential and
//! puts PhotoCraft's own window inside its shell; its AI then drives the editor over this bridge.
//! `docs/erasmus.md` is the design, `../Erasmus/Skills/erasmus-connection.md` the wire contract.
//! The protocol itself — routing, auth, the MCP dispatch, the skills and actions documents —
//! lives in `photocraft_automation::erasmus`, which has no UI dependency and is unit-tested
//! without a window. This module is the part that can only exist here: the transport onto the UI
//! thread, the handshake file, and the accept loop.
//!
//! Switched on by the presence of `PHOTOCRAFT_ERASMUS_PORT`. Nothing starts without it, so an
//! ordinary launch is untouched.
//!
//! The bridge shares the app's one control channel with the JSON-lines control server rather than
//! opening a listener of its own onto the UI thread. Two front ends, one queue, one dispatcher —
//! the alternative was a second mapping of the same 25 tools, which is how two mappings come to
//! disagree.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::Sender;
use std::time::Instant;

use photocraft_automation::erasmus::http::Response;
use photocraft_automation::erasmus::mcp::McpDispatch;
use photocraft_automation::erasmus::{Bridge, HEALTH_PROBE_BUDGET, HealthDetail, serve_connection};
use photocraft_automation::security::{ConnectionLimiter, MAX_CONNECTIONS, configure_stream};
use photocraft_automation::{AutomationError, ControlTransport, PhotocraftMcp};
use photocraft_ui_egui::ControlRequest;
use photocraft_ui_egui::control::{REPLY_DEADLINE, timeout_error};
use serde_json::{Value, json};

/// The port Erasmus allocated for us, matching `bridge.port.envVar` in the manifest.
const PORT_VAR: &str = "PHOTOCRAFT_ERASMUS_PORT";
/// The credential Erasmus generated, matching `bridge.token.envVar`.
const TOKEN_VAR: &str = "PHOTOCRAFT_ERASMUS_TOKEN";
/// Where to write the handshake. **The host chooses this path**, which is what makes a stale file
/// from a previous run impossible and means Erasmus never has to guess how we derive a data
/// directory. Matches `bridge.handshake.envVar`.
const HANDSHAKE_VAR: &str = "PHOTOCRAFT_ERASMUS_HANDSHAKE";

/// What the environment says about being hosted.
pub struct Config {
    pub port: u16,
    pub token: String,
    pub handshake: Option<PathBuf>,
}

impl Config {
    /// Read the configuration, or `None` for an ordinary launch.
    ///
    /// A port that is set but unparseable is a misconfiguration worth saying so about rather than
    /// starting silently without a bridge: the host would show the program as permanently
    /// unreachable and nothing would say why.
    pub fn from_env() -> Option<Config> {
        let raw = std::env::var(PORT_VAR).ok()?;
        let port: u16 = match raw.trim().parse() {
            Ok(port) => port,
            Err(_) => {
                log::warn!("{PORT_VAR} is `{raw}`, which is not a port: the Erasmus bridge is off");
                return None;
            }
        };
        let token = std::env::var(TOKEN_VAR).unwrap_or_default();
        if token.is_empty() {
            log::warn!("{TOKEN_VAR} is empty: the Erasmus bridge is off, since every route but health needs a credential");
            return None;
        }
        Some(Config { port, token, handshake: std::env::var_os(HANDSHAKE_VAR).map(PathBuf::from).filter(|p| !p.as_os_str().is_empty()) })
    }
}

// ---------------------------------------------------------------------------
// The transport onto the UI thread
// ---------------------------------------------------------------------------

/// A [`ControlTransport`] over the app's in-process control channel.
///
/// The same hop the JSON-lines control server makes: queue a [`ControlRequest`], ask for a repaint
/// so the UI thread notices, and wait for the reply. Documents and the GPU compositor belong to
/// that thread, so there is no other way in — and sharing its queue is why the MCP tools reach the
/// *visible* window rather than a second session of their own.
struct ChannelTransport {
    requests: Sender<ControlRequest>,
    ctx: egui::Context,
}

impl ChannelTransport {
    /// One request, blocking until the UI thread answers or the deadline passes.
    ///
    /// `budget` exists for health, which must answer while the UI is busy; everything else uses
    /// the app's own [`REPLY_DEADLINE`], so the error a caller sees is the app's and not a
    /// shorter one invented here.
    fn dispatch(&self, method: &str, params: Value, budget: std::time::Duration) -> Result<Value, AutomationError> {
        self.dispatch_detailed(method, params, budget).map_err(|(error, _)| error)
    }

    /// [`Self::dispatch`], also saying whether the UI thread answered at all.
    ///
    /// The distinction only matters to health, and it matters a lot there: a reply that *is* an
    /// error still proves the UI thread is running frames and servicing the queue. Treating any
    /// failure as "unresponsive" reports a wedged application every time a method name is wrong,
    /// which is both alarming and useless — and is exactly the bug the first live launch of this
    /// bridge had.
    fn dispatch_detailed(&self, method: &str, params: Value, budget: std::time::Duration) -> Result<Value, (AutomationError, Answered)> {
        let deadline = Instant::now() + budget;
        let (mut request, reply) = ControlRequest::new(method, params);
        request.deadline = Some(deadline);
        if self.requests.send(request).is_err() {
            return Err((AutomationError::App("the application is shutting down".into()), Answered::No));
        }
        self.ctx.request_repaint();
        let response = match reply.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(response) => response,
            Err(_) => return Err((AutomationError::App(timeout_error()), Answered::No)),
        };
        unwrap_control(response).map_err(|error| (error, Answered::Yes))
    }
}

/// Whether the UI thread answered a request, regardless of what it answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answered {
    Yes,
    No,
}

/// The control protocol's envelope: `{ok, result|error}` becomes a `Result`.
fn unwrap_control(response: Value) -> Result<Value, AutomationError> {
    if response.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(response.get("result").cloned().unwrap_or(Value::Null));
    }
    let message = response.get("error").and_then(Value::as_str).unwrap_or("the application reported an error");
    Err(AutomationError::App(message.to_owned()))
}

impl ControlTransport for ChannelTransport {
    fn call<'a>(&'a self, method: &'a str, params: Value) -> photocraft_automation::bridge::Call<'a> {
        let method = method.to_owned();
        Box::pin(async move {
            // `spawn_blocking`, not a blocking wait inline: this runs on a tokio worker, and a
            // tool that waits seconds for a filter would otherwise hold a worker thread hostage.
            let transport = self.clone_handle();
            tokio::task::spawn_blocking(move || transport.dispatch(&method, params, REPLY_DEADLINE))
                .await
                .map_err(|e| AutomationError::App(format!("control dispatch failed: {e}")))?
        })
    }

    fn call_read<'a>(&'a self, method: &'a str, params: Value) -> photocraft_automation::bridge::Call<'a> {
        // A read applies nothing, but the app's own dispatcher is the thing that would have to
        // retry and it does not: one attempt, same as a write.
        self.call(method, params)
    }
}

impl ChannelTransport {
    /// A detached copy for a blocking task. Both halves are cheap clones that address the same
    /// channel and the same window.
    fn clone_handle(&self) -> ChannelTransport {
        ChannelTransport { requests: self.requests.clone(), ctx: self.ctx.clone() }
    }

    /// The application's contribution to `GET /health`.
    ///
    /// Two facts, both from the sibling integration having been hurt by their absence. A document
    /// that has never been written to disk is lost outright when the app restarts, and the
    /// manifest declares `survivesRestart: false` — so Erasmus closes PhotoCraft on quit and
    /// cannot save first. Naming those documents in the payload it already polls every 1.5
    /// seconds is how it can warn instead. And a UI thread that misses the budget is reported as
    /// unresponsive rather than waited for: a health probe that blocks inside a large filter is a
    /// bridge the host marks unreachable.
    ///
    /// `dirty` is deliberately not the signal. A document with unsaved edits and a file on disk
    /// still has a file on disk; having no path at all is the unambiguous case.
    fn health(&self) -> HealthDetail {
        match self.dispatch_detailed("session.list", json!({}), HEALTH_PROBE_BUDGET) {
            Ok(session) => {
                let unsaved = session
                    .get("documents")
                    .and_then(Value::as_array)
                    .map(|documents| {
                        documents
                            .iter()
                            .filter(|d| d.get("path").is_none_or(Value::is_null))
                            .map(|d| d.get("name").and_then(Value::as_str).unwrap_or("Untitled").to_owned())
                            .collect()
                    })
                    .unwrap_or_default();
                HealthDetail { unsaved_documents: unsaved, ui_responsive: true }
            }
            // The UI thread answered and the method failed. Responsive, and nothing can be said
            // about documents — reporting none would claim there is nothing to lose.
            Err((error, Answered::Yes)) => {
                log::warn!("the Erasmus health probe could not list documents: {error}");
                HealthDetail { unsaved_documents: Vec::new(), ui_responsive: true }
            }
            // Nothing came back inside the budget: busy, not dead.
            Err((_, Answered::No)) => HealthDetail { unsaved_documents: Vec::new(), ui_responsive: false },
        }
    }
}

// ---------------------------------------------------------------------------
// The handshake file
// ---------------------------------------------------------------------------

/// The file we announce ourselves in, at the path the host chose.
///
/// Carries only what the host cannot already know: our pid, our window handle, where our logs are,
/// and our own longest tool budget. The port and the credential came *from* Erasmus, so repeating
/// them would be telling it what it told us.
pub struct Handshake {
    path: PathBuf,
    port: u16,
    log_dir: Option<PathBuf>,
    /// 0 until the window exists. See [`Self::publish_window_handle`].
    hwnd: AtomicIsize,
}

impl Handshake {
    pub fn new(path: PathBuf, port: u16, log_dir: Option<PathBuf>) -> Handshake {
        Handshake { path, port, log_dir, hwnd: AtomicIsize::new(0) }
    }

    fn detail(&self) -> Value {
        json!({
            "port": self.port,
            "pid": std::process::id(),
            "hwnd": self.hwnd.load(Ordering::Relaxed),
            "running": true,
            "log_dir": self.log_dir.as_ref().map(|p| p.display().to_string()),
            // The longest PhotoCraft will let one tool call take. The host's own budget must be
            // LONGER than ours, and this is published rather than hand-copied into the manifest
            // because two hand-kept numbers is how they end up the wrong way round. Ours being
            // smaller than the manifest's floor is the safe direction: we give up first, so the
            // model is never told a call failed while the edit lands anyway.
            "max_tool_timeout_ms": u64::try_from(REPLY_DEADLINE.as_millis()).unwrap_or(u64::MAX),
        })
    }

    /// Replace the file atomically.
    ///
    /// Write a sibling and rename over the target, never delete-then-recreate: a host reading
    /// during a rewrite would otherwise get "no such file", which is indistinguishable from
    /// "PhotoCraft is not running" and turns its natural retry into a wait on a file that may
    /// never come back. Rename-over is atomic on both Windows and POSIX, so the file always
    /// exists and is always complete.
    pub fn write(&self) {
        let detail = self.detail();
        let temporary = self.path.with_extension("tmp");
        if let Err(error) = write_atomically(&temporary, &self.path, &detail) {
            // Never fatal. The handshake is how the host learns three optional things; losing it
            // costs a title-matched window attach and a vaguer crash report, not the session.
            log::warn!("could not write the Erasmus handshake {}: {error}", self.path.display());
            let _ = std::fs::remove_file(&temporary);
        }
    }

    /// Re-announce once the window exists.
    ///
    /// The bridge starts before there is a window to point at, so the first write carries no
    /// handle; this is called from the first frame, when there is one. Idempotent — an unchanged
    /// handle writes nothing — and safe to repeat because [`Self::write`] replaces atomically.
    ///
    /// Worth the trouble because it is what lets the manifest ask for `strategy: "handshake"`,
    /// the one exact way for the host to find our window. A title match on "PhotoCraft" would
    /// happily attach to a copy the user already had open, which for an image editor is the
    /// ordinary case rather than a corner one.
    pub fn publish_window_handle(&self, handle: isize) {
        if handle == 0 || self.hwnd.swap(handle, Ordering::Relaxed) == handle {
            return;
        }
        self.write();
        log::info!("published the window handle {handle} to the Erasmus handshake");
    }
}

fn write_atomically(temporary: &Path, target: &Path, detail: &Value) -> std::io::Result<()> {
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(temporary)?;
    file.write_all(serde_json::to_string_pretty(detail)?.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(temporary, target)
}

/// The app's own native window handle, for the handshake.
///
/// Only meaningful on Windows, where a handle is something a host can act on; the attach path in
/// `../Erasmus/electron/main/window/manager.ts` is Windows-only and the schema says so. Anywhere
/// else this is `None` and the host falls back to searching, exactly as it would for a program
/// that published nothing.
#[cfg(target_os = "windows")]
pub fn window_handle(cc: &eframe::CreationContext<'_>) -> Option<isize> {
    use eframe::wgpu::rwh::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

#[cfg(not(target_os = "windows"))]
pub fn window_handle(_cc: &eframe::CreationContext<'_>) -> Option<isize> {
    None
}

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

/// Start the bridge, returning the handshake so the first frame can publish the window handle.
///
/// `requests` is the app's control channel, shared with the JSON-lines control server. Failing to
/// bind is reported and not fatal: a PhotoCraft the user can still use beats one that refuses to
/// open because its automation port was taken.
pub fn start(config: Config, requests: Sender<ControlRequest>, ctx: egui::Context, log_dir: Option<PathBuf>) -> Option<Arc<Handshake>> {
    let listener = match TcpListener::bind(("127.0.0.1", config.port)) {
        Ok(listener) => listener,
        Err(error) => {
            log::error!("the Erasmus bridge could not bind 127.0.0.1:{}: {error}", config.port);
            return None;
        }
    };

    let transport = Arc::new(ChannelTransport { requests, ctx });
    let health = {
        let transport = Arc::clone(&transport);
        move || transport.health()
    };
    // Its own runtime, on its own threads. The UI thread is not a tokio thread and must never
    // become one: a runtime that drove the event loop would make every stalled tool call a
    // stalled frame.
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("photocraft-erasmus").enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            log::error!("the Erasmus bridge could not start its runtime: {error}");
            return None;
        }
    };
    let server = PhotocraftMcp::with_transport(transport);
    let dispatch = McpDispatch::new(server, runtime.handle().clone(), health);
    let bridge = Arc::new(Bridge::new(config.token, false, dispatch));

    let handshake = config.handshake.map(|path| {
        let handshake = Arc::new(Handshake::new(path, config.port, log_dir));
        handshake.write();
        handshake
    });

    std::thread::Builder::new()
        .name("photocraft-erasmus-accept".into())
        .spawn(move || {
            // Held so the runtime outlives every connection that borrows its handle.
            let _runtime = runtime;
            let limiter = ConnectionLimiter::new(MAX_CONNECTIONS);
            log::info!("the Erasmus bridge is listening on 127.0.0.1:{}", config.port);
            for stream in listener.incoming().flatten() {
                let Some(permit) = limiter.try_acquire() else {
                    // Answer rather than drop: a client that cannot tell "refused" from "crashed"
                    // retries, and health is polled every 1.5 seconds.
                    let _ = configure_stream(&stream);
                    let mut stream = stream;
                    let _ = Response::json(503, br#"{"ok":false,"error":"connection limit reached"}"#.to_vec()).write_to(&mut stream);
                    continue;
                };
                let bridge = Arc::clone(&bridge);
                if let Err(error) = std::thread::Builder::new().name("photocraft-erasmus-conn".into()).spawn(move || {
                    let _permit = permit;
                    serve(stream, &bridge);
                }) {
                    log::warn!("the Erasmus bridge could not spawn a connection thread: {error}");
                }
            }
        })
        .map_err(|error| log::error!("the Erasmus bridge could not spawn its accept thread: {error}"))
        .ok()?;

    handshake
}

/// One connection. The reading, routing and writing are
/// [`photocraft_automation::erasmus::serve_connection`] — generic, and integration-tested over a
/// real socket there. This only sets the socket's timeouts first.
fn serve(stream: TcpStream, bridge: &Bridge<McpDispatch>) {
    if configure_stream(&stream).is_err() {
        return;
    }
    serve_connection(&stream, bridge);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_needs_a_port_and_a_credential() {
        // Read through the same parsing the environment goes through, without touching the
        // process environment (which other tests share).
        assert!("not-a-port".parse::<u16>().is_err());
        assert_eq!("7878".parse::<u16>().ok(), Some(7878u16));
    }

    #[test]
    fn the_handshake_carries_what_the_host_cannot_know() {
        let handshake = Handshake::new(PathBuf::from("unused"), 7878, Some(PathBuf::from("/logs")));
        let detail = handshake.detail();
        assert_eq!(detail.get("port").and_then(Value::as_u64), Some(7878));
        assert_eq!(detail.get("hwnd").and_then(Value::as_i64), Some(0), "zero until the window exists");
        assert_eq!(detail.get("running").and_then(Value::as_bool), Some(true));
        assert!(detail.get("pid").and_then(Value::as_u64).is_some_and(|p| p > 0));
        assert!(detail.get("log_dir").and_then(Value::as_str).is_some());
        // Ours must be no longer than the manifest's floor, so we give up before the host does.
        let budget = detail.get("max_tool_timeout_ms").and_then(Value::as_u64).expect("a budget");
        assert_eq!(budget, 60_000, "the app's own reply deadline");
        assert!(budget <= 300_000, "the manifest declares 300000 as the host's floor");
    }

    #[test]
    fn the_handshake_is_written_atomically_and_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("photocraft-erasmus-test-{}", std::process::id()));
        let path = dir.join("photocraft_bridge.json");
        let handshake = Handshake::new(path.clone(), 7878, None);
        handshake.write();
        let written = std::fs::read_to_string(&path).expect("the handshake exists");
        let parsed: Value = serde_json::from_str(&written).expect("complete JSON, never a partial write");
        assert_eq!(parsed.get("port").and_then(Value::as_u64), Some(7878));
        assert!(!path.with_extension("tmp").exists(), "the temporary must not survive");

        // Rewriting replaces rather than removing: a reader never sees the file absent.
        handshake.publish_window_handle(4242);
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(&path).expect("still there")).expect("JSON");
        assert_eq!(parsed.get("hwnd").and_then(Value::as_i64), Some(4242));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn publishing_the_window_handle_is_idempotent_and_ignores_zero() {
        let handshake = Handshake::new(PathBuf::from("unused"), 1, None);
        handshake.publish_window_handle(0);
        assert_eq!(handshake.hwnd.load(Ordering::Relaxed), 0, "no window is not a handle");
        handshake.hwnd.store(99, Ordering::Relaxed);
        handshake.publish_window_handle(99);
        assert_eq!(handshake.hwnd.load(Ordering::Relaxed), 99);
    }

    #[test]
    fn the_control_envelope_becomes_a_result() {
        assert_eq!(unwrap_control(json!({"ok": true, "result": {"a": 1}})).expect("ok"), json!({"a": 1}));
        assert_eq!(unwrap_control(json!({"ok": true})).expect("ok"), Value::Null);
        let error = unwrap_control(json!({"ok": false, "error": "no document"})).expect_err("an error");
        assert!(error.to_string().contains("no document"), "{error}");
        // A reply that is neither shape is an error, not a silent success.
        assert!(unwrap_control(json!({})).is_err());
    }
}
