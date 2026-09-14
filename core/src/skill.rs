//! Skills: spec, trait, parsed/validated output.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{ParseError, PatchError};
use crate::model::{Action, RawOutput, StepOutput};
use crate::patch::{CheckedPatch, InboxAcked, PatchStrategy, RawPatch, ValidatedPatch};
use crate::promptable::{Promptable, sealed};
use crate::report::Report;
use crate::version::{CommittedState, HarnessState, Version};

/// A harness-trusted JSON merge-patch value: the raw wire shape a skill's
/// [`Skill::after_tool`] hook returns to write durable, harness-owned state
/// (e.g. `seen`, milestone 10) after a tool call. Applied by the session
/// with `apply_trusted_patch`-style top-level field replacement -- each key
/// present in the value becomes the field's new value outright, not a
/// deep/keyed-array merge -- so a hook that wants to shrink or reorder an
/// array (e.g. evicting an entry) can just send the finished array.
pub type Patch = Value;

/// A binding decision: the mutating call the model has committed to making
/// next, set in the workspace before it starts reading toward that call.
/// While a commitment is set, the session's commitment-slot rule (see
/// `Active::step` in `session.rs`) limits how many read-only calls on the
/// committed path it may make before the harness expects the call itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Commitment {
    /// The mutating tool you will call next: edit_file, write_file, shell, or test.
    pub tool: String,
    /// The file that call will touch, when it touches one.
    #[serde(default)]
    pub path: Option<String>,
    /// One line: exactly what the call will do (e.g. "delete the duplicated `if` at lines 742-744", "insert the owned-fields loop before the ask_user check").
    /// Defaults to empty so a patch that omits it is caught by the skill's
    /// commitment rule ("`next.what` must say ...") instead of a bare
    /// "missing field" parse error.
    #[serde(default)]
    pub what: String,
}

/// Activation-specific instructions. Immutable per activation (C1): no
/// `&mut` accessors.
pub struct SkillSpec {
    name: String,
    instructions: String,
}

impl SkillSpec {
    pub fn new(name: impl Into<String>, instructions: impl Into<String>) -> Self {
        SkillSpec {
            name: name.into(),
            instructions: instructions.into(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn instructions(&self) -> &str {
        &self.instructions
    }
}

impl sealed::Sealed for Arc<SkillSpec> {}
impl Promptable for Arc<SkillSpec> {
    fn render(&self) -> String {
        format!("# {}\n{}", self.name, self.instructions)
    }
}

/// The output of core's parser: `StepOutput<P::Patch>` re-serialized as
/// `RawPatch`, plus action, acks, and the prompt version it answers.
pub struct ParsedOutput<W> {
    patch: RawPatch,
    action: Action,
    ack_inbox: Vec<u64>,
    version: Version,
    _marker: std::marker::PhantomData<W>,
}

impl<W> std::fmt::Debug for ParsedOutput<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedOutput")
            .field("patch", &self.patch)
            .field("action", &self.action)
            .field("ack_inbox", &self.ack_inbox)
            .field("version", &self.version)
            .finish()
    }
}

impl<W> ParsedOutput<W> {
    /// Deserialize `raw.content` into `StepOutput<P::Patch>` (this is the
    /// schema check). Tolerates leading/trailing whitespace and a fenced
    /// ```json block around the object.
    pub fn parse<P: PatchStrategy<W>>(raw: RawOutput) -> Result<ParsedOutput<W>, ParseError> {
        let (out, _fixes) = Self::parse_with_fixes::<P>(raw)?;
        Ok(out)
    }

