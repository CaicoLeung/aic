//! Secrets redaction gate (issue #155, ADR 0017): refuse to send any
//! secret-shaped diff content to the LLM.
//!
//! Every payload the commit Run sends to a Backend — the batch-plan Diff JSON
//! envelope, the per-batch Drafted Message diff input, and the staged
//! single-commit diff — passes this gate first. The detector is a pure
//! function over text: a hardcoded table of structural token patterns plus a
//! PEM-header check. There is deliberately no entropy scoring and no
//! `api_key = "…"` assignment heuristic (issue #155's benign-content
//! contract — base64 blobs and long hashes must pass — rules both out), and
//! no config field: the escape hatch is the one-off `--no-redact` flag, the
//! same Run-scoped policy as `--hint` (ADR 0008's single-source-of-truth
//! spirit). The resolve workflow's whole-file payloads are a tracked
//! follow-up; both paths will share this module.

/// One detected secret in a scanned payload: the user-facing pattern kind
/// and a masked preview of the matched text (first 4 chars + `…`). The full
/// match is never surfaced — reprinting a secret into terminal scrollback
/// would defeat the gate's purpose.
pub(crate) struct Finding {
    /// Pattern-kind label, e.g. `"AWS access key ID"`.
    pub(crate) kind: &'static str,
    /// Masked preview, e.g. `"AKIA…"`.
    pub(crate) masked: String,
}

/// Which characters may extend a token past its structural prefix.
#[derive(Clone, Copy)]
enum Tail {
    /// Uppercase letters and digits only (AWS key IDs).
    UpperAlnum,
    /// Letters and digits, no separators (classic `sk-` keys).
    Alnum,
    /// Letters, digits, `_`, `-` (modern prefixed tokens, base64url).
    Token,
}

/// One structural token pattern: a fixed prefix plus a minimum run of tail
/// characters. Prefixes are specific enough that word boundaries are not
/// needed — no benign prose contains `AKIA` + 16 upper-alnum characters.
struct Pattern {
    /// User-facing kind label naming this secret family.
    kind: &'static str,
    /// Literal prefix, case-sensitive.
    prefix: &'static str,
    /// Minimum tail length after the prefix.
    min_tail: usize,
    /// Character class allowed in the tail.
    tail: Tail,
}

/// The v1 vocabulary (ADR 0017). Ordered: family-specific prefixes
/// (`sk-ant-`, `sk-proj-`) run before the generic `sk-` so a modern key
/// reports its precise family; the generic arm cannot double-fire on them
/// (their tails contain `-`, which the `sk-` tail class rejects).
const PATTERNS: &[Pattern] = &[
    Pattern {
        kind: "AWS access key ID",
        prefix: "AKIA",
        min_tail: 16,
        tail: Tail::UpperAlnum,
    },
    Pattern {
        kind: "AWS access key ID",
        prefix: "ASIA",
        min_tail: 16,
        tail: Tail::UpperAlnum,
    },
    Pattern {
        kind: "GitHub token",
        prefix: "ghp_",
        min_tail: 36,
        tail: Tail::Token,
    },
    Pattern {
        kind: "GitHub token",
        prefix: "gho_",
        min_tail: 36,
        tail: Tail::Token,
    },
    Pattern {
        kind: "GitHub token",
        prefix: "ghu_",
        min_tail: 36,
        tail: Tail::Token,
    },
    Pattern {
        kind: "GitHub token",
        prefix: "ghs_",
        min_tail: 36,
        tail: Tail::Token,
    },
    Pattern {
        kind: "GitHub token",
        prefix: "ghr_",
        min_tail: 36,
        tail: Tail::Token,
    },
    Pattern {
        kind: "GitHub token",
        prefix: "github_pat_",
        min_tail: 22,
        tail: Tail::Token,
    },
    Pattern {
        kind: "Anthropic API key",
        prefix: "sk-ant-",
        min_tail: 20,
        tail: Tail::Token,
    },
    Pattern {
        kind: "OpenAI API key",
        prefix: "sk-proj-",
        min_tail: 20,
        tail: Tail::Token,
    },
    Pattern {
        kind: "OpenAI API key",
        prefix: "sk-svcacct-",
        min_tail: 20,
        tail: Tail::Token,
    },
    Pattern {
        kind: "OpenAI API key",
        prefix: "sk-",
        min_tail: 20,
        tail: Tail::Alnum,
    },
    Pattern {
        kind: "Google API key",
        prefix: "AIza",
        min_tail: 35,
        tail: Tail::Token,
    },
    Pattern {
        kind: "Slack token",
        prefix: "xoxb-",
        min_tail: 10,
        tail: Tail::Token,
    },
    Pattern {
        kind: "Slack token",
        prefix: "xoxa-",
        min_tail: 10,
        tail: Tail::Token,
    },
    Pattern {
        kind: "Slack token",
        prefix: "xoxp-",
        min_tail: 10,
        tail: Tail::Token,
    },
    Pattern {
        kind: "Slack token",
        prefix: "xoxr-",
        min_tail: 10,
        tail: Tail::Token,
    },
    Pattern {
        kind: "Slack token",
        prefix: "xoxs-",
        min_tail: 10,
        tail: Tail::Token,
    },
    Pattern {
        kind: "Stripe secret key",
        prefix: "sk_live_",
        min_tail: 20,
        tail: Tail::Alnum,
    },
    Pattern {
        kind: "Stripe secret key",
        prefix: "rk_live_",
        min_tail: 20,
        tail: Tail::Alnum,
    },
    Pattern {
        kind: "GitLab token",
        prefix: "glpat-",
        min_tail: 20,
        tail: Tail::Token,
    },
];

