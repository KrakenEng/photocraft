# Hosting PhotoCraft inside Erasmus

> **Status:** the bridge is implemented and verified against a real launch. It was published to the
> private marketplace as `photocraft 0.9.1` and has since been **delisted** at the owner's request ·
> **Last updated:** 2026-10-16 · **Target:** Erasmus host 0.3.47

Erasmus is an application harness: it installs a program, launches it itself, allocates the port
its control bridge listens on, hands it a handshake file, and puts the program's real OS window
inside the Erasmus shell. Its AI then drives the program over that bridge — a local model gets the
tool list compiled into a grammar, an agent CLI gets the bridge handed to it as an MCP server.

This document records what that harness requires, what PhotoCraft already has, and the plan to
close the gap. The authoritative sources are in the Erasmus checkout (`../Erasmus`):

| Read | For |
|---|---|
| `Skills/erasmus-connection.md` | The wire protocol: routes, auth, shapes, and the failure modes two earlier integrations already hit |
| `Skills/DESKTOP_MARKETPLACE_SKILLS.md` | The manifest field by field, publishing, window modes, access control |
| `shared/schema/manifest.ts` | The contract. A manifest is validated against this and nothing else |
| `shared/schema/connectors.ts` | The skills and actions documents, and `selectProgramSkills` |
| `docs/requirements/{manifests,bridge,window,programs-ai}.md` | The numbered requirements, with their tests |
| `../erasmus-packages/PUBLISHING.md` | The private marketplace: `packages.txt`, manifests, release assets, checksums |

## 1. What the harness requires

### The bridge is one loopback HTTP server

Six routes, three required. Everything but health takes a single credential in one custom header,
checked exactly, and answers 401/403 without it.

| Route | Required | What it is |
|---|---|---|
| `GET /health` | yes | **Unauthenticated.** Fast success/failure. Polled on an interval to decide `ready` / `unreachable` / still starting |
| `POST /mcp` | yes | Real MCP, JSON-RPC 2.0: `initialize`, `notifications/initialized`, `tools/list`, `tools/call` |
| `GET /tools` | no | Cheap `GET` tool list for the panel sidebar and local-model grammar compilation |
| `GET /skill` | no | One flat markdown document (the coarse predecessor of `/skills`) |
| `GET /skills` | no | Several small named documents, selected by keyword against what the user typed |
| `GET /actions` | no | Named, curated tool calls, each with a declared `effect` |

Health is unauthenticated deliberately, and not as a shortcut: it is the only thing that makes
"not running yet" distinguishable from "running, wrong credential" from outside.

### Four failure modes the spec exists to prevent

These are diagnosed, named mistakes from the two integrations that already exist. Each one applies
to PhotoCraft and is cheap to avoid now and expensive to find later.

1. **An event-stream response that outlives its own answer.** A `text/event-stream` reply to
   `/mcp` that stays open after the answer is indistinguishable from "more data is coming", and a
   client reading to end-of-stream waits out the full call timeout on *every* call — no CPU, no
   log, indistinguishable from a hang. Answer plain `application/json`. PhotoCraft streams no
   incremental progress over MCP, so there is no reason to do otherwise.
2. **Two endpoints disagreeing about the same tools.** MCP names a tool's argument schema
   `inputSchema`. One real integration's convenience endpoint served `input_schema` instead, and a
   caller checking only the MCP-correct key read every tool as taking no arguments — then
   attempted a destructive call with none, was refused, and retried identically forever. `/tools`
   and `/mcp` must emit the identical shape, which argues for generating both from one source.
3. **A skills document that fails validation is discarded entirely and silently.** No retry,
   nothing logged on the host's side — it looks exactly like a program that published no skills.
   Validate before serving, and answer a real 500 naming the problems. Keywords are matched as
   literal lowercase substrings: an uppercase keyword matches nothing, ever, and nothing will say so.
4. **A host that gives up before the program does.** If the host's call timeout is shorter than the
   work, the model is told the call failed while PhotoCraft carries on and commits the edit anyway.
   The model then reasons about a document that no longer matches what it believes. The host's wait
   must be longer than ours — which is why PhotoCraft should publish its own budget rather than
   leave two hand-kept numbers to drift apart.