    /// Like [`parse`](Self::parse), but also returns the list of
    /// normalizations applied to the raw JSON before deserializing (e.g. a
    /// local model sending the tool name as `action.type`). Empty when the
    /// output was already well-formed.
    pub fn parse_with_fixes<P: PatchStrategy<W>>(
        raw: RawOutput,
    ) -> Result<(ParsedOutput<W>, Vec<String>), ParseError> {
        let version = raw.version();
        let cleaned = strip_fence(raw.content.trim());
        let mut fixes: Vec<String> = Vec::new();
        let mut value: Value = match serde_json::from_str(cleaned) {
            Ok(v) => v,
            Err(e) => match repair_json(cleaned, &mut fixes) {
                Some(v) => v,
                None => return Err(ParseError::InvalidJson(e.to_string())),
            },
        };
        // `Action::Continue` is harness-internal (see its doc in
        // `core/src/model.rs`): it is never offered to the model (absent
        // from the flat `JsonSchema` enum), so a model output naming it is
        // always a mistake or an attempted jailbreak, never a legitimate
        // choice. Checked before `normalize_action` (which would otherwise
        // treat the unrecognized-by-`normalize_action` tag "continue" as a
        // tool name and rewrite it into a tool call) and before
        // `StepOutput`'s own `serde` deserialization (whose tag would
        // otherwise happily accept it, since `Action::Continue` carries the
        // same `rename_all = "snake_case"` derive as every other variant),
        // so it gets the same "unknown action type" rejection shape an
        // actually-unknown `type` value gets from serde.
        if value
            .get("action")
            .and_then(|a| a.get("type"))
            .and_then(Value::as_str)
            == Some("continue")
        {
            return Err(ParseError::InvalidJson(
                "unknown action type \"continue\": not a valid Action".to_string(),
            ));
        }
        fixes.extend(normalize_action(&mut value));

        // A well-formed `patch` with no `action` at all is not the model
        // choosing anything -- with thinking on, the server does not
        // enforce the schema, and a bench run sent this shape four times in
        // a row, hitting the retry cap on what should have been a
        // recoverable step. Rather than rejecting (the schema still
        // requires `action` -- this is not a model-facing relaxation, see
        // `StepOutput`), treat it as the harness-internal `Action::Continue`:
        // the patch commits, the model is told every output needs an
        // action. Only this exact shape (an object, a `patch` member that
        // is itself an object, and NO `action` member) is recovered this
        // way; `action` present-but-malformed, `patch` missing, or the
        // value not an object at all all keep falling through to the
        // ordinary deserialize below and its existing error.
        if let Value::Object(map) = &value
            && matches!(map.get("patch"), Some(Value::Object(_)))
            && !map.contains_key("action")
        {
            let raw_patch_value = map.get("patch").cloned().unwrap_or(Value::Null);
            // Same schema check the ordinary path applies to `patch`
            // (deserialize into `P::Patch`, then re-serialize): a `patch`
            // that is well-formed JSON but not a valid `P::Patch` should
            // still fail to parse, not be silently accepted just because
            // `action` happened to be missing too.
            let typed_patch: P::Patch = serde_json::from_value(raw_patch_value)
                .map_err(|e| ParseError::InvalidJson(e.to_string()))?;
            let patch_value = serde_json::to_value(&typed_patch)
                .map_err(|e| ParseError::InvalidJson(e.to_string()))?;
            let ack_inbox = map
                .get("ack_inbox")
                .and_then(|v| serde_json::from_value::<Vec<u64>>(v.clone()).ok())
                .unwrap_or_default();
            return Ok((
                ParsedOutput {
                    patch: RawPatch(patch_value),
                    action: Action::Continue {
                        note: "your output had no `action`; the patch was kept. Every output \
                               needs an action: a tool call, `done`, or `ask_user`."
                            .to_string(),
                    },
                    ack_inbox,
                    version,
                    _marker: std::marker::PhantomData,
                },
                fixes,
            ));
        }

        let parsed: StepOutput<P::Patch> =
            serde_json::from_value(value).map_err(classify_step_output_error)?;
        let patch_value = serde_json::to_value(&parsed.patch)
            .map_err(|e| ParseError::InvalidJson(e.to_string()))?;
        Ok((
            ParsedOutput {
                patch: RawPatch(patch_value),
                action: parsed.action,
                ack_inbox: parsed.ack_inbox,
                version,
                _marker: std::marker::PhantomData,
            },
            fixes,
        ))
    }

    pub fn patch(&self) -> &RawPatch {
        &self.patch
    }

    /// Mutable access to the raw merge patch, for a `Skill::validate`
    /// override that needs to inject or fix up a key before calling
    /// [`validate_default`] (e.g. `plan`'s bare-`done` recovery: injecting
    /// `next_skill` when the model sent `done` with none set).
    pub fn patch_mut(&mut self) -> &mut RawPatch {
        &mut self.patch
    }

    pub fn action(&self) -> &Action {
        &self.action
    }

    /// Replaces the parsed action, for a `Skill::validate` override that
    /// converts what the model sent into the harness-internal
    /// `Action::Continue` instead of rejecting the step outright (e.g. the
    /// coding skills' premature-`done`/empty-`ask_user` conversions in
    /// `skills/src/coding/mod.rs`). The replaced action is what
    /// `validate_default` (or a fully custom `validate`) sees and acts on
    /// from this point on.
    pub fn set_action(&mut self, action: Action) {
        self.action = action;
    }

    pub fn ack_inbox(&self) -> &[u64] {
        &self.ack_inbox
    }

