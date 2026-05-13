use async_trait::async_trait;
use mcp_flowgate_core::error::ExecutorError;
use mcp_flowgate_core::model::{ExecuteRequest, ExecuteResult};
use mcp_flowgate_core::ports::Executor;
use serde_json::json;

/// An executor that always succeeds with `{}`. Used as the default for
/// proxy exposures that don't specify one and as a placeholder during
/// development.
pub struct NoopExecutor;

#[async_trait]
impl Executor for NoopExecutor {
    async fn execute(&self, _request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult {
            output: json!({}),
            evidence: vec![],
        })
    }
}