### What it asks of the program beyond the routes

- **No mid-call approval.** MCP has a shape for "needs a human's OK", and a host driving many
  programs cannot honour it: the gate is a standing per-program grant decided once, up front. Do
  the work or refuse outright.
- **Onboard AI off when hosted.** Two agents with hands on one document is a corruption risk.
  PhotoCraft has none — there is no `openai`/`anthropic`/LLM client anywhere in the tree — so this
  requirement is satisfied by construction and there is nothing to suppress.
- **Every internal listener gets a port it did not choose for itself**, not just the main one.

### Two independent gates, both off by default

`allowRendererCalls` is the vendor's statement that its tools are safe to expose to Erasmus's own
UI. `programToolAccess` is the user's per-program switch for whether the assistant may drive the
program unattended. The second is off on a fresh install, and a `destructive` action is never
handed to an automated caller no matter what either gate says.

### The marketplace tier is three artifacts that must agree

A GitHub release holds the bytes; `manifests/<id>.json` says what the program is; `packages.txt`
names the current release and the SHA-256 of its asset. Erasmus takes the hash from `packages.txt`
— a file a person wrote and `git log` records — and never from the API response that served the
download, because a hash supplied with the file is not a check on the file. If the three disagree,
the package is withheld by name rather than installed.

## 2. What PhotoCraft already has

More than it might look, because the shape is already right and only the transport is wrong.

| Erasmus wants | PhotoCraft has | Gap |
|---|---|---|
| Loopback control plane on a host-allocated port | `--control <port>` / `PHOTOCRAFT_CONTROL_PORT`, loopback-only (`apps/photocraft/src/control_server.rs`) | None in shape |
| One credential, injected at launch | `--control-token` / `PHOTOCRAFT_CONTROL_TOKEN`, constant-time compare, auth before any method is reachable | Sent as a JSON `auth` frame, not an HTTP header |
| MCP tool surface | 25 `#[tool]`s on `PhotocraftMcp` (`crates/automation/src/server.rs`), official `rmcp` SDK | Served over **stdio**, not HTTP |
| Tools that drive the live window | Bridge mode: `PhotocraftMcp` → `BridgeClient` → the app's control port | Works, but is a second process |
| Connection limits, bounded reads, call deadlines | `crates/automation/src/security.rs`, `budgets.rs` | Reusable as-is |
| A window to attach | Native egui/winit, title `PhotoCraft`, custom frame; `window_frame.rs` already resolves its own `HWND` via `raw_window_handle` + `winsafe` | Needs publishing into the handshake file |
| A packaged Windows artifact | `packaging/windows/package.ps1` → `photocraft-<version>-windows-<arch>-portable.zip` (`photocraft.exe`, `photocraft-cli.exe`, `portable.txt`) | Needs a manifest |

Two things fall out of that table.

**The transport is the whole gap.** PhotoCraft's control channel is JSON-lines over TCP with a
JSON `auth` handshake; Erasmus wants JSON-RPC 2.0 over HTTP with a header credential. Everything
underneath — the tool catalog, the dispatch into the UI thread, the limits, the deadlines — already
exists and is well covered by tests.

**Portable mode is a gift.** `portable.txt` shipping beside `photocraft.exe` makes PhotoCraft keep
its preferences, brush presets and crash-recovery autosaves in `PhotoCraftData` next to the
executable instead of `%APPDATA%`. Inside an Erasmus install root that means a self-contained
install and an uninstall with nothing left behind — so `uninstall.warnPaths` has little to warn
about, which is the rare case of that field being honestly short.

## 3. Decisions

### The bridge lives in the desktop app, not in a sidecar

Erasmus launches exactly one executable and attaches exactly one window, and the tools have to
affect *that* window. So `photocraft.exe` serves the bridge itself. `photocraft-cli mcp --bridge`
stays exactly as it is for the standalone agent case; this is a second front end on the same
engine, not a replacement.

