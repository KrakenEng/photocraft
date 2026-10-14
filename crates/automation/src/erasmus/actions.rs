//! The actions document served at `GET /actions`.
//!
//! An action is a curated tool call: a label a person recognizes, a sentence, the arguments the
//! vendor has fixed, a schema for the rest, and — the part with teeth — a declared `effect`.
//! Performing one issues an ordinary `tools/call` over the same bridge with the same auth, the
//! same timeout and the same per-program permission. It is never a second way in.
//!
//! **Why PhotoCraft publishes one at all.** Without this document every one of PhotoCraft's tools
//! is reachable by an automated caller with no per-tool distinction between "reads the layer tree"
//! and "overwrites the user's PSD". That distinction exists nowhere else in the system: `effect`
//! is the vendor's declaration and the harness's gate, in that order. `read` and `modify` may be
//! automated once the user has granted the program general permission; **`destructive` is never
//! handed to an automated caller regardless of what any permission says** — only a person, after
//! confirmation.
//!
//! ## Two rules this module enforces structurally, both learned from Perdix
//!
//! **`effect` is derived, never written down twice.** [`TOOL_EFFECT`] is the single place a tool's
//! effect is stated, and [`build`] reads each action's effect from it. An [`ActionSpec`] carries
//! only what the tool list cannot know — the name a person would use for the tool. A hand-copied
//! effect is a value that drifts away from the thing it describes, and this is the one value where
//! drifting in the permissive direction means a model reaching for something the harness believed
//! it could not.
//!
//! **A tool whose effect depends on an argument cannot be an action.** `command_run`,
//! `command_batch`, `control_call` and `ui_menu_invoke` all take the name of the operation to
//! perform. Their effect is whatever was passed, so no honest entry in [`TOOL_EFFECT`] exists for
//! them and [`build`] refuses to make one an action. This is not theoretical tidiness: the host
//! merges a caller's arguments **over** the vendor's fixed ones
//! (`mergeActionArguments` in `../Erasmus/shared/schema/connectors.ts`) and does not validate the
//! caller's object against `input` first. So an action pinning `{"command": "edit.undo"}` and
//! declaring itself `modify` can be called with `{"command": "file.close"}` and will run it — a
//! destructive operation through an action the harness was told was benign. A pinned argument is a
//! default, not a constraint, and nothing that must be constrained can be expressed this way.
//!
//! Nothing is lost in capability by either rule. `preferOverTools` is false, so the raw tools stay
//! visible; an arbitrary-command escape hatch is still reachable when the user has granted general
//! tool access, which is the honest place for it. What goes away is a truthful-looking label on
//! something that could do anything.

use serde::Serialize;
use serde_json::{Value, json};

/// Schema bounds from `../Erasmus/shared/schema/connectors.ts`.
const MAX_ACTIONS: usize = 64;
const MAX_LABEL: usize = 60;
const MAX_SUMMARY: usize = 300;
const MAX_TOOL: usize = 120;
const MAX_KEYWORDS: usize = 24;
const MAX_KEYWORD: usize = 40;

/// What performing an action does to the user's work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    /// Answers a question and changes nothing.
    Read,
    /// Changes the open document.
    Modify,
    /// Loses something that is not coming back.
    Destructive,
}

/// Each tool's effect: the one place the classification is written down.
///
/// Absence is meaningful and is the whole of the second rule above. A tool missing from this table
/// is one whose effect is not a property of the tool — it is whatever its arguments asked for — so
/// it has no truthful effect to declare and cannot back an action. The four such tools today are
/// `command_run`, `command_batch`, `control_call` and `ui_menu_invoke`.
///
/// `doc_save` is `Destructive` because writing a document back over its own file loses the
/// previous contents of that file. `doc_export` is `Modify` because it refuses a path that already
/// exists rather than overwriting it, so it cannot lose anything — which is what keeps an
/// automatable way to produce output available at all.
pub const TOOL_EFFECT: &[(&str, Effect)] = &[
    ("command_list", Effect::Read),
    ("doc_close", Effect::Destructive),
    ("doc_export", Effect::Modify),
    ("doc_export_data_sets", Effect::Modify),
    ("doc_export_layers", Effect::Modify),
    ("doc_import_data_sets", Effect::Modify),
    ("doc_inspect", Effect::Read),
    ("doc_new", Effect::Modify),
    ("doc_open", Effect::Modify),
    ("doc_place", Effect::Modify),
    ("doc_render_preview", Effect::Read),
    ("doc_save", Effect::Destructive),
    ("doc_select", Effect::Modify),
    ("jobs_cancel", Effect::Modify),
    ("jobs_list", Effect::Read),
    ("open_file", Effect::Modify),
    ("render_preview", Effect::Read),
    ("session_list", Effect::Read),
    ("ui_inspect", Effect::Read),
    ("ui_pointer", Effect::Modify),
    ("ui_screenshot", Effect::Read),
    ("ui_set", Effect::Modify),
];