    pub fn version(&self) -> Version {
        self.version
    }
}

/// Normalizes known malformed `action` shapes in place, returning a
/// human-readable description of each fix applied:
///
/// - `{"type": "<tool-name>", ...}` where `<tool-name>` is not one of the
///   real action tags (`tool`/`ask_user`/`yield`/`done`) is treated as a
///   tool call named `<tool-name>`: rewritten to
///   `{"type": "tool", "name": "<tool-name>", "args": ...}`. Any existing
///   `name` is left as is; only `type` is fixed.
/// - `{"type": "tool", ...}` with no `name` but a `tool` or `tool_name`
///   string key uses that key's value as `name`.
/// - Keys that belong at the top level (`ack_inbox`, `patch`) but were
///   nested inside `action` by the model (typically because a repaired
///   truncation left them stranded there) are hoisted back out, provided
///   the top level doesn't already have one.
fn normalize_action(value: &mut Value) -> Vec<String> {
    let mut fixes = Vec::new();
    let Some(action) = value.get_mut("action").and_then(Value::as_object_mut) else {
        return fixes;
    };

    const KNOWN_TYPES: [&str; 5] = ["tool", "ask_user", "yield", "done", "delegate"];

    if let Some(type_str) = action.get("type").and_then(Value::as_str)
        && !KNOWN_TYPES.contains(&type_str)
    {
        let bad_type = type_str.to_string();
        fixes.push(format!(
            "action.type \"{bad_type}\" treated as a tool call named \"{bad_type}\""
        ));
        if !action.contains_key("name") {
            action.insert("name".to_string(), Value::String(bad_type));
        }
        if !action.contains_key("args") {
            action.insert("args".to_string(), Value::Object(Default::default()));
        }
        action.insert("type".to_string(), Value::String("tool".to_string()));
    }

    if action.get("type").and_then(Value::as_str) == Some("tool") && !action.contains_key("name") {
        let alt_name = action
            .get("tool")
            .and_then(Value::as_str)
            .or_else(|| action.get("tool_name").and_then(Value::as_str))
            .map(str::to_string);
        if let Some(name) = alt_name {
            fixes.push(format!(
                "action.type \"tool\" missing \"name\"; used \"{name}\" from tool/tool_name"
            ));
            action.insert("name".to_string(), Value::String(name));
        }
    }

    let mut strays: Vec<(&'static str, Value)> = Vec::new();
    for key in ["ack_inbox", "patch"] {
        if let Some(stray) = action.remove(key) {
            strays.push((key, stray));
        }
    }

    let top = value.as_object_mut().expect("value is an object");
    for (key, stray) in strays {
        if top.contains_key(key) {
            // Top level already has one; drop the stray nested copy
            // silently rather than overwriting real data.
            continue;
        }
        top.insert(key.to_string(), stray);
        fixes.push(format!("moved \"{key}\" out of \"action\""));
    }

    fixes
}

/// True when `e` looks like serde_json ran out of input mid-structure
/// (`is_eof()`, or a message containing "EOF while parsing" for older
/// serde_json versions where the classification API differs).
fn is_eof_error(e: &serde_json::Error) -> bool {
    e.is_eof() || e.to_string().contains("EOF while parsing")
}

/// Attempts, in order, the repairs whose trigger matches `e`'s message:
/// trailing-cut, control-char escape, then unclosed-bracket close. Each
/// repair that applies re-parses; if re-parsing still fails with an error
/// matching a *later* repair's trigger, that repair is tried too, so
/// multiple fixes can combine (e.g. trailing text left behind by a first
/// repair, then an EOF from an unclosed bracket). Human-readable
/// descriptions of every fix actually applied are pushed onto `fixes`.
/// Returns `None` (leaving `fixes` untouched by the failing attempt) when no
/// repair applies or the repaired text still doesn't parse as an object.
fn repair_json(text: &str, fixes: &mut Vec<String>) -> Option<Value> {
    let mut current = text.to_string();
    let mut local_fixes: Vec<String> = Vec::new();
    // Bound the number of repair rounds so a pathological input can't loop.
    for _ in 0..4 {
        let e = match serde_json::from_str::<Value>(&current) {
            Ok(v) if v.is_object() => {
                fixes.extend(local_fixes);
                return Some(v);
            }
            Ok(_) => return None,
            Err(e) => e,
        };
        let err_string = e.to_string();

        if err_string.contains("trailing characters")
            && let Some((cut, dropped)) = drop_trailing_characters(&current)
        {
            local_fixes.push(format!(
                "dropped {dropped} trailing character(s) after the JSON object"
            ));
            current = cut;
            continue;
        }
        if err_string.contains("control character")
            && let Some((escaped, n)) = escape_control_characters_in_strings(&current)
        {
            local_fixes.push(format!(
                "escaped {n} raw control character(s) inside strings"
            ));
            current = escaped;
            continue;
        }
        if is_eof_error(&e)
            && let Some((repaired, closers_added)) = repair_unclosed_json(&current)
        {
            local_fixes.push(format!(
                "closed {closers_added} unclosed bracket(s) at end of output"
            ));
            current = repaired;
            continue;
        }
        // No repair matched this error; stop trying.
        break;
    }
    None
}

/// String-aware scan for the index at which the first top-level `{...}`
/// object closes. Returns the text cut to that point plus the number of
/// trailing characters dropped, or `None` if the input never reaches
/// balance (nothing to cut) or there is no trailing remainder.
fn drop_trailing_characters(text: &str) -> Option<(String, usize)> {
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut started = false;
    let mut end_byte: Option<usize> = None;

    for (i, c) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => {
                depth += 1;
                started = true;
            }
            '}' => {
                depth -= 1;
                if started && depth == 0 {
                    end_byte = Some(i + c.len_utf8());
                    break;
                }
            }
            _ => {}
        }
    }

    let end = end_byte?;
    let remainder = &text[end..];
    if remainder.trim().is_empty() {
        return None;
    }
    let dropped = remainder.chars().count();
    Some((text[..end].to_string(), dropped))
}

