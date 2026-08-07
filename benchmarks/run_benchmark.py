#!/usr/bin/env python3
"""Benchmark runner for AIC commit-message prompts.

Runs a prompt variant against the sample set (samples.jsonl), asks the LLM
(OpenAI-compatible chat completions, DeepSeek by default) for a commit message
per diff, caches raw generations in results/<variant>.json, then scores every
cached generation with score.py and writes results/<variant>.summary.json.

Design goals (learned from the first failed run — a bare LLM loop that timed
out the whole heartbeat):
  * every LLM call has a hard per-call timeout
  * results are cached per variant: reruns skip already-generated samples
  * --limit bounds how many samples are generated in one invocation
  * scoring is deterministic and offline

Usage:
  python3 benchmarks/run_benchmark.py --variant baseline [--limit 12] [--force]
  python3 benchmarks/run_benchmark.py --variant variant-whatwhy --prompt benchmarks/prompts/variant-whatwhy.txt
  python3 benchmarks/run_benchmark.py --variant baseline --score-only   # offline rescore
"""
import argparse
import concurrent.futures as cf
import json
import os
import re
import sys
import time
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from score import score_message

HERE = Path(__file__).parent
SAMPLES = HERE / "samples.jsonl"
RESULTS = HERE / "results"
PROMPTS = HERE / "prompts"
DEFAULT_PROMPT_FILE = HERE / "prompt-baseline.txt"

API_URL = os.environ.get("AIC_BENCH_API_URL", "https://api.deepseek.com/chat/completions")
API_KEY = os.environ.get("DEEPSEEK_API_KEY") or os.environ.get("LLM_API_KEY", "")
MODEL = os.environ.get("LLM_MODEL", "deepseek-v4-flash")
PER_CALL_TIMEOUT = int(os.environ.get("AIC_BENCH_TIMEOUT", "90"))
MAX_WORKERS = int(os.environ.get("AIC_BENCH_WORKERS", "4"))


def load_samples(limit=None):
    samples = []
    for line in open(SAMPLES):
        line = line.strip()
        if line:
            samples.append(json.loads(line))
    return samples[:limit] if limit else samples


def load_prompt(variant):
    """Built-in baseline comes from prompt.rs (SYSTEM_PROMPT_GIT_MESSAGE,
    exported to prompt-baseline.txt by export_prompt.py); variants come from
    prompts/<variant>.txt."""
    if variant == "baseline":
        return DEFAULT_PROMPT_FILE.read_text().strip()
    f = PROMPTS / f"{variant}.txt"
    if not f.exists():
        sys.exit(f"no prompt file for variant '{variant}': {f} (create it or use --prompt)")
    return f.read_text().strip()


def ask_llm(prompt, diff):
    payload = {
        "model": MODEL,
        "messages": [
            {"role": "system", "content": prompt},
            {"role": "user", "content": diff},
        ],
        "temperature": 0.2,
        "max_tokens": 1500,
    }
    req = urllib.request.Request(
        API_URL,
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {API_KEY}"},
    )
    # DeepSeek reasoning models intermittently return empty `content` after
    # spending the budget on reasoning_content (see src/retry.rs). Retry up to
    # 3 attempts like the production retry policy.
    last = None
    for attempt in range(3):
        with urllib.request.urlopen(req, timeout=PER_CALL_TIMEOUT) as resp:
            data = json.loads(resp.read())
        content = (data["choices"][0]["message"].get("content") or "").strip()
        if content:
            return content
        last = data["choices"][0]["message"].get("reasoning_content") or "(none)"
        time.sleep(0.5 * (attempt + 1))
    raise RuntimeError(f"empty content after 3 attempts (reasoning_content: {last[:120]})")


def generate_one(args):
    variant, sample, prompt = args
    sid = sample["id"]
    try:
        raw = ask_llm(prompt, sample["diff"])
    except Exception as e:  # network, timeout, 4xx/5xx
        return {"id": sid, "ok": False, "error": str(e)[:200]}
    return {"id": sid, "ok": True, "generated": raw, "time": time.time()}


def main():
    ap = argparse.ArgumentParser(description="Run the AIC commit-message benchmark")
    ap.add_argument("--variant", required=True, help="variant name (baseline or prompts/<name>.txt)")
    ap.add_argument("--prompt", help="explicit prompt file (overrides variant lookup)")
    ap.add_argument("--limit", type=int, default=0, help="max samples to GENERATE this run (0 = all)")
    ap.add_argument("--force", action="store_true", help="regenerate samples already cached")
    ap.add_argument("--score-only", action="store_true", help="rescore cached results without calling the LLM")
    args = ap.parse_args()

    RESULTS.mkdir(exist_ok=True)
    out_file = RESULTS / f"{args.variant}.json"
    cached = {}
    if out_file.exists():
        for line in open(out_file):
            line = line.strip()
            if line:
                rec = json.loads(line)
                cached[rec["id"]] = rec

    samples = load_samples(args.limit)
    prompt = load_prompt(args.variant) if not args.prompt else Path(args.prompt).read_text().strip()

    if not args.score_only:
        todo = [s for s in samples if s["id"] not in cached or args.force]
        print(f"[{args.variant}] {len(todo)}/{len(samples)} samples to generate (cached: {len(cached)})")
        if todo:
            with cf.ThreadPoolExecutor(max_workers=MAX_WORKERS) as ex:
                for rec in ex.map(generate_one, [(args.variant, s, prompt) for s in todo]):
                    cached[rec["id"]] = rec
                    status = "ok" if rec["ok"] else f"FAIL: {rec.get('error','')}"
                    print(f"  {rec['id']} {status}")
                    if rec["ok"]:
                        with open(out_file, "w") as f:
                            for sid in [s["id"] for s in samples]:
                                if sid in cached:
                                    f.write(json.dumps(cached[sid]) + "\n")

    # Score every cached generation for the samples in scope.
    by_id = {s["id"]: s for s in samples}
    scored = []
    for sid, sample in by_id.items():
        rec = cached.get(sid)
        if not rec or not rec.get("ok"):
            continue
        s = score_message(rec["generated"], sample["diff"], sample["added"], sample["removed"], sample["files"])
        scored.append({"id": sid, "category": sample["category"], **s})

    if not scored:
        sys.exit(f"no scored samples for variant '{args.variant}' — run without --score-only first")

    import statistics
    dims = ["a_conventional", "b_subject", "c_body", "d_relevance"]
    summary = {
        "variant": args.variant,
        "model": MODEL,
        "n": len(scored),
        "samples": [s["id"] for s in scored],
        "mean_total": round(statistics.mean(s["total"] for s in scored), 2),
        "mean_dims": {d: round(statistics.mean(s["scores"][d] for s in scored), 2) for d in dims},
        "per_category": {
            cat: round(statistics.mean(s["total"] for s in scored if s["category"] == cat), 2)
            for cat in sorted({s["category"] for s in scored})
        },
    }
    (RESULTS / f"{args.variant}.summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print("\n=== summary:", args.variant, "===")
    print(f"  n={summary['n']}  mean_total={summary['mean_total']}")
    print("  dims:", json.dumps(summary["mean_dims"]))
    print("  per_category:", json.dumps(summary["per_category"]))


if __name__ == "__main__":
    main()
