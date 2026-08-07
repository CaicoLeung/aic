#!/usr/bin/env python3
"""Deterministic commit-message quality scorer for the AIC benchmark.

Scores a generated message against the rubric in RUBRIC.md. Pure stdlib, no
LLM calls: reproducible baseline + variant comparison. Each of the four
dimensions scores 0-5; total is 0-20.

Input: JSON lines with {"message": "...", "body": "..."} or a single JSON
object; or a raw string where the first line is the subject and the rest the
body. Output: JSON with per-dimension scores.
"""
import json
import re
import sys

TYPES = {"feat", "fix", "docs", "style", "refactor", "test", "chore", "perf", "ci", "build", "revert"}

IMPERATIVE_VERBS = {
    "add", "fix", "refactor", "remove", "rename", "update", "improve", "implement", "support",
    "handle", "use", "allow", "enable", "prevent", "avoid", "replace", "simplify", "clean",
    "cleanup", "restore", "revert", "bump", "drop", "introduce", "extract", "unify", "fold",
    "guard", "cap", "stream", "cut", "drain", "eliminate", "merge", "split", "parse", "render",
    "print", "hide", "show", "validate", "skip", "retry", "resolve", "document", "annotate",
    "build", "publish", "release", "generate", "create", "define", "derive", "skip", "fail",
    "pass", "verify", "assert", "check", "sort", "format", "normalize", "migrate", "persist",
    "load", "save", "read", "write", "open", "close", "start", "stop", "register", "export",
    "import", "install", "configure", "make", "keep", "wrap", "unpack", "convert", "move",
}

WHY_KEYWORDS = {
    "because", "avoid", "allows", "enables", "prevents", "fixes", "needed", "instead",
    "so that", "without", "avoids", "so", "thus", "thereby", "ensures", "reduces", "cuts",
}

STOPWORDS = {
    "the", "a", "an", "and", "or", "of", "to", "in", "on", "for", "with", "from", "by",
    "at", "is", "are", "was", "were", "be", "been", "it", "its", "this", "that", "as",
    "not", "no", "but", "into", "out", "over", "under", "via", "our", "their", "your",
    "his", "her", "we", "you", "they", "i", "do", "does", "did", "will", "would", "can",
    "could", "should", "also", "than", "then", "when", "while", "after", "before",
}


def parse_message(raw):
    """Parse a generated message into (subject, body). Lenient: tries strict
    JSON, then a bare `{...}` block, then treats first line as subject."""
    raw = (raw or "").strip()
    if not raw:
        return "", ""
    # Strict / fenced JSON
    for candidate in (raw, re.sub(r"^```[a-zA-Z]*\n|\n```$", "", raw)):
        try:
            obj = json.loads(candidate)
            if isinstance(obj, dict):
                msg = str(obj.get("message", "")).strip()
                body = str(obj.get("body") or "").strip()
                return msg, body
        except (json.JSONDecodeError, TypeError):
            pass
    # Bare {..} block anywhere (models occasionally add commentary)
    m = re.search(r"\{[^{}]*\"message\"[^{}]*\}", raw, re.S)
    if m:
        try:
            obj = json.loads(m.group(0))
            return str(obj.get("message", "")).strip(), str(obj.get("body") or "").strip()
        except (json.JSONDecodeError, TypeError):
            pass
    # Field-level fallback: works even when the JSON object is truncated
    # (model hit max_tokens mid-body).
    mm = re.search(r'"message"\s*:\s*"((?:[^"\\]|\\.)*)"', raw, re.S)
    mb = re.search(r'"body"\s*:\s*"((?:[^"\\]|\\.)*)"', raw, re.S)
    if mm:
        msg = json.loads('"' + mm.group(1) + '"')
        body = json.loads('"' + mb.group(1) + '"') if mb else ""
        return msg.strip(), body.strip()
    lines = raw.splitlines()
    return lines[0].strip(), "\n".join(lines[1:]).strip()


