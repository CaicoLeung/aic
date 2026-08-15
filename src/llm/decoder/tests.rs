use super::*;

// ---- claude stream-json decoding -----------------------------------------
//
// The decoder is exercised against event shapes captured from a real
// `claude -p … --output-format stream-json --include-partial-messages`
// run (claude 2.1.x), so the field paths match production output exactly.
// Payloads avoid nested quotes so the test inputs stay readable.

#[test]
fn decode_extracts_thinking_delta() {
    let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"analyzing the diff"}},"session_id":"s","uuid":"u"}"#;
    assert_eq!(
        decode_claude_stream_line(line),
        Some(ClaudeDelta::Thinking("analyzing the diff".to_string()))
    );
}

#[test]
fn decode_extracts_text_delta() {
    let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hello world"}},"session_id":"s","uuid":"u"}"#;
    assert_eq!(
        decode_claude_stream_line(line),
        Some(ClaudeDelta::Text("hello world".to_string()))
    );
}

#[test]
fn decode_extracts_init_milestone() {
    // The `system/init` event carries model + tools + mcp_servers; the
    // milestone summarizes them so the cold start is visible progress.
    let line = r#"{"type":"system","subtype":"init","model":"glm-5.2","tools":["Bash","Read","Edit"],"mcp_servers":[{"name":"zai","status":"connected"},{"name":"other","status":"failed"}]}"#;
    assert_eq!(
        decode_claude_stream_line(line),
        Some(ClaudeDelta::Milestone(
            "Initialized — model glm-5.2, 3 tools, 1/2 MCP servers connected".to_string()
        ))
    );
}

#[test]
fn decode_extracts_hook_started_milestone() {
    let line = r#"{"type":"system","subtype":"hook_started","hook_event":"SessionStart","hook_name":"SessionStart:startup"}"#;
    assert_eq!(
        decode_claude_stream_line(line),
        Some(ClaudeDelta::Milestone(
            "Running SessionStart hooks…".to_string()
        ))
    );
}

#[test]
fn decode_extracts_terminal_result_event() {
    // The authoritative final-answer carrier; shape trimmed to the fields
    // the decoder reads (real events also carry usage/cost/etc.).
    let line = r#"{"is_error":false,"result":"hello world","type":"result","subtype":"success"}"#;
    assert_eq!(
        decode_claude_stream_line(line),
        Some(ClaudeDelta::Result("hello world".to_string()))
    );
}

#[test]
fn decode_drops_non_milestone_system_and_assistant_noise() {
    // SessionStart `hook_started`/`init` now surface as milestones (see
    // sibling tests); the still-noise subtypes — `hook_response`,
    // `status` — and assistant message snapshots stay filtered so they
    // never reach the reasoning window or the answer.
    let hook_response =
        r#"{"type":"system","subtype":"hook_response","hook_id":"h","outcome":"success"}"#;
    let status = r#"{"type":"system","subtype":"status","status":"ready"}"#;
    let snapshot = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#;
    assert_eq!(decode_claude_stream_line(hook_response), None);
    assert_eq!(decode_claude_stream_line(status), None);
    assert_eq!(decode_claude_stream_line(snapshot), None);
    // Non-delta stream_events (content_block_start) are dropped too.
    let start = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0}}"#;
    assert_eq!(decode_claude_stream_line(start), None);
}

