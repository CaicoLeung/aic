use anyhow::Context;
use git2::{DiffFormat, DiffLineType, Repository, Status};
use std::path::Path;

#[derive(Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct FileStatus {
    pub path: String,
    pub staged: bool,
    pub kind: StatusKind,
}

#[derive(Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub enum StatusKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
}

pub struct Git;

impl Git {
    fn repo() -> anyhow::Result<Repository> {
        Repository::discover(".").with_context(|| "failed to discover git repository")
    }

    pub fn status() -> anyhow::Result<Vec<FileStatus>> {
        let repo = Self::repo()?;
        let statuses = repo
            .statuses(None)
            .context("failed to get repository status")?;

        let mut result = Vec::new();

        for entry in statuses.iter() {
            let path = match entry.path() {
                Some(p) => p.to_string(),
                None => continue,
            };
            let flags = entry.status();

            if flags.intersects(
                Status::INDEX_NEW
                    | Status::INDEX_MODIFIED
                    | Status::INDEX_DELETED
                    | Status::INDEX_RENAMED,
            ) {
                result.push(FileStatus {
                    path: path.clone(),
                    staged: true,
                    kind: index_status_kind(flags),
                });
            }

            if flags.intersects(
                Status::WT_NEW | Status::WT_MODIFIED | Status::WT_DELETED | Status::WT_RENAMED,
            ) {
                result.push(FileStatus {
                    path,
                    staged: false,
                    kind: wt_status_kind(flags),
                });
            }
        }

        Ok(result)
    }

    pub fn diff(path: Option<&str>) -> anyhow::Result<String> {
        let repo = Self::repo()?;
        let head_tree = match repo.head() {
            Ok(r) => Some(r.peel_to_tree().context("failed to peel HEAD to tree")?),
            Err(_) => None,
        };

        let mut opts = git2::DiffOptions::new();
        opts.include_untracked(true);
        if let Some(p) = path {
            opts.pathspec(p);
        }

        let diff = repo
            .diff_tree_to_index(head_tree.as_ref(), None, Some(&mut opts))
            .context("failed to compute diff")?;

        format_diff(&diff)
    }

    pub fn diff_workdir(path: Option<&str>) -> anyhow::Result<String> {
        let repo = Self::repo()?;
        let index = repo.index().context("failed to get repository index")?;

        let mut opts = git2::DiffOptions::new();
        opts.include_untracked(true);
        opts.show_untracked_content(true);

        let diff = repo
            .diff_index_to_workdir(Some(&index), Some(&mut opts))
            .context("failed to compute workdir diff")?;

        match path {
            Some(p) => format_diff_for_path(&diff, p),
            None => format_diff(&diff),
        }
    }

    pub fn add(paths: &[&str]) -> anyhow::Result<()> {
        let repo = Self::repo()?;
        let mut index = repo.index().context("failed to get repository index")?;

        if paths.is_empty() {
            index
                .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
                .context("failed to add all files to index")?;
        } else {
            for path in paths {
                index
                    .add_path(Path::new(path))
                    .with_context(|| format!("failed to add {path} to index"))?;
            }
        }

        index.write().context("failed to write index")?;
        Ok(())
    }

    pub fn commit(message: String, body: Option<String>) -> anyhow::Result<()> {
        let repo = Self::repo()?;
        let mut index = repo.index().context("failed to get repository index")?;
        let tree_id = index.write_tree_to(&repo).context("failed to write tree")?;
        let tree = repo.find_tree(tree_id).context("failed to find tree")?;
        let sig = repo.signature().context("failed to get git signature")?;

        let parents: Vec<_> = match repo.head() {
            Ok(r) => vec![
                r.peel_to_commit()
                    .context("failed to peel HEAD to commit")?,
            ],
            Err(_) => vec![],
        };
        let parent_refs: Vec<_> = parents.iter().collect();

        let full_message = match body {
            Some(b) => format!("{message}\n\n{b}"),
            None => message.to_string(),
        };

        repo.commit(Some("HEAD"), &sig, &sig, &full_message, &tree, &parent_refs)
            .context("failed to create commit")?;

        Ok(())
    }
}

fn index_status_kind(flags: Status) -> StatusKind {
    if flags.contains(Status::INDEX_NEW) {
        StatusKind::Added
    } else if flags.contains(Status::INDEX_MODIFIED) {
        StatusKind::Modified
    } else if flags.contains(Status::INDEX_DELETED) {
        StatusKind::Deleted
    } else {
        StatusKind::Renamed
    }
}

fn format_line(line: &git2::DiffLine, output: &mut String) {
    match line.origin_value() {
        DiffLineType::Context | DiffLineType::Addition | DiffLineType::Deletion => {
            let origin = match line.origin() {
                '+' => "+",
                '-' => "-",
                ' ' => " ",
                _ => "",
            };
            output.push_str(origin);
            output.push_str(&String::from_utf8_lossy(line.content()));
        }
        _ => {
            output.push_str(&String::from_utf8_lossy(line.content()));
        }
    }
}

fn format_diff(diff: &git2::Diff) -> anyhow::Result<String> {
    if diff.deltas().len() == 0 {
        return Ok(String::new());
    }

    let mut output = String::new();
    diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
        format_line(&line, &mut output);
        true
    })
    .context("failed to format diff")?;

    Ok(output)
}

fn format_diff_for_path(diff: &git2::Diff, path: &str) -> anyhow::Result<String> {
    let matched = diff
        .deltas()
        .any(|d| d.new_file().path().is_some_and(|p| p == Path::new(path)));
    if !matched {
        return Ok(String::new());
    }

    let mut output = String::new();
    diff.print(DiffFormat::Patch, |delta, _hunk, line| {
        if delta.new_file().path().is_none_or(|p| p != Path::new(path)) {
            return true;
        }
        format_line(&line, &mut output);
        true
    })
    .context("failed to format diff")?;

    Ok(output)
}

fn wt_status_kind(flags: Status) -> StatusKind {
    if flags.contains(Status::WT_NEW) {
        StatusKind::Untracked
    } else if flags.contains(Status::WT_MODIFIED) {
        StatusKind::Modified
    } else if flags.contains(Status::WT_DELETED) {
        StatusKind::Deleted
    } else {
        StatusKind::Renamed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct CwdGuard {
        original: PathBuf,
    }

    impl CwdGuard {
        fn new(dir: &Path) -> Self {
            let original = std::env::current_dir().unwrap();
            std::env::set_current_dir(dir).unwrap();
            Self { original }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    fn init_test_repo(dir: &Path) {
        let repo = Repository::init(dir).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "test").unwrap();
        config.set_str("user.email", "test@test.com").unwrap();

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

    #[test]
    fn diff_workdir_returns_untracked_content() {
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        std::fs::write(dir.path().join("new_file.txt"), "new content\n").unwrap();

        let _guard = CwdGuard::new(dir.path());
        let result = Git::diff_workdir(Some("new_file.txt")).unwrap();
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

        let _guard = CwdGuard::new(dir.path());
        let result = Git::diff_workdir(Some("tracked.txt")).unwrap();
        assert!(
            !result.is_empty(),
            "should have diff content for modified file"
        );
    }
}
