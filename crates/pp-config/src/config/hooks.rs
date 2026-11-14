//! 生命周期 command hooks 的值类型：用户级 `~/.pp/agent/hooks.yml` 与
//! 项目级 `<启动目录>/.pp/hooks.yml` 共用同一形状。
//!
//! 只描述配置形状与结构校验；正则编译、事件匹配与进程执行都在 runtime。
//! 严格解析：未知事件名、未知 handler 字段一律拒绝，不为了兼容 Claude
//! 配置而静默忽略。

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// hooks 允许的超时上限（秒）；超过按配置错误拒绝。
pub const MAX_HOOK_TIMEOUT_SECONDS: u64 = 3600;
/// `timeout` 缺省值：30 秒。
pub const DEFAULT_HOOK_TIMEOUT_SECONDS: u64 = 30;

/// 五个支持的生命周期事件；wire 名称就是枚举名。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum HookEvent {
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    Stop,
}

impl HookEvent {
    /// 全部事件的固定顺序；runtime 用它建立索引表。
    pub const ALL: [Self; 5] = [
        Self::UserPromptSubmit,
        Self::PreToolUse,
        Self::PostToolUse,
        Self::PostToolUseFailure,
        Self::Stop,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::Stop => "Stop",
        }
    }

    /// 工具事件按 pp 原生工具名匹配；其余事件不匹配工具名。
    pub fn is_tool_event(self) -> bool {
        matches!(
            self,
            Self::PreToolUse | Self::PostToolUse | Self::PostToolUseFailure
        )
    }

    /// 控制点：内部故障默认 fail-closed；post 事件默认放行并告警。
    pub fn is_control_point(self) -> bool {
        !matches!(self, Self::PostToolUse | Self::PostToolUseFailure)
    }
}

/// 唯一支持的 handler 类型：外部命令。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookHandlerType {
    Command,
}

/// handler 内部故障时的显式策略；缺省由事件默认策略决定。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookFailurePolicy {
    Block,
    Continue,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandHook {
    #[serde(rename = "type")]
    pub kind: HookHandlerType,
    pub command: String,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(rename = "onFailure", default)]
    pub on_failure: Option<HookFailurePolicy>,
}

fn default_timeout() -> u64 {
    DEFAULT_HOOK_TIMEOUT_SECONDS
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookMatcherGroup {
    #[serde(default)]
    pub matcher: Option<String>,
    pub hooks: Vec<CommandHook>,
}

/// `hooks:` 的五个事件数组；缺省全部为空。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HooksSettings {
    #[serde(rename = "UserPromptSubmit")]
    pub user_prompt_submit: Vec<HookMatcherGroup>,
    #[serde(rename = "PreToolUse")]
    pub pre_tool_use: Vec<HookMatcherGroup>,
    #[serde(rename = "PostToolUse")]
    pub post_tool_use: Vec<HookMatcherGroup>,
    #[serde(rename = "PostToolUseFailure")]
    pub post_tool_use_failure: Vec<HookMatcherGroup>,
    #[serde(rename = "Stop")]
    pub stop: Vec<HookMatcherGroup>,
}

impl HooksSettings {
    pub fn groups(&self, event: HookEvent) -> &[HookMatcherGroup] {
        match event {
            HookEvent::UserPromptSubmit => &self.user_prompt_submit,
            HookEvent::PreToolUse => &self.pre_tool_use,
            HookEvent::PostToolUse => &self.post_tool_use,
            HookEvent::PostToolUseFailure => &self.post_tool_use_failure,
            HookEvent::Stop => &self.stop,
        }
    }

    fn groups_mut(&mut self, event: HookEvent) -> &mut Vec<HookMatcherGroup> {
        match event {
            HookEvent::UserPromptSubmit => &mut self.user_prompt_submit,
            HookEvent::PreToolUse => &mut self.pre_tool_use,
            HookEvent::PostToolUse => &mut self.post_tool_use,
            HookEvent::PostToolUseFailure => &mut self.post_tool_use_failure,
            HookEvent::Stop => &mut self.stop,
        }
    }

    /// 追加另一个来源的组：项目级组接在用户级组之后，同一事件内按添加顺序匹配。
    pub fn extend(&mut self, other: &Self) {
        for event in HookEvent::ALL {
            self.groups_mut(event)
                .extend_from_slice(other.groups(event));
        }
    }

    pub fn is_empty(&self) -> bool {
        HookEvent::ALL
            .iter()
            .all(|event| self.groups(*event).is_empty())
    }