/// String-aware scan that replaces raw control characters (bytes < 0x20:
/// newline, carriage return, tab, and others) found *inside* JSON string
/// literals with their JSON escape sequences, leaving structural whitespace
/// between tokens untouched. Returns the escaped text plus the number of
/// characters escaped, or `None` if no control character inside a string
/// was found (nothing to repair).
fn escape_control_characters_in_strings(text: &str) -> Option<(String, usize)> {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut count = 0usize;

    for c in text.chars() {
        if in_string {
            if escaped {
                escaped = false;
                out.push(c);
                continue;
            }
            if c == '\\' {
                escaped = true;
                out.push(c);
                continue;
            }
            if c == '"' {
                in_string = false;
                out.push(c);
                continue;
            }
            if (c as u32) < 0x20 {
                count += 1;
                match c {
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    _ => out.push_str(&format!("\\u{:04x}", c as u32)),
                }
                continue;
            }
            out.push(c);
            continue;
        }
        if c == '"' {
            in_string = true;
        }
        out.push(c);
    }

    if count == 0 { None } else { Some((out, count)) }
}

/// String-aware scan tracking a stack of open `{`/`[`. Appends the matching
/// closers in reverse order for any brackets still open at end of input.
///
/// Deliberately does NOT repair a cutoff that lands inside a string (e.g. a
/// `write_file` whose `content` value was cut off mid-character): closing
/// the string there would silently accept truncated file content as if it
/// were complete, which is worse than rejecting outright. Returns `None` in
/// that case, and also when there is nothing to repair (already balanced),
/// so callers can tell "no repair possible/needed" apart from "repaired".
/// On `Some`, also returns the number of closing brackets appended, for the
/// human-readable fix message.
fn repair_unclosed_json(text: &str) -> Option<(String, usize)> {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for c in text.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => stack.push(c),
            '}' => {
                if stack.last() == Some(&'{') {
                    stack.pop();
                }
            }
            ']' => {
                if stack.last() == Some(&'[') {
                    stack.pop();
                }
            }
            _ => {}
        }
    }

    if in_string || stack.is_empty() {
        return None;
    }

    let mut repaired = text.to_string();
    let mut closers_added = 0;
    for open in stack.iter().rev() {
        repaired.push(match open {
            '{' => '}',
            '[' => ']',
            _ => unreachable!(),
        });
        closers_added += 1;
    }
    Some((repaired, closers_added))
}

/// Improves the raw serde error for the common "tool action with no name"
/// case; all other deserialization failures keep serde's raw message.
fn classify_step_output_error(e: serde_json::Error) -> ParseError {
    let msg = e.to_string();
    if msg.contains("missing field") && msg.contains("name") {
        ParseError::InvalidJson(
            "action.type \"tool\" needs a \"name\" (the tool to call) and \"args\"".to_string(),
        )
    } else {
        ParseError::InvalidJson(msg)
    }
}

fn strip_fence(s: &str) -> &str {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("```json") {
        let rest = rest.trim_start();
        let rest = rest.strip_suffix("```").unwrap_or(rest);
        return rest.trim();
    }
    if let Some(rest) = s.strip_prefix("```") {
        let rest = rest.trim_start();
        let rest = rest.strip_suffix("```").unwrap_or(rest);
        return rest.trim();
    }
    s
}

/// A validated step: a committable patch plus the action to execute and the
/// acks that were applied.
pub struct ValidatedStep<W> {
    patch: ValidatedPatch<W>,
    action: Action,
    acks: Vec<u64>,
    /// Harness-facing notes recorded by a skill's `validate` override (e.g.
    /// the coding skills' `strip_malformed_next` dropping a malformed `next`
    /// rather than rejecting the step). Default empty; `validate_default`
    /// never produces any. `Session::step` appends each to the step's
    /// observation the same way `commitment_mismatch_note` is (or, if the
    /// action is not a tool call and there is no pending observation to
    /// append to, logs it as a `System` entry so it is not lost).
    notes: Vec<String>,
}

impl<W: std::fmt::Debug> std::fmt::Debug for ValidatedStep<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidatedStep")
            .field("patch", &self.patch)
            .field("action", &self.action)
            .field("acks", &self.acks)
            .field("notes", &self.notes)
            .finish()
    }
}

impl<W> ValidatedStep<W> {
    /// Build a validated step whose action is `Tool`. No inbox-ack witness
    /// required: tool calls are allowed with unacked inbox items.
    pub fn tool(patch: ValidatedPatch<W>, acks: Vec<u64>, name: String, args: Value) -> Self {
        ValidatedStep {
            patch,
            action: Action::Tool { name, args },
            acks,
            notes: Vec::new(),
        }
    }

    /// Build a validated step whose action is the harness-internal
    /// `Action::Continue { note }`: the patch commits, no tool runs, and
    /// `note` becomes the model's next observation (see
    /// `core/src/session.rs`'s handling of `Action::Continue`). No
    /// inbox-ack witness required, the same as `tool` -- a harness-authored
    /// `Continue` is not the model choosing to park, so the unacked-inbox
    /// rule that gates `AskUser`/`Yield`/`Done` does not apply to it.
    pub fn continue_(patch: ValidatedPatch<W>, acks: Vec<u64>, note: impl Into<String>) -> Self {
        ValidatedStep {
            patch,
            action: Action::Continue { note: note.into() },
            acks,
            notes: Vec::new(),
        }
    }