### The HTTP front end goes in `photocraft-automation`; the wiring goes in the app

`crates/automation` is L6 and already owns the MCP server, the security helpers and the budgets.
The HTTP routing, the auth check, the JSON-RPC dispatch and the skills/actions documents belong
there: no UI-toolkit dependency, and every piece is unit-testable without a window.

It cannot reach `ControlRequest`, which lives in `photocraft-ui-egui` — also L6, with no declared
intra-layer order between the two, so neither may depend on the other. `apps/photocraft` is exempt
and depends on both, so that is where the two are joined.

### `Backend::Bridge` becomes a trait, so the tool catalog stays in one place

All 25 tools funnel through `BridgeClient::call` / `call_read`. Abstracting that pair into a small
trait lets `apps/photocraft` implement it against its in-process `ControlRequest` channel and hand
it to the existing `PhotocraftMcp`. The alternative — re-expressing 25 MCP tools as a second
name-to-control-method mapping inside the app — is the duplication that guarantees the two drift.

This also keeps us off failure mode 2: `/tools` and `/mcp` both render from the one `ToolRouter`,
so they cannot disagree about `inputSchema`.

### Port allocated, token generated, handshake file anyway

The marketplace guide's checklist says to declare `fixed` and match a program's own default port —
and records a real bug where `allocated` defeated Perdix's deliberate discoverability. That
reasoning does not transfer: PhotoCraft has **no** default control port. `--control <port>` is
required and user-chosen, by design, so there is nothing for `fixed` to match and nothing for
`allocated` to defeat. `allocated` it is, which also lets two PhotoCraft windows run at once.

The token is `generated` for the same reason: nothing external has a saved PhotoCraft bridge URL to
keep working across restarts, which is the one thing that would require `handshake`.

A handshake file is still wanted, independently of port and token, to carry three things only the
running program knows:

| Field | Why |
|---|---|
| `hwnd` | Lets `window.match.strategy` be `handshake` — the one exact strategy. A title match on `PhotoCraft` would happily attach to a copy the user already had open, which for an image editor is the normal case, not a corner one |
| `log_dir` | Read on exit, when the runtime directory is about to be swept. The alternative is a panel that can only say "Exited with code …" |
| `max_tool_timeout_ms` | Failure mode 4. PhotoCraft publishes its own longest real budget and the host raises its wait to match, instead of two numbers drifting |

### The call budget is generous, and measured rather than guessed

`docs/scorecard.md` is blunt about what PhotoCraft's slow operations actually cost on a 36 MP
document: saving a layered PSD is ~3.4 s p95, a Gaussian blur ~3.1 s, a full refresh of
14000×14000 ~9.9 s, and Content-Aware Scale on 24 MP is currently measured at **954 s** against a
3 s budget. A 30 s default would abort real work and leave the model believing an edit failed that
in fact landed.

`callTimeoutMs: 300000` (5 minutes) is the manifest floor, with the real number published through
`max_tool_timeout_ms`. The schema's ceiling is 10 minutes on the reasoning that a tool call is a
person waiting. That ceiling and the Content-Aware Scale number are in genuine conflict — which is
an argument for fixing P28 (it is a hang, not a slow path) rather than for asking the host to wait
sixteen minutes.

### Actions, and the `destructive` list

PhotoCraft without an actions document means every one of its 25 tools is reachable by an automated
caller with no distinction between "reads the layer list" and "overwrites the user's PSD". That
distinction exists nowhere else. The first list does not need to be complete, but it does need to
name everything that genuinely loses work, and every effect it states has to be one the tool can
actually hold to.

Effects live in exactly one place, `TOOL_EFFECT` in `crates/automation/src/erasmus/actions.rs`, and
each action derives its own from the tool it names. Writing them per action instead is how an
effect drifts away from the thing it describes, and this is the one value where drifting in the
permissive direction means a model reaching for something the harness believed it could not:

