use crate::llm::LLM;
use crate::prompt::PromptConfig;

pub struct Generator {}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CommitOutput {
    pub message: String,
    pub body: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct BatchPlanBatch {
    pub files: Vec<String>,
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct BatchPlanOutput {
    pub batches: Vec<BatchPlanBatch>,
}

impl Generator {
    pub async fn generate_commit_message(diff: &str) -> anyhow::Result<CommitOutput> {
        let p = PromptConfig::default().git_message;
        LLM::from_env().agent(&p).schema::<CommitOutput>(diff).await
    }

    pub async fn split_patch(diff: &str) -> anyhow::Result<BatchPlanOutput> {
        let p = PromptConfig::default().batch_plan_prompt;
        LLM::from_env()
            .agent(&p)
            .schema::<BatchPlanOutput>(diff)
            .await
    }
}
