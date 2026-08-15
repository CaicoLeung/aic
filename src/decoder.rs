//! Per-provider NDJSON envelope decoding for the CLI-agent backend
//! ([`crate::cli_agent`]).
//!
//! Each coding-agent CLI (`claude --output-format stream-json`, `pi --mode json`,
//! `opencode --format json`, `codex --json`) prints a different newline-delimited
//! JSON stream. This module owns the one-pass fold that turns those lines into
//! (a) reasoning forwarded live to the spinner and (b) the assembled answer
//! text, behind a single [`Decoder`] trait that
//! [`crate::cli_agent::CliAgent`] drives via `run_streamed`.
//!
//! Each provider is a pure `decode_*_stream_line(&str) -> Option<XxxDelta>`
//! function (unit-testable against captured output without spawning the CLI)
//! paired with a [`Decoder`] struct holding the fold state. Adding an envelope
//! is a new pair plus one arm in `CliAgent::run_once`'s `Encoding` match.
//!
//! Split out of `cli_agent.rs`: the four envelopes and their delta enums were
//! ~half that file and wholly self-contained parsing logic with no coupling to
//! the run/classify/retry machinery — collocating them with `CliAgent` made both
//! halves harder to read. The host's only touchpoints are the [`Decoder`] trait
//! and the four constructors.

// ---- Claude stream-json decoding --------------------------------------------
//
// Claude Code's `--output-format stream-json --include-partial-messages`
// emits one JSON object per stdout line. aic cares about exactly three event
// shapes; everything else (system hooks, the init config dump with its tools/
// agents/skills/plugins list, assistant message snapshots, status) is noise
// that must never reach the reasoning window or the answer buffer. Decoders
// are pure functions over a single line so they unit-test against captured
// real output without spawning claude.

/// One decoded chunk from a claude `stream-json` line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClaudeDelta {
    /// A startup-phase progress line decoded from a `system` event
    /// (`hook_started` → "Running SessionStart hooks…", `init` → "Initialized —
    /// model X, N tools, M MCP servers"). Forwarded live to the reasoning
    /// window so the cold start (hooks + MCP handshake + TTFT, often 6–10 s)
    /// is visible progress, not a bare spinner — and so the loading grace
    /// never trips mid-startup, since each milestone flips `got_output` and
    /// resets the idle clock. Deduped by the caller so claude's repeated
    /// `SessionStart:startup` hook pairs yield one line.
    Milestone(String),
    /// A `thinking_delta` — the model's reasoning, streamed live to the
    /// batch-plan reasoning window (parity with the API path's reasoning
    /// feed). May be a fragment of a word; aic's [`ThinkingView`](crate::progress::ThinkingView)
    /// assembles fragments into lines.
    Thinking(String),
    /// A `text_delta` — assistant answer text. Concatenated across all
    /// `text_delta` events to reconstruct the (typically JSON) answer the
    /// typed path parses. Never shown live — there is no answer-preview UI,
    /// only the reasoning window.
    Text(String),
    /// The terminal `result` event's `result` field — the authoritative full
    /// answer text. Preferred over the concatenated [`Self::Text`] deltas
    /// when both are present, since claude guarantees this field on success.
    Result(String),
}

/// Decode one `stream-json` stdout line into a [`ClaudeDelta`], or `None` for
/// any non-answer/non-reasoning event (the common case — system/init/hook/
/// assistant-snapshot noise). Tolerant: a line that is not valid JSON, or is
/// missing any expected field, yields `None` rather than an error, so a
/// future claude version adding fields or a partial flush never breaks the
/// stream — the worst case is a dropped line, not a failed run. The event
// shapes handled:
//
// ```text
// {"type":"stream_event","event":{"type":"content_block_delta",
//  "delta":{"type":"thinking_delta","thinking":"…"}}}
// {"type":"stream_event","event":{"type":"content_block_delta",
//  "delta":{"type":"text_delta","text":"…"}}}
// {"type":"result","result":"…full answer…"}
// ```
fn decode_claude_stream_line(line: &str) -> Option<ClaudeDelta> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let typ = v.get("type").and_then(|t| t.as_str())?;
    match typ {
        "stream_event" => {
            let ev = v.get("event")?;
            if ev.get("type").and_then(|t| t.as_str())? != "content_block_delta" {
                return None;
            }
            let delta = ev.get("delta")?;
            match delta.get("type").and_then(|t| t.as_str())? {
                "thinking_delta" => Some(ClaudeDelta::Thinking(
                    delta.get("thinking").and_then(|x| x.as_str())?.to_string(),
                )),
                "text_delta" => Some(ClaudeDelta::Text(
                    delta.get("text").and_then(|x| x.as_str())?.to_string(),
                )),
                _ => None,
            }
        }
        "result" => v
            .get("result")
            .and_then(|r| r.as_str())
            .map(|s| ClaudeDelta::Result(s.to_string())),
        // Startup-phase `system` events: surfaced as a live milestone so the
        // cold start (hooks + MCP handshake + TTFT) is visible progress in
        // the reasoning window, not a bare spinner — and so the loading
        // grace never trips mid-startup (each milestone flips `got_output`
        // and resets the idle clock). `hook_response`/`status` carry nothing
        // worth showing and stay filtered.
        "system" => decode_claude_system_event(&v),
        _ => None,
    }
}

