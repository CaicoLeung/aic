use super::*;
use std::fs;

pub(crate) fn init_test_repo(dir: &Path) {
    let repo = Repository::init(dir).unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "test").unwrap();
    config.set_str("user.email", "test@test.com").unwrap();
    // Pin line-ending handling so test repos are deterministic across
    // platforms: Windows defaults to core.autocrlf=true, which rewrites
    // working-tree files to CRLF on checkout and breaks LF assertions
    // (e.g. e2e::resolve::resolve_finalizes_cherry_pick_sequence).
    config.set_str("core.autocrlf", "false").unwrap();

    std::fs::write(dir.join("tracked.txt"), "original\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("tracked.txt")).unwrap();
    index.write().unwrap();

    let tree_id = index.write_tree_to(&repo).unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = repo.signature().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .unwrap();
}

/// Core of the hunk-split feature: stage only some of a file's hunks via
/// `git apply --cached`, then confirm the index holds exactly those hunks.
#[test]
fn stage_hunks_stages_only_selected_hunk() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());

    // A base with two change sites far enough apart that git emits two
    // separate hunks (>= ~6 unchanged lines between them).
    let pad: String = (0..8).map(|i| format!("pad{i}\n")).collect();
    let base = format!("a0\n{pad}c0\n");
    let changed = format!("a1\n{pad}c1\n");
    std::fs::write(dir.path().join("tracked.txt"), &base).unwrap();
    {
        let git = Git::at(dir.path()).unwrap();
        git.add(&["tracked.txt"]).unwrap();
        git.commit("base".into(), None).unwrap();
    }
    std::fs::write(dir.path().join("tracked.txt"), &changed).unwrap();

    let git = Git::at(dir.path()).unwrap();
    let raw = git.diff_workdir(Some("tracked.txt")).unwrap();
    let patch = parse_file_patch(&raw);
    assert_eq!(patch.hunk_count(), 2, "expected two separate hunks");

    // Stage ONLY hunk 1; hunk 2 must stay unstaged.
    git.stage_hunks(&raw, &[1]).unwrap();
    let staged = git.diff(Some("tracked.txt")).unwrap();
    assert!(
        staged.contains("a1"),
        "staged index must include hunk 1 (a1)"
    );
    assert!(
        !staged.contains("c1"),
        "staged index must NOT include hunk 2 (c1)"
    );

    // Now stage hunk 2 as well and confirm both are present.
    git.stage_hunks(&raw, &[2]).unwrap();
    let staged = git.diff(Some("tracked.txt")).unwrap();
    assert!(staged.contains("a1") && staged.contains("c1"));
}

#[test]
fn run_git_delivers_stdin_and_captures_stdout() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());
    let git = Git::at(dir.path()).unwrap();

    // `git hash-object --stdin` hashes stdin and prints the object id to
    // stdout — proves stdin delivery and stdout capture in one shot. The
    // expected value is the independent blob hash of `hello\n`.
    let out = git
        .run_git(&["hash-object", "--stdin"], Some("hello\n"), &[])
        .unwrap();
    assert_eq!(out.trim(), "ce013625030ba8dba906f756967f9e9ca394464a");
}

#[test]
fn run_git_applies_env() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());
    let git = Git::at(dir.path()).unwrap();

    // `git var GIT_AUTHOR_IDENT` honors GIT_AUTHOR_NAME from the
    // environment, proving the env passthrough reaches the child.
    let out = git
        .run_git(
            &["var", "GIT_AUTHOR_IDENT"],
            None,
            &[("GIT_AUTHOR_NAME", "Ada")],
        )
        .unwrap();
    assert!(out.starts_with("Ada <"), "got: {}", out);
}

#[test]
fn run_git_surfaces_git_stderr_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());
    let git = Git::at(dir.path()).unwrap();

    // A well-formed patch for a file the index doesn't have: git refuses
    // with its own diagnostic. The error must carry that stderr plus the
    // exit status — never a bare exit code.
    let patch = "diff --git a/nope.txt b/nope.txt\n\
                 index 0000000..3b18e51 100644\n\
                 --- a/nope.txt\n\
                 +++ b/nope.txt\n\
                 @@ -0,0 +1 @@\n\
                 +hello\n";
    let err = git
        .run_git(&["apply", "--cached", "-"], Some(patch), &[])
        .expect_err("git apply must reject a patch for a missing file");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("does not exist in index"),
        "git's real stderr must be surfaced: {msg}"
    );
    // ExitStatus renders as "exit status: N" on Unix and "exit code: N"
    // on Windows (a std::os Display difference), so accept either — the
    // point is that the non-zero exit info is surfaced, never a bare
    // message.
    assert!(
        msg.contains("exit status") || msg.contains("exit code"),
        "exit status must be surfaced: {msg}"
    );
}