#[test]
fn decode_tolerates_non_json_and_partial_lines() {
    // A partial flush or a non-JSON line that leaked onto stdout must not
    // abort the stream — dropped, not fatal.
    assert_eq!(decode_claude_stream_line("not json at all"), None);
    assert_eq!(decode_claude_stream_line(""), None);
    assert_eq!(decode_claude_stream_line("   "), None);
    // Valid JSON missing the expected fields → None, not a panic.
    assert_eq!(
        decode_claude_stream_line(r#"{"type":"stream_event"}"#),
        None
    );
    assert_eq!(decode_claude_stream_line(r#"{"type":"unknown"}"#), None);
}

/// Feed `blob` through a decoder line-by-line, collecting what each
/// `decode_line` returns (what would be forwarded to the reasoning window)
/// and the `finish()` answer. The decoder-interface analogue of the old
/// `decode_*_answer(blob)` single-shot helpers.
fn run_decoder<D: Decoder>(mut dec: D, blob: &str) -> (Vec<String>, Option<String>) {
    let mut forwarded = Vec::new();
    for line in blob.lines() {
        if let Some(f) = dec.decode_line(line) {
            forwarded.push(f);
        }
    }
    (forwarded, dec.finish())
}

#[test]
fn claude_decoder_finish_prefers_terminal_result_over_text_deltas() {
    // Hook noise (a milestone, forwarded live) + a thinking delta (also
    // forwarded) + a partial text-delta fragment + the terminal result
    // event with the full authoritative answer. `result` wins at finish.
    let blob = [
        r#"{"type":"system","subtype":"hook_started","hook_id":"h"}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"t"}}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"partial"}}}"#,
        r#"{"type":"result","result":"full answer","subtype":"success"}"#,
    ]
    .join("\n");
    let (fwd, ans) = run_decoder(ClaudeDecoder::new(), &blob);
    assert_eq!(ans.as_deref(), Some("full answer"));
    // Reasoning streamed live; the answer text did not.
    assert!(fwd.iter().any(|s| s == "t"), "thinking forwarded: {fwd:?}");
    assert!(
        !fwd.iter().any(|s| s.contains("partial")),
        "answer not forwarded: {fwd:?}"
    );
}

#[test]
fn claude_decoder_finish_falls_back_to_concatenated_text_deltas() {
    // An error turn may omit the `result` event; the concatenated
    // `text_delta`s still reconstruct the answer.
    let blob = [
        r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hello "}}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"world"}}}"#,
    ]
    .join("\n");
    let (_, ans) = run_decoder(ClaudeDecoder::new(), &blob);
    assert_eq!(ans.as_deref(), Some("hello world"));
}

#[test]
fn claude_decoder_finish_returns_none_for_only_noise_or_empty() {
    // Pure noise (hooks/init/thinking) carries no answer text → None so
    // the caller surfaces a typed error rather than feeding empty to JSON.
    let noise = [
        r#"{"type":"system","subtype":"init"}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"t"}}}"#,
    ]
    .join("\n");
    assert_eq!(run_decoder(ClaudeDecoder::new(), &noise).1, None);
    assert_eq!(run_decoder(ClaudeDecoder::new(), "").1, None);
}

#[test]
fn claude_decoder_dedups_repeated_milestones_and_resets_on_thinking() {
    // claude emits repeated `SessionStart:startup` hook pairs; the decoder
    // forwards a milestone once, then suppresses its immediate repeat. A
    // thinking delta clears the dedup window, so a milestone re-fires
    // after reasoning resumes. Milestones carry a trailing newline.
    let init = r#"{"type":"system","subtype":"init","model":"m","tools":[],"mcp_servers":[]}"#;
    let think = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"x"}}}"#;
    let blob = [init, init, think, init].join("\n");
    let (fwd, _) = run_decoder(ClaudeDecoder::new(), &blob);
    // Two milestone forwards (the first init, then the post-thinking
    // re-fire) — not three; the consecutive duplicate was deduped.
    let milestones = fwd.iter().filter(|s| s.ends_with('\n')).count();
    assert_eq!(milestones, 2, "dedup: {fwd:?}");
    // The thinking delta reset the window → the trailing init re-fired.
    assert!(
        fwd.iter().any(|s| s == "x"),
        "thinking forwarded + reset: {fwd:?}"
    );
}

// ---- pi `--mode json` decoding -------------------------------------------
//
// Shapes captured from a real `pi --no-tools --mode json -p` run: every
// delta is a `message_update` carrying an `assistantMessageEvent`.

#[test]
fn decode_pi_extracts_thinking_delta() {
    let line = r#"{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"User"}}"#;
    assert_eq!(
        decode_pi_stream_line(line),
        Some(PiDelta::Thinking("User".to_string()))
    );
}

#[test]
fn decode_pi_extracts_text_delta() {
    let line = r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":1,"delta":"Fore"}}"#;
    assert_eq!(
        decode_pi_stream_line(line),
        Some(PiDelta::Text("Fore".to_string()))
    );
}

