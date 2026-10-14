//! The structured skills document served at `GET /skills`.
//!
//! A tool list says what can be called. It says nothing about when, why, or what happens
//! afterwards — and that gap is what this closes. These are several small named documents rather
//! than one blob so the host can show the two that bear on the question in front of it instead of
//! all of them: Erasmus picks `always` skills plus any whose keywords appear in what the user
//! typed, in the order published, dropping whole skills when the budget runs out.
//!
//! Three rules from `../Erasmus/Skills/erasmus-connection.md` shape everything here, and each one
//! is enforced by [`validate`] rather than left to care:
//!
//! * **Keywords are matched as literal, case-insensitive substrings of what the user typed.** An
//!   uppercase keyword in our own data matches nothing, ever, and nothing on the host's side will
//!   say so.
//! * **A document that fails the host's validation is discarded entirely and silently** — no
//!   retry, nothing logged there. It looks exactly like a program that published no skills, which
//!   is a bug discovered months later as "the assistant doesn't seem to know about this feature".
//!   So we validate before serving and answer 500 naming the problems.
//! * **The budget is per program, and `always` skills spend it first.** A vendor that marks
//!   everything `always` has written one long document and is budgeted as one, so exactly one
//!   skill here is `always` and it is the short one.

use serde::Serialize;

/// The host's default selection budget, in characters of `body`. Mirrors `selectProgramSkills`'s
/// own default in `../Erasmus/shared/schema/connectors.ts`; [`tests::always_skills_stay_under_half_the_budget`]
/// holds us to the "under half" guidance rather than to the limit itself.
pub const SELECTION_BUDGET: usize = 2000;

/// Schema bounds from `../Erasmus/shared/schema/connectors.ts`. Mirrored as constants so a failure
/// names the limit it hit rather than "invalid".
const MAX_SKILLS: usize = 50;
const MAX_TITLE: usize = 80;
const MAX_SUMMARY: usize = 200;
const MAX_BODY: usize = 4000;
const MAX_KEYWORDS: usize = 24;
const MAX_KEYWORD: usize = 40;

/// One named piece of PhotoCraft's own documentation.
#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    pub name: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    pub body: &'static str,
    pub keywords: &'static [&'static str],
    pub always: bool,
}

/// The document shape the host parses: `{ "skills": [...] }`.
#[derive(Debug, Serialize)]
pub struct Document {
    pub skills: &'static [Skill],
}