#[test]
fn diff_workdir_returns_untracked_content() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());

    std::fs::write(dir.path().join("new_file.txt"), "new content\n").unwrap();

    let git = Git::at(dir.path()).unwrap();
    let result = git.diff_workdir(Some("new_file.txt")).unwrap();
    assert!(
        !result.is_empty(),
        "should have diff content for untracked file"
    );
}

#[test]
fn diff_workdir_returns_modified_content() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());

    std::fs::write(dir.path().join("tracked.txt"), "modified\n").unwrap();

    let git = Git::at(dir.path()).unwrap();
    let result = git.diff_workdir(Some("tracked.txt")).unwrap();
    assert!(
        !result.is_empty(),
        "should have diff content for modified file"
    );
}

#[test]
fn diff_workdir_returns_deleted_content() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());

    std::fs::remove_file(dir.path().join("tracked.txt")).unwrap();

    let git = Git::at(dir.path()).unwrap();
    let result = git.diff_workdir(Some("tracked.txt")).unwrap();
    assert!(
        !result.is_empty(),
        "should have diff content for deleted file"
    );
}

/// Regression: `Git::add` must stage a working-tree deletion. Previously it
/// called `index.add_path`, which stats the file on disk and failed with
/// NotFound for deleted files — breaking the whole unstaged-deletion flow.
#[test]
fn add_stages_working_tree_deletion() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());

    std::fs::remove_file(dir.path().join("tracked.txt")).unwrap();

    let git = Git::at(dir.path()).unwrap();
    git.add(&["tracked.txt"])
        .expect("add should stage a deleted file");

    let repo = Repository::open(dir.path()).unwrap();
    let statuses = repo.statuses(None).unwrap();
    let entry = statuses
        .iter()
        .find(|s| s.path() == Ok("tracked.txt"))
        .unwrap();
    assert!(
        entry.status().contains(Status::INDEX_DELETED),
        "deletion should be staged in the index"
    );
}

/// `Git::add` must reject a pathspec that is neither on disk nor tracked,
/// matching `git add`'s "did not match any files" error. Without this, a
/// hallucinated path from the LLM batch plan would be silently dropped
/// and the commit would report success while missing a file.
#[test]
fn add_rejects_untracked_absent_path() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());

    let git = Git::at(dir.path()).unwrap();
    let err = git
        .add(&["does-not-exist.txt"])
        .expect_err("add should reject an absent, untracked path");
    assert!(
        format!("{err:#}").contains("did not match any tracked or working-tree file"),
        "error should explain the pathspec mismatch, got: {err:#}"
    );
}

/// Guard: `Git::diff` (tree-to-index) must return content for a staged
/// deletion so the commit-message LLM has something to describe. This is
/// load-bearing for the deletion flow — `generate_and_commit` calls
/// `Git::diff` after staging a removal.
#[test]
fn diff_returns_content_for_staged_deletion() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());

    {
        let repo = Repository::open(dir.path()).unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
    }

    let git = Git::at(dir.path()).unwrap();
    let result = git.diff(Some("tracked.txt")).unwrap();
    assert!(
        !result.is_empty(),
        "should have diff content for a staged deletion"
    );
}

#[test]
fn assert_commit_safe_blocks_mid_merge() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());
    crate::git::conflict::tests::make_content_conflict(dir.path());

    let git = Git::at(dir.path()).unwrap();
    let err = git.assert_commit_safe().expect_err("must abort mid-merge");
    assert!(
        format!("{err:#}").contains("mid-merge"),
        "expected mid-merge message, got: {err:#}"
    );
}

#[test]
fn assert_commit_safe_blocks_staged_markers() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());

    // Repo state is Clean, but a staged file carries leftover markers.
    std::fs::write(
        dir.path().join("tracked.txt"),
        "<<<<<<< HEAD\nmine\n=======\nyours\n>>>>>>> branch\n",
    )
    .unwrap();

    let git = Git::at(dir.path()).unwrap();
    git.add(&["tracked.txt"]).unwrap();

    let err = git
        .assert_commit_safe()
        .expect_err("must abort on staged markers");
    assert!(
        format!("{err:#}").contains("conflict markers"),
        "expected marker message, got: {err:#}"
    );
}

