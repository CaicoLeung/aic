#!/usr/bin/env python3
"""Generate the curated, re-renderable demo cast for the README hero GIF.

`docs/demo/aic-hunk-split.cast` is a *curated* cast: it depicts a real `aic`
run's actual output (the commit messages aic produces for this sample repo),
paced into a tight ~8 s loop with no spinner flicker. It is the source for
`docs/demo/aic-hunk-split.gif` (rendered with `agg`).

Why curated instead of a raw recording? A raw `asciinema rec` of `aic` is
dominated by the indicatif spinner (~90% of frames are redraw noise), which
flickers badly when compressed to an 8 s GIF. This generator emits only the
meaningful output — the labels aic really prints ("Analyzing changes", the
per-commit "Generating commit message" work, the `[k/n] ✓` results, and the
final `git log`) — so the loop is clean and honest.

Re-render end to end:

    python3 docs/demo/make-cast.py
    agg docs/demo/aic-hunk-split.cast docs/demo/aic-hunk-split.gif --theme monokai --font-size 15 --line-height 1.3

The messages below mirror the recorded fixture in
`examples/before-after/after.txt` and the real model output. Commit hashes are
non-deterministic in a real run, so they are randomly generated here for visual
realism — the *count and message text* are the deterministic part.
"""
import json
import random
import time

random.seed(42)
WIDTH, HEIGHT = 76, 20


def sha():
    chars = "0123456789abcdef"
    return "".join(random.choice(chars) for _ in range(7))


def main():
    events = []
    t = 0.0

    def emit(s, dt=0.0):
        nonlocal t
        t += dt
        events.append([round(t, 3), "o", s])

    def type_cmd(cmd, per=0.028):
        nonlocal t
        for ch in cmd:
            t += per
            events.append([round(t, 3), "o", ch])
        t += 0.12
        events.append([round(t, 3), "o", "\n"])

    # Scene 1 — show the mixed working-tree change: ONE file, three concerns.
    emit("\x1b[2J\x1b[H", 0.05)
    type_cmd("$ git diff --stat")
    emit(
        " \x1b[1;33msrc/main.rs\x1b[0m | \x1b[32m13\x1b[0m "
        "\x1b[32m+++++++\x1b[0m\x1b[31m-----\x1b[0m\n"
        " 1 file changed, \x1b[32m8 insertions(+)\x1b[0m, "
        "\x1b[31m5 deletions(-)\x1b[0m\n",
        0.5,
    )

    # Scene 2 — aic partitions the hunks and lands one commit per concern.
    # "Analyzing changes" is the real label aic's batch-plan renderer prints.
    type_cmd("$ aic")
    emit("  \x1b[1mAnalyzing changes…\x1b[0m\n", 0.8)
    emit(
        "  \x1b[32m[1/3] ✓\x1b[0m " + sha() + " "
        "\x1b[1mfix(main): guard against missing name argument\x1b[0m\n",
        0.6,
    )
    emit(
        "  Indexing args[1] panicked when no name was given; now prints usage "
        "and exits.\n",
        0.45,
    )
    emit(
        "  \x1b[32m[2/3] ✓\x1b[0m " + sha() + " "
        "\x1b[1mfeat(greet): support --upper flag to uppercase name\x1b[0m\n",
        0.6,
    )
    emit(
        "  Display the name in uppercase when --upper is passed on the command "
        "line.\n",
        0.45,
    )
    emit(
        "  \x1b[32m[3/3] ✓\x1b[0m " + sha() + " "
        "\x1b[1mrefactor(access): simplify log_access formatting\x1b[0m\n",
        0.6,
    )
    emit(
        "\n  \x1b[1m3 atomic commits\x1b[0m from one file — one per logical "
        "change.\n",
        0.6,
    )

    # Scene 3 — the resulting clean history.
    type_cmd("$ git log --oneline")
    shas = [sha() for _ in range(4)]
    emit(
        "\x1b[33m" + shas[0] + "\x1b[0m refactor(access): simplify log_access formatting\n"
        "\x1b[33m" + shas[1] + "\x1b[0m feat(greet): support --upper flag to uppercase name\n"
        "\x1b[33m" + shas[2] + "\x1b[0m fix(main): guard against missing name argument\n"
        "\x1b[90m" + shas[3] + "\x1b[0m chore: seed sample-repo base state\n",
        0.9,
    )
    # Hold the final frame so the result is readable before the loop restarts.
    t += 1.3
    events.append([round(t, 3), "o", ""])

    header = {
        "version": 2,
        "width": WIDTH,
        "height": HEIGHT,
        "timestamp": int(time.time()),
        "title": "aic — hunk-level atomic commits",
        "idle_time_limit": 2.0,
    }
    with open("docs/demo/aic-hunk-split.cast", "w") as f:
        f.write(json.dumps(header) + "\n")
        for e in events:
            f.write(json.dumps(e) + "\n")
    print(f"wrote docs/demo/aic-hunk-split.cast  duration={t:.1f}s events={len(events)}")


if __name__ == "__main__":
    main()