    /// Build a validated step that parks the session (`AskUser`, `Yield`,
    /// or `Done`). Requires proof (`InboxAcked`) that all unacked inbox
    /// items were acked by this step.
    pub fn parking(
        patch: ValidatedPatch<W>,
        acks: Vec<u64>,
        action: Action,
        _w: InboxAcked,
    ) -> Self {
        debug_assert!(
            !matches!(action, Action::Tool { .. }),
            "ValidatedStep::parking called with a Tool action; use ValidatedStep::tool instead"
        );
        debug_assert!(
            !matches!(action, Action::Continue { .. }),
            "ValidatedStep::parking called with a Continue action; use ValidatedStep::continue_ instead"
        );
        ValidatedStep {
            patch,
            action,
            acks,
            notes: Vec::new(),
        }
    }

    pub fn patch(&self) -> &ValidatedPatch<W> {
        &self.patch
    }

    pub fn action(&self) -> &Action {
        &self.action
    }

    pub fn acks(&self) -> &[u64] {
        &self.acks
    }

    /// Harness-facing notes recorded during validation (see the field
    /// doc). Empty for the vast majority of steps.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// Appends one note, recorded during a skill's `validate` override.
    /// See the `notes` field doc.
    pub fn push_note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    /// Builder form of [`ValidatedStep::push_note`].
    pub fn with_notes(mut self, notes: Vec<String>) -> Self {
        self.notes = notes;
        self
    }

    pub fn into_parts(self) -> (ValidatedPatch<W>, Action, Vec<u64>) {
        (self.patch, self.action, self.acks)
    }
}

impl<W> ValidatedStep<W> {
    /// A user-authored patch (`/edit`): action is always `Yield`, no inbox
    /// acks are applied. Used by `Active::user_patch`.
    pub(crate) fn user(patch: ValidatedPatch<W>) -> Self {
        ValidatedStep {
            patch,
            action: Action::Yield,
            acks: Vec::new(),
            notes: Vec::new(),
        }
    }
}

/// Validates a user-authored patch (`/edit`): applies the patch strategy and
/// the skill's `rules`, but does not require or check inbox acks (the
/// resulting action is always `Yield`). Returns the committable patch.
pub(crate) fn validate_user_patch<W, P>(
    skill: &(impl ?Sized + SkillRules<W>),
    cur: &CommittedState<W>,
    raw: RawPatch,
    version: Version,
) -> Result<ValidatedPatch<W>, PatchError>
where
    P: PatchStrategy<W>,
{
    let patch: P::Patch =
        serde_json::from_value(raw.0.clone()).map_err(|e| PatchError::Malformed(e.to_string()))?;
    let next = P::apply(&cur.agent, &patch)?;
    let checked = CheckedPatch::new(raw.0, next, version);

    skill.check_rules(cur, checked.next())?;

    Ok(ValidatedPatch::from_checked(checked))
}

/// All unacked inbox ids must be present in `acks`. Unknown ack ids
/// (referring to no inbox item) are ignored. Returns a witness proving the
/// check happened.
pub fn check_inbox(cur: &HarnessState, acks: &[u64]) -> Result<InboxAcked, PatchError> {
    let missing: Vec<u64> = cur
        .inbox
        .iter()
        .filter(|item| !item.acked)
        .map(|item| item.id)
        .filter(|id| !acks.contains(id))
        .collect();
    if missing.is_empty() {
        Ok(InboxAcked::new())
    } else {
        Err(PatchError::UnackedInbox(missing))
    }
}