/// Regression: the guard must scan the staged *blob*, not the worktree
/// file. A user can `git add` a marker-laden file then clean the worktree
/// without re-staging — the index (what gets committed) still has markers,
/// but the worktree file is clean. Reading the worktree would let the
/// marker-laden blob ship; reading the index blob catches it.
#[test]
fn assert_commit_safe_scans_index_blob_not_worktree() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());

    // Stage a marker-laden version of a tracked file.
    std::fs::write(
        dir.path().join("tracked.txt"),
        "<<<<<<< HEAD\nmine\n=======\nyours\n>>>>>>> branch\n",
    )
    .unwrap();

    let git = Git::at(dir.path()).unwrap();
    git.add(&["tracked.txt"]).unwrap();

    // Clean the worktree WITHOUT re-staging: index still holds the markers.
    std::fs::write(dir.path().join("tracked.txt"), "clean\n").unwrap();

    let err = git
        .assert_commit_safe()
        .expect_err("guard must read the staged blob, not the worktree");
    assert!(
        format!("{err:#}").contains("conflict markers"),
        "expected marker message from staged blob, got: {err:#}"
    );
}

/// Make a hook file executable so git will actually invoke it. git skips a
/// non-executable hook silently, which would mask a regression in the
/// shell-out path. No-op off Unix — Windows runs hooks through `sh`
/// without checking the exec bit.
#[cfg(unix)]
fn make_hook_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}
#[cfg(not(unix))]
fn make_hook_executable(_path: &Path) {}

/// Install an executable hook script at `.git/hooks/<name>`. Shared by the
/// git.rs unit tests and the e2e suite (issue #20), which both need a real
/// hook the `git commit` shell-out will execute.
pub(crate) fn install_hook(dir: &Path, name: &str, body: &str) {
    let hooks_dir = dir.join(".git/hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook = hooks_dir.join(name);
    fs::write(&hook, format!("#!/bin/sh\n{body}\n")).unwrap();
    make_hook_executable(&hook);
}

/// The core claim of issue #19 (AC#2): a normal Run commits through the
/// real `git commit`, so all three commit hooks — `pre-commit`,
/// `prepare-commit-msg`, and `commit-msg` — run. libgit2 never runs hooks,
/// so this test is red under the old implementation and green only once
/// `Git::commit` shells out.
#[test]
fn commit_runs_pre_commit_and_commit_msg_hooks() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());

    // pre-commit drops a sentinel file — proves the hook executed.
    install_hook(dir.path(), "pre-commit", "echo ran > sentinel.txt");
    // prepare-commit-msg drops its own sentinel — proves it ran too. It
    // receives `$1` (message file) and `$2` (source = "message" for `-F`);
    // we only need the side effect.
    install_hook(
        dir.path(),
        "prepare-commit-msg",
        "echo ran > prepare-ran.txt",
    );
    // commit-msg appends a trailer to the message file ($1) — proves the
    // hook executed AND its edit survived into the landed commit.
    install_hook(
        dir.path(),
        "commit-msg",
        "echo 'Signed-off-by: aic-test' >> \"$1\"",
    );

    std::fs::write(dir.path().join("tracked.txt"), "changed\n").unwrap();
    let git = Git::at(dir.path()).unwrap();
    git.add(&["tracked.txt"]).unwrap();

    let hash = git.commit("chore: hook test".into(), None).unwrap();

    // pre-commit ran → sentinel exists.
    assert!(
        dir.path().join("sentinel.txt").exists(),
        "pre-commit hook must run during Git::commit"
    );
    // prepare-commit-msg ran → its sentinel exists.
    assert!(
        dir.path().join("prepare-ran.txt").exists(),
        "prepare-commit-msg hook must run during Git::commit"
    );
    // commit-msg ran → its trailer is in the committed message.
    let msg = git
        .run_git(&["log", "-1", "--pretty=%B"], None, &[])
        .unwrap();
    assert!(
        msg.contains("Signed-off-by: aic-test"),
        "commit-msg hook must run during Git::commit; got message:\n{msg}"
    );
    // The authored subject survives alongside the hook's trailer.
    assert!(msg.contains("chore: hook test"));
    // Hash matches git's own abbreviation (`rev-parse --short`), not a
    // fixed width — so it stays correct if git extends it for this repo.
    assert!(
        !hash.is_empty() && hash.chars().all(|c| c.is_ascii_hexdigit()),
        "hash must be non-empty hex: {hash}"
    );
}

