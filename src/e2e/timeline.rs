//! Run timeline e2e (ADR 0018): the non-TTY ledger each flow appends through
//! the Run's Display as its steps complete (ADR 0018), plus the failed-screen
//! shape (ADR 0018). Every test here drives the real `commit_run` against a
//! scratch git repo with stub LLM seams — the same harness as the other e2e
//! modules, asserting on the captured buffer.

use super::common::*;

/// The unstaged path's happy-path ledger: one `✓` line per step in skeleton
/// order — Scan, Redaction Gate, Plan, Draft, Commit — and nothing else from
/// the timeline (the Commit entries print separately as panel rows).
/// `starts_with` because a slow machine can push a step over the 1 s
/// duration threshold, appending `· Xs`.
#[tokio::test]
async fn unstaged_run_appends_the_step_ledger() {
    let dir = with_repo(two_file_unstaged_repo);

    let plan = generator::BatchPlanOutput {
        batches: vec![
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "alpha.txt".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("change alpha".into()),
            },
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "beta.txt".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("change beta".into()),
            },
        ],
    };
    let buf = BufferWrite::default();
    let git = Git::at(dir.path()).unwrap();
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
    let step = |prefix: &str| lines.iter().any(|l| l.starts_with(prefix));
    assert!(step("  ✓ Scanned 2 files"), "{lines:?}");
    assert!(step("  ✓ Redaction gate · clean"), "{lines:?}");
    assert!(step("  ✓ Planned 2 batches"), "{lines:?}");
    assert!(step("  ✓ Drafted 2 messages"), "{lines:?}");
    assert!(step("  ✓ Committed 2 batches"), "{lines:?}");
    assert!(
        lines.iter().all(|l| !l.contains('✗')),
        "the happy path never prints a failed screen: {lines:?}"
    );
}

/// The staged path's ledger: Scan, (gate), Draft message, Commit — no plan
/// or review steps, singular counts.
#[tokio::test]
async fn staged_run_appends_the_three_step_ledger() {
    let dir = with_repo(gh::init_test_repo);
    stage_file(dir.path(), "tracked.txt", "diverged\n");

    let buf = BufferWrite::default();
    let git = Git::at(dir.path()).unwrap();
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
    let step = |prefix: &str| lines.iter().any(|l| l.starts_with(prefix));
    assert!(step("  ✓ Scanned 1 file"), "{lines:?}");
    assert!(step("  ✓ Redaction gate · clean"), "{lines:?}");
    assert!(step("  ✓ Drafted message"), "{lines:?}");
    assert!(step("  ✓ Committed 1 batch"), "{lines:?}");
    assert!(
        lines.iter().all(|l| !l.contains('✗')),
        "the happy path never prints a failed screen: {lines:?}"
    );
}

/// `--no-redact` (`Redact::Off`): the gate step is absent from the skeleton
/// and the ledger — no "Redaction gate" line at all.
#[tokio::test]
async fn no_redact_run_has_no_gate_step() {
    let dir = with_repo(gh::init_test_repo);
    stage_file(dir.path(), "tracked.txt", "diverged\n");

    let buf = BufferWrite::default();
    let git = Git::at(dir.path()).unwrap();
    let result = commit_run(
        &git,
        RunDeps {
            review: BatchReview::Disabled,
            redact: Redact::Off,
            display: Display::with(buf.clone()),
            planner: unreachable_planner(),
            messenger: messenger_fixed("feat: staged"),
            confirm: Confirm::Disabled,
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
    let dir = with_repo(gh::init_test_repo);
    std::fs::write(
        dir.path().join("secret.txt"),
        "-----BEGIN RSA PRIVATE KEY-----\nabc\n",
    )
    .unwrap();

    let buf = BufferWrite::default();
    let git = Git::at(dir.path()).unwrap();
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
    assert!(
        format!("{err:#}").contains("secret-shaped"),
        "the propagated error keeps its information: {err:#}"
    );
}

/// Declining a batch's confirmation fails the Commit step's screen: the
/// headline names the batch, and the remediation rows carry the recovery
/// contract (still staged; re-run `aic`). Later steps are absent.
#[tokio::test]
async fn declined_batch_shapes_the_failed_screen() {
    let dir = with_repo(two_file_unstaged_repo);

    let plan = generator::BatchPlanOutput {
        batches: vec![
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "alpha.txt".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("change alpha".into()),
            },
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "beta.txt".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("change beta".into()),
            },
        ],
    };
    let buf = BufferWrite::default();
    let git = Git::at(dir.path()).unwrap();
    let err = commit_run(
        &git,
        RunDeps {
            review: BatchReview::Disabled,
            redact: Redact::Off,
            display: Display::with(buf.clone()),
            planner: planner_fixed(plan),
            messenger: messenger_fixed("feat: stub message"),
            confirm: Confirm::Interactive {
                menu: menu_queue(vec![ConfirmChoice::Commit, ConfirmChoice::Abort]),
                editor: editor_fixed("edited", None),
            },
        },
    )
    .await
    .expect_err("the decline must end the Run");
    assert!(
        format!("{err:#}").contains("declined on batch 2 of 2"),
        "{err:#}"
    );
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

/// The Batch Review flow completes its step with the batch count — plus the
/// merge count when merges happened.
#[tokio::test]
async fn review_flow_completes_the_plan_step() {
    let dir = with_repo(two_file_unstaged_repo);

    let plan = generator::BatchPlanOutput {
        batches: vec![
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "alpha.txt".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("change alpha".into()),
            },
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "beta.txt".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("change beta".into()),
            },
        ],
    };
    let buf = BufferWrite::default();
    let git = Git::at(dir.path()).unwrap();
    let result = commit_run(
        &git,
        RunDeps {
            review: BatchReview::Interactive {
                menu: review_queue(vec![ReviewChoice::Merge(0, 1), ReviewChoice::Continue]),
            },
            redact: Redact::Off,
            display: Display::with(buf.clone()),
            planner: planner_fixed(plan),
            messenger: messenger_fixed("feat: stub message"),
            confirm: Confirm::Disabled,
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
    let dir = with_repo(two_file_unstaged_repo);
    let plan = plan_single_batch("alpha.txt", "change alpha");

    let buf = BufferWrite::default();
    let git = Git::at(dir.path()).unwrap();
    let err = commit_run(
        &git,
        RunDeps {
            review: BatchReview::Interactive {
                menu: review_queue(vec![ReviewChoice::Abort]),
            },
            redact: Redact::Off,
            display: Display::with(buf.clone()),
            planner: planner_fixed(plan),
            messenger: unreachable_messenger(),
            confirm: Confirm::Disabled,
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
    let dir = with_repo(two_file_unstaged_repo);
    // An empty plan is invalid (real work exists) → deterministic regrouping.
    let plan = generator::BatchPlanOutput { batches: vec![] };

    let buf = BufferWrite::default();
    let git = Git::at(dir.path()).unwrap();
    let result = commit_run(
        &git,
        RunDeps {
            review: BatchReview::Disabled,
            redact: Redact::Off,
            display: Display::with(buf.clone()),
            planner: planner_fixed(plan),
            messenger: messenger_fixed("feat: stub message"),
            confirm: Confirm::Disabled,
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
