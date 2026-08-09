# Research: Does the CLI-agent backend stream output like the API-provider backend?

> **Short answer: Partial.** On the only path that streams on either backend —
> the batch-plan "Analyzing changes" reasoning feed — the CLI-agent backend
> **does** stream live, line-by-line, into the exact same `on_reasoning` UI
> callback the API-provider backend uses (so the user-visible spinner/reasoning
> window is identical). On the commit-message and conflict-resolve paths,
> **neither** backend streams — both buffer the full response before returning.
> The CLI backend is never a "spawn, `wait_with_output`, then parse" design:
> child stdout/stderr are always read incrementally by two owned reader tasks,
> and the only question is whether the per-line callback is wired to the UI
> (`stream_typed_with_reasoning`) or a no-op (`call`, `schema`).

---

## How each backend works

### API-provider backend (`Backend::Rig` / `LLMAgent`)

Routes over HTTP through `rig-core`'s streaming prompt API.

- `LLMAgent::stream_once_with_reasoning` calls `agent.stream_prompt(prompt).await`
  and pulls `StreamedAssistantContent` items off a `Stream`:
  `Text` deltas accumulate into the answer, `ReasoningDelta`/`Reasoning` are
  forwarded to `on_reasoning` as they arrive (`src/llm.rs:651-680`).
- `LLMAgent::stream_typed_with_reasoning::<T>` wraps that in a retry loop,
  tolerant-parsing the accumulated text (`src/llm.rs:703-742`).
- `LLMAgent::call` (conflict-resolve path) and `LLMAgent::schema` (commit-message
  path) do **not** stream — they use `prompt_once` / `prompt_typed` (single
  non-streaming await) under a retry wrapper (`src/llm.rs:610-632`, `744-762`).

Streaming = HTTP token/chunk deltas from the provider, with reasoning tokens
surfaced live.

### CLI-agent backend (`Backend::Cli` / `CliAgent`)

Shells out to an external coding-agent CLI in headless/print mode
(`claude -p`, `codex exec -s read-only …`, `pi --no-tools -p …`).

- `TokioRunner::run` spawns the child with `stdout`/`stderr` piped
  (`Stdio::piped()`) and `kill_on_drop(true)` (`src/cli_agent.rs:236-241`).
- Two owned tokio tasks each own one pipe and forward every **complete line**
  to an unbounded channel (`src/cli_agent.rs:269-283`):
  ```rust
  tokio::spawn(async move {
      let mut lines = BufReader::new(stdout).lines();
      while let Ok(Some(line)) = lines.next_line().await {
          if out_tx.send((0, line)).is_err() { break; }
      }
  });
  ```
  The module doc explicitly notes this design replaced an earlier
  `wait_with_output`-based buffer-and-block design (see test comment,
  `src/cli_agent.rs:770-773`):
  > "the regression for the old 'no response' UX: the runner used to buffer
  > everything via `wait_with_output` and kill at a hard wall-clock deadline."
- The main loop reads the channel, calls `on_output(&line)` **per line as it
  arrives**, and accumulates the same line into `stdout_acc`/`stderr_acc`
  (`src/cli_agent.rs:295-313`). After both readers EOF, it reaps the child and
  returns the accumulated buffer (`src/cli_agent.rs:315-325`).
- The timeout is an **idle** budget, reset on every received line, not a
  wall-clock cap (`src/cli_agent.rs:296-300`, ADR 0010 §"Streaming/reasoning").
  An actively-streaming CLI is never killed mid-thought.

Which `CliAgent` method is used determines whether `on_output` reaches the UI:

| Method | `on_output` callback | Path | Streams to UI? |
|---|---|---|---|
| `stream_typed_with_reasoning::<T>` (`src/cli_agent.rs:491-507`) | real `on_reasoning` from caller | batch-plan | **Yes** — line-by-line |
| `schema::<T>` (`src/cli_agent.rs:476-482`) | `let mut noop = \|_: &str\| {};` | commit-message | No (buffered) |
| `call` (`src/cli_agent.rs:435-438`) | `let mut noop = \|_: &str\| {};` | conflict-resolve | No (buffered) |
| `verify` (`src/cli_agent.rs:519-524`) | noop | `aic setup` probe | No (buffered) |