    /// 结构校验：命令、超时范围与不匹配工具名的事件 matcher。
    /// 正则编译留给 runtime，错误消息只带事件名与下标。
    pub fn validate(&self) -> Result<()> {
        for event in HookEvent::ALL {
            for (group_index, group) in self.groups(event).iter().enumerate() {
                let where_ = format!("hooks.{}[{}]", event.as_str(), group_index);
                if !event.is_tool_event()
                    && let Some(matcher) = &group.matcher
                {
                    ensure!(
                        matcher.is_empty() || matcher == "*",
                        "{where_}.matcher must be empty or \"*\" for {}",
                        event.as_str()
                    );
                }
                for (hook_index, hook) in group.hooks.iter().enumerate() {
                    ensure!(
                        !hook.command.trim().is_empty(),
                        "{where_}.hooks[{hook_index}].command must not be blank"
                    );
                    ensure!(
                        (1..=MAX_HOOK_TIMEOUT_SECONDS).contains(&hook.timeout),
                        "{where_}.hooks[{hook_index}].timeout must be between 1 and {MAX_HOOK_TIMEOUT_SECONDS}"
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<HooksSettings> {
        Ok(serde_yaml::from_str(yaml)?)
    }

    #[test]
    fn hooks_parse_full_shape_with_defaults() {
        let settings = parse(
            "PreToolUse:\n  - matcher: 'bash,edit'\n    hooks:\n      - type: command\n        command: /bin/true\nStop:\n  - hooks:\n      - type: command\n        command: /bin/true\n        args: ['--flag', 'a b']\n        timeout: 5\n        onFailure: block\n",
        )
        .unwrap();
        settings.validate().unwrap();
        let group = &settings.pre_tool_use[0];
        assert_eq!(group.matcher.as_deref(), Some("bash,edit"));
        assert_eq!(group.hooks[0].timeout, DEFAULT_HOOK_TIMEOUT_SECONDS);
        assert_eq!(group.hooks[0].on_failure, None);
        assert_eq!(group.hooks[0].args, None);
        let stop = &settings.stop[0].hooks[0];
        assert_eq!(
            stop.args.as_deref(),
            Some(&["--flag".to_owned(), "a b".to_owned()][..])
        );
        assert_eq!(stop.timeout, 5);
        assert_eq!(stop.on_failure, Some(HookFailurePolicy::Block));
        assert!(!settings.is_empty());
    }

    #[test]
    fn hooks_reject_unknown_events_and_handler_fields() {
        for yaml in [
            "SessionStart:\n  - hooks:\n      - type: command\n        command: /bin/true\n",
            "PreToolUse:\n  - hooks:\n      - type: command\n        command: /bin/true\n        async: true\n",
            "PreToolUse:\n  - hooks:\n      - type: prompt\n        command: /bin/true\n",
            "PreToolUse:\n  - hooks:\n      - type: command\n        command: /bin/true\n        onFailure: retry\n",
            "PreToolUse:\n  - match: bash\n    hooks:\n      - type: command\n        command: /bin/true\n",
        ] {
            assert!(parse(yaml).is_err(), "{yaml} must be rejected");
        }
    }

    #[test]
    fn hooks_reject_blank_command_timeout_and_tool_matcher_on_input_events() {
        let blank =
            parse("PreToolUse:\n  - hooks:\n      - type: command\n        command: '  '\n")
                .unwrap();
        assert!(blank.validate().is_err());
        let timeout = parse("PreToolUse:\n  - hooks:\n      - type: command\n        command: /bin/true\n        timeout: 0\n").unwrap();
        assert!(timeout.validate().is_err());
        let too_long = parse("PreToolUse:\n  - hooks:\n      - type: command\n        command: /bin/true\n        timeout: 3601\n").unwrap();
        assert!(too_long.validate().is_err());
        let matcher = parse("UserPromptSubmit:\n  - matcher: bash\n    hooks:\n      - type: command\n        command: /bin/true\n").unwrap();
        let error = matcher.validate().unwrap_err().to_string();
        assert!(error.contains("UserPromptSubmit"), "{error}");
        let tool_matcher =
            parse("PostToolUse:\n  - matcher: '^(bash|write)$'\n    hooks: []\n").unwrap();
        tool_matcher.validate().unwrap();
        assert!(parse("PostToolUse:\n  - hooks: {}\n").is_err());
    }
}
