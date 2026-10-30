//! `/task-test` 的固定自检场景：仅在 cargo debug 构建中暴露。
//!
//! 主 Agent 用当前会话模型派发一次 `task` 批次，三个只读子 Agent 分别使用
//! FAST、SLOW 与 REVIEW 三个 Model Role；因此缺少任一已启用 Role 时只报本地
//! 错误，不发送任何模型请求。

use crate::config::{ModelRole, ModelRoles};
use anyhow::{Result, ensure};

/// 固定提示词与批次内容；实现与测试都依赖它保持逐字不变。
pub(crate) const PROMPT: &str = r#"Debug build self-test for pp multi-subagent display and messaging. Follow these steps exactly.

1. Call the `task` tool exactly once with this batch and do not change names, agents, roles, or task text:
{"context":"/task-test debug scenario: read-only inspection of the current repository.","tasks":[{"name":"fast-rust-count","agent":"scout","role":"fast","task":"Count regular Rust source files (*.rs) under the current working directory, skipping target/ and .git/, and report the total number."},{"name":"slow-symbol-lookup","agent":"scout","role":"slow","task":"Use grep to find where the symbol AgentEvent is defined (struct, enum, trait, or type alias) and report the file path and line number, or state that it is absent."},{"name":"review-events-risk","agent":"reviewer","task":"Review src/events.rs and report one concrete risk in its event payload or serialization shape, citing file and line references. Propose no edits."}]}
2. Use the `wait` tool with every returned run id until all three results are delivered to you.
3. Print a compact three-line summary: name, status, and one-line result for each subagent.
4. Do not spawn additional subagents, do not modify files, and do not do any other work.

If an agent type or model role is unavailable, report the exact tool error instead of retrying, then stop."#;

/// 固定场景要求的三个 Role；全部启用才允许发送提示词。
pub(crate) fn ready(roles: &ModelRoles) -> Result<()> {
    let missing = [ModelRole::Fast, ModelRole::Slow, ModelRole::Review]
        .into_iter()
        .filter(|role| !roles.get(role).is_some_and(|assignment| assignment.enabled))
        .map(ModelRole::name)
        .collect::<Vec<_>>();
    ensure!(
        missing.is_empty(),
        "Enable model roles {} in /Roles before /task-test",
        missing.join(", ")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RoleAssignment, ThinkLevel};

    fn roles(enabled: &[ModelRole]) -> ModelRoles {
        ModelRole::ALL
            .into_iter()
            .map(|role| {
                (
                    role,
                    RoleAssignment {
                        enabled: enabled.contains(&role),
                        model: "fixture/primary".into(),
                        effort: ThinkLevel::None,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn ready_requires_fast_slow_and_review_roles() {
        let all = [ModelRole::Fast, ModelRole::Slow, ModelRole::Review];
        assert!(ready(&roles(&all)).is_ok());
        assert!(ready(&roles(&ModelRole::ALL)).is_ok());
        let message = ready(&roles(&[ModelRole::Fast])).unwrap_err().to_string();
        assert_eq!(
            message,
            "Enable model roles SLOW, REVIEW in /Roles before /task-test"
        );
        assert_eq!(
            ready(&ModelRoles::default()).unwrap_err().to_string(),
            "Enable model roles FAST, SLOW, REVIEW in /Roles before /task-test"
        );
    }

    #[test]
    fn prompt_pins_the_fixed_subagent_batch() {
        for marker in [
            "fast-rust-count",
            "slow-symbol-lookup",
            "review-events-risk",
            "\"role\":\"fast\"",
            "\"role\":\"slow\"",
            "\"agent\":\"scout\"",
            "\"agent\":\"reviewer\"",
        ] {
            assert!(PROMPT.contains(marker), "missing {marker}");
        }
    }
}