- `doc_save` — writes the document back over its own file. **`destructive`**
- `doc_close` — the document's unsaved edits are gone. **`destructive`**
- `doc_export*` — writes new files and refuses a path that already exists, so it cannot lose
  anything. `modify` — and it is what keeps an automatable way to produce output available at all
- `doc_open`, `doc_new`, `doc_place`, `doc_select`, `doc_import_data_sets`, `jobs_cancel`,
  `ui_pointer`, `ui_set` — `modify`
- `session_list`, `doc_inspect`, `doc_render_preview`, `render_preview`, `ui_inspect`,
  `ui_screenshot`, `command_list`, `jobs_list` — `read`
- `command_run`, `command_batch`, `control_call`, `ui_menu_invoke` — **no effect of their own.**
  Each takes the operation to perform as an argument, so its effect is its caller's choice. They
  have no entry, and an action cannot be built on one. §3a has the mechanism and the defect this
  rule was written in response to.

An undeclared effect reads as `modify` on the host's side, so the cost of *omitting* one is a
confirmation. The cost of stating one that is not true is someone's afternoon, which is why the
unclassifiable tools are excluded rather than given the cautious-looking default.

### `open_file` and the extensions worth claiming

The tool name is fixed by convention — always the literal `open_file`, taking an absolute `path` —
so a host that matched a file's extension never has to look up what this program called its own
opener. PhotoCraft's `doc_open` already does exactly this; `open_file` is a thin, correctly-named
alias that answers a real MCP tool error (`isError: true`) with one sentence when the load fails.

Claiming the layered formats is obvious: `psd`, `psb`, `pcraft`. The flat ones (`png`, `jpg`,
`tif`, `webp`, `exr`, …) are a judgement call — PhotoCraft opens them, but so does everything else,
and a host matching `png` to an image editor over a viewer is a choice to make deliberately rather
than by claiming the longest list.

## 3a. What the Perdix integration teaches

Perdix (parametric CAD, a FreeCAD fork) is the reference integration the Erasmus docs cite
throughout, and it is the closest analogue PhotoCraft has: a native desktop application with a
main-thread-only UI toolkit, documents a user can lose, and tool calls that genuinely take minutes.
Its bridge is `perdix/ai/addon/PerdixAI/perdix_ai/` — `server.py`, `mcp_http.py`, `skills.py`,
`actions.py`. Every point below is from reading it, and the corresponding behaviour in `../Erasmus`
where it matters.

**Confirmations.** Three decisions in §3 were made independently and Perdix made the same ones for
the same stated reasons: the bridge is served *in-process* rather than from a sidecar ("removes
every moving part that one needed: no pip install, no console script on PATH, no second process");
it runs on a worker thread and hops to the UI thread for anything touching the application, because
the toolkit is main-thread-only; and the handshake file carries `hwnd`, `log_dir` and
`max_tool_timeout_ms` for exactly the reasons §3 gives.

**The event-stream hang, with the mechanism.** Perdix's own source carries the postmortem: its
`/mcp` branch "used to write the events straight out after `end_headers()` with no Content-Length,
no chunked encoding and no `Connection: close`, while the handler speaks HTTP/1.1 — a response with
no terminator", so a keep-alive client "blocked until its own timeout, on every call, **because
merely listing `text/event-stream` in Accept takes this branch**". And
`../Erasmus/electron/main/bridge/client.ts` sends `Accept: application/json, text/event-stream` on
every `POST /mcp`. So an Accept-sniffing server takes the streaming branch for *every* Erasmus
request. PhotoCraft's bridge ignores `Accept` entirely and always answers plain JSON, which is why
this cannot happen here.

**Health should carry more than `ok`.** Perdix's health payload includes `unsaved_documents` and
`gui_responsive`, and both exist because of real damage. A document never written to disk is lost
when the application restarts — "restarting to pick up a new build is the normal thing to do here,
and it has cost real models more than once" — and `isTouched()` is explicitly *not* the signal,
because it tracks pending recompute rather than unsaved edits; having no filename is unambiguous.
`gui_responsive` is false when the UI thread misses a 250 ms budget: "busy, not dead", and "a host
that shows a program as unresponsive has something true to say; one that only knows the bridge
answered does not". Both are now in PhotoCraft's health route (`HealthDetail`), which also answers
§5's open question about `survivesRestart`: the harness cannot save for us, but it can be told
constantly and cheaply that there is something to lose.

**`effect` must be derived, and some tools cannot have one.** Perdix's `actions.py` derives every
action's effect from the tool's registered risk and *raises* rather than emit a hand-written one,
because "a hand-copied effect is a value that drifts"; it also keeps a `FORBIDDEN_TOOLS` set
(`python`, `python_eval`) that may never appear in an action whatever a spec says. Reviewing
PhotoCraft's first draft against that found a real defect in it, since fixed:

> The host merges a caller's arguments **over** the vendor's fixed ones (`mergeActionArguments`)
> and never validates the caller's object against `input` first. So the draft's `undo` action —
> pinning `{"command": "edit.undo"}`, declared `modify`, automatable — could be called with
> `{"command": "file.close"}` and would run it. Its sibling `run-command` was worse: `modify`,
> automatable, and arbitrary by design. Both published a label the harness gates on for something
> that could do anything, which is precisely the property the actions document exists to provide.

`TOOL_EFFECT` is now the single place an effect is stated, `build()` derives from it, and a tool
whose effect is its caller's choice (`command_run`, `command_batch`, `control_call`,
`ui_menu_invoke`) has no entry and cannot back an action. A fixed argument is a default and never a
constraint, so no action uses one and `validate` refuses one that tries. Nothing is lost in
capability: `preferOverTools` is false, so an arbitrary-command escape hatch stays reachable as a
raw tool once the user has granted general tool access, which is the honest place for it.