/// A pluggable skill: workspace type, patch strategy, tools, rules.
pub trait Skill: Send + Sync + 'static {
    type Workspace: Serialize
        + DeserializeOwned
        + schemars::JsonSchema
        + Clone
        + Send
        + Sync
        + 'static;
    type Patch: PatchStrategy<Self::Workspace>;

    fn name(&self) -> &str;
    fn spec(&self) -> Arc<SkillSpec>;
    fn initial_workspace(&self) -> Self::Workspace;
    /// Active tool names.
    fn tools(&self) -> Vec<String>;
    /// Skill names this one may hand off to.
    fn allowed_transitions(&self) -> Vec<String> {
        vec![]
    }
    fn is_terminal(&self, s: &Self::Workspace) -> bool;

    /// If `Some(name)`, the session hands off to the named skill instead of
    /// calling the model this step. Checked before `is_terminal`.
    fn handoff(&self, _s: &Self::Workspace) -> Option<String> {
        None
    }

    /// Skill-specific rules on the candidate next state (e.g. a length cap
    /// on a summary field).
    fn rules(
        &self,
        _cur: &CommittedState<Self::Workspace>,
        _next: &Self::Workspace,
    ) -> Result<(), PatchError> {
        Ok(())
    }

    /// Whether action `yield` is allowed in this skill. Default `true`;
    /// skills that must always park via `ask_user` or hand off with `done`
    /// (e.g. the coding skill set) override this to `false`.
    fn allows_yield(&self) -> bool {
        true
    }

    /// Configured child skill names this skill may `delegate` to. Default
    /// empty (no delegation). A skill that calls a configured subagent
    /// overrides this to list the children it's allowed to call.
    fn allowed_delegates(&self) -> Vec<String> {
        vec![]
    }

    /// Whether action `ask_user` is allowed in this skill. Default `true`;
    /// children (milestone 9) override this to `false` -- they cannot talk
    /// to the user, only their parent.
    fn allows_ask_user(&self) -> bool {
        true
    }

    /// If a rejected `tool` call names a tool this skill doesn't have but
    /// some skill this one may hand off to (`allowed_transitions()`) does,
    /// the name of that skill -- so the rejection can tell the model where
    /// to go instead of just what it can't do here (item 29). Default
    /// `None`: skills that don't know of such a mapping (or whose
    /// `allowed_transitions()` doesn't include the skill that has it) leave
    /// this unimplemented.
    fn skill_with_tool(&self, _tool: &str) -> Option<String> {
        None
    }

    /// Absorbs a subagent's finished `Report` into this skill's workspace.
    /// Called by the harness after a `delegate` action completes, never by
    /// the model. Default rejects: only skills that declare
    /// `allowed_delegates()` need to implement this.
    fn absorb_report(&self, _ws: &mut Self::Workspace, _report: Report) -> Result<(), PatchError> {
        Err(PatchError::RuleViolation(
            "this skill does not accept subagent reports".to_string(),
        ))
    }

    /// The workspace's current commitment, if the skill supports the
    /// commitment slot: the mutating call the model has committed to
    /// making next (see `Commitment`). Default `None` -- skills that don't
    /// carry a `next: Option<Commitment>` field simply never trigger the
    /// session's commitment-slot rule.
    fn commitment(&self, _ws: &Self::Workspace) -> Option<Commitment> {
        None
    }

    /// The number of consecutive read-only calls with an EMPTY patch (the
    /// raw patch object has no keys) the model may make with no commitment
    /// set before the next such call is rejected. A read whose patch writes
    /// anything (a file note, a plan step, `recent_summary`, ...) is never
    /// counted and resets the streak -- reading is not the problem; reading
    /// that records nothing is. `None` (the default) disables the rule -- a
    /// skill with no `next` field, or one that doesn't want reads gated at
    /// all, never triggers it.
    fn silent_read_cap(&self) -> Option<u32> {
        None
    }

    /// Top-level workspace keys the harness writes and the model may never
    /// patch (e.g. `notes`, maintained by a `Maintainer` call after each
    /// step). Default empty -- a skill with no harness-owned fields never
    /// triggers the check. See `validate_default`.
    fn harness_owned_fields(&self) -> Vec<String> {
        vec![]
    }

    /// The harness-maintained notes text, if this workspace carries one.
    /// Default `None`. See `harness_owned_fields`.
    fn notes(&self, _ws: &Self::Workspace) -> Option<String> {
        None
    }

    /// Unit 26 (situational rules): localised, per-step guidance derived
    /// from the harness's own state (`cur.harness`) and the workspace --
    /// the 2-hop neighbourhood of "what just happened" in a procedure
    /// graph, rather than the skill's whole rules block repeated every
    /// step (RELATED-WORK.md: localised guidance beat whole-procedure
    /// injection by 9 points and 71% fewer tokens in that study).
    /// `SkillSpec` is immutable per activation (C1) and so cannot carry
    /// this -- it must be recomputed fresh each step from `cur`, hence a
    /// method taking `&CommittedState` rather than a field on the spec.
    /// Default `None`: a skill that has no situational guidance (or has
    /// the switch controlling it turned off) renders no `## Now` section
    /// at all, identical to today's prompt. `Some(text)` is rendered as
    /// its own section headed `## Now`, placed right after the harness
    /// view (`core/src/session.rs`'s prompt builder), truncated to the
    /// same budget rule `notes` gets.
    fn situational(&self, _cur: &CommittedState<Self::Workspace>) -> Option<String> {
        None
    }

    /// Called by the session right after a tool call has executed
    /// successfully -- not on a tool error, not on a skipped-redundant
    /// call, but a paged `read_file` repeat counts as executed (with its
    /// rewritten `args`) -- and after the model's own patch for the step
    /// has already committed. `ws` is the just-committed workspace, `step`
    /// the harness step the call ran at, `name`/`args` the tool call
    /// (post-paging-rewrite where relevant), and `output` its full text
    /// output.
    ///
    /// Returning `Some(patch)` has the session apply `patch` as one extra,
    /// harness-trusted commit (source `"harness"`, a `Patch` log entry with
    /// `"reason": "after_tool"`), the same way a maintainer or absorbed-
    /// report patch commits -- never failing the step: an error applying it
    /// is logged as a `System` `note` and otherwise ignored. Default
    /// `None`: a skill that doesn't maintain any harness-owned, tool-
    /// derived state never triggers this.
    fn after_tool(
        &self,
        _ws: &Self::Workspace,
        _harness: &HarnessState,
        _step: u64,
        _name: &str,
        _args: &Value,
        _output: &str,
    ) -> Option<Patch> {
        None
    }

    /// Called once right after this skill is activated, with the workspace
    /// as committed; a returned patch is applied as a harness-sourced
    /// version, e.g. to clear a consumed handoff.
    ///
    /// A handoff is requested by the model setting the workspace's
    /// `next_skill` (paired with action `done`, per `Skill::handoff`). The
    /// session consumes it by activating the named skill next, but nothing
    /// else clears the field: without this hook, the new activation runs
    /// with the old `next_skill` still committed, which a skill's own
    /// bare-`done`/handoff logic can then misread as a (self-)handoff
    /// already named. Runs for every activation path -- fresh activate,
    /// handoff, resume with `--skill`, and `activate_forced` -- since all of
    /// them funnel through `Session::activate_inner`.
    ///
    /// Applied the same way an [`after_tool`](Skill::after_tool) patch is:
    /// one extra harness-trusted commit (source `"harness"`, a `Patch` log
    /// entry with `"reason": "on_activate"`), never failing activation --
    /// an error applying it is logged as a `System` `note` and otherwise
    /// ignored. Default `None`: a skill with no such state never triggers
    /// this.
    fn on_activate(&self, _ws: &Self::Workspace) -> Option<Patch> {
        None
    }

    /// Full validation. Provided; skills override `rules`, not this.
    fn validate(
        &self,
        cur: &CommittedState<Self::Workspace>,
        out: ParsedOutput<Self::Workspace>,
    ) -> Result<ValidatedStep<Self::Workspace>, PatchError> {
        validate_default::<Self::Workspace, Self::Patch>(self, cur, out)
    }
}