/// One decoded chunk from a pi `--mode json` line. pi's stream is simpler
/// than claude's: every delta is a `message_update` carrying an
/// `assistantMessageEvent` of type `thinking_delta` (reasoning) or
/// `text_delta` (answer). There are no startup milestones (pi's cold start is
/// fast — no hooks/MCP dump) and no terminal `result` event with the full
/// answer, so the answer is reconstructed by concatenating `text_delta`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PiDelta {
    Thinking(String),
    Text(String),
}

/// Decode one pi `--mode json` stdout line into a [`PiDelta`], or `None` for
/// any non-delta event (`session`, `agent_start`, `turn_start`, `message_start`,
/// `message_end`, `turn_end`, `agent_end`, `entry_appended`, `agent_settled`,
/// and the `thinking_start`/`thinking_end`/`text_start`/`text_end` lifecycle
/// markers). Tolerant like the claude decoder: a malformed line or missing
/// field yields `None`, never an error.
///
/// Event shape:
/// ```text
/// {"type":"message_update","assistantMessageEvent":
///  {"type":"thinking_delta","delta":"…"}}
/// ```
fn decode_pi_stream_line(line: &str) -> Option<PiDelta> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type").and_then(|t| t.as_str())? != "message_update" {
        return None;
    }
    let ev = v.get("assistantMessageEvent")?;
    let delta = ev.get("delta").and_then(|d| d.as_str())?;
    match ev.get("type").and_then(|t| t.as_str())? {
        "thinking_delta" => Some(PiDelta::Thinking(delta.to_string())),
        "text_delta" => Some(PiDelta::Text(delta.to_string())),
        _ => None,
    }
}

/// Decode a `system` event into a startup [`ClaudeDelta::Milestone`], or
/// `None` for subtypes with nothing worth surfacing (`hook_response`,
/// `status`, `thinking_tokens`). Pure and tolerant like the line decoder: a
/// missing field degrades the wording rather than failing.
///
/// - `init` → "Initialized — model X, N tools, M/K MCP servers connected",
///   built from the event's `model`/`tools`/`mcp_servers` fields. The MCP
///   count is `connected/total` so a half-up MCP cluster is visible.
/// - `hook_started` → "Running {hook_event} hooks…" (e.g. SessionStart). The
///   caller dedupes consecutive identical milestones, so claude's repeated
///   `SessionStart:startup` pairs collapse to one line.
fn decode_claude_system_event(v: &serde_json::Value) -> Option<ClaudeDelta> {
    let sub = v.get("subtype").and_then(|s| s.as_str())?;
    match sub {
        "init" => {
            let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("?");
            let ntools = v
                .get("tools")
                .and_then(|t| t.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let (total, connected) = match v.get("mcp_servers").and_then(|m| m.as_array()) {
                Some(a) => {
                    let total = a.len();
                    let connected = a
                        .iter()
                        .filter(|s| s.get("status").and_then(|x| x.as_str()) == Some("connected"))
                        .count();
                    (total, connected)
                }
                None => (0, 0),
            };
            Some(ClaudeDelta::Milestone(format!(
                "Initialized — model {model}, {ntools} tools, {connected}/{total} MCP servers connected"
            )))
        }
        "hook_started" => {
            let ev = v
                .get("hook_event")
                .and_then(|e| e.as_str())
                .unwrap_or("hook");
            Some(ClaudeDelta::Milestone(format!("Running {ev} hooks…")))
        }
        _ => None,
    }
}

/// One decoded chunk from an opencode `run --format json` line. opencode emits
/// the full answer as a single `text` event's `part.text` at completion (not
/// token-streamed), and — only when reasoning is produced — a `reasoning`
/// event the same way. `step_start`/`step_finish` carry metadata only.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OpenCodeDelta {
    Thinking(String),
    Text(String),
}