**The handshake is written more than once, atomically.** `start()` runs before there is a window,
so Perdix writes the file again from the first turn of the event loop once `hwnd` exists
(`publish_window_handle`, idempotent). Writes go to a sibling and `os.replace` over the target,
because delete-then-recreate let a client reading during a restart get `FileNotFoundError` —
"indistinguishable from *Perdix is not running*, which turns the natural retry into a busy-wait on
a file that may never come back". `stop()` zeroes the handle, since "a handle outlives the window
it names only as a number that now means something else". eframe creates PhotoCraft's window after
setup too, so step 7 inherits all of this.

**A hosted flag, set by the manifest.** Perdix's manifest sets `exec.env` to
`PERDIX_HOSTED=1` and `PERDIX_POLICY=bypass`: the first suppresses its own chat dock and agent loop
("one assistant instead of two"), the second disables its mid-call approval flow, which a host
cannot answer. PhotoCraft has no onboard AI and no approval flow, so neither is needed for its
stated purpose — but the same signal is the right way to suppress anything else that assumes a
user is present at the window, and a modal "save changes?" on close is the obvious candidate. Not
in the manifest yet: a manifest declaring a flag the application ignores is the same kind of lie as
a mis-declared effect.

**One deliberate divergence.** Perdix answers `GET /mcp` with 200 and a keep-alive comment, on the
grounds that some clients read a refusal as a failed connection. PhotoCraft answers 405, which is
what the MCP specification requires of a server that offers no SSE stream at its endpoint; Erasmus
itself never issues that request (`client.ts` only GETs health, tools and the connector paths). If
an agent CLI handed the bridge URL ever turns out to need the lenient answer, that is a client bug
worth accommodating — but not pre-emptively.

## 3b. The rest of the harness's requirements, checked against PhotoCraft

§1 and §3a covered the bridge and the manifest. These are the remaining numbered requirement
domains — the ones that govern shipping a program rather than driving one — read against what
PhotoCraft actually does. Sources: `../Erasmus/docs/requirements/{installer,supervisor,security,market,updates}.md`
and `Skills/DESKTOP_MARKETPLACE_SKILLS.md` §§2, 4, 8, 11.