/// The default `Skill::validate` implementation, exposed as a free function
/// so trait objects and generic code can call it directly.
pub fn validate_default<W, P>(
    skill: &(impl ?Sized + SkillRules<W>),
    cur: &CommittedState<W>,
    out: ParsedOutput<W>,
) -> Result<ValidatedStep<W>, PatchError>
where
    P: PatchStrategy<W>,
    W: Serialize,
{
    let version = out.version();
    let patch: P::Patch = serde_json::from_value(out.patch().0.clone())
        .map_err(|e| PatchError::Malformed(e.to_string()))?;
    let next = P::apply(&cur.agent, &patch)?;
    let checked = CheckedPatch::new(out.patch().0.clone(), next, version);

    // Harness-owned fields: for EVERY action (not just a subset), a patch
    // that *changes* a key the harness itself writes (e.g. `notes`,
    // maintained by a `Maintainer` call after each step) is rejected
    // before any other check runs. Checked by comparing the committed
    // value to the post-patch value for each owned key, not by checking
    // whether the key is merely present in the raw patch object: a patch
    // strategy that sends the whole workspace (e.g. `ReplaceStrategy`)
    // always carries every key, so a presence check would fire on every
    // patch regardless of content. Comparing values instead tolerates a
    // patch that echoes the field back unchanged -- required for replace
    // strategies to be usable at all -- while still catching a strategy
    // (e.g. a JSON merge patch) that sets the field to a new value or to
    // `null` to delete it; a key absent from either side compares as
    // `Null`. `validate_user_patch` (operator patches) deliberately does
    // not run this check: the operator and the maintainer are trusted
    // writers of these fields.
    let committed_value =
        serde_json::to_value(&cur.agent).map_err(|e| PatchError::Malformed(e.to_string()))?;
    let next_value =
        serde_json::to_value(checked.next()).map_err(|e| PatchError::Malformed(e.to_string()))?;
    for key in skill.owned_fields() {
        let before = committed_value.get(&key).unwrap_or(&Value::Null);
        let after = next_value.get(&key).unwrap_or(&Value::Null);
        if before != after {
            return Err(PatchError::RuleViolation(format!(
                "field `{key}` is harness-owned in skill `{}` and cannot be patched",
                skill.skill_name()
            )));
        }
    }

    skill.check_rules(cur, checked.next())?;

    let validated = ValidatedPatch::from_checked(checked);
    let action = out.action().clone();
    let acks = out.ack_inbox().to_vec();

    if matches!(action, Action::Tool { .. }) {
        let (name, args) = match action {
            Action::Tool { name, args } => (name, args),
            _ => unreachable!(),
        };
        Ok(ValidatedStep::tool(validated, acks, name, args))
    } else if let Action::Continue { note } = action {
        // Harness-internal: a `Skill::validate` override (`ParsedOutput::
        // set_action`) already decided to commit this patch and tell the
        // model `note` instead of running the terminal/handoff/ask_user
        // checks below, which were written for what the *model* asked for,
        // not for a harness-authored `Continue`. No inbox-ack witness is
        // required, the same as a tool call -- see `ValidatedStep::
        // continue_`'s doc.
        Ok(ValidatedStep::continue_(validated, acks, note))
    } else {
        if matches!(action, Action::Done)
            && !skill.terminal(validated.next())
            && skill.handoff_target(validated.next()).is_none()
        {
            let transitions = skill.transitions();
            let message = if transitions.is_empty() {
                "action `done` is only valid when the workspace is terminal or hands off: \
                 this skill has no handoff targets; the workspace must reach its terminal \
                 condition"
                    .to_string()
            } else {
                // Only offer the pause actions this skill's own rules accept
                // (item 28: the router can neither `yield` nor `ask_user`).
                let pause = match (skill.yield_allowed(), skill.ask_user_allowed()) {
                    (true, true) => ", or use `yield`/`ask_user`",
                    (true, false) => ", or use `yield`",
                    (false, true) => ", or use `ask_user`",
                    (false, false) => "",
                };
                let first_target = transitions
                    .first()
                    .expect("transitions is non-empty in this branch");
                format!(
                    "action `done` is only valid when the workspace is terminal or hands off: \
                     set `next_skill` to one of {:?} (you are `{}`) and send `done` again, or \
                     keep working with a tool{pause}\n\
                     Exact shape: {{\"patch\": {{\"next_skill\": \"{first_target}\"}}, \
                     \"action\": {{\"type\": \"done\", \"name\": \"\", \"args\": {{}}}}, \
                     \"ack_inbox\": []}}",
                    transitions,
                    skill.skill_name()
                )
            };
            return Err(PatchError::RuleViolation(message));
        }
        if matches!(action, Action::Yield) && !skill.yield_allowed() {
            let transitions = skill.transitions();
            let message = if transitions.is_empty() {
                format!(
                    "action `yield` is not allowed in skill `{}`: keep working with a tool, \
                     or use `ask_user` with a concrete question if you need input",
                    skill.skill_name()
                )
            } else {
                format!(
                    "action `yield` is not allowed in skill `{}`: keep working with a tool, \
                     hand off by setting `next_skill` to one of {:?} and sending `done`, or \
                     use `ask_user` with a concrete question if you need input",
                    skill.skill_name(),
                    transitions
                )
            };
            return Err(PatchError::RuleViolation(message));
        }
        if matches!(action, Action::AskUser { .. }) && !skill.ask_user_allowed() {
            return Err(PatchError::RuleViolation(format!(
                "action `ask_user` is not allowed in skill `{}`",
                skill.skill_name()
            )));
        }
        if let Action::AskUser { message } = &action
            && message.trim().is_empty()
        {
            return Err(PatchError::RuleViolation(
                "action `ask_user` requires a non-empty `message`: the question for the user"
                    .to_string(),
            ));
        }
        if let Action::Delegate {
            skill: target,
            task,
        } = &action
            && (target.trim().is_empty() || task.trim().is_empty())
        {
            return Err(PatchError::RuleViolation(
                "action `delegate` requires non-empty `skill` and `task`".to_string(),
            ));
        }
        if let Action::Delegate { skill: target, .. } = &action {
            let delegates = skill.delegates_allowed();
            if !delegates.contains(target) {
                return Err(PatchError::RuleViolation(format!(
                    "action `delegate` is not allowed in skill `{}`: allowed delegates are {:?}",
                    skill.skill_name(),
                    delegates
                )));
            }
        }
        let witness = check_inbox(&cur.harness, &acks)?;
        Ok(ValidatedStep::parking(validated, acks, action, witness))
    }
}

