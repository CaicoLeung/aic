//! Run timeline e2e (ADR 0018): the non-TTY ledger each flow appends through
//! the Run's Display as its steps complete (ADR 0018), plus the failed-screen
//! shape (ADR 0018). Every test here drives the real `commit_run` against a
//! scratch git repo with stub LLM seams — the same harness as the other e2e
//! modules, asserting on the captured buffer.

use super::common::*;

/// Whether any ledger line starts with `prefix`.
fn has_step(lines: &[String], prefix: &str) -> bool {
    lines.iter().any(|l| l.starts_with(prefix))
}

/// The unstaged path's happy-path ledger: one `✓` line per step in skeleton
/// order — Scan, Redaction Gate, Plan, Draft, Commit — and nothing else from
/// the timeline (the Commit entries print separately as panel rows).
/// `starts_with` because a slow machine can push a step over the 1 s
/// duration threshold, appending `· Xs`.
#[tokio::test]
async fn unstaged_run_appends_the_step_ledger() {
    let (_dir, git) = with_git(two_file_unstaged_repo);

    let plan = two_file_two_batch_plan();
    let buf = BufferWrite::default();

    let result = commit_run(
        &git,
        RunDeps {
            display: Display::with(buf.clone()),
            ..deps(planner_fixed(plan), messenger_fixed("feat: stub message"))
        },
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    let lines = buf.lines();
    assert!(has_step(&lines, "  ✓ Scanned 2 files"), "{lines:?}");
    assert!(has_step(&lines, "  ✓ Redaction gate · clean"), "{lines:?}");
    assert!(has_step(&lines, "  ✓ Planned 2 batches"), "{lines:?}");
    assert!(has_step(&lines, "  ✓ Drafted 2 messages"), "{lines:?}");
    assert!(has_step(&lines, "  ✓ Committed 2 batches"), "{lines:?}");
    assert!(
        lines.iter().all(|l| !l.contains('✗')),
        "the happy path never prints a failed screen: {lines:?}"
    );
}

/// The staged path's ledger: Scan, (gate), Draft message, Commit — no plan
/// or review steps, singular counts.
#[tokio::test]
async fn staged_run_appends_the_three_step_ledger() {
    let (dir, git) = with_git(gh::init_test_repo);
    stage_file(dir.path(), "tracked.txt", "diverged\n");

    let buf = BufferWrite::default();

    let result = commit_run(
        &git,
        RunDeps {
            display: Display::with(buf.clone()),
            ..deps(unreachable_planner(), messenger_fixed("feat: staged"))
        },
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    let lines = buf.lines();
    assert!(has_step(&lines, "  ✓ Scanned 1 file"), "{lines:?}");
    assert!(has_step(&lines, "  ✓ Redaction gate · clean"), "{lines:?}");
    assert!(has_step(&lines, "  ✓ Drafted message"), "{lines:?}");
    assert!(has_step(&lines, "  ✓ Committed 1 batch"), "{lines:?}");
    assert!(
        lines.iter().all(|l| !l.contains('✗')),
        "the happy path never prints a failed screen: {lines:?}"
    );
}

/// `--no-redact` (`Redact::Off`): the gate step is absent from the skeleton
/// and the ledger — no "Redaction gate" line at all.
#[tokio::test]
async fn no_redact_run_has_no_gate_step() {
    let (dir, git) = with_git(gh::init_test_repo);
    stage_file(dir.path(), "tracked.txt", "diverged\n");

    let buf = BufferWrite::default();

    let result = commit_run(
        &git,
        RunDeps {
            redact: Redact::Off,
            display: Display::with(buf.clone()),
            ..deps(unreachable_planner(), messenger_fixed("feat: staged"))
        },
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    let lines = buf.lines();
    assert!(
        lines.iter().all(|l| !l.contains("Redaction gate")),
        "the gate step must not appear: {lines:?}"
    );
    assert!(lines.iter().any(|l| l.starts_with("  ✓ Scanned 1 file")));
}

/// A redaction refusal is the gate step's failed screen: the `✗` headline
/// names the gate, and the detail rows carry the full refusal (files +
/// patterns). Nothing erases, and the error still propagates.
#[tokio::test]
async fn redaction_refusal_shapes_the_failed_screen() {
    let (dir, git) = with_git(gh::init_test_repo);
    std::fs::write(
        dir.path().join("secret.txt"),
        "-----BEGIN RSA PRIVATE KEY-----\nabc\n",
    )
    .unwrap();

    let buf = BufferWrite::default();

    let err = commit_run(
        &git,
        RunDeps {
            display: Display::with(buf.clone()),
            ..deps(unreachable_planner(), unreachable_messenger())
        },
    )
    .await
    .expect_err("the gate must refuse the secret");
    let lines = buf.lines();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("  ✗ Redaction gate — secret-like content detected")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("secret.txt") && l.contains("private key")),
        "the refusal detail must name the file and pattern: {lines:?}"
    );
    err_contains(&err, &["secret-shaped"]);
}

/// Declining a batch's confirmation fails the Commit step's screen: the
/// headline names the batch, and the remediation rows carry the recovery
/// contract (still staged; re-run `aic`). Later steps are absent.
#[tokio::test]
async fn declined_batch_shapes_the_failed_screen() {
    let (_dir, git) = with_git(two_file_unstaged_repo);

    let plan = two_file_two_batch_plan();
    let buf = BufferWrite::default();

    let err = commit_run(
        &git,
        RunDeps {
            redact: Redact::Off,
            display: Display::with(buf.clone()),
            confirm: Confirm::Interactive {
                menu: menu_queue(vec![ConfirmChoice::Commit, ConfirmChoice::Abort]),
                editor: editor_fixed("edited", None),
            },
            ..deps(planner_fixed(plan), messenger_fixed("feat: stub message"))
        },
    )
    .await
    .expect_err("the decline must end the Run");
    err_contains(&err, &["declined on batch 2 of 2"]);
    let lines = buf.lines();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("  ✗ Commit batches · 2/2 — declined")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("the remaining changes are still staged")),
        "the remediation row must survive: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("re-run `aic` to continue")),
        "{lines:?}"
    );
    // The failed screen is the last thing the ledger says — the frame stops
    // at the failed step.
    let last = lines.last().unwrap();
    assert!(
        last.contains("re-run `aic`") || last.contains("staged"),
        "nothing may follow the failed screen: {lines:?}"
    );
}