**The child's environment is an allowlist, not inheritance** (REQ-SUP-02, REQ-SEC-06). A hosted
program never receives Erasmus's own `process.env`; it gets a fixed base plus whatever the manifest
names in `exec.env`. Worth checking rather than assuming, because PhotoCraft resolves its settings
directory from the environment: `APPDATA`, `LOCALAPPDATA`, `USERPROFILE`, `TEMP`, `TMP`, `PATH`,
`PROGRAMDATA` are all on the Windows list (`electron/main/process/env.ts`), so
`app_dirs::platform_dir` resolves normally. `PHOTOCRAFT_CONFIG_DIR` is *not* inherited, which is
correct — nothing should be reaching into a hosted install's configuration from outside it.

**An update replaces the install root, which portable mode sits inside** — the one real defect this
pass found, now fixed. `portable.txt` puts preferences, brush presets and crash-recovery autosaves
in `PhotoCraftData` beside the executable. That is the right shape for a hosted program (uninstall
takes the install root and leaves nothing behind, REQ-INST-08), but an install finishes with **one
atomic rename into that same install root** (REQ-INST-06) — so without `install.keep` every update
would quietly discard the user's settings. `keep: [{ "path": "PhotoCraftData" }]` carries the tree
across by hard-link (`carryOver.ts` walks a directory and links each file). This is exactly the
field's purpose, and nothing in the bridge work would ever have surfaced it.

**Where PhotoCraft writes, in full.** `app_dirs::resolve` has exactly two outcomes: `PhotoCraftData`
inside the install root (portable, the hosted case), or `%APPDATA%\Photocraft` when portable mode is
on and that directory is not writable. There is no cache or scratch location besides those, so
`uninstall.warnPaths` naming `%APPDATA%\Photocraft` alone is complete rather than merely plausible.

**`ai.hardware` is correctly absent.** It grades a program against the measured machine
(REQ-MKT-10), and it "only makes sense for a program doing its *own* AI work locally". PhotoCraft's
direction is `serves`: the inference happens inside Erasmus against whichever model the user chose,
whose requirements the Model Library already states. Perdix omits it for the same reason.

**`verifyFiles` as a named failure rather than a mysterious one.** The guidance is to list whatever
a packaging slip would otherwise turn into an unhelpful downstream error — Perdix lists `qt.conf`
because a build without it failed on every machine but the one that built it. PhotoCraft's portable
zip contains exactly three files and all three are listed; `portable.txt` is there precisely because
its absence silently changes where user data lives. The binaries link the CRT statically, so there
is no redistributable to verify.

**Already satisfied, worth noting rather than acting on.** Code-signing the Windows binaries is an
open item on Erasmus's own side (REQ-SEC-13) and PhotoCraft already signs (`packaging/windows/sign.ps1`,
Azure secrets in the release workflow). `shell: false` and executables resolving inside the install
root after symlink resolution (REQ-SEC-08) are the harness's to enforce; our `exec.path` is a plain
file at the root. Archive screening rejects traversal, absolute paths, links and special files
(REQ-INST-04) — `Compress-Archive` produces none of those. Tokens are redacted from captured output
(REQ-SEC-07) and PhotoCraft never prints its own bridge credential anyway.

**Does not apply.** `updates.md` is Erasmus updating *itself* (electron-updater, its own version in
`package.json`); a hosted program is updated through the catalog instead. `REQ-SUP-06` — nothing
Erasmus started outlives it — is the same fact `survivesRestart: false` already states, and the
reason health reports unsaved documents.

Still open from this pass: `approxInstalledBytes` (optional, and not knowable until the release zip
is built — step 10) and `PHOTOCRAFT_HOSTED` via `exec.env` (step 8a).

## 4. Plan