/// The returned hash must match `git rev-parse --short HEAD` for the new
/// commit — we defer the abbreviation width to git (honors `core.abbrev`)
/// rather than slicing a fixed 7 chars ourselves.
#[test]
fn commit_returns_rev_parse_short_prefix() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());

    std::fs::write(dir.path().join("tracked.txt"), "changed\n").unwrap();
    let git = Git::at(dir.path()).unwrap();
    git.add(&["tracked.txt"]).unwrap();

    let hash = git.commit("chore: hash format".into(), None).unwrap();

    let short = git
        .run_git(&["rev-parse", "--short", "HEAD"], None, &[])
        .unwrap();
    assert_eq!(hash, short.trim());
    assert!(!hash.is_empty());
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
}

/// A drafted body must land verbatim in the committed message — the
/// `--cleanup=verbatim` flag keeps git from collapsing consecutive blank
/// lines or stripping trailing whitespace (the default `strip` cleanup
/// does both). The body also carries a `#`-prefixed line, which git treats
/// as commentary in interactive messages.
#[test]
fn commit_preserves_authored_body() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());

    std::fs::write(dir.path().join("tracked.txt"), "changed\n").unwrap();
    let git = Git::at(dir.path()).unwrap();
    git.add(&["tracked.txt"]).unwrap();

    let hash = git.commit(
        "fix: subject".into(),
        Some(
            "explanation line\n\n\nsecond paragraph\n\n# a comment-looking line\nline with trailing spaces  "
                .into(),
        ),
    )
    .unwrap();
    assert!(!hash.is_empty());
    assert!(
        hash.chars().all(|c| c.is_ascii_hexdigit()),
        "hash must be hex: {hash}"
    );

    let msg = git
        .run_git(&["log", "-1", "--pretty=%B"], None, &[])
        .unwrap();
    // Byte-for-byte: the blank run, the `#` line, and the trailing
    // whitespace all survive; git appends the closing newline.
    assert_eq!(
        msg,
        "fix: subject\n\nexplanation line\n\n\nsecond paragraph\n\n# a comment-looking line\nline with trailing spaces  \n"
    );
}

/// The enforcement half of #19's AC2: a hook that vetoes the commit
/// aborts `Git::commit`, surfaces the hook's own stderr, and lands
/// nothing. (The happy-path test above proves hooks run; this proves a
/// refusal is reported, not swallowed.)
#[test]
fn commit_vetoed_by_hook_surfaces_stderr_and_lands_nothing() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());

    install_hook(
        dir.path(),
        "pre-commit",
        "echo 'blocked by policy' >&2; exit 1",
    );

    std::fs::write(dir.path().join("tracked.txt"), "changed\n").unwrap();
    let git = Git::at(dir.path()).unwrap();
    git.add(&["tracked.txt"]).unwrap();
    let before = git.run_git(&["rev-parse", "HEAD"], None, &[]).unwrap();

    let err = git
        .commit("chore: vetoed".into(), None)
        .expect_err("a vetoing pre-commit hook must abort Git::commit");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("blocked by policy"),
        "the hook's stderr must surface, got: {msg}"
    );

    let after = git.run_git(&["rev-parse", "HEAD"], None, &[]).unwrap();
    assert_eq!(before, after, "a vetoed commit must not move HEAD");
}

/// With nothing staged, `git commit` refuses instead of silently landing
/// an empty commit (libgit2 created one). aic always stages before
/// committing, so this only fires on a state that must not be committed —
/// and the refusal carries git's own message.
#[test]
fn commit_refuses_when_nothing_staged() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let err = git
        .commit("chore: nothing".into(), None)
        .expect_err("git must refuse a commit with nothing staged");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("nothing to commit"),
        "git's refusal must surface, got: {msg}"
    );
    let count = git
        .run_git(&["rev-list", "--count", "HEAD"], None, &[])
        .unwrap();
    assert_eq!(count.trim(), "1", "no commit may land");
}

