pub mod generator;
pub mod git;
pub mod llm;
pub mod prompt;

use crate::git::Git;

async fn generate_and_commit(paths: &[String]) -> anyhow::Result<()> {
    let files: Vec<serde_json::Value> = paths
        .iter()
        .map(|p| {
            let diff = Git::diff(Some(p.as_str()))?;
            Ok(serde_json::json!({ "path": p, "diff": diff }))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let diff = serde_json::json!({ "staged_files": files });
    let result = generator::Generator::generate_commit_message(&diff.to_string()).await?;
    #[cfg(debug_assertions)]
    println!("commit result: {:#?}", result);
    Git::commit(result.message, result.body)?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let status = Git::status()?;
    let staged_files: Vec<_> = status.iter().filter(|f| f.staged).collect();

    if staged_files.is_empty() {
        let unstaged_files: Vec<_> = status.iter().filter(|f| !f.staged).collect();
        let files: Vec<serde_json::Value> = unstaged_files
            .iter()
            .map(|f| {
                let diff = Git::diff_workdir(Some(f.path.as_str()))?;
                Ok(serde_json::json!({ "path": f.path, "diff": diff }))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let diff = serde_json::json!({ "unstaged_files": files });
        let result = generator::Generator::split_patch(&diff.to_string()).await?;
        #[cfg(debug_assertions)]
        println!("split_patch result: {:#?}", result);
        for batch in &result.batches {
            let paths: Vec<&str> = batch.files.iter().map(|s| s.as_str()).collect();
            Git::add(&paths)?;
            generate_and_commit(&batch.files).await?;
        }
    } else {
        let paths: Vec<String> = staged_files.iter().map(|f| f.path.clone()).collect();
        generate_and_commit(&paths).await?;
    }

    Ok(())
}