/// Tools that take the name of the operation to perform, so their effect is their caller's choice
/// rather than their own. Listed only to say so in an error; the operative fact is their absence
/// from [`TOOL_EFFECT`].
pub const OPERATION_DISPATCHING_TOOLS: &[&str] = &["command_batch", "command_run", "control_call", "ui_menu_invoke"];

/// Every tool name `PhotocraftMcp` publishes, so [`validate`] can refuse an action naming a tool
/// that does not exist before the host discovers it at call time.
///
/// Kept as a list rather than read from the router because this module deliberately knows nothing
/// about rmcp; `tests/mcp.rs::erasmus_known_tools_match_the_server` is what holds the two together.
pub const KNOWN_TOOLS: &[&str] = &[
    "command_batch",
    "command_list",
    "command_run",
    "control_call",
    "doc_close",
    "doc_export",
    "doc_export_data_sets",
    "doc_export_layers",
    "doc_import_data_sets",
    "doc_inspect",
    "doc_new",
    "doc_open",
    "doc_place",
    "doc_render_preview",
    "doc_save",
    "doc_select",
    "jobs_cancel",
    "jobs_list",
    "open_file",
    "render_preview",
    "session_list",
    "ui_inspect",
    "ui_menu_invoke",
    "ui_pointer",
    "ui_screenshot",
    "ui_set",
];

/// The effect of `tool`, or `None` when it has none that is a property of the tool.
pub fn effect_of(tool: &str) -> Option<Effect> {
    TOOL_EFFECT.iter().find(|(name, _)| *name == tool).map(|(_, effect)| *effect)
}

/// What the tool list cannot know: the name a person would use for a tool, and when to reach for
/// it. Deliberately has no `effect` field — see this module's header.
#[derive(Debug, Clone)]
pub struct ActionSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub summary: &'static str,
    pub tool: &'static str,
    pub keywords: &'static [&'static str],
    /// JSON Schema for what the caller supplies. `None` means the action takes no arguments.
    pub input: Option<fn() -> Value>,
}

/// One named operation, as the host parses it.
#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub id: &'static str,
    pub label: &'static str,
    pub summary: &'static str,
    pub tool: &'static str,
    /// Always empty. A fixed argument is a default the caller overrides, not a constraint, so
    /// nothing here relies on one; the field stays because the host's schema expects it.
    pub arguments: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    pub effect: Effect,
    pub confirm: bool,
    pub keywords: &'static [&'static str],
}

/// The document shape the host parses: `{ "actions": [...] }`.
#[derive(Debug, Serialize)]
pub struct Document {
    pub actions: Vec<Action>,
}

