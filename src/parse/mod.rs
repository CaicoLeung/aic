//! Tolerant parsing of model replies — the decode layer both backends share.
//!
//! The API backend ([`crate::llm`]) and the CLI-agent backend
//! ([`crate::cli_agent`]) must turn raw model text into typed values the same
//! way: strip a wrapping code fence, parse the JSON payload amid stray prose,
//! and classify unusable output for the shared retry policy. Neither backend
//! hosts that logic; both import it from here.
//!
//! [`classify_retry`] is the single rig→[`RetryReason`] mapping — the only
//! place the retry domain touches a rig type, kept here (not in
//! [`crate::retry`]) so the retry module stays provider-agnostic.
//!
//! Split out of `llm.rs`: these helpers were Backend-neutral but lived in the
//! API backend, forcing `cli_agent.rs` to reverse-import from its sibling
//! backend.

use crate::retry::RetryReason;
use anyhow::Result;
use rig::completion::StructuredOutputError;

/// The single rig→[`RetryReason`] mapping: rig's
/// [`StructuredOutputError::EmptyResponse`] (no content) and
/// [`StructuredOutputError::DeserializationError`] (content truncated
/// mid-generation) are the retryable "no usable content" failures; anything
/// else — a wrapped rig completion failure (auth, rate limit, network) or an
/// unrelated error — is `None`, so the caller propagates the original error
/// unchanged.
pub(crate) fn classify_retry(err: &anyhow::Error) -> Option<RetryReason> {
    match err.downcast_ref::<StructuredOutputError>() {
        Some(StructuredOutputError::EmptyResponse) => Some(RetryReason::Empty),
        Some(StructuredOutputError::DeserializationError(_)) => Some(RetryReason::Truncated),
        _ => None,
    }
}

/// Strip a surrounding ```…``` code fence if the model ignored the "no fences"
/// instruction. Only touches a fence that wraps the entire output; partial
/// fences (e.g. a fenced block legitimately inside the file) are left alone.
pub(crate) fn strip_code_fence(mut s: &str) -> &str {
    s = s.strip_suffix('\n').unwrap_or(s).trim();
    if !s.starts_with("```") {
        return s;
    }
    // Drop the opening fence line (``` or ```lang).
    let Some(nl) = s.find('\n') else {
        return s;
    };
    s = &s[nl + 1..];
    // Drop a trailing closing fence.
    let trimmed_end = s.trim_end();
    if let Some(idx) = trimmed_end.rfind("```")
        && trimmed_end[idx..].trim() == "```"
    {
        return trimmed_end[..idx].trim();
    }
    s.trim()
}

