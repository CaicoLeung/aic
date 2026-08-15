# Research: 32 flat modules under src/ — should we redesign the workspace structure?

> **Short answer: No — keep the flat layout (Option A).** The flat `src/*.rs`
> layout is not accidental accumulation: every module was extracted on purpose
> as a named domain concept (CONTEXT.md documents each one), the names are
> already domain-descriptive, and the measured coupling shows the natural
> clusters are *not* real boundaries — 37 of 72 directed `use crate::` edges
> (51%) cross the candidate cluster lines, so subdirectories would regroup
> files without reducing coupling, at a mechanical cost of ~191 path-rewrite
> lines across 29 files plus `main.rs` and 3 new `mod` roots. ADR-0006's
> precedent ("the size reflects genuine coupling, not accidental accumulation")
> applies to the directory question exactly as it did to `git.rs`. The actual
> pain — findability — is cheaply fixed where it lives: a grouped, commented
> module list in `main.rs` (and CONTEXT.md already maps every domain term to
> its `src/*.rs` file).

> **Revision (2026-08-15):** The measurement and recommendation above stand
> for what they measured — *regrouping* existing modules into topic buckets
> (Options B/C). They do not cover splitting an oversized single module into
> a directory, which is what ADR 0015 adopted: `setup.rs` and `git.rs`
> split into `setup/` and `git/` directories (ADR 0016 supersedes ADR
> 0006's one-*file* reading), test mods moved to sibling `tests.rs` files,
> the module root moved from `main.rs` to `lib.rs`, and — overriding this
> document's flat-file default — every module became a directory
> (`foo/mod.rs`), so `src/` holds only `lib.rs`, `main.rs`, and module
> directories. The anti-regrouping evidence stands: no topic buckets.

> **Second revision (2026-08-15, later the same day):** the owner then
> adopted topic grouping anyway — five domain roots (`core/ git/ llm/
> render/ workflow/`) — overruling this document's anti-regrouping
> recommendation **on navigation grounds, not coupling grounds**. The
> re-measurement after the splits (78 edges, 55% crossing group lines)
> confirms the coupling claim was never contested: directories are a topic
> index, not a dependency firewall. What the owner bought is findability —
> five domains at `src/` instead of 32 siblings — at a one-time cost of
> ~232 path-rewrite sites. Lesson recorded: coupling is the wrong yardstick
> for a navigation question.

---

## Question

`src/` holds 32 top-level `.rs` files (~19.6k LOC, `wc -l src/*.rs`):
`setup.rs` 2108, `git.rs` 1550, `config.rs` 1529, `cli_agent.rs` 1316,
`display.rs` 1251, `markdown.rs` 1248, `progress.rs` 1227 … down to `cli.rs`
36, `types.rs` 43, `main.rs` 101, plus `src/e2e/` (7 files, 3527 LOC). Is the
flat layout a problem worth a structural redesign, or should it stay?

---

## Evidence (repo)

### 1. The flat layout is original and has never been reshuffled

`git log --diff-filter=R --summary -- src/` returns **nothing** — no module
has ever been renamed or moved. All 31 modules besides `main.rs` arrived as
*additions* (extractions or features), flat from day one:

| Module(s) | Added by | Date |
|---|---|---|
| `git`, `llm`, `prompt`, `generator`, `cli`, `config` | initial build (`a632cef`, `7a9d7b4`, `4b58b0c`, `e670257`, `655d9f0`, `851a57f`) | 2026-05-12/17 |
| `conflict.rs` | `6c1ead8` refactor(conflict): extract … into a deep module (#95) | 2026-08-07 |
| `retry.rs` | `2ad89d8` refactor(retry): unify Drafted-Message retry (#79) | 2026-08-04 |
| `grouping.rs` | `0172b9e` feat(grouping): deterministic block grouping v1 (#96) | 2026-08-08 |
| `setup.rs`, `input.rs` | `d5a533b` refactor(config): extract setup wizard and input primitives (#102) | 2026-08-08 |
| `completion.rs` | `edcca3c` refactor(completion): extract … into deep module (#103) | 2026-08-08 |
| `progress.rs`, `layout.rs` | `6b40983` refactor(display): extract live progress surface into progress + layout (#104) | 2026-08-08 |
| `cursor.rs`, `decoder.rs` | `0e2bb75` refactor: extract cursor + decoder modules (#124) | 2026-08-11 |
| `reasoning_feed.rs` | `7e375f4` refactor: extract reasoning-feed driver (#130) | 2026-08-12 |
| `run.rs`, `resolve.rs` | `4e037b0` refactor: split workflows out of main into run and resolve modules | 2026-08-15 |
| `diff_json.rs` | `e173c9a` feat(diff_json): add shared diff-JSON envelope module | 2026-08-15 |
| `parse.rs` | `89ce19b` refactor(parse): extract shared LLM parsing helpers | 2026-08-15 |
| `markdown.rs` | `b18cb77` refactor(progress): extract markdown painter into markdown module | 2026-08-15 |
| `commit_type.rs`, `palette.rs` | `05ea989` refactor: split types.rs into commit_type and palette modules | 2026-08-15 |

So the history shows a steady *extract-a-deep-module* rhythm (13 extraction
commits since 2026-08-04) — the file count is the residue of deliberate
deepening, not entropy. The repo already has a term for this discipline:
CONTEXT.md's glossary maps each module by name — "Run module" (`src/run.rs`,
CONTEXT.md:56-58), "Conflict module" (`src/conflict.rs`, CONTEXT.md:122-124),
"Resolve module" (`src/resolve.rs`, CONTEXT.md:126-128), "Palette"
(`src/palette.rs`, CONTEXT.md:132-134), "Commit Type" (`src/commit_type.rs`,
CONTEXT.md:136-138), "Reasoning feed" (`src/reasoning_feed.rs`,
CONTEXT.md:140-142), "Markdown renderer" (`src/markdown.rs`,
CONTEXT.md:144-146), "Block grouping" (`src/grouping.rs`, CONTEXT.md:28).

### 2. No ADR governs src layout — but ADR-0006 is the governing precedent

None of the 13 ADRs in `docs/adr/` addresses directory structure. The closest
is **ADR-0006** ("Git stays one module", docs/adr/0006-git-stays-one-module.md),
whose reasoning transfers verbatim to the layout question:

> "**Negative:** `git.rs` remains a large file. Accepted: the size reflects
> genuine coupling, not accidental accumulation. Depth is a property of the
> interface, not the line count." (ADR-0006:86-89)

And its meta-consequence:

> "Future architecture reviews can stop at '… the remainder is coupled by the
> shared handle and deliberately unified.' This ADR is the reason not to
> re-suggest splitting `Git` further." (ADR-0006:82-85)

ADR-0006 also records that the one split that *did* happen (`conflict.rs` out
of `git.rs`) was justified because the conflict domain was self-contained
(ADR-0006:50-53) — i.e. this repo splits when a **real seam** exists, and the
2026-08-15 `run`/`resolve` split (`4e037b0`) followed the same test.

### 3. Measured coupling: the candidate clusters are not real boundaries

Method: for each `src/*.rs`, extract every `use crate::<mod>` first segment,
dedupe per file (72 unique directed edges; `grep -oE 'use crate::[a-z_]+'`
per file, 2026-08-15 tree). Assign each module to the natural cluster a
redesign would use — render = `display/layout/cursor/palette/progress/markdown/
decoder`; git-side = `git/diff/diff_json/staging/conflict/grouping`;
llm = `llm/cli_agent/prompt/parse/retry/reasoning_feed/generator`; workflow =
`main/run/confirm/resolve/setup/input/completion/config/commit_type/types/
update/cli`.

Result: **35 intra-cluster vs 37 cross-cluster edges (51% cross).** The
workflow spine fans out to everything: `run` alone depends on 15 modules —
5 intra (config, confirm, input, resolve, types) and **10 cross** (cursor,
diff, diff_json, display, generator, git, grouping, progress,
reasoning_feed, staging). The most-depended-on modules straddle clusters:
`generator` (8 importers — workflow, git-side, and llm files all use it),
`git` (6), `display` (5), `cli_agent` (5), `retry` (5), `diff` (5). Even the
smallest cluster assignment leaks: `display → commit_type` and
`palette → commit_type` are render→workflow edges, because Commit Type is
vocabulary both sides consume (CONTEXT.md:136-138).

That is the ADR-0006 signature at directory scale: the coupling is in the
domain (a Run reads git, drives the LLM, and paints the terminal in one
spine), not in the file arrangement. Subdirectories would move files between
buckets without making any edge shorter.

### 4. Churn cost of regrouping (the Option B bill)

If the 22 render/git/llm modules moved into e.g. `src/git/`, `src/llm/`,
`src/render/`, every path mentioning them rewrites. Counting every line in
`src/*.rs` + `src/e2e/*.rs` that mentions `crate::<moved-module>` (uses,
fully-qualified paths, and test code — a floor, since multi-segment paths
rewrite the same line once):

- **191 lines across 29 files** — worst offenders: `src/resolve.rs` 25,
  `src/config.rs` 24, `src/progress.rs` 16, `src/run.rs` 13,
  `src/grouping.rs` 11, `src/markdown.rs` 11, `src/e2e/common.rs` 6.
- Plus the 31 `pub mod` declarations in `src/main.rs:5-35` become three
  `mod` roots + re-export decisions, plus 3 new `mod.rs`/root files.
- Plus 7 intra-doc links `` [`mod`] `` across src and `docs/adr/` retarget.

Against that: zero functional gain (same edges, one more path segment), and a
blurred `git log --follow`/blame surface for every one of the 13 extraction
commits cited above.

---

## External conventions

- **The Rust Book, ch. 7** — the language prescribes *when* to split, never
  *that a project must bucket into directories: start with modules in one
  file, move a module to its own file when it grows, and let a module become
  a directory only when it itself has sub-modules
  (https://doc.rust-lang.org/book/ch07-02-defining-scope-and-privacy-with-modules.html,
  https://doc.rust-lang.org/book/ch07-05-separating-modules-into-different-files.html).
  aic's flat 31 single-file modules are exactly the prescribed shape: each is
  one file because none has sub-modules.
- **bat** (sharkdp/bat, a CLI of comparable surface): `src/` is **25 flat
  `.rs` files** plus only `assets/`, `bin/`, `syntax_mapping/` directories
  (https://github.com/sharkdp/bat/tree/master/src). Flat at a larger file
  count than aic's.
- **gitui** (extrawurst/gitui, ratatui TUI): `src/` top level is ~17 flat
  files (`app.rs`, `cmdbar.rs`, `input.rs`, `queue.rs`, …) plus 7 concern
  directories (`components/` with 10 files, `keys/`, `popups/`, `tabs/`,
  `ui/`, …) (https://github.com/extrawurst/gitui/tree/master/src). Note the
  directories exist because those concerns have *many small sibling files* —
  aic's render concern is 7 already-large files.
- **ratatui**: `src/` is 4 flat files + one `widgets/` directory holding the
  many widgets (https://github.com/ratatui-org/ratatui/tree/main/ratatui/src).
  Again: directories appear where file *count per concern* is high, not as a
  findability device.
- **ripgrep**: solves size with a *workspace of 11 crates*, and even the core
  binary crate is 6 flat files + `flags/` + `index/`
  (https://github.com/BurntSushi/ripgrep/tree/master/crates,
  …/crates/core/src). Workspace-splitting is the Rust answer when crate-level
  compile boundaries pay; aic at ~20k LOC has no such pressure.

Consensus across all four: flat-until-a-concern-sprawls. None regroups for
findability.

---

## Options

### (A) Keep flat + cheap findability aids — recommended

- Keep `src/*.rs` as is. Make the existing alphabetical `pub mod` list in
  `src/main.rs:5-35` a *grouped, commented* list (render / git / llm /
  workflow headers over the same 31 declarations — comment-only diff, no
  code churn, no ADR needed since it changes no structure).
- Lean on what already exists: CONTEXT.md's glossary is the module map —
  every entry names its `src/*.rs` file (CONTEXT.md:28, 56-58, 122-146), and
  `grep -l <term> src/*.rs` over 32 domain-named files is a one-command hop
  from any CONTEXT.md term.
- Cost: ~30 lines of comments. Gain: the "which file is this" answer sits in
  the two places a newcomer already reads (main.rs, CONTEXT.md).

### (B) Subdirectories by concern — measured, rejected

Concrete tree the clusters imply:

```
src/
  git/       mod.rs  git  diff  diff_json  staging  conflict  grouping
  llm/       mod.rs  llm  cli_agent  prompt  parse  retry  reasoning_feed  generator
  render/    mod.rs  display  layout  cursor  palette  progress  markdown  decoder
  (flat)     main run confirm resolve setup input completion config
             commit_type types update cli   + e2e/
```

Measured cost (this file, §4): **191 path-rewrite lines across 29 files**,
31→3-root mod rewiring in `main.rs`, 3 new module roots, 7 intra-doc-link
retargets, blame/history noise over 13 extraction commits. Measured benefit:
**zero edges removed** — 37/72 dependency edges still cross cluster lines
(§3), so no `use` gets shorter, no visibility tightens, no compile boundary
appears. It also strands `commit_type` (render files import it; it was
assigned nowhere good) and makes `generator` — the most-imported module (8
importers) — an `llm/` resident that workflow and git-side files reach into.
This is "a seam for seam's sake," the exact phrase ADR-0006 uses to reject
its `git::exec` alternative (ADR-0006:57-64).

### (C) What the evidence suggests instead: keep extracting on demand

The history (§1) shows the layout already self-corrects the way ADR-0006
endorses: when an architecture review finds a *self-contained* domain
(conflict out of git, run/resolve out of main, commit_type/palette out of
types), it is extracted as a new flat, domain-named file — 13 such commits
since 2026-08-04, most recently `parse.rs` (`89ce19b`) and
`commit_type.rs`/`palette.rs` (`05ea989`) on 2026-08-15. If any single file
later hurts (`setup.rs` at 2108 lines is the only one above 2k), the remedy
is another ADR-0006-style decision about *that file's internal cohesion*,
not a directory reshuffle of 31 bystanders. The one structural lever the
evidence does support, if compile times or reuse ever demand it, is the
ripgrep-style workspace split — a much bigger decision with real payoff
boundaries, to be weighed then, not now.

---

## Recommendation

**Option A.** Keep `src/` flat; add a grouped, commented module list in
`main.rs` (and keep CONTEXT.md's glossary as the term→file map). The file
count is the visible residue of 13 deliberate module-deepening extractions,
the names are already the domain vocabulary (CONTEXT.md), 51% of dependency
edges cross any candidate cluster line so directories buy nothing structural,
and Option B's 191-line rewrite bill pays for cosmetics only. ADR-0006's
closing logic is the precedent: accept the size, because it reflects genuine
(domain) coupling, not accidental accumulation — and record that this note is
the reason not to re-suggest a directory redesign.