/// What PhotoCraft publishes. Reads first, then the modifies, then the two that lose something.
pub const SPECS: &[ActionSpec] = &[
    ActionSpec {
        id: "list-documents",
        label: "List open documents",
        summary: "Every open document with its index, name, size and whether it has unsaved changes, plus which one is active.",
        tool: "session_list",
        keywords: &["documents", "open", "list", "session", "tabs"],
        input: None,
    },
    ActionSpec {
        id: "inspect-document",
        label: "Inspect the document",
        summary: "The layer tree, canvas size, colour mode, bit depth and active selection of the current document.",
        tool: "doc_inspect",
        keywords: &["inspect", "layers", "structure", "size", "mode", "selection"],
        input: None,
    },
    ActionSpec {
        id: "preview-document",
        label: "Render a preview",
        summary: "Render the current composite to a PNG, to check what the document actually looks like now.",
        tool: "doc_render_preview",
        keywords: &["preview", "render", "thumbnail", "composite", "look"],
        input: Some(|| {
            json!({
                "type": "object",
                "properties": { "maxSide": { "type": "integer", "description": "Longest side in pixels (default: a thumbnail)." } },
                "additionalProperties": false
            })
        }),
    },
    ActionSpec {
        id: "screenshot-app",
        label: "Screenshot the window",
        summary: "What is on screen right now, including any dialog waiting for input that is blocking an edit.",
        tool: "ui_screenshot",
        keywords: &["screenshot", "screen", "window", "dialog", "stuck"],
        input: None,
    },
    ActionSpec {
        id: "find-command",
        label: "Find a command",
        summary: "Search the full command catalogue by name. Everything PhotoCraft's menus do is a command that can be run by id.",
        tool: "command_list",
        keywords: &["command", "find", "search", "menu", "catalogue", "catalog"],
        input: Some(|| {
            json!({
                "type": "object",
                "properties": { "filter": { "type": "string", "description": "Substring matched against command ids and labels." } },
                "additionalProperties": false
            })
        }),
    },
    ActionSpec {
        id: "open-document",
        label: "Open a file",
        summary: "Open an image or document from an absolute path as a new document, the same as File ▸ Open.",
        // `open_file` rather than `doc_open`, though they do the same thing: this is the name the
        // manifest's `opens` block promises, so the two surfaces stay the same call.
        tool: "open_file",
        keywords: &["open", "load", "file", "import"],
        input: Some(|| {
            json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Absolute path to the file to open." } },
                "required": ["path"],
                "additionalProperties": false
            })
        }),
    },
    ActionSpec {
        id: "export-copy",
        label: "Export a copy",
        summary: "Write the document to a new file. A path that already exists is refused rather than overwritten, so this cannot lose anything.",
        tool: "doc_export",
        keywords: &["export", "copy", "png", "jpeg", "jpg", "save as", "output"],
        input: Some(|| {
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path to write. Must not already exist." },
                    "format": { "type": "string", "description": "Override the format implied by the extension." },
                    "quality": { "type": "integer", "description": "For lossy formats." }
                },
                "required": ["path"],
                "additionalProperties": false
            })
        }),
    },
    ActionSpec {
        id: "cancel-job",
        label: "Cancel a running job",
        summary: "Stop a background operation started with `wait: false`. Its partial work is discarded, not committed.",
        tool: "jobs_cancel",
        keywords: &["cancel", "stop", "abort", "job"],
        input: Some(|| {
            json!({
                "type": "object",
                "properties": { "job": { "type": "string", "description": "Job id from jobs_list (default: the running one)." } },
                "additionalProperties": false
            })
        }),
    },
    ActionSpec {
        id: "save-in-place",
        label: "Save over the original file",
        summary: "Write the document back to its own file. The previous contents of that file are gone, which is why a person has to ask for it.",
        tool: "doc_save",
        keywords: &["save", "overwrite", "write back", "in place"],
        input: None,
    },
    ActionSpec {
        id: "close-document",
        label: "Close the document",
        summary: "Close the current document. Anything unsaved in it is lost.",
        tool: "doc_close",
        keywords: &["close", "discard", "quit document"],
        input: Some(|| {
            json!({
                "type": "object",
                "properties": { "index": { "type": "integer", "description": "Document index (default: the active one)." } },
                "additionalProperties": false
            })
        }),
    },
];

/// Why the document could not be built. A spec naming an unclassifiable tool is a programming
/// error caught by [`tests::the_published_document_builds`], not something to serve around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildError {
    pub action: &'static str,
    pub why: String,
}

