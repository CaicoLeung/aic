//! Unit tests for the plan-time [`WorkdirSnapshot`].

use std::path::Path;
use std::process::Command;

use super::WorkdirSnapshot;
use crate::git::Git;
use crate::git::diff;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?} failed");
}

fn unstaged(git: &Git) -> Vec<crate::git::FileStatus> {
    git.status()
        .unwrap()
        .into_iter()
        .filter(|f| !f.staged)
        .collect()
}

/// One traversal, agreeing views: the snapshot's hunk counts match a
/// fresh parse of each raw diff it captured, the payload envelope
/// carries every scanned path, and the raw pairs are the same diffs the
/// Redaction Gate and the fallback would consume.
#[test]
fn scan_captures_counts_payload_and_raw_diffs_together() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    let base = "a\n".repeat(12);
    std::fs::write(dir.path().join("a.txt"), &base).unwrap();
    std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "base"]);
    // Two disjoint edits in a.txt (two hunks), one in b.txt.
    let mut edited = base.clone();
    edited.replace_range(0..1, "A");
    edited.replace_range(edited.len() - 2..edited.len() - 1, "A");
    std::fs::write(dir.path().join("a.txt"), edited).unwrap();
    std::fs::write(dir.path().join("b.txt"), "B\n").unwrap();

    let git = Git::at(dir.path()).unwrap();
    let statuses = unstaged(&git);
    let files: Vec<_> = statuses.iter().collect();
    let snapshot = WorkdirSnapshot::scan(&git, &files).unwrap();

    let counts = snapshot.hunk_counts();
    assert_eq!(counts.len(), 2);
    for (path, raw) in snapshot.raw_pairs() {
        let parsed = diff::parse_file_patch(raw).hunk_count();
        let claimed = counts
            .iter()
            .find(|(p, _)| p == path)
            .unwrap_or_else(|| panic!("raw pair path {path} missing from counts"))
            .1;
        assert_eq!(
            parsed, claimed,
            "count disagrees with the raw diff for {path}"
        );
    }
    assert_eq!(counts[0], ("a.txt".to_string(), 2));
    assert_eq!(counts[1], ("b.txt".to_string(), 1));
    assert_eq!(snapshot.paths().len(), 2);
    let payload = snapshot.planner_payload();
    assert!(payload.contains("a.txt") && payload.contains("b.txt"));
    assert!(
        payload.starts_with("{\"unstaged_files\":"),
        "planner payload is the scan envelope"
    );
}

/// A changed file with no textual hunks (mode/bit-flip content that
/// parses to zero hunks) still enters the envelope — via the explicit
/// binary marker — so the model cannot silently drop it.
#[test]
fn scan_marks_non_textual_changes_instead_of_dropping_them() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    std::fs::write(dir.path().join("run.sh"), "hello\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "base"]);
    // Pure mode change: no textual hunks, non-empty raw diff.
    git(dir.path(), &["update-index", "--chmod=+x", "run.sh"]);

    let git = Git::at(dir.path()).unwrap();
    let files = unstaged(&git);
    let snapshot = WorkdirSnapshot::scan(&git, &files.iter().collect::<Vec<_>>()).unwrap();

    assert_eq!(snapshot.hunk_counts(), &[("run.sh".to_string(), 0)]);
    assert!(
        snapshot
            .planner_payload()
            .contains(crate::llm::prompt::BINARY_MARKER),
        "zero-hunk change must carry the explicit marker, not an empty diff"
    );
}