/// PEM private-key header line, e.g. `-----BEGIN OPENSSH PRIVATE KEY-----`.
/// Matched structurally (both markers on one line) rather than by prefix
/// table: the algorithm name between `BEGIN` and `PRIVATE KEY` varies
/// (RSA, EC, DSA, ENCRYPTED, OPENSSH, or nothing).
const PEM_BEGIN: &str = "-----BEGIN";
const PEM_END: &str = "PRIVATE KEY-----";

/// Does `c` belong to `class`'s character set?
fn in_class(c: char, class: Tail) -> bool {
    match class {
        Tail::UpperAlnum => c.is_ascii_uppercase() || c.is_ascii_digit(),
        Tail::Alnum => c.is_ascii_alphanumeric(),
        Tail::Token => c.is_ascii_alphanumeric() || c == '_' || c == '-',
    }
}

/// Longest run of `class` characters starting at `bytes[from..]` of `text`.
fn tail_run(text: &str, from: usize, class: Tail) -> usize {
    text[from..]
        .bytes()
        .take_while(|&b| in_class(b as char, class))
        .count()
}

/// Find the first `prefix` + ≥`min_tail` `class` characters occurrence.
/// Returns the full matched token (prefix + tail) on a hit.
fn find_structural(text: &str, prefix: &str, min_tail: usize, class: Tail) -> Option<String> {
    let mut from = 0;
    while let Some(at) = text[from..].find(prefix) {
        let start = from + at;
        let after = start + prefix.len();
        let run = tail_run(text, after, class);
        if run >= min_tail {
            return Some(text[start..after + run].to_string());
        }
        from = start + 1;
    }
    None
}

/// True if any line carries a PEM private-key header (`-----BEGIN … PRIVATE
/// KEY-----` with the algorithm name varying).
fn has_pem_block(text: &str) -> bool {
    text.lines()
        .any(|line| line.contains(PEM_BEGIN) && line.contains(PEM_END))
}

/// Mask a matched token for display: first 4 chars + `…`. Matches are ASCII
/// by construction (every pattern's prefix and tail are ASCII), so slicing
/// at 4 bytes cannot split a character.
fn masked(token: &str) -> String {
    let cut = token.len().min(4);
    format!("{}…", &token[..cut])
}

/// Scan one payload for secret-shaped content. Findings are deduplicated by
/// kind (first occurrence wins for the masked preview) — the message names
/// each secret *family* present, not every copy.
pub(crate) fn scan(text: &str) -> Vec<Finding> {
    let mut findings: Vec<Finding> = Vec::new();
    for pattern in PATTERNS {
        if findings.iter().any(|f| f.kind == pattern.kind) {
            continue;
        }
        if let Some(token) = find_structural(text, pattern.prefix, pattern.min_tail, pattern.tail) {
            findings.push(Finding {
                kind: pattern.kind,
                masked: masked(&token),
            });
        }
    }
    if findings.iter().all(|f| f.kind != "private key block (PEM)") && has_pem_block(text) {
        findings.push(Finding {
            kind: "private key block (PEM)",
            masked: format!("{PEM_BEGIN}…"),
        });
    }
    findings
}

