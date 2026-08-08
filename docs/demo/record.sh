#!/usr/bin/env bash
# docs/demo/record.sh
#
# Records docs/demo/aic-hunk-split.cast (a real aic run via demo-run.sh) with
# asciinema, then renders it to docs/demo/aic-hunk-split.gif with agg.
#
# The cast is committed alongside the GIF so the demo can be re-rendered or
# re-recorded deterministically — it is a faithful capture of a real aic
# hunk-level split, not a hand-animated fake.
#
# Provider creds are passed straight through to demo-run.sh: export
# AIC_DEMO_BACKEND + AIC_DEMO_API_KEY (and optionally AIC_DEMO_MODEL), or let
# demo-run.sh resolve your existing aic config / a local Ollama.
#
# Usage:
#   AIC_DEMO_BACKEND=deepseek AIC_DEMO_API_KEY=sk-... ./docs/demo/record.sh
#
# Requires: asciinema (pip install asciinema) and agg (brew install agg).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

DEMO_DIR="docs/demo"
CAST="$DEMO_DIR/aic-hunk-split.cast"
GIF="$DEMO_DIR/aic-hunk-split.gif"

for tool in asciinema agg; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "missing '$tool'." >&2
    echo "  pip install asciinema   # or: uv tool install asciinema" >&2
    echo "  brew install agg        # asciinema → GIF renderer" >&2
    exit 1
  }
done

# A fixed terminal size keeps the GIF layout stable across re-recordings.
COLS="${AIC_DEMO_COLS:-76}"
ROWS="${AIC_DEMO_ROWS:-20}"

echo "▶ recording $CAST (${COLS}x${ROWS})…"
# `stty cols/rows` inside the command sizes the PTY asciinema allocates;
# --idle-time-limit collapses pauses between sections so the GIF stays tight.
asciinema rec \
  --overwrite \
  --command "bash -lc 'stty cols $COLS rows $ROWS 2>/dev/null || true; AIC_BIN=\"${AIC_BIN:-}\" AIC_DEMO_BACKEND=\"${AIC_DEMO_BACKEND:-}\" AIC_DEMO_API_KEY=\"${AIC_DEMO_API_KEY:-}\" AIC_DEMO_MODEL=\"${AIC_DEMO_MODEL:-}\" ./docs/demo/demo-run.sh'" \
  --idle-time-limit 1.5 \
  "$CAST"

# Normalise the cast header width so a headless recording still renders at the
# intended aspect ratio (agg treats the header width as authoritative).
tmp="$(mktemp)"
python3 - "$CAST" "$tmp" "$COLS" <<'PY'
import json, sys
src, dst, cols = sys.argv[1], sys.argv[2], int(sys.argv[3])
with open(src) as f:
    lines = f.readlines()
header = json.loads(lines[0])
header["width"] = cols
lines[0] = json.dumps(header) + "\n"
with open(dst, "w") as f:
    f.writelines(lines)
PY
mv "$tmp" "$CAST"

echo "▶ rendering $GIF…"
# --speed tightens the real ~50s run into a snappy loop; --fps and a modest
# cols keep the file small enough for GitHub's repo-header preview.
agg \
  --cols "$COLS" \
  --rows "$ROWS" \
  --speed "${AIC_DEMO_SPEED:-2.5}" \
  --fps-cap 12 \
  --theme monokai \
  --font-family "JetBrains Mono,Fira Code,DejaVu Sans Mono,Menlo,monospace" \
  "$CAST" "$GIF"

echo "✓ done:"
ls -lh "$CAST" "$GIF"
