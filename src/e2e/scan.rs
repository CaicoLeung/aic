//! The Run's Scan phase against a real repo: `WorkdirSnapshot::scan` is
//! the one traversal every plan-time consumer reads (the planner's
//! envelope, the validator's hunk counts, the Draft slices), so its
//! views must agree with a fresh parse of the diffs it captured.

use super::common::*;
use crate::git::diff;

/// The unstaged `FileStatus` list — the Run's dispatch input.
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
    let (dir, git) = with_git(gh::init_test_repo);
    let base = "a\n".repeat(12);
    commit_file(dir.path(), "a.txt", &base, "base a");
    commit_file(dir.path(), "b.txt", "b\n", "base b");
    // Two disjoint edits in a.txt (two hunks), one in b.txt.
    let mut edited = base.clone();
    edited.replace_range(0..1, "A");
    edited.replace_range(edited.len() - 2..edited.len() - 1, "A");
    std::fs::write(dir.path().join("a.txt"), edited).unwrap();
    std::fs::write(dir.path().join("b.txt"), "B\n").unwrap();

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

/// A changed file with no textual hunks (binary content that parses to
/// zero hunks) still enters the envelope — via the explicit binary
/// marker — so the model cannot silently drop it.
#[test]
fn scan_marks_non_textual_changes_instead_of_dropping_them() {
    let (dir, git) = with_git(gh::init_test_repo);
    // Binary base, binary rewrite: a non-empty workdir diff with zero
    // `@@` hunks on every platform. (A mode flip is not — Windows has
    // no exec bit, so `update-index --chmod=+x` never surfaces as an
    // unstaged change there.)
    std::fs::write(dir.path().join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
    git_in(dir.path(), &["add", "blob.bin"]);
    git_in(dir.path(), &["commit", "-m", "add binary"]);
    std::fs::write(dir.path().join("blob.bin"), [9u8, 9, 9, 9]).unwrap();

    let files = unstaged(&git);
    let snapshot = WorkdirSnapshot::scan(&git, &files.iter().collect::<Vec<_>>()).unwrap();

    assert_eq!(snapshot.hunk_counts(), &[("blob.bin".to_string(), 0)]);
    assert!(
        snapshot
            .planner_payload()
            .contains(crate::llm::prompt::BINARY_MARKER),
        "zero-hunk change must carry the explicit marker, not an empty diff"
    );
}