/// Turn the specs into the document, deriving each effect from [`TOOL_EFFECT`].
///
/// Fails rather than guessing. An absent effect means the tool's effect is its caller's choice,
/// and the safe-looking fallback — treat it as `modify` — is exactly the wrong one here: it would
/// publish an arbitrary-operation tool under a label the harness trusts.
pub fn build(specs: &[ActionSpec]) -> Result<Vec<Action>, BuildError> {
    specs
        .iter()
        .map(|spec| {
            let Some(effect) = effect_of(spec.tool) else {
                let why = if OPERATION_DISPATCHING_TOOLS.contains(&spec.tool) {
                    format!("`{}` takes the operation to perform as an argument, so it has no effect of its own to declare", spec.tool)
                } else {
                    format!("`{}` has no entry in TOOL_EFFECT", spec.tool)
                };
                return Err(BuildError { action: spec.id, why });
            };
            Ok(Action {
                id: spec.id,
                label: spec.label,
                summary: spec.summary,
                tool: spec.tool,
                arguments: json!({}),
                input: spec.input.map(|f| f()),
                effect,
                // Forced by the host for a destructive action anyway; derived here so the two
                // cannot disagree about what this document asked for.
                confirm: effect == Effect::Destructive,
                keywords: spec.keywords,
            })
        })
        .collect()
}

/// The document to serialize for `GET /actions`.
pub fn document() -> Result<Document, BuildError> {
    Ok(Document { actions: build(SPECS)? })
}

/// Why an actions document would be rejected, or would misbehave once accepted.
pub fn validate(actions: &[Action], known_tools: &[&str]) -> Vec<String> {
    let mut problems = Vec::new();
    if actions.len() > MAX_ACTIONS {
        problems.push(format!("{} actions exceeds the limit of {MAX_ACTIONS}", actions.len()));
    }
    for (i, a) in actions.iter().enumerate() {
        let at = |what: &str| format!("actions[{i}] `{}`: {what}", a.id);
        if !super::is_connector_id(a.id) {
            problems.push(at("id must match ^[a-z0-9][a-z0-9_-]{0,63}$"));
        }
        if actions.iter().take(i).any(|other| other.id == a.id) {
            problems.push(at("duplicate id"));
        }
        if a.label.is_empty() || a.label.chars().count() > MAX_LABEL {
            problems.push(at(&format!("label must be 1..={MAX_LABEL} characters")));
        }
        if a.summary.is_empty() || a.summary.chars().count() > MAX_SUMMARY {
            problems.push(at(&format!("summary must be 1..={MAX_SUMMARY} characters")));
        }
        if a.tool.is_empty() || a.tool.chars().count() > MAX_TOOL {
            problems.push(at(&format!("tool must be 1..={MAX_TOOL} characters")));
        }
        // An action naming a tool we do not publish fails at call time, on the host's side, after
        // a person has already clicked it.
        if !known_tools.contains(&a.tool) {
            problems.push(at(&format!("tool `{}` is not in tools/list", a.tool)));
        }
        // The effect must still be the one the table states, even for an Action built by hand.
        match effect_of(a.tool) {
            None => problems.push(at(&format!("`{}` has no effect of its own and cannot back an action", a.tool))),
            Some(expected) if expected != a.effect => {
                problems.push(at(&format!("effect is {:?} but `{}` is registered {expected:?}", a.effect, a.tool)));
            }
            Some(_) => {}
        }
        // A fixed argument is merged *under* the caller's and is not validated against `input`,
        // so it is a default and never a constraint. An action that appears to pin something is
        // claiming a guarantee it does not have.
        if !a.arguments.as_object().is_some_and(serde_json::Map::is_empty) {
            problems.push(at("arguments must be empty: a fixed argument is overridden by the caller, so it cannot constrain anything"));
        }
        if let Some(input) = &a.input
            && input.get("type").and_then(Value::as_str) != Some("object")
        {
            problems.push(at("input schema must be an object schema"));
        }
        if a.effect == Effect::Destructive && !a.confirm {
            problems.push(at("a destructive action should declare confirm: true"));
        }
        if a.keywords.len() > MAX_KEYWORDS {
            problems.push(at(&format!("more than {MAX_KEYWORDS} keywords")));
        }
        for k in a.keywords {
            if k.is_empty() || k.chars().count() > MAX_KEYWORD {
                problems.push(at(&format!("keyword {k:?} must be 1..={MAX_KEYWORD} characters")));
            }
            if k.chars().any(char::is_uppercase) {
                problems.push(at(&format!("keyword {k:?} has an uppercase character and can never match")));
            }
        }
    }
    problems
}