#[test]
fn decode_pi_drops_lifecycle_and_session_noise() {
    // session/agent_start/turn_start/message lifecycle markers carry no
    // delta and must not reach the reasoning window or the answer.
    let session = r#"{"type":"session","version":3,"id":"s"}"#;
    let turn = r#"{"type":"turn_start"}"#;
    let thinking_start = r#"{"type":"message_update","assistantMessageEvent":{"type":"thinking_start","contentIndex":0}}"#;
    let text_end =
        r#"{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":1}}"#;
    let settled = r#"{"type":"agent_settled"}"#;
    for l in [session, turn, thinking_start, text_end, settled] {
        assert_eq!(decode_pi_stream_line(l), None, "noise leaked: {l}");
    }
}

#[test]
fn decode_pi_tolerates_non_json_and_missing_fields() {
    assert_eq!(decode_pi_stream_line("not json"), None);
    assert_eq!(decode_pi_stream_line(""), None);
    // Valid JSON, wrong type → None.
    assert_eq!(decode_pi_stream_line(r#"{"type":"session"}"#), None);
    // message_update missing the event → None.
    assert_eq!(decode_pi_stream_line(r#"{"type":"message_update"}"#), None);
}

#[test]
fn pi_decoder_concatenates_text_deltas() {
    // pi emits no terminal "result" event; the answer is the concatenation
    // of every text_delta, in arrival order. thinking deltas stream live
    // to the reasoning window (and are ignored for the answer).
    let blob = [
        r#"{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","delta":"t"}}"#,
        r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"hello "}}"#,
        r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"world"}}"#,
        r#"{"type":"agent_end"}"#,
    ]
    .join("\n");
    let (fwd, ans) = run_decoder(PiDecoder::new(), &blob);
    assert_eq!(ans.as_deref(), Some("hello world"));
    assert!(fwd.iter().any(|s| s == "t"), "thinking forwarded: {fwd:?}");
}

#[test]
fn pi_decoder_returns_none_when_only_thinking_or_noise() {
    let thinking_only = r#"{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","delta":"t"}}"#;
    assert_eq!(run_decoder(PiDecoder::new(), thinking_only).1, None);
    assert_eq!(run_decoder(PiDecoder::new(), "").1, None);
}

// ---- opencode `--format json` decoding -----------------------------------
//
// Shapes captured from a real `opencode run --format json` run: the full
// answer arrives as one `text` event's `part.text` at completion.

#[test]
fn decode_opencode_extracts_text_event() {
    let line = r#"{"type":"text","timestamp":1,"sessionID":"s","part":{"id":"p","type":"text","text":"hello world"}}"#;
    assert_eq!(
        decode_opencode_stream_line(line),
        Some(OpenCodeDelta::Text("hello world".to_string()))
    );
}

#[test]
fn decode_opencode_extracts_reasoning_event() {
    let line = r#"{"type":"reasoning","part":{"type":"reasoning","text":"thinking it over"}}"#;
    assert_eq!(
        decode_opencode_stream_line(line),
        Some(OpenCodeDelta::Thinking("thinking it over".to_string()))
    );
}