/// Decode one opencode `--format json` stdout line into an [`OpenCodeDelta`],
/// or `None` for non-content events (`step_start`, `step_finish`, `tool_use`,
/// …). Tolerant like the other decoders: malformed JSON or a missing `text`
/// field yields `None`, never an error.
///
/// Event shapes:
/// ```text
/// {"type":"reasoning","part":{"type":"reasoning","text":"…"}}
/// {"type":"text","part":{"type":"text","text":"…"}}
/// ```
fn decode_opencode_stream_line(line: &str) -> Option<OpenCodeDelta> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let typ = v.get("type").and_then(|t| t.as_str())?;
    let text = v
        .get("part")
        .and_then(|p| p.get("text"))
        .and_then(|t| t.as_str())?;
    match typ {
        "reasoning" => Some(OpenCodeDelta::Thinking(text.to_string())),
        "text" => Some(OpenCodeDelta::Text(text.to_string())),
        _ => None,
    }
}

// ---- codex `--json` decoding ----------------------------------------------
//
// codex `exec --json` emits one JSON object per stdout line. The answer and
// reasoning text arrive at `item.completed`; the rest is structural
// (`thread.*`, `turn.*`) or tool-use (`item.started`/`item.updated`/
// `item.completed` for `command_execution`/`file_change`/`mcp_tool_call`).
//
// codex is a **batch/silent** protocol: unlike claude and pi it streams no
// reasoning tokens (verified on 0.147 — `reasoning_output_tokens` is non-zero
// yet zero reasoning events stream, and `concurrent_reasoning_summaries` is
// still "under development"), so the reasoning window would otherwise stay
// empty for the whole run. To give visible progress — and because codex
// routinely runs shell commands under the read-only sandbox before answering
// — `turn.started` and tool-use `item.started` events are surfaced as live
// [`CodexDelta::Progress`] milestones. (The runner's idle timer already resets
// on every raw line, so agentic runs never time out; the progress forwarding
// is purely UX.) Tolerant of the documented `agent_message` ↔
// `assistant_message` drift (Issue #4776) and of missing fields (→ None,
// never an error), matching the other decoders.

/// One decoded chunk from a codex `--json` line. codex emits reasoning and
/// answer text only at `item.completed`, so the text-bearing variants carry
/// the full text for that item; [`CodexDelta::Progress`] carries a live
/// milestone string forwarded to the reasoning window.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CodexDelta {
    /// A `reasoning` item's `text` — the model's reasoning summary, forwarded
    /// live when present (best-effort: account/org dependent, often absent —
    /// Issue #10746).
    Thinking(String),
    /// An `agent_message` (or drifted `assistant_message`) item's `text` —
    /// the assistant's answer. Multiple → last wins.
    Text(String),
    /// A live progress milestone (a `turn.started` boundary, or a tool-use
    /// `item.started` — a shell command, file edit, or MCP call the model
    /// issued). Forwarded to the reasoning window so a silent-by-design CLI
    /// still shows visible progress. Never the answer.
    Progress(String),
}