/// A failed commit (hook veto) failed screen: the ✗ headline names the
/// batch and the remediation rows carry the recovery contract, but the
/// error chain is NOT re-rendered in the frame — it prints once,
/// untruncated, in main's boundary report after the frame ends, so no
/// width-capped duplicate row ships alongside it. The propagated error
/// still carries the full chain (git's own stderr) — that text is what the
/// boundary report prints.
#[tokio::test]
async fn commit_failure_screen_carries_remediation_only() {
    let (dir, git) = with_git(gh::init_test_repo);
    gh::install_hook(
        dir.path(),
        "pre-commit",
        "echo 'blocked by policy' >&2; exit 1",
    );
    std::fs::write(dir.path().join("tracked.txt"), "changed by hook test\n").unwrap();
    let plan = plan_single_batch("tracked.txt", "hook veto");
    let buf = BufferWrite::default();

    let err = commit_run(
        &git,
        RunDeps {
            display: Display::with(buf.clone()),
            ..deps(planner_fixed(plan), messenger_fixed("chore: vetoed"))
        },
    )
    .await
    .expect_err("a vetoing hook must abort the Run");

    err_contains(&err, &["blocked by policy", "aborted on batch 1 of 1"]);
    let lines = buf.lines();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("  ✗ Commit batches · 1/1 — failed on batch 1 of 1")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("the remaining changes are still staged")),
        "the remediation row must survive: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("re-run `aic` to continue")),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("git commit -F")),
        "the error chain must not render in the frame: {lines:?}"
    );
}

/// The Batch Review flow completes its step with the batch count — plus the
/// merge count when merges happened.
#[tokio::test]
async fn review_flow_completes_the_plan_step() {
    let (_dir, git) = with_git(two_file_unstaged_repo);

    let plan = two_file_two_batch_plan();
    let buf = BufferWrite::default();

    let result = commit_run(
        &git,
        RunDeps {
            review: BatchReview::Interactive {
                menu: review_queue(vec![ReviewChoice::Merge(0, 1), ReviewChoice::Continue]),
            },
            redact: Redact::Off,
            display: Display::with(buf.clone()),
            ..deps(planner_fixed(plan), messenger_fixed("feat: stub message"))
        },
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    let lines = buf.lines();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("  ✓ Plan approved · 1 batch · 1 merged")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("  ✓ Committed 1 batch")),
        "the merged plan commits as one batch: {lines:?}"
    );
}

/// Review abort is the failed screen: `✗ Review plan — aborted by user` plus
/// the nothing-staged hint, and the Run bails with the same clean wording.
#[tokio::test]
async fn review_abort_shapes_the_failed_screen() {
    let (_dir, git) = with_git(two_file_unstaged_repo);
    let plan = plan_single_batch("alpha.txt", "change alpha");

    let buf = BufferWrite::default();

    let err = commit_run(
        &git,
        RunDeps {
            review: BatchReview::Interactive {
                menu: review_queue(vec![ReviewChoice::Abort]),
            },
            redact: Redact::Off,
            display: Display::with(buf.clone()),
            ..deps(planner_fixed(plan), unreachable_messenger())
        },
    )
    .await
    .expect_err("abort must end the Run");
    assert!(format!("{err:#}").contains("aborted — no commit made"));
    let lines = buf.lines();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("  ✗ Review plan — aborted by user")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("nothing staged, nothing committed")),
        "{lines:?}"
    );
}

/// The deterministic-fallback plan completes the Plan step with the same
/// "Planned" verb as the happy path (the ⚠ caution row tells the fallback
/// story) and the durable caution row rides under it.
#[tokio::test]
async fn invalid_plan_completes_with_planned_verb_and_caution_row() {
    let (_dir, git) = with_git(two_file_unstaged_repo);
    // An empty plan is invalid (real work exists) → deterministic regrouping.
    let plan = plan(vec![]);

    let buf = BufferWrite::default();

    let result = commit_run(
        &git,
        RunDeps {
            redact: Redact::Off,
            display: Display::with(buf.clone()),
            ..deps(planner_fixed(plan), messenger_fixed("feat: stub message"))
        },
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    let lines = buf.lines();
    assert!(
        lines.iter().any(|l| l.starts_with("  ✓ Planned 2 batches")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("Regrouping deterministically")),
        "{lines:?}"
    );
}