/// What PhotoCraft publishes.
///
/// Ordering is load-bearing: it is the only ranking signal the host has, and it takes skills in
/// published order until the budget runs out. The general ones come first.
pub const SKILLS: &[Skill] = &[
    Skill {
        name: "overview",
        title: "Driving PhotoCraft",
        summary: "What PhotoCraft is and the one rule that explains its whole tool surface.",
        body: "PhotoCraft is a layered image editor with Photoshop-compatible behaviour and file \
               fidelity. Every user-visible action is a command with an id, so the tool surface is \
               small and the reach is not: `command_list` filters the full catalogue and \
               `command_run` performs any of it, which is how to do anything the 25 named tools \
               do not cover. Prefer a command id over improvising with pixel tools. Documents are \
               open in an indexed session (`session_list`); every tool defaults to the active one, \
               so pass an index only to act on another. Bit depth (8/16/32f) and colour mode \
               (RGB, Grayscale, CMYK, Lab) are properties of the open document, not assumptions \
               to make.",
        keywords: &[],
        always: true,
    },
    Skill {
        name: "saving-and-exporting",
        title: "Saving, exporting and what each format drops",
        summary: "`.pcraft` keeps everything; every other format is an export that loses something.",
        body: "`doc_save` with no `path` writes the document back to its own file, and only for a \
               PSD, PSB or `.pcraft` — anything else needs an explicit path. `.pcraft` is the \
               native format and the only lossless one: it keeps every layer, mask, adjustment, \
               effect and smart object. Saving to `.psd`/`.psb` keeps the layered structure but \
               is not byte-identical to the file that was opened, because image resources, layer \
               records and the composite are rewritten; anything PhotoCraft does not model is \
               carried over verbatim rather than dropped. Flat formats (png, jpg, tif, webp, exr) \
               flatten. Every save returns warnings naming what the chosen format could not \
               hold — read them rather than assuming a clean write. A save to an explicit path \
               overwrites without asking.",
        keywords: &["save", "export", "psd", "pcraft", "overwrite", "write", "flatten", "jpeg", "png"],
        always: false,
    },
    Skill {
        name: "non-destructive-edits",
        title: "Prefer adjustment layers and masks to baked pixels",
        summary: "Most edits have a reversible form; use it unless asked otherwise.",
        body: "An adjustment applied to a layer's pixels cannot be undone once the history is \
               gone; the same adjustment as an adjustment layer stays editable and can be masked, \
               re-ordered and switched off. Unless the request is explicitly to flatten or bake, \
               reach for `layer.newAdjustmentLayer.<kind>` over `image.adjustments.<kind>`, a \
               layer mask over an erased region, and a smart object over a resampled layer. Layer \
               styles (drop shadow, stroke, bevel, glow) are live effects, not pixels. This \
               matters more here than in a one-shot tool because the user keeps working in the \
               same document afterwards.",
        keywords: &["adjustment", "curves", "levels", "mask", "non-destructive", "smart object", "layer style", "effect"],
        always: false,
    },
    Skill {
        name: "slow-operations",
        title: "Operations that take longer than a tool call should",
        summary: "Heavy work returns a job; poll it instead of blocking the call.",
        body: "Some operations genuinely take minutes on a large document: Content-Aware Scale, \
               Content-Aware Fill, large-radius blurs, Select Subject, and saving a 36 MP layered \
               PSD. A command accepts `wait: false`, which returns a job id immediately; \
               `jobs_list` reports progress and `jobs_cancel` stops it. Use that for anything \
               heavy rather than holding one tool call open, because a call that outlives the \
               host's budget is reported to you as a failure while PhotoCraft carries on and \
               commits the edit anyway — leaving you reasoning about a document that no longer \
               matches what you were told. If a call does come back as a timeout, inspect the \
               document before retrying; do not assume nothing happened.",
        keywords: &["slow", "timeout", "job", "progress", "cancel", "wait", "content-aware", "blur", "hang"],
        always: false,
    },
    Skill {
        name: "seeing-the-result",
        title: "Look at the document before saying it worked",
        summary: "`doc_inspect` for structure, `doc_render_preview` and `ui_screenshot` for pixels.",
        body: "PhotoCraft is a visual editor and a command that returned `ok` can still have done \
               the wrong thing — to the wrong layer, outside the selection, or invisibly behind \
               another layer. `doc_inspect` reports the layer tree, sizes, modes and the active \
               selection. `doc_render_preview` renders the composite to a PNG, which is the cheap \
               way to check an edit landed. `ui_inspect` reports the live app's own state (tools, \
               panels, open dialogs) and `ui_screenshot` shows what is actually on screen, \
               including a dialog waiting for input that is blocking the thing you asked for. \
               Check the pixels for anything visual.",
        keywords: &["screenshot", "preview", "render", "inspect", "verify", "check", "see", "look"],
        always: false,
    },
    Skill {
        name: "selections",
        title: "Selections scope almost everything",
        summary: "An active selection silently changes what the next edit touches.",
        body: "Most edits apply within the active selection, so a leftover selection from an \
               earlier step is the usual reason an adjustment appears to have done nothing or to \
               have affected a stripe of the image. `doc_inspect` reports whether one is active. \
               `select.all`, `select.deselect` and `select.inverse` are commands like any other, \
               and the selection commands under `select.modify.*` (feather, expand, contract, \
               smooth, border) take a radius in pixels. A selection converted to a layer mask is \
               how a scoped edit becomes a reversible one.",
        keywords: &["selection", "select", "deselect", "marquee", "lasso", "feather", "crop"],
        always: false,
    },
    Skill {
        name: "opening-files",
        title: "Opening files, and the roots that bound it",
        summary: "`open_file` takes an absolute path; headless automation is confined to its roots.",
        body: "`open_file` takes an absolute path and loads it as a new document, the same as \
               File ▸ Open — that is the tool a host calls when someone clicks a file it matched \
               to PhotoCraft. `doc_open` is the same operation under its own name. PhotoCraft \
               reads PSD and PSB, its own `.pcraft`, camera raw (NEF, CR2, DNG, ARW and others), \
               Affinity documents read-only, and the usual flat formats. A file that fails to \
               load answers with a reason rather than an empty document; a malformed file is an \
               error, never a crash. In headless automation, paths are confined to the configured \
               read and write roots, and a path outside them is refused rather than silently \
               resolved.",
        keywords: &["open", "file", "path", "import", "raw", "nef", "dng", "affinity", "load"],
        always: false,
    },
    Skill {
        name: "colour-management",
        title: "Colour is managed; do not assume sRGB",
        summary: "Profiles, depth and mode are document data, and conversions go through the CMS.",
        body: "A document carries an ICC profile, a colour mode and a bit depth, and none of the \
               three is safe to assume. Converting a document's mode (`image.mode.*`) is a real \
               colour conversion with a rendering intent, not a reinterpretation of the same \
               numbers, and it can be lossy — CMYK to RGB especially. Assigning a profile and \
               converting to one are different operations with different results; pick \
               deliberately. 32-bit float documents are HDR and clip differently from 8-bit ones, \
               so a value that looks correct at 8 bits may not be. When a result looks wrong in \
               colour rather than in structure, check the document's mode and profile first.",
        keywords: &["colour", "color", "profile", "icc", "srgb", "cmyk", "lab", "bit depth", "16-bit", "hdr"],
        always: false,
    },
];