Note: even on the "buffered" CLI paths, stdout is still read incrementally by
the reader tasks (so a long-running CLI doesn't fill an OS pipe buffer and
deadlock); the lines are simply forwarded to a no-op callback and only the
final accumulated string is returned. There is no `wait_with_output()` call
anywhere in the current CLI runner.

### Backend dispatch (single call-site shape)

`Backend` is an enum, not a `Box<dyn>` (so the generic `schema<T>` /
`stream_typed_with_reasoning<T>` stay monomorphized per backend).
`Backend::stream_typed_with_reasoning` dispatches the same `on_reasoning`
callback to whichever arm is active (`src/llm.rs:796-819`):

```rust
pub async fn stream_typed_with_reasoning<T>(...) -> Result<T> {
    match self {
        Self::Rig(a) => a.stream_typed_with_reasoning::<T>(prompt, on_reasoning).await,
        Self::Cli(a) => a.stream_typed_with_reasoning::<T>(prompt, on_reasoning).await,
    }
}
```

The UI consumer is `analyze_changes` in `src/main.rs:73-86` — it calls
`generator::Generator::split_patch_streaming` and feeds each delta into
`progress::ReasoningRenderer::paint`. This is the **same renderer** for both
backends, so the visible "Analyzing changes" reasoning window behaves
identically whether the backend is `Rig` or `Cli`.

---

## Evidence

### 1. CLI runner reads child stdout/stderr line-by-line and forwards live

`src/cli_agent.rs:217-325` — `TokioRunner::run`:

```rust
// L236-241: piped stdio + kill_on_drop
cmd.args(&spec.args)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
// L269-283: two owned reader tasks forward every complete line
tokio::spawn(async move {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if out_tx.send((0, line)).is_err() { break; }
    }
});
// L296-313: per-line callback + idle-timeout reset
match tokio::time::timeout(timeout, rx.recv()).await {
    Err(_) => return Err(anyhow::Error::new(LlmError::Timeout(secs))),
    Ok(None) => break,
    Ok(Some((stream, line))) => {
        on_output(&line);            // <-- live, per line
        if stream == 0 { stdout_acc.push_str(&line); stdout_acc.push('\n'); }
        else          { stderr_acc.push_str(&line); stderr_acc.push('\n'); }
    }
}
```

### 2. CLI batch-plan path wires the real reasoning callback

`src/cli_agent.rs:491-507` — `CliAgent::stream_typed_with_reasoning`:

```rust
pub async fn stream_typed_with_reasoning<T>(
    &self,
    user_prompt: &str,
    mut on_reasoning: impl FnMut(&str) + Send,
) -> Result<T> { ... }
```

Delegates to `typed_internal`, which forwards `on_output` to the runner
(`src/cli_agent.rs:458`), which is `TokioRunner::run`'s `on_output` parameter
— i.e. each stdout/stderr line becomes a `on_reasoning` call.

### 3. CLI commit-message & conflict-resolve paths use a no-op callback

`src/cli_agent.rs:435-438`:
```rust
pub async fn call(&self, user_prompt: &str) -> Result<String> {
    let mut noop = |_: &str| {};
    self.run_once(user_prompt, Mode::Text, &mut noop).await
}
```
`src/cli_agent.rs:476-482` — `schema` does the same with `Mode::Json`.

So those CLI paths collect the full stdout before returning, but still via
incremental reads (no `wait_with_output`).

### 4. API path mirrors the exact split

- Streaming reasoning path: `LLMAgent::stream_typed_with_reasoning`,
  `src/llm.rs:703-742`, pulling `StreamedAssistantContent` off
  `agent.stream_prompt(..)` in `stream_once_with_reasoning`
  (`src/llm.rs:651-680`).
- Buffered paths: `LLMAgent::call` (`src/llm.rs:610-632`, uses `prompt_once`)
  and `LLMAgent::schema` (`src/llm.rs:744-762`, uses `prompt_typed_once`).
  Neither invokes `on_reasoning`.

### 5. UI consumes both backends through one renderer