| # | Step | Where | |
|---|---|---|---|
| 1 | `ControlTransport`: the two methods every tool reaches the app through, so `Backend::Bridge` can be a socket *or* an in-process channel | `crates/automation/src/bridge.rs`, `server.rs` | done |
| 2 | HTTP/1.1 request parse + response write, every length bounded before it is allocated | `crates/automation/src/erasmus/http.rs` | done |
| 3 | Router + header auth (constant-time), health unauthenticated, 401 everywhere else | `crates/automation/src/erasmus/mod.rs` | done |
| 4 | JSON-RPC over HTTP: `initialize`, `notifications/initialized` → 202, `tools/list`, `tools/call`. Plain JSON, never an event stream | `crates/automation/src/erasmus/mcp.rs` | done |
| 5 | The skills and actions documents, validated before serving | `crates/automation/src/erasmus/{skills,actions}.rs` | done |
| 6 | `open_file`, the fixed-name opener | `crates/automation/src/server.rs` | done |
| 7 | Handshake writer: `pid`, `hwnd`, `log_dir`, `max_tool_timeout_ms`, atomic temp+rename, rewritten once the window exists | `apps/photocraft/src/erasmus.rs` | done |
| 8 | The transport onto the UI thread, the accept loop, and both front ends sharing one control channel | `apps/photocraft/src/{erasmus,control_server,main}.rs` | done |
| 9 | The manifest, validated by running the host's own zod schema over it | `packaging/erasmus/photocraft.json` | done |
| 8a | A `PHOTOCRAFT_HOSTED` flag and the `exec.env` that sets it, once something reads it — a modal "save changes?" on close is the first candidate | `apps/photocraft/src/main.rs`, the manifest | open |
| 10 | Publish: the portable zip, the release asset, the manifest copy, the `packages.txt` line and `approxInstalledBytes` | `../erasmus-packages` | done, then delisted |

### Delisted (2026-10-16)

The `packages.txt` line and `manifests/photocraft.json` were removed, so nothing in the Market
offers PhotoCraft. Per `PUBLISHING.md` that is the whole delisting operation: the
`photocraft-v0.9.1` release and its asset are deliberately left in place, because whether those
bytes stay retrievable is a separate decision, and anyone who had already installed keeps their
copy either way — delisting is not an uninstall. Everything below describes what that publish
was, and stands as the record of how to do it again.

### What was published, and on what base

`photocraft 0.9.1`, release tag `photocraft-v0.9.1` on `KrakenIntegratedSystems/erasmus-packages`,
asset `photocraft-0.9.1-windows-x64-portable.zip`
(sha256 `b0c00efcf6e7777155ae5fbcf66f8a08f363c336c07a489e45b480b2fa2619db`, 110,823,082 bytes).
GitHub's own digest for the uploaded asset was cross-checked against the hash written into
`packages.txt` — that hash is the marketplace's whole integrity guarantee, so measuring it twice
from two sources is worth the one extra call.

**It is a 0.9.1 because it had to be.** The bridge work was developed on 0.8.2 while upstream moved
498 commits ahead to 0.9.0, which was released the same day — and no published release contains the
bridge. Publishing as 0.8.2 or 0.9.0 would have put two different binaries behind one version
number. So the work was rebased onto `upstream/main` (every anchor it depends on survived; no
conflicts) and the workspace version set to 0.9.1.

**Known differences from an official release.** The binaries are **unsigned** — the Azure signing
secrets are a CI input and are not available locally, so Windows SmartScreen will warn on first
run. The marketplace's integrity guarantee does not depend on that signature: it is the sha256 in
`packages.txt`, which a person wrote and `git log` records, and which Erasmus deliberately never
takes from the same response that served the download. Signing is still the thing to fix before
this goes wider, and it is Erasmus's own open item too (REQ-SEC-13).

The MSI was not built — WiX is not installed here and the Erasmus package needs only the portable
zip — so the zip was assembled to `packaging/windows/package.ps1`'s exact layout rather than by
running it: the two executables, `README.md`, both licences, the six `OFL-*.txt` files, and
`portable.txt`, under one wrapper directory. `CRAFT_FONTS_DIR` was set from a fresh `craft-fonts`
checkout so the bundled OFL families are embedded as in an official build, and
`PHOTOCRAFT_BUILD_SHA`/`DATE` were set so the binary reports `0.9.1 (c820c69f1, 2026-10-14)` rather
than a dev build.

### How the pieces sit