/// The husky / lint-staged flow this PR exists for: a `pre-commit` hook
/// edits a file and re-stages it; the landed commit must contain the
/// hook's change. (The other hook tests prove hooks *run*; this proves
/// hook-staged content *ships*.)
#[test]
fn commit_includes_hook_staged_changes() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());

    // lint-staged-style: rewrite a file and stage it from the hook.
    install_hook(
        dir.path(),
        "pre-commit",
        "echo 'auto-fixed by hook' > hook-fixed.txt\ngit add hook-fixed.txt",
    );

    std::fs::write(dir.path().join("tracked.txt"), "changed\n").unwrap();
    let git = Git::at(dir.path()).unwrap();
    git.add(&["tracked.txt"]).unwrap();

    let hash = git.commit("chore: hook staged".into(), None).unwrap();
    assert!(!hash.is_empty());
    assert!(
        hash.chars().all(|c| c.is_ascii_hexdigit()),
        "hash must be hex: {hash}"
    );

    let content = git
        .run_git(&["show", "HEAD:hook-fixed.txt"], None, &[])
        .unwrap();
    assert_eq!(content.trim(), "auto-fixed by hook");
}

/// The enforcement half of the hook window: a `pre-commit` hook re-stages
/// a file that holds conflict markers — content the pre-commit guard never
/// scanned (it runs before hooks). The post-commit tree scan catches it:
/// the commit landed, but `Git::commit` reports the violation, names the
/// file, and offers the recovery path instead of shipping silently.
#[test]
fn commit_reports_markers_staged_by_hook() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());

    install_hook(
        dir.path(),
        "pre-commit",
        "printf '<<<<<<< ours\\nbad\\n>>>>>>> theirs\\n' > sneaky.txt\ngit add sneaky.txt",
    );

    std::fs::write(dir.path().join("tracked.txt"), "changed\n").unwrap();
    let git = Git::at(dir.path()).unwrap();
    git.add(&["tracked.txt"]).unwrap();
    let before = git.run_git(&["rev-parse", "HEAD"], None, &[]).unwrap();

    let err = git
        .commit("chore: sneaky".into(), None)
        .expect_err("hook-staged conflict markers must be reported");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("sneaky.txt"),
        "the marked file must be named, got: {msg}"
    );
    assert!(
        msg.contains("commit landed"),
        "the error must say the commit exists, got: {msg}"
    );
    assert!(
        msg.contains("amend"),
        "the error must offer the recovery path, got: {msg}"
    );

    let after = git.run_git(&["rev-parse", "HEAD"], None, &[]).unwrap();
    assert_ne!(before, after, "the commit landed despite the markers");
}

/// `git commit -F -` refuses an empty message ("empty commit message")
/// where libgit2 created the commit — a silent empty-subject commit is
/// worse than a loud refusal. aic's message comes from the LLM, so an
/// empty string is a realistic input.
#[test]
fn commit_refuses_empty_message() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());

    std::fs::write(dir.path().join("tracked.txt"), "changed\n").unwrap();
    let git = Git::at(dir.path()).unwrap();
    git.add(&["tracked.txt"]).unwrap();
    let before = git.run_git(&["rev-parse", "HEAD"], None, &[]).unwrap();

    let err = git
        .commit(String::new(), None)
        .expect_err("an empty message must abort the commit");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("empty commit message"),
        "git's refusal must surface, got: {msg}"
    );

    let after = git.run_git(&["rev-parse", "HEAD"], None, &[]).unwrap();
    assert_eq!(before, after, "no commit may land with an empty message");
}

/// Regression: `diff_workdir` must return a real new-file patch for a file
/// that lives inside an *untracked directory* (e.g. a new `src/foo/` module
/// created by splitting a single file into a module dir). Without
/// `recurse_untracked_dirs`, libgit2 collapses the whole untracked directory
/// into one directory-level delta, `format_diff_for_path` finds no delta for
/// the inner file, returns an empty patch, and `stage_hunks` later feeds
/// `git apply --cached` an empty input → "No valid patches in input" → the
/// whole batch aborts. This is the splitting-a-file-into-a-module case.
#[test]
fn diff_workdir_returns_content_for_file_in_untracked_dir() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());

    // An untracked directory with a file inside — mirrors `src/e2e/`.
    let new_dir = dir.path().join("mymod");
    std::fs::create_dir(&new_dir).unwrap();
    std::fs::write(
        new_dir.join("mod.rs"),
        "pub fn x() {}
",
    )
    .unwrap();

    let git = Git::at(dir.path()).unwrap();
    let result = git.diff_workdir(Some("mymod/mod.rs")).unwrap();
    assert!(
        !result.is_empty(),
        "must produce a patch for a file in an untracked dir"
    );
    assert!(
        result.contains("--- /dev/null"),
        "must be a new-file patch; got:\n{result}"
    );
    // The rebuilt patch must actually apply to the index.
    let patch = parse_file_patch(&result);
    assert!(patch.hunk_count() > 0, "must have at least one hunk");
    git.run_git(&["apply", "--cached", "-"], Some(&result), &[])
        .expect("rebuilt new-file patch must apply to the index");
}