`src/main.rs:73-86`:
```rust
async fn analyze_changes(diff: &str) -> anyhow::Result<generator::BatchPlanOutput> {
    let mut view = progress::ThinkingView::new();
    let mut renderer = progress::ReasoningRenderer::new("Analyzing changes");
    let result = generator::Generator::split_patch_streaming(diff, |delta| {
        let window = view.push(delta);
        renderer.paint(&window);
    }).await;
    renderer.finish();
    result
}
```
`generator::split_patch_streaming` → `Backend::stream_typed_with_reasoning`
(`src/generator.rs:96-107`), which dispatches to whichever backend is active.

### 6. Docs confirm intent

- `CONTEXT.md` — **CLI-agent Backend** vocab: "shells out to an external
  coding-agent CLI … in headless/print mode and reuses that CLI's own auth."
- `src/cli_agent.rs:27-31` (module doc, "Streams live"):
  > "the CLI's stdout/stderr are forwarded line-by-line to `on_reasoning` as
  > they arrive (the model's live 'thinking process'), mirroring the API path.
  > The timeout is an **idle** budget (reset on every line), so an
  > actively-streaming CLI is never killed mid-thought."
- `docs/adr/0010-cli-agent-backend.md:121-127`:
  > "Streaming/reasoning: the CLI's stdout/stderr are streamed **live**,
  > line-by-line, into `on_reasoning` as they arrive … This mirrors the API
  > path's reasoning stream so the 'Analyzing changes' window shows the model's
  > thinking process under the CLI backend too — the prior 'print mode is
  > single-shot, so `on_reasoning` never fires' design left the UI silent for
  > the CLI's whole run."
- `README.md:38` advertises live reasoning generically: "aic streams the
  model's thinking as it decides the split."

---

## Notes / caveats

- **Per-path, not per-backend, is the real distinction.** Both backends stream
  on the batch-plan path (`stream_typed_with_reasoning`) and buffer on the
  commit-message (`schema`) and conflict-resolve (`call`) paths. The CLI
  backend is not categorically less streamed than the API backend — they are
  wired symmetrically through the `Backend` enum.
- **"Line-level" vs "token-level".** The CLI relay granularity is one complete
  stdout line per `on_reasoning` call (the reader tasks use
  `BufReader::lines()` / `next_line`). The API path's granularity is finer —
  one `rig` stream item (reasoning delta or text delta) per call. So a CLI
  that flushes partial lines or single tokens will be coalesced to line
  boundaries by aic; a CLI that only emits full lines already matches its own
  natural granularity. This is option (c) in the question's framing — the CLI
  agent's own stdout stream is relayed live, not transformed into HTTP-style
  token deltas.
- **The earlier `wait_with_output` design is explicitly gone.** The current
  `TokioRunner` never buffers-then-blocks; this is called out as a regression
  fix in `src/cli_agent.rs:770-773` and is exercised by the
  `streams_lines_and_survives_frequent_output` integration test
  (`src/cli_agent.rs:775-789`, Unix-only, real `sh -c` subprocess).
- **CLI-agent-dependent behavior.** What actually appears in the reasoning
  window depends on what the driven CLI prints to stdout/stderr while thinking.
  Claude Code in `-p` print mode, `codex exec`, and `pi --no-tools -p` each
  have their own output behavior; a CLI that prints nothing until the final
  answer will produce an empty reasoning feed (the spinner still shows, but no
  thinking text flows). A CLI that buffers all output internally until exit
  cannot be made to stream by aic — aic can only relay what the CLI flushes.
- **Idle timeout, not wall-clock.** `DEFAULT_TIMEOUT_SECS = 240`
  (`src/cli_agent.rs:36`) is per-line idle, so an actively-printing CLI runs
  unbounded; only a fully silent one for the full budget surfaces
  `LlmError::Timeout` (`src/cli_agent.rs:296-300`). The API path uses its own
  HTTP-level timeouts and is unaffected.
- **No `docs/research/` subdir existed** — `docs/` contains only `adr/` and
  `agents/`, so this file is placed at the requested `docs/research-cli-agent-streaming.md`.