/// The document to serialize for `GET /skills`.
pub const DOCUMENT: Document = Document { skills: SKILLS };

/// Why a skill document would be rejected by the host.
///
/// Returned rather than logged because the route answers 500 with these in the body: a validation
/// failure the host would swallow silently has to be loud on our side instead.
pub fn validate(skills: &[Skill]) -> Vec<String> {
    let mut problems = Vec::new();
    if skills.len() > MAX_SKILLS {
        problems.push(format!("{} skills exceeds the limit of {MAX_SKILLS}", skills.len()));
    }
    for (i, s) in skills.iter().enumerate() {
        let at = |what: &str| format!("skills[{i}] `{}`: {what}", s.name);
        if !super::is_connector_id(s.name) {
            problems.push(at("name must match ^[a-z0-9][a-z0-9_-]{0,63}$"));
        }
        if skills.iter().take(i).any(|other| other.name == s.name) {
            problems.push(at("duplicate name"));
        }
        if s.title.is_empty() || s.title.chars().count() > MAX_TITLE {
            problems.push(at(&format!("title must be 1..={MAX_TITLE} characters")));
        }
        if s.summary.chars().count() > MAX_SUMMARY {
            problems.push(at(&format!("summary must be at most {MAX_SUMMARY} characters")));
        }
        // Every skill needs a summary even though the schema permits an empty one: it is what
        // makes a skill reachable by name when it never auto-loads.
        if s.summary.is_empty() {
            problems.push(at("summary is empty, so the skill cannot be offered by name"));
        }
        if s.body.is_empty() || s.body.chars().count() > MAX_BODY {
            problems.push(at(&format!("body must be 1..={MAX_BODY} characters")));
        }
        if s.keywords.len() > MAX_KEYWORDS {
            problems.push(at(&format!("more than {MAX_KEYWORDS} keywords")));
        }
        for k in s.keywords {
            if k.is_empty() || k.chars().count() > MAX_KEYWORD {
                problems.push(at(&format!("keyword {k:?} must be 1..={MAX_KEYWORD} characters")));
            }
            // The failure this catches is invisible from the host's side: matching is a literal
            // lowercase substring test, so an uppercase keyword can never be selected.
            if k.chars().any(char::is_uppercase) {
                problems.push(at(&format!("keyword {k:?} has an uppercase character and can never match")));
            }
        }
        if !s.always && s.keywords.is_empty() {
            problems.push(at("a situational skill with no keywords can only be reached by its title"));
        }
    }
    problems
}