/// A narrow view of `Skill` used by `validate_default` so it can be called
/// generically without requiring `Self: Sized`.
pub trait SkillRules<W> {
    fn check_rules(&self, cur: &CommittedState<W>, next: &W) -> Result<(), PatchError>;
    fn terminal(&self, next: &W) -> bool;
    fn handoff_target(&self, next: &W) -> Option<String>;
    /// Skill names this one may hand off to.
    fn transitions(&self) -> Vec<String>;
    /// This skill's own name.
    fn skill_name(&self) -> String;
    /// Whether action `yield` is allowed in this skill.
    fn yield_allowed(&self) -> bool;
    /// Configured child skill names this skill may `delegate` to.
    fn delegates_allowed(&self) -> Vec<String>;
    /// Whether action `ask_user` is allowed in this skill.
    fn ask_user_allowed(&self) -> bool;
    /// Top-level workspace keys the harness writes and the model may never
    /// patch.
    fn owned_fields(&self) -> Vec<String>;
}

impl<K> SkillRules<K::Workspace> for K
where
    K: Skill + ?Sized,
{
    fn check_rules(
        &self,
        cur: &CommittedState<K::Workspace>,
        next: &K::Workspace,
    ) -> Result<(), PatchError> {
        self.rules(cur, next)
    }

    fn terminal(&self, next: &K::Workspace) -> bool {
        self.is_terminal(next)
    }

    fn handoff_target(&self, next: &K::Workspace) -> Option<String> {
        self.handoff(next)
    }

    fn transitions(&self) -> Vec<String> {
        self.allowed_transitions()
    }

    fn skill_name(&self) -> String {
        self.name().to_string()
    }

    fn delegates_allowed(&self) -> Vec<String> {
        self.allowed_delegates()
    }

    fn ask_user_allowed(&self) -> bool {
        self.allows_ask_user()
    }

    fn yield_allowed(&self) -> bool {
        self.allows_yield()
    }

    fn owned_fields(&self) -> Vec<String> {
        self.harness_owned_fields()
    }
}
