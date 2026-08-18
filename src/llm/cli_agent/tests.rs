use super::*;
use std::sync::Mutex;

/// Scripted runner: returns canned outputs in order, one per call.
struct FakeRunner {
    outputs: Mutex<Vec<Result<CommandOutput>>>,
    seen: Mutex<Vec<CommandSpec>>,
}

impl FakeRunner {
    fn new(outputs: Vec<Result<CommandOutput>>) -> Self {
        Self {
            outputs: Mutex::new(outputs),
            seen: Mutex::new(Vec::new()),
        }
    }
    fn calls(&self) -> Vec<CommandSpec> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl CommandRunner for FakeRunner {
    async fn run(
        &self,
        spec: &CommandSpec,
        _timeout: Duration,
        _on_output: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<CommandOutput> {
        self.seen.lock().unwrap().push(spec.clone());
        let mut queue = self.outputs.lock().unwrap();
        queue.pop().expect("FakeRunner ran out of canned outputs")
    }
}

fn spec(args: Vec<String>) -> CliSpec {
    CliSpec {
        command: "claude".into(),
        args,
        timeout_secs: DEFAULT_TIMEOUT_SECS,
        encoding: Encoding::Plain,
    }
}

fn ok(stdout: &str) -> Result<CommandOutput> {
    Ok(CommandOutput {
        success: true,
        code: Some(0),
        stdout: stdout.into(),
        stderr: String::new(),
    })
}

fn fail(stderr: &str, code: Option<i32>) -> Result<CommandOutput> {
    Ok(CommandOutput {
        success: false,
        code,
        stdout: String::new(),
        stderr: stderr.into(),
    })
}

fn agent_with(outputs: Vec<Result<CommandOutput>>) -> (CliAgent, Arc<FakeRunner>) {
    let runner = Arc::new(FakeRunner::new(outputs));
    let agent = CliAgent::with_runner(
        spec(vec!["-p".into(), PROMPT_PLACEHOLDER.into()]),
        "You write commit messages.".into(),
        runner.clone(),
    );
    (agent, runner)
}

#[test]
fn preset_migration_rewrites_legacy_claude_to_stream_json() {
    // The exact stale shape from the regression: plain `-p {prompt}`.
    let (name, new_args) = cli_preset_migration("claude", &["-p".into(), "{prompt}".into()])
        .expect("legacy claude fingerprint must migrate");
    assert_eq!(name, "claude");
    assert!(new_args.iter().any(|a| a == "stream-json"));
    assert!(new_args.iter().any(|a| a == "--include-partial-messages"));
}

#[test]
fn preset_migration_rewrites_legacy_pi_to_mode_json() {
    // Stale pi: plain `--no-tools -p {prompt}` (pre-`--mode json` streaming).
    let (name, new_args) =
        cli_preset_migration("pi", &["--no-tools".into(), "-p".into(), "{prompt}".into()])
            .expect("legacy pi fingerprint must migrate");
    assert_eq!(name, "pi");
    assert!(
        new_args
            .windows(2)
            .any(|w| w[0] == "--mode" && w[1] == "json")
    );
}

#[test]
fn preset_migration_is_noop_for_current_preset_and_custom() {
    // A config already on the current claude preset matches no legacy
    // fingerprint → None (idempotent: re-running migration is a no-op).
    let current = cli_preset("claude").unwrap();
    assert_eq!(
        cli_preset_migration(&current.command, &current.args),
        None,
        "current preset must not re-migrate"
    );
    // A custom command (even one close to a preset, with an extra flag) is
    // left alone — exact-match only.
    assert_eq!(
        cli_preset_migration(
            "claude",
            &["-p".into(), "{prompt}".into(), "--model".into(), "x".into()]
        ),
        None,
        "customized args must never be silently rewritten"
    );
    // A wholly custom command is untouched too.
    assert_eq!(cli_preset_migration("my-agent", &["run".into()]), None);
}

#[test]
fn preset_migration_rejects_wrong_command_for_legacy_args() {
    // Same args as legacy claude, but a different command — not a match.
    assert_eq!(
        cli_preset_migration("codex", &["-p".into(), "{prompt}".into()]),
        None
    );
}

#[test]
fn presets_use_print_mode_and_known_programs() {
    let c = cli_preset("claude").unwrap();
    assert_eq!(c.command, "claude");
    // stream-json + partial messages: the only preset that surfaces a
    // reasoning feed (plain `-p` returns only the final answer).
    assert_eq!(
        c.args,
        vec![
            "-p",
            PROMPT_PLACEHOLDER,
            "--output-format",
            "stream-json",
            "--include-partial-messages"
        ]
    );
    assert_eq!(c.encoding, Encoding::ClaudeStreamJson);
    // Streaming CLI: streaming-sized idle budget.
    assert_eq!(c.timeout_secs, DEFAULT_TIMEOUT_SECS);
    let codex = cli_preset("codex").unwrap();
    assert_eq!(codex.command, "codex");
    // exec pinned to a read-only sandbox; --json switches stdout to the
    // JSONL event stream decoded centrally by Encoding::CodexJson.
    assert_eq!(
        codex.args,
        vec!["exec", "--json", "-s", "read-only", PROMPT_PLACEHOLDER]
    );
    assert_eq!(codex.encoding, Encoding::CodexJson);
    // Batch CLI (silent during reasoning): larger idle budget so a long
    // reasoning run is not killed.
    assert_eq!(codex.timeout_secs, BATCH_TIMEOUT_SECS);
    let pi = cli_preset("pi").unwrap();
    assert_eq!(pi.command, "pi");
    // --no-tools disables all tools so print mode is text-only.
    assert!(pi.args.iter().any(|a| a == "--no-tools"));
    assert!(pi.args.iter().any(|a| a == "-p"));
    assert_eq!(pi.encoding, Encoding::PiStreamJson);
    assert_eq!(pi.timeout_secs, DEFAULT_TIMEOUT_SECS);
    let oc = cli_preset("opencode").unwrap();
    assert_eq!(oc.command, "opencode");
    // run --format json: NDJSON events for clean answer extraction.
    assert!(
        oc.args
            .windows(2)
            .any(|w| w[0] == "--format" && w[1] == "json")
    );
    assert_eq!(oc.encoding, Encoding::OpenCodeJson);
    // opencode is batch too (answer arrives whole at completion).
    assert_eq!(oc.timeout_secs, BATCH_TIMEOUT_SECS);
    assert!(cli_preset("nope").is_none());
    assert_eq!(
        PRESETS,
        &[
            "claude", "codex", "pi", "opencode", "omp", "gemini", "cursor", "windsurf", "copilot",
            "trae", "qwen"
        ]
    );
}

#[test]
fn new_presets_are_headless_print_mode() {
    // The seven added presets: every one is a single-shot `-p`-style print
    // mode against its own binary. omp reuses pi's NDJSON envelope (it is a
    // pi fork) so it gets the live reasoning feed; the rest are Plain print
    // mode (no decoder needed — stdout IS the answer).
    let cases = [
        (
            "omp",
            "omp",
            vec!["--mode", "json", PROMPT_PLACEHOLDER],
            Encoding::PiStreamJson,
            DEFAULT_TIMEOUT_SECS,
        ),
        (
            "gemini",
            "gemini",
            vec!["-p", PROMPT_PLACEHOLDER],
            Encoding::Plain,
            DEFAULT_TIMEOUT_SECS,
        ),
        (
            "cursor",
            "cursor-agent",
            vec!["-p", PROMPT_PLACEHOLDER],
            Encoding::Plain,
            DEFAULT_TIMEOUT_SECS,
        ),
        (
            "windsurf",
            "devin",
            vec!["-p", PROMPT_PLACEHOLDER],
            Encoding::Plain,
            DEFAULT_TIMEOUT_SECS,
        ),
        (
            "copilot",
            "copilot",
            vec!["-p", PROMPT_PLACEHOLDER],
            Encoding::Plain,
            DEFAULT_TIMEOUT_SECS,
        ),
        (
            "trae",
            "traecli",
            vec!["-p", PROMPT_PLACEHOLDER],
            Encoding::Plain,
            DEFAULT_TIMEOUT_SECS,
        ),
        (
            "qwen",
            "qwen",
            vec!["-p", PROMPT_PLACEHOLDER],
            Encoding::Plain,
            DEFAULT_TIMEOUT_SECS,
        ),
    ];
    for (name, command, args, encoding, timeout) in cases {
        let spec = cli_preset(name).unwrap_or_else(|| panic!("{name} preset missing"));
        assert_eq!(spec.command, command, "{name}");
        assert_eq!(spec.args, args, "{name}");
        assert_eq!(spec.encoding, encoding, "{name}");
        assert_eq!(spec.timeout_secs, timeout, "{name}");
    }
    // The `gemini` preset shadows the provider name in `aic use` (presets
    // win), so the Google API stays reachable via its alias.
    assert!(is_preset("gemini"));
    assert!(is_preset("OMP"));
    assert!(is_preset("Qwen"));
}

#[test]
fn streams_reasoning_live_only_for_token_streamers() {
    // The cold-start-notice policy lives here, on the envelope: only the
    // two live-token-streamers (claude `thinking_delta`, pi
    // `thinking_delta`) expect a reasoning feed whose pre-first-delta wait
    // is a cold start. opencode/codex arrive whole at completion → false
    // (no live stream to cold-start into); plain never streams.
    assert!(Encoding::ClaudeStreamJson.streams_reasoning_live());
    assert!(Encoding::PiStreamJson.streams_reasoning_live());
    assert!(!Encoding::OpenCodeJson.streams_reasoning_live());
    assert!(!Encoding::CodexJson.streams_reasoning_live());
    assert!(!Encoding::Plain.streams_reasoning_live());
}

#[tokio::test]
async fn call_returns_raw_text() {
    // `call` mirrors LLMAgent::call and returns raw assistant text; the
    // resolve workflow strips the fence itself.
    let (agent, _) = agent_with(vec![ok("```\nresolved file body\n```")]);
    let out = agent.call("conflicted file").await.unwrap();
    assert_eq!(out, "```\nresolved file body\n```");
}

#[tokio::test]
async fn schema_parses_json_and_substitutes_prompt() {
    let (agent, runner) = agent_with(vec![ok(r#"{"message":"feat: x","body":null}"#)]);
    let v: serde_json::Value = agent.schema("the diff").await.unwrap();
    assert_eq!(v["message"], "feat: x");

    // The single argv element carries the full prompt with the boundary.
    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].program, "claude");
    assert_eq!(calls[0].args.len(), 2);
    assert_eq!(calls[0].args[0], "-p");
    let prompt = &calls[0].args[1];
    assert!(prompt.contains("<aic_input>"));
    assert!(prompt.contains("the diff"));
    assert!(prompt.contains("</aic_input>"));
    assert!(prompt.contains("You write commit messages."));
    // JSON reminder present on the typed path.
    assert!(prompt.contains("ONLY the JSON"));
}

#[tokio::test]
async fn schema_retries_once_on_bad_json_then_succeeds() {
    // Queue is LIFO in the fake: push failure first, then success.
    let (agent, runner) = agent_with(vec![
        ok(r#"{"message":"feat: recovered","body":null}"#),
        ok("not json at all"),
    ]);
    let v: serde_json::Value = agent.schema("diff").await.unwrap();
    assert_eq!(v["message"], "feat: recovered");
    assert_eq!(runner.calls().len(), 2, "one retry → 2 total attempts");
}

#[tokio::test]
async fn schema_gives_up_after_one_retry() {
    let (agent, runner) = agent_with(vec![ok("still not json"), ok("not json either")]);
    let res: Result<serde_json::Value> = agent.schema("diff").await;
    assert!(res.is_err());
    assert_eq!(runner.calls().len(), 2);
}

#[tokio::test]
async fn not_installed_surfaces_immediately_no_retry() {
    // A missing binary surfaces as CliNotInstalled directly from the
    // runner; run_once propagates it before any parse retry.
    let (agent, runner) = agent_with(vec![Err(anyhow::Error::new(LlmError::CliNotInstalled(
        "claude".into(),
    )))]);
    let res: Result<serde_json::Value> = agent.schema("diff").await;
    assert!(res.is_err());
    assert_eq!(runner.calls().len(), 1, "infra errors are never retried");
    assert!(matches!(
        res.unwrap_err().downcast_ref::<LlmError>(),
        Some(LlmError::CliNotInstalled(_))
    ));
}

#[tokio::test]
async fn signal_death_is_not_misreported_as_not_installed() {
    // A spawned agent killed by a signal (ExitStatus::code() == None) with
    // no output must surface as NonZeroExit, NOT CliNotInstalled — the
    // binary was found and ran. Distinct from the NotFound path above.
    let (agent, _) = agent_with(vec![Ok(CommandOutput {
        success: false,
        code: None,
        stdout: String::new(),
        stderr: String::new(),
    })]);
    let res = agent.call("x").await;
    match res.unwrap_err().downcast_ref::<LlmError>() {
        Some(LlmError::NonZeroExit { code, .. }) => assert_eq!(*code, None),
        other => panic!("expected NonZeroExit(code None) for signal death, got {other:?}"),
    }
}

#[tokio::test]
async fn auth_failure_is_classified() {
    let (agent, _) = agent_with(vec![fail(
        "Error: not logged in. Run `claude` to authenticate.",
        Some(1),
    )]);
    let res = agent.call("x").await;
    let err = res.unwrap_err();
    assert!(matches!(
        err.downcast_ref::<LlmError>(),
        Some(LlmError::CliNotAuthenticated(_))
    ));
}

#[tokio::test]
async fn non_zero_exit_carries_stderr() {
    let (agent, _) = agent_with(vec![fail("boom", Some(42))]);
    let res = agent.call("x").await;
    let err = res.unwrap_err();
    match err.downcast_ref::<LlmError>() {
        Some(LlmError::NonZeroExit { code, stderr, .. }) => {
            assert_eq!(*code, Some(42));
            assert!(stderr.contains("boom"));
        }
        other => panic!("expected NonZeroExit, got {other:?}"),
    }
}

#[tokio::test]
async fn timeout_is_classified() {
    // The runner now surfaces a timeout as a typed error directly (not as
    // a CommandOutput carrying a magic stderr string), so classification
    // cannot be fooled by a CLI that happens to print "timed out".
    let (agent, _) = agent_with(vec![Err(anyhow::Error::new(LlmError::Timeout(60)))]);
    let res = agent.call("x").await;
    assert!(matches!(
        res.unwrap_err().downcast_ref::<LlmError>(),
        Some(LlmError::Timeout(n)) if *n == 60
    ));
}

#[tokio::test]
async fn no_placeholder_appends_prompt_as_trailing_arg() {
    let runner = Arc::new(FakeRunner::new(vec![ok("OK")]));
    let agent = CliAgent::with_runner(
        CliSpec {
            command: "weirdcli".into(),
            args: vec!["--batch".into()], // no {prompt}
            timeout_secs: 10,
            encoding: Encoding::Plain,
        },
        "sys".into(),
        runner.clone(),
    );
    let out = agent.call("payload").await.unwrap();
    assert_eq!(out, "OK");
    let call = &runner.calls()[0];
    assert_eq!(call.program, "weirdcli");
    assert_eq!(call.args.len(), 2);
    assert_eq!(call.args[0], "--batch");
    assert!(call.args[1].contains("payload"));
}

// ---- streaming + idle timeout (real subprocess via TokioRunner) ----

/// A `CliAgent` wired to the production [`TokioRunner`] running `script`
/// through `sh -c`. Exercises the real spawn/stream/idle-timeout path that
/// the [`FakeRunner`] tests above cannot reach.
fn real_agent(script: &str, idle_timeout_secs: u64) -> CliAgent {
    CliAgent::new(
        CliSpec {
            command: "sh".into(),
            args: vec!["-c".into(), script.into()],
            timeout_secs: idle_timeout_secs,
            encoding: Encoding::Plain,
        },
        "sys".into(),
    )
}

/// A CLI that prints a line every 50ms (≈0.4s total) then the JSON answer
/// must NOT trip a 1s idle timeout, even though the whole run is long —
/// every inter-line gap (50ms) is well under it. Proves the timeout is
/// **idle** (reset per line), not wall-clock, and that each line streams
/// to `on_reasoning` live (the regression for the old "no response" UX:
/// the runner used to buffer everything via `wait_with_output` and kill at
/// a hard wall-clock deadline).
#[tokio::test]
#[cfg(unix)]
async fn streams_lines_and_survives_frequent_output() {
    let script = "for i in 1 2 3 4 5 6 7 8; do echo \"think $i\"; sleep 0.05; done; \
                  echo '{\"message\":\"feat: x\",\"body\":null}'";
    let agent = real_agent(script, 1);
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured = seen.clone();
    let v: serde_json::Value = agent
        .stream_typed_with_reasoning("diff", move |s| {
            captured.lock().unwrap().push(s.to_string());
        })
        .await
        .expect("frequent output must not trip the idle timeout");
    assert_eq!(v["message"], "feat: x");
    let lines = seen.lock().unwrap();
    assert!(
        lines.iter().any(|l| l.contains("think 1")),
        "first line streamed: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("think 8")),
        "last line streamed: {lines:?}"
    );
}

/// A CLI that emits one line then goes silent far longer than the idle
/// timeout must surface `LlmError::Timeout` — the timeout exists for a
/// stalled/no-response CLI. The initial line arrived (so this is NOT a
/// wall-clock-from-spawn deadline); the *subsequent* silence trips it.
#[tokio::test]
#[cfg(unix)]
async fn idle_timeout_fires_when_cli_goes_silent() {
    let script = "echo started; sleep 3; echo never";
    let agent = real_agent(script, 1);
    let res: Result<serde_json::Value> = agent.stream_typed_with_reasoning("diff", |_| {}).await;
    let err = res.unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<LlmError>(),
            Some(LlmError::Timeout(n)) if *n == 1
        ),
        "expected idle Timeout(1), got {err:#}"
    );
}