/// The host's selection, reimplemented so our tests can assert against the real algorithm.
///
/// Mirrors `selectProgramSkills` in `../Erasmus/shared/schema/connectors.ts`: `always` first, then
/// keyword or title matches in published order, dropping whole skills at the budget rather than
/// truncating one. The guidance is explicit that a vendor should mirror this and assert every
/// situational skill is actually selectable; that is what [`tests`] below does.
pub fn select<'a>(skills: &'a [Skill], utterance: &str, budget: usize) -> Vec<&'a Skill> {
    let utterance = utterance.to_lowercase();
    let matches = |s: &Skill| {
        s.always
            || (!utterance.is_empty()
                && (s.keywords.iter().any(|k| !k.is_empty() && utterance.contains(&k.to_lowercase())) || utterance.contains(&s.title.to_lowercase())))
    };
    let mut picked = Vec::new();
    let mut used = 0usize;
    for s in skills.iter().filter(|s| matches(s)) {
        let len = s.body.chars().count();
        if used + len > budget {
            continue;
        }
        picked.push(s);
        used += len;
    }
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_published_document_validates() {
        assert_eq!(validate(SKILLS), Vec::<String>::new(), "the document we serve must be one the host accepts");
    }

    #[test]
    fn it_serializes_to_the_documented_shape() {
        let json = serde_json::to_value(DOCUMENT).expect("serializes");
        let skills = json.get("skills").and_then(|s| s.as_array()).expect("a skills array");
        assert_eq!(skills.len(), SKILLS.len());
        let first = skills.first().expect("at least one skill");
        for key in ["name", "title", "summary", "body", "keywords", "always"] {
            assert!(first.get(key).is_some(), "every skill carries `{key}`");
        }
    }

    #[test]
    fn exactly_one_skill_is_always() {
        let always: Vec<_> = SKILLS.iter().filter(|s| s.always).map(|s| s.name).collect();
        assert_eq!(always, vec!["overview"], "marking more than the universal skill `always` spends the budget on everything");
    }

    #[test]
    fn always_skills_stay_under_half_the_budget() {
        let spent: usize = SKILLS.iter().filter(|s| s.always).map(|s| s.body.chars().count()).sum();
        assert!(spent * 2 < SELECTION_BUDGET, "always skills spend {spent} of {SELECTION_BUDGET}, leaving too little for a situational one");
    }

    /// The check the integration guide asks for by name: a skill whose own first keyword does not
    /// select it can never be reached by a real user utterance, whatever it says.
    #[test]
    fn every_situational_skill_is_selected_by_its_own_first_keyword() {
        for s in SKILLS.iter().filter(|s| !s.always) {
            let keyword = s.keywords.first().expect("a situational skill has keywords");
            let picked = select(SKILLS, keyword, SELECTION_BUDGET);
            assert!(picked.iter().any(|p| p.name == s.name), "`{}` is not selected by its own first keyword {keyword:?}", s.name);
        }
    }

    #[test]
    fn every_keyword_selects_its_own_skill() {
        for s in SKILLS {
            for k in s.keywords {
                let picked = select(SKILLS, &format!("please {k} this"), SELECTION_BUDGET);
                assert!(picked.iter().any(|p| p.name == s.name), "`{}` is not selected by its keyword {k:?}", s.name);
            }
        }
    }

    #[test]
    fn an_unrelated_request_gets_only_the_always_skill() {
        let picked = select(SKILLS, "what is the capital of france", SELECTION_BUDGET);
        assert_eq!(picked.iter().map(|s| s.name).collect::<Vec<_>>(), vec!["overview"]);
    }

    #[test]
    fn an_empty_utterance_selects_only_always() {
        let picked = select(SKILLS, "", SELECTION_BUDGET);
        assert!(picked.iter().all(|s| s.always), "a question that is not about PhotoCraft gets nothing situational");
    }

    #[test]
    fn matching_is_case_insensitive_on_the_utterance() {
        let picked = select(SKILLS, "Export This As A PNG", SELECTION_BUDGET);
        assert!(picked.iter().any(|s| s.name == "saving-and-exporting"), "an utterance's own case must not matter");
    }

    #[test]
    fn whole_skills_are_dropped_at_the_budget_never_truncated() {
        // A budget that fits the always skill and nothing else.
        let always_len: usize = SKILLS.iter().filter(|s| s.always).map(|s| s.body.chars().count()).sum();
        let picked = select(SKILLS, "save export mask selection colour", always_len);
        assert_eq!(picked.len(), 1, "only the always skill fits");
        let total: usize = picked.iter().map(|s| s.body.chars().count()).sum();
        assert!(total <= always_len, "selection never exceeds the budget");
    }

    #[test]
    fn validate_catches_an_uppercase_keyword() {
        const BAD: &[Skill] = &[Skill { name: "bad", title: "Bad", summary: "Has an uppercase keyword.", body: "body", keywords: &["PSD"], always: false }];
        let problems = validate(BAD);
        assert!(problems.iter().any(|p| p.contains("uppercase")), "{problems:?}");
    }

    #[test]
    fn validate_catches_a_bad_name_a_duplicate_and_an_empty_summary() {
        const BAD: &[Skill] = &[
            Skill { name: "Bad Name", title: "t", summary: "s", body: "b", keywords: &["k"], always: false },
            Skill { name: "dup", title: "t", summary: "", body: "b", keywords: &["k"], always: false },
            Skill { name: "dup", title: "t", summary: "s", body: "b", keywords: &["k"], always: false },
        ];
        let problems = validate(BAD);
        assert!(problems.iter().any(|p| p.contains("name must match")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("duplicate name")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("summary is empty")), "{problems:?}");
    }

    #[test]
    fn validate_catches_a_situational_skill_with_no_keywords() {
        const BAD: &[Skill] = &[Skill { name: "ok", title: "Ok", summary: "s", body: "b", keywords: &[], always: false }];
        let problems = validate(BAD);
        assert!(problems.iter().any(|p| p.contains("no keywords")), "{problems:?}");
    }
}