#[test]
fn decode_opencode_drops_step_lifecycle_noise() {
    let step_start = r#"{"type":"step_start","part":{"type":"step-start"}}"#;
    let step_finish = r#"{"type":"step_finish","part":{"type":"step-finish","tokens":{}}}"#;
    assert_eq!(decode_opencode_stream_line(step_start), None);
    assert_eq!(decode_opencode_stream_line(step_finish), None);
    // Non-JSON / missing text field → None.
    assert_eq!(decode_opencode_stream_line("not json"), None);
    assert_eq!(
        decode_opencode_stream_line(r#"{"type":"text","part":{"type":"text"}}"#),
        None
    );
}

#[test]
fn opencode_decoder_takes_last_text_event() {
    // The answer arrives whole as one `text` event. A multi-step run could
    // emit several; the final answer is the last one. reasoning events
    // stream live to the reasoning window.
    let blob = [
        r#"{"type":"step_start","part":{"type":"step-start"}}"#,
        r#"{"type":"reasoning","part":{"type":"reasoning","text":"t"}}"#,
        r#"{"type":"text","part":{"type":"text","text":"intermediate"}}"#,
        r#"{"type":"text","part":{"type":"text","text":"final answer"}}"#,
        r#"{"type":"step_finish","part":{"type":"step-finish"}}"#,
    ]
    .join("\n");
    let (fwd, ans) = run_decoder(OpenCodeDecoder::new(), &blob);
    assert_eq!(ans.as_deref(), Some("final answer"));
    assert!(fwd.iter().any(|s| s == "t"), "reasoning forwarded: {fwd:?}");
}

#[test]
fn opencode_decoder_returns_none_without_text_event() {
    let no_text = r#"{"type":"step_start","part":{"type":"step-start"}}"#;
    assert_eq!(run_decoder(OpenCodeDecoder::new(), no_text).1, None);
    assert_eq!(run_decoder(OpenCodeDecoder::new(), "").1, None);
}

// ---- codex `--json` decoding ---------------------------------------------
//
// Shapes per the codex `exec --json` event stream (openai/codex). Reasoning
// and answer text arrive only at `item.completed`; `agent_message` (codex
// ≥ v0.44.0) and its documented alias `assistant_message` (Issue #4776)
// are both accepted — a tolerant parse that is load-bearing against
// version skew.

#[test]
fn decode_codex_extracts_agent_message() {
    let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"hello world"}}"#;
    assert_eq!(
        decode_codex_stream_line(line),
        Some(CodexDelta::Text("hello world".to_string()))
    );
}

#[test]
fn decode_codex_accepts_drifted_assistant_message_alias() {
    // Docs say `assistant_message`; codex v0.44.0 emits `agent_message`
    // (Issue #4776). Both must decode so a version skew never breaks the
    // answer extraction.
    let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"assistant_message","text":"hi"}}"#;
    assert_eq!(
        decode_codex_stream_line(line),
        Some(CodexDelta::Text("hi".to_string()))
    );
}

#[test]
fn decode_codex_extracts_reasoning_when_present() {
    let line = r#"{"type":"item.completed","item":{"id":"item_2","type":"reasoning","text":"thinking it over"}}"#;
    assert_eq!(
        decode_codex_stream_line(line),
        Some(CodexDelta::Thinking("thinking it over".to_string()))
    );
}

#[test]
fn decode_codex_drops_lifecycle_and_non_text_items() {
    // thread.*/turn.completed/item.updated carry no final text and no
    // useful progress; item.started for a non-tool item type is noise too.
    // (turn.STARTED and tool-use item.started ARE forwarded as Progress —
    // see the dedicated tests below.)
    let thread = r#"{"type":"thread.started","id":"t"}"#;
    let turn_completed = r#"{"type":"turn.completed","id":"t"}"#;
    let started = r#"{"type":"item.started","item":{"type":"agent_message"}}"#;
    let updated = r#"{"type":"item.updated","item":{"type":"agent_message","text":"partial"}}"#;
    // item.completed for a non-text item type (command_execution) is not
    // the answer and not progress → None.
    let cmd =
        r#"{"type":"item.completed","item":{"id":"c","type":"command_execution","text":"ls"}}"#;
    for l in [thread, turn_completed, started, updated, cmd] {
        assert_eq!(decode_codex_stream_line(l), None, "noise leaked: {l}");
    }
}

#[test]
fn decode_codex_forwards_turn_started_as_progress() {
    // codex is silent during reasoning; turn.started is the earliest
    // milestone, forwarded live so the reasoning window is not empty.
    let line = r#"{"type":"turn.started"}"#;
    assert_eq!(
        decode_codex_stream_line(line),
        Some(CodexDelta::Progress("codex turn started".to_string()))
    );
}

