#!/usr/bin/env python3
"""Extract real diff samples from aic's own git history into samples.jsonl.

Each sample: id, category, subject, body, files, diff (bounded lines).
The diff is the raw patch text (from `diff --git` onward) — the same shape
Generator::generate_commit_message receives. Categories are quota-balanced:
multi-file, refactor, fix, feature, tests, docs.
"""
import json, subprocess, sys, re
from pathlib import Path
from collections import Counter

OUT = Path(__file__).parent / "samples.jsonl"
MAX_DIFF_LINES = 120  # keep prompts bounded like real usage
WALK = "-1200"

# category -> how many samples to keep
QUOTAS = {"multi-file": 8, "refactor": 8, "fix": 8, "feature": 8, "tests": 2, "docs": 2}
TYPE_CAT = {
    "feat": "feature", "fix": "fix", "refactor": "refactor", "docs": "docs",
    "test": "tests", "chore": "chore", "perf": "perf", "style": "style", "ci": "ci",
}

def git(*args):
    return subprocess.run(["git", *args], capture_output=True, text=True, check=True).stdout

raw = git("log", "--no-merges", f"--format=@@%H%x1f%s%x1f%b%x1e", WALK)
records = [r for r in raw.split("\x1e") if r.strip()]
picked = {}   # category -> [sample]
seen = set()
for r in records:
    parts = r.strip().split("\x1f")
    sha = parts[0].removeprefix("@@").strip()
    subject = parts[1].strip() if len(parts) > 1 else ""
    body = parts[2].strip() if len(parts) > 2 else ""
    if not sha or subject.startswith("Merge") or sha in seen:
        continue
    seen.add(sha)
    m = re.match(r"^([a-z]+)(?:\(([^)]+)\))?:", subject)
    if not m:
        continue
    typ = m.group(1)
    if typ not in TYPE_CAT:
        continue
    diff = git("show", "--format=", "-m", "--first-parent", sha)
    if not diff.strip():
        continue
    lines = diff.splitlines()
    nfiles = sum(1 for l in lines if l.startswith("diff --git "))
    added = sum(1 for l in lines if l.startswith("+") and not l.startswith("+++"))
    removed = sum(1 for l in lines if l.startswith("-") and not l.startswith("---"))
    if added + removed < 3:
        continue
    if nfiles >= 3:
        cat = "multi-file"
    elif typ == "refactor":
        cat = "refactor"
    elif typ == "fix":
        cat = "fix"
    elif typ == "feat":
        cat = "feature"
    elif typ == "test":
        cat = "tests"
    elif typ == "docs":
        cat = "docs"
    else:
        continue
    if len(picked.get(cat, [])) >= QUOTAS[cat]:
        continue
    if len(lines) > MAX_DIFF_LINES:
        lines = lines[:MAX_DIFF_LINES] + ["... (truncated)"]
    picked.setdefault(cat, []).append({
        "id": f"s{sum(len(v) for v in picked.values())+1:02d}",
        "sha": sha,
        "category": cat,
        "type": typ,
        "files": nfiles,
        "added": added,
        "removed": removed,
        "reference": {"subject": subject, "body": body},
        "diff": "\n".join(lines),
    })
    if all(len(picked.get(c, [])) >= q for c, q in QUOTAS.items()):
        break

samples = [s for cat in QUOTAS for s in picked.get(cat, [])]
with open(OUT, "w") as f:
    for s in samples:
        f.write(json.dumps(s) + "\n")

print(f"wrote {len(samples)} samples to {OUT}")
print("categories:", dict(Counter(s["category"] for s in samples)))
if len(samples) < sum(QUOTAS.values()):
    print("WARNING: history exhausted before quotas filled; missing:", 
          {c: QUOTAS[c] - len(picked.get(c, [])) for c in QUOTAS if len(picked.get(c, [])) < QUOTAS[c]})
