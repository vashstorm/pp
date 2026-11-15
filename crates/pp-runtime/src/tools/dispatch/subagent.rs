//! 子 Agent 前置分支：`wait`、`task` 与 `read`／`write` 的 URI 结果读取。
use super::*;

impl Tools {
    /// 返回被处理的工具结果；`None` 表示交给普通 match 分发。
    pub(in crate::tools) fn dispatch_subagent(
        &self,
        name: &str,
        arguments: &str,
        call_id: &str,
        policy: &ToolPolicy<'_>,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Option<Value>> {
        if name == "wait" {
            return self
                .run_wait(arguments, call_id, cancellation, emit)
                .map(Some);
        }
        if let Some(binding) = &self.subagent {
            if name == "task" {
                let plan = matches!(policy, ToolPolicy::Plan { .. });
                return binding
                    .execute(name, arguments, plan, cancellation)
                    .map(Some);
            }
            if matches!(name, "read" | "write") {
                let value: Value = serde_json::from_str(arguments)?;
                if let Some(path) = value["path"].as_str().filter(|p| {
                    crate::subagent::is_uri(p) && !(name == "read" && p.starts_with("agent://"))
                }) {
                    let result =
                        binding.uri(name, path, value["content"].as_str(), cancellation)?;
                    return Ok(Some(result));
                }
            }
        }
        Ok(None)
    }
}