/// Parse a JSON-structured LLM response, tolerating the stray prose or code
/// fence models occasionally emit around the payload. Strips a wrapping
/// ```` ``` ````-fence, jumps to the first value start (skipping any leading
/// prose), and lets serde_json's streaming deserializer parse exactly one value
/// — so trailing commentary is ignored without us hand-rolling brace matching.
///
/// A parse failure is reported as
/// [`StructuredOutputError::DeserializationError`] — the same classification
/// rig uses for a truncated `prompt_typed` response — so the shared retry
/// policy ([`classify_retry`]) treats tolerant-parse failures exactly like
/// typed-path truncation. The raw text rides in an anyhow context (the
/// downcast in `classify_retry` still finds the underlying error).
pub(crate) fn parse_json_response<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T> {
    let body = strip_code_fence(raw);
    let start = body.find(['{', '[']).unwrap_or(0);
    let mut stream =
        serde_json::Deserializer::from_str(body[start..].trim_start()).into_iter::<T>();
    match stream.next() {
        Some(Ok(value)) => Ok(value),
        Some(Err(e)) => Err(
            anyhow::Error::new(StructuredOutputError::DeserializationError(e)).context(format!(
                "failed to parse LLM JSON response\n--- raw ---\n{raw}"
            )),
        ),
        None => Err(
            anyhow::Error::new(StructuredOutputError::DeserializationError(
                serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "LLM response contained no JSON value",
                )),
            ))
            .context(format!("--- raw ---\n{raw}")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::BatchPlanOutput;
    use crate::retry::RetryReason;

    /// [`classify_retry`] is the boundary mapping every retry seam relies on:
    /// the two unusable-content shapes become retryable reasons — including
    /// through the anyhow context `parse_json_response` adds — and anything
    /// else is `None`, propagating unchanged.
    #[test]
    fn classify_retry_maps_unusable_content() {
        assert!(matches!(
            classify_retry(&anyhow::Error::new(StructuredOutputError::EmptyResponse)),
            Some(RetryReason::Empty)
        ));
        let json_err = serde_json::from_str::<serde_json::Value>("not json")
            .expect_err("must be a parse error");
        assert!(matches!(
            classify_retry(&anyhow::Error::new(
                StructuredOutputError::DeserializationError(json_err)
            )),
            Some(RetryReason::Truncated)
        ));
        // Context-wrapped, as parse_json_response produces it.
        let wrapped = anyhow::Error::new(StructuredOutputError::DeserializationError(
            serde_json::from_str::<serde_json::Value>("nope").expect_err("must be a parse error"),
        ))
        .context("failed to parse LLM JSON response");
        assert!(matches!(
            classify_retry(&wrapped),
            Some(RetryReason::Truncated)
        ));
        assert!(
            classify_retry(&anyhow::anyhow!("network / auth / etc.")).is_none(),
            "non-content errors must not be retried"
        );
    }

    // --- Tolerant output parsing (moved from generator.rs with the seam) ---

    #[test]
    fn parse_json_response_ignores_leading_prose_and_trailing_junk() {
        // Fence sits inside leading prose (so strip_code_fence can't help);
        // jump-to-`{` + serde_json's streaming parser handle both ends.
        let raw = "Here is the plan:\n```json\n{\"batches\": []}\n```\ndone";
        let out: BatchPlanOutput = parse_json_response(raw).unwrap();
        assert!(out.batches.is_empty());
    }

    #[test]
    fn parse_json_response_handles_escaped_quotes() {
        let raw =
            r#"{"batches": [{"changes": [{"file": "a\"b.rs", "hunks": []}], "reason": "x"}]}"#;
        let out: BatchPlanOutput = parse_json_response(raw).unwrap();
        assert_eq!(out.batches[0].changes[0].file, "a\"b.rs");
    }

    #[test]
    fn parse_json_response_returns_err_when_no_json() {
        let res: anyhow::Result<BatchPlanOutput> = parse_json_response("no json here at all");
        assert!(res.is_err());
    }

    /// The contract that makes batch-plan truncation retryable: a
    /// tolerant-parse failure must surface as
    /// [`StructuredOutputError::DeserializationError`], the same class rig's
    /// `prompt_typed` produces for truncated content — so [`classify_retry`]
    /// retries it with the same policy as the typed path.
    #[test]
    fn parse_failure_is_classified_as_deserialization_error() {
        let err = parse_json_response::<BatchPlanOutput>("no json here").expect_err("must fail");
        assert!(
            matches!(classify_retry(&err), Some(RetryReason::Truncated)),
            "parse failures must be retried like typed-path truncation"
        );
        assert!(matches!(
            err.downcast_ref::<StructuredOutputError>(),
            Some(StructuredOutputError::DeserializationError(_))
        ));
    }

    #[test]
    fn strip_fence_removes_wrapping_fence() {
        assert_eq!(strip_code_fence("```\nfn main() {}\n```"), "fn main() {}");
    }

    #[test]
    fn strip_fence_removes_language_tag() {
        assert_eq!(strip_code_fence("```rust\nlet x = 1;\n```"), "let x = 1;");
    }

    #[test]
    fn strip_fence_leaves_plain_content_alone() {
        assert_eq!(strip_code_fence("fn main() {}"), "fn main() {}");
    }

    #[test]
    fn strip_fence_leaves_inner_fences_alone() {
        // A fenced block that is legitimately part of the file is not stripped —
        // only a fence wrapping the *entire* output is.
        let inner = "text before\n\n```rs\ncode\n```\n\ntext after";
        assert_eq!(strip_code_fence(inner), inner);
    }

    #[test]
    fn strip_fence_bare_opening_without_newline_is_left_alone() {
        // A lone opening fence with no content line after it has nothing to
        // strip — returned unchanged rather than producing a dangling slice.
        assert_eq!(strip_code_fence("```"), "```");
        assert_eq!(strip_code_fence("```rust"), "```rust");
    }

    #[test]
    fn strip_fence_opening_with_unclean_closing_keeps_trailing_text() {
        // A closing fence followed by trailing text is not a clean wrapper: the
        // opening fence line is still dropped (so the body is exposed to the
        // tolerant parser), but the trailing "``` text" stays in place — the
        // fallthrough keeps whatever followed the opening fence.
        assert_eq!(
            strip_code_fence("```rust\nlet x = 1;\n``` trailing"),
            "let x = 1;\n``` trailing"
        );
    }
}