/// Decode one codex `--json` stdout line into a [`CodexDelta`], or `None` for
/// events that carry neither answer text, reasoning, nor useful progress.
/// Tolerant: a malformed line or a missing field yields `None`, never an
/// error.
///
/// Answer/reasoning event shapes:
/// ```text
/// {"type":"item.completed","item":{"id":"…","type":"agent_message","text":"…"}}
/// {"type":"item.completed","item":{"id":"…","type":"reasoning","text":"…"}}
/// ```
///
/// Progress event shapes (forwarded live):
/// ```text
/// {"type":"turn.started"}
/// {"type":"item.started","item":{"id":"…","type":"command_execution","command":"/bin/zsh -lc '…'"}}
/// ```
///
/// Both `agent_message` (codex ≥ v0.44.0) and its documented alias
/// `assistant_message` are accepted — Issue #4776 records this drift, and a
/// tolerant parse here is load-bearing against version skew.
fn decode_codex_stream_line(line: &str) -> Option<CodexDelta> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let typ = v.get("type").and_then(|t| t.as_str())?;
    match typ {
        // A single progress milestone at the start of the turn: codex emits
        // this near-instantly, so forwarding it gives the user an immediate
        // "it's working" signal before the (silent) reasoning phase.
        "turn.started" => return Some(CodexDelta::Progress("codex turn started".to_string())),
        "item.started" | "item.completed" => {}
        _ => return None,
    }
    let item = v.get("item")?;
    let ityp = item.get("type").and_then(|t| t.as_str())?;

    // Live progress for tool-use items the model issues mid-run. codex under
    // the read-only sandbox frequently runs shell commands (pwd/git diff/cat)
    // before answering; surfacing them turns a silent wait into visible
    // progress. Only `item.started` is forwarded so each tool shows once,
    // when it begins — the matching `item.completed` would just double the
    // noise.
    if typ == "item.started" {
        if let Some(cmd) = item.get("command").and_then(|c| c.as_str()) {
            return Some(CodexDelta::Progress(format!(
                "codex: {}",
                short_command(cmd)
            )));
        }
        return match ityp {
            "file_change" => Some(CodexDelta::Progress("codex: editing file".to_string())),
            "mcp_tool_call" => Some(CodexDelta::Progress("codex: mcp tool call".to_string())),
            _ => None,
        };
    }

    // item.completed: the answer (agent_message) and best-effort reasoning.
    let text = item.get("text").and_then(|t| t.as_str())?.to_string();
    match ityp {
        "agent_message" | "assistant_message" => Some(CodexDelta::Text(text)),
        "reasoning" => Some(CodexDelta::Thinking(text)),
        _ => None,
    }
}

/// Trim a codex `command_execution` command for display in the reasoning
/// window. codex wraps shell commands as `<shell> -lc '<body>'` (e.g./// `/bin/zsh -lc 'git diff --cached'`); strip the wrapper and cap the length
/// so a giant one-liner does not flood the window. Best-effort — falls back
/// to the raw string when the shape differs.
fn short_command(cmd: &str) -> String {
    const CAP: usize = 100;
    let s = cmd.trim();
    let body = [
        "/bin/zsh -lc ",
        "/bin/bash -lc ",
        "/bin/sh -lc ",
        "zsh -lc ",
        "bash -lc ",
        "sh -lc ",
    ]
    .iter()
    .find_map(|p| s.strip_prefix(p))
    .unwrap_or(s)
    .trim_matches(|c| c == '\'' || c == '"');
    let mut out: String = body.chars().take(CAP).collect();
    if body.chars().count() > CAP {
        out.push('…');
    }
    out
}

// ---- the decode seam ------------------------------------------------------
//
// One private, object-safe interface per streamed envelope. The four free
// `decode_*_stream_line` helpers above stay as the line-shape source (and the
// drift-lock tests exercise them directly); each `Decoder` impl owns the
// per-envelope *state* — the answer accumulator, claude's milestone-dedup
// window, opencode/codex's last-wins — behind a two-method surface:
//
//   - `decode_line` returns what to forward to the reasoning window this line
//     (a thinking delta, a claude milestone, …), or `None`. It also folds any
//     answer text into its internal accumulator. Subsumes both live reasoning
//     deltas (claude/pi) and whole-blob reasoning (opencode/codex): a
//     milestone and a thinking fragment are both "a string to show", so the
//     caller never needs to tell them apart.
//   - `finish` returns the assembled answer, or `None` (→ a typed error).
//
// This collapses the old `run_once` closure-per-envelope plus the double-walk
// (`decode_*_answer` re-parsing stdout the forward closure had already seen)
// into a single pass: `run_streamed` forwards each `decode_line` result, then
// calls `finish`. Plain (`Encoding::Plain`) stays a special case in
// `run_once` — its run path differs (raw stdout via `into_result`, no
// decode), so forcing it through a no-op decoder would be a leak, not
// uniformity.
pub(crate) trait Decoder: Send {
    /// Fold one stdout/stderr line: return what to forward to the reasoning
    /// window (if anything), and accumulate any answer text internally.
    fn decode_line(&mut self, line: &str) -> Option<String>;
    /// Return the assembled answer after all lines, or `None` if none arrived.
    fn finish(&mut self) -> Option<String>;
}

/// Claude `stream-json` decoder. Owns the concatenated `text_delta` answer,
/// the terminal `result` event's text (preferred at `finish`), and the
/// milestone-dedup window (claude emits repeated `SessionStart:startup` hook
/// pairs; a `thinking_delta` clears the window so a milestone re-fires after
/// reasoning resumes). Milestones carry their own trailing newline; thinking
/// does not — a decoder-internal formatting choice the caller never sees.
pub(crate) struct ClaudeDecoder {
    text: String,
    result: Option<String>,
    last_milestone: Option<String>,
}