#[test]
fn decode_codex_forwards_command_started_as_progress() {
    // codex under the read-only sandbox routinely runs shell commands
    // before answering; surfacing them turns a silent wait into visible
    // progress. The `/bin/zsh -lc '…'` wrapper is stripped and the body
    // is capped.
    let line = r#"{"type":"item.started","item":{"id":"c","type":"command_execution","command":"/bin/zsh -lc 'git diff --cached | head -50'"}}"#;
    assert_eq!(
        decode_codex_stream_line(line),
        Some(CodexDelta::Progress(
            "codex: git diff --cached | head -50".to_string()
        ))
    );
}

#[test]
fn decode_codex_short_command_caps_long_bodies() {
    // A huge one-liner must not flood the reasoning window.
    let body = "echo ".to_string() + &"x".repeat(200);
    let line = format!(
        r#"{{"type":"item.started","item":{{"id":"c","type":"command_execution","command":"/bin/zsh -lc '{body}'"}}}}"#
    );
    match decode_codex_stream_line(&line) {
        Some(CodexDelta::Progress(p)) => {
            assert!(p.starts_with("codex: echo "), "got: {p}");
            assert!(p.ends_with('…'), "should be capped with ellipsis: {p}");
            // "codex: " (7) + 100 body chars + "…" (1) = 108.
            assert_eq!(p.chars().count(), 108);
        }
        other => panic!("expected Progress, got {other:?}"),
    }
}

#[test]
fn decode_codex_forwards_file_change_and_mcp_tool_started_as_progress() {
    let file_change = r#"{"type":"item.started","item":{"id":"f","type":"file_change"}}"#;
    assert_eq!(
        decode_codex_stream_line(file_change),
        Some(CodexDelta::Progress("codex: editing file".to_string()))
    );
    let mcp = r#"{"type":"item.started","item":{"id":"m","type":"mcp_tool_call"}}"#;
    assert_eq!(
        decode_codex_stream_line(mcp),
        Some(CodexDelta::Progress("codex: mcp tool call".to_string()))
    );
}

#[test]
fn decode_codex_tolerates_non_json_and_missing_fields() {
    assert_eq!(decode_codex_stream_line("not json"), None);
    assert_eq!(decode_codex_stream_line(""), None);
    // item.completed missing the item → None.
    assert_eq!(
        decode_codex_stream_line(r#"{"type":"item.completed"}"#),
        None
    );
    // item with an unknown type → None.
    assert_eq!(
        decode_codex_stream_line(
            r#"{"type":"item.completed","item":{"type":"mystery","text":"x"}}"#
        ),
        None
    );
}

#[test]
fn codex_decoder_takes_last_agent_message() {
    // A multi-step run may emit several agent_message items; the last is
    // the final answer. reasoning forwards live when present.
    let blob = [
        r#"{"type":"item.started","item":{"type":"agent_message"}}"#,
        r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"intermediate"}}"#,
        r#"{"type":"item.completed","item":{"id":"i2","type":"reasoning","text":"deliberating"}}"#,
        r#"{"type":"item.completed","item":{"id":"i3","type":"agent_message","text":"final answer"}}"#,
    ]
    .join("\n");
    let (fwd, ans) = run_decoder(CodexDecoder::new(), &blob);
    assert_eq!(ans.as_deref(), Some("final answer"));
    assert!(
        fwd.iter().any(|s| s == "deliberating"),
        "reasoning forwarded: {fwd:?}"
    );
}

#[test]
fn codex_decoder_reasoning_absence_is_normal() {
    // Issue #10746: with API-key auth codex emits NO reasoning items —
    // only agent_message. That must not be an error; the answer still
    // extracts and the reasoning window simply stayed empty.
    let blob = r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"just the answer"}}"#;
    let (fwd, ans) = run_decoder(CodexDecoder::new(), blob);
    assert_eq!(ans.as_deref(), Some("just the answer"));
    assert!(
        fwd.is_empty(),
        "no reasoning forwarded when none emitted: {fwd:?}"
    );
}

#[test]
fn codex_decoder_returns_none_without_agent_message() {
    let noise =
        r#"{"type":"item.completed","item":{"id":"c","type":"command_execution","text":"ls"}}"#;
    assert_eq!(run_decoder(CodexDecoder::new(), noise).1, None);
    assert_eq!(run_decoder(CodexDecoder::new(), "").1, None);
}