/// The footer's data contract: `staged_stats` describes exactly what the
/// next commit would land (new file, counts per file), and
/// `committed_stats` after the commit describes the same diff — the
/// preview footer and the ✓-line footer must agree.
#[test]
fn staged_and_committed_stats_match_the_landed_diff() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());
    let git = Git::at(dir.path()).unwrap();

    // New file with 3 lines, and a modified file: the seeded
    // "original\n" → "new\nextra\n" (one line replaced, one added).
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(dir.path().join("tracked.txt"), "new\nextra\n").unwrap();
    git.add(&["a.txt", "tracked.txt"]).unwrap();

    let paths = vec!["a.txt".to_string(), "tracked.txt".to_string()];
    let staged = git.staged_stats(&paths).unwrap();
    assert_eq!(staged.len(), 2, "one delta per path: {staged:?}");
    assert_eq!(staged[0].path, "a.txt");
    assert_eq!(staged[0].added, 3);
    assert_eq!(staged[0].deleted, 0);
    assert!(staged[0].new, "a.txt is new");
    assert!(!staged[0].removed);
    assert_eq!(staged[1].path, "tracked.txt");
    assert_eq!(staged[1].added, 2);
    assert_eq!(staged[1].deleted, 1);
    assert!(!staged[1].new);
    assert!(!staged[1].removed);

    git.commit("chore: stats test".into(), None).unwrap();
    let landed = git.committed_stats(&paths).unwrap();
    assert_eq!(landed, staged, "preview and landed footers must agree");
}

/// `committed_stats` on a root commit (no parent) diffs against the empty
/// tree — the `parent_count() == 0` branch. After `init_test_repo`, HEAD
/// is exactly such a root commit, seeded with `tracked.txt`.
#[test]
fn committed_stats_handles_root_commit_with_no_parent() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let stats = git.committed_stats(&["tracked.txt".to_string()]).unwrap();
    assert_eq!(
        stats.len(),
        1,
        "one delta against the empty tree: {stats:?}"
    );
    assert_eq!(stats[0].path, "tracked.txt");
    assert_eq!(stats[0].added, 1, "seeded `original\\n` is one added line");
    assert_eq!(stats[0].deleted, 0);
    assert!(stats[0].new, "root commit introduces tracked.txt");
}

/// `staged_stats` with no HEAD (a fresh repo, no commits yet) treats every
/// staged path as a new file — the `repo.head()` error branch. Mirrors
/// `Git::diff`'s head-less handling.
#[test]
fn staged_stats_treats_every_path_as_new_without_head() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.config()
        .unwrap()
        .set_str("core.autocrlf", "false")
        .unwrap();
    std::fs::write(dir.path().join("first.txt"), "alpha\nbeta\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("first.txt")).unwrap();
    index.write().unwrap();

    let git = Git::at(dir.path()).unwrap();
    let stats = git.staged_stats(&["first.txt".to_string()]).unwrap();
    assert_eq!(stats.len(), 1, "one staged path: {stats:?}");
    assert_eq!(stats[0].path, "first.txt");
    assert_eq!(stats[0].added, 2);
    assert!(stats[0].new, "no HEAD → every staged path counts as new");
    assert!(!stats[0].removed);
}

/// A planned path absent from the diff (e.g. a pre-commit hook that
/// cancelled its changes) is kept at zero counts rather than dropped, so
/// the footer always accounts for every planned file.
#[test]
fn committed_stats_keeps_unmatched_path_at_zero() {
    let dir = tempfile::tempdir().unwrap();
    init_test_repo(dir.path());
    let git = Git::at(dir.path()).unwrap();
    // tracked.txt is the only file in HEAD; a path with no delta is kept.
    let stats = git
        .committed_stats(&["nonexistent.txt".to_string()])
        .unwrap();
    assert_eq!(
        stats.len(),
        1,
        "unmatched path is kept, not dropped: {stats:?}"
    );
    assert_eq!(stats[0].path, "nonexistent.txt");
    assert_eq!(stats[0].added, 0);
    assert_eq!(stats[0].deleted, 0);
    assert!(
        !stats[0].new && !stats[0].removed && !stats[0].binary,
        "no delta → all flags false"
    );
}
