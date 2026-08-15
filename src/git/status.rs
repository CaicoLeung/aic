//! Status listing: the staged/worktree file inventory a Run plans over.
//! Index/wt flag → `StatusKind` mapping lives here too.

use super::*;

impl Git {
    pub fn status(&self) -> anyhow::Result<Vec<FileStatus>> {
        let repo = &self.repo;
        let statuses = repo
            .statuses(None)
            .context("failed to get repository status")?;

        let mut result = Vec::new();

        for entry in statuses.iter() {
            let path = match entry.path() {
                Ok(p) => p.to_string(),
                Err(_) => continue,
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