impl ClaudeDecoder {
    pub(crate) fn new() -> Self {
        Self {
            text: String::new(),
            result: None,
            last_milestone: None,
        }
    }
}

impl Decoder for ClaudeDecoder {
    fn decode_line(&mut self, line: &str) -> Option<String> {
        match decode_claude_stream_line(line) {
            Some(ClaudeDelta::Milestone(m)) => {
                if self.last_milestone.as_deref() == Some(m.as_str()) {
                    None // dedup'd duplicate of the previous milestone
                } else {
                    self.last_milestone = Some(m.clone());
                    Some(format!("{m}\n"))
                }
            }
            Some(ClaudeDelta::Thinking(t)) => {
                self.last_milestone = None; // reasoning clears the dedup window
                Some(t)
            }
            Some(ClaudeDelta::Text(s)) => {
                self.text.push_str(&s);
                None
            }
            Some(ClaudeDelta::Result(s)) => {
                self.result = Some(s);
                None
            }
            None => None,
        }
    }

    fn finish(&mut self) -> Option<String> {
        // Terminal `result` event wins (authoritative on success); otherwise
        // the concatenated `text_delta` (the error-turn path where claude
        // omits `result`). Empty → None so the caller surfaces a typed error.
        self.result.take().or_else(|| {
            if self.text.trim().is_empty() {
                None
            } else {
                Some(std::mem::take(&mut self.text))
            }
        })
    }
}

/// pi `--mode json` decoder. The answer is the concatenation of every
/// `text_delta` in arrival order (pi emits no terminal result event);
/// `thinking_delta` streams live to the reasoning window.
pub(crate) struct PiDecoder {
    text: String,
}

impl PiDecoder {
    pub(crate) fn new() -> Self {
        Self {
            text: String::new(),
        }
    }
}

impl Decoder for PiDecoder {
    fn decode_line(&mut self, line: &str) -> Option<String> {
        match decode_pi_stream_line(line) {
            Some(PiDelta::Thinking(t)) => Some(t),
            Some(PiDelta::Text(s)) => {
                self.text.push_str(&s);
                None
            }
            None => None,
        }
    }

    fn finish(&mut self) -> Option<String> {
        if self.text.trim().is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.text))
        }
    }
}

/// opencode `--format json` decoder. The answer arrives whole as one `text`
/// event's `part.text` at completion; a multi-step run may emit several, in
/// which case the last wins. `reasoning` events forward live (usually a UI
/// no-op — they land with the answer).
pub(crate) struct OpenCodeDecoder {
    answer: Option<String>,
}

impl OpenCodeDecoder {
    pub(crate) fn new() -> Self {
        Self { answer: None }
    }
}

impl Decoder for OpenCodeDecoder {
    fn decode_line(&mut self, line: &str) -> Option<String> {
        match decode_opencode_stream_line(line) {
            Some(OpenCodeDelta::Thinking(t)) => Some(t),
            Some(OpenCodeDelta::Text(s)) => {
                self.answer = Some(s); // last wins
                None
            }
            None => None,
        }
    }

    fn finish(&mut self) -> Option<String> {
        self.answer.take().filter(|s| !s.trim().is_empty())
    }
}

/// codex `--json` decoder. opencode-shaped: the answer arrives whole as one
/// `agent_message` (or drifted `assistant_message`) item's `text` at
/// `item.completed`; the last one wins. `reasoning` items forward live when
/// present — best-effort, since they are account/org dependent and often
/// absent (Issue #10746); their absence is normal, never an error.
pub(crate) struct CodexDecoder {
    answer: Option<String>,
}

impl CodexDecoder {
    pub(crate) fn new() -> Self {
        Self { answer: None }
    }
}

impl Decoder for CodexDecoder {
    fn decode_line(&mut self, line: &str) -> Option<String> {
        match decode_codex_stream_line(line) {
            Some(CodexDelta::Thinking(t)) => Some(t),
            // Live progress (turn boundary / tool-use item.started): forwarded
            // so a silent-by-design CLI shows visible activity. Carries its
            // own trailing newline — a milestone, like claude's.
            Some(CodexDelta::Progress(p)) => Some(format!("{p}\n")),
            Some(CodexDelta::Text(s)) => {
                self.answer = Some(s); // last wins
                None
            }
            None => None,
        }
    }

    fn finish(&mut self) -> Option<String> {
        self.answer.take().filter(|s| !s.trim().is_empty())
    }
}

#[cfg(test)]
mod tests;