```text
Erasmus ──HTTP──▶ apps/photocraft/src/erasmus.rs      the accept loop, one thread per connection
                    └─▶ automation::erasmus::Bridge   routing, header auth, skills, actions
                          ├─▶ McpDispatch             JSON-RPC, one short-lived MCP session per request
                          │     └─▶ PhotocraftMcp     the same 25+1 tools the stdio server serves
                          │           └─▶ ChannelTransport ──▶ ControlRequest ──▶ the UI thread
                          └─▶ HealthDetail            unsaved documents, ui_responsive
```

The one control channel is shared with the JSON-lines control server, so a tool call and a
`--control` request are the same request by the time the UI thread sees them.

### Verified against a real launch

The publishing checklist asks for the live endpoint to be curled with a real token and read by eye
rather than only through our own validator. Done, on 2026-10-13, with the app started under the
harness's own environment variables:

| Checked | Result |
|---|---|
| `GET /health`, no credential | `{"ok":true,"ui_responsive":true,"unsaved_documents":[],…}` |
| The handshake file | Written at the host-chosen path with a real `hwnd`, `log_dir`, `pid`, `port`, `max_tool_timeout_ms: 60000` |
| `tools/call doc_new` | `{"document":0}` — and the window really showed `BridgeTest` at 320×200 |
| `GET /health` after it | `unsaved_documents: ["BridgeTest"]`, with the warning sentence |
| `/skills`, `/actions`, `/tools` | 8 skills; 10 actions (5 read, 3 modify, 2 destructive); 26 tools |
| Any route without the credential | 401 |
| `ui.screenshot` with a `path` | Refused: "automation write authority is not configured" |

Two bugs only a real launch could find, both fixed: `health` asked for `session.list`, which the
*app's* control protocol did not have (it was a headless-only method, so a dedicated one was added
to `control.rs` — `ui.inspect` would have meant serializing the whole UI and the active layer tree
every 1.5 seconds); and `health` mapped any dispatch failure to `ui_responsive: false`, so a wrong
method name read as a wedged application. A reply that *is* an error still proves the UI thread is
servicing its queue, so only a timeout counts as unresponsive now.

### What `max_tool_timeout_ms` actually publishes

The app reports its own `REPLY_DEADLINE` — 60 s — which is *less* than the manifest's 300 000 ms
floor. That is the safe direction and the reason the field exists: we give up first, so the model
is never told a call failed while the edit lands anyway. Raising PhotoCraft's own deadline is what
would let a slow tool run longer, and the handshake means the host's wait follows automatically
instead of being hand-edited in two places.

Rules that still apply, from `AGENTS.md`: the bridge is not user-visible behaviour, so it is not a
command — but everything it dispatches is. No panics on any request (this is a network-facing
parser, so §3's "input-derived numbers are hostile" is the whole of it). `cfg(not(target_arch =
"wasm32"))` throughout, and `cargo xtask wasm` to prove it. Tests with every step.

## 5. Open questions

- **Flat image extensions in `opens`.** Claim them and PhotoCraft becomes the default opener for
  every PNG mentioned in a transcript; claim only `psd`/`psb`/`pcraft` and a click on a PNG goes to
  a text editor. Worth a deliberate answer.
- **`allowRendererCalls`.** Letting Erasmus's own UI run PhotoCraft tools directly is a weaker
  statement than the user's AI grant, but it is still a statement. Off until there is a reason.
- ~~**`survivesRestart`.**~~ *Answered by §3a.* It stays false — Erasmus kills what it started, and
  for an editor holding unsaved work that is the honest default. The harness still cannot save for
  us, but health now names every document that has never been written to disk, so it can warn
  before it quits rather than discover the loss afterwards.
- **Window attach and responsiveness.** `attachHint: 'own'` is the default because `SetParent`
  across processes ties the two message queues together, and a program that stops pumping freezes
  the harness. PhotoCraft's measured frame times under load (`docs/scorecard.md` P14: 4.3 s p95 for
  a layer move on a 15000×10000 document) say plainly that it does stop pumping sometimes. `'own'`
  is right, and even then Erasmus's own docs record the remaining risk as unfixed.