def parse_subject(message):
    """Parse '<type>(<scope>): <subject>' -> (type, scope, subject) or None."""
    m = re.match(r"^\s*([a-z][a-z-]*)(?:\(([a-z0-9][a-z0-9-]*)\))?:\s*(.*?)\s*$", message, re.S)
    if not m:
        return None
    return m.group(1), m.group(2) or "", m.group(3)


def significant_tokens(text):
    """Lowercased, stopword-free tokens of >= 4 chars."""
    toks = re.findall(r"[a-z][a-z0-9_]{3,}", text.lower())
    return {t for t in toks if t not in STOPWORDS}


def score_dimension_a(subject, message):
    parsed = parse_subject(message)
    if not parsed:
        return 0
    typ, _scope, subj = parsed
    score = 2 if typ in TYPES else 0
    if not subj:
        return score
    score += 1 if len(subj) <= 72 else 0
    score += 1 if not subj.rstrip().endswith(".") else 0
    return score


def score_dimension_b(subject, diff):
    parsed = parse_subject(subject)
    if not parsed or not parsed[2]:
        return 0
    typ, _scope, subj = parsed
    words = [w.lower() for w in re.findall(r"[a-zA-Z]+", subj)]
    score = 2 if words and words[0] in IMPERATIVE_VERBS else 0
    sig = [w for w in words if len(w) >= 4 and w not in STOPWORDS]
    score += 1 if len(sig) >= 2 else 0
    score += 1 if not re.search(r"[.!?;:]\s*$", subj) and not re.search(r"\b[A-Z][a-z]+\b", subj[1:]) else 0
    diff_toks = significant_tokens(diff)
    score += 1 if sig and any(w in diff_toks for w in sig) else 0
    return score


def score_dimension_c(body, added, removed, files):
    complex_diff = files >= 3 or (added + removed) >= 30
    body_present = bool(body)
    score = 2 if (complex_diff and body_present) or (not complex_diff and not body_present) else 0
    if body_present and len(body) <= 300:
        score += 1
    elif not body_present and not complex_diff:
        score += 1  # lean: no body is fine for tiny diffs
    low = body.lower()
    score += 2 if any(k in low for k in WHY_KEYWORDS) else 0
    return score


def score_dimension_d(subject, body, diff):
    toks = significant_tokens(subject + " " + body)
    if not toks:
        return 0
    low = diff.lower()
    # substring match is forgiving of plural/derived forms ("path" in "paths")
    matched = sum(1 for t in toks if t in low)
    return min(5.0, 5.0 * matched / len(toks))


def score_message(raw, diff, added=0, removed=0, files=1):
    """Score one generated message. Returns dict of dimension scores + total."""
    message, body = parse_message(raw)
    subject, _b = message, body  # message IS the subject line
    a = score_dimension_a(subject, message)
    b = score_dimension_b(subject, diff)
    c = score_dimension_c(body, added, removed, files)
    d = score_dimension_d(subject, body, diff)
    total = a + b + c + d
    return {
        "message": message,
        "body": body,
        "scores": {"a_conventional": a, "b_subject": b, "c_body": c, "d_relevance": d},
        "total": total,
    }


def main():
    import argparse

    ap = argparse.ArgumentParser(description="Score generated commit messages against RUBRIC.md")
    ap.add_argument("--message", help="single message (or JSON {message, body}) to score")
    ap.add_argument("--file", help="JSONL file of messages (one per line)")
    ap.add_argument("--diff", default="", help="diff text used for relevance scoring")
    ap.add_argument("--added", type=int, default=0)
    ap.add_argument("--removed", type=int, default=0)
    ap.add_argument("--files", type=int, default=1)
    args = ap.parse_args()

    if args.message:
        result = score_message(args.message, args.diff, args.added, args.removed, args.files)
        print(json.dumps(result, indent=2))
    elif args.file:
        for line in open(args.file):
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
                raw = obj.get("message") or obj.get("generated") or line
            except json.JSONDecodeError:
                raw = line
            result = score_message(raw, args.diff, args.added, args.removed, args.files)
            print(json.dumps(result))
    else:
        ap.error("need --message or --file")


if __name__ == "__main__":
    main()