/// Cap on offending files listed in the refusal message. Past it the list
/// collapses to "… and N more files" — enough to act on, not a wall.
// ponytail: fixed cap, not configurable — a longer list has never changed
// what the user does next (inspect the files, or --no-redact).
const LIST_CAP: usize = 5;

/// The Redaction Gate: refuse before any LLM call when a payload contains
/// secret-shaped content. `diff_per_file` is `(path, diff text)` per file —
/// raw diffs, before any JSON wrapping, so the error names files directly.
/// Called only on a Run's *initial* payloads (ADR 0017): the confirmation
/// menu's Re-generate redraft re-sends already-gated content and stays
/// ungated by design.
///
/// Returns `Err` naming every offending file and pattern kind (masked
/// previews, capped at [`LIST_CAP`] files) plus the `--no-redact` override;
/// `Ok(())` when the payloads are clean.
pub(crate) fn gate(diff_per_file: &[(String, String)]) -> anyhow::Result<()> {
    let offending: Vec<(&str, Vec<Finding>)> = diff_per_file
        .iter()
        .filter_map(|(path, diff)| {
            let findings = scan(diff);
            (!findings.is_empty()).then_some((path.as_str(), findings))
        })
        .collect();
    if offending.is_empty() {
        return Ok(());
    }
    let mut msg = String::from(
        "refused to run: secret-shaped content found — nothing was sent to the LLM:\n",
    );
    for (path, findings) in offending.iter().take(LIST_CAP) {
        let kinds: Vec<String> = findings
            .iter()
            .map(|f| format!("{} ({})", f.kind, f.masked))
            .collect();
        msg.push_str(&format!("  {path}: {}\n", kinds.join(", ")));
    }
    if offending.len() > LIST_CAP {
        msg.push_str(&format!(
            "  … and {} more files\n",
            offending.len() - LIST_CAP
        ));
    }
    msg.push_str("override for this Run only: aic --no-redact");
    anyhow::bail!("{msg}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pattern family fires on a real-shaped token of that family.
    #[test]
    fn detects_each_pattern_family() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "AWS access key ID",
                "aws_key_id=AKIAIOSFODNN7EXAMPLE",
                "AKIA…",
            ),
            (
                "AWS access key ID",
                "key = \"ASIAIOSFODNN7EXAMPLE\"",
                "ASIA…",
            ),
            (
                "GitHub token",
                "token: ghp_16C7e42F292c6912E7710c838347Ae178B4a36Aa",
                "ghp_…",
            ),
            (
                "GitHub token",
                "github_pat_11A0ABCD1234efgh5678IJkl9012mn34",
                "gith…",
            ),
            (
                "Anthropic API key",
                "sk-ant-api03-48charsofpayloadfollowhere000000",
                "sk-a…",
            ),
            (
                "OpenAI API key",
                "sk-proj-abcdefghij1234567890abcdefghij",
                "sk-p…",
            ),
            (
                "OpenAI API key",
                "sk-svcacct-abcdefghij1234567890xyz",
                "sk-s…",
            ),
            (
                "OpenAI API key",
                "\"api_key\": \"sk-1234567890abcdefghij45678\"",
                "sk-1…",
            ),
            (
                "Google API key",
                "AIzaSyA1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q",
                "AIza…",
            ),
            ("Slack token", "xoxb-1234567890-abcdef", "xoxb…"),
            (
                "Stripe secret key",
                "sk_live_51H8xQk2Z3y4v5b6N7m8L9",
                "sk_l…",
            ),
            (
                "Stripe secret key",
                "rk_live_51H8xQk2Z3y4v5b6N7m8L9",
                "rk_l…",
            ),
            // Zeros, not a realistic token: GitHub push protection flags
            // realistic-shaped fixtures — the matcher only needs the
            // `glpat-` prefix + a 20-char token-class tail.
            ("GitLab token", "glpat-00000000000000000001", "glpa…"),
        ];
        for (kind, text, preview) in cases {
            let findings = scan(text);
            assert!(
                findings.iter().any(|f| f.kind == *kind),
                "{text:?}: expected {kind:?}, got {:?}",
                findings.iter().map(|f| f.kind).collect::<Vec<_>>()
            );
            let hit = findings.iter().find(|f| f.kind == *kind).unwrap();
            assert_eq!(hit.masked, *preview, "{text:?}");
        }
    }

    /// Every PEM private-key header variant fires, bare `BEGIN` lines
    /// (certificates) do not.
    #[test]
    fn detects_pem_private_key_blocks_only() {
        for header in [
            "-----BEGIN RSA PRIVATE KEY-----",
            "-----BEGIN EC PRIVATE KEY-----",
            "-----BEGIN DSA PRIVATE KEY-----",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "-----BEGIN ENCRYPTED PRIVATE KEY-----",
            "-----BEGIN PRIVATE KEY-----",
        ] {
            assert!(
                scan(&format!("{header}\nMIIabc=="))
                    .iter()
                    .any(|f| f.kind == "private key block (PEM)"),
                "{header} must be detected"
            );
        }
        // Public certificates and CSRs are not private keys.
        assert!(scan("-----BEGIN CERTIFICATE-----\nMIIabc==").is_empty());
        assert!(scan("-----BEGIN PUBLIC KEY-----\nMIIabc==").is_empty());
    }

    /// Benign content passes: base64 blobs, long hex/SHA hashes, prose with
    /// short prefixed words (issue #155 acceptance contract).
    #[test]
    fn benign_content_passes() {
        let base64_blob = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42AAAAAA==";
        let sha256 = "a3f5b7c9d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f70";
        let prose = "the risk-averse sky diver skied fast; ask-anything is our motto";
        let jwt_like = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJVadQssw5c";
        for text in [base64_blob, sha256, prose, jwt_like] {
            assert!(scan(text).is_empty(), "benign content must pass: {text:?}");
        }
    }

    /// A prefix without a long-enough tail does not fire (short `sk-`
    /// words, truncated tokens).
    #[test]
    fn prefix_without_full_tail_does_not_fire() {
        assert!(scan("sk-too-short ghp_abc AIza123 xyz").is_empty());
        assert!(scan("AKIAshort").is_empty());
    }

    /// The gate names every offending file with its kinds, masks previews,
    /// caps the list, and carries the override hint.
    #[test]
    fn gate_names_files_kinds_and_override() {
        let files = vec![
            ("clean.rs".to_string(), "fn main() {}".to_string()),
            (
                "config.toml".to_string(),
                "aws_key = \"AKIAIOSFODNN7EXAMPLE\"".to_string(),
            ),
            (
                "id_rsa".to_string(),
                "-----BEGIN RSA PRIVATE KEY-----".to_string(),
            ),
        ];
        let err = gate(&files).expect_err("must refuse");
        let msg = format!("{err:#}");
        assert!(msg.contains("config.toml"), "{msg}");
        assert!(msg.contains("AWS access key ID (AKIA…)"), "{msg}");
        assert!(msg.contains("id_rsa"), "{msg}");
        assert!(msg.contains("private key block (PEM)"), "{msg}");
        assert!(msg.contains("--no-redact"), "{msg}");
        assert!(
            !msg.contains("AKIAIOSFODNN7EXAMPLE"),
            "full secret must not print: {msg}"
        );

        gate(&[("clean.rs".to_string(), "let x = 1;".to_string())])
            .expect("clean payload must pass");
    }

    /// Past the list cap the refusal collapses the tail to a count.
    #[test]
    fn gate_caps_the_offending_file_list() {
        let files: Vec<(String, String)> = (0..7)
            .map(|i| {
                (
                    format!("f{i}.txt"),
                    format!("k{i} = \"AKIAIOSFODNN7EXAMPLE\""),
                )
            })
            .collect();
        let msg = format!("{:?}", gate(&files).unwrap_err());
        assert!(msg.contains("… and 2 more files"), "{msg}");
    }
}