/// Whether an assistant may reach for this action unattended, once permitted. Mirrors
/// `actionIsAutomatable` in `../Erasmus/shared/schema/connectors.ts`.
pub fn is_automatable(action: &Action) -> bool {
    action.effect != Effect::Destructive && !action.confirm
}

#[cfg(test)]
mod tests {
    use super::*;

    fn built() -> Vec<Action> {
        build(SPECS).expect("the published specs build")
    }

    #[test]
    fn the_published_document_builds() {
        let actions = built();
        assert_eq!(actions.len(), SPECS.len());
    }

    #[test]
    fn the_published_document_validates() {
        assert_eq!(validate(&built(), KNOWN_TOOLS), Vec::<String>::new());
    }

    #[test]
    fn it_serializes_to_the_documented_shape() {
        let json = serde_json::to_value(document().expect("builds")).expect("serializes");
        let list = json.get("actions").and_then(|a| a.as_array()).cloned().expect("an actions array");
        assert_eq!(list.len(), SPECS.len());
        let first = list.first().expect("at least one action");
        for key in ["id", "label", "summary", "tool", "arguments", "effect", "confirm", "keywords"] {
            assert!(first.get(key).is_some(), "every action carries `{key}`");
        }
        assert_eq!(first.get("effect").and_then(Value::as_str), Some("read"), "effect serializes as the lowercase wire value");
    }

    #[test]
    fn an_action_with_no_arguments_omits_input_rather_than_sending_null() {
        let json = serde_json::to_value(document().expect("builds")).expect("serializes");
        let list = json.get("actions").and_then(|a| a.as_array()).cloned().expect("array");
        let save = list.iter().find(|a| a.get("id").and_then(Value::as_str) == Some("save-in-place")).expect("the save action");
        assert!(save.get("input").is_none(), "absent means no arguments; null would be a schema that parses as one");
    }

    /// The rule that matters most here. The host merges a caller's arguments over the vendor's and
    /// does not check them against `input`, so a pinned value guarantees nothing — and an action
    /// that looked like it pinned one would be claiming an effect it cannot hold to.
    #[test]
    fn no_action_relies_on_a_fixed_argument() {
        for a in built() {
            assert_eq!(a.arguments, json!({}), "`{}` pins an argument the caller can simply override", a.id);
        }
    }

    /// The other half of the same rule: a tool whose effect is its caller's choice can never
    /// appear, whatever a spec says.
    #[test]
    fn an_operation_dispatching_tool_cannot_back_an_action() {
        for tool in OPERATION_DISPATCHING_TOOLS {
            assert!(effect_of(tool).is_none(), "`{tool}` must have no registered effect");
            let spec = [ActionSpec { id: "sneaky", label: "Sneaky", summary: "s", tool, keywords: &["s"], input: None }];
            let error = build(&spec).expect_err("must refuse to build");
            assert_eq!(error.action, "sneaky");
            assert!(error.why.contains("as an argument"), "{}", error.why);
        }
        for spec in SPECS {
            assert!(!OPERATION_DISPATCHING_TOOLS.contains(&spec.tool), "`{}` is published on an unclassifiable tool", spec.id);
        }
    }

    #[test]
    fn every_effect_comes_from_the_table_not_the_spec() {
        for a in built() {
            assert_eq!(Some(a.effect), effect_of(a.tool), "`{}`'s effect disagrees with TOOL_EFFECT", a.id);
        }
    }

    #[test]
    fn the_effect_table_covers_every_tool_it_honestly_can() {
        let classified: Vec<&str> = TOOL_EFFECT.iter().map(|(t, _)| *t).collect();
        for tool in KNOWN_TOOLS {
            let expected_absent = OPERATION_DISPATCHING_TOOLS.contains(tool);
            assert_eq!(
                !classified.contains(tool),
                expected_absent,
                "`{tool}` is {} TOOL_EFFECT but {} an operation-dispatching tool",
                if classified.contains(tool) { "in" } else { "missing from" },
                if expected_absent { "is" } else { "is not" }
            );
        }
        for tool in &classified {
            assert!(KNOWN_TOOLS.contains(tool), "TOOL_EFFECT classifies `{tool}`, which is not a published tool");
        }
    }

    #[test]
    fn everything_that_loses_work_is_destructive() {
        let all = built();
        for id in ["save-in-place", "close-document"] {
            let a = all.iter().find(|a| a.id == id).unwrap_or_else(|| panic!("{id} exists"));
            assert_eq!(a.effect, Effect::Destructive, "{id} overwrites or discards the user's work");
            assert!(a.confirm, "{id} must declare its confirmation");
            assert!(!is_automatable(a), "{id} must never be reachable by an automated caller");
        }
    }

    #[test]
    fn the_assistant_keeps_a_way_to_produce_output_that_cannot_lose_anything() {
        let all = built();
        let export = all.iter().find(|a| a.id == "export-copy").expect("export-copy exists");
        assert_eq!(export.effect, Effect::Modify);
        assert!(is_automatable(export), "classifying every write as destructive would leave no automatable way to produce a file");
    }

    #[test]
    fn every_read_action_is_automatable_and_changes_nothing() {
        for a in built().iter().filter(|a| a.effect == Effect::Read) {
            assert!(is_automatable(a), "`{}` is a read and should not need a confirmation", a.id);
        }
    }

    #[test]
    fn validate_catches_a_tool_that_does_not_exist() {
        let bad = vec![Action {
            id: "ghost",
            label: "Ghost",
            summary: "Names a tool we do not publish.",
            tool: "no_such_tool",
            arguments: json!({}),
            input: None,
            effect: Effect::Read,
            confirm: false,
            keywords: &["ghost"],
        }];
        let problems = validate(&bad, KNOWN_TOOLS);
        assert!(problems.iter().any(|p| p.contains("not in tools/list")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("cannot back an action")), "{problems:?}");
    }

    #[test]
    fn validate_catches_an_effect_that_disagrees_with_the_table() {
        let bad = vec![Action {
            id: "liar",
            label: "Liar",
            summary: "Claims a save only modifies.",
            tool: "doc_save",
            arguments: json!({}),
            input: None,
            effect: Effect::Modify,
            confirm: false,
            keywords: &["liar"],
        }];
        let problems = validate(&bad, KNOWN_TOOLS);
        assert!(problems.iter().any(|p| p.contains("registered Destructive")), "{problems:?}");
    }

    #[test]
    fn validate_catches_a_fixed_argument() {
        let bad = vec![Action {
            id: "pinned",
            label: "Pinned",
            summary: "Pretends to pin an argument.",
            tool: "doc_inspect",
            arguments: json!({ "index": 0 }),
            input: None,
            effect: Effect::Read,
            confirm: false,
            keywords: &["pinned"],
        }];
        let problems = validate(&bad, KNOWN_TOOLS);
        assert!(problems.iter().any(|p| p.contains("overridden by the caller")), "{problems:?}");
    }

    #[test]
    fn validate_catches_a_destructive_action_that_forgot_to_confirm() {
        let bad = vec![Action {
            id: "risky",
            label: "Risky",
            summary: "Destructive without confirm.",
            tool: "doc_close",
            arguments: json!({}),
            input: None,
            effect: Effect::Destructive,
            confirm: false,
            keywords: &["risky"],
        }];
        let problems = validate(&bad, KNOWN_TOOLS);
        assert!(problems.iter().any(|p| p.contains("confirm: true")), "{problems:?}");
    }

    #[test]
    fn validate_catches_an_uppercase_keyword_and_a_duplicate_id() {
        let bad = vec![
            Action {
                id: "dup",
                label: "A",
                summary: "s",
                tool: "doc_inspect",
                arguments: json!({}),
                input: None,
                effect: Effect::Read,
                confirm: false,
                keywords: &["PSD"],
            },
            Action {
                id: "dup",
                label: "B",
                summary: "s",
                tool: "doc_inspect",
                arguments: json!({}),
                input: None,
                effect: Effect::Read,
                confirm: false,
                keywords: &["psd"],
            },
        ];
        let problems = validate(&bad, KNOWN_TOOLS);
        assert!(problems.iter().any(|p| p.contains("uppercase")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("duplicate id")), "{problems:?}");
    }
}
